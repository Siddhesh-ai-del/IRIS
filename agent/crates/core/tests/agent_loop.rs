//! Stage 1.9 — the headless agent loop, end to end.
//!
//! A scripted in-process provider drives the loop mechanics (limits,
//! policy, ask-resolution, observer); the `httpmock` cassette proves a
//! full 3-turn tool conversation through the real OpenAI-compatible
//! client and its SSE path (stages 1.2 + 1.3).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::stream;
use httpmock::prelude::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use iris_core::agent_loop::{
    LoopEvent, LoopOptions, LoopOutcome, Recorded, RunError, run, run_recorded,
};
use iris_core::config::{Config, ConfigInputs, ModelPrice, load_config};
use iris_core::metrics::TurnMetrics;
use iris_core::provider::{
    CompletionRequest, CompletionResponse, EventStream, OpenAiClient, Provider, ProviderError,
    StreamEvent,
};
use iris_core::session::writer::WriterError;
use iris_core::tools::{
    AskResolver, Decision, DenyOnAsk, ListTool, ReadTool, Resolution, Tool, ToolContext, ToolError,
    ToolRegistry, WriteTool,
};
use iris_core::types::{ContentBlock, Message, Role, StopReason, ToolCall, ToolResult, Usage};

// --- fixtures ------------------------------------------------------------

/// Provider that replays scripted responses over a fake event stream and
/// records every request the loop actually sent.
struct Scripted {
    queue: Mutex<VecDeque<CompletionResponse>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl Scripted {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            queue: Mutex::new(responses.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for Scripted {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        unimplemented!("the loop always streams (stage 1.9)")
    }

    async fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> Result<EventStream, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let response = self
            .queue
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

fn text_response(text: &str, total_tokens: u64) -> CompletionResponse {
    CompletionResponse {
        message: Message::text(Role::Assistant, text),
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: total_tokens - 2,
            output_tokens: 2,
            total_tokens,
            ..Usage::default()
        },
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
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            ..Usage::default()
        },
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MarkerArgs {}

/// Fixture tool: records whether its body ever ran and touches a marker
/// file inside the workspace.
struct MarkerTool {
    fired: Arc<AtomicBool>,
}

impl Tool for MarkerTool {
    type Args = MarkerArgs;

    fn name(&self) -> &'static str {
        "touch_marker"
    }

    fn description(&self) -> &'static str {
        "Touches a marker file"
    }

    async fn execute(&self, ctx: &ToolContext, _args: MarkerArgs) -> Result<ToolResult, ToolError> {
        self.fired.store(true, Ordering::SeqCst);
        std::fs::write(ctx.workspace_root.join("MARKER"), "touched").map_err(|err| {
            ToolError::Failed {
                tool: self.name().to_string(),
                message: err.to_string(),
            }
        })?;
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: "marker touched".into(),
            is_error: false,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EchoArgs {
    content: String,
}

/// Fixture tool: echoes back whatever input it received — how stage 4.6
/// proves *which* input the loop actually executed.
struct EchoTool {
    seen: Arc<Mutex<String>>,
}

impl Tool for EchoTool {
    type Args = EchoArgs;

    fn name(&self) -> &'static str {
        "echo_input"
    }

    fn description(&self) -> &'static str {
        "Echoes its content argument"
    }

    async fn execute(&self, _ctx: &ToolContext, args: EchoArgs) -> Result<ToolResult, ToolError> {
        *self.seen.lock().expect("poisoned") = args.content.clone();
        Ok(ToolResult {
            tool_use_id: String::new(),
            content: format!("saw: {}", args.content),
            is_error: false,
        })
    }
}

/// Resolver returning one scripted [`Resolution`] — stage 4.6 needs a
/// resolution no closure can express (`AllowWith`).
struct ScriptedResolution(Resolution);

impl AskResolver for ScriptedResolution {
    fn confirm(&self, _tool: &str, _detail: &str) -> Resolution {
        self.0.clone()
    }
}

fn config_from(file: Option<&str>) -> Config {
    load_config(&ConfigInputs {
        file_toml: file,
        env: &Default::default(),
        flag_base_url: None,
    })
    .expect("valid test config")
}

fn options(max_turns: u32, max_tokens_budget: u64) -> LoopOptions {
    LoopOptions {
        model: "test-model".into(),
        max_turns,
        max_tokens_budget,
        max_tokens: None,
        temperature: None,
        price: None,
        compact: None,
        hooks: iris_core::config::Hooks::default(),
    }
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn marker_registry(fired: &Arc<AtomicBool>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(MarkerTool {
        fired: Arc::clone(fired),
    });
    registry
}

fn allow_all(_tool: &str, _detail: &str) -> Decision {
    Decision::Allow
}

fn no_observer(_event: LoopEvent<'_>) {}

/// Tool results fed back into the conversation on the given request.
fn fed_back(request: &CompletionRequest) -> Vec<&ToolResult> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect()
}

// --- loop mechanics (scripted provider) ----------------------------------

#[tokio::test]
async fn max_turns_cap_stops_an_endless_tool_loop() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        tool_response("c2", "touch_marker", json!({})),
        tool_response("c3", "touch_marker", json!({})), // never reached
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(2, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::MaxTurns { turns: 2, .. }),
        "got {outcome:?}"
    );
    assert_eq!(scripted.requests().len(), 2, "no call beyond max_turns");
    assert!(fired.load(Ordering::SeqCst), "allowed tools still ran");
}

