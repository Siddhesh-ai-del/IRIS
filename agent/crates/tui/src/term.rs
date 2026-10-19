//! The terminal shell (stage 4.1) — the only module that touches I/O.
//!
//! Responsibilities, deliberately kept thin so the rest of the crate
//! never needs a terminal:
//!
//! - crossterm setup/teardown (with a restore guard, so every exit
//!   path — including a panic while drawing — returns the terminal).
//! - A background tokio task per run: the core loop streams its
//!   events onto an owned channel ([`AppEvent`]) and records to the
//!   shared transcript writer. A permission `ask` posts
//!   [`AppEvent::PermissionRequested`] and parks the run task on the
//!   answer channel — the frame cycle never blocks (stage 4.5).
//! - The frame cycle: **drain** loop events (`try_recv`, never
//!   blocking), **poll** input with a timeout, **draw** from the `&App`
//!   snapshot. Nothing in the render path waits on the loop.
//!
//! Known 4.x follow-ups (honest gaps of the skeleton):
//!
//! - Quitting mid-run cancels the task — the transcript then simply
//!   has no `session_end`, which is exactly how resume (2.3) detects
//!   and repairs interrupted runs.
//! - Continuing after `max_turns` can leave a dangling `tool_use` in
//!   the in-memory history; resume's repair covers it only for
//!   transcript-sourced histories.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iris_core::agent_loop::{self, LoopOutcome, Recorded, RunError};
use iris_core::provider::Provider;
use iris_core::session::schema::Event as TranscriptEvent;
use iris_core::session::writer::TranscriptWriter;
use iris_core::tools::{AskResolver, ToolContext, ToolRegistry};
use iris_core::types::{Message, Role};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::app::{App, PermissionAnswer};
use crate::ask::PromptingResolver;
use crate::events::{self, AppEvent};
use crate::ui;

/// How long the frame cycle waits for input before draining the loop
/// channel again — the skeleton's frame pacing.
const TICK: Duration = Duration::from_millis(50);

/// Everything the chat shell runs with, prepared by the CLI (config,
/// key, workdir and registry all stay on that side of the boundary).
pub struct Chat<P: Provider + Send + Sync + 'static> {
    /// The provider every run streams from.
    pub provider: Arc<P>,
    /// The tool registry every run dispatches through.
    pub registry: Arc<ToolRegistry>,
    /// Workspace + permission gate for tool dispatch.
    pub ctx: ToolContext,
    /// Loop knobs (model, limits, price-table entry).
    pub opts: agent_loop::LoopOptions,
    /// The session transcript — appends are shared across runs.
    pub writer: TranscriptWriter,
    /// The system message every run starts from (already recorded in
    /// the transcript by the CLI).
    pub system: Message,
    /// Where an `always-allow (rule)` answer writes its
    /// `[permissions]` entry back (stage 4.5); `None` when no config
    /// file path resolves (rule degrades to session scope).
    pub config_path: Option<PathBuf>,
}

/// Run the interactive chat until the user quits. Restores the
/// terminal on every exit path.
pub fn run<P>(chat: Chat<P>) -> Result<(), String>
where
    P: Provider + Send + Sync + 'static,
{
    enable_raw_mode().map_err(|err| format!("cannot enter raw mode: {err}"))?;
    let _restore = RestoreGuard;
    // Bracketed paste makes terminals deliver pastes as one `Event::
    // Paste` instead of a storm of key events (4.2).
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste
    )
    .map_err(|err| format!("cannot enter alternate screen: {err}"))?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal =
        Terminal::new(backend).map_err(|err| format!("terminal init failed: {err}"))?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|err| format!("cannot start runtime: {err}"))?;

    let (tx, rx) = mpsc::channel::<AppEvent>();
    // The stage-4.5 answer channel: the resolver (loop side) blocks
    // on its receiver; spin() forwards the user's prompt keys into it.
    let (answer_tx, answer_rx) = mpsc::channel::<PermissionAnswer>();
    // One resolver per session, so `always-allow (session)` rules
    // outlive individual runs (they live inside the resolver).
    let ask: Arc<dyn AskResolver> = Arc::new(PromptingResolver::new(
        tx.clone(),
        answer_rx,
        chat.config_path.clone(),
    ));
    // The gauges' model / turn cap / token budget come from the run's
    // loop options (4.4).
    let app = App::with_run_config(
        &chat.opts.model,
        chat.opts.max_turns,
        chat.opts.max_tokens_budget,
    );
    spin(chat, app, rx, tx, answer_tx, ask, &rt, &mut terminal)
}

