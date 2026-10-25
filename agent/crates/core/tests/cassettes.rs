//! Stage 7.1 — the HTTP cassette layer: **record** against the real
//! OpenRouter API (an ignored live test — key from the environment
//! only, free models), **replay** from committed JSON cassettes through
//! the real streaming client (hermetic: no network, no key, CI-safe).
//!
//! Matching is *exact request-body* equality — a replay turn is served
//! only for a byte-identical request, so a cassette proves transcript
//! growth (tool results fed back) exactly as strongly as the hand-built
//! fixtures of stage 1.7, but with **real** OpenRouter SSE shapes
//! instead of hand-written ones.
//!
//! Recorders capture bodies only, never headers: an API key can never
//! land in a committed file.
//!
//! Re-record (env-only key, free models):
//!
//! ```text
//! OPENROUTER_API_KEY=… cargo test --test cassettes record_ -- --ignored --nocapture
//! ```

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use httpmock::prelude::*;
use serde::{Deserialize, Serialize};

use iris_core::agent_loop::{LoopEvent, LoopOptions, LoopOutcome, run};
use iris_core::config::{ConfigInputs, load_config};
use iris_core::provider::openai::CapturedExchange;
use iris_core::provider::{CompletionRequest, OpenAiClient, Provider, collect_stream, wire};
use iris_core::tools::{DenyOnAsk, ReadTool, ToolContext, ToolRegistry};
use iris_core::types::{Message, Role};

// --- scenario constants (identical for record and replay) ------------------

const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";
const CASSETTE_DIR: &str = "tests/cassettes";
/// Proven free model for recording; override with `IRIS_RECORD_MODEL`
/// if it rotates off the free list.
const DEFAULT_RECORD_MODEL: &str = "nvidia/nemotron-3.5-lightning:free";

const SYSTEM: &str = "You are iris, a coding agent. Use tools when they help.";
const STREAMING_PROMPT: &str = "Greet the user with one short sentence. Do not use any tools.";
const TOOL_PROMPT: &str = "Use the read_file tool to read notes.txt in this workspace, then say exactly what it contains.";
/// The fixture's content marker — must flow back into the second
/// recorded request for the tool loop to be a real tool loop.
const FIXTURE_MARKER: &str = "CASSETTE_FIXTURE_71_MARK";
const FIXTURE: &str = "CASSETTE_FIXTURE_71_MARK\n";

const CASSETTE_CONFIG: &str = r#"
max_turns = 10
max_tokens_budget = 100000

[permissions]
read_file = "allow"
"#;

// --- cassette format --------------------------------------------------------

/// A committed cassette: one scripted conversation's HTTP exchanges,
/// in conversation order.
#[derive(Debug, Serialize, Deserialize)]
struct Cassette {
    /// Human label, also the file name.
    name: String,
    /// Model slug the recording used — replay must request it too.
    model: String,
    exchanges: Vec<CapturedExchange>,
}

fn cassette_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(CASSETTE_DIR).join(format!("{name}.json"))
}

/// Load a committed cassette, with a re-record hint when it's absent.
fn load(name: &str) -> Cassette {
    let path = cassette_path(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "cassette {} missing ({err}) — re-record with `OPENROUTER_API_KEY=… \
             cargo test --test cassettes record_ -- --ignored --nocapture`",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("valid cassette JSON")
}

/// Write a cassette pretty-printed for review, newline-terminated.
fn write(name: &str, model: &str, exchanges: Vec<CapturedExchange>) {
    let cassette = Cassette {
        name: name.into(),
        model: model.into(),
        exchanges,
    };
    let mut json = serde_json::to_string_pretty(&cassette).expect("cassette serializes");
    json.push('\n');
    std::fs::create_dir_all(CASSETTE_DIR).expect("cassette dir exists");
    std::fs::write(cassette_path(name), json).expect("cassette written");
}

/// Register every recorded exchange on `httpmock` as an **exact-body**
/// mock: each turn matches only a byte-identical request, so any
/// divergence 404s the run instead of serving the wrong turn.
fn replay<'a>(server: &'a MockServer, cassette: &Cassette) -> Vec<httpmock::Mock<'a>> {
    cassette
        .exchanges
        .iter()
        .map(|exchange| {
            server.mock(|when, then| {
                when.method(POST)
                    .path("/chat/completions")
                    .body(exchange.request_body.clone());
                then.status(exchange.status)
                    .header("content-type", exchange.content_type.clone())
                    .body(exchange.response_body.clone());
            })
        })
        .collect()
}

// --- the scripted scenario (shared byte-for-byte by record and replay) ------

