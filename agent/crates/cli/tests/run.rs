//! Stage 1.10 — headless `iris run -p …`, end to end through the real
//! binary. These tests double as the **E2E harness proof** for every
//! later stage: spawn `iris` → hermetic env → httpmock provider → assert
//! stdout/stderr/exit code.
//!
//! Contract under test:
//!   - model text streams to **stdout** (pipeable), tool events and the
//!     run summary go to **stderr**
//!   - exit code = agent outcome: 0 completed · 1 error (config/key/
//!     provider/IO) · 3 max turns · 4 token budget (2 stays reserved for
//!     clap usage errors)

mod common;

use common::{iris, sse_body, sse_body_with_usage, text_delta, tool_delta};
use httpmock::prelude::*;
use iris_core::session::schema::{Event, Outcome};
use iris_core::session::writer::load;
use serde_json::json;
use std::path::Path;

/// The final event of the single session in a store — the run's
/// `session_end` when it was recorded at all.
fn last_event_of_single_session(store: &Path) -> Option<Event> {
    let entry = std::fs::read_dir(store)
        .ok()?
        .filter_map(Result::ok)
        .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))?;
    load(&entry.path()).ok()?.pop()
}

/// The stage's Done-when: a full tool conversation through the real
/// binary — turn 1 calls `read_file`, turn 2 answers in text. Turn 2 is
/// matched on the tool result in the transcript (proves growth through
/// the CLI, not just the loop), so it is registered first (first-match).
#[test]
fn run_streams_text_to_stdout_and_tool_events_to_stderr() {
    let dir = tempfile::tempdir().expect("temp workdir");
    std::fs::write(dir.path().join("hello.txt"), "HELLO_FROM_FIXTURE_42\n").unwrap();

    let server = MockServer::start();

    // Turn 2 (registered first): marker = the read_file result.
    let turn2 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("HELLO_FROM_FIXTURE_42");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("All done."), "stop", (6, 4, 10)));
    });
    // Turn 1 (registered last): marker = the prompt.
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

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("Please inspect the demo workspace.")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    // Both turns' text streamed to stdout, tool events stayed off it.
    assert!(stdout.contains("Reading hello.txt now."), "{stdout}");
    assert!(stdout.contains("All done."), "{stdout}");
    assert!(
        !stdout.contains("[iris]"),
        "tool events leaked to stdout: {stdout}"
    );
    assert!(
        stdout.ends_with('\n'),
        "stdout must end with a newline: {stdout:?}"
    );
    // Tool events on stderr.
    assert!(stderr.contains("read_file"), "{stderr}");
    assert!(stderr.contains("[iris]"), "{stderr}");

    turn1.assert();
    turn2.assert();
}

/// Headless runs fail closed: `write_file` is `ask` by default, and
/// `DenyOnAsk` refuses it — the denial is *data* fed back to the model,
/// which then finishes. The file must never be created.
#[test]
fn run_headless_denies_asking_tools_by_default() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let server = MockServer::start();

    // Turn 2 (registered first): marker = the denial in the transcript.
    let turn2 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("permission denied for `write_file`");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &text_delta("I could not write the file."),
                "stop",
                (6, 4, 10),
            ));
    });
    // Turn 1 (registered last): the model asks to write.
    let turn1 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("please write the file");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &tool_delta(
                    0,
                    "call_1",
                    "write_file",
                    &json!({"path": "pwn.txt", "content": "x"}),
                ),
                "tool_calls",
                (10, 5, 15),
            ));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("please write the file")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        !dir.path().join("pwn.txt").exists(),
        "write_file must be refused headlessly"
    );
    assert!(stderr.contains("write_file"), "{stderr}");

    turn1.assert();
    turn2.assert();
}

/// No key (D6: env only) → clean exit 1 before any provider traffic.
#[test]
fn run_without_api_key_fails_before_any_request() {
    let server = MockServer::start();
    let any = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200).body("must never be reached");
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("hi")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("OPENROUTER_API_KEY"), "{stderr}");
    assert_eq!(any.calls(), 0, "no request may be sent without a key");
}

