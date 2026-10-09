//! `run_command` shell tool (stage 1.7): PTY spawn, streamed capture,
//! timeout + kill — **always routed through the permission gate**.
//!
//! The command runs inside a real pseudo-terminal ([`portable-pty`]), so
//! children behave like in a terminal (colors, progress bars, no
//! "not a tty" fallbacks). A PTY has a single stream: stdout and stderr
//! arrive **merged** — separation is impossible by construction, and the
//! schema says so.
//!
//! Capture is *streamed* into a bounded buffer from a non-blocking
//! master fd on a worker thread: no deadlock on the small pty buffer,
//! bounded memory (the reader keeps draining past the cap so the child
//! never stalls), and a hard deadline so the reader always returns —
//! even when a backgrounded grandchild holds the pty open.
//!
//! The result always tells the model *why* a command stopped: exit-code
//! trailer on failure, timeout trailer + kill, truncation trailer.

use std::io::Read as _;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::registry::{Tool, ToolContext, ToolError};
use crate::types::ToolResult;

/// Default wall-clock timeout when the model doesn't ask for one.
pub const DEFAULT_TIMEOUT_SECS: u32 = 60;
/// Hard cap — the model cannot extend beyond this.
pub const MAX_TIMEOUT_SECS: u32 = 600;
/// Captured output cap; larger output is truncated **with a visible
/// notice** (the reader keeps draining so the child never blocks).
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Poll interval for the reader while the non-blocking pty has no data.
const READER_POLL_MS: u64 = 10;
/// Poll interval for the exit-status check loop.
const EXIT_POLL_MS: u64 = 10;
/// Extra time the reader may live past the timeout to observe the kill.
const KILL_GRACE_SECS: u64 = 2;
/// Settle window for output that arrives right after the child exits
/// (e.g. a backgrounded writer still holding the pty).
const READER_GRACE_AFTER_EXIT_MS: u64 = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// Command line, executed through the user's shell inside a PTY.
    /// stdout and stderr are captured merged (a PTY has one stream).
    command: String,
    /// Optional wall-clock timeout in seconds (default 60, max 600).
    /// The process is killed when it expires.
    #[serde(default)]
    timeout_secs: Option<u32>,
}

pub struct RunCommandTool;

impl Tool for RunCommandTool {
    type Args = ShellArgs;