#[tokio::test]
async fn token_budget_stops_before_the_overshooting_call() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})), // 15 tokens
        tool_response("c2", "touch_marker", json!({})), // 30 cumulative
        text_response("never reached", 7),              // never reached
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(10, 20),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    match outcome {
        LoopOutcome::BudgetExceeded { turns: 2, usage } => {
            assert_eq!(usage.total_tokens, 30, "usage accumulates across turns");
        }
        other => panic!("expected BudgetExceeded, got {other:?}"),
    }
    assert_eq!(
        scripted.requests().len(),
        2,
        "budget must be checked before each provider call"
    );
}

#[tokio::test]
async fn denied_policy_never_runs_the_tool_but_tells_the_model() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        text_response("Understood — skipping.", 7),
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(Some("[permissions]\ntouch_marker = \"deny\"\n"));
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "a denial is data, not a loop failure: got {outcome:?}"
    );
    assert!(!fired.load(Ordering::SeqCst), "denied tool must never run");
    assert!(!dir.path().join("MARKER").exists());

    let requests = scripted.requests();
    let results = fed_back(&requests[1]);
    assert_eq!(results.len(), 1, "one tool result fed back");
    assert!(results[0].is_error, "denial surfaces as an error result");
    assert!(
        results[0].content.contains("permission denied"),
        "got: {}",
        results[0].content
    );
    assert_eq!(results[0].tool_use_id, "c1");
}

#[tokio::test]
async fn ask_policy_runs_the_tool_when_the_resolver_allows() {
    // Default policy asks for unlisted tools; the resolver approves.
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        text_response("done", 7),
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "got {outcome:?}"
    );
    assert!(fired.load(Ordering::SeqCst), "approved tool must run");
    let requests = scripted.requests();
    let results = fed_back(&requests[1]);
    assert_eq!(results.len(), 1);
    assert!(!results[0].is_error);
    assert_eq!(results[0].content, "marker touched");
}

#[tokio::test]
async fn ask_policy_fails_closed_when_the_resolver_refuses() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        text_response("done", 7),
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &DenyOnAsk,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "got {outcome:?}"
    );
    assert!(!fired.load(Ordering::SeqCst), "refused tool must never run");
    let requests = scripted.requests();
    let results = fed_back(&requests[1]);
    assert!(results[0].is_error);
    assert!(
        results[0].content.contains("permission denied"),
        "got: {}",
        results[0].content
    );
}

