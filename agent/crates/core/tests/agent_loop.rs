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

use iris_core::agent_loop::{LoopEvent, LoopOptions, LoopOutcome, run};
use iris_core::config::{Config, ConfigInputs, load_config};
use iris_core::provider::{
    CompletionRequest, CompletionResponse, EventStream, OpenAiClient, Provider, ProviderError,
    StreamEvent,
};
use iris_core::tools::{
    Decision, DenyOnAsk, ListTool, ReadTool, Tool, ToolContext, ToolError, ToolRegistry,
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
