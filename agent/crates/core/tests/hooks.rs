//! Stage 6.4 — user hooks: configured shell commands fired at the
//! loop's lifecycle points (`on_tool_pre`, `on_tool_post`,
//! `on_turn_end`, `on_compact`) with a redacted JSON payload on stdin,
//! a hard timeout, and **advisory** failures — a hook that fails,
//! hangs, or is missing must never stop the run.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::stream;
use serde_json::{Value, json};

use iris_core::agent_loop::{LoopEvent, LoopOptions, LoopOutcome, run};
use iris_core::compact::{COMPACT_AT_PCT, CompactOpts};
use iris_core::config::Hooks;
use iris_core::provider::{
    CompletionRequest, CompletionResponse, EventStream, Provider, ProviderError, StreamEvent,
};
use iris_core::tokens;
use iris_core::tools::{Decision, DenyOnAsk, Gate, ToolContext, ToolRegistry, WriteTool};
use iris_core::types::{ContentBlock, Message, Role, StopReason, ToolCall, Usage};

// --- fixtures ------------------------------------------------------------

/// Provider that scripts summarizer (`complete`) and turn
/// (`complete_stream`) responses separately — the stage-6.2 compaction
/// test's fixture, copied so this file can script both call paths.
struct Scripted {
    summarize: Mutex<VecDeque<CompletionResponse>>,
    turns: Mutex<VecDeque<CompletionResponse>>,
}

impl Scripted {
    fn new(summarize: Vec<CompletionResponse>, turns: Vec<CompletionResponse>) -> Self {
        Self {
            summarize: Mutex::new(summarize.into()),
            turns: Mutex::new(turns.into()),
        }
    }
}

impl Provider for Scripted {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.summarize
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ProviderError::NoChoices)
    }

    async fn complete_stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<EventStream, ProviderError> {
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

fn tool_response(id: &str, name: &str, input: Value) -> CompletionResponse {
    CompletionResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse(ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                input,
            })],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
    }
}

/// system + `n` user/assistant pairs with enough body text to register
/// in the token estimate (the stage-6.2 compaction fixture, so the
/// fold counts here match its numbers).
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

fn options(hooks: Hooks) -> LoopOptions {
    LoopOptions {
        model: "test-model".into(),
        max_turns: 5,
        max_tokens_budget: u64::MAX,
        max_tokens: None,
        temperature: None,
        price: None,
        compact: None,
        hooks,
    }
}

fn initial() -> Vec<Message> {
    vec![
        Message::text(Role::System, "You are iris."),
        Message::text(Role::User, "go"),
    ]
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn allow_gate() -> Gate {
    Arc::new(|_: &str, _: &str| Decision::Allow)
}

/// A builtin tool registry that can really write (the loop test's
/// dispatch sandwich needs a tool that exists).
fn write_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(WriteTool);
    registry
}

/// Read a hook's payload file and parse it — panicking with context
/// if the hook never fired or wrote something that is not JSON.
fn payload_of(path: &std::path::Path) -> Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("hook did not write {}: {err}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|err| panic!("payload is not JSON ({err}): {raw}"))
}

// --- the payload ---------------------------------------------------------

#[tokio::test]
async fn tool_hooks_fire_around_a_dispatch_with_the_call_payload() {
    let dir = workspace();
    let pre_path = dir.path().join("pre.json");
    let post_path = dir.path().join("post.json");
    let hooks = Hooks {
        on_tool_pre: vec![format!("cat > {}", pre_path.display())],
        on_tool_post: vec![format!("cat > {}", post_path.display())],
        ..Hooks::default()
    };
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "notes.txt", "content": "hi"}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |_| {},
    )
    .await
    .expect("the run completes");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));
    assert!(dir.path().join("notes.txt").exists(), "the tool ran");

    let pre = payload_of(&pre_path);
    assert_eq!(pre["event"], "on_tool_pre");
    assert_eq!(pre["turn"], 1);
    assert_eq!(pre["model"], "test-model");
    assert_eq!(pre["workspace_root"], dir.path().display().to_string());
    assert_eq!(pre["tool"]["name"], "write_file");
    assert_eq!(pre["tool"]["id"], "call-1");
    assert_eq!(pre["tool"]["input"]["path"], "notes.txt");

    let post = payload_of(&post_path);
    assert_eq!(post["event"], "on_tool_post");
    assert_eq!(post["turn"], 1);
    assert_eq!(post["tool"]["name"], "write_file");
    assert_eq!(post["tool"]["id"], "call-1");
    assert_eq!(post["tool"]["is_error"], false);
    assert!(
        post["tool"]["content"].is_string(),
        "the outcome rides along: {post}"
    );
}