    fn name(&self) -> &'static str {
        "run_command"
    }

    fn description(&self) -> &'static str {
        "Runs a command line through the user's shell in a PTY (timeout \\\n         kills the process group; output capped at 256 KiB)"
    }

    async fn execute(&self, ctx: &ToolContext, args: ShellArgs) -> Result<ToolResult, ToolError> {
        let command = args.command.as_str();
        let timeout = effective_timeout(args.timeout_secs);

        // Resolve the user's shell ($SHELL when executable, password-db
        // fallback) without spawning anything.
        let shell = portable_pty::CommandBuilder::new("true").get_shell();

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| self.failed(format!("openpty failed: {err}")))?;

        let mut cmd = portable_pty::CommandBuilder::new(&shell);
        cmd.arg("-c");
        cmd.arg(command);
        cmd.cwd(&ctx.workspace_root);

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| self.failed(format!("spawn failed: {err}")))?;
        // The child holds its own slave fds; ours must go or EOF never comes.
        drop(pair.slave);
        // We never send stdin; close the write half (matters on Windows —
        // a unix pty master fd is a single bidirectional fd).
        drop(pair.master.take_writer().ok());

        let pid = child.process_id();

        // Streamed, bounded capture on a worker thread over a non-blocking
        // master fd: no deadlock on the small pty buffer, and a hard
        // deadline so the reader always returns — even when a backgrounded
        // grandchild keeps the pty open.
        if let Some(fd) = pair.master.as_raw_fd() {
            set_nonblocking(fd);
        }
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| self.failed(format!("pty reader unavailable: {err}")))?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let deadline = Instant::now() + timeout + Duration::from_secs(KILL_GRACE_SECS);
        tokio::task::spawn_blocking(move || read_pty(reader, deadline, tx));

        // Reap via a poll loop so `child` stays borrowable for the kill.
        let (exit, timed_out) = match tokio::time::timeout(timeout, poll_exit(child.as_mut())).await
        {
            Ok(Ok(status)) => (
                Some(ExitFacts {
                    code: status.exit_code(),
                    signal: status.signal().map(str::to_string),
                }),
                false,
            ),
            Ok(Err(err)) => {
                kill_group(child.as_mut(), pid);
                return Err(self.failed(format!("waiting for exit status: {err}")));
            }
            Err(_elapsed) => {
                // The pty spawn runs the child as session+group leader
                // (setsid), so signaling the group kills grandchildren too.
                kill_group(child.as_mut(), pid);
                let _ = tokio::time::timeout(
                    Duration::from_secs(KILL_GRACE_SECS),
                    poll_exit(child.as_mut()),
                )
                .await;
                (None, true)
            }
        };

        // Collect until the reader finishes or the settle window closes
        // (a background holder of the pty keeps the thread alive until its
        // own deadline — the partial capture is still useful).
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut read_failure = None;
        let collect = async {
            while let Some(msg) = rx.recv().await {
                match msg {
                    Capture::Chunk(chunk) => {
                        let room = MAX_OUTPUT_BYTES.saturating_sub(bytes.len());
                        let take = chunk.len().min(room);
                        bytes.extend_from_slice(&chunk[..take]);
                        if take < chunk.len() {
                            truncated = true;
                        }
                    }
                    Capture::Failed(message) => {
                        read_failure = Some(message);
                        break;
                    }
                }
            }
        };
        let settled =
            tokio::time::timeout(Duration::from_millis(READER_GRACE_AFTER_EXIT_MS), collect).await;
        let _ = settled; // Err = grandchild still holds the pty; keep partials
        if let Some(message) = read_failure {
            return Err(self.failed(message));
        }

        let output = normalize_newlines(&bytes);
        let (content, is_error) = assemble_content(&output, exit, timed_out, truncated, timeout);
        Ok(ToolResult {
            tool_use_id: String::new(),
            content,
            is_error,
        })
    }
}

impl RunCommandTool {
    fn failed(&self, message: impl Into<String>) -> ToolError {
        ToolError::Failed {
            tool: self.name().to_string(),
            message: message.into(),
        }
    }
}

/// A message from the capture thread.
enum Capture {
    Chunk(Vec<u8>),
    Failed(String),
}

/// Worker thread: stream pty output until EOF/EIO or the hard deadline.
/// Never blocks (non-blocking fd + short poll); forwards chunks and lets
/// the receiver apply the size cap.
fn read_pty(
    mut reader: Box<dyn std::io::Read + Send>,
    deadline: Instant,
    tx: mpsc::UnboundedSender<Capture>,
) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // child side fully closed
            Ok(n) => {
                if tx.send(Capture::Chunk(buf[..n].to_vec())).is_err() {
                    break; // receiver gone
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(READER_POLL_MS));
            }
            // A pty master reads EIO on linux once the slave side is gone
            // — that is our EOF.
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            Err(err) => {
                let _ = tx.send(Capture::Failed(format!("reading pty output: {err}")));
                break;
            }
        }
    }
}

/// Reap the child without blocking a runtime thread.
async fn poll_exit(
    child: &mut (dyn portable_pty::Child + Send + Sync),
) -> std::io::Result<portable_pty::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(EXIT_POLL_MS)).await;
    }
}

/// SIGKILL the child's process group, then the child itself. The group
/// reaches grandchildren that would otherwise hold the pty open.
fn kill_group(child: &mut (dyn portable_pty::Child + Send + Sync), pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags != -1 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Resolve the requested timeout against the default and hard cap.
pub(crate) fn effective_timeout(requested: Option<u32>) -> Duration {
    let secs = requested
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);
    Duration::from_secs(u64::from(secs))
}

/// The pty line discipline turns `\n` into `\r\n`; normalize back so the
/// model sees plain text.
fn normalize_newlines(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.into_owned()
    }
}

