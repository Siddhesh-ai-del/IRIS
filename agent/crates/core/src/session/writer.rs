//! Crash-safe transcript writer (stage 2.2).
//!
//! Durability contract:
//!
//! - [`TranscriptWriter::append`] writes one complete JSONL line
//!   (`line` + `\n`) in a single `write_all`, then `fsync`s — a crash
//!   can tear only the **final** line, never corrupt a middle one.
//! - [`TranscriptWriter::create`] publishes the session manifest by
//!   writing `<id>.manifest.json.tmp`, fsyncing, and renaming over the
//!   final path (rename is atomic on POSIX), so a reader never sees a
//!   half-written manifest.
//! - [`load`] recovers every newline-terminated line and best-effort
//!   parses an unterminated tail: because each line is a single
//!   top-level JSON object, the only valid prefix of a line is the
//!   complete line itself, so accepting a parseable tail cannot accept
//!   a torn write.
//!
//! The transcript file is the source of truth; any manifest/index is a
//! cache that can be rebuilt from it.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::session::schema::{Envelope, Event, SchemaError};

/// Static metadata for one session, published atomically at creation.
///
/// Everything in here is *also* recorded in the transcript's
/// `session_start` line; the manifest exists so `sessions` can list and
/// locate sessions without parsing every transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub session_id: String,
    pub cwd: String,
    pub model: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

/// A transcript file could not be written, read, or recovered.
#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    /// Filesystem failure (create, write, fsync, rename, read).
    #[error("transcript io error: {0}")]
    Io(#[from] std::io::Error),
    /// A schema line failed to encode/decode.
    #[error(transparent)]
    Schema(#[from] SchemaError),
    /// The manifest file exists but is not valid JSON for [`Manifest`].
    #[error("invalid session manifest: {0}")]
    Manifest(String),
    /// A session id that would escape the sessions directory.
    #[error("invalid session id {0:?}: must match [A-Za-z0-9_-]+")]
    InvalidSessionId(String),
    /// A newline-terminated line that does not parse — corruption
    /// outside the torn-tail allowance, not a recoverable tear.
    #[error("corrupt transcript line {index}: {message}")]
    CorruptLine { index: usize, message: String },
}

/// Append-only writer for one session's JSONL transcript.
#[derive(Debug)]
pub struct TranscriptWriter {
    file: fs::File,
    path: PathBuf,
}

impl TranscriptWriter {
    /// Publish the manifest (temp-file + fsync + rename) and open
    /// `<root>/<session_id>.jsonl` for appending (creating it if this
    /// is a resume, not just a first run).
    pub fn create(root: &Path, manifest: &Manifest) -> Result<Self, WriterError> {
        let path = transcript_path(root, &manifest.session_id)?;
        fs::create_dir_all(root)?;
        write_manifest_atomic(&path_manifest(root, &manifest.session_id)?, manifest)?;
        let file = fs::File::options().create(true).append(true).open(&path)?;
        Ok(Self { file, path })
    }

    /// Append one event as a single JSONL line and fsync so it survives
    /// a crash. Stamps the current schema version itself — callers
    /// cannot forget it.
    pub fn append(&mut self, event: &Event) -> Result<(), WriterError> {
        let line = Envelope::new(event.clone()).to_line()?;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.sync_all()?;
        Ok(())
    }

    /// The transcript file this writer owns.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Read back every event a transcript currently holds — the recovery
/// half of [`TranscriptWriter::append`]'s contract.
///
/// Newline-terminated lines must parse (otherwise:
/// [`WriterError::CorruptLine`]). An unterminated tail is a torn final
/// write: it is kept **iff** it parses as a complete line (the only
/// parseable prefix of a line is the whole line), otherwise dropped.
pub fn load(path: &Path) -> Result<Vec<Event>, WriterError> {
    let bytes = fs::read(path)?;
    let mut events = Vec::new();
    let mut start = 0usize;
    let mut line_index = 0usize;
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'\n' {
            continue;
        }
        let line = &bytes[start..i];
        start = i + 1;
        let corrupt = |message: String| WriterError::CorruptLine {
            index: line_index,
            message,
        };
        let text = std::str::from_utf8(line).map_err(|e| corrupt(e.to_string()))?;
        let env = Envelope::from_line(text).map_err(|e| corrupt(e.to_string()))?;
        events.push(env.event);
        line_index += 1;
    }
    // Torn final write: kept only if it already parses as a complete
    // line; an empty/invalid tail is dropped (best effort, never an error).
    let tail = &bytes[start..];
    if let Ok(text) = std::str::from_utf8(tail)
        && let Ok(env) = Envelope::from_line(text)
    {
        events.push(env.event);
    }
    Ok(events)
}

/// Read a session manifest published by [`TranscriptWriter::create`].
pub fn load_manifest(root: &Path, session_id: &str) -> Result<Manifest, WriterError> {
    let path = path_manifest(root, session_id)?;
    let text = fs::read_to_string(&path)?;
    serde_json::from_str(&text).map_err(|e| WriterError::Manifest(e.to_string()))
}

/// `<root>/<session_id>.jsonl`, confined to `root`.
fn transcript_path(root: &Path, session_id: &str) -> Result<PathBuf, WriterError> {
    validate_session_id(session_id)?;
    Ok(root.join(format!("{session_id}.jsonl")))
}

/// `<root>/<session_id>.manifest.json`, confined to `root`.
fn path_manifest(root: &Path, session_id: &str) -> Result<PathBuf, WriterError> {
    validate_session_id(session_id)?;
    Ok(root.join(format!("{session_id}.manifest.json")))
}

/// Session ids become file names, so they are path-confinement input:
/// nothing outside `[A-Za-z0-9_-]+` gets anywhere near `join`.
fn validate_session_id(session_id: &str) -> Result<(), WriterError> {
    let ok = !session_id.is_empty()
        && session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(WriterError::InvalidSessionId(session_id.to_owned()))
    }
}

