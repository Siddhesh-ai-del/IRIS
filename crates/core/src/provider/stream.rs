//! Streaming plumbing (stage 1.3): SSE payload → [`StreamEvent`] →
//! [`StreamAssembler`] → [`CompletionResponse`].
//!
//! Wire quirks handled (the plan's "partial tool-call deltas" risk):
//!
//! - tool-call fragments arrive keyed by `index`; `id`/`name` only in the
//!   first fragment, `function.arguments` split **anywhere** (mid-JSON) and
//!   accumulated as a string until `[DONE]`
//! - `finish_reason` arrives in a separate, delta-less chunk
//! - `usage` arrives in its own chunk (we request `stream_options.include_usage`)
//! - the stream ends with the literal `data: [DONE]`

use std::collections::{BTreeMap, VecDeque};

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::Deserialize;

use super::sse::SseDecoder;
use super::wire::{WireUsage, map_finish_reason, usage_from_wire};
use super::{CompletionResponse, EventStream, ProviderError, StreamEvent};
use crate::types::{ContentBlock, Message, Role, StopReason, ToolCall, Usage};

// --- streaming chunk DTOs --------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamChoice {
    #[serde(default)]
    pub delta: StreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamDelta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamToolCallDelta {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

// --- SSE data payload → events ---------------------------------------------

/// Parses one `data:` payload at a time, holding cross-chunk state
/// (`finish_reason`, whether tool deltas were seen, `[DONE]` seen).
#[derive(Debug, Default)]
pub(crate) struct StreamParser {
    finish_reason: Option<StopReason>,
    saw_tool_delta: bool,
    pub(crate) done_seen: bool,
}

impl StreamParser {
    /// One SSE `data:` payload → zero or more events. The `[DONE]` marker
    /// yields the terminal [`StreamEvent::Done`]. An unknown `finish_reason`
    /// fails loud here, before `[DONE]`.
    pub(crate) fn push(&mut self, data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
        if data == "[DONE]" {
            self.done_seen = true;
            return Ok(vec![self.done_event()]);
        }
        let chunk: StreamChunk = serde_json::from_str(data)?;
        let mut events = Vec::new();

        if let Some(usage) = chunk.usage {
            events.push(StreamEvent::Usage(usage_from_wire(Some(usage))));
        }
        if let Some(choice) = chunk.choices.into_iter().next() {
            if let Some(raw) = choice.finish_reason.as_deref() {
                // Map eagerly: an unknown value kills the stream now, not
                // at [DONE].
                self.finish_reason = Some(map_finish_reason(Some(raw), self.saw_tool_delta)?);
            }
            if let Some(text) = choice.delta.content.filter(|t| !t.is_empty()) {
                events.push(StreamEvent::DeltaText(text));
            }
            for delta in choice.delta.tool_calls.into_iter().flatten() {
                self.saw_tool_delta = true;
                events.push(StreamEvent::DeltaToolCall {
                    index: delta.index,
                    id: delta.id,
                    name: delta.function.as_ref().and_then(|f| f.name.clone()),
                    arguments_delta: delta.function.and_then(|f| f.arguments).unwrap_or_default(),
                });
            }
        }
        Ok(events)
    }

    /// EOF without a `[DONE]` marker (proxy stripped it): emit a terminal
    /// event anyway, inferring the stop reason like the non-streaming path.
    pub(crate) fn finish(&self) -> StreamEvent {
        self.done_event()
    }

    fn done_event(&self) -> StreamEvent {
        let stop_reason = self
            .finish_reason
            .clone()
            .unwrap_or(if self.saw_tool_delta {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            });
        StreamEvent::Done { stop_reason }
    }
}

struct StreamState<S> {
    /// Boxed + pinned: reqwest's body stream is `!Unpin`.
    inner: std::pin::Pin<Box<S>>,
    decoder: SseDecoder,
    parser: StreamParser,
    pending: VecDeque<Result<StreamEvent, ProviderError>>,
    ended: bool,
}

impl<S> StreamState<S>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send,
{
    fn feed(&mut self, data: &str) {
        match self.parser.push(data) {
            Ok(events) => self.pending.extend(events.into_iter().map(Ok)),
            Err(err) => {
                self.pending.push_back(Err(err));
                self.ended = true;
            }
        }
        if self.parser.done_seen {
            // [DONE] received — drain what we have, then stop reading.
            self.ended = true;
        }
    }
}

/// Build the boxed event stream over a reqwest body byte-stream.
pub(crate) fn event_stream<S>(bytes: S) -> EventStream
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let state = StreamState {
        inner: Box::pin(bytes),
        decoder: SseDecoder::default(),
        parser: StreamParser::default(),
        pending: VecDeque::new(),
        ended: false,
    };
    Box::pin(futures::stream::unfold(state, |mut st| async move {
        loop {
            if let Some(item) = st.pending.pop_front() {
                return Some((item, st));
            }
            if st.ended {
                return None;
            }
            match st.inner.next().await {
                Some(Ok(chunk)) => {
                    for data in st.decoder.push(&chunk) {
                        st.feed(&data);
                        if st.ended {
                            break;
                        }
                    }
                }
                Some(Err(err)) => {
                    st.pending.push_back(Err(ProviderError::Http(err)));
                    st.ended = true;
                }
                None => {
                    // EOF: flush the decoder, then a terminal event if the
                    // server never sent `[DONE]`.
                    if let Some(data) = st.decoder.finish() {
                        st.feed(&data);
                    }
                    if !st.ended && !st.parser.done_seen {
                        st.pending.push_back(Ok(st.parser.finish()));
                    }
                    st.ended = true;
                }
            }
        }
    }))
}