/// The frame cycle: drain → draw → poll input. The only loop that
/// owns the terminal.
#[allow(clippy::too_many_arguments)] // one natural dependency set, not a growing bag
fn spin<P>(
    chat: Chat<P>,
    mut app: App,
    rx: Receiver<AppEvent>,
    tx: Sender<AppEvent>,
    answer_tx: Sender<PermissionAnswer>,
    ask: Arc<dyn AskResolver>,
    rt: &tokio::runtime::Runtime,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> Result<(), String>
where
    P: Provider + Send + Sync + 'static,
{
    let Chat {
        provider,
        registry,
        ctx,
        opts,
        writer,
        system,
        config_path: _,
    } = chat;
    let writer = Arc::new(Mutex::new(writer));
    let mut history = vec![system];
    let mut run_text = String::new();

    while !app.should_quit() {
        // Drain everything the loop has produced — non-blocking, so a
        // busy stream never stalls input or drawing.
        while let Ok(event) = rx.try_recv() {
            match &event {
                AppEvent::Delta(text) => run_text.push_str(text),
                AppEvent::RunEnded { .. } => {
                    let text = std::mem::take(&mut run_text);
                    history.push(Message::text(Role::Assistant, text));
                }
                AppEvent::Error { .. } => {
                    // Keep whatever streamed before the failure: the
                    // user saw it, so the model must see it too.
                    let text = std::mem::take(&mut run_text);
                    if !text.is_empty() {
                        history.push(Message::text(Role::Assistant, text));
                    }
                }
                _ => {}
            }
            app.handle(event);
        }

        terminal
            .draw(|frame| ui::render(frame, &app))
            .map_err(|err| format!("draw failed: {err}"))?;

        if !event::poll(TICK).map_err(|err| format!("input poll failed: {err}"))? {
            continue;
        }
        match event::read().map_err(|err| format!("input read failed: {err}"))? {
            // While a permission prompt is open the composer stays
            // closed — pastes would leak into a draft the user cannot
            // see or send (4.5).
            Event::Paste(text) if app.pending().is_none() => app.handle_paste(&text),
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let prompt = app.handle_key(key);
                // A permission answer goes straight to the blocked
                // resolver — the loop task resumes on it.
                if let Some(answer) = app.take_answer() {
                    let _ = answer_tx.send(answer);
                }
                let Some(prompt) = prompt else {
                    continue;
                };

                // Record the user turn first (transcript is the resume
                // primitive), then start the run in the background.
                let user = Message::text(Role::User, prompt);
                let recorded = writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .append(&TranscriptEvent::Message(user.clone()));
                if let Err(err) = recorded {
                    app.handle(AppEvent::Error {
                        message: format!("transcript write failed: {err}"),
                    });
                    continue;
                }
                history.push(user);
                run_text.clear();
                spawn_run(
                    rt,
                    &provider,
                    &registry,
                    &ctx,
                    &opts,
                    history.clone(),
                    tx.clone(),
                    Arc::clone(&writer),
                    Arc::clone(&ask),
                );
            }
            _ => {} // resize etc. — the next draw covers it
        }
    }
    Ok(())
}