#[tokio::test]
async fn ask_allow_with_runs_the_resolver_replacement_input() {
    // Stage 4.6: a partially accepted file edit feeds back as a
    // *replaced input* — the loop must execute THAT, not the model's
    // original JSON.
    let seen = Arc::new(Mutex::new(String::new()));
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool {
        seen: Arc::clone(&seen),
    });

    let scripted = Scripted::new(vec![
        tool_response("c1", "echo_input", json!({"content": "ORIGINAL"})),
        text_response("done", 7),
    ]);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());
    let resolver = ScriptedResolution(Resolution::AllowWith(
        json!({"content": "PARTIAL"}).to_string(),
    ));

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &resolver,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "got {outcome:?}"
    );
    assert_eq!(
        seen.lock().expect("poisoned").as_str(),
        "PARTIAL",
        "the loop must execute the resolver's replacement input"
    );
    let requests = scripted.requests();
    let results = fed_back(&requests[1]);
    assert!(
        !results[0].is_error,
        "replacement ran: {}",
        results[0].content
    );
}

#[tokio::test]
async fn ask_allow_with_unparsable_replacement_fails_closed() {
    // A replacement that is not valid JSON must never run — neither the
    // replacement nor the original (fail closed, stage 4.6).
    let seen = Arc::new(Mutex::new(String::new()));
    let mut registry = ToolRegistry::new();
    registry.register(EchoTool {
        seen: Arc::clone(&seen),
    });

    let scripted = Scripted::new(vec![
        tool_response("c1", "echo_input", json!({"content": "ORIGINAL"})),
        text_response("done", 7),
    ]);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());
    let resolver = ScriptedResolution(Resolution::AllowWith("not json".to_string()));

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &resolver,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "got {outcome:?}"
    );
    assert_eq!(
        seen.lock().expect("poisoned").as_str(),
        "",
        "the original input must not run either"
    );
    let requests = scripted.requests();
    let results = fed_back(&requests[1]);
    assert!(results[0].is_error, "denied: {}", results[0].content);
    assert!(
        results[0].content.contains("permission denied"),
        "got: {}",
        results[0].content
    );
}

#[tokio::test]
async fn unknown_tool_becomes_an_error_result_the_model_can_react_to() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "ghost_tool", json!({"x": 1})),
        text_response("done", 7),
    ]);
    let registry = marker_registry(&fired); // only touch_marker exists
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let outcome = run(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
    )
    .await
    .unwrap();

    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "a hallucinated tool is data, not a crash: got {outcome:?}"
    );
    assert!(!fired.load(Ordering::SeqCst));
    let requests = scripted.requests();
    let results = fed_back(&requests[1]);
    assert!(results[0].is_error);
    assert!(
        results[0].content.contains("unknown tool: ghost_tool"),
        "got: {}",
        results[0].content
    );
}

#[tokio::test]
async fn observer_sees_streamed_text_and_tool_lifecycle() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        text_response("All finished.", 7),
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let observer = move |event: LoopEvent<'_>| {
        let line = match event {
            LoopEvent::TextDelta(text) => format!("text:{text}"),
            LoopEvent::ToolStart { id, name } => format!("start:{name}:{id}"),
            LoopEvent::ToolEnd { id, name, result } => {
                format!("end:{name}:{id}:{}", result.is_error)
            }
            // Static run — system and tools never change, so this arm
            // must never contribute a line (the final equality proves
            // it if it ever does).
            LoopEvent::CacheBust { .. } => "cache-bust".to_string(),
            // Timing-dependent (stage 3.3) — this assertion pins event
            // *ordering*, so metrics ride their own event instead.
            LoopEvent::Metrics(_) => return,
            // Stage 6.2 — never fires here (`compact: None` in these
            // runs); its own suite asserts the real line.
            LoopEvent::Compact { .. } => "compact".to_string(),
            // Stage 6.4 — never fires here (default hooks); the hooks
            // suite pins its own warnings.
            LoopEvent::HookWarning(_) => return,
        };
        sink.lock().unwrap().push(line);
    };

    run(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        observer,
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "start:touch_marker:c1",
            "end:touch_marker:c1:false",
            "text:All finished."
        ]
    );
}

