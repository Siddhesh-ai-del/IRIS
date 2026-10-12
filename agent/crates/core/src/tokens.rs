//! Token counting (stage 3.2) — tiktoken for OpenAI-accurate counts,
//! reconciled against what the provider actually reports.
//!
//! Two honest limits, by design:
//!
//! - **Tokenizer**: `cl100k_base` is OpenAI-accurate for the GPT-4
//!   family; OpenRouter routes to many models with their own BPEs. A
//!   mismatch is not silent — it shows up as a recorded delta.
//! - **Template**: the estimate counts the canonical request prefix
//!   from stage 3.1 (the exact bytes we send), *not* the provider's
//!   internal chat template. The difference is exactly what the
//!   per-turn reconciliation records.

use std::sync::OnceLock;

use tiktoken_rs::CoreBPE;

use crate::provider::CompletionRequest;

/// The counting encoding: `cl100k_base`, loaded once.
fn encoding() -> &'static CoreBPE {
    static ENCODING: OnceLock<CoreBPE> = OnceLock::new();
    ENCODING.get_or_init(|| tiktoken_rs::cl100k_base().expect("cl100k_base ships with the crate"))
}

/// Count `text` with the default encoding (special tokens counted as
/// the model sees them).
pub fn count_text(text: &str) -> u64 {
    encoding().encode_with_special_tokens(text).len() as u64
}

/// Estimate the input tokens of a request: the canonical request
/// prefix from stage 3.1 — the exact bytes that go to the provider —
/// counted with [`count_text`].
pub fn estimate_input(request: &CompletionRequest) -> u64 {
    count_text(&crate::assembly::prefix_bytes(request))
}

/// Reconcile estimated against actual: **actual − estimate**.
/// Positive means the provider counted more than we did (chat
/// template, hidden tokens, tokenizer mismatch); negative means we
/// overestimated.
pub fn reconcile(estimated: u64, actual: u64) -> i64 {
    actual as i64 - estimated as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, Role};

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(count_text(""), 0);
    }

    /// cl100k_base goldens: "hello world" is two merges, "Hello, world!"
    /// four. If these ever change the whole accounting layer drifts —
    /// fail loud, not silently.
    #[test]
    fn counts_strings_with_cl100k_base() {
        assert_eq!(count_text("hello world"), 2);
        assert_eq!(count_text("Hello, world!"), 4);
        assert!(
            count_text("irreverent-AI-coding-agent") > 4,
            "a long hyphenated word is many BPE merges, got {}",
            count_text("irreverent-AI-coding-agent")
        );
    }

    /// The estimate is a count over the canonical prefix bytes — same
    /// session, same config, same number, every time.
    #[test]
    fn estimate_counts_the_canonical_prefix() {
        let request = CompletionRequest {
            model: "openai/gpt-4o-mini".into(),
            messages: vec![
                Message::text(Role::System, "You are iris."),
                Message::text(Role::User, "list the files"),
            ],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        };
        let expected = count_text(&crate::assembly::prefix_bytes(&request));
        assert_eq!(estimate_input(&request), expected);
        assert_eq!(
            estimate_input(&request),
            estimate_input(&request),
            "estimation is deterministic"
        );
        assert!(
            expected > 10,
            "the fixture prefix is nontrivial: {expected}"
        );
    }

    #[test]
    fn reconcile_is_actual_minus_estimate() {
        assert_eq!(reconcile(100, 125), 25, "provider counted more");
        assert_eq!(reconcile(100, 80), -20, "we overestimated");
        assert_eq!(reconcile(50, 50), 0, "exact match");
    }
}
