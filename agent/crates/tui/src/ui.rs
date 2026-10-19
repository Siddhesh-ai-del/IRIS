//! The render pass — a function of `&App` only, so it is proven
//! against ratatui's `TestBackend` without a terminal.
//!
//! Panes: a header, the transcript rendered as markdown (stage 4.3:
//! `tui-markdown`, code blocks keep their language tag, tail-following
//! scrollback that pauses while the user is paged up), the status
//! block (stage 4.4: state + status on row A, the gauge bar — model,
//! turn, budget, cost, TTFT, tok/s, cache % — on row B), and the
//! composer (stage 4.2: the `ratatui-textarea` widget with key hints
//! in its title). While a permission is pending, the stage-4.5 popup
//! (preview + `y/n/a/r` hints) floats over everything.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;
use crate::diff::EditDiff;

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
            // Row A: state + status; row B: the stage-4.4 gauge bar.
            Constraint::Length(2),
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

    let status_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(chunks[2]);
    let status = match app.pending() {
        Some(pending) => format!(
            "[{}] allow {} ({})?",
            app.state().label(),
            pending.name,
            pending.id
        ),
        None => format!("[{}] {}", app.state().label(), app.status()),
    };
    frame.render_widget(Paragraph::new(status), status_chunks[0]);
    frame.render_widget(Paragraph::new(gauges(app)), status_chunks[1]);

    let composer_block = Block::default().borders(Borders::ALL).title(COMPOSER_HINTS);
    frame.render_widget(composer_block, chunks[3]);
    let composer_area = chunks[3].inner(Margin::new(1, 1));
    frame.render_widget(app.composer(), composer_area);

    // The permission prompt (4.5/4.6) floats above every pane: the
    // hunk view for file edits, the plain preview otherwise — then the
    // key hints. Drawn last so it overlaps the transcript.
    if let Some(pending) = app.pending() {
        let diff = app.pending_diff();
        // The hunk view needs more vertical room than the preview.
        let popup = centered_rect(70, if diff.is_some() { 65 } else { 45 }, frame.area());
        let inner = popup.inner(Margin::new(1, 1));
        let hint_rows = if diff.is_some() { 4 } else { 3 };
        let room = inner.height.saturating_sub(hint_rows) as usize;
        let mut lines = match diff {
            Some(diff) => diff_view(diff, app.hunk_cursor(), room),
            None => preview(&pending.detail, room),
        };
        lines.push(Line::from(""));
        if diff.is_some() {
            lines.push(Line::from(
                "j/k hunks · space accept/reject — y runs the accepted parts",
            ));
        }
        lines.push(Line::from("[y] allow once   [n] deny once"));
        lines.push(Line::from(
            "[a] always (this session)   [r] always (rule → config)",
        ));
        let title = match diff {
            Some(diff) => {
                let (accepted, total) = diff.accepted_counts();
                format!(
                    "Permission — {} ({}) · {} — {accepted}/{total} hunks",
                    pending.name, pending.id, diff.path
                )
            }
            None => format!("Permission — {} ({})", pending.name, pending.id),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        frame.render_widget(Paragraph::new(lines).block(block), popup);
    }
}

/// A centered sub-rectangle in percentages of `area` — the permission
/// prompt's popup.
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

/// The command/diff preview at the heart of the prompt (4.5): the
/// command for `run_command`, path + first body lines for file writes,
/// patch text for `apply_patch`, pretty JSON otherwise — and the raw
/// detail when it is not JSON at all. `room` is the popup's preview
/// lines; more is clipped to an ellipsis so the key hints always fit.
fn preview(detail: &str, room: usize) -> Vec<Line<'static>> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(detail) else {
        return clip(
            detail
                .lines()
                .map(|line| Line::from(line.to_string()))
                .collect(),
            room,
        );
    };
    let body: Vec<String> = if let Some(command) = value.get("command").and_then(|v| v.as_str()) {
        vec![format!("$ {command}")]
    } else if let (Some(path), Some(content)) = (
        value.get("path").and_then(|v| v.as_str()),
        value.get("content").and_then(|v| v.as_str()),
    ) {
        let mut lines = vec![path.to_string()];
        lines.extend(content.lines().map(str::to_string));
        lines
    } else if let Some(patch) = value
        .get("patch")
        .and_then(|v| v.as_str())
        .or_else(|| value.get("diff").and_then(|v| v.as_str()))
    {
        patch.lines().map(str::to_string).collect()
    } else {
        vec![serde_json::to_string_pretty(&value).unwrap_or_else(|_| detail.to_string())]
    };
    clip(body.into_iter().map(Line::from).collect(), room)
}

