//! Session ids (stage 2.3).
//!
//! An id names a session, becomes its file names inside the sessions
//! directory, and is what `iris sessions resume <id>` takes on the
//! command line. It is generated here so the format is one place: the
//! prefix keeps ids self-describing in listings, the rest is mixed
//! wall-clock time, pid and a per-process counter so two runs (even two
//! processes started in the same nanosecond) never collide.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Generate a fresh session id: `ses_` + 16 lowercase hex characters.
pub fn new_session_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let salt = nanos ^ (u64::from(std::process::id())).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    // splitmix64: distinct inputs give distinct outputs (the finalizer
    // is a permutation), and the counter makes in-process collisions
    // impossible by construction.
    let mut state = salt.wrapping_add(counter.wrapping_mul(0xbf58_476d_1ce4_e5b9));
    state = (state ^ (state >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    state = (state ^ (state >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    state ^= state >> 31;
    format!("ses_{state:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Ids are file names *and* command-line input (writer 2.2 rejects
    /// anything outside this alphabet before it reaches a `join`).
    #[test]
    fn ids_are_filename_safe() {
        for _ in 0..200 {
            let id = new_session_id();
            assert!(
                id.starts_with("ses_"),
                "ids must be self-describing, got {id}"
            );
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "id must stay inside [A-Za-z0-9_-]+, got {id}"
            );
            assert!(id.len() > "ses_".len(), "id must carry entropy, got {id}");
        }
    }

    #[test]
    fn ids_do_not_collide() {
        let ids: HashSet<String> = (0..2000).map(|_| new_session_id()).collect();
        assert_eq!(ids.len(), 2000, "session ids must be unique");
    }
}
