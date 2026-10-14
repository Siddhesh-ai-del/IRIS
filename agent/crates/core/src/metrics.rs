//! Per-turn metrics (stage 3.3) — one immutable [`TurnMetrics`] per
//! provider response, appended to the transcript as `turn_metrics`
//! events and surfaced to observers for the stage-3.4 gauge.
//!
//! Honest definitions, fixed here so every consumer agrees:
//!
//! - **`ttft_ms`**: request sent → first streamed token (text delta or
//!   tool-call argument fragment — both are tokens). `None` when no
//!   token ever arrived.
//! - **`tokens_per_sec`**: completion tokens ÷ streaming time measured
//!   first token → stream end (0.0 when nothing streamed). Excludes
//!   queue/wait time before the first byte, which the provider owns.
//! - **`cache_hit_pct`**: `cached_input_tokens / input_tokens × 100`.
//!   `None` when the provider does not report caching — an unreported
//!   cache is not an empty cache.
//! - **`cost_usd`**: the provider's own charge (stage 3.2) when
//!   reported; otherwise an estimate from the config file's
//!   [`ModelPrice`] table; otherwise `None` — never a hardcoded price.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::ModelPrice;
use crate::types::Usage;

/// Immutable measurements of one provider turn (stage 3.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TurnMetrics {
    /// 1-based turn — the same number as this turn's `usage` event.
    pub turn: u32,
    /// Request sent → first streamed token. Absent when no token
    /// arrived (nothing measured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Completion tokens per second of streaming (first token → end).
    pub tokens_per_sec: f64,
    /// Cached share of input tokens in percent, `None` when the
    /// provider reports no cache accounting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_hit_pct: Option<f64>,
    /// Provider-reported charge, else price-table estimate, else `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl TurnMetrics {
    /// Measure one turn: raw timings plus the turn's usage, priced
    /// through the optional price-table entry (see the module docs for
    /// the exact definition of each field).
    pub fn compute(
        turn: u32,
        usage: &Usage,
        price: Option<&ModelPrice>,
        ttft: Option<Duration>,
        stream_duration: Duration,
    ) -> Self {
        let tokens_per_sec = if usage.output_tokens == 0 || stream_duration.is_zero() {
            0.0
        } else {
            usage.output_tokens as f64 / stream_duration.as_secs_f64()
        };
        let cache_hit_pct = match usage.cached_input_tokens {
            Some(cached) if usage.input_tokens > 0 => {
                Some(cached as f64 * 100.0 / usage.input_tokens as f64)
            }
            _ => None,
        };
        let cost_usd = usage
            .cost_usd
            .or_else(|| price.map(|price| estimate_cost(price, usage)));
        Self {
            turn,
            ttft_ms: ttft.map(|elapsed| elapsed.as_millis() as u64),
            tokens_per_sec,
            cache_hit_pct,
            cost_usd,
        }
    }
}

/// The price table's estimate for one response: uncached input at the
/// input rate, the cached share at the cache rate, output at the output
/// rate — cached tokens are a subset of input tokens, never extra.
fn estimate_cost(price: &ModelPrice, usage: &Usage) -> f64 {
    let cached = usage
        .cached_input_tokens
        .unwrap_or(0)
        .min(usage.input_tokens);
    let uncached = usage.input_tokens - cached;
    let cache_rate = price
        .cached_input_per_million
        .unwrap_or(price.input_per_million);
    uncached as f64 * price.input_per_million / 1_000_000.0
        + cached as f64 * cache_rate / 1_000_000.0
        + usage.output_tokens as f64 * price.output_per_million / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> ModelPrice {
        ModelPrice {
            input_per_million: 0.15,
            output_per_million: 0.60,
            cached_input_per_million: Some(0.075),
        }
    }

    fn usage() -> Usage {
        Usage {
            input_tokens: 194,
            output_tokens: 2,
            total_tokens: 196,
            cached_input_tokens: Some(64),
            cost_usd: None,
        }
    }

    #[test]
    fn ttft_and_generation_speed_come_from_stream_timing() {
        let metrics = TurnMetrics::compute(
            1,
            &Usage {
                output_tokens: 40,
                ..Usage::default()
            },
            None,
            Some(Duration::from_millis(200)),
            Duration::from_millis(800),
        );
        assert_eq!(metrics.ttft_ms, Some(200));
        // 40 completion tokens over 0.8 s of streaming.
        assert_eq!(metrics.tokens_per_sec, 50.0);
    }

    #[test]
    fn no_streamed_token_means_no_ttft_and_zero_speed() {
        let metrics = TurnMetrics::compute(2, &Usage::default(), None, None, Duration::ZERO);
        assert_eq!(metrics.ttft_ms, None, "no token → nothing to time");
        assert_eq!(metrics.tokens_per_sec, 0.0);
    }

    #[test]
    fn cache_hit_pct_is_the_cached_share_of_input() {
        let metrics = TurnMetrics::compute(1, &usage(), None, None, Duration::ZERO);
        let pct = metrics.cache_hit_pct.expect("cache reported");
        assert!(
            (pct - 64.0 * 100.0 / 194.0).abs() < 1e-9,
            "64 of 194 input tokens hit the cache, got {pct}"
        );

        let unreported = TurnMetrics::compute(
            2,
            &Usage {
                cached_input_tokens: None,
                ..usage()
            },
            None,
            None,
            Duration::ZERO,
        );
        assert_eq!(
            unreported.cache_hit_pct, None,
            "unreported cache is not a 0% cache"
        );

        let no_input = TurnMetrics::compute(
            3,
            &Usage {
                input_tokens: 0,
                cached_input_tokens: Some(0),
                ..Usage::default()
            },
            None,
            None,
            Duration::ZERO,
        );
        assert_eq!(no_input.cache_hit_pct, None, "no input → no ratio");
    }

    #[test]
    fn price_table_bills_uncached_cached_and_output_separately() {
        // 130 uncached × $0.15/M + 64 cached × $0.075/M + 2 out × $0.60/M
        // = 0.0000195 + 0.0000048 + 0.0000012 = $0.0000255
        let cost = estimate_cost(&price(), &usage());
        assert!(
            (cost - 0.0000255).abs() < 1e-12,
            "cached tokens bill at the cache rate, got {cost}"
        );

        // Without a cached rate the cached share bills at the input rate.
        let flat = estimate_cost(
            &ModelPrice {
                input_per_million: 1.0,
                output_per_million: 2.0,
                cached_input_per_million: None,
            },
            &Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                total_tokens: 2_000_000,
                cached_input_tokens: Some(400_000),
                cost_usd: None,
            },
        );
        assert!((flat - 3.0).abs() < 1e-9, "1×in + 2×out, got {flat}");
    }

    #[test]
    fn cost_prefers_the_provider_report_then_the_price_table() {
        // Provider charged it → that is the truth, table ignored.
        let charged = TurnMetrics::compute(
            1,
            &Usage {
                cost_usd: Some(0.95),
                ..usage()
            },
            Some(&price()),
            None,
            Duration::ZERO,
        );
        assert_eq!(charged.cost_usd, Some(0.95));

        // No report → estimate from the table.
        let estimated = TurnMetrics::compute(1, &usage(), Some(&price()), None, Duration::ZERO);
        assert!((estimated.cost_usd.expect("table estimate") - 0.0000255).abs() < 1e-12);

        // Neither → honest `None`, not a fabricated zero.
        let unknown = TurnMetrics::compute(1, &usage(), None, None, Duration::ZERO);
        assert_eq!(unknown.cost_usd, None);
    }
}