#[tokio::test]
async fn turn_end_fires_once_per_turn_with_stop_reason_and_usage() {
    let dir = workspace();
    let path = dir.path().join("turns.jsonl");
    // Payloads arrive newline-terminated, so two fires land as two
    // parseable JSON lines, not one glued blob.
    let hooks = Hooks {
        on_turn_end: vec![format!("cat >> {}", path.display())],
        ..Hooks::default()
    };
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "x.txt", "content": "y"}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |_| {},
    )
    .await
    .expect("the run completes");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));

    let raw = std::fs::read_to_string(&path).expect("on_turn_end fired");
    let events: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON payload"))
        .collect();
    assert_eq!(events.len(), 2, "one payload per turn: {raw}");
    assert_eq!(events[0]["event"], "on_turn_end");
    assert_eq!(events[0]["turn"], 1);
    assert_eq!(events[0]["stop_reason"], "tool_use");
    assert!(events[0]["usage"]["total_tokens"].is_u64());
    assert_eq!(events[1]["event"], "on_turn_end");
    assert_eq!(events[1]["turn"], 2);
    assert_eq!(events[1]["stop_reason"], "end_turn");
}

#[tokio::test]
async fn compact_hook_reports_the_fold_and_pinned_summary() {
    let dir = workspace();
    let path = dir.path().join("compact.json");
    let hooks = Hooks {
        on_compact: vec![format!("cat > {}", path.display())],
        ..Hooks::default()
    };
    let messages = conversation(4);
    let est = estimate(&messages);
    let window = est * 100 / COMPACT_AT_PCT; // exactly at the 70% trigger
    let mut opts = options(hooks);
    opts.compact = Some(CompactOpts { window, keep: 2 });

    let scripted = Scripted::new(
        vec![text_response("PINNED-SUMMARY")],
        vec![text_response("final answer")],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());

    let outcome = run(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &DenyOnAsk,
        &opts,
        messages,
        |_| {},
    )
    .await
    .expect("the run completes");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));

    let payload = payload_of(&path);
    assert_eq!(payload["event"], "on_compact");
    assert_eq!(payload["turn"], 1, "the fold precedes the next turn");
    assert_eq!(payload["summary"], "PINNED-SUMMARY");
    assert_eq!(payload["folded"], 6, "conversation(4) keeps the last 2");
    assert_eq!(payload["keep"], 2);
}

// --- advisory failures ---------------------------------------------------

#[tokio::test]
async fn a_failing_hook_warns_but_never_stops_the_run() {
    let dir = workspace();
    let hooks = Hooks {
        on_tool_pre: vec!["exit 9".into()],
        ..Hooks::default()
    };
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "x.txt", "content": "y"}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut warnings: Vec<String> = Vec::new();

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |event| {
            if let LoopEvent::HookWarning(message) = &event {
                warnings.push(message.clone());
            }
        },
    )
    .await
    .expect("the run still completes");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));
    assert_eq!(
        warnings.len(),
        1,
        "one failing command → one warning: {warnings:?}"
    );
    assert!(warnings[0].contains("on_tool_pre"), "{}", warnings[0]);
    assert!(warnings[0].contains("exit status: 9"), "{}", warnings[0]);
}

#[tokio::test]
async fn a_timed_out_hook_is_killed_and_the_run_continues_quickly() {
    let dir = workspace();
    let hooks = Hooks {
        on_turn_end: vec!["sleep 30".into()],
        timeout_secs: 1,
        ..Hooks::default()
    };
    let scripted = Scripted::new(vec![], vec![text_response("done")]);
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut warnings: Vec<String> = Vec::new();

    let started = Instant::now();
    let outcome = run(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |event| {
            if let LoopEvent::HookWarning(message) = &event {
                warnings.push(message.clone());
            }
        },
    )
    .await
    .expect("the run still completes");
    let elapsed = started.elapsed();
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));
    assert!(
        elapsed < Duration::from_secs(10),
        "the hook was killed at its 1s timeout, took {elapsed:?}"
    );
    assert_eq!(warnings.len(), 1, "warnings: {warnings:?}");
    assert!(
        warnings[0].contains("timed out after 1s"),
        "{}",
        warnings[0]
    );
}

// --- redaction + defaults ------------------------------------------------

#[tokio::test]
async fn payloads_pass_the_redaction_chokepoint_before_leaving() {
    let dir = workspace();
    let path = dir.path().join("pre.json");
    let key = "sk-or-v1-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
    let hooks = Hooks {
        on_tool_pre: vec![format!("cat > {}", path.display())],
        ..Hooks::default()
    };
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "x.txt", "content": key}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |_| {},
    )
    .await
    .expect("the run completes");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));

    let raw = std::fs::read_to_string(&path).expect("on_tool_pre fired");
    assert!(raw.contains("[REDACTED:openrouter-key]"), "{raw}");
    assert!(
        !raw.contains(key),
        "the key itself must never reach a hook: {raw}"
    );
}

