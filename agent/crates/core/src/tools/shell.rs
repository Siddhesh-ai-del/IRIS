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
use std::io::Write as _;
#[cfg(unix)]
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
        // fallback) without spawning anything. On Windows the shell is
        // %ComSpec% (cmd.exe) — there is no $SHELL equivalent.
        #[cfg(unix)]
        let shell = portable_pty::CommandBuilder::new("true").get_shell();
        #[cfg(windows)]
        let shell = std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into());

        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| self.failed(format!("openpty failed: {err}")))?;

        let mut cmd = portable_pty::CommandBuilder::new(&shell);
        // `sh -c` on unix, `cmd /C` on Windows.
        #[cfg(unix)]
        cmd.arg("-c");
        #[cfg(windows)]
        cmd.arg("/C");
        cmd.arg(command);
        cmd.cwd(&ctx.workspace_root);

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| self.failed(format!("spawn failed: {err}")))?;
        // The child holds its own slave fds; ours must go or EOF never comes.
        drop(pair.slave);
        // We never send stdin. On unix the master write half must go now or
        // a stdin-reading child never sees EOF (a master fd is a single
        // bidirectional fd). On Windows the writer is the terminal's voice:
        // closing it reads as the terminal hanging up — conhost tears the
        // session down and the child dies with 0xC000013A — and leaving
        // conhost's inherit-cursor query unanswered blocks the session
        // before the child runs at all. The reader owns the handle on
        // Windows: it answers `ESC[6n` like a real terminal, and the pipe
        // closes when the reader does.
        #[cfg(unix)]
        let input: Option<Box<dyn std::io::Write + Send>> = {
            drop(pair.master.take_writer().ok());
            None
        };
        #[cfg(windows)]
        let input = pair.master.take_writer().ok();

        let pid = child.process_id();

        // Streamed, bounded capture on a worker thread over a non-blocking
        // master fd: no deadlock on the small pty buffer, and a hard
        // deadline so the reader always returns — even when a backgrounded
        // grandchild keeps the pty open.
        // Windows: the reader below blocks instead — ConPTY reports EOF
        // as a broken pipe once the child tree dies, and the master
        // handle drops at the end of this function, unblocking it.
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            set_nonblocking(fd);
        }
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| self.failed(format!("pty reader unavailable: {err}")))?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let deadline = Instant::now() + timeout + Duration::from_secs(KILL_GRACE_SECS);
        tokio::task::spawn_blocking(move || read_pty(reader, input, deadline, tx));

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
        #[cfg(windows)]
        let output = strip_conpty_chatter(&output);
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
///
/// It also plays the outer terminal's half of ConPTY's inherit-cursor
/// handshake on Windows: conhost writes `ESC[6n` and blocks the session
/// until the "terminal" answers `ESC[row;colR` on the input pipe — we
/// answer `1;1` (conhost's own assumed startup position), or the child
/// never runs. `input` is `None` on unix, where the pty does no handshake.
fn read_pty(
    mut reader: Box<dyn std::io::Read + Send>,
    mut input: Option<Box<dyn std::io::Write + Send>>,
    deadline: Instant,
    tx: mpsc::UnboundedSender<Capture>,
) {
    let mut buf = [0u8; 8192];
    // Rolling tail so a query split across two reads is still seen.
    let mut tail: Vec<u8> = Vec::new();
    let mut answered = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break, // child side fully closed
            Ok(n) => {
                if !answered && input.is_some() {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.windows(4).any(|window| window == b"\x1b[6n")
                        && let Some(terminal) = input.as_mut()
                        && terminal.write_all(b"\x1b[1;1R").is_ok()
                    {
                        answered = true;
                    }
                    if tail.len() > 8 {
                        tail.drain(..tail.len() - 4);
                    }
                }
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
            #[cfg(unix)]
            Err(err) if err.raw_os_error() == Some(libc::EIO) => break,
            // Windows pipes report the same EOF as a broken/aborted pipe.
            Err(err)
                if err.kind() == std::io::ErrorKind::BrokenPipe
                    || err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
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
#[cfg(unix)]
fn kill_group(child: &mut (dyn portable_pty::Child + Send + Sync), pid: Option<u32>) {
    if let Some(pid) = pid {
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

/// Windows: the pty spawn runs `cmd.exe`, which spawns the actual
/// command as a grandchild. `TerminateProcess` on the child alone would
/// orphan the tree (and the pty with it), so walk it with
/// `taskkill /T /F` — then still `kill()` the direct child as a belt.
#[cfg(windows)]
fn kill_group(child: &mut (dyn portable_pty::Child + Send + Sync), pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
}

#[cfg(unix)]
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

/// ConPTY's connection chatter lands at the front of every Windows capture
/// before the child prints anything — it is the pty talking to *us*, not
/// the command's output; unix captures have no equivalent. Two waves, both
/// prefix-only: the attach handshake (cursor-inherit query + mode enables)
/// and conhost/cmd's startup render (attribute reset, cmd's title change
/// forwarded as OSC 0, cursor show). Stripping stops at the first byte
/// that is not one of these, so a command that legitimately opens with its
/// own ANSI sequence keeps it.
#[cfg(windows)]
fn strip_conpty_chatter(content: &str) -> String {
    const FIXED: [&str; 5] = [
        "\x1b[6n",     // inherit-cursor query
        "\x1b[?9001h", // win32-input-mode enable
        "\x1b[?1004h", // focus-reporting enable
        "\x1b[m",      // SGR reset (initial render)
        "\x1b[?25h",   // cursor show (initial render)
    ];
    let mut rest = content;
    loop {
        let before = rest;
        for seq in FIXED {
            if let Some(tail) = rest.strip_prefix(seq) {
                rest = tail;
            }
        }
        // cmd sets its window title at startup: `ESC]0;<title>BEL`
        // (variable content — matched to its terminator, BEL or ST).
        if let Some(after) = rest.strip_prefix("\x1b]0;") {
            if let Some(bel) = after.find('\x07') {
                rest = &after[bel + 1..];
            } else if let Some(st) = after.find("\x1b\\") {
                rest = &after[st + 2..];
            }
        }
        if rest.len() == before.len() {
            return rest.to_string();
        }
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
            "\n[iris: output truncated at {MAX_OUTPUT_BYTES} bytes]"
        ));
    }
    let is_error = if timed_out {
        content.push_str(&format!(
            "\n[iris: timed out after {}s — process killed]",
            timeout.as_secs()
        ));
        true
    } else {
        match exit {
            Some(ExitFacts {
                code,
                signal: Some(signal),
            }) => {
                content.push_str(&format!("\n[iris: killed by {signal} (exit code {code})]"));
                true
            }
            Some(ExitFacts { code, signal: None }) if code != 0 => {
                content.push_str(&format!("\n[iris: exit code {code}]"));
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

    /// Platform command table: the test bodies are shared, only the
    /// shell syntax differs (`sh` vs `cmd.exe`).
    #[cfg(unix)]
    mod platform {
        pub const PIPE: &str = "echo hello | cat";
        pub const MERGED_STDERR: &str = "echo danger >&2";
        pub const MULTI_LINE: &str = "printf 'a\\nb\\n'";
        // ~1.7 MB of output against the 256 KiB cap.
        pub const HUGE: &str = "seq 1 300000";
        pub fn sleep_then_touch(marker: &std::path::Path) -> String {
            format!("sleep 2 && touch {}", marker.display())
        }
    }
    #[cfg(windows)]
    mod platform {
        pub const PIPE: &str = "echo hello | findstr hello";
        pub const MERGED_STDERR: &str = "echo danger 1>&2";
        pub const MULTI_LINE: &str = "echo a&echo b";
        // ~1.1 MB over the cap; cmd has no `seq`, and a for-loop of
        // builtin echos stays fast enough for the 60s default timeout.
        pub const HUGE: &str =
            "for /L %i in (1,1,30000) do @echo 0123456789012345678901234567890123";
        pub fn sleep_then_touch(marker: &std::path::Path) -> String {
            // ping waits ~1s per hop: 3 hops ≈ 2s, past the 1s timeout.
            format!(
                "ping -n 3 127.0.0.1 > nul && echo x> \"{}\"",
                marker.display()
            )
        }
    }
    use platform::*;

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
        assert!(content.contains("[iris: exit code 3]"), "{content}");
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
            content.contains("[iris: killed by SIGKILL (exit code 1)]"),
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
            content.contains("[iris: timed out after 1s — process killed]"),
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
        assert!(content.contains("[iris: output truncated"), "{content}");
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
        // either); on Windows cmd parses its own pipeline syntax.
        let result = run(&ctx, PIPE, None).await.unwrap();
        assert_eq!(result.content.trim(), "hello");
    }

    #[tokio::test]
    async fn stderr_arrives_merged_through_the_pty() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, MERGED_STDERR, None).await.unwrap();
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
            result.content.contains("[iris: exit code 3]"),
            "{:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn command_runs_in_the_workspace_root() {
        let (dir, ctx) = workspace();
        // A file probe, not a path echo: cmd's `cd` and unix `pwd`
        // format paths differently from `canonicalize()`, but a file
        // appearing in the workspace root proves the cwd either way.
        let result = run(&ctx, "echo probe > cwd_probe.txt", None).await.unwrap();
        assert!(!result.is_error, "{:?}", result.content);
        assert!(
            dir.path().join("cwd_probe.txt").exists(),
            "the workspace root must be the process cwd"
        );
    }

    #[tokio::test]
    async fn carriage_returns_from_the_line_discipline_are_gone() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, MULTI_LINE, None).await.unwrap();
        assert!(!result.content.contains('\r'), "{:?}", result.content);
        assert!(result.content.contains("a\nb"), "{:?}", result.content);
    }

    #[tokio::test]
    async fn huge_output_is_truncated_with_visible_notice() {
        let (_dir, ctx) = workspace();
        let result = run(&ctx, HUGE, None).await.unwrap();
        assert!(result.content.len() <= MAX_OUTPUT_BYTES + 200);
        assert!(
            result.content.contains("[iris: output truncated at"),
            "truncation must be visible"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_process_and_says_so() {
        let (dir, ctx) = workspace();
        let marker = dir.path().join("survived");
        // If the sleep survived the kill it would touch the marker at ~2s;
        // we check well after that.
        let command = sleep_then_touch(&marker);

        let started = Instant::now();
        let result = run(&ctx, &command, Some(1)).await.unwrap();
        let elapsed = started.elapsed();
        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("[iris: timed out after 1s — process killed]"),
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
