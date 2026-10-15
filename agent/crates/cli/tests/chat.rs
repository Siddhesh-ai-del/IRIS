//! Stage 4.1 — `iris chat` (the TUI shell) from the outside.
//!
//! The TUI needs a real terminal; these tests pin the contract
//! *around* it, and both are deliberately hang-proof: neither may
//! reach a state where the shell waits on a terminal nobody types
//! into.

mod common;

use std::process::Command;

/// The full session pipeline runs, and it fails fast — before any
/// terminal takeover — when there is no key. (The stage-4.0 stub
/// printed "not implemented" and exited 2; this proves the real
/// `chat` command is wired.)
#[test]
fn chat_without_a_key_fails_fast_before_touching_the_terminal() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let out = common::iris()
        .arg("chat")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("OPENROUTER_API_KEY"),
        "clean error: {stderr}"
    );
    assert!(
        !stderr.contains("not implemented"),
        "the stub is gone: {stderr}"
    );
}

/// Without a controlling tty the shell reports it cleanly instead of
/// hijacking or hanging. `setsid -w` detaches the child from any
/// terminal, so crossterm's `/dev/tty` fallback cannot reach the
/// developer's real session — running this from a live terminal must
/// never steal it.
#[test]
fn chat_without_a_tty_fails_fast_with_a_clean_error() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let out = Command::new("setsid")
        .arg("-w")
        .arg(env!("CARGO_BIN_EXE_iris"))
        .arg("chat")
        .arg("--workdir")
        .arg(dir.path())
        .env("IRIS_CONFIG", "/nonexistent/iris-e2e-config.toml")
        .env_remove("OPENROUTER_API_KEY")
        .env("IRIS_SESSIONS_DIR", dir.path().join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .output()
        .expect("setsid must be available on Linux (util-linux)");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("raw mode"), "clean error: {stderr}");
}