// --- the Done-when cassette ----------------------------------------------

/// SSE body in the stage-1.3 wire shape: role preamble, the given delta
/// lines, a finish chunk, a usage chunk, then `[DONE]`.
fn sse_body(delta_lines: &str, finish: &str, usage: (u64, u64, u64)) -> String {
    let (prompt, completion, total) = usage;
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n\
         {delta_lines}\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{finish}\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\
         \"completion_tokens\":{completion},\"total_tokens\":{total}}}}}\n\n\
         data: [DONE]\n\n"
    )
}

fn text_delta(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
        serde_json::to_string(text).expect("a string serializes")
    )
}

fn tool_delta(index: u32, id: &str, name: &str, args: &Value) -> String {
    let call = json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    });
    format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]})
    )
}

/// The stage's Done-when: a full 3-turn tool conversation to completion
/// through the real streaming client.
///
/// Turn 1 calls `read_file`, turn 2 calls `list_dir` on the result,
/// turn 3 answers in plain text. Each mock is matched on a request-body
/// marker that only exists once the prior tool result was fed back —
/// the mocks prove transcript growth, not just ordering.
///
/// httpmock serves the first registered matching mock, so the layered
/// turn mocks are registered in reverse turn order.
#[tokio::test]
async fn cassette_three_turn_tool_conversation_to_completion() {
    let dir = workspace();
    std::fs::write(dir.path().join("hello.txt"), "HELLO_FROM_FIXTURE_42\n").unwrap();
    std::fs::write(dir.path().join("zebra_marker.dat"), "").unwrap();

    let server = MockServer::start();

    // Turn 3 (registered first): its marker is the list_dir result.
    let turn3 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("zebra_marker.dat");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &text_delta("Workspace inspected. Done."),
                "stop",
                (6, 4, 10),
            ));
    });
    // Turn 2: its marker is the read_file result.
    let turn2 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("HELLO_FROM_FIXTURE_42");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &tool_delta(0, "call_2", "list_dir", &json!({"path": "."})),
                "tool_calls",
                (15, 5, 20),
            ));
    });
    // Turn 1 (registered last): only the very first request matches it.
    let turn1 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("inspect the demo workspace");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &format!(
                    "{}{}",
                    text_delta("Reading hello.txt now."),
                    tool_delta(0, "call_1", "read_file", &json!({"path": "hello.txt"}))
                ),
                "tool_calls",
                (10, 5, 15),
            ));
    });

    let mut registry = ToolRegistry::new();
    registry.register(ReadTool);
    registry.register(ListTool);

    let config = config_from(Some(
        r#"
        max_turns = 10
        max_tokens_budget = 100000

        [permissions]
        read_file = "allow"
        list_dir = "allow"
        "#,
    ));
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());
    let client = OpenAiClient::new(server.base_url(), "sk-or-cassette");

    let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&deltas);
    let outcome = run(
        &client,
        &registry,
        &ctx,
        &DenyOnAsk,
        &LoopOptions::from_config(&config, "test-model"),
        vec![
            Message::text(Role::System, "You are iris."),
            Message::text(Role::User, "Please inspect the demo workspace."),
        ],
        move |event: LoopEvent<'_>| {
            if let LoopEvent::TextDelta(text) = event {
                sink.lock().unwrap().push(text.to_string());
            }
        },
    )
    .await
    .expect("loop must run");

    match outcome {
        LoopOutcome::Completed { text, turns, usage } => {
            assert_eq!(text, "Workspace inspected. Done.");
            assert_eq!(turns, 3, "three provider calls");
            assert_eq!(usage.total_tokens, 45, "usage accumulates across turns");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(
        *deltas.lock().unwrap(),
        vec!["Reading hello.txt now.", "Workspace inspected. Done."]
    );

    turn1.assert();
    turn2.assert();
    turn3.assert();
    assert_eq!(turn1.calls(), 1, "each mock serves exactly its turn");
    assert_eq!(turn2.calls(), 1);
    assert_eq!(turn3.calls(), 1);
}

// --- durability recording (stage 2.3) ------------------------------------

/// The recorder sees the conversation exactly as it grows — assistant
/// message, that turn's usage, each tool result, then the next
/// assistant message. Replaying the recorded messages reproduces the
/// conversation the following provider request carried.
#[tokio::test]
async fn run_recorded_mirrors_the_conversation_in_transcript_order() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        text_response("all done", 7),
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let mut recorded: Vec<Recorded> = Vec::new();
    let outcome = run_recorded(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(u32::MAX, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
        |event| {
            recorded.push(event);
            Ok(())
        },
    )
    .await
    .expect("loop must run");
    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "got {outcome:?}"
    );

    // Stage 3.3 metrics ride between each usage and its tool results;
    // this test pins the conversation mirroring, so set them aside —
    // their own test (`records_per_turn_metrics…`) covers them.
    let recorded: Vec<Recorded> = recorded
        .into_iter()
        .filter(|event| !matches!(event, Recorded::Metrics(_)))
        .collect();

    assert_eq!(
        recorded.len(),
        5,
        "assistant, usage, tool, assistant, usage — got {recorded:?}"
    );
    let role_of = |recorded: &Recorded| match recorded {
        Recorded::Message(msg) => msg.role.clone(),
        other => panic!("expected a message, got {other:?}"),
    };
    assert_eq!(role_of(&recorded[0]), Role::Assistant);
    assert!(
        matches!(
            &recorded[1],
            Recorded::Usage { turn: 1, usage, .. } if usage.total_tokens == 15
        ),
        "turn 1 usage, got {:?}",
        recorded[1]
    );
    assert_eq!(role_of(&recorded[2]), Role::Tool);
    assert_eq!(role_of(&recorded[3]), Role::Assistant);
    assert!(
        matches!(
            &recorded[4],
            Recorded::Usage { turn: 2, usage, .. } if usage.total_tokens == 7
        ),
        "turn 2 usage, got {:?}",
        recorded[4]
    );

    // Recorded messages mirror the conversation the second request
    // carried (minus the initial history the recorder was never handed)
    // plus the final assistant message that ended the loop.
    let recorded_messages: Vec<&Message> = recorded
        .iter()
        .filter_map(|event| match event {
            Recorded::Message(msg) => Some(msg),
            // Never in these runs (`compact: None`) — the stage-6.2
            // suite owns this record kind.
            Recorded::Compact { .. } => None,
            Recorded::Usage { .. } | Recorded::Metrics(_) => None,
        })
        .collect();
    let (final_answer, mirrored) = recorded_messages.split_last().unwrap();
    assert_eq!(final_answer.role, Role::Assistant, "the loop's last act");
    let request2 = &scripted.requests()[1];
    assert_eq!(
        *mirrored,
        request2.messages[1..].iter().collect::<Vec<_>>(),
        "recorder must mirror the exact messages the provider saw"
    );
}

