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

use common::{iris, sse_body, text_delta, tool_delta};
use httpmock::prelude::*;
use serde_json::json;

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
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(3), "stderr: {stderr}");
    assert!(stderr.contains("max_turns"), "{stderr}");
    assert_eq!(tooly.calls(), 1, "no provider call past the turn limit");
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
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(4), "stderr: {stderr}");
    assert!(stderr.contains("budget"), "{stderr}");
    assert_eq!(tooly.calls(), 1, "no provider call past the token budget");
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