/// `max_turns` (file-layer only, stage 1.9) stops the run with its own
/// exit code, and the limit is checked *before* the next provider call:
/// exactly one request ever hits the mock.
#[test]
fn run_max_turns_exits_distinctly_without_an_overshooting_call() {
    let dir = tempfile::tempdir().expect("temp workdir");
    std::fs::write(dir.path().join("hello.txt"), "x").unwrap();
    let server = MockServer::start();

    // Every answer is a tool call — only the turn limit can stop us.
    let tooly = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &tool_delta(0, "call_1", "read_file", &json!({"path": "hello.txt"})),
                "tool_calls",
                (10, 5, 15),
            ));
    });

    let cfg = tempfile::NamedTempFile::new().expect("config file");
    std::fs::write(cfg.path(), "max_turns = 1\n").unwrap();

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("loop forever")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_CONFIG", cfg.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");
    assert!(stderr.contains("max_turns"), "{stderr}");
    assert_eq!(tooly.calls(), 1, "no provider call past the turn limit");

    // The stop is durable: the transcript records why the run ended.
    let Some(Event::SessionEnd { outcome, .. }) =
        last_event_of_single_session(&dir.path().join("sessions"))
    else {
        panic!("a max_turns stop must be recorded as session_end");
    };
    assert!(matches!(outcome, Outcome::MaxTurns), "got {outcome:?}");
}

/// The token budget stops the run with its own exit code — checked
/// before the next provider call, so exactly one request was made.
#[test]
fn run_budget_exceeded_exits_distinctly_without_an_overshooting_call() {
    let dir = tempfile::tempdir().expect("temp workdir");
    std::fs::write(dir.path().join("hello.txt"), "x").unwrap();
    let server = MockServer::start();

    let tooly = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &tool_delta(0, "call_1", "read_file", &json!({"path": "hello.txt"})),
                "tool_calls",
                (10, 5, 15),
            ));
    });

    // Budget is file-layer only (stage 1.9): 1 token, turn 1 uses 15.
    let cfg = tempfile::NamedTempFile::new().expect("config file");
    std::fs::write(cfg.path(), "max_tokens_budget = 1\n").unwrap();

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("spend it all")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_CONFIG", cfg.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "stderr: {stderr}");
    assert!(stderr.contains("budget"), "{stderr}");
    assert_eq!(tooly.calls(), 1, "no provider call past the token budget");

    // The stop is durable: the transcript records why the run ended.
    let Some(Event::SessionEnd { outcome, .. }) =
        last_event_of_single_session(&dir.path().join("sessions"))
    else {
        panic!("a budget stop must be recorded as session_end");
    };
    assert!(
        matches!(outcome, Outcome::BudgetExceeded),
        "got {outcome:?}"
    );
}

/// A provider failure aborts the run with exit 1 and a readable message.
#[test]
fn run_provider_error_exits_one() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let server = MockServer::start();
    let boom = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(500).body("boom");
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("hi")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("HTTP 500"), "{stderr}");
    boom.assert();
}

/// A nonexistent workdir is a usage-level error: exit 1, no panic.
#[test]
fn run_missing_workdir_exits_one() {
    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("hi")
        .arg("--workdir")
        .arg("/nonexistent/iris-e2e-workdir")
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("workdir"), "{stderr}");
}

/// Stage 3.1 — no volatile content in the request body. The only mock
/// that will answer refuses bodies containing the workspace path or
/// today's date, so a prefix that leaks either (a) fails every match,
/// (b) 404s, (c) exits 1 — while a clean body streams normally.
#[test]
fn request_body_excludes_workspace_path_and_dates() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let canonical = std::fs::canonicalize(dir.path()).unwrap();
    let workspace = canonical.to_string_lossy().into_owned();
    let today = time::OffsetDateTime::now_utc().date();
    let today = format!(
        "{:04}-{:02}-{:02}",
        today.year(),
        u8::from(today.month()),
        today.day()
    );

    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_excludes(workspace.as_str())
            .body_excludes(today.as_str());
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("Clean prefix."), "stop", (3, 1, 4)));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("say hello")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the volatile-free body must match: {stderr}"
    );
    assert_eq!(reply.calls(), 1);
}

