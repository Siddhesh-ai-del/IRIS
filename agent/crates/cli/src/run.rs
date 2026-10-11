//! `iris run -p …` — the headless one-shot runner (stage 1.10).
//!
//! Runs one prompt through the agent loop with **no TUI**: model text
//! streams to **stdout** (pipeable), tool events and the run summary go
//! to **stderr**. The process exit code *is* the agent outcome, so CI
//! can branch on it:
//!
//! | code | meaning                                  |
//! |------|------------------------------------------|
//! | 0    | the model finished (no pending tools)    |
//! | 1    | error: config, missing key, provider, IO |
//! | 3    | `max_turns` reached while tools pending  |
//! | 4    | token budget exceeded                    |
//!
//! (2 stays reserved for clap usage errors.) Permissions fail closed:
//! the gate is the config policy (built-in: `read_file`/`list_dir`
//! allow, everything else ask) resolved by [`DenyOnAsk`], so nothing
//! that asks ever runs without a config override.
//!
//! Since stage 2.3 every run also persists a **session**: the manifest
//! plus a JSONL transcript appended as the conversation grows
//! (`session_start`, messages, per-turn usage, `session_end`), so
//! `iris sessions resume` can rehydrate it — even after a kill.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use iris_core::agent_loop::{self, LoopEvent, LoopOptions, LoopOutcome, Recorded, RunError};
use iris_core::config::{self, Config, ConfigInputs};
use iris_core::provider::OpenAiClient;
use iris_core::session;
use iris_core::session::schema::Event;
use iris_core::session::writer::{Manifest, TranscriptWriter};
use iris_core::tools::{
    ApplyPatchTool, DenyOnAsk, ListTool, ReadTool, RunCommandTool, ToolContext, ToolRegistry,
    WriteTool,
};
use iris_core::types::{Message, Role};

/// The model finished on its own.
pub const EXIT_COMPLETED: i32 = 0;
/// Configuration, key, workdir, or provider failure.
pub const EXIT_ERROR: i32 = 1;
/// `max_turns` was reached while the model still wanted tools.
pub const EXIT_MAX_TURNS: i32 = 3;
/// Cumulative usage passed `max_tokens_budget`.
pub const EXIT_BUDGET: i32 = 4;

const SYSTEM_PROMPT: &str = "You are iris, a terminal-based AI coding agent. Use the \
                             provided tools to inspect and modify files in the working \
                             directory; answer in plain text when the task is done.";

/// Everything a session run needs after the fail-fast checks — shared
/// by `run` and `sessions resume` (stage 2.3).
pub(crate) struct Prepared {
    config: Config,
    api_key: String,
    workdir: PathBuf,
    registry: ToolRegistry,
}

/// Run one prompt to a stopping condition. Returns the process exit code.
///
/// The conversation is persisted as it happens: `session_start`, the
/// system and user messages, then every loop event the recorder hands
/// back — printed to stderr as `[iris] session <id>` so a caller can
/// pick the run up again with `sessions resume <id>`.
pub fn run(
    prompt: &str,
    model: &str,
    workdir: &Path,
    base_url_flag: Option<&str>,
    env: &HashMap<String, String>,
) -> i32 {
    let prepared = match prepare(workdir, base_url_flag, env) {
        Ok(prepared) => prepared,
        Err(message) => {
            eprintln!("[iris] error: {message}");
            return EXIT_ERROR;
        }
    };
    let root = match session::store::sessions_root(env) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("[iris] error: {err}");
            return EXIT_ERROR;
        }
    };

    // Open the store *before* the loop: a session that cannot be
    // created must fail before any provider traffic, never halfway in.
    let manifest = Manifest::new(
        session::id::new_session_id(),
        prepared.workdir.to_string_lossy().into_owned(),
        model,
    );
    let mut writer = match TranscriptWriter::create(&root, &manifest) {
        Ok(writer) => writer,
        Err(err) => {
            eprintln!("[iris] error: cannot create session: {err}");
            return EXIT_ERROR;
        }
    };
    if let Err(err) = writer.append(&manifest.start_event()) {
        eprintln!("[iris] error: cannot record session start: {err}");
        return EXIT_ERROR;
    }
    let messages = vec![
        Message::text(Role::System, SYSTEM_PROMPT),
        Message::text(Role::User, prompt),
    ];
    for message in &messages {
        if let Err(err) = writer.append(&Event::Message(message.clone())) {
            eprintln!("[iris] error: cannot record session: {err}");
            return EXIT_ERROR;
        }
    }
    eprintln!("[iris] session {}", manifest.session_id);

    execute(prepared, model, messages, writer)
}

/// The checks every session run shares, all before the store or the
/// provider is touched: config layering, key (env only, D6), workdir,
/// tool registry.
pub(crate) fn prepare(
    workdir: &Path,
    base_url_flag: Option<&str>,
    env: &HashMap<String, String>,
) -> Result<Prepared, String> {
    let file_text = read_config_file(env)?;
    let config = config::load_config(&ConfigInputs {
        file_toml: file_text.as_deref(),
        env,
        flag_base_url: base_url_flag,
    })
    .map_err(|err| err.to_string())?;
    let api_key =
        config::resolve_api_key(env).ok_or("OPENROUTER_API_KEY is not set (see `iris doctor`)")?;
    let workdir = std::fs::canonicalize(workdir)
        .map_err(|_| format!("workdir does not exist: {}", workdir.display()))?;

    let mut registry = ToolRegistry::new();
    registry.register(ReadTool);
    registry.register(WriteTool);
    registry.register(ListTool);
    registry.register(RunCommandTool);
    registry.register(ApplyPatchTool);

    Ok(Prepared {
        config,
        api_key,
        workdir,
        registry,
    })
}