/// How the child exited (portable-pty always has a code; signal deaths
/// additionally name the signal and report code 1).
pub(crate) struct ExitFacts {
    code: u32,
    signal: Option<String>,
}

/// Assemble the tool result: normalized output plus explicit trailers
/// for truncation / timeout / nonzero exit / signal death. Returns
/// `(content, is_error)`.
fn assemble_content(
    output: &str,
    exit: Option<ExitFacts>,
    timed_out: bool,
    truncated: bool,
    timeout: Duration,
) -> (String, bool) {
    let mut content = output.to_string();
    if truncated {
        content.push_str(&format!(
            "\n[ferro: output truncated at {MAX_OUTPUT_BYTES} bytes]"
        ));
    }
    let is_error = if timed_out {
        content.push_str(&format!(
            "\n[ferro: timed out after {}s — process killed]",
            timeout.as_secs()
        ));
        true
    } else {
        match exit {
            Some(ExitFacts {
                code,
                signal: Some(signal),
            }) => {
                content.push_str(&format!("\n[ferro: killed by {signal} (exit code {code})]"));
                true
            }
            Some(ExitFacts { code, signal: None }) if code != 0 => {
                content.push_str(&format!("\n[ferro: exit code {code}]"));
                true
            }
            _ => false,
        }
    };
    (content, is_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use serde_json::json;

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path());
        (dir, ctx)
    }

    /// Run a command through the registry (the real dispatch path).
    async fn run(
        ctx: &ToolContext,
        command: &str,
        timeout_secs: Option<u32>,
    ) -> Result<ToolResult, ToolError> {
        let mut registry = ToolRegistry::new();
        registry.register(RunCommandTool);
        registry
            .execute(
                "run_command",
                ctx,
                "call_1",
                json!({"command": command, "timeout_secs": timeout_secs}),
            )
            .await
    }

    // --- pure helpers (RED-phase-tested) --------------------------------

    #[test]
    fn timeout_defaults_and_caps() {
        assert_eq!(effective_timeout(None), Duration::from_secs(60));
        assert_eq!(effective_timeout(Some(5)), Duration::from_secs(5));
        assert_eq!(effective_timeout(Some(0)), Duration::from_secs(1));
        assert_eq!(effective_timeout(Some(100_000)), Duration::from_secs(600));
    }

    #[test]
    fn pty_crlf_is_normalized() {
        assert_eq!(normalize_newlines(b"a\r\nb\r\n"), "a\nb\n");
        assert_eq!(normalize_newlines(b"plain\n"), "plain\n");
        assert_eq!(normalize_newlines(b""), "");
    }

    #[test]
    fn assemble_reports_clean_success_silently() {
        let exit = ExitFacts {
            code: 0,
            signal: None,
        };
        let (content, is_error) =
            assemble_content("done\n", Some(exit), false, false, Duration::from_secs(60));
        assert_eq!(content, "done\n");
        assert!(!is_error);
    }

    #[test]
    fn assemble_reports_nonzero_exit_with_trailer() {
        let exit = ExitFacts {
            code: 3,
            signal: None,
        };
        let (content, is_error) =
            assemble_content("boom\n", Some(exit), false, false, Duration::from_secs(60));
        assert!(content.contains("[ferro: exit code 3]"), "{content}");
        assert!(is_error);
    }

    #[test]
    fn assemble_reports_signal_death_with_trailer() {
        let exit = ExitFacts {
            code: 1,
            signal: Some("SIGKILL".to_string()),
        };
        let (content, is_error) =
            assemble_content("gone\n", Some(exit), false, false, Duration::from_secs(60));
        assert!(
            content.contains("[ferro: killed by SIGKILL (exit code 1)]"),
            "{content}"
        );
        assert!(is_error);
    }

    #[test]
    fn assemble_reports_timeout_with_trailer() {
        let (content, is_error) =
            assemble_content("partial", None, true, false, Duration::from_secs(1));
        assert!(content.starts_with("partial"));
        assert!(
            content.contains("[ferro: timed out after 1s — process killed]"),
            "{content}"
        );
        assert!(is_error);
    }

    #[test]
    fn assemble_reports_truncation_with_trailer() {
        let exit = ExitFacts {
            code: 0,
            signal: None,
        };
        let (content, is_error) =
            assemble_content("big", Some(exit), false, true, Duration::from_secs(60));
        assert!(content.contains("[ferro: output truncated"), "{content}");
        assert!(!is_error, "truncation alone is not an error");
    }

    // --- process behavior (RED until the pty core is implemented) -------

    #[tokio::test]
    async fn captures_stdout_of_simple_command() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, "echo hello", None).await.unwrap();
        assert_eq!(result.content.trim(), "hello");
        assert!(!result.is_error);
        assert_eq!(result.tool_use_id, "call_1", "registry stamps the id");
    }

    #[tokio::test]
    async fn shell_syntax_is_interpreted_not_execve() {
        let (_dir, ctx) = workspace();
        // A pipeline only works if a *shell* parsed the line — and it is
        // valid in both POSIX shells and fish (the user's $SHELL may be
        // either).
        let result = run(&ctx, "echo hello | cat", None).await.unwrap();
        assert_eq!(result.content.trim(), "hello");
    }

    #[tokio::test]
    async fn stderr_arrives_merged_through_the_pty() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, "echo danger >&2", None).await.unwrap();
        assert!(
            result.content.contains("danger"),
            "stderr must be captured (merged on a pty): {:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_with_code_trailer() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, "exit 3", None).await.unwrap();
        assert!(result.is_error);
        assert!(
            result.content.contains("[ferro: exit code 3]"),
            "{:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn command_runs_in_the_workspace_root() {
        let (dir, ctx) = workspace();
        let result = run(&ctx, "pwd", None).await.unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        assert!(
            result.content.contains(canonical.to_str().unwrap()),
            "cwd must be the workspace root: {:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn carriage_returns_from_the_line_discipline_are_gone() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, "printf 'a\\nb\\n'", None).await.unwrap();
        assert!(!result.content.contains('\r'), "{:?}", result.content);
        assert!(result.content.contains("a\nb"), "{:?}", result.content);
    }

    #[tokio::test]
    async fn huge_output_is_truncated_with_visible_notice() {
        let (_dir, ctx) = workspace();
        // ~1.7 MB of output against the 256 KiB cap.
        let result = run(&ctx, "seq 1 300000", None).await.unwrap();
        assert!(result.content.len() <= MAX_OUTPUT_BYTES + 200);
        assert!(
            result.content.contains("[ferro: output truncated at"),
            "truncation must be visible"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_process_and_says_so() {
        let (dir, ctx) = workspace();
        let marker = dir.path().join("survived");
        // If the sleep survived the kill it would touch the marker at ~2s;
        // we check well after that.
        let command = format!("sleep 2 && touch {}", marker.display());

        let started = Instant::now();
        let result = run(&ctx, &command, Some(1)).await.unwrap();
        let elapsed = started.elapsed();
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("[ferro: timed out after 1s — process killed]"),
            "{:?}",
            result.content
        );
        assert!(elapsed < Duration::from_secs(10), "hung for {elapsed:?}");

        tokio::time::sleep(Duration::from_millis(1800)).await;
        assert!(
            !marker.exists(),
            "the killed process must not have survived"
        );
    }

    #[tokio::test]
    async fn empty_command_is_a_clean_no_op() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, "", None).await.unwrap();
        assert!(!result.is_error);
        // Some shells (fish) print a stray newline for an empty command.
        assert!(result.content.trim().is_empty(), "{:?}", result.content);
    }

    // --- gate routing (end-to-end; fails until the dispatch gate lands) --

    #[tokio::test]
    async fn denied_command_never_spawns() {
        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("pwned");
        let gated = ToolContext::with_gate(
            dir.path(),
            std::sync::Arc::new(crate::tools::permissions::DenyAll),
        );

        let mut registry = ToolRegistry::new();
        registry.register(RunCommandTool);
        let err = registry
            .execute(
                "run_command",
                &gated,
                "call_1",
                json!({"command": format!("touch {}", marker.display())}),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
        assert!(!marker.exists(), "denied command must never spawn");
    }
}
