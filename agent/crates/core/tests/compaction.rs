//! Stage 6.2 — compaction that fires: the loop folds at **70% of the
//! estimated** context window *before* the request goes out, keeps
//! system text and tool pairs intact, redacts the summary through the
//! stage-5.1 chokepoint, and the compacted context survives a resume
//! byte-identically.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use futures::stream;
use time::OffsetDateTime;

use iris_core::agent_loop::{LoopEvent, LoopOptions, LoopOutcome, Recorded, run_recorded};
use iris_core::compact::{self, COMPACT_AT_PCT, CompactOpts, summary_message};
use iris_core::config::{ConfigInputs, DEFAULT_COMPACT_KEEP, DEFAULT_CONTEXT_WINDOW, load_config};
use iris_core::provider::{
    CompletionRequest, CompletionResponse, EventStream, Provider, ProviderError, StreamEvent,
};
use iris_core::session::resume::rehydrate;
use iris_core::session::schema::Event;
use iris_core::tokens;
use iris_core::tools::{Decision, DenyOnAsk, ToolContext, ToolRegistry};
use iris_core::types::{ContentBlock, Message, Role, StopReason, Usage};

// --- fixtures ------------------------------------------------------------

/// Which side of the loop a scripted call came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    /// `Provider::complete` — the stage-6.2 summarizer.
    Summarize,
    /// `Provider::complete_stream` — a conversational turn.
    Turn,
}

/// Provider that scripts summarizer and turn responses separately and
/// logs every request with which side asked for it.
struct Scripted {
    summarize: Mutex<VecDeque<CompletionResponse>>,
    turns: Mutex<VecDeque<CompletionResponse>>,
    log: Mutex<Vec<(Call, CompletionRequest)>>,
}

impl Scripted {
    fn new(summarize: Vec<CompletionResponse>, turns: Vec<CompletionResponse>) -> Self {
        Self {
            summarize: Mutex::new(summarize.into()),
            turns: Mutex::new(turns.into()),
            log: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<(Call, CompletionRequest)> {
        self.log.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.log.lock().unwrap().push((Call::Summarize, request));
        self.summarize
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ProviderError::NoChoices)
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> Result<EventStream, ProviderError> {
        self.log.lock().unwrap().push((Call::Turn, request));
        let response = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ProviderError::NoChoices)?;
        Ok(Box::pin(stream::iter(events_for(&response))))
    }
}

/// Re-encode a response as the stream events its SSE body would carry.
fn events_for(response: &CompletionResponse) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut events: Vec<Result<StreamEvent, ProviderError>> = response
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(Ok(StreamEvent::DeltaText(text.clone()))),
            ContentBlock::ToolUse(call) => Some(Ok(StreamEvent::DeltaToolCall {
                index: 0,
                id: Some(call.id.clone()),
                name: Some(call.name.clone()),
                arguments_delta: call.input.to_string(),
            })),
            ContentBlock::Image { .. } | ContentBlock::ToolResult(_) => None,
        })
        .collect();
    events.push(Ok(StreamEvent::Usage(response.usage.clone())));
    events.push(Ok(StreamEvent::Done {
        stop_reason: response.stop_reason.clone(),
    }));
    events
}

fn text_response(text: &str) -> CompletionResponse {
    CompletionResponse {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    }
}

/// system + `n` user/assistant pairs with enough body text to register
/// in the token estimate (no tools — see the tool-pair test for that).
fn conversation(n: usize) -> Vec<Message> {
    let mut messages = vec![Message::text(Role::System, "You are iris.")];
    for i in 0..n {
        let body = format!(
            "step {i}: {}",
            "the agent examined the repository and recorded detailed findings. ".repeat(20)
        );
        messages.push(Message::text(Role::User, &body));
        messages.push(Message::text(
            Role::Assistant,
            format!("acknowledged step {i}"),
        ));
    }
    messages
}

/// The loop's own pre-request estimate for these messages.
fn estimate(messages: &[Message]) -> u64 {
    tokens::estimate_input(&CompletionRequest {
        model: "test-model".into(),
        messages: messages.to_vec(),
        tools: vec![],
        max_tokens: None,
        temperature: None,
    })
}

