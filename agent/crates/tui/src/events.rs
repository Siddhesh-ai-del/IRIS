//! The UI-side event vocabulary (stage 4.1).
//!
//! [`AppEvent`] is the **owned** mirror of the core loop's `LoopEvent`
//! (which borrows: `TextDelta(&str)` cannot cross a channel), extended
//! with run-lifecycle events the shell synthesizes: permission asks
//! (from the stage-4.5 resolver), run end, and errors.

use iris_core::agent_loop::{LoopEvent, LoopOutcome};
use iris_core::metrics::TurnMetrics;
use iris_core::types::Usage;

/// One event for the app. Channel-safe (fully owned).
#[derive(Clone, Debug, PartialEq)]
pub enum AppEvent {
    /// A fragment of assistant text, in arrival order.
    Delta(String),
    /// About to dispatch a tool call.
    ToolStart { id: String, name: String },
    /// The call finished (failures are data, not loop errors).
    ToolEnd {
        id: String,
        name: String,
        ok: bool,
        content: String,
    },
    /// The cacheable prefix changed — any prompt cache is invalidated.
    CacheBust {
        system_changed: bool,
        tools_changed: bool,
    },
    /// This turn's measurements (stage 3.3).
    Metrics(TurnMetrics),
    /// The loop's resolver is blocked asking the user (stage 4.5).
    PermissionRequested {
        id: String,
        name: String,
        detail: String,
    },
    /// The user answered — the loop resumes (`note` is shown in the
    /// status line: "rule saved …" or the degraded-to-session note,
    /// stage 4.5).
    PermissionAnswered { note: Option<String> },
    /// The run stopped on its own terms — with its cumulative usage
    /// (the stage-4.4 budget gauge reads `total_tokens` from it).
    RunEnded { summary: String, usage: Usage },
    /// The run died (provider or transcript failure).
    Error { message: String },
}

impl From<LoopEvent<'_>> for AppEvent {
    fn from(event: LoopEvent<'_>) -> Self {
        match event {
            LoopEvent::TextDelta(text) => AppEvent::Delta(text.to_string()),
            LoopEvent::ToolStart { id, name } => AppEvent::ToolStart {
                id: id.to_string(),
                name: name.to_string(),
            },
            LoopEvent::ToolEnd { id, name, result } => AppEvent::ToolEnd {
                id: id.to_string(),
                name: name.to_string(),
                ok: !result.is_error,
                content: result.content.clone(),
            },
            LoopEvent::CacheBust {
                system_changed,
                tools_changed,
            } => AppEvent::CacheBust {
                system_changed,
                tools_changed,
            },
            LoopEvent::Metrics(metrics) => AppEvent::Metrics(metrics),
        }
    }
}

/// One-line human summary of how a run ended — the shell's status
/// line, matching the headless runner's wording.
pub fn summarize_outcome(outcome: &LoopOutcome) -> String {
    match outcome {
        LoopOutcome::Completed { turns, usage, .. } => {
            format!("completed in {turns} turns ({} tokens)", usage.total_tokens)
        }
        LoopOutcome::MaxTurns { turns, usage } => format!(
            "stopped: max_turns ({turns}) reached after {} tokens",
            usage.total_tokens
        ),
        LoopOutcome::BudgetExceeded { turns, usage } => format!(
            "stopped: token budget exceeded after {turns} turns ({} tokens)",
            usage.total_tokens
        ),
    }
}

#[cfg(test)]
mod tests {
    use iris_core::types::ToolResult;

    use super::*;

    #[test]
    fn loop_events_convert_to_owned_app_events() {
        assert_eq!(
            AppEvent::from(LoopEvent::TextDelta("hi ")),
            AppEvent::Delta("hi ".into())
        );
        assert_eq!(
            AppEvent::from(LoopEvent::ToolStart {
                id: "c1",
                name: "read_file"
            }),
            AppEvent::ToolStart {
                id: "c1".into(),
                name: "read_file".into()
            }
        );
        assert_eq!(
            AppEvent::from(LoopEvent::ToolEnd {
                id: "c1",
                name: "run_command",
                result: &ToolResult {
                    tool_use_id: "c1".into(),
                    content: "$ make\nok".into(),
                    is_error: false,
                },
            }),
            AppEvent::ToolEnd {
                id: "c1".into(),
                name: "run_command".into(),
                ok: true,
                content: "$ make\nok".into(),
            }
        );
        assert_eq!(
            AppEvent::from(LoopEvent::CacheBust {
                system_changed: false,
                tools_changed: true
            }),
            AppEvent::CacheBust {
                system_changed: false,
                tools_changed: true
            }
        );
        let metrics = TurnMetrics {
            turn: 1,
            ttft_ms: Some(9),
            tokens_per_sec: 12.5,
            cache_hit_pct: None,
            cost_usd: None,
        };
        assert_eq!(
            AppEvent::from(LoopEvent::Metrics(metrics.clone())),
            AppEvent::Metrics(metrics)
        );
    }

    #[test]
    fn summarize_outcome_matches_the_headless_wording() {
        use iris_core::types::Usage;

        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            ..Usage::default()
        };
        assert_eq!(
            summarize_outcome(&LoopOutcome::Completed {
                text: String::new(),
                turns: 3,
                usage: usage.clone(),
            }),
            "completed in 3 turns (120 tokens)"
        );
        assert_eq!(
            summarize_outcome(&LoopOutcome::MaxTurns {
                turns: 50,
                usage: usage.clone()
            }),
            "stopped: max_turns (50) reached after 120 tokens"
        );
        assert_eq!(
            summarize_outcome(&LoopOutcome::BudgetExceeded { turns: 7, usage }),
            "stopped: token budget exceeded after 7 turns (120 tokens)"
        );
    }
}
