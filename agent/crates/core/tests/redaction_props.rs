//! Stage 5.2 properties. The corpus pins *shapes we thought of*; these
//! pin the general contract:
//!
//! 1. a planted, real-shaped secret never survives redaction, whatever
//!    text surrounds it;
//! 2. redaction never panics on arbitrary Unicode and is idempotent —
//!    so a resume replaying a scrubbed transcript re-scrubs to the
//!    same bytes.
//!
//! Planted secrets respect each rule's documented minimum body length —
//! the redactor deliberately ignores shorter near-misses (too many
//! false positives otherwise), so the property pins what it promises.

use iris_core::redact::redact_text;
use proptest::prelude::*;
use proptest::sample::select;

/// Body of a generated secret from `alphabet`, within `[min, max]`.
fn body(alphabet: &'static str, min: usize, max: usize) -> impl Strategy<Value = String> {
    let chars: Vec<char> = alphabet.chars().collect();
    prop::collection::vec(select(chars), min..=max).prop_map(|vec| vec.into_iter().collect())
}

/// One real-shaped secret per pattern family.
fn arb_secret() -> impl Strategy<Value = String> {
    const ALNUM: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";
    const LOWER: &str = "abcdefghijklmnopqrstuvwxyz0123456789";
    prop_oneof![
        body(UPPER, 16, 40).prop_map(|b| format!("AKIA{b}")),
        body(ALNUM, 36, 40).prop_map(|b| format!("ghp_{b}")),
        body(TOKEN, 40, 44).prop_map(|b| format!("github_pat_{b}")),
        body(TOKEN, 16, 40).prop_map(|b| format!("sk-ant-{b}")),
        body(ALNUM, 32, 40).prop_map(|b| format!("sk-or-v1-{b}")),
        body(TOKEN, 24, 40).prop_map(|b| format!("sk-{b}")),
        body(TOKEN, 35, 40).prop_map(|b| format!("AIza{b}")),
        body(LOWER, 4, 20).prop_map(|b| format!("PASSWORD={b}")),
        body(LOWER, 4, 20).prop_map(|b| format!("api_key: \"{b}\"")),
    ]
}

/// Ordinary surrounding text: lowercase + punctuation only, so the
/// cover can never itself be a credential shape.
fn arb_cover() -> impl Strategy<Value = String> {
    let chars: Vec<char> = " abcdefghijklmnopqrstuvwxyz.,;:()-_/\"\n\t"
        .chars()
        .collect();
    prop::collection::vec(select(chars), 0..=40).prop_map(|vec| vec.into_iter().collect::<String>())
}

proptest! {
    #[test]
    fn planted_secrets_never_survive(prefix in arb_cover(), secret in arb_secret(), suffix in arb_cover()) {
        // Spaces pin the boundary so the secret is a real occurrence.
        let input = format!("{prefix} {secret} {suffix}");
        prop_assume!(!prefix.contains(&secret) && !suffix.contains(&secret));
        let out = redact_text(&input).into_owned();
        prop_assert!(!out.contains(&secret), "raw secret survived: {out}");
    }

    #[test]
    fn redaction_never_panics_and_is_idempotent(text in prop::collection::vec(any::<char>(), 0..80)) {
        let text: String = text.into_iter().collect();
        let once = redact_text(&text).into_owned();
        let twice = redact_text(&once).into_owned();
        prop_assert_eq!(once, twice);
    }
}
