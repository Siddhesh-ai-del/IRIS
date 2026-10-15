//! The TUI application state machine (stage 4.1).
//!
//! Pure, synchronous, and fully unit-testable: it owns no terminal and
//! no channels — [`crate::term`] feeds it loop events and key presses;
//! [`crate::ui`] renders an `&App` snapshot.
//!
//! The three states the plan names, and what they mean here:
//!
//! - [`AppState::Idle`] — no run in flight.
//! - [`AppState::Streaming`] — a run is in flight. The skeleton makes
//!   no finer distinction between token streaming and tool execution;
//!   4.3/4.4 will if they need to.
//! - [`AppState::AwaitingPermission`] — the loop is blocked on a tool
//!   permission. The interactive resolver lands in stage 4.5; the
//!   state and its transitions exist (and are tested) now, so 4.5 only
//!   has to plug a resolver in.

use iris_core::metrics::TurnMetrics;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::events::AppEvent;

/// Which of the three run states the app is in (plan 4.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppState {
    /// No run in flight.
    Idle,
    /// A run is in flight (tokens or tools).
    Streaming,
    /// The loop is blocked on a tool permission (stage 4.5 resolves it).
    AwaitingPermission,
}

impl AppState {
    /// The label the status line renders.
    pub fn label(self) -> &'static str {
        match self {
            AppState::Idle => "Idle",
            AppState::Streaming => "Streaming",
            AppState::AwaitingPermission => "AwaitingPermission",
        }
    }
}

/// A permission the loop is waiting on (stage 4.5 renders a prompt
/// from this; the skeleton shows it in the status line).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPermission {
    /// Tool-call id — the answer must reference it.
    pub id: String,
    /// Tool name, e.g. `run_command`.
    pub name: String,
    /// What the tool would do (command / diff preview, 4.5).
    pub detail: String,
}

/// The whole UI-facing state of a chat session.
#[derive(Clone, Debug, PartialEq)]
pub struct App {
    state: AppState,
    /// Raw assistant text accumulated so far (stage 4.3 swaps the
    /// pane to markdown; the buffer stays the source).
    transcript: String,
    /// Last status line (tool lifecycle, run outcome, errors).
    status: String,
    /// The minimal single-line composer (stage 4.2 grows it).
    prompt: String,
    /// A loop task is in flight.
    running: bool,
    /// The user asked to quit.
    quit: bool,
    /// Set while [`AppState::AwaitingPermission`].
    pending: Option<PendingPermission>,
    /// Last turn's metrics (stage 4.4's status bar consumes these).
    last_metrics: Option<TurnMetrics>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// A fresh, idle app.
    pub fn new() -> Self {
        Self {
            state: AppState::Idle,
            transcript: String::new(),
            status: String::new(),
            prompt: String::new(),
            running: false,
            quit: false,
            pending: None,
            last_metrics: None,
        }
    }

    /// The current state.
    pub fn state(&self) -> AppState {
        self.state
    }

    /// The accumulated assistant text.
    pub fn transcript(&self) -> &str {
        &self.transcript
    }

    /// The last status line.
    pub fn status(&self) -> &str {
        &self.status
    }

    /// The composer's current content.
    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    /// Whether a loop task is in flight.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether the event loop should exit.
    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The permission being awaited, if any.
    pub fn pending(&self) -> Option<&PendingPermission> {
        self.pending.as_ref()
    }

    /// The most recent turn's metrics.
    pub fn last_metrics(&self) -> Option<&TurnMetrics> {
        self.last_metrics.as_ref()
    }

    /// Apply one loop/lifecycle event — the state machine itself.
    ///
    /// The loop is a single FIFO producer, so events arrive in the
    /// order the table below assumes: a response's deltas and metrics
    /// precede its tool dispatch (and any permission ask).
    pub fn handle(&mut self, event: AppEvent) {
        match event {
            AppEvent::Delta(text) => {
                self.transcript.push_str(&text);
                self.state = AppState::Streaming;
            }
            AppEvent::ToolStart { id, name } => {
                self.state = AppState::Streaming;
                self.status = format!("running {name} ({id})");
            }
            AppEvent::ToolEnd { id, name, ok, .. } => {
                self.state = AppState::Streaming;
                self.status = format!("{name} ({id}) {}", if ok { "ok" } else { "failed" });
            }
            AppEvent::CacheBust {
                system_changed,
                tools_changed,
            } => {
                let what = match (system_changed, tools_changed) {
                    (true, true) => "system + tools",
                    (true, false) => "system prompt",
                    (false, true) => "tool schemas",
                    // Never emitted: a bust always changes something.
                    (false, false) => "prefix",
                };
                self.status = format!("cache-bust: {what} changed");
            }
            AppEvent::Metrics(metrics) => {
                self.last_metrics = Some(metrics);
                self.state = AppState::Streaming;
            }
            AppEvent::PermissionRequested { id, name, detail } => {
                self.state = AppState::AwaitingPermission;
                self.pending = Some(PendingPermission { id, name, detail });
            }
            AppEvent::PermissionAnswered => {
                self.pending = None;
                self.state = AppState::Streaming;
            }
            AppEvent::RunEnded { summary } => {
                self.state = AppState::Idle;
                self.running = false;
                self.status = summary;
            }
            AppEvent::Error { message } => {
                self.state = AppState::Idle;
                self.running = false;
                self.status = message;
            }
        }
    }