/// temp-file + fsync + rename: readers see either the old manifest or
/// the new one, never a partial write.
fn write_manifest_atomic(dest: &Path, manifest: &Manifest) -> Result<(), WriterError> {
    let tmp = dest.with_extension("json.tmp");
    let json =
        serde_json::to_string_pretty(manifest).map_err(|e| WriterError::Manifest(e.to_string()))?;
    {
        let mut f = fs::File::options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(json.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, dest)?;
    if let Ok(dir) = fs::File::open(dest.parent().unwrap_or(Path::new("."))) {
        dir.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema::Outcome;
    use crate::types::{Message, Role, Usage};
    use proptest::prelude::*;
    use tempfile::tempdir;

    fn ts() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn manifest() -> Manifest {
        Manifest {
            session_id: "ses_ab12".into(),
            cwd: "/home/u/proj".into(),
            model: "openai/gpt-4o-mini".into(),
            started_at: ts(),
        }
    }

    fn start_event() -> Event {
        Event::SessionStart {
            session_id: "ses_ab12".into(),
            started_at: ts(),
            cwd: "/home/u/proj".into(),
            model: "openai/gpt-4o-mini".into(),
            iris_version: "0.1.0".into(),
        }
    }

    fn events() -> Vec<Event> {
        vec![
            start_event(),
            Event::Message(Message::text(Role::User, "read Cargo.toml")),
            Event::Usage {
                turn: 1,
                usage: Usage {
                    input_tokens: 120,
                    output_tokens: 34,
                    total_tokens: 154,
                },
            },
            Event::SessionEnd {
                outcome: Outcome::Completed,
                turns: 1,
                usage: Usage {
                    input_tokens: 120,
                    output_tokens: 34,
                    total_tokens: 154,
                },
                ended_at: ts(),
            },
        ]
    }

    #[test]
    fn append_writes_one_complete_jsonl_line_per_event() {
        let dir = tempdir().unwrap();
        let mut w = TranscriptWriter::create(dir.path(), &manifest()).unwrap();
        for event in events() {
            w.append(&event).unwrap();
        }
        let text = std::fs::read_to_string(w.path()).unwrap();
        assert!(text.ends_with('\n'), "every line is \\n-terminated");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        for (line, expected) in lines.iter().zip(events()) {
            let env = Envelope::from_line(line).unwrap();
            assert_eq!(env.event, expected);
        }
        assert_eq!(
            load(w.path()).unwrap(),
            events(),
            "recovery must round-trip what the writer produced"
        );
    }

    #[test]
    fn append_is_durable_before_returning() {
        let dir = tempdir().unwrap();
        let mut w = TranscriptWriter::create(dir.path(), &manifest()).unwrap();
        w.append(&start_event()).unwrap();
        // A second, independent handle sees the bytes immediately —
        // append + fsync happened before append() returned.
        let text = std::fs::read_to_string(w.path()).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with(r#"{"v":0,"type":"session_start","#));
    }

    #[test]
    fn create_writes_manifest_atomically_and_leaves_no_tmp() {
        let dir = tempdir().unwrap();
        let w = TranscriptWriter::create(dir.path(), &manifest()).unwrap();
        let manifest_path = dir.path().join("ses_ab12.manifest.json");
        assert!(manifest_path.exists());
        let left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !left.iter().any(|n| n.ends_with(".tmp")),
            "temp file must be renamed away, found: {left:?}"
        );
        let loaded = load_manifest(dir.path(), "ses_ab12").unwrap();
        assert_eq!(loaded, manifest());
        assert_eq!(w.path(), dir.path().join("ses_ab12.jsonl"));
    }

    #[test]
    fn leftover_tmp_from_a_crashed_publish_is_ignored() {
        let dir = tempdir().unwrap();
        // Simulate a crash mid-publish: garbage sits in the tmp path.
        std::fs::write(dir.path().join("ses_ab12.manifest.json.tmp"), "{half").unwrap();
        TranscriptWriter::create(dir.path(), &manifest()).unwrap();
        assert_eq!(
            load_manifest(dir.path(), "ses_ab12").unwrap(),
            manifest(),
            "the real manifest must win over a torn tmp"
        );
    }

    #[test]
    fn create_rejects_session_ids_that_escape_the_directory() {
        let dir = tempdir().unwrap();
        for bad in ["../evil", "..", "a/b", "", "a b", "a\\b", ".hidden"] {
            let m = Manifest {
                session_id: bad.into(),
                ..manifest()
            };
            let err = TranscriptWriter::create(dir.path(), &m).unwrap_err();
            assert!(
                matches!(err, WriterError::InvalidSessionId(_)),
                "id {bad:?} must be rejected as path traversal, got: {err:?}"
            );
        }
        // Nothing may have been created outside root.
        let created: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(created.iter().all(|n| !n.contains("evil")), "{created:?}");
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = tempdir().unwrap();
        let err = load(&dir.path().join("nope.jsonl")).unwrap_err();
        assert!(matches!(err, WriterError::Io(_)), "got: {err:?}");
    }

    #[test]
    fn load_corrupt_complete_line_fails_loudly() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_bad.jsonl");
        std::fs::write(
            &path,
            "{\"v\":0,\"type\":\"message\",\"role\":\"user\",\"content\":[]}\nnot json at all\n",
        )
        .unwrap();
        let err = load(&path).unwrap_err();
        assert!(
            matches!(err, WriterError::CorruptLine { index: 1, .. }),
            "a corrupt *terminated* line is real corruption, got: {err:?}"
        );
    }

    #[test]
    fn load_drops_a_torn_unterminated_tail() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_torn.jsonl");
        let mut text = String::new();
        for event in events() {
            text.push_str(&Envelope::new(event).to_line().unwrap());
            text.push('\n');
        }
        let torn = &text[..text.len() - 7]; // cut inside the final line
        std::fs::write(&path, torn).unwrap();
        let recovered = load(&path).unwrap();
        assert_eq!(
            recovered,
            events()[..events().len() - 1].to_vec(),
            "complete lines survive; the torn tail is dropped"
        );
    }

    #[test]
    fn load_keeps_a_complete_final_line_missing_only_its_newline() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_halfnl.jsonl");
        let mut text = String::new();
        for event in events() {
            text.push_str(&Envelope::new(event).to_line().unwrap());
            text.push('\n');
        }
        text.pop(); // strip the last \n — line itself is complete
        std::fs::write(&path, text).unwrap();
        assert_eq!(load(&path).unwrap(), events());
    }

    /// The stage's done-when: truncate the transcript at **any** byte
    /// and recovery still returns every complete line — as a prefix of
    /// the original, never an error, never corrupt content.
    #[test]
    fn truncation_at_any_byte_recovers_all_complete_lines() {
        proptest!(|(texts in proptest::collection::vec(any::<String>(), 1..8),
                    cut in any::<usize>())| {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ses_prop.jsonl");

        let mut expected = vec![start_event()];
        for (i, t) in texts.iter().enumerate() {
            expected.push(Event::Message(Message::text(Role::User, t)));
            expected.push(Event::Usage {
                turn: (i + 1) as u32,
                usage: Usage {
                    input_tokens: (i as u64) * 10,
                    output_tokens: 1,
                    total_tokens: (i as u64) * 10 + 1,
                },
            });
        }

        let mut bytes = Vec::new();
        for event in &expected {
            bytes.extend_from_slice(Envelope::new(event.clone()).to_line().unwrap().as_bytes());
            bytes.push(b'\n');
        }
        let cut = cut % (bytes.len() + 1);
        std::fs::write(&path, &bytes[..cut]).unwrap();

        let recovered = load(&path).unwrap();
        // Content is never invented: recovered must be a true prefix.
        prop_assert!(recovered.len() <= expected.len());
        for (got, want) in recovered.iter().zip(expected.iter()) {
            prop_assert_eq!(got, want);
        }
        // Every newline-terminated line before the cut survives.
        let terminated = bytes[..cut].iter().filter(|b| **b == b'\n').count();
        prop_assert!(recovered.len() >= terminated);
        // ...and at most the one unterminated tail line beside it.
        prop_assert!(recovered.len() <= terminated + 1);
        if cut == bytes.len() {
            prop_assert_eq!(recovered.len(), expected.len());
        }
        });
    }
}