fn opts(window: u64, keep: usize) -> LoopOptions {
    LoopOptions {
        model: "test-model".into(),
        max_turns: 5,
        max_tokens_budget: u64::MAX,
        max_tokens: None,
        temperature: None,
        price: None,
        compact: Some(CompactOpts { window, keep }),
    }
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn allow_gate() -> iris_core::tools::Gate {
    Arc::new(|_: &str, _: &str| Decision::Allow)
}

/// What the observer saw, as owned data.
#[derive(Debug, PartialEq)]
enum Seen {
    Compact { folded: usize, kept: usize },
    Bust,
    Other,
}

// --- the trigger ---------------------------------------------------------

#[test]
fn the_trigger_is_exactly_70_percent() {
    let knobs = CompactOpts {
        window: 1000,
        keep: 4,
    };
    assert!(
        !compact::due(699, knobs),
        "one token below the trigger never fires"
    );
    assert!(compact::due(700, knobs), "exactly 70% must fire");
    assert!(compact::due(1000, knobs), "past the trigger fires");
    assert!(
        !compact::due(50_000, CompactOpts { window: 0, keep: 4 }),
        "window 0 means compaction is off"
    );
}

// --- the fold ------------------------------------------------------------

#[test]
fn a_fold_never_splits_a_tool_use_from_its_result() {
    let messages = vec![
        Message::text(Role::System, "sys"),
        Message::text(Role::User, "read the file"),
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse(iris_core::types::ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            })],
        },
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult(iris_core::types::ToolResult {
                tool_use_id: "c1".into(),
                content: "bytes".into(),
                is_error: false,
            })],
        },
    ];

    // Naive keep-last-1 would keep the orphaned tool_result; the plan
    // walks the boundary back to the assistant so the pair survives.
    let plan = compact::plan(&messages, 1).expect("a safe fold exists");
    assert_eq!(
        (plan.start, plan.boundary, plan.folded),
        (1, 2, 1),
        "boundary sits on the assistant; one fewer message folds"
    );

    let view = compact::apply(messages.clone(), plan, "S");
    assert_eq!(view.len(), 4, "system + summary + assistant + result");
    assert_eq!(view[0], messages[0], "system untouched");
    assert_eq!(view[1], summary_message("S"), "summary pinned at the fold");
    assert_eq!(view[2].role, Role::Assistant, "pair starts with tool_use");
    assert_eq!(view[3].role, Role::Tool, "its result follows");

    // Fewer messages than `keep` — nothing to fold, safely refused.
    assert_eq!(compact::plan(&messages[1..2], 1), None);
}

// --- the loop ------------------------------------------------------------

#[tokio::test]
async fn compaction_fires_at_70_percent_before_the_request() {
    let messages = conversation(4); // 8 non-system messages
    let keep = 2usize;
    let est = estimate(&messages);
    // The largest window that still reads "at 70%" for this estimate —
    // due() is then true by integer math, not by luck.
    let window = est * 100 / COMPACT_AT_PCT;
    assert!(
        est * 100 >= COMPACT_AT_PCT * window,
        "the fixture must sit at the trigger"
    );

    let scripted = Scripted::new(
        vec![text_response("PINNED-SUMMARY")],
        vec![text_response("final answer")],
    );
    let dir = workspace();
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut seen = Vec::new();
    let mut recorded: Vec<Recorded> = Vec::new();

    let outcome = run_recorded(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &DenyOnAsk,
        &opts(window, keep),
        messages.clone(),
        |event| {
            seen.push(match event {
                LoopEvent::Compact { folded, kept } => Seen::Compact { folded, kept },
                LoopEvent::CacheBust { .. } => Seen::Bust,
                _ => Seen::Other,
            });
        },
        |entry| {
            recorded.push(entry);
            Ok(())
        },
    )
    .await
    .expect("the run completes");

    assert!(matches!(outcome, LoopOutcome::Completed { .. }));

    // Exactly two provider calls: summarize, then the real turn.
    let calls = scripted.calls();
    let kinds: Vec<Call> = calls.iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, vec![Call::Summarize, Call::Turn], "got {kinds:?}");
    assert!(
        calls[0].1.tools.is_empty(),
        "the summarizer request never carries tools"
    );

    // Exactly one fold: 8 raw − keep(2) = 6, recorded with the summary.
    let compacts: Vec<_> = recorded
        .iter()
        .filter_map(|entry| match entry {
            Recorded::Compact {
                summary,
                folded,
                keep,
            } => Some((summary.clone(), *folded, *keep)),
            _ => None,
        })
        .collect();
    assert_eq!(
        compacts.len(),
        1,
        "exactly one compaction, got {compacts:?}"
    );
    assert_eq!(compacts[0].1, 6, "folded = non-system − keep");
    assert_eq!(compacts[0].2, keep);

    // What actually went to the provider: system byte-identical, then
    // the pinned summary, then exactly the kept tail.
    let sent = &calls[1].1.messages;
    assert_eq!(sent.len(), 1 + 1 + keep, "system + summary + kept tail");
    assert_eq!(sent[0], messages[0], "system must be byte-identical");
    assert_eq!(sent[1], summary_message("PINNED-SUMMARY"));
    assert_eq!(
        &sent[2..],
        &messages[messages.len() - keep..],
        "the kept tail is the real last messages, unpinned"
    );

    // Folding is a message-history event, never a prefix change.
    assert!(
        !seen.contains(&Seen::Bust),
        "compaction must not bust the cacheable prefix"
    );
    assert!(
        seen.contains(&Seen::Compact {
            folded: 6,
            kept: keep
        }),
        "the observer is told about the fold: {seen:?}"
    );
}