/// Clip a preview to `room` lines, ending in an ellipsis.
fn clip(mut lines: Vec<Line<'static>>, room: usize) -> Vec<Line<'static>> {
    if room > 0 && lines.len() > room {
        lines.truncate(room.saturating_sub(1));
        lines.push(Line::from("…"));
    }
    lines
}

/// The hunk view inside the prompt (stage 4.6): each hunk's header
/// carries its accept mark (`✓`/`✗`, the cursor's header highlighted),
/// `+` lines are green, `-` red, context dim. When the whole diff does
/// not fit, the view scrolls so the cursor hunk's header stays
/// visible, with ↑/↓ markers for the cut-off rest.
fn diff_view(diff: &EditDiff, cursor: usize, room: usize) -> Vec<Line<'static>> {
    if room == 0 {
        return Vec::new();
    }
    let mut flat: Vec<Line<'static>> = Vec::new();
    for (index, hunk) in diff.hunks.iter().enumerate() {
        let mark = if hunk.accepted { "✓" } else { "✗" };
        let header_style = if index == cursor {
            Style::new().fg(Color::Yellow).bold()
        } else if hunk.accepted {
            Style::new().fg(Color::DarkGray)
        } else {
            Style::new().fg(Color::Red)
        };
        flat.push(Line::styled(
            format!("{mark} {}", hunk.header),
            header_style,
        ));
        for (sign, text) in &hunk.lines {
            let style = match sign {
                '+' => Style::new().fg(Color::Green),
                '-' => Style::new().fg(Color::Red),
                _ => Style::new().fg(Color::DarkGray),
            };
            let text = text.trim_end_matches('\n');
            flat.push(Line::styled(format!("  {sign} {text}"), style));
        }
    }
    let total = flat.len();
    if total <= room {
        return flat;
    }
    // Scroll: the cursor hunk's header is the first visible line once
    // the tail would otherwise push it out of the window.
    let cursor_start: usize = diff
        .hunks
        .iter()
        .take(cursor.min(diff.hunks.len()))
        .map(|hunk| 1 + hunk.lines.len())
        .sum();
    let start = cursor_start.min(total.saturating_sub(room));
    let mut take = room.saturating_sub(usize::from(start > 0));
    if start + take < total {
        take = take.saturating_sub(1); // room for the ↓ marker
    }
    let mut visible: Vec<Line<'static>> = Vec::with_capacity(room);
    if start > 0 {
        visible.push(Line::from("  ↑"));
    }
    visible.extend(flat.into_iter().skip(start).take(take));
    if start + take < total {
        visible.push(Line::from("  ↓"));
    }
    visible
}

/// The stage-4.4 gauge row: model · turn · budget bar · cost · TTFT ·
/// tok/s · cache %. The budget segment (digits + bar) turns red when
/// the last run met or passed its token budget. A cost *cap* has no
/// config knob yet (stage 3 shipped a token budget only), so cost is
/// shown but never a red trigger — documented in the plan tick.
fn gauges(app: &App) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    if !app.model().is_empty() {
        // Basename only — `openai/gpt-4o-mini` → `gpt-4o-mini`; the
        // org prefix repeats what the config already says.
        let model = app.model();
        spans.push(Span::raw(
            model.rsplit('/').next().unwrap_or(model).to_string(),
        ));
        sep(&mut spans);
    }

    let turn = app
        .last_metrics()
        .map(|metrics| metrics.turn.to_string())
        .unwrap_or_else(|| "-".to_string());
    spans.push(Span::raw(format!("turn {turn}/{}", app.max_turns())));

    if app.max_tokens_budget() > 0 {
        sep(&mut spans);
        let style = if app.over_budget() {
            Style::new().fg(Color::Red)
        } else {
            Style::new()
        };
        spans.push(Span::styled(
            format!("tok {}/{}", app.run_tokens(), app.max_tokens_budget()),
            style,
        ));
        spans.push(Span::styled(
            format!(" {}", budget_bar(app.run_tokens(), app.max_tokens_budget())),
            style,
        ));
    }

    if app.session_cost() > 0.0 {
        sep(&mut spans);
        spans.push(Span::raw(format!("${:.4}", app.session_cost())));
    }

    if let Some(metrics) = app.last_metrics() {
        if let Some(ttft) = metrics.ttft_ms {
            sep(&mut spans);
            spans.push(Span::raw(format!("{ttft}ms")));
        }
        sep(&mut spans);
        spans.push(Span::raw(format!("{:.1} t/s", metrics.tokens_per_sec)));
        if let Some(pct) = metrics.cache_hit_pct {
            sep(&mut spans);
            spans.push(Span::raw(format!("cache {pct:.0}%")));
        }
    }

    Line::from(spans)
}

fn sep(spans: &mut Vec<Span<'static>>) {
    spans.push(Span::raw(" · "));
}

/// A six-cell budget bar: filled cells = used ÷ budget (clamped).
fn budget_bar(used: u64, budget: u64) -> String {
    const CELLS: u64 = 6;
    let filled = used.min(budget) * CELLS / budget;
    "▓".repeat(filled as usize) + &"░".repeat((CELLS - filled) as usize)
}

