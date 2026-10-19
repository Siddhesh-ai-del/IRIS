//! The TUI application state machine.
//!
//! Pure, synchronous, and fully unit-testable: it owns no terminal and
//! no channels — [`crate::term`] feeds it loop events and key presses;
//! [`crate::ui`] renders an `&App` snapshot.
//!
//! Composer (stage 4.2): a `ratatui-textarea` multiline draft with
//! history recall (Up/Down at the composer's edges), locally
//! dispatched [`COMMANDS`] (`/help` `/quit` `/clear` — never sent to
//! the model), bracketed-paste insertion, and a scrollback distance
//! ([`PAGE_LINES`] per page) the transcript pane pauses while the
//! user is reading (4.3).
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
use ratatui_textarea::TextArea;

use crate::diff::EditDiff;
use crate::events::AppEvent;

/// How many lines one scrollback page moves ([`App::page_up`] /
/// [`App::page_down`]).
pub const PAGE_LINES: u16 = 10;

/// The `/`-commands the composer dispatches locally (stage 4.2).
/// Further commands (e.g. `/model`) join when their features do.
pub const COMMANDS: [(&str, &str); 3] = [
    ("/help", "list these commands"),
    ("/quit", "exit iris"),
    ("/clear", "clear the screen (the session keeps recording)"),
];

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

/// A permission the loop is waiting on (stage 4.5 renders the popup
/// prompt from this; the status line keeps a one-line summary).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingPermission {
    /// Tool-call id — the answer must reference it.
    pub id: String,
    /// Tool name, e.g. `run_command`.
    pub name: String,
    /// What the tool would do (command / diff preview, 4.5).
    pub detail: String,
}

/// What the user chose at the permission prompt (stages 4.5/4.6) —
/// stored by [`App::handle_key`], taken once by the shell via
/// [`App::take_answer`] and forwarded to the loop's blocked resolver.
/// (`AllowWith` carries data, so this is clone-only, not `Copy`.)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PermissionAnswer {
    /// Run this call only.
    Allow,
    /// Refuse this call only.
    Deny,
    /// Allow every later ask for this tool in this session (in-memory
    /// rule — the resolver's session map).
    AllowSession,
    /// Allow and persist `[permissions].<tool> = "allow"` back to the
    /// config file, so future sessions allow it too.
    AllowRule,
    /// Allow with a **replaced input** (stage 4.6): the user accepted
    /// only part of a file edit, and the loop runs the call with
    /// `input` instead of the model's original JSON. A partial choice
    /// is deliberately never remembered (session or config).
    AllowWith { input: String, note: String },
}