// --- events → completion ---------------------------------------------------

#[derive(Debug, Default)]
struct ToolCallAcc {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Folds [`StreamEvent`]s back into one [`CompletionResponse`] — the same
/// domain shape the non-streaming path produces (stage 1.3 done-when).
#[derive(Debug, Default)]
pub struct StreamAssembler {
    text: String,
    tool_calls: BTreeMap<u32, ToolCallAcc>,
    stop_reason: Option<StopReason>,
    usage: Option<Usage>,
}

impl StreamAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one event (in arrival order). Infallible: all fallible work
    /// happens in [`finish`][Self::finish].
    pub fn push(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::DeltaText(text) => self.text.push_str(&text),
            StreamEvent::DeltaToolCall {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let acc = self.tool_calls.entry(index).or_default();
                if let Some(id) = id {
                    acc.id = Some(id);
                }
                if let Some(name) = name {
                    acc.name = Some(name);
                }
                acc.arguments.push_str(&arguments_delta);
            }
            StreamEvent::Usage(usage) => self.usage = Some(usage),
            StreamEvent::Done { stop_reason } => self.stop_reason = Some(stop_reason),
        }
    }

    /// Build the final response. Mirrors non-streaming mapping: text block
    /// first, then tool uses in index order; empty arguments → `{}`;
    /// malformed arguments → [`ProviderError::Decode`].
    pub fn finish(self) -> Result<CompletionResponse, ProviderError> {
        let StreamAssembler {
            text,
            tool_calls,
            stop_reason,
            usage,
        } = self;

        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
        let has_tool_calls = !tool_calls.is_empty();
        for (index, acc) in tool_calls {
            let name = acc.name.filter(|name| !name.is_empty()).ok_or_else(|| {
                ProviderError::Decode(format!("streamed tool call {index} arrived without a name"))
            })?;
            // arguments is a JSON *string* accumulated across fragments;
            // empty (no-arg call) → {}.
            let input = if acc.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&acc.arguments)?
            };
            let id = acc.id.unwrap_or_else(|| format!("call_{index}"));
            content.push(ContentBlock::ToolUse(ToolCall { id, name, input }));
        }

        let stop_reason = stop_reason.unwrap_or(if has_tool_calls {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        Ok(CompletionResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: usage.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::wire::response_to_domain;
    use crate::provider::{StreamEvent, collect_stream, wire};
    use serde_json::json;

    /// Drive payloads through parser + assembler (without HTTP).
    fn assemble(payloads: &[&str]) -> CompletionResponse {
        let mut parser = StreamParser::default();
        let mut assembler = StreamAssembler::new();
        for payload in payloads {
            for event in parser.push(payload).unwrap() {
                assembler.push(event);
            }
        }
        if !parser.done_seen {
            assembler.push(parser.finish());
        }
        assembler.finish().unwrap()
    }

    fn data(value: &serde_json::Value) -> String {
        format!("data: {value}\n\n")
    }

    #[test]
    fn text_deltas_reassemble_into_text_message() {
        let response = assemble(&[
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            r#"{"choices":[{"delta":{"content":"Hello, "}}]}"#,
            r#"{"choices":[{"delta":{"content":"world."}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#,
            "[DONE]",
        ]);
        assert_eq!(
            response.message,
            Message::text(Role::Assistant, "Hello, world.")
        );
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 3,
                output_tokens: 2,
                total_tokens: 5
            }
        );
    }

    #[test]
    fn tool_call_fragments_reassemble_across_chunks() {
        // id/name only in the first fragment; arguments split mid-JSON.
        let response = assemble(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"read","arguments":""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"src/lib.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(response.message.role, Role::Assistant);
        assert_eq!(response.message.content.len(), 1);
        match &response.message.content[0] {
            ContentBlock::ToolUse(call) => {
                assert_eq!(call.id, "c1");
                assert_eq!(call.name, "read");
                assert_eq!(call.input, json!({"path": "src/lib.rs"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        // No usage chunk → zero default (mirrors non-streaming).
        assert_eq!(response.usage, Usage::default());
    }

    #[test]
    fn multiple_tool_calls_in_one_chunk_keep_index_order() {
        let response = assemble(&[
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":1,"id":"c2","function":{"name":"shell","arguments":"{}"}},
                {"index":0,"id":"c1","function":{"name":"read","arguments":"{}"}}
            ]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]);
        let names: Vec<_> = response
            .message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolUse(call) => call.name.as_str(),
                other => panic!("expected tool_use, got {other:?}"),
            })
            .collect();
        // Arrived as [1, 0] — must reassemble as [0, 1].
        assert_eq!(names, ["read", "shell"]);
    }

    #[test]
    fn length_finish_reason_maps_to_max_tokens() {
        let response = assemble(&[
            r#"{"choices":[{"delta":{"content":"trunc"},"finish_reason":"length"}]}"#,
            "[DONE]",
        ]);
        assert_eq!(response.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn unknown_finish_reason_fails_loud_mid_stream() {
        let mut parser = StreamParser::default();
        let err = parser
            .push(r#"{"choices":[{"delta":{},"finish_reason":"quantum_flux"}]}"#)
            .unwrap_err();
        assert!(matches!(
            err,
            ProviderError::UnknownFinishReason(reason) if reason == "quantum_flux"
        ));
    }

    #[test]
    fn eof_without_done_marker_still_terminates() {
        let mut parser = StreamParser::default();
        parser
            .push(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":""}}]}}]}"#)
            .unwrap();
        assert!(!parser.done_seen);
        // Inferred from the message shape, same as non-streaming.
        assert_eq!(
            parser.finish(),
            StreamEvent::Done {
                stop_reason: StopReason::ToolUse
            }
        );
    }

    #[test]
    fn missing_tool_call_name_fails_as_decode() {
        let mut parser = StreamParser::default();
        let mut assembler = StreamAssembler::new();
        for event in parser
            .push(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"arguments":""}}]}}]}"#)
            .unwrap()
        {
            assembler.push(event);
        }
        let err = assembler.finish().unwrap_err();
        assert!(matches!(err, ProviderError::Decode(_)), "got {err:?}");
    }

    #[test]
    fn missing_tool_call_id_gets_placeholder() {
        let response = assemble(&[
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"x","arguments":""}}]}}]}"#,
            "[DONE]",
        ]);
        match &response.message.content[0] {
            ContentBlock::ToolUse(call) => assert_eq!(call.id, "call_0"),
            other => panic!("expected tool_use, got {other:?}"),
        }
    }

    #[test]
    fn malformed_arguments_fail_as_decode_not_panic() {
        let mut parser = StreamParser::default();
        let mut assembler = StreamAssembler::new();
        for event in parser
            .push(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"x","arguments":"{not json"}}]}}]}"#)
            .unwrap()
        {
            assembler.push(event);
        }
        assert!(matches!(assembler.finish(), Err(ProviderError::Decode(_))));
    }

    #[test]
    fn streaming_wire_request_carries_stream_flag_and_usage_option() {
        let body = wire::request_to_streaming_wire(crate::provider::CompletionRequest {
            model: "m".into(),
            messages: vec![],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        });
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["stream"], json!(true));
        assert_eq!(value["stream_options"], json!({"include_usage": true}));
    }

    // --- done-when: streamed ≡ non-streamed -------------------------------

    const PROP_TEXT: &str = "Reading the file now. Café ✓ Almost done.";

    /// Canonical non-streaming response — the target the stream must reach.
    fn canonical_response() -> CompletionResponse {
        let json = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": PROP_TEXT,
                    "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "read", "arguments": "{\"path\":\"src/lib.rs\"}"}},
                        {"id": "c2", "type": "function",
                         "function": {"name": "shell", "arguments": "{\"cmd\":\"ls -la\"}"}}
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
        });
        response_to_domain(serde_json::from_value(json).unwrap()).unwrap()
    }

    /// The same turn as an SSE byte stream, with arguments split mid-JSON
    /// and content split around non-ASCII characters.
    fn canonical_sse() -> String {
        let mut out = String::new();
        let mut push = |value: serde_json::Value| out.push_str(&data(&value));
        push(json!({"choices": [{"delta": {"role": "assistant"}, "finish_reason": null}]}));
        push(
            json!({"choices": [{"delta": {"content": "Reading the file now. Ca"}, "finish_reason": null}]}),
        );
        push(
            json!({"choices": [{"delta": {"content": "fé ✓ Almost done."}, "finish_reason": null}]}),
        );
        push(json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"pa"}}
        ]}, "finish_reason": null}]}));
        push(json!({"choices": [{"delta": {"tool_calls": [
            {"index": 0, "function": {"arguments": "th\":\"src/lib.rs\"}"}}
        ]}, "finish_reason": null}]}));
        push(json!({"choices": [{"delta": {"tool_calls": [
            {"index": 1, "id": "c2", "type": "function", "function": {"name": "shell", "arguments": "{\"cmd\""}}
        ]}, "finish_reason": null}]}));
        push(json!({"choices": [{"delta": {"tool_calls": [
            {"index": 1, "function": {"arguments": ":\"ls -la\"}"}}
        ]}, "finish_reason": null}]}));
        push(json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}));
        push(
            json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}}),
        );
        out.push_str("data: [DONE]\n\n");
        out
    }

    /// Property: for every chunk boundary, the streamed completion is
    /// byte-identical to the non-streaming one (plan done-when).
    #[tokio::test]
    async fn streamed_output_reassembles_identical_to_non_streaming() {
        let expected = canonical_response();
        let sse = canonical_sse();

        for size in [sse.len(), 64, 13, 7, 5, 2, 1] {
            let chunks: Vec<Result<Bytes, reqwest::Error>> = sse
                .as_bytes()
                .chunks(size)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            let stream = event_stream(futures::stream::iter(chunks));
            let got = collect_stream(stream)
                .await
                .unwrap_or_else(|err| panic!("chunk size {size}: {err}"));
            assert_eq!(got, expected, "chunk size {size} diverged");
        }
    }

    // --- cancellation ------------------------------------------------------

    /// A stream that yields exactly one event, then pends forever.
    fn one_event_then_pending() -> EventStream {
        Box::pin(
            futures::stream::once(futures::future::ready(Ok(StreamEvent::DeltaText(
                "partial".into(),
            ))))
            .chain(futures::stream::pending()),
        )
    }

    /// `tokio::select!` cancellation (the pattern the loop uses for
    /// Ctrl-C): dropping the stream stops consumption mid-flight and the
    /// partial progress survives.
    #[tokio::test]
    async fn select_cancellation_preserves_partial_progress() {
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let mut stream = one_event_then_pending();
            let mut assembler = StreamAssembler::new();
            tokio::select! {
                biased;
                _ = &mut rx => return None,
                event = stream.next() => assembler.push(
                    event.expect("stream yields once").expect("no stream error"),
                ),
            }
            tokio::select! {
                biased;
                _ = &mut rx => {}
                _ = stream.next() => panic!("stream must pend forever"),
            }
            Some(assembler)
        });

        tokio::task::yield_now().await; // let the task park in its selects
        tx.send(()).unwrap();

        let assembler = handle.await.unwrap().expect("cancelled, not dropped");
        let response = assembler.finish().unwrap();
        assert_eq!(response.message, Message::text(Role::Assistant, "partial"));
        // Never saw Done — stop reason inferred, as in non-streaming.
        assert_eq!(response.stop_reason, StopReason::EndTurn);
    }

    /// Real Ctrl-C: register the SIGINT handler first, raise it in-process,
    /// and cancel a pending stream through `tokio::select!`.
    /// (Isolated per process under cargo-nextest — `just test`.)
    #[tokio::test]
    async fn ctrl_c_cancels_stream_mid_flight() {
        let mut ctrl = std::pin::pin!(tokio::signal::ctrl_c());
        // First poll registers the SIGINT handler; must still be pending.
        match futures::poll!(ctrl.as_mut()) {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(Err(err)) => panic!("SIGINT handler failed: {err}"),
            std::task::Poll::Ready(Ok(())) => panic!("Ctrl-C fired before it was raised"),
        }

        let mut stream = one_event_then_pending();
        let mut assembler = StreamAssembler::new();

        tokio::select! {
            biased;
            _ = ctrl.as_mut() => panic!("Ctrl-C before the first event"),
            event = stream.next() => assembler.push(
                event.expect("stream yields once").expect("no stream error"),
            ),
        }

        unsafe { libc::raise(libc::SIGINT) };

        tokio::select! {
            biased;
            _ = ctrl.as_mut() => {}
            _ = stream.next() => panic!("stream must not win against Ctrl-C"),
        }

        let response = assembler.finish().unwrap();
        assert_eq!(response.message, Message::text(Role::Assistant, "partial"));
    }
}