#[tokio::test]
async fn default_hooks_are_silent() {
    let dir = workspace();
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "x.txt", "content": "y"}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut warnings = 0usize;

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(Hooks::default()),
        initial(),
        |event| {
            if matches!(event, LoopEvent::HookWarning(_)) {
                warnings += 1;
            }
        },
    )
    .await
    .expect("the run completes");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));
    assert_eq!(warnings, 0, "no configured hooks → nothing to warn about");
}

// --- coverage fills (stage 7.2): every advisory path a hook can take --------

#[test]
fn previews_are_capped_with_an_explicit_marker() {
    assert_eq!(iris_core::hooks::preview("short"), "short");
    let long = "é".repeat(5000);
    let out = iris_core::hooks::preview(&long);
    assert!(out.starts_with(&"é".repeat(4096)), "head kept verbatim");
    assert!(
        out.ends_with("…[iris truncated this preview]"),
        "the drop is stated, never silent"
    );
    assert_eq!(
        out.chars().count(),
        4096 + "…[iris truncated this preview]".chars().count()
    );
}

#[tokio::test]
async fn a_command_sh_cannot_find_warns_and_the_run_continues() {
    let dir = workspace();
    let hooks = Hooks {
        on_tool_pre: vec!["/nonexistent/iris-no-such-hook".into()],
        ..Hooks::default()
    };
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "x.txt", "content": "y"}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut warnings: Vec<String> = Vec::new();

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |event| {
            if let LoopEvent::HookWarning(message) = &event {
                warnings.push(message.clone());
            }
        },
    )
    .await
    .expect("the run completes");
    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        warnings.len(),
        1,
        "one unfindable command, one warning: {warnings:?}"
    );
    assert!(warnings[0].contains("on_tool_pre"), "{}", warnings[0]);
    // Hooks run through `sh -c`, so a missing binary is a non-zero
    // exit — advisory like any other hook failure.
    assert!(warnings[0].contains("exit status: 127"), "{}", warnings[0]);
}

#[tokio::test]
async fn failing_post_and_turn_end_hooks_warn_but_the_results_stand() {
    let dir = workspace();
    let hooks = Hooks {
        on_tool_post: vec!["exit 9".into()],
        on_turn_end: vec!["exit 7".into()],
        ..Hooks::default()
    };
    let scripted = Scripted::new(
        vec![],
        vec![
            tool_response(
                "call-1",
                "write_file",
                json!({"path": "x.txt", "content": "y"}),
            ),
            text_response("done"),
        ],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut warnings: Vec<String> = Vec::new();

    let outcome = run(
        &scripted,
        &write_registry(),
        &ctx,
        &DenyOnAsk,
        &options(hooks),
        initial(),
        |event| {
            if let LoopEvent::HookWarning(message) = &event {
                warnings.push(message.clone());
            }
        },
    )
    .await
    .expect("the run completes");
    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "{outcome:?}"
    );
    // One tool turn + two completed turns: the tool's `on_tool_post`
    // warns once, `on_turn_end` once per turn.
    assert_eq!(warnings.len(), 3, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("on_tool_post") && w.contains("exit status: 9")),
        "{warnings:?}"
    );
    assert_eq!(
        warnings
            .iter()
            .filter(|w| w.contains("on_turn_end") && w.contains("exit status: 7"))
            .count(),
        2,
        "turn_end warns on every turn, final one included: {warnings:?}"
    );
}

#[tokio::test]
async fn a_failing_compact_hook_warns_but_the_fold_still_applies() {
    let dir = workspace();
    let hooks = Hooks {
        on_compact: vec!["exit 9".into()],
        ..Hooks::default()
    };
    let messages = conversation(4);
    let est = estimate(&messages);
    let window = est * 100 / COMPACT_AT_PCT; // exactly at the trigger
    let mut opts = options(hooks);
    opts.compact = Some(CompactOpts { window, keep: 2 });

    let scripted = Scripted::new(
        vec![text_response("PINNED-SUMMARY")],
        vec![text_response("final answer")],
    );
    let ctx = ToolContext::with_gate(dir.path(), allow_gate());
    let mut warnings: Vec<String> = Vec::new();
    let mut folded = false;

    let outcome = run(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &DenyOnAsk,
        &opts,
        messages,
        |event| match event {
            LoopEvent::HookWarning(message) => warnings.push(message.clone()),
            LoopEvent::Compact { .. } => folded = true,
            _ => {}
        },
    )
    .await
    .expect("the run completes");
    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert!(folded, "the fold itself still applied");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("on_compact"), "{}", warnings[0]);
    assert!(warnings[0].contains("exit status: 9"), "{}", warnings[0]);
}