/// The whole UI-facing state of a chat session.
#[derive(Clone, Debug)]
pub struct App {
    state: AppState,
    /// Raw assistant text accumulated so far (stage 4.3 swaps the
    /// pane to markdown; the buffer stays the source).
    transcript: String,
    /// Last status line (tool lifecycle, run outcome, errors).
    status: String,
    /// The multiline composer (stage 4.2, `ratatui-textarea`).
    composer: TextArea<'static>,
    /// Previously submitted prompts, oldest first (4.2).
    history: Vec<String>,
    /// Position in [`App::history`] while browsing; `None` = editing
    /// a fresh draft (4.2).
    history_cursor: Option<usize>,
    /// The unsent text saved when history browsing starts (4.2).
    draft: String,
    /// Transcript scrollback distance from the tail, in lines — 0
    /// follows new output (4.2).
    scroll_from_bottom: u16,
    /// Gauge identity (stage 4.4): the model id every run uses, its
    /// turn cap, and its per-run token budget.
    model: String,
    max_turns: u32,
    max_tokens_budget: u64,
    /// Cumulative session cost in USD (4.4) — summed over reported
    /// turn costs; unreported costs add nothing (never a fake zero).
    session_cost: f64,
    /// The last finished run's `total_tokens` (4.4) — what the budget
    /// gauge compares against [`App::max_tokens_budget`].
    run_tokens: u64,
    /// The user's answer to the open permission prompt, waiting to be
    /// forwarded to the resolver (4.5).
    answer: Option<PermissionAnswer>,
    /// An answer was sent but the resolver has not acknowledged with
    /// [`AppEvent::PermissionAnswered`] yet — the prompt keys stay
    /// swallowed so one press cannot be captured twice.
    answer_in_flight: bool,
    /// A loop task is in flight.
    running: bool,
    /// The user asked to quit.
    quit: bool,
    /// Set while [`AppState::AwaitingPermission`].
    pending: Option<PendingPermission>,
    /// The hunk view of the open ask (stage 4.6) — `Some` for file
    /// edits with something to show; `None` for every other ask.
    pending_diff: Option<EditDiff>,
    /// Which hunk the popup's cursor is on ([`App::handle_key`]'s
    /// j/k/space move and toggle it, stage 4.6).
    hunk_cursor: usize,
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
            composer: TextArea::default(),
            history: Vec::new(),
            history_cursor: None,
            draft: String::new(),
            scroll_from_bottom: 0,
            model: String::new(),
            max_turns: 0,
            max_tokens_budget: 0,
            session_cost: 0.0,
            run_tokens: 0,
            answer: None,
            answer_in_flight: false,
            running: false,
            quit: false,
            pending: None,
            pending_diff: None,
            hunk_cursor: 0,
            last_metrics: None,
        }
    }

    /// A fresh app whose gauges know the run's identity (model, turn
    /// cap, token budget — from `LoopOptions` at the shell, stage
    /// 4.4).
    pub fn with_run_config(
        model: impl Into<String>,
        max_turns: u32,
        max_tokens_budget: u64,
    ) -> Self {
        Self {
            model: model.into(),
            max_turns,
            max_tokens_budget,
            ..Self::new()
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

    /// The composer's current content (lines joined with `\n`).
    pub fn prompt(&self) -> String {
        self.composer.lines().join("\n")
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

    /// The open ask's hunk view (stage 4.6), when it is a file edit
    /// with hunks to show.
    pub fn pending_diff(&self) -> Option<&EditDiff> {
        self.pending_diff.as_ref()
    }

    /// Which hunk the popup's cursor is on (stage 4.6).
    pub fn hunk_cursor(&self) -> usize {
        self.hunk_cursor
    }

    /// Take the pending permission answer, if the user has given one
    /// (the shell forwards it to the blocked resolver — exactly once).
    pub fn take_answer(&mut self) -> Option<PermissionAnswer> {
        // RED stub: handle_key does not capture answers yet (4.5).
        self.answer.take()
    }

    /// The most recent turn's metrics.
    pub fn last_metrics(&self) -> Option<&TurnMetrics> {
        self.last_metrics.as_ref()
    }

    /// The model id every run uses (stage 4.4).
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The per-run turn cap.
    pub fn max_turns(&self) -> u32 {
        self.max_turns
    }

    /// The per-run token budget the bar measures against.
    pub fn max_tokens_budget(&self) -> u64 {
        self.max_tokens_budget
    }

    /// Cumulative session cost in USD (only reported costs count).
    pub fn session_cost(&self) -> f64 {
        self.session_cost
    }

    /// The last finished run's total tokens.
    pub fn run_tokens(&self) -> u64 {
        self.run_tokens
    }

    /// Whether the last run met or passed its token budget — the
    /// gauge bar's red state (stage 4.4).
    pub fn over_budget(&self) -> bool {
        self.max_tokens_budget > 0 && self.run_tokens >= self.max_tokens_budget
    }

    /// How far the transcript view is paged up from the tail, in
    /// lines — 0 means "following new output".
    pub fn scroll_from_bottom(&self) -> u16 {
        self.scroll_from_bottom
    }

    /// The composer widget's state, for [`crate::ui`]'s render pass.
    pub fn composer(&self) -> &TextArea<'static> {
        &self.composer
    }

    /// Page the transcript scrollback up (older output) by
    /// [`PAGE_LINES`]. Overshoot clamps at the head in the render
    /// pass, which knows the visible height.
    pub fn page_up(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(PAGE_LINES);
    }

    /// Page the transcript scrollback down by [`PAGE_LINES`],
    /// clamped at the tail.
    pub fn page_down(&mut self) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(PAGE_LINES);
    }

    /// Insert a bracketed paste verbatim into the composer — dropped
    /// while a run is in flight, like every other composer key.
    pub fn handle_paste(&mut self, text: &str) {
        if self.running {
            return;
        }
        self.composer.insert_str(text);
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
                // Auto-scroll (4.3): at the tail (`scroll_from_bottom`
                // == 0) the view follows new output by construction;
                // while the user is paged up, output must NOT yank
                // them back — the offset is left alone. Limitation:
                // the parked view still slides toward the tail as
                // output accumulates (a true pin needs the rendered
                // height the render pass owns); see the module docs.
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
                // Session-scope cost accumulates over reported turn
                // costs — an unreported cost adds nothing (4.4).
                if let Some(cost) = metrics.cost_usd {
                    self.session_cost += cost;
                }
                self.last_metrics = Some(metrics);
                self.state = AppState::Streaming;
            }
            AppEvent::PermissionRequested {
                id,
                name,
                detail,
                old,
            } => {
                self.state = AppState::AwaitingPermission;
                // File edits get a hunk view (stage 4.6); everything
                // else keeps the plain 4.5 prompt.
                self.pending_diff = crate::diff::build(&name, &detail, old.as_deref());
                self.hunk_cursor = 0;
                self.pending = Some(PendingPermission { id, name, detail });
            }
            AppEvent::PermissionAnswered { note } => {
                self.pending = None;
                self.pending_diff = None;
                self.hunk_cursor = 0;
                self.state = AppState::Streaming;
                self.answer_in_flight = false;
                if let Some(note) = note {
                    self.status = note;
                }
            }
            AppEvent::RunEnded { summary, usage } => {
                self.state = AppState::Idle;
                self.running = false;
                self.status = summary;
                // What the budget gauge compares against the run's
                // token budget (4.4).
                self.run_tokens = usage.total_tokens;
                // A dead run can never leave a modal prompt — or a
                // captured-but-unsent answer — behind (4.5), diff
                // view included (4.6).
                self.pending = None;
                self.pending_diff = None;
                self.hunk_cursor = 0;
                self.answer = None;
                self.answer_in_flight = false;
            }
            AppEvent::Error { message } => {
                self.state = AppState::Idle;
                self.running = false;
                self.status = message;
                // Same guarantee as RunEnded: the prompt closes with
                // the run that asked for it.
                self.pending = None;
                self.pending_diff = None;
                self.hunk_cursor = 0;
                self.answer = None;
                self.answer_in_flight = false;
            }
        }
    }

    /// The `y` answer for the open prompt when hunks were rejected
    /// (stage 4.6): `Some(PermissionAnswer::AllowWith { .. })` runs
    /// only the accepted parts; `None` means a plain allow is right
    /// (everything accepted, or no diff at all).
    fn partial_answer(&self) -> Option<PermissionAnswer> {
        let diff = self.pending_diff.as_ref()?;
        if diff.all_accepted() {
            return None;
        }
        let (accepted, total) = diff.accepted_counts();
        let input = diff.replacement_input()?;
        Some(PermissionAnswer::AllowWith {
            note: format!(
                "applied {accepted}/{total} hunks to {} — partial, not remembered",
                diff.path
            ),
            input,
        })
    }

    /// Apply one key press. Returns `Some(prompt)` when Enter submits
    /// a new run request (the shell starts the loop with it).
    ///
    /// Key map: while a permission prompt is open it is **modal** —
    /// `y` `n` `a` `r` answer it (stage 4.5), everything else is
    /// ignored; Ctrl-C still quits. Otherwise (stage 4.2): Enter
    /// sends; Shift+Enter or Ctrl-J break
    /// the line (unix terminals deliver Ctrl-J as Enter+CONTROL);
    /// Up/Down recall history at the composer's top/bottom row and
    /// move the cursor otherwise; PageUp/PageDown page the transcript
    /// scrollback (they work mid-run — reading output is the point);
    /// Esc clears the draft, then quits when idle; Ctrl-C always
    /// quits. Everything else is the textarea's editing keys.
    pub fn handle_key(&mut self, key: KeyEvent) -> Option<String> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return None;
        }
        // The permission prompt is modal (4.5): `y` `n` `a` `r` are
        // its answers (captured once — a second press before the
        // resolver acknowledges is swallowed), everything else is
        // ignored so the composer can never eat prompt keys. Ctrl-C
        // above still quits straight out of the prompt. With the 4.6
        // diff view open, j/k (or the arrows) move between hunks and
        // space toggles the current one.
        if self.pending.is_some() {
            if self.pending_diff.is_some() {
                let last = self
                    .pending_diff
                    .as_ref()
                    .map_or(0, |diff| diff.hunks.len().saturating_sub(1));
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        self.hunk_cursor = (self.hunk_cursor + 1).min(last);
                        return None;
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        self.hunk_cursor = self.hunk_cursor.saturating_sub(1);
                        return None;
                    }
                    KeyCode::Char(' ') => {
                        if let Some(diff) = &mut self.pending_diff {
                            diff.toggle(self.hunk_cursor);
                        }
                        return None;
                    }
                    _ => {}
                }
            }
            if !self.answer_in_flight {
                self.answer = match key.code {
                    // 4.6: `y` with rejected hunks means "run the
                    // accepted part only" — a replaced input, not the
                    // model's original call.
                    KeyCode::Char('y') => match self.partial_answer() {
                        Some(answer) => Some(answer),
                        None => Some(PermissionAnswer::Allow),
                    },
                    KeyCode::Char('n') => Some(PermissionAnswer::Deny),
                    KeyCode::Char('a') => Some(PermissionAnswer::AllowSession),
                    KeyCode::Char('r') => Some(PermissionAnswer::AllowRule),
                    _ => None,
                };
                self.answer_in_flight = self.answer.is_some();
            }
            return None;
        }
        match key.code {
            KeyCode::PageUp => {
                self.page_up();
                return None;
            }
            KeyCode::PageDown => {
                self.page_down();
                return None;
            }
            _ => {}
        }
        if self.running {
            return None;
        }
        let line_break = key.code == KeyCode::Enter
            && (key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL));
        match key.code {
            KeyCode::Esc if self.prompt().is_empty() && self.state == AppState::Idle => {
                self.quit = true;
            }
            KeyCode::Esc => {
                self.composer.clear();
            }
            KeyCode::Enter if line_break => self.composer.insert_newline(),
            KeyCode::Enter => return self.submit(),
            // History only claims Up/Down at the composer's edges;
            // inside a multiline draft they move the cursor.
            KeyCode::Up if self.composer.cursor().0 == 0 => self.history_prev(),
            KeyCode::Down if self.composer.cursor().0 + 1 == self.composer.lines().len() => {
                self.history_next()
            }
            _ => {
                self.composer.input(key);
            }
        }
        None
    }

    /// A non-empty Enter: record the prompt (it is what the transcript
    /// and resume will show), then either dispatch a `/command`
    /// locally or hand the run request to the shell.
    fn submit(&mut self) -> Option<String> {
        let prompt = self.prompt();
        let prompt = prompt.trim_end_matches('\n').to_string();
        if prompt.trim().is_empty() {
            return None;
        }
        self.history.push(prompt.clone());
        self.history_cursor = None;
        self.draft.clear();
        self.composer = TextArea::default();

        if prompt.starts_with('/') {
            self.dispatch_command(&prompt);
            return None;
        }
        self.state = AppState::Streaming;
        self.running = true;
        Some(prompt)
    }

    /// Slash commands run here, never in the loop — no tokens, no
    /// transcript write, no run state.
    fn dispatch_command(&mut self, text: &str) {
        match text {
            "/help" => {
                self.status = COMMANDS
                    .iter()
                    .map(|(name, help)| format!("{name} — {help}"))
                    .collect::<Vec<_>>()
                    .join(" · ");
            }
            "/quit" => self.quit = true,
            "/clear" => {
                self.transcript.clear();
                self.scroll_from_bottom = 0;
                self.status = "screen cleared".into();
            }
            other => self.status = format!("unknown command: {other} — try /help"),
        }
    }

    /// Recall the previous history entry, saving the unsent draft the
    /// first time (it comes back when the cursor leaves the end).
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            Some(0) => return, // already at the oldest
            Some(index) => index - 1,
            None => {
                self.draft = self.prompt();
                self.history.len() - 1
            }
        };
        self.history_cursor = Some(next);
        let text = self.history[next].clone();
        self.set_prompt(&text);
    }

    /// Recall the next history entry; past the newest, restore the
    /// saved draft.
    fn history_next(&mut self) {
        let Some(index) = self.history_cursor else {
            return; // not browsing
        };
        let next = index + 1;
        if next < self.history.len() {
            self.history_cursor = Some(next);
            let text = self.history[next].clone();
            self.set_prompt(&text);
        } else {
            // Past the newest entry: back to a fresh draft.
            self.history_cursor = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_prompt(&draft);
        }
    }

    /// Replace the composer's content (history recall).
    fn set_prompt(&mut self, text: &str) {
        self.composer = TextArea::new(text.split('\n').map(String::from).collect());
    }
}