/// Start one loop run in the background; its events (plus a final
/// `RunEnded` or `Error`) arrive on `tx`.
#[allow(clippy::too_many_arguments)] // one natural dependency set, not a growing bag
fn spawn_run<P>(
    rt: &tokio::runtime::Runtime,
    provider: &Arc<P>,
    registry: &Arc<ToolRegistry>,
    ctx: &ToolContext,
    opts: &agent_loop::LoopOptions,
    messages: Vec<Message>,
    tx: Sender<AppEvent>,
    writer: Arc<Mutex<TranscriptWriter>>,
    ask: Arc<dyn AskResolver>,
) where
    P: Provider + Send + Sync + 'static,
{
    let provider = Arc::clone(provider);
    let registry = Arc::clone(registry);
    let ctx = ctx.clone();
    let opts = opts.clone();
    rt.spawn(async move {
        let outcome = agent_loop::run_recorded(
            &*provider,
            &registry,
            &ctx,
            &*ask,
            &opts,
            messages,
            |event| {
                let _ = tx.send(AppEvent::from(event));
            },
            move |recorded| {
                let mut writer = writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                writer
                    .append(&transcript_event(recorded))
                    .map_err(RunError::Transcript)
            },
        )
        .await;
        let final_event = match outcome {
            Ok(outcome) => {
                let summary = events::summarize_outcome(&outcome);
                let usage = match &outcome {
                    LoopOutcome::Completed { usage, .. }
                    | LoopOutcome::MaxTurns { usage, .. }
                    | LoopOutcome::BudgetExceeded { usage, .. } => usage.clone(),
                };
                AppEvent::RunEnded { summary, usage }
            }
            Err(err) => AppEvent::Error {
                message: err.to_string(),
            },
        };
        let _ = tx.send(final_event);
    });
}

/// What one loop record becomes in the transcript — total over
/// [`Recorded`], so a new variant can never be silently dropped.
fn transcript_event(recorded: Recorded) -> TranscriptEvent {
    match recorded {
        Recorded::Message(message) => TranscriptEvent::Message(message),
        Recorded::Usage {
            turn,
            usage,
            input_token_delta,
        } => TranscriptEvent::Usage {
            turn,
            usage,
            input_token_delta,
        },
        Recorded::Metrics(metrics) => TranscriptEvent::TurnMetrics(metrics),
    }
}

/// Restore the terminal however we leave (raw mode off, main screen).
struct RestoreGuard;

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            LeaveAlternateScreen,
            DisableBracketedPaste
        );
    }
}

#[cfg(test)]
mod tests {
    use iris_core::types::Usage;

    use super::*;

    #[test]
    fn the_tick_is_short_enough_to_feel_live() {
        assert!(
            TICK < Duration::from_millis(100),
            "a frame wait longer than 100ms reads as a hang"
        );
    }

    #[test]
    fn restore_guard_drops_without_panicking() {
        // Dropping in a process that never entered raw mode must be a
        // no-op, not a panic on an exit path we cannot test without a
        // pty.
        let guard = RestoreGuard;
        drop(guard);
    }

    #[test]
    fn transcript_event_is_total_over_every_recorded_kind() {
        // Session persistence must never silently lose a record type.
        assert!(matches!(
            transcript_event(Recorded::Message(Message::text(Role::User, "hi"))),
            TranscriptEvent::Message(_)
        ));
        assert!(matches!(
            transcript_event(Recorded::Usage {
                turn: 1,
                usage: Usage::default(),
                input_token_delta: Some(2),
            }),
            TranscriptEvent::Usage {
                turn: 1,
                input_token_delta: Some(2),
                ..
            }
        ));
        let metrics = iris_core::metrics::TurnMetrics {
            turn: 2,
            ttft_ms: None,
            tokens_per_sec: 0.0,
            cache_hit_pct: None,
            cost_usd: None,
        };
        assert!(matches!(
            transcript_event(Recorded::Metrics(metrics.clone())),
            TranscriptEvent::TurnMetrics(m) if m == metrics
        ));
    }
}
