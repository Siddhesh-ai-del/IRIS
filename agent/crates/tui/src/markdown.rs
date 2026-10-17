//! Transcript markdown rendering (stage 4.3).
//!
//! [`transcript`] parses the assistant's raw markdown with
//! `tui-markdown` (pulldown-cmark on the backend) into styled ratatui
//! lines: headings bold/colored, inline code and fenced blocks
//! styled as code, and a ```` ```lang ```` fence line that keeps the
//! code block's language tag — the plan's "code blocks with language
//! tag" requirement comes from the renderer for free.
//!
//! Only assistant text enters the pane ([`crate::events::AppEvent`]
//! carries tool results in `ToolEnd`, never in `Delta`), so the whole
//! buffer can go through the parser. Tool output the model shows the
//! user arrives inside fenced code blocks, where markdown preserves
//! lines verbatim — prose soft breaks reflow to spaces, which is
//! correct markdown and exactly why the scrollback tests fence their
//! fixtures.
//!
//! Cost note (KQ2.5): the pane re-parses the transcript every draw.
//! pulldown-cmark is linear and session transcripts are human-scale
//! (even ~100 KB parses in a small fraction of a frame), so the
//! skeleton pays no caching complexity. If profiling ever disagrees,
//! the seam is a cached `Text` invalidated on append length.

use ratatui::text::Text;

/// Parse raw transcript markdown into renderable, styled lines.
///
/// Borrows from `raw` — the caller (`[`crate::ui`]`) renders it in
/// the same frame, so no owned copy crosses the boundary.
pub fn transcript(raw: &str) -> Text<'_> {
    tui_markdown::from_str(raw)
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

    #[test]
    fn headings_code_and_markers_render_as_markdown() {
        let text = transcript("# Hi\n\n**bold**\n\n```rs\ncode()\n```\n");
        let lines: Vec<String> = text
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        let all = lines.join("\n");
        assert!(all.contains("```rs"), "language tag kept: {all}");
        assert!(all.contains("code()"), "code kept: {all}");
        assert!(!all.contains("**"), "markers consumed: {all}");
    }

    #[test]
    fn prose_soft_breaks_reflow_to_spaces() {
        // The markdown contract the scrollback fixtures rely on:
        // fenced lines survive, prose lines reflow.
        let fenced = transcript("```\na\nb\n```");
        assert_eq!(fenced.lines.len(), 4, "fence keeps every line");
        let prose = transcript("a\nb");
        assert_eq!(prose.lines.len(), 1, "prose reflows to one line");
    }

    #[test]
    fn code_lines_carry_the_code_style() {
        let text = transcript("```\nx\n```");
        let code_line = text
            .lines
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content == "x"))
            .expect("code line rendered");
        // The plain (non-highlighting) path styles the whole line.
        assert_eq!(
            code_line.style.bg,
            Some(Color::Black),
            "white-on-black code style"
        );
    }
}