/// Stage 3.2 — OpenRouter's accounting travels end to end: the usage
/// chunk's cached tokens and USD charge land in the transcript's usage
/// event (alongside the estimate reconciliation), and `sessions list`
/// sums the charge into the cost column that used to be stuck on `-`.
#[test]
fn openrouter_accounting_reaches_the_transcript_and_the_listing() {
    let dir = tempfile::tempdir().expect("temp workdir");

    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body_with_usage(
                &text_delta("All accounted for."),
                "stop",
                &json!({
                    "prompt_tokens": 194,
                    "completion_tokens": 2,
                    "total_tokens": 196,
                    "cost": 0.0042,
                    "prompt_tokens_details": {"cached_tokens": 64}
                }),
            ));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("account me")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    reply.assert();

    // The transcript's usage event carries the full accounting plus
    // the estimated-vs-actual reconciliation.
    let store = dir.path().join("sessions");
    let transcript = std::fs::read_dir(&store)
        .expect("sessions store")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .expect("a transcript");
    let events = load(&transcript.path()).expect("parseable transcript");
    let usage_events: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::Usage {
                usage,
                input_token_delta,
                ..
            } => Some((usage, input_token_delta)),
            _ => None,
        })
        .collect();
    assert_eq!(usage_events.len(), 1, "one response, one usage event");
    let (usage, delta) = usage_events[0];
    assert_eq!(usage.cached_input_tokens, Some(64), "prompt cache reads");
    assert_eq!(usage.cost_usd, Some(0.0042), "the charge, in USD");
    assert!(
        delta.is_some(),
        "the estimate reconciliation must be recorded with the usage"
    );

    // `sessions list` sums the charge into the cost column (4 dp).
    let listed = iris()
        .arg("sessions")
        .arg("list")
        .env("IRIS_SESSIONS_DIR", &store)
        .output()
        .expect("sessions list must run");
    assert_eq!(
        listed.status.code(),
        Some(0),
        "stdout: {}",
        String::from_utf8_lossy(&listed.stdout)
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(
        stdout.trim_end().ends_with("\t0.0042"),
        "cost column must show the summed charge to 4 dp, got: {stdout}"
    );
}

/// Stage 3.3 — the config price table reaches the transcript end to
/// end: a run priced *only* by `[pricing."<model>"]` in the config
/// file records a `turn_metrics` event with ttft, cache hit % and the
/// table's estimated cost (the fixture reports cache reads but no
/// charge, so the estimate is the only possible source).
#[test]
fn price_table_costs_land_in_turn_metrics_events() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let config = dir.path().join("iris-config.toml");
    std::fs::write(
        &config,
        "[pricing.\"openai/gpt-4o-mini\"]\n\
         input_per_million = 0.15\n\
         output_per_million = 0.60\n\
         cached_input_per_million = 0.075\n",
    )
    .expect("write config file");

    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body_with_usage(
                &text_delta("Measured."),
                "stop",
                &json!({
                    "prompt_tokens": 194,
                    "completion_tokens": 2,
                    "total_tokens": 196,
                    "prompt_tokens_details": {"cached_tokens": 64}
                }),
            ));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("measure me")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_CONFIG", &config)
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    reply.assert();

    let store = dir.path().join("sessions");
    let transcript = std::fs::read_dir(&store)
        .expect("sessions store")
        .filter_map(Result::ok)
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
        .expect("a transcript");
    let events = load(&transcript.path()).expect("parseable transcript");
    let metrics: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::TurnMetrics(metrics) => Some(metrics),
            _ => None,
        })
        .collect();
    assert_eq!(metrics.len(), 1, "one response, one turn_metrics event");
    let metrics = metrics[0];
    assert_eq!(metrics.turn, 1, "same turn as the usage event");
    assert!(metrics.ttft_ms.is_some(), "a token did arrive");
    let hit = metrics.cache_hit_pct.expect("fixture reports cache reads");
    assert!(
        (hit - 64.0 * 100.0 / 194.0).abs() < 1e-9,
        "64 of 194 inputs hit cache, got {hit}"
    );
    let cost = metrics
        .cost_usd
        .expect("priced by the config table — the fixture reported no charge");
    assert!(
        (cost - 0.0000255).abs() < 1e-12,
        "130×0.15 + 64×0.075 + 2×0.60 per million, got {cost}"
    );
}

/// Stage 3.4 (Done-when) — against the `httpmock` fixture with cache
/// usage, the live gauge prints the turn's expected hit % and its cost
/// to 4 dp. ttft and tok/s are timing-dependent, so the line is found
/// by prefix and pinned from `cache` onward.
#[test]
fn live_gauge_prints_expected_hit_percent_and_cost_to_4dp() {
    let dir = tempfile::tempdir().expect("temp workdir");

    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body_with_usage(
                &text_delta("Gauged."),
                "stop",
                &json!({
                    "prompt_tokens": 194,
                    "completion_tokens": 2,
                    "total_tokens": 196,
                    "prompt_tokens_details": {"cached_tokens": 64},
                    "cost": 0.0042
                }),
            ));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("gauge me")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    reply.assert();

    let gauge = stderr
        .lines()
        .find(|line| line.starts_with("[iris] turn 1"))
        .unwrap_or_else(|| panic!("the gauge status line must print, stderr: {stderr}"));
    assert!(
        gauge.contains("ttft ") && gauge.contains("tok/s"),
        "live timings ride the line: {gauge}"
    );
    assert!(
        gauge.ends_with("· cache 32.99% · cost $0.0042"),
        "expected hit % and cost to 4 dp: {gauge}"
    );
}