/// Run the loop for a prepared session: stream to stdout/stderr,
/// record every conversation event to `writer` as it happens, and
/// close the transcript with `session_end` when — and only when — the
/// run stops on its own terms (a crash or kill leaves no clean ending,
/// which is exactly how a reader tells an interrupted run apart).
/// Returns the process exit code.
pub(crate) fn execute(
    prepared: Prepared,
    model: &str,
    messages: Vec<Message>,
    mut writer: TranscriptWriter,
) -> i32 {
    let Prepared {
        config,
        api_key,
        workdir,
        registry,
    } = prepared;
    let ctx = ToolContext::with_gate(workdir, config.permission_gate());
    let client = OpenAiClient::new(&config.provider_base_url, api_key);
    let opts = LoopOptions::from_config(&config, model);

    // --- run; the observer streams as events happen ----------------------
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut saw_text = false;
    let mut ends_with_newline = true;
    let outcome = rt.block_on(agent_loop::run_recorded(
        &client,
        &registry,
        &ctx,
        &DenyOnAsk,
        &opts,
        messages,
        |event| match event {
            LoopEvent::TextDelta(text) => {
                saw_text = true;
                ends_with_newline = text.ends_with('\n') || (text.is_empty() && ends_with_newline);
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
            }
            LoopEvent::ToolStart { id, name } => {
                eprintln!("[iris] running {name} ({id})");
            }
            LoopEvent::ToolEnd { id, name, result } => {
                let preview = first_line(&result.content, 120);
                if result.is_error {
                    eprintln!("[iris] {name} ({id}) failed: {preview}");
                } else {
                    eprintln!("[iris] {name} ({id}) ok: {preview}");
                }
            }
        },
        |recorded| {
            let appended = match recorded {
                Recorded::Message(message) => writer.append(&Event::Message(message)),
                Recorded::Usage { turn, usage } => writer.append(&Event::Usage { turn, usage }),
            };
            Ok(appended?)
        },
    ));
    if saw_text && !ends_with_newline {
        println!();
    }

    match outcome {
        Ok(outcome) => {
            let (code, summary) = match &outcome {
                LoopOutcome::Completed { turns, usage, .. } => (
                    EXIT_COMPLETED,
                    format!(
                        "[iris] completed in {turns} turns ({} tokens)",
                        usage.total_tokens
                    ),
                ),
                LoopOutcome::MaxTurns { turns, usage } => (
                    EXIT_MAX_TURNS,
                    format!(
                        "[iris] stopped: max_turns ({turns}) reached after {} tokens",
                        usage.total_tokens
                    ),
                ),
                LoopOutcome::BudgetExceeded { turns, usage } => (
                    EXIT_BUDGET,
                    format!(
                        "[iris] stopped: token budget exceeded after {turns} turns ({} tokens)",
                        usage.total_tokens
                    ),
                ),
            };
            let (turns, usage) = match &outcome {
                LoopOutcome::Completed { turns, usage, .. }
                | LoopOutcome::MaxTurns { turns, usage }
                | LoopOutcome::BudgetExceeded { turns, usage } => (*turns, usage.clone()),
            };
            eprintln!("{summary}");
            if let Err(err) = writer.append(&Event::session_end(outcome.into(), turns, usage)) {
                eprintln!("[iris] error: cannot record session end: {err}");
                return EXIT_ERROR;
            }
            code
        }
        Err(RunError::Provider(err)) => {
            eprintln!("[iris] error: {err}");
            EXIT_ERROR
        }
        Err(RunError::Transcript(err)) => {
            eprintln!("[iris] error: transcript write failed: {err}");
            EXIT_ERROR
        }
    }
}

/// The config file layer: `$IRIS_CONFIG` → XDG → HOME, `None` when
/// absent (a missing file is not an error — defaults apply).
fn read_config_file(env: &HashMap<String, String>) -> Result<Option<String>, String> {
    let Some(path) = config::config_file_path(env) else {
        return Ok(None);
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("cannot read {}: {err}", path.display())),
    }
}

/// First line of tool output, truncated — stderr is a progress log, not
/// a data channel (full results go back to the model via the loop).
fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("");
    let mut preview: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_takes_one_line_untruncated() {
        assert_eq!(first_line("hello\nworld", 120), "hello");
    }

    #[test]
    fn first_line_truncates_long_lines_with_an_ellipsis() {
        let long = "x".repeat(200);
        let preview = first_line(&long, 120);
        assert_eq!(preview.chars().count(), 121);
        assert!(preview.ends_with('…'));
    }

    #[test]
    fn first_line_of_empty_content_is_empty() {
        assert_eq!(first_line("", 120), "");
    }
}
