//! The render pass — a function of `&App` only, so it is proven
//! against ratatui's `TestBackend` without a terminal.
//!
//! Panes: a header, the transcript rendered as markdown (stage 4.3:
//! `tui-markdown`, code blocks keep their language tag, tail-following
//! scrollback that pauses while the user is paged up), the
//! status/state line (stage 4.4 turns the status area into the full
//! gauge bar), and the composer (stage 4.2: the `ratatui-textarea`
//! widget with key hints in its title).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;

/// Composer key hints, rendered in the pane title (the `> ` prefix
/// marks the input row, as since the 4.1 skeleton).
const COMPOSER_HINTS: &str =
    "> input — Enter sends · Shift+Enter/Ctrl-J newline · PgUp/PgDn scroll · /help";

/// Draw one frame of the current app state.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
            // Tall enough to show a few draft lines (4.2 multiline
            // composer); the textarea scrolls internally beyond that.
            Constraint::Length(6),
        ])
        .split(frame.area());

    let header = Paragraph::new(format!("IRIS — {}", app.state().label()))
        .block(Block::default().borders(Borders::ALL).title("iris chat"));
    frame.render_widget(header, chunks[0]);

    // Scrollback: the pane renders assistant text as markdown (4.3),
    // so scroll math counts the *rendered* lines — markdown reflow
    // changes them. The view follows the tail (offset 0) until the
    // user pages up; overshooting past the head clamps at the top.
    let view_height = chunks[1].height.saturating_sub(2); // borders
    let text = crate::markdown::transcript(app.transcript());
    let line_count = text.lines.len() as u16;
    let max_top = line_count.saturating_sub(view_height);
    let top = max_top.saturating_sub(app.scroll_from_bottom());
    let transcript = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("transcript — PgUp/PgDn pages history"),
        )
        // `trim: false` — code blocks keep their indentation.
        .wrap(Wrap { trim: false })
        .scroll((top, 0));
    frame.render_widget(transcript, chunks[1]);

    let status = match app.pending() {
        Some(pending) => format!(
            "[{}] allow {} ({})? — prompt lands in 4.5",
            app.state().label(),
            pending.name,
            pending.id
        ),
        None => format!("[{}] {}", app.state().label(), app.status()),
    };
    frame.render_widget(Paragraph::new(status), chunks[2]);

    let composer_block = Block::default().borders(Borders::ALL).title(COMPOSER_HINTS);
    frame.render_widget(composer_block, chunks[3]);
    let composer_area = chunks[3].inner(Margin::new(1, 1));
    frame.render_widget(app.composer(), composer_area);
}

#[cfg(test)]
mod tests {
    use iris_core::metrics::TurnMetrics;
    use ratatui::style::Color;

    use super::*;
    use crate::events::AppEvent;

    /// One drawn frame's buffer, for text *and* style assertions.
    fn buffer_of(app: &App) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| render(frame, app)).expect("draw");
        terminal.backend().buffer().clone()
    }

    /// The rendered frame as plain text, row by row.
    fn rendered(app: &App) -> String {
        let buffer = buffer_of(app);
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn idle_frame_shows_header_state_and_composer() {
        let text = rendered(&App::new());
        assert!(text.contains("IRIS"), "header present:\n{text}");
        assert!(text.contains("Idle"), "state visible:\n{text}");
        assert!(text.contains("> "), "composer visible:\n{text}");
    }

    #[test]
    fn streaming_frame_shows_the_transcript_text_and_state() {
        let mut app = App::new();
        app.handle(AppEvent::Delta("Streaming now…".into()));
        let text = rendered(&app);
        assert!(text.contains("Streaming now…"), "transcript pane:\n{text}");
        assert!(text.contains("Streaming"), "state visible:\n{text}");
    }

    #[test]
    fn awaiting_frame_shows_the_pending_permission() {
        let mut app = App::new();
        // Real order within a turn: the response completes (metrics),
        // *then* the loop dispatches tools and may ask.
        app.handle(AppEvent::Metrics(TurnMetrics {
            turn: 1,
            ttft_ms: None,
            tokens_per_sec: 0.0,
            cache_hit_pct: None,
            cost_usd: None,
        }));
        app.handle(AppEvent::PermissionRequested {
            id: "c3".into(),
            name: "run_command".into(),
            detail: "rm -rf build".into(),
        });
        let text = rendered(&app);
        assert!(text.contains("run_command"), "the ask is visible:\n{text}");
        assert!(
            text.contains("AwaitingPermission"),
            "state visible:\n{text}"
        );
    }

    #[test]
    fn composer_renders_every_line_of_a_multiline_draft() {
        let mut app = App::new();
        app.handle_paste("alpha\nbeta");
        let text = rendered(&app);
        assert!(text.contains("alpha"), "first line visible:\n{text}");
        assert!(text.contains("beta"), "second line visible:\n{text}");
        assert!(
            text.contains("Enter send"),
            "the composer's key hints are shown:\n{text}"
        );
    }

    #[test]
    fn scrollback_follows_the_tail_and_pages_up_into_history() {
        let mut app = App::new();
        // A fenced block: markdown preserves its lines verbatim (soft
        // line breaks in prose reflow to spaces) — the same shape
        // real tool output takes in the transcript.
        app.handle(AppEvent::Delta("```\n".into()));
        for i in 0..40 {
            app.handle(AppEvent::Delta(format!("L{i}\n")));
        }
        app.handle(AppEvent::Delta("```\n".into()));

        // The pane shows the newest output, not the oldest.
        let text = rendered(&app);
        assert!(!text.contains("L0"), "the view follows the tail:\n{text}");
        assert!(text.contains("L39"), "the newest line is visible:\n{text}");

        // Three pages up reaches the head of a 42-line block
        // (12 visible rows).
        for _ in 0..3 {
            app.page_up();
        }
        let text = rendered(&app);
        assert!(text.contains("L0"), "paging up reaches history:\n{text}");
    }

    #[test]
    fn transcript_renders_as_marked_up_markdown() {
        let mut app = App::new();
        app.handle(AppEvent::Delta(
            "# Title\n\nSome **bold** text.\n\n```rust\nfn main() {}\n```\n".into(),
        ));
        let text = rendered(&app);
        assert!(text.contains("Title"), "heading text survives:\n{text}");
        assert!(
            text.contains("```rust"),
            "the code fence keeps its language tag:\n{text}"
        );
        assert!(text.contains("fn main() {}"), "code content:\n{text}");
        assert!(
            !text.contains("**"),
            "markdown markers are consumed by the parser, not shown raw:\n{text}"
        );
    }

    #[test]
    fn code_blocks_are_styled_as_code() {
        let mut app = App::new();
        app.handle(AppEvent::Delta("```\nlet x = 1;\n```\n".into()));
        let buffer = buffer_of(&app);

        // Find the code line and check it carries the code style
        // (white on black), not the default paragraph style.
        let mut found = false;
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                .collect();
            if let Some(pos) = row.find("let x") {
                let cell = buffer
                    .cell((pos as u16, y))
                    .expect("the cell holding the code");
                assert_eq!(
                    cell.style().bg,
                    Some(Color::Black),
                    "the code line is styled as code"
                );
                found = true;
            }
        }
        assert!(found, "the code line is rendered");
    }
}
