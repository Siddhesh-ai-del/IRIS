//! OpenAI-compatible HTTP client (OpenRouter by default — decision D4).
//!
//! Auth: `Authorization: Bearer $OPENROUTER_API_KEY` only (D6 — the key is
//! passed in by the caller, never read from config files here).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use super::wire::{self, WireRequest, WireResponse};
use super::{
    CONNECT_TIMEOUT_SECS, CompletionRequest, CompletionResponse, DEFAULT_TIMEOUT_SECS, EventStream,
    Provider, ProviderError, StreamEvent,
};

/// One HTTP exchange captured verbatim for a cassette (stage 7.1) —
/// the record half of the record/replay layer.
///
/// **Headers are deliberately never captured**: the API key travels in
/// `Authorization`, so a recorded file can never contain one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapturedExchange {
    /// The exact JSON body that was POSTed.
    pub request_body: String,
    /// HTTP status code of the response.
    pub status: u16,
    /// The response `content-type` — replay must serve
    /// `text/event-stream` for streams to parse.
    pub content_type: String,
    /// The response body as received — raw SSE for streams.
    pub response_body: String,
}

/// Who to hand a finished exchange to ([`OpenAiClient::with_recorder`]).
type Recorder = Arc<dyn Fn(CapturedExchange) + Send + Sync>;

/// Non-streaming `POST {base_url}/chat/completions` client.
pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
    /// Cassette recorder (stage 7.1) — `None` in production.
    recorder: Option<Recorder>,
}

