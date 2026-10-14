//! The live gauge (stage 3.4) — one status line per turn, rendered from
//! the loop's `LoopEvent::Metrics` as they arrive.
//!
//! Text mode first: `run -p` and `sessions resume` print these to
//! stderr (stdout belongs to the model's answer, pipeable), and the
//! stage-4 TUI renders the same `TurnMetrics` over channels — the
//! numbers are validated here before any widget work.
//!
//! Unreported measurements render as `–`, never as a fake `0`: an
//! unreported cache is not an empty cache (the stage-3.3 rule, restated
//! for the view layer).

use iris_core::metrics::TurnMetrics;

/// The complete status line for one turn, `[iris]` prefix included:
///
/// ```text
/// [iris] turn 1 · ttft 184ms · 62.5 tok/s · cache 32.99% · cost $0.0042
/// ```
///
/// - `ttft` — whole milliseconds; absent (`–`) until a token arrived.
/// - `tok/s` — completion tokens per second of streaming, one decimal.
/// - `cache` — hit percent, two decimals (64/194 → `32.99%`).
/// - `cost` — provider charge or price-table estimate, **4 decimals**
///   (the same precision `sessions list` prints).
pub(crate) fn format_turn(metrics: &TurnMetrics) -> String {
    let ttft = match metrics.ttft_ms {
        Some(ms) => format!("{ms}ms"),
        None => "–".to_string(),
    };
    let hit = match metrics.cache_hit_pct {
        Some(pct) => format!("{pct:.2}%"),
        None => "–".to_string(),
    };
    let cost = match metrics.cost_usd {
        Some(usd) => format!("${usd:.4}"),
        None => "–".to_string(),
    };
    format!(
        "[iris] turn {} · ttft {ttft} · {:.1} tok/s · cache {hit} · cost {cost}",
        metrics.turn, metrics.tokens_per_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_metrics_render_every_segment() {
        let line = format_turn(&TurnMetrics {
            turn: 1,
            ttft_ms: Some(184),
            tokens_per_sec: 62.5,
            cache_hit_pct: Some(32.98969072164949),
            cost_usd: Some(0.0042),
        });
        assert_eq!(
            line,
            "[iris] turn 1 · ttft 184ms · 62.5 tok/s · cache 32.99% · cost $0.0042"
        );
    }

    #[test]
    fn unreported_fields_show_placeholders_not_zeroes() {
        let line = format_turn(&TurnMetrics {
            turn: 2,
            ttft_ms: None,
            tokens_per_sec: 0.0,
            cache_hit_pct: None,
            cost_usd: None,
        });
        assert_eq!(
            line, "[iris] turn 2 · ttft – · 0.0 tok/s · cache – · cost –",
            "absent is shown as absent, never as a fabricated number"
        );
    }

    #[test]
    fn cost_renders_to_four_decimals_and_speed_to_one() {
        let line = format_turn(&TurnMetrics {
            turn: 7,
            ttft_ms: Some(0),
            tokens_per_sec: 1234.5678,
            cache_hit_pct: Some(100.0),
            cost_usd: Some(1.2345678),
        });
        assert_eq!(
            line,
            "[iris] turn 7 · ttft 0ms · 1234.6 tok/s · cache 100.00% · cost $1.2346"
        );
    }
}
