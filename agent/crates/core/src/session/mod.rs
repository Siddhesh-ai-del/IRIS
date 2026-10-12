//! Session durability (phase 2): the versioned JSONL transcript schema
//! (stage 2.1), the crash-safe append writer (2.2), list/resume (2.3),
//! and the rebuildable sqlite index (2.4).

pub mod id;
pub mod index;
pub mod resume;
pub mod schema;
pub mod store;
pub mod writer;

/// Something structural is wrong with a session beyond a single file.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// No `IRIS_SESSIONS_DIR`, no `XDG_DATA_HOME`, no `HOME` — there is
    /// nowhere to put sessions.
    #[error("cannot determine the sessions directory (set IRIS_SESSIONS_DIR or HOME)")]
    NoSessionsRoot,
    /// A transcript being resumed must open with `session_start`; the
    /// first line is the session's identity.
    #[error("transcript does not begin with a session_start event")]
    NoSessionStart,
}