/// Run the scripted conversation against `client`: same system/user
/// text, same fixture workspace, same config, same registry every time.
/// Returns the outcome, the streamed text deltas, and the workspace
/// root (record asserts it never leaks into a request body — replay
/// would diverge).
async fn run_scenario(
    client: &OpenAiClient,
    model: &str,
    prompt: &str,
) -> (LoopOutcome, Vec<String>, String) {
    let dir = tempfile::tempdir().expect("temp workspace");
    std::fs::write(dir.path().join("notes.txt"), FIXTURE).expect("fixture written");

    let config = load_config(&ConfigInputs {
        file_toml: Some(CASSETTE_CONFIG),
        env: &Default::default(),
        flag_base_url: None,
    })
    .expect("test config");
    let mut registry = ToolRegistry::new();
    registry.register(ReadTool);
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());

    let deltas: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&deltas);
    let outcome = run(
        client,
        &registry,
        &ctx,
        &DenyOnAsk,
        &LoopOptions::from_config(&config, model),
        vec![
            Message::text(Role::System, SYSTEM),
            Message::text(Role::User, prompt),
        ],
        move |event: LoopEvent<'_>| {
            if let LoopEvent::TextDelta(text) = event {
                sink.lock().unwrap().push(text.to_string());
            }
        },
    )
    .await
    .expect("loop must run");
    let root = dir.path().to_string_lossy().into_owned();
    (outcome, deltas.lock().unwrap().clone(), root)
}

/// Drive the scenario against the live OpenRouter API with a recorder
/// attached; returns the model and the captured exchanges.
async fn record_live(prompt: &str, min_exchanges: usize) -> (String, Vec<CapturedExchange>) {
    let key = std::env::var("OPENROUTER_API_KEY").unwrap_or_else(|_| {
        panic!("set OPENROUTER_API_KEY in the environment (env only — never a file) to record")
    });
    let model =
        std::env::var("IRIS_RECORD_MODEL").unwrap_or_else(|_| DEFAULT_RECORD_MODEL.to_string());
    let captured: Arc<Mutex<Vec<CapturedExchange>>> = Arc::new(Mutex::new(Vec::new()));
    let client = OpenAiClient::new(OPENROUTER_BASE, &key).with_recorder({
        let sink = Arc::clone(&captured);
        move |exchange| sink.lock().unwrap().push(exchange)
    });

    let (outcome, _deltas, root) = run_scenario(&client, &model, prompt).await;
    let exchanges = std::mem::take(&mut *captured.lock().unwrap());
    match outcome {
        LoopOutcome::Completed { .. } => {}
        other => panic!("the record run must complete, got {other:?}"),
    }
    assert!(
        exchanges.len() >= min_exchanges,
        "expected ≥ {min_exchanges} exchanges, got {} — the model did not follow \
         the script; adjust the prompt or model and re-record",
        exchanges.len()
    );
    for exchange in &exchanges {
        assert!(
            !exchange.request_body.contains(&root),
            "the workspace path leaked into a request body — replay would diverge"
        );
        assert!(
            !exchange.request_body.contains(&key),
            "an API key entered a request body — keys belong in headers only"
        );
    }
    (model, exchanges)
}

// --- recorded-exchange extraction (test-side assertions only) ---------------

/// Concatenated `delta.content` of an SSE body — the text a recorded
/// stream actually delivered.
fn sse_text(sse: &str) -> String {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .filter(|payload| *payload != "[DONE]")
        .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .filter_map(|value| {
            value["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_string)
        })
        .collect::<Vec<_>>()
        .join("")
}

// --- recorder seam (pinned without any network) ----------------------------

fn request() -> CompletionRequest {
    CompletionRequest {
        model: "cassette-model".into(),
        messages: vec![Message::text(Role::User, "hi")],
        tools: vec![],
        max_tokens: None,
        temperature: None,
    }
}

/// A stream with real-world shape: an id field, several deltas, a
/// usage chunk, `[DONE]`.
const SSE: &str = concat!(
    "data: {\"id\":\"gen-abc\",\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"Cassette \"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"hello.\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":3,\"total_tokens\":8}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn recorder_captures_a_streaming_exchange_verbatim() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream; charset=utf-8")
            .body(SSE);
    });
    let captured: Arc<Mutex<Vec<CapturedExchange>>> = Arc::new(Mutex::new(Vec::new()));
    let client = OpenAiClient::new(server.base_url(), "sk-or-v1-never-captured").with_recorder({
        let sink = Arc::clone(&captured);
        move |exchange| sink.lock().unwrap().push(exchange)
    });

    let stream = client.complete_stream(request()).await.expect("stream");
    let response = collect_stream(stream).await.expect("drained");
    assert_eq!(response.usage.total_tokens, 8, "the run saw the stream");

    let exchanges = captured.lock().unwrap();
    assert_eq!(exchanges.len(), 1, "one fully-consumed exchange recorded");
    let exchange = &exchanges[0];
    assert_eq!(exchange.status, 200);
    assert_eq!(exchange.content_type, "text/event-stream; charset=utf-8");
    assert_eq!(exchange.response_body, SSE, "the raw bytes, quirks and all");
    let expected =
        serde_json::to_value(wire::request_to_streaming_wire(request())).expect("wire value");
    let got: serde_json::Value =
        serde_json::from_str(&exchange.request_body).expect("recorded body is JSON");
    assert_eq!(got, expected, "the exact wire request that was sent");
    assert!(
        !exchange.request_body.contains("sk-or-v1-never-captured"),
        "keys live in headers — a request body must never carry one"
    );
    mock.assert();
}