#[tokio::test]
async fn below_the_threshold_nothing_folds_and_no_summarizer_runs() {
    let messages = conversation(4);
    let est = estimate(&messages);
    let window = est * 2; // the estimate is 50% of the window — under 70%

    let scripted = Scripted::new(vec![], vec![text_response("final answer")]);
    let dir = workspace();
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut recorded: Vec<Recorded> = Vec::new();

    let outcome = run_recorded(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &DenyOnAsk,
        &opts(window, 2),
        messages.clone(),
        |_| {},
        |entry| {
            recorded.push(entry);
            Ok(())
        },
    )
    .await
    .expect("the run completes");

    assert!(matches!(outcome, LoopOutcome::Completed { .. }));
    let kinds: Vec<Call> = scripted.calls().iter().map(|(kind, _)| *kind).collect();
    assert_eq!(kinds, vec![Call::Turn], "no summarizer call below 70%");
    assert!(
        !recorded
            .iter()
            .any(|entry| matches!(entry, Recorded::Compact { .. })),
        "nothing recorded below the trigger"
    );
    assert_eq!(
        scripted.calls()[0].1.messages,
        messages,
        "the untouched conversation goes out as-is"
    );
}

// --- survives resume -----------------------------------------------------

#[tokio::test]
async fn the_compaction_survives_resume_byte_identically() {
    let messages = conversation(4);
    let keep = 2usize;
    let window = estimate(&messages) * 100 / COMPACT_AT_PCT;

    let scripted = Scripted::new(
        vec![text_response("PINNED-SUMMARY")],
        vec![text_response("final answer")],
    );
    let dir = workspace();
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut recorded: Vec<Recorded> = Vec::new();

    run_recorded(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &DenyOnAsk,
        &opts(window, keep),
        messages.clone(),
        |_| {},
        |entry| {
            recorded.push(entry);
            Ok(())
        },
    )
    .await
    .expect("the run completes");

    // The transcript as it stood at compaction time: session_start, the
    // raw history (append-only), then the compact event.
    let mut events = vec![Event::SessionStart {
        session_id: "s1".into(),
        started_at: OffsetDateTime::now_utc(),
        cwd: "/w".into(),
        model: "test-model".into(),
        iris_version: "0.1.0".into(),
    }];
    for message in &messages {
        events.push(Event::Message(message.clone()));
    }
    for entry in &recorded {
        if let Recorded::Compact {
            summary,
            folded,
            keep,
        } = entry
        {
            events.push(Event::Compact {
                summary: summary.clone(),
                folded: *folded,
                keep: *keep,
            });
        }
    }

    let sent = &scripted.calls()[1].1.messages; // the turn request
    let resumed = rehydrate(&events).expect("rehydrate");
    assert_eq!(
        resumed, *sent,
        "resume rebuilds the compacted context byte-identically"
    );

    // Messages appended after the fold ride on top of the resumed view.
    let after = Message::text(Role::User, "next task");
    let mut extended = events.clone();
    extended.push(Event::Message(after.clone()));
    let mut expected = (*sent).clone();
    expected.push(after);
    assert_eq!(rehydrate(&extended).expect("rehydrate"), expected);
}

// --- redaction chokepoint (stage 5.1 discipline) -------------------------

#[test]
fn the_summary_passes_the_redaction_chokepoint() {
    let event = Event::Compact {
        summary: "left AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE in the script".into(),
        folded: 4,
        keep: 2,
    };
    let redacted = iris_core::redact::redact_event(event);
    let Event::Compact { summary, .. } = redacted else {
        panic!("redaction must keep the event's shape");
    };
    assert!(
        !summary.contains("AKIAIOSFODNN7EXAMPLE"),
        "raw secret survived: {summary}"
    );
    assert!(summary.contains("[REDACTED:aws-access-key]"), "{summary}");
}

// --- config plumbing -----------------------------------------------------

#[test]
fn compaction_knobs_layer_from_file_and_env_into_the_loop() {
    let load = |file: Option<&str>, env: HashMap<String, String>| {
        load_config(&ConfigInputs {
            file_toml: file,
            env: &env,
            flag_base_url: None,
        })
        .expect("valid test config")
    };

    // File layer reaches the loop.
    let file = "context_window = 50000\ncompact_keep = 3";
    let config = load(Some(file), HashMap::new());
    assert_eq!(
        LoopOptions::from_config(&config, "m").compact,
        Some(CompactOpts {
            window: 50_000,
            keep: 3
        }),
        "TOML knobs must reach LoopOptions"
    );

    // Env beats file; 0 turns compaction off.
    let mut env = HashMap::new();
    env.insert("IRIS_CONTEXT_WINDOW".to_string(), "0".to_string());
    let config = load(Some(file), env);
    assert_eq!(
        LoopOptions::from_config(&config, "m").compact,
        None,
        "IRIS_CONTEXT_WINDOW=0 disables compaction"
    );

    // Default layer: compaction on, with the documented defaults.
    let config = load(None, HashMap::new());
    assert_eq!(
        LoopOptions::from_config(&config, "m").compact,
        Some(CompactOpts {
            window: DEFAULT_CONTEXT_WINDOW,
            keep: DEFAULT_COMPACT_KEEP
        }),
        "default config compacts at the 128k default"
    );
}
