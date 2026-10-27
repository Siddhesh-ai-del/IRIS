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

/// Spawn `iris chat` with no way to reach a terminal. `setsid -w`
/// (unix) detaches the child from any controlling terminal, so
/// crossterm's `/dev/tty` fallback cannot reach the developer's real
/// session — running this from a live terminal must never steal it.
/// Windows has no controlling tty to escape: `.output()` pipes are
/// already not a console, and crossterm only ever touches the stdin
/// console handle — pipes make raw-mode setup fail cleanly.
#[cfg(unix)]
fn spawn_chat_without_a_tty(dir: &std::path::Path) -> std::process::Output {
    Command::new("setsid")
        .arg("-w")
        .arg(env!("CARGO_BIN_EXE_iris"))
        .arg("chat")
        .arg("--workdir")
        .arg(dir)
        .env("IRIS_CONFIG", "/nonexistent/iris-e2e-config.toml")
        .env_remove("OPENROUTER_API_KEY")
        .env("IRIS_SESSIONS_DIR", dir.join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .output()
        .expect("setsid must be available on Linux (util-linux)")
}

#[cfg(windows)]
fn spawn_chat_without_a_tty(dir: &std::path::Path) -> std::process::Output {
    use std::os::windows::process::CommandExt;
    // DETACHED_PROCESS ≈ setsid: the child gets no console at all, so
    // crossterm's CONIN$ lookup fails (it opens the *console*, never
    // the piped stdin) and raw-mode setup reports the clean error
    // instead of starting a TUI on the runner's console.
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let mut child = Command::new(env!("CARGO_BIN_EXE_iris"))
        .arg("chat")
        .arg("--workdir")
        .arg(dir)
        .env("IRIS_CONFIG", "/nonexistent/iris-e2e-config.toml")
        .env_remove("OPENROUTER_API_KEY")
        .env("IRIS_SESSIONS_DIR", dir.join("sessions"))
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .creation_flags(DETACHED_PROCESS)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("iris chat must spawn");

    // Bounded wait: if chat ever reaches a TUI instead of failing, kill
    // it — a suite hang is never an acceptable failure mode.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                break;
            }
            Err(err) => panic!("reaping chat failed: {err}"),
        }
    }
    child.wait_with_output().expect("iris chat must be reaped")
}

#[test]
fn chat_without_a_tty_fails_fast_with_a_clean_error() {
    let dir = tempfile::tempdir().expect("temp workdir");
    let out = spawn_chat_without_a_tty(dir.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("raw mode"), "clean error: {stderr}");
}