    /// Apply one key press. Returns `Some(prompt)` when Enter submits
    /// a new run request (the shell starts the loop with it).
    ///
    /// Keys while a run is in flight are dropped except Ctrl-C — the
    /// queued/queued-aware composer is stage 4.2.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        if self.running {
            return None;
        }
        match key.code {
            KeyCode::Esc if self.prompt.is_empty() && self.state == AppState::Idle => {
                self.quit = true;
            }
            KeyCode::Esc => self.prompt.clear(),
            KeyCode::Backspace => {
                self.prompt.pop();
            }
            KeyCode::Char(c) => self.prompt.push(c),
            KeyCode::Enter if !self.prompt.trim().is_empty() => {
                let prompt = std::mem::take(&mut self.prompt);
                self.state = AppState::Streaming;
                self.running = true;
                return Some(prompt);
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn starts_idle_with_an_empty_composer() {
        let app = App::new();
        assert_eq!(app.state(), AppState::Idle);
        assert_eq!(app.transcript(), "");
        assert_eq!(app.prompt(), "");
        assert!(!app.is_running());
        assert!(!app.should_quit());
        assert_eq!(app.pending(), None);
        assert_eq!(app.last_metrics(), None);
    }

    #[test]
    fn deltas_accumulate_in_the_transcript_and_enter_the_run_state() {
        let mut app = App::new();
        app.handle(AppEvent::Delta("Hel".into()));
        app.handle(AppEvent::Delta("lo, ".into()));
        app.handle(AppEvent::Delta("world".into()));
        assert_eq!(app.transcript(), "Hello, world");
        assert_eq!(app.state(), AppState::Streaming);
    }

    #[test]
    fn tool_lifecycle_reports_in_the_status_without_ending_the_run() {
        let mut app = App::new();
        app.handle(AppEvent::ToolStart {
            id: "c1".into(),
            name: "read_file".into(),
        });
        assert_eq!(app.state(), AppState::Streaming);
        assert_eq!(app.status(), "running read_file (c1)");

        app.handle(AppEvent::ToolEnd {
            id: "c1".into(),
            name: "read_file".into(),
            ok: true,
            content: "[package]".into(),
        });
        assert_eq!(app.state(), AppState::Streaming, "the run continues");
        assert_eq!(app.status(), "read_file (c1) ok");
    }

    #[test]
    fn run_end_and_error_both_return_to_idle() {
        let mut app = App::new();
        type_text(&mut app, "hi");
        assert!(app.handle_key(key(KeyCode::Enter)).is_some());
        assert!(app.is_running());

        app.handle(AppEvent::RunEnded {
            summary: "completed in 1 turns (9 tokens)".into(),
        });
        assert_eq!(app.state(), AppState::Idle);
        assert!(!app.is_running());
        assert_eq!(app.status(), "completed in 1 turns (9 tokens)");

        // Same recovery after a mid-run failure.
        type_text(&mut app, "again");
        assert!(app.handle_key(key(KeyCode::Enter)).is_some());
        app.handle(AppEvent::Error {
            message: "HTTP 500".into(),
        });
        assert_eq!(app.state(), AppState::Idle);
        assert!(!app.is_running());
        assert_eq!(app.status(), "HTTP 500");
    }

    #[test]
    fn permission_events_enter_and_leave_awaiting() {
        let mut app = App::new();
        app.handle(AppEvent::PermissionRequested {
            id: "c7".into(),
            name: "run_command".into(),
            detail: "rm -rf target".into(),
        });
        assert_eq!(app.state(), AppState::AwaitingPermission);
        assert_eq!(
            app.pending(),
            Some(&PendingPermission {
                id: "c7".into(),
                name: "run_command".into(),
                detail: "rm -rf target".into(),
            })
        );

        app.handle(AppEvent::PermissionAnswered);
        assert_eq!(app.state(), AppState::Streaming);
        assert_eq!(app.pending(), None);
    }

    #[test]
    fn composer_keys_edit_the_prompt_and_enter_submits_once() {
        let mut app = App::new();
        type_text(&mut app, "fix the build");
        assert_eq!(app.prompt(), "fix the build");

        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(app.prompt(), "fix the buil");

        let submitted = app.handle_key(key(KeyCode::Enter));
        assert_eq!(submitted, Some("fix the buil".into()));
        assert_eq!(app.prompt(), "", "the composer clears on submit");
        assert!(app.is_running());
        assert_eq!(app.state(), AppState::Streaming);

        // While a run is in flight the composer stays closed.
        type_text(&mut app, "queued?");
        assert_eq!(app.prompt(), "", "no submission mid-run (4.2 may queue)");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);

        // Esc quits only when idle with an empty composer; Ctrl-C
        // always quits.
        let mut fresh = App::new();
        fresh.handle_key(key(KeyCode::Esc));
        assert!(fresh.should_quit());

        let mut mid = App::new();
        type_text(&mut mid, "draft");
        mid.handle_key(key(KeyCode::Esc));
        assert!(!mid.should_quit(), "Esc first clears the draft");
        assert_eq!(mid.prompt(), "");
        mid.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(mid.should_quit(), "Ctrl-C quits from anywhere");
    }

    #[test]
    fn metrics_and_cache_bust_update_side_state() {
        let mut app = App::new();
        let metrics = TurnMetrics {
            turn: 3,
            ttft_ms: Some(120),
            tokens_per_sec: 41.0,
            cache_hit_pct: Some(93.5),
            cost_usd: Some(0.0011),
        };
        app.handle(AppEvent::Metrics(metrics.clone()));
        assert_eq!(app.last_metrics(), Some(&metrics), "stage 4.4 reads this");
        assert_eq!(app.state(), AppState::Streaming);

        app.handle(AppEvent::CacheBust {
            system_changed: true,
            tools_changed: false,
        });
        assert_eq!(app.status(), "cache-bust: system prompt changed");
    }
}