#[cfg(test)]
mod tests {
    use iris_core::metrics::TurnMetrics;
    use iris_core::types::Usage;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
            old: None,
        });
        let text = rendered(&app);
        assert!(text.contains("run_command"), "the ask is visible:\n{text}");
        assert!(
            text.contains("AwaitingPermission"),
            "state visible:\n{text}"
        );
    }

    #[test]
    fn permission_popup_previews_the_command_and_lists_the_keys() {
        let mut app = App::new();
        app.handle(AppEvent::PermissionRequested {
            id: "c3".into(),
            name: "run_command".into(),
            detail: "{\"command\":\"rm -rf build\"}".into(),
            old: None,
        });
        let text = rendered(&app);
        assert!(text.contains("rm -rf build"), "command previewed:\n{text}");
        assert!(text.contains("[y]"), "allow key:\n{text}");
        assert!(text.contains("[n]"), "deny key:\n{text}");
        assert!(text.contains("session"), "session scope key:\n{text}");
        assert!(text.contains("[r]"), "rule key:\n{text}");
    }

    #[test]
    fn the_diff_popup_renders_hunks_marks_and_the_nav_hint() {
        let mut app = App::new();
        app.handle(AppEvent::PermissionRequested {
            id: "c1".into(),
            name: "write_file".into(),
            detail: serde_json::json!({
                "path": "src/main.rs",
                "content": "fn main() {\n    let name = \"iris\";\n    println!(\"{name}\");\n}\n"
            })
            .to_string(),
            old: Some("fn main() {\n    println!(\"hi\");\n}\n".into()),
        });
        let text = rendered(&app);
        assert!(
            text.contains("src/main.rs"),
            "target path in the title:\n{text}"
        );
        assert!(text.contains("@@"), "hunk header:\n{text}");
        assert!(
            text.contains('+') && text.contains('-'),
            "diff signs:\n{text}"
        );
        assert!(text.contains("space accept/reject"), "nav hint:\n{text}");
        insta::assert_snapshot!("permission_diff_popup", text);
    }

    #[test]
    fn rejecting_a_hunk_marks_it_and_updates_the_counts() {
        let mut app = App::new();
        app.handle(AppEvent::PermissionRequested {
            id: "c1".into(),
            name: "write_file".into(),
            detail: serde_json::json!({"path": "f.txt", "content": "a1\nCHANGED\na3\n"})
                .to_string(),
            old: Some("a1\na2\na3\n".into()),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        let text = rendered(&app);
        assert!(text.contains('✗'), "rejected mark:\n{text}");
        assert!(text.contains("0/1 hunks"), "counts update:\n{text}");
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

    #[test]
    fn status_bar_shows_the_run_gauges() {
        let mut app = App::with_run_config("openai/gpt-4o-mini", 10, 1_000);
        app.handle(AppEvent::Metrics(TurnMetrics {
            turn: 2,
            ttft_ms: Some(420),
            tokens_per_sec: 38.2,
            cache_hit_pct: Some(62.0),
            cost_usd: Some(0.0123),
        }));
        app.handle(AppEvent::RunEnded {
            summary: "completed in 2 turns".into(),
            usage: Usage {
                total_tokens: 400,
                ..Usage::default()
            },
        });
        let text = rendered(&app);
        assert!(text.contains("gpt-4o-mini"), "model (basename):\n{text}");
        assert!(text.contains("turn 2/10"), "turn gauge:\n{text}");
        assert!(text.contains("tok 400/1000"), "budget gauge:\n{text}");
        assert!(
            text.contains("▓▓░░░░"),
            "budget bar filled by ratio:\n{text}"
        );
        assert!(text.contains("$0.0123"), "cost gauge:\n{text}");
        assert!(text.contains("420ms"), "ttft gauge:\n{text}");
        assert!(text.contains("38.2 t/s"), "speed gauge:\n{text}");
        // cache % is on the bar too, but past column 78 on this test
        // frame — its presence is asserted through the app's metrics
        // state (see the unit test) and on wider terminals.
    }

    #[test]
    fn the_budget_gauge_goes_red_when_exceeded() {
        let mut app = App::with_run_config("m", 5, 500);
        app.handle(AppEvent::RunEnded {
            summary: "stopped: token budget exceeded".into(),
            usage: Usage {
                total_tokens: 500,
                ..Usage::default()
            },
        });
        let buffer = buffer_of(&app);
        // The gauge row is the lower of the two status rows: above the
        // 6-row composer.
        let gauge_row = buffer.area.height - 7;
        let red = (0..buffer.area.width).any(|x| {
            buffer
                .cell((x, gauge_row))
                .is_some_and(|cell| cell.style().fg == Some(Color::Red))
        });
        assert!(red, "the budget segment turns red at/over budget");
    }
}