/// A transcript that cannot be written aborts the run *before* the next
/// provider call — continuing would produce a session with amnesia.
#[tokio::test]
async fn run_recorded_aborts_on_the_first_recorder_failure() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![
        tool_response("c1", "touch_marker", json!({})),
        text_response("never reached", 7),
    ]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let mut calls = 0;
    let err = run_recorded(
        &scripted,
        &registry,
        &ctx,
        &allow_all,
        &options(u32::MAX, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
        |_event| {
            calls += 1;
            Err(WriterError::Io(std::io::Error::other("disk full")).into())
        },
    )
    .await
    .expect_err("recorder failure must surface");
    assert!(matches!(err, RunError::Transcript(_)), "got {err}");
    assert_eq!(calls, 1, "the run aborts at the first failed append");
    assert_eq!(
        scripted.requests().len(),
        1,
        "no provider call after the transcript failed"
    );
}

// --- prefix-invariant assembly (stage 3.1) -------------------------------

/// The stage's Done-when, through the real loop: two runs with the same
/// session and config — the only difference being tool *registration*
/// order, the volatile input a HashMap-seeded builder would introduce —
/// must send name-sorted tools and byte-identical request prefixes.
#[tokio::test]
async fn identical_config_runs_send_byte_identical_prefixes_with_sorted_tools() {
    fn registry_in(order: [&'static str; 3]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in order {
            match name {
                "read_file" => registry.register(ReadTool),
                "list_dir" => registry.register(ListTool),
                "write_file" => registry.register(WriteTool),
                other => panic!("unknown fixture tool: {other}"),
            }
        }
        registry
    }

    let messages = || {
        vec![
            Message::text(Role::System, "You are iris."),
            Message::text(Role::User, "say done"),
        ]
    };
    let ctx = ToolContext::new("/tmp/iris-prefix-ws");

    let first = Scripted::new(vec![text_response("done.", 7)]);
    run(
        &first,
        &registry_in(["read_file", "list_dir", "write_file"]),
        &ctx,
        &DenyOnAsk,
        &options(2, 1_000),
        messages(),
        no_observer,
    )
    .await
    .expect("first run");

    let second = Scripted::new(vec![text_response("done.", 7)]);
    run(
        &second,
        &registry_in(["write_file", "read_file", "list_dir"]),
        &ctx,
        &DenyOnAsk,
        &options(2, 1_000),
        messages(),
        no_observer,
    )
    .await
    .expect("second run");

    let request_one = &first.requests()[0];
    let request_two = &second.requests()[0];
    let names: Vec<&str> = request_one
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["list_dir", "read_file", "write_file"],
        "tools reach the provider name-sorted, never registration-ordered"
    );
    assert_eq!(
        iris_core::assembly::prefix_bytes(request_one),
        iris_core::assembly::prefix_bytes(request_two),
        "same session + same config ⇒ byte-identical request prefix"
    );
}

