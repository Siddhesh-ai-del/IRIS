//! Stage 5.3: telemetry-off guarantee. Three pins:
//!
//! 1. **Runtime** — a full scripted run (streaming provider + real tool
//!    execution) opens sockets only for the configured provider host,
//!    and every socket it opened drains back to the pre-run baseline
//!    once the run ends: nothing dials home, nothing lingers.
//! 2. **Dependency graph** — the workspace may not grow a second HTTP
//!    client or a telemetry SDK. reqwest, used solely by the provider
//!    client, is the only network stack allowed anywhere.
//! 3. **rmcp features** (stage 6.3) — the MCP client is stdio-only:
//!    `default-features = false` plus `client` +
//!    `transport-child-process`, never an HTTP/SSE transport feature.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use httpmock::prelude::*;
use serde_json::{Value, json};

use iris_core::agent_loop::{LoopEvent, LoopOptions, LoopOutcome, run};
use iris_core::config::{ConfigInputs, load_config};
use iris_core::provider::OpenAiClient;
use iris_core::tools::{DenyOnAsk, ReadTool, ToolContext, ToolRegistry};
use iris_core::types::{Message, Role};

/// Open sockets this process currently holds, counted from its fd
/// table — process-specific, unlike `/proc/net` (per-netns).
#[cfg(target_os = "linux")]
fn socket_fds() -> usize {
    let Ok(entries) = fs::read_dir("/proc/self/fd") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            fs::read_link(entry.path())
                .map(|link| link.to_string_lossy().starts_with("socket:"))
                .unwrap_or(false)
        })
        .count()
}

#[cfg(not(target_os = "linux"))]
fn socket_fds() -> usize {
    0
}

/// Re-encode a response as the stage-1.3 SSE wire shape.
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

/// The runtime half: a scripted two-turn run — turn 1 executes a real
/// `read_file`, turn 2 completes in text — must touch only the
/// configured provider and leave zero sockets behind.
#[tokio::test(flavor = "multi_thread")]
async fn a_full_run_opens_sockets_only_for_the_provider_and_leaves_none() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("hello.txt"), "telemetry-off fixture\n").unwrap();

    let baseline = socket_fds();
    let server = MockServer::start();

    // httpmock pools its servers (Drop returns the adapter for reuse),
    // so the harness keeps its own listener open by design. Everything
    // the *run* opens must drain back to this harness footprint.
    let settle_deadline = Instant::now() + Duration::from_secs(2);
    let mut harness = socket_fds();
    while Instant::now() < settle_deadline {
        std::thread::sleep(Duration::from_millis(50));
        let now = socket_fds();
        if now == harness {
            break;
        }
        harness = now;
    }
    assert!(harness >= baseline, "harness cannot shrink the process");

    let turn2 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("telemetry-off fixture");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("Done."), "stop", (5, 2, 7)));
    });
    let turn1 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("inspect hello.txt");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &tool_delta(0, "call_1", "read_file", &json!({"path": "hello.txt"})),
                "tool_calls",
                (10, 5, 15),
            ));
    });

    let mut registry = ToolRegistry::new();
    registry.register(ReadTool);
    let config = load_config(&ConfigInputs {
        file_toml: Some("[permissions]\nread_file = \"allow\"\n"),
        env: &Default::default(),
        flag_base_url: None,
    })
    .expect("valid test config");
    let ctx = ToolContext::with_gate(dir.path(), config.permission_gate());
    let client = OpenAiClient::new(server.base_url(), "sk-or-telemetry-off");

    let outcome = run(
        &client,
        &registry,
        &ctx,
        &DenyOnAsk,
        &LoopOptions::from_config(&config, "test-model"),
        vec![
            Message::text(Role::System, "You are iris."),
            Message::text(Role::User, "Please inspect hello.txt."),
        ],
        |_event: LoopEvent<'_>| {},
    )
    .await
    .expect("loop must run");
    assert!(
        matches!(outcome, LoopOutcome::Completed { .. }),
        "expected completion, got {outcome:?}"
    );

    // Every provider request hit the configured host — exactly the two
    // scripted calls, nothing dialed out beside them.
    turn1.assert();
    turn2.assert();

    drop(client);

    // Every socket the run opened drains closed: the process returns to
    // the harness's own listener footprint. (Multi-thread runtime so
    // pool teardown progresses while we poll.)
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + Duration::from_secs(3);
        while socket_fds() > harness && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            socket_fds(),
            harness,
            "sockets outlived the run — something lingers beyond the provider"
        );
    }
}

