//! Where sessions live and how they are enumerated (stage 2.3).
//!
//! The sessions root resolves from the environment only (never the
//! config file — persistence is not a tunable): `$IRIS_SESSIONS_DIR` →
//! `$XDG_DATA_HOME/iris/sessions` → `$HOME/.local/share/iris/sessions`.
//! Tests inject a temp directory through the env override.
//!
//! [`list_sessions`] is a best-effort view over the published manifests:
//! an unreadable or corrupt manifest hides that session from the listing
//! but never fails the whole listing, and never touches the transcripts
//! (the transcript is the source of truth — stage 2.4's index rebuilds
//! from it).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::session::SessionError;
use crate::session::writer::{Manifest, load_manifest};

/// Resolve the sessions directory from the environment. Empty values
/// count as unset.
pub fn sessions_root(env: &HashMap<String, String>) -> Result<PathBuf, SessionError> {
    let non_empty = |key: &str| {
        env.get(key)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };

    if let Some(explicit) = non_empty("IRIS_SESSIONS_DIR") {
        return Ok(PathBuf::from(explicit));
    }
    if let Some(xdg) = non_empty("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("iris").join("sessions"));
    }
    if let Some(home) = non_empty("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("iris")
            .join("sessions"));
    }
    Err(SessionError::NoSessionsRoot)
}

/// Every session the store currently knows about, newest first
/// (`started_at` descending, ties broken by id descending so the order
/// is total and stable). Missing directory: empty listing.
pub fn list_sessions(root: &Path) -> Vec<Manifest> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut sessions: Vec<Manifest> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".manifest.json")
        })
        .filter_map(|entry| load_manifest(root, &manifest_session_id(&entry.file_name())).ok())
        .collect();
    sessions.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.session_id.cmp(&a.session_id))
    });
    sessions
}

/// `ses_ab12.manifest.json` → `ses_ab12` (loader re-validates the id).
fn manifest_session_id(file_name: &std::ffi::OsStr) -> String {
    let name = file_name.to_string_lossy();
    name.trim_end_matches(".manifest.json").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::OffsetDateTime;

    fn ts(unix: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix).unwrap()
    }

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn manifest(id: &str, started_at: OffsetDateTime) -> Manifest {
        Manifest {
            session_id: id.into(),
            cwd: "/home/u/proj".into(),
            model: "openai/gpt-4o-mini".into(),
            started_at,
        }
    }

    // --- sessions_root --------------------------------------------------

    #[test]
    fn root_prefers_the_explicit_override() {
        let env = env(&[
            ("IRIS_SESSIONS_DIR", "/tmp/override"),
            ("XDG_DATA_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ]);
        assert_eq!(
            sessions_root(&env).unwrap(),
            PathBuf::from("/tmp/override"),
            "tests and unusual installs pin the store explicitly"
        );
    }

    #[test]
    fn root_follows_xdg_data_home() {
        let env = env(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(
            sessions_root(&env).unwrap(),
            PathBuf::from("/xdg/iris/sessions")
        );
    }

    #[test]
    fn root_falls_back_to_home_dot_local_share() {
        let env = env(&[("HOME", "/home/u")]);
        assert_eq!(
            sessions_root(&env).unwrap(),
            PathBuf::from("/home/u/.local/share/iris/sessions")
        );
    }

    #[test]
    fn root_errors_when_no_base_is_known() {
        let err = sessions_root(&env(&[])).unwrap_err();
        assert!(
            matches!(err, SessionError::NoSessionsRoot),
            "no IRIS_SESSIONS_DIR, no XDG, no HOME — nowhere to put sessions"
        );
    }

    #[test]
    fn root_ignores_empty_and_whitespace_values() {
        let env = env(&[("IRIS_SESSIONS_DIR", "  "), ("HOME", "/home/u")]);
        assert_eq!(
            sessions_root(&env).unwrap(),
            PathBuf::from("/home/u/.local/share/iris/sessions"),
            "an empty override must not downgrade to a broken path"
        );
    }

    // --- list_sessions --------------------------------------------------

    #[test]
    fn listing_a_missing_store_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_sessions(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn listing_returns_sessions_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        crate::session::writer::TranscriptWriter::create(
            dir.path(),
            &manifest("ses_old", ts(1_000)),
        )
        .unwrap();
        crate::session::writer::TranscriptWriter::create(
            dir.path(),
            &manifest("ses_new", ts(2_000)),
        )
        .unwrap();

        let listed = list_sessions(dir.path());
        let ids: Vec<&str> = listed.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(ids, ["ses_new", "ses_old"], "newest first, got {ids:?}");
    }

    #[test]
    fn listing_ties_break_by_id_descending() {
        let dir = tempfile::tempdir().unwrap();
        crate::session::writer::TranscriptWriter::create(dir.path(), &manifest("ses_a", ts(1_000)))
            .unwrap();
        crate::session::writer::TranscriptWriter::create(dir.path(), &manifest("ses_b", ts(1_000)))
            .unwrap();

        let listed = list_sessions(dir.path());
        let ids: Vec<&str> = listed.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(ids, ["ses_b", "ses_a"], "total order required, got {ids:?}");
    }

    #[test]
    fn listing_skips_corrupt_manifests_and_junk_files() {
        let dir = tempfile::tempdir().unwrap();
        crate::session::writer::TranscriptWriter::create(
            dir.path(),
            &manifest("ses_ok", ts(1_000)),
        )
        .unwrap();
        // Half-written manifest from external corruption.
        std::fs::write(dir.path().join("ses_broken.manifest.json"), "{half").unwrap();
        // Junk that must never be parsed as a manifest.
        std::fs::write(dir.path().join("notes.txt"), "hi").unwrap();
        std::fs::write(dir.path().join("ses_t.jsonl"), "{\"v\":0}").unwrap();
        std::fs::write(dir.path().join("ses_crashed.manifest.json.tmp"), "{torn").unwrap();

        let listed = list_sessions(dir.path());
        let ids: Vec<&str> = listed.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(
            ids,
            ["ses_ok"],
            "one bad manifest must not hide the rest: {ids:?}"
        );
    }
}