#[tokio::test]
async fn non_streaming_completions_are_recorded_too() {
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"Hi."},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}"#;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "application/json")
            .body(body);
    });
    let captured: Arc<Mutex<Vec<CapturedExchange>>> = Arc::new(Mutex::new(Vec::new()));
    let client = OpenAiClient::new(server.base_url(), "k").with_recorder({
        let sink = Arc::clone(&captured);
        move |exchange| sink.lock().unwrap().push(exchange)
    });

    let response = client.complete(request()).await.expect("completes");
    assert_eq!(response.usage.total_tokens, 5);

    let exchanges = captured.lock().unwrap();
    assert_eq!(exchanges.len(), 1);
    assert_eq!(exchanges[0].status, 200);
    assert_eq!(exchanges[0].content_type, "application/json");
    assert_eq!(exchanges[0].response_body, body);
    let expected = serde_json::to_value(wire::request_to_wire(request())).expect("wire value");
    let got: serde_json::Value = serde_json::from_str(&exchanges[0].request_body).unwrap();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn a_dropped_stream_is_never_recorded() {
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(SSE);
    });
    let captured: Arc<Mutex<Vec<CapturedExchange>>> = Arc::new(Mutex::new(Vec::new()));
    let client = OpenAiClient::new(server.base_url(), "k").with_recorder({
        let sink = Arc::clone(&captured);
        move |exchange| sink.lock().unwrap().push(exchange)
    });

    let mut stream = client.complete_stream(request()).await.expect("stream");
    let first = stream.next().await.expect("first event");
    assert!(first.is_ok());
    drop(stream); // cancelled mid-flight, as Ctrl-C would

    assert!(
        captured.lock().unwrap().is_empty(),
        "a partial conversation is not a cassette"
    );
}

// --- replay (hermetic — committed cassettes, exact-body matching) -----------

#[tokio::test]
async fn streaming_cassette_replays_through_the_real_client() {
    let cassette = load("streaming");
    let server = MockServer::start();
    let mocks = replay(&server, &cassette);
    assert_eq!(mocks.len(), cassette.exchanges.len());

    let client = OpenAiClient::new(server.base_url(), "sk-or-replay-fake");
    let (outcome, deltas, _root) = run_scenario(&client, &cassette.model, STREAMING_PROMPT).await;

    let expected = sse_text(
        &cassette
            .exchanges
            .first()
            .expect("recorded exchange")
            .response_body,
    );
    assert!(!expected.is_empty(), "the recording delivered text");
    match outcome {
        LoopOutcome::Completed { text, turns, .. } => {
            assert_eq!(text, expected, "the real model's answer, verbatim");
            assert_eq!(turns, 1, "one provider call, one recorded exchange");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(!deltas.is_empty(), "it streamed as deltas, not one blob");
    for mock in &mocks {
        assert_eq!(mock.calls(), 1, "each recorded turn served exactly once");
    }
}

#[tokio::test]
async fn tool_loop_cassette_replays_the_full_conversation() {
    let cassette = load("tool_loop");
    assert!(
        cassette.exchanges.len() >= 2,
        "the recording is a real tool loop ({} exchanges)",
        cassette.exchanges.len()
    );
    assert!(
        cassette.exchanges[1].request_body.contains(FIXTURE_MARKER),
        "turn 2's recorded request carried the file content back — transcript growth"
    );
    let server = MockServer::start();
    let mocks = replay(&server, &cassette);

    let client = OpenAiClient::new(server.base_url(), "sk-or-replay-fake");
    let (outcome, _deltas, _root) = run_scenario(&client, &cassette.model, TOOL_PROMPT).await;

    let expected = sse_text(&cassette.exchanges.last().unwrap().response_body);
    match outcome {
        LoopOutcome::Completed { text, turns, .. } => {
            assert_eq!(text, expected, "the real model's answer, verbatim");
            assert_eq!(
                turns as usize,
                cassette.exchanges.len(),
                "every recorded exchange served exactly its turn"
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    for mock in &mocks {
        assert_eq!(mock.calls(), 1, "each recorded turn served exactly once");
    }
}

// --- record (live, ignored — real API, env-only key, free models) -----------

#[tokio::test]
#[ignore = "live record: OPENROUTER_API_KEY=… cargo test --test cassettes record_ -- --ignored --nocapture"]
async fn record_streaming_cassette() {
    let (model, exchanges) = record_live(STREAMING_PROMPT, 1).await;
    write("streaming", &model, exchanges);
}

#[tokio::test]
#[ignore = "live record: OPENROUTER_API_KEY=… cargo test --test cassettes record_ -- --ignored --nocapture"]
async fn record_tool_loop_cassette() {
    let (model, exchanges) = record_live(TOOL_PROMPT, 2).await;
    assert!(
        exchanges[1].request_body.contains(FIXTURE_MARKER),
        "the model did not actually read notes.txt — adjust prompt/model and re-record"
    );
    write("tool_loop", &model, exchanges);
}