// --- token accounting (stage 3.2) -----------------------------------------

/// The loop reconciles its tiktoken estimate of the request prefix
/// against the provider's actual input tokens and hands the signed
/// delta to the recorder with every usage event — and hands `None`
/// when the provider reported no usage (nothing to reconcile against).
#[tokio::test]
async fn records_the_reconciliation_delta_on_every_usage_event() {
    let fired = Arc::new(AtomicBool::new(false));
    let tool_turn = CompletionResponse {
        message: Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse(ToolCall {
                id: "c1".into(),
                name: "touch_marker".into(),
                input: json!({}),
            })],
        },
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 1234,
            output_tokens: 5,
            total_tokens: 1239,
            ..Usage::default()
        },
    };
    // Turn 2: a provider that reports no usage at all.
    let silent_turn = CompletionResponse {
        message: Message::text(Role::Assistant, "all done"),
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
    };
    let scripted = Scripted::new(vec![tool_turn, silent_turn]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let mut deltas: Vec<Option<i64>> = Vec::new();
    let outcome = run_recorded(
        &scripted,
        &registry,
        &ctx,
        &DenyOnAsk,
        &options(5, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
        |recorded| {
            if let Recorded::Usage {
                input_token_delta, ..
            } = recorded
            {
                deltas.push(input_token_delta);
            }
            Ok(())
        },
    )
    .await
    .expect("loop must run");

    assert!(matches!(outcome, LoopOutcome::Completed { .. }));
    assert_eq!(deltas.len(), 2, "one usage event per provider response");
    let estimated = iris_core::tokens::estimate_input(&scripted.requests()[0]);
    assert_eq!(
        deltas[0],
        Some(1234 - estimated as i64),
        "delta = actual input tokens − the tiktoken estimate of the prefix"
    );
    assert_eq!(deltas[1], None, "no usage reported → nothing to reconcile");
    match outcome {
        LoopOutcome::Completed { usage, .. } => {
            assert_eq!(
                usage.input_tokens, 1234,
                "lifetime totals only sum what the provider reported"
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

// --- per-turn metrics (stage 3.3) -----------------------------------------

/// Every response gets a `TurnMetrics` handed to the recorder (the
/// transcript's `turn_metrics` event) and to the observer (the
/// stage-3.4 gauge), measured from the real stream: ttft present,
/// cache hit from the provider's report, cost from the config price
/// table when the provider didn't charge one.
#[tokio::test]
async fn records_per_turn_metrics_and_delivers_them_to_the_observer() {
    let fired = Arc::new(AtomicBool::new(false));
    let scripted = Scripted::new(vec![CompletionResponse {
        message: Message::text(Role::Assistant, "hi"),
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 194,
            output_tokens: 2,
            total_tokens: 196,
            cached_input_tokens: Some(64),
            cost_usd: None,
        },
    }]);
    let registry = marker_registry(&fired);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let mut opts = options(5, u64::MAX);
    opts.price = Some(ModelPrice {
        input_per_million: 0.15,
        output_per_million: 0.60,
        cached_input_per_million: Some(0.075),
    });

    let mut recorded_metrics: Vec<TurnMetrics> = Vec::new();
    let mut observed_metrics: Vec<TurnMetrics> = Vec::new();
    let outcome = run_recorded(
        &scripted,
        &registry,
        &ctx,
        &DenyOnAsk,
        &opts,
        vec![Message::text(Role::User, "go")],
        |event| {
            if let LoopEvent::Metrics(metrics) = event {
                observed_metrics.push(metrics.clone());
            }
        },
        |recorded| {
            if let Recorded::Metrics(metrics) = recorded {
                recorded_metrics.push(metrics);
            }
            Ok(())
        },
    )
    .await
    .expect("loop must run");
    assert!(matches!(outcome, LoopOutcome::Completed { .. }));

    assert_eq!(
        recorded_metrics.len(),
        1,
        "one metrics record per provider response"
    );
    let metrics = &recorded_metrics[0];
    assert_eq!(metrics.turn, 1, "matches its usage event's turn");
    assert!(metrics.ttft_ms.is_some(), "a token did arrive");
    assert!(metrics.tokens_per_sec >= 0.0);
    let hit = metrics.cache_hit_pct.expect("cache was reported");
    assert!(
        (hit - 64.0 * 100.0 / 194.0).abs() < 1e-9,
        "64 of 194 inputs hit cache, got {hit}"
    );
    // Provider reported no charge → the price table estimates it.
    let cost = metrics.cost_usd.expect("estimated from the table");
    assert!(
        (cost - 0.0000255).abs() < 1e-12,
        "130×0.15 + 64×0.075 + 2×0.60 per million, got {cost}"
    );

    assert_eq!(
        observed_metrics, recorded_metrics,
        "the observer sees exactly what the transcript records"
    );
}

// --- coverage fills (stage 7.2) ---------------------------------------------

/// The transcript is the resume primitive: a recorder that fails on
/// the usage event aborts the run with a transcript error instead of
/// continuing into silent amnesia (the `?` on `Recorded::Usage`).
#[tokio::test]
async fn a_recorder_failure_on_the_usage_event_aborts_the_run() {
    let scripted = Scripted::new(vec![text_response("answer", 7)]);
    let dir = workspace();
    let config = config_from(None);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let err = run_recorded(
        &scripted,
        &ToolRegistry::new(),
        &ctx,
        &allow_all,
        &options(u32::MAX, u64::MAX),
        vec![Message::text(Role::User, "go")],
        no_observer,
        |entry| match entry {
            Recorded::Usage { .. } => Err(RunError::Transcript(WriterError::Io(
                std::io::Error::other("transcript disk full"),
            ))),
            _ => Ok(()),
        },
    )
    .await
    .expect_err("the run must stop when the transcript cannot be written");
    assert!(
        matches!(err, RunError::Transcript(_)),
        "expected a transcript abort, got {err:?}"
    );
}