/// The dependency half: the graph itself is the boundary. No second
/// HTTP client, no telemetry SDK, in any workspace crate.
#[test]
fn reqwest_is_the_workspaces_only_network_stack() {
    // Network clients outside the provider (reqwest, core only).
    const HTTP_CLIENTS: &[&str] = &[
        "hyper",
        "ureq",
        "curl",
        "attohttpc",
        "isahc",
        "minreq",
        "surf",
        "awc",
        "tungstenite",
        "tokio-tungstenite",
    ];
    // Telemetry/analytics/crash-reporting SDKs — forbidden everywhere.
    const TELEMETRY: &[&str] = &[
        "opentelemetry",
        "sentry",
        "sentry-core",
        "sentry-anyhow",
        "posthog",
        "segment_api",
        "analytics",
        "mixpanel",
        "amplitude",
        "newrelic",
        "dynatrace",
    ];

    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    for crate_dir in ["core", "cli", "tui"] {
        let manifest = crates.join(crate_dir).join("Cargo.toml");
        let text = fs::read_to_string(&manifest).expect("workspace manifest readable");
        let table: toml::Table = toml::from_str(&text).expect("manifest parses as TOML");

        let mut deps: Vec<String> = Vec::new();
        for section in ["dependencies", "dev-dependencies"] {
            if let Some(toml::Value::Table(section_table)) = table.get(section) {
                deps.extend(section_table.keys().cloned());
            }
        }

        for dep in &deps {
            assert!(
                !HTTP_CLIENTS.contains(&dep.as_str()),
                "{crate_dir} pulls HTTP client `{dep}` — the provider client \
                 (reqwest, core only) is the workspace's only network stack"
            );
            assert!(
                !TELEMETRY.contains(&dep.as_str()),
                "{crate_dir} pulls telemetry SDK `{dep}` — IRIS ships telemetry-off"
            );
            if dep == "reqwest" {
                assert_eq!(
                    crate_dir, "core",
                    "`reqwest` outside iris-core: only the provider client may dial out"
                );
            }
        }
    }
}

/// Stage 6.3: rmcp is pinned to a stdio-only client feature set. An
/// HTTP/SSE transport feature would give the workspace a second dialer
/// and a second wire protocol to reason about — the dependency-graph
/// pin above only sees crate *names*, so features need their own check.
#[test]
fn rmcp_is_a_stdio_only_client() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("core")
        .join("Cargo.toml");
    let text = fs::read_to_string(&manifest).expect("core manifest readable");
    let table: toml::Table = toml::from_str(&text).expect("manifest parses as TOML");

    let rmcp = table
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .and_then(|deps| deps.get("rmcp"))
        .expect("iris-core declares rmcp (stage 6.3)");
    let spec = rmcp.as_table().expect("rmcp is an inline dependency spec");

    assert_eq!(
        spec.get("default-features"),
        Some(&toml::Value::Boolean(false)),
        "rmcp must disable default features (no server, no macros, no HTTP)"
    );
    let features = spec
        .get("features")
        .and_then(toml::Value::as_array)
        .expect("rmcp declares its feature list")
        .iter()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    let allowed = ["client", "transport-child-process"];
    for feature in &features {
        assert!(
            allowed.contains(feature),
            "rmcp feature `{feature}` is not stdio-client scope — MCP stays \
             child-process stdio, no second network stack"
        );
    }
    assert!(
        features.contains(&"client") && features.contains(&"transport-child-process"),
        "the stdio client needs both `client` and `transport-child-process`: {features:?}"
    );
}