#[cfg(test)]
mod tests {
    use iris_core::types::Usage;

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
            usage: Usage::default(),
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
            old: None,
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

        app.handle(AppEvent::PermissionAnswered { note: None });
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

    #[test]
    fn shift_enter_and_ctrl_j_break_lines_while_enter_sends() {
        let mut app = App::new();
        app.handle_key(key(KeyCode::Char('a')));

        // On unix terminals Ctrl-J arrives as Enter+CONTROL; that is
        // the portable line break (kitty-protocol terminals also send
        // Shift+Enter).
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            None,
            "Ctrl-J must break the line, not submit"
        );
        app.handle_key(key(KeyCode::Char('b')));
        assert_eq!(app.prompt(), "a\nb", "the break split the composer");

        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        app.handle_key(key(KeyCode::Char('c')));
        assert_eq!(app.prompt(), "a\nb\nc");

        let submitted = app.handle_key(key(KeyCode::Enter));
        assert_eq!(submitted, Some("a\nb\nc".into()), "plain Enter sends");
        assert_eq!(app.prompt(), "", "the composer clears on submit");
    }

    #[test]
    fn history_recalls_prompts_and_restores_the_draft() {
        let mut app = App::new();
        type_text(&mut app, "one");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some("one".into()));
        app.handle(AppEvent::RunEnded {
            summary: "done".into(),
            usage: Usage::default(),
        });
        type_text(&mut app, "two");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some("two".into()));
        app.handle(AppEvent::RunEnded {
            summary: "done".into(),
            usage: Usage::default(),
        });

        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.prompt(), "two");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.prompt(), "one");
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.prompt(), "one", "clamped at the oldest entry");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.prompt(), "two");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.prompt(), "", "the unsent draft comes back");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.prompt(), "", "clamped at the newest position");
    }

    #[test]
    fn slash_commands_never_reach_the_loop() {
        let mut app = App::new();
        app.handle(AppEvent::Delta("some output\n".into()));

        type_text(&mut app, "/help");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None, "/help is local");
        assert!(
            app.status().contains("/quit"),
            "help lists every command: {}",
            app.status()
        );
        assert!(!app.is_running(), "commands never start a run");

        type_text(&mut app, "/clear");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert_eq!(app.transcript(), "", "/clear resets the visible screen");
        assert!(!app.is_running());

        type_text(&mut app, "/nope");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(
            app.status().contains("unknown command"),
            "unknown commands say so: {}",
            app.status()
        );

        type_text(&mut app, "/quit");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
        assert!(app.should_quit());
    }

    #[test]
    fn page_keys_scroll_the_scrollback_and_new_text_follows() {
        let mut app = App::new();
        for i in 0..50 {
            app.handle(AppEvent::Delta(format!("line{i}\n")));
        }
        assert_eq!(
            app.scroll_from_bottom(),
            0,
            "new output anchors to the tail"
        );

        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(
            app.scroll_from_bottom(),
            PAGE_LINES,
            "PageUp pages into the scrollback"
        );

        app.page_up();
        assert_eq!(app.scroll_from_bottom(), PAGE_LINES * 2);

        app.page_down();
        app.page_down();
        app.page_down(); // overshoot
        assert_eq!(app.scroll_from_bottom(), 0, "clamped at the tail");

        app.handle(AppEvent::Delta("more\n".into()));
        assert_eq!(app.scroll_from_bottom(), 0, "still following at the tail");
    }

    #[test]
    fn paste_lands_verbatim_in_the_composer() {
        let mut app = App::new();
        app.handle_paste("first line\nsecond line");
        assert_eq!(app.prompt(), "first line\nsecond line");

        type_text(&mut app, " one more");
        assert_eq!(app.prompt(), "first line\nsecond line one more");

        // A paste while a run is in flight stays out of the composer,
        // like every other key.
        assert!(app.handle_key(key(KeyCode::Enter)).is_some());
        app.handle_paste("dropped");
        assert_eq!(app.prompt(), "");
    }

    #[test]
    fn manual_scroll_is_not_yanked_back_by_new_output() {
        let mut app = App::new();
        for i in 0..30 {
            app.handle(AppEvent::Delta(format!("para {i}\n\n")));
        }
        app.handle_key(key(KeyCode::PageUp));
        assert_eq!(app.scroll_from_bottom(), PAGE_LINES);

        // 4.3: while the reader is paged up, streamed output must not
        // yank the view back to the tail.
        app.handle(AppEvent::Delta("streamed while reading\n".into()));
        assert_eq!(
            app.scroll_from_bottom(),
            PAGE_LINES,
            "manual scroll pauses the auto-scroll"
        );

        // Paging back to the tail resumes following.
        app.page_down();
        app.page_down();
        assert_eq!(app.scroll_from_bottom(), 0);
        app.handle(AppEvent::Delta("and we follow again\n".into()));
        assert_eq!(app.scroll_from_bottom(), 0, "following resumes");
    }

    #[test]
    fn gauges_accumulate_cost_and_track_the_run_budget() {
        let mut app = App::with_run_config("openai/gpt-4o-mini", 10, 1_000);
        assert_eq!(app.model(), "openai/gpt-4o-mini");
        assert_eq!(app.max_turns(), 10);
        assert_eq!(app.max_tokens_budget(), 1_000);
        assert_eq!(app.session_cost(), 0.0);
        assert_eq!(app.run_tokens(), 0);
        assert!(!app.over_budget());

        app.handle(AppEvent::Metrics(TurnMetrics {
            turn: 1,
            ttft_ms: Some(10),
            tokens_per_sec: 1.0,
            cache_hit_pct: None,
            cost_usd: Some(0.25),
        }));
        app.handle(AppEvent::Metrics(TurnMetrics {
            turn: 2,
            ttft_ms: Some(20),
            tokens_per_sec: 2.0,
            cache_hit_pct: Some(50.0),
            cost_usd: Some(0.75),
        }));
        assert!(
            (app.session_cost() - 1.0).abs() < 1e-9,
            "session cost accumulates reported turn costs, got {}",
            app.session_cost()
        );
        assert_eq!(app.last_metrics().map(|m| m.turn), Some(2));

        app.handle(AppEvent::RunEnded {
            summary: "completed in 2 turns".into(),
            usage: Usage {
                total_tokens: 600,
                ..Usage::default()
            },
        });
        assert_eq!(
            app.run_tokens(),
            600,
            "budget usage comes from the run's cumulative usage"
        );
        assert!(!app.over_budget(), "600 of 1000 is under budget");

        app.handle(AppEvent::RunEnded {
            summary: "stopped: token budget exceeded".into(),
            usage: Usage {
                total_tokens: 1_200,
                ..Usage::default()
            },
        });
        assert!(app.over_budget(), "1200 of 1000 is over budget");

        // An unconfigured app (no shell plumbing) renders no budget.
        assert!(!App::new().over_budget(), "budget 0 never goes red");
    }

    #[test]
    fn permission_keys_offer_the_four_answers_modally() {
        let mut app = App::new();
        app.handle(AppEvent::PermissionRequested {
            id: "c1".into(),
            name: "run_command".into(),
            detail: "{\"command\":\"ls\"}".into(),
            old: None,
        });

        // `y` answers the prompt and never reaches the composer.
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_answer(), Some(PermissionAnswer::Allow));
        assert!(app.prompt().is_empty(), "composer untouched");

        // In flight until the resolver acknowledges: a second press is
        // swallowed, so one decision can never be captured twice.
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.take_answer(), None, "double-answer suppressed");

        // The acknowledgment carries a status note ("rule saved …").
        app.handle(AppEvent::PermissionAnswered {
            note: Some("rule saved: run_command = allow".into()),
        });
        assert_eq!(app.pending(), None, "prompt closed");
        assert_eq!(app.state(), AppState::Streaming);
        assert_eq!(app.status(), "rule saved: run_command = allow");

        // The other three keys map one-to-one.
        app.handle(AppEvent::PermissionRequested {
            id: "c2".into(),
            name: "write_file".into(),
            detail: "{}".into(),
            old: None,
        });
        app.handle_key(key(KeyCode::Char('n')));
        assert_eq!(app.take_answer(), Some(PermissionAnswer::Deny));
        app.handle(AppEvent::PermissionAnswered { note: None });

        app.handle(AppEvent::PermissionRequested {
            id: "c3".into(),
            name: "write_file".into(),
            detail: "{}".into(),
            old: None,
        });
        app.handle_key(key(KeyCode::Char('a')));
        assert_eq!(app.take_answer(), Some(PermissionAnswer::AllowSession));
        app.handle(AppEvent::PermissionAnswered { note: None });

        app.handle(AppEvent::PermissionRequested {
            id: "c4".into(),
            name: "write_file".into(),
            detail: "{}".into(),
            old: None,
        });
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(app.take_answer(), Some(PermissionAnswer::AllowRule));

        // A dead run can never leave a modal prompt — or a
        // captured-but-unsent answer — behind.
        app.handle(AppEvent::PermissionRequested {
            id: "c5".into(),
            name: "run_command".into(),
            detail: "{}".into(),
            old: None,
        });
        app.handle_key(key(KeyCode::Char('y')));
        app.handle(AppEvent::Error {
            message: "provider down".into(),
        });
        assert_eq!(app.pending(), None, "prompt closed by run end");
        assert_eq!(app.take_answer(), None, "stale answer dropped");
    }

    #[test]
    fn the_prompt_is_modal_until_answered() {
        let mut app = App::new();
        type_text(&mut app, "draft");
        app.handle(AppEvent::PermissionRequested {
            id: "c1".into(),
            name: "run_command".into(),
            detail: "ls".into(),
            old: None,
        });

        // Composer keys are swallowed while the prompt is open.
        app.handle_key(key(KeyCode::Char('z')));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.prompt(), "draft", "composer untouched");
        assert!(!app.is_running(), "Enter did not submit");
        assert_eq!(app.take_answer(), None, "`z` is not an answer");

        // Non-answers stay non-answers.
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.take_answer(), None);

        // Ctrl-C still quits straight out of the prompt.
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_quit());
    }

    #[test]
    fn diff_asks_open_with_hunks_the_keys_move_and_toggle() {
        // A write_file ask carrying the current content → the app
        // builds the hunk view (stage 4.6).
        let mut app = App::new();
        let detail =
            serde_json::json!({"path": "f.txt", "content": "a1\nCHANGED\na3\n"}).to_string();
        app.handle(AppEvent::PermissionRequested {
            id: "c1".into(),
            name: "write_file".into(),
            detail,
            old: Some("a1\na2\na3\n".into()),
        });
        let diff = app.pending_diff().expect("diff view");
        assert_eq!(diff.hunks.len(), 1);
        assert!(diff.all_accepted(), "accepted by default");
        assert_eq!(app.hunk_cursor(), 0);

        // j/k move the hunk cursor (clamped), space toggles.
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.hunk_cursor(), 0, "clamped at the top");
        app.handle_key(key(KeyCode::Char('j')));
        assert_eq!(app.hunk_cursor(), 0, "clamped at the bottom (one hunk)");
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(
            !app.pending_diff().expect("diff").all_accepted(),
            "space rejected the hunk"
        );

        // The prompt stays modal — these keys are not answers.
        assert_eq!(app.take_answer(), None);
        assert!(app.pending().is_some());
    }

    #[test]
    fn rejecting_hunks_turns_y_into_a_partial_apply() {
        let mut app = App::new();
        let old = "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n";
        let new = "a1\nCHANGED\na3\na4\na5\na6\na7\na8\na9\nCHANGED2\n";

        app.handle(AppEvent::PermissionRequested {
            id: "c1".into(),
            name: "write_file".into(),
            detail: serde_json::json!({"path": "f.txt", "content": new}).to_string(),
            old: Some(old.into()),
        });
        assert_eq!(
            app.pending_diff().expect("diff").hunks.len(),
            2,
            "two distant changes"
        );

        // Everything accepted (the default) → `y` is the plain allow.
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(app.take_answer(), Some(PermissionAnswer::Allow));
        app.handle(AppEvent::PermissionAnswered { note: None });
        assert_eq!(app.pending_diff(), None, "diff cleared with the prompt");

        // Reject the first hunk → `y` becomes a partial apply whose
        // input keeps only the second change.
        app.handle(AppEvent::PermissionRequested {
            id: "c2".into(),
            name: "write_file".into(),
            detail: serde_json::json!({"path": "f.txt", "content": new}).to_string(),
            old: Some(old.into()),
        });
        app.handle_key(key(KeyCode::Char(' '))); // reject hunk 0
        app.handle_key(key(KeyCode::Char('y')));
        let Some(PermissionAnswer::AllowWith { input, note }) = app.take_answer() else {
            panic!("partial apply expected");
        };
        assert!(note.contains("1/2"), "note counts hunks: {note}");
        let input: serde_json::Value = serde_json::from_str(&input).expect("json");
        let expected = "a1\na2\na3\na4\na5\na6\na7\na8\na9\nCHANGED2\n";
        assert_eq!(input["content"], expected, "accepted hunk only");

        // A dead run clears the diff view too.
        app.handle(AppEvent::PermissionAnswered { note: None });
        app.handle(AppEvent::PermissionRequested {
            id: "c3".into(),
            name: "write_file".into(),
            detail: serde_json::json!({"path": "f.txt", "content": new}).to_string(),
            old: Some(old.into()),
        });
        app.handle(AppEvent::RunEnded {
            summary: "stopped".into(),
            usage: Usage::default(),
        });
        assert_eq!(app.pending_diff(), None, "diff cleared by run end");
    }
}
