//! The render pass (stage 4.1) — a function of `&App` only, so it is
//! proven against ratatui's `TestBackend` without a terminal.
//!
//! Skeleton panes: a header, the raw transcript (stage 4.3 turns this
//! into a markdown pane), the status/state line (stage 4.4 turns the
//! status area into the full gauge bar), and the minimal composer
//! (stage 4.2 turns it into the real composer).

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;

/// Draw one frame of the current app state.
pub fn render(frame: &mut Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(frame.area());

    let header = Paragraph::new(format!("IRIS — {}", app.state().label()))
        .block(Block::default().borders(Borders::ALL).title("iris chat"));
    frame.render_widget(header, chunks[0]);

    let transcript = Paragraph::new(app.transcript())
        .block(Block::default().borders(Borders::ALL).title("transcript"))
        .wrap(Wrap { trim: true });
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

    let composer = Paragraph::new(format!("> {}", app.prompt())).block(
        Block::default()
            .borders(Borders::ALL)
            .title("input (composer: 4.2)"),
    );
    frame.render_widget(composer, chunks[3]);
}

#[cfg(test)]
mod tests {
    use iris_core::metrics::TurnMetrics;

    use super::*;
    use crate::events::AppEvent;

    /// The rendered frame as plain text, row by row.
    fn rendered(app: &App) -> String {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| render(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer();
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
}