impl OpenAiClient {
    /// `base_url` without a trailing slash, e.g. `https://openrouter.ai/api/v1`.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
                .build()
                .expect("rustls-backed reqwest client cannot fail to build"),
            recorder: None,
        }
    }

    /// Attach a cassette recorder (stage 7.1): every response the
    /// client *fully reads* is handed over verbatim — a streamed
    /// response only once its terminal event arrived or the stream was
    /// drained, so a cancelled run records nothing. Test/record tooling
    /// only; production builds carry `None` and behave identically.
    pub fn with_recorder(
        mut self,
        recorder: impl Fn(CapturedExchange) + Send + Sync + 'static,
    ) -> Self {
        self.recorder = Some(Arc::new(recorder));
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    /// The exact JSON about to go on the wire — computed only when
    /// someone is recording; production serializes once, as before.
    fn capture_request<T: serde::Serialize>(
        &self,
        body: &T,
    ) -> Result<Option<String>, ProviderError> {
        match &self.recorder {
            Some(_) => Ok(Some(
                serde_json::to_string(body)
                    .map_err(|err| ProviderError::Decode(err.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// Hand a fully-read response to the recorder, if any.
    fn record(
        &self,
        captured: Option<String>,
        status: reqwest::StatusCode,
        content_type: String,
        response_body: String,
    ) {
        if let (Some(recorder), Some(request_body)) = (&self.recorder, captured) {
            recorder(CapturedExchange {
                request_body,
                status: status.as_u16(),
                content_type,
                response_body,
            });
        }
    }

    fn map_http_error(status: reqwest::StatusCode, body: String) -> ProviderError {
        // OpenRouter/OpenAI error body: {"error": {"message": "..."}}
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(|message| message.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    status.to_string()
                } else {
                    trimmed.chars().take(200).collect()
                }
            });
        ProviderError::Api {
            status: status.as_u16(),
            message,
        }
    }
}

impl Provider for OpenAiClient {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let body: WireRequest = wire::request_to_wire(request);
        let captured = self.capture_request(&body)?;
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let content_type = content_type_of(&response);
        let text = response.text().await?;
        // Recorded whatever the status — an error exchange is a
        // fully-read response like any other.
        self.record(captured, status, content_type, text.clone());
        if !status.is_success() {
            return Err(Self::map_http_error(status, text));
        }

        let wire_response: WireResponse = serde_json::from_str(&text)?;
        wire::response_to_domain(wire_response)
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> Result<EventStream, ProviderError> {
        let body = wire::request_to_streaming_wire(request);
        let captured = self.capture_request(&body)?;
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            // No total timeout: a generation may run for minutes (the
            // client caps connection setup instead).
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let content_type = content_type_of(&response);
            let text = response.text().await?;
            self.record(captured, status, content_type, text.clone());
            return Err(Self::map_http_error(status, text));
        }
        let content_type = content_type_of(&response);
        let Some(recorder) = self.recorder.clone() else {
            return Ok(super::stream::event_stream(response.bytes_stream()));
        };
        let Some(request_body) = captured else {
            return Ok(super::stream::event_stream(response.bytes_stream()));
        };
        // Recording: tee the raw bytes for fidelity, and hand the
        // finished exchange over when the turn completes (terminal
        // `Done` seen or stream drained) — a dropped, cancelled stream
        // records nothing.
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let bytes = tee(response.bytes_stream(), Arc::clone(&buffer));
        let stream = super::stream::event_stream(bytes);
        Ok(record_on_end(
            stream,
            request_body,
            status.as_u16(),
            content_type,
            buffer,
            recorder,
        ))
    }
}

/// The response `content-type` header, verbatim ("" when absent).
fn content_type_of(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Pass byte chunks through while keeping a copy — only bytes the
/// consumer actually pulled end up in the recorder's buffer.
fn tee<S>(
    inner: S,
    buffer: Arc<Mutex<Vec<u8>>>,
) -> impl Stream<Item = Result<Bytes, reqwest::Error>> + Send
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send,
{
    inner.map(move |chunk| {
        if let Ok(bytes) = &chunk {
            buffer.lock().unwrap().extend_from_slice(bytes);
        }
        chunk
    })
}

/// The recorder's per-stream state (see [`record_on_end`]).
struct RecordState {
    inner: EventStream,
    request_body: String,
    status: u16,
    content_type: String,
    buffer: Arc<Mutex<Vec<u8>>>,
    recorder: Recorder,
    fired: bool,
}

/// Hand the finished exchange to the recorder exactly once — when the
/// terminal `Done` event passes by, or when the stream drains,
/// whichever comes first — and never for a dropped stream.
fn record_on_end(
    inner: EventStream,
    request_body: String,
    status: u16,
    content_type: String,
    buffer: Arc<Mutex<Vec<u8>>>,
    recorder: Recorder,
) -> EventStream {
    let state = RecordState {
        inner,
        request_body,
        status,
        content_type,
        buffer,
        recorder,
        fired: false,
    };
    Box::pin(futures::stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        match state.inner.next().await {
            Some(Ok(event)) => {
                if matches!(event, StreamEvent::Done { .. }) {
                    fire(&mut state);
                }
                Some((Ok(event), Some(state)))
            }
            Some(Err(err)) => Some((Err(err), Some(state))),
            None => {
                fire(&mut state);
                None
            }
        }
    }))
}

/// Capture the buffer into one exchange — idempotent (`fired`).
fn fire(state: &mut RecordState) {
    if state.fired {
        return;
    }
    state.fired = true;
    let response_body = String::from_utf8_lossy(&state.buffer.lock().unwrap()).into_owned();
    (state.recorder)(CapturedExchange {
        request_body: std::mem::take(&mut state.request_body),
        status: state.status,
        content_type: std::mem::take(&mut state.content_type),
        response_body,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, Role};
    use httpmock::prelude::*;
    use serde_json::json;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "test-model".into(),
            messages: vec![Message::text(Role::User, "hello")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    /// Register one canned chat-completions reply on an existing server.
    fn mock_chat(server: &MockServer, status: u16, body: serde_json::Value) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(POST).path("/chat/completions");
            then.status(status).json_body(body);
        })
    }

    fn canned_text_reply() -> serde_json::Value {
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hi!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        })
    }

    #[tokio::test]
    async fn non_streaming_completion_round_trip() {
        let server = MockServer::start();
        let mock = mock_chat(&server, 200, canned_text_reply());
        let client = OpenAiClient::new(server.base_url(), "test-key");

        let response = client.complete(request()).await.unwrap();

        assert_eq!(response.message, Message::text(Role::Assistant, "Hi!"));
        assert_eq!(response.usage.total_tokens, 7);
        mock.assert();
        // Exactly one POST to the documented endpoint.
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn sends_bearer_auth_and_wire_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/chat/completions")
                .header("Authorization", "Bearer sk-or-test-key")
                .body_includes("\"model\":\"test-model\"");
            then.status(200).json_body(canned_text_reply());
        });
        let client = OpenAiClient::new(server.base_url(), "sk-or-test-key");

        client.complete(request()).await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn api_error_status_becomes_typed_error_with_message() {
        let server = MockServer::start();
        let _mock = mock_chat(
            &server,
            401,
            json!({"error": {"message": "invalid api key", "code": 401}}),
        );
        let client = OpenAiClient::new(server.base_url(), "bad-key");

        let err = client.complete(request()).await.unwrap_err();
        match err {
            ProviderError::Api { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, "invalid api key");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_json_error_body_falls_back_to_truncated_body() {
        let server = MockServer::start();
        // Plain-text (non-JSON) error body, e.g. an HTML proxy error page.
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/chat/completions");
            then.status(500).body("upstream exploded");
        });
        let client = OpenAiClient::new(server.base_url(), "k");

        let err = client.complete(request()).await.unwrap_err();
        match err {
            ProviderError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "upstream exploded");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_success_body_is_decode_error() {
        let server = MockServer::start();
        let _mock = mock_chat(&server, 200, json!({"unexpected": true}));
        let client = OpenAiClient::new(server.base_url(), "k");

        let err = client.complete(request()).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "expected Decode, got {err:?}"
        );
    }

    #[tokio::test]
    async fn default_base_url_targets_openrouter() {
        // Stage 0.2 wires the config default; here we pin the endpoint shape.
        let client = OpenAiClient::new("https://openrouter.ai/api/v1", "k");
        assert_eq!(
            client.endpoint(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn trailing_slash_on_base_url_is_normalized() {
        let client = OpenAiClient::new("https://example.com/v1/", "k");
        assert_eq!(client.endpoint(), "https://example.com/v1/chat/completions");
    }

    #[tokio::test]
    async fn wire_request_body_is_valid_json_shape() {
        // Guard the exact JSON we put on the wire (in addition to wire tests).
        let body = wire::request_to_wire(request());
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "hello");
        // Non-streaming must not carry stream flags.
        assert!(value.get("stream").is_none());
        assert!(value.get("stream_options").is_none());
    }

    // --- streaming (stage 1.3) -------------------------------------------

    /// A canned SSE body: text, then a tool call with arguments split
    /// mid-JSON, then finish/usage/[DONE].
    const SSE_BODY: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Reading \"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"file.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",",
        "\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.rs\\\"}\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":4,\"total_tokens\":13}}\n\n",
        "data: [DONE]\n\n",
    );

    #[tokio::test]
    async fn complete_stream_reassembles_response_from_sse() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/chat/completions")
                .header("Authorization", "Bearer sk-or-test-key")
                .body_includes("\"stream\":true")
                .body_includes("\"include_usage\":true");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(SSE_BODY);
        });
        let client = OpenAiClient::new(server.base_url(), "sk-or-test-key");

        let stream = client.complete_stream(request()).await.unwrap();
        let response = crate::provider::collect_stream(stream).await.unwrap();

        assert_eq!(
            response.message.content[0],
            crate::types::ContentBlock::Text {
                text: "Reading file.".into()
            }
        );
        match &response.message.content[1] {
            crate::types::ContentBlock::ToolUse(call) => {
                assert_eq!(call.name, "read");
                assert_eq!(call.input, serde_json::json!({"path": "a.rs"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(response.stop_reason, crate::types::StopReason::ToolUse);
        assert_eq!(response.usage.total_tokens, 13);
        mock.assert();
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn complete_stream_http_error_fails_before_any_event() {
        let server = MockServer::start();
        let _mock = mock_chat(
            &server,
            429,
            json!({"error": {"message": "rate limited", "code": 429}}),
        );
        let client = OpenAiClient::new(server.base_url(), "bad-key");

        let err = client
            .complete_stream(request())
            .await
            .err()
            .expect("429 must fail before any event");
        match err {
            ProviderError::Api { status, message } => {
                assert_eq!(status, 429);
                assert_eq!(message, "rate limited");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }
}
