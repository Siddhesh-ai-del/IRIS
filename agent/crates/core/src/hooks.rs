//! User hooks (stage 6.4): configured shell commands fired at the
//! agent loop's four lifecycle points — `on_tool_pre`, `on_tool_post`,
//! `on_turn_end`, `on_compact`.
//!
//! Contract (mirrored in [`crate::config::Hooks`]):
//!
//! - **Payload** — the event's JSON object (field shapes are pinned by
//!   the loop's call sites and `tests/hooks.rs`) plus an `"event"`
//!   name, passed through the stage-5.1 redaction chokepoint and
//!   newline-terminated, arrives on the command's **stdin**.
//! - **cwd** — the workspace root; stdout and stderr are discarded
//!   (a hook that needs output writes it to its own file).
//! - **Deadline** — [`crate::config::Hooks::timeout_secs`] total per
//!   command, then the child is killed. `0` clamps to one second —
//!   there is deliberately no "wait forever".
//! - **Advisory failures** — a bad command, non-zero exit, or timeout
//!   becomes a [`LoopEvent::HookWarning`](crate::agent_loop::LoopEvent)
//!   the runner prints; the run continues. Hooks observe; they never
//!   block (there is no deny-via-hook: the permission gate stays the
//!   one enforcement point, stage 1.7).
//!
//! The `[hooks]` table is trusted exactly like `[mcp]` spawn commands:
//! user-authored code in the user's own config file, run with the
//! process environment.

use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Hooks;

/// One of the four lifecycle points a hook list attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hook {
    /// Before a tool dispatch — the model's request as asked, before
    /// the permission gate resolves it.
    ToolPre,
    /// After a tool dispatch — the outcome the model will see
    /// (`is_error` covers denials, failures, and errors alike).
    ToolPost,
    /// After a completed turn, the final one included.
    TurnEnd,
    /// After compaction folded the history, before the next request.
    Compact,
}

impl Hook {
    /// The `[hooks]` key **and** the payload's `event` value — the
    /// same string, so a hook can switch on the name it was
    /// configured by.
    pub fn key(self) -> &'static str {
        match self {
            Hook::ToolPre => "on_tool_pre",
            Hook::ToolPost => "on_tool_post",
            Hook::TurnEnd => "on_turn_end",
            Hook::Compact => "on_compact",
        }
    }
}

/// Longest free-text string (`tool.content`, `text`, `summary`)
/// carried in a payload — hooks get a bounded preview, not the whole
/// file a tool may have just read.
pub fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 4096;
    if text.chars().count() <= MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_CHARS).collect();
    format!("{head}…[iris truncated this preview]")
}

/// One lifecycle point's configured commands: the payload stamped with
/// `event`, redacted (5.1), and delivered to each command in order.
///
/// Returns one warning per failed command (`[]` when everything
/// succeeded or nothing is configured). **Never fails the run** —
/// hooks are side channels, not control flow.
pub async fn fire(hooks: &Hooks, which: Hook, cwd: &Path, payload: &Value) -> Vec<String> {
    let commands: &[String] = match which {
        Hook::ToolPre => &hooks.on_tool_pre,
        Hook::ToolPost => &hooks.on_tool_post,
        Hook::TurnEnd => &hooks.on_turn_end,
        Hook::Compact => &hooks.on_compact,
    };
    if commands.is_empty() {
        return Vec::new();
    }

    // Stamp the event name into a copy, then redact the whole thing
    // at the chokepoint — a payload never leaves this process
    // carrying a live secret. Newline-terminated, so a hook that
    // appends stdin to a log lands one parseable line per fire.
    let mut full = payload.clone();
    if let Some(object) = full.as_object_mut() {
        object.insert("event".into(), Value::String(which.key().into()));
    }
    let mut bytes = crate::redact::redact_json(&full).to_string().into_bytes();
    bytes.push(b'\n');

    let configured = hooks.timeout_secs.max(1);
    let timeout = Duration::from_secs(configured);
    let key = which.key();
    let mut warnings = Vec::new();
    for command in commands {
        match run_one(command, cwd, &bytes, timeout).await {
            Ok(status) if status.success() => {}
            Ok(status) => warnings.push(format!("{key}: `{command}` {status}")),
            Err(FireError::Spawn(err)) => {
                warnings.push(format!("{key}: `{command}` failed to start: {err}"));
            }
            Err(FireError::Io(err)) => {
                warnings.push(format!(
                    "{key}: `{command}` failed to deliver the payload: {err}"
                ));
            }
            Err(FireError::Timeout) => {
                warnings.push(format!("{key}: `{command}` timed out after {configured}s"));
            }
        }
    }
    warnings
}

/// Why one hook command could not produce an exit status.
#[derive(Debug)]
enum FireError {
    /// The shell itself could not be spawned.
    Spawn(std::io::Error),
    /// Delivering the payload (or waiting) failed.
    Io(std::io::Error),
    /// The deadline expired; the child was killed.
    Timeout,
}

/// Spawn `sh -c command` with `cwd`, deliver the payload on stdin,
/// and wait under **one** deadline — killing the child when the
/// deadline expires or delivery fails hard, so no orphaned hook
/// lingers. A child that closes stdin without reading it (`exit 9`,
/// `exec 0<&-`) gets EPIPE on the write: that is a *declined payload*,
/// not a failed delivery — the payload is advisory and the child's
/// exit status is the contract — so it is still reaped normally.
async fn run_one(
    command: &str,
    cwd: &Path,
    payload: &[u8],
    timeout: Duration,
) -> Result<ExitStatus, FireError> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(FireError::Spawn)?;

    let deadline = tokio::time::Instant::now() + timeout;

    // Deliver the payload, then **close** the write end: the EOF is
    // what tells a reading command its input is complete (a mere
    // flush would leave it blocked on read until the timeout).
    let mut stdin = child.stdin.take().expect("stdin was piped");
    let deliver = async move {
        match stdin.write_all(payload).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::BrokenPipe => {
                // The child closed stdin without reading — the
                // payload was declined. Not an error: reap the exit
                // status instead of reporting a bare broken pipe.
            }
            Err(err) => return Err(FireError::Io(err)),
        }
        drop(stdin);
        Ok::<(), FireError>(())
    };
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    match tokio::time::timeout(remaining, deliver).await {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            let _ = child.kill().await;
            return Err(err);
        }
        Err(_) => {
            let _ = child.kill().await;
            return Err(FireError::Timeout);
        }
    }

    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    match tokio::time::timeout(remaining, child.wait()).await {
        // `timeout` polls the future first, so a child that already
        // exited still reports its status even at the deadline edge.
        Ok(Ok(status)) => Ok(status),
        Ok(Err(err)) => Err(FireError::Io(err)),
        Err(_) => {
            let _ = child.kill().await;
            Err(FireError::Timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// A child that closes stdin without reading it must still be
    /// reaped for its **exit status** — an EPIPE on payload delivery
    /// means "declined the payload", not "delivery failed". The
    /// payload here is larger than any default pipe buffer, so the
    /// write is guaranteed to be pending when the child closes the
    /// read end: EPIPE every time, no scheduling race.
    #[tokio::test]
    async fn a_child_that_closes_stdin_still_reports_its_exit_status() {
        let payload = vec![b'x'; 256 * 1024];
        let status = run_one(
            "exec 0<&-; exit 9",
            Path::new("."),
            &payload,
            Duration::from_secs(10),
        )
        .await
        .expect("a closed stdin is a declined payload, not a delivery failure");
        assert_eq!(status.code(), Some(9));
    }
}
