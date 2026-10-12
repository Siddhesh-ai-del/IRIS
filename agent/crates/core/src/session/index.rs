//! The rebuildable session index (stage 2.4).
//!
//! A SQLite cache over the JSONL transcripts so `sessions list` can show
//! cwd, turns, tokens, cost and last-active without re-parsing every
//! transcript on every invocation. Two rules keep it honest:
//!
//! - **JSONL is the source of truth** ([`SessionIndex::refresh`] re-reads
//!   any transcript whose mtime changed, prunes rows whose transcript is
//!   gone, and a deleted `index.db` rebuilds from scratch), so the index
//!   can always be thrown away.
//! - **Cost is `NULL` unless a transcript reports one** — summed from
//!   the per-turn `usage` events since stage 3.2 (providers that never
//!   report a charge keep `NULL`, so the list shows `-`).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use rusqlite::{Connection, params};
use time::OffsetDateTime;

use crate::session::schema::Event;
use crate::session::writer::{Manifest, load, load_manifest};
use crate::types::sum_reported;

/// The index file inside the sessions directory.
pub const INDEX_FILE: &str = "index.db";

/// The index schema, applied on open. `PRAGMA user_version` is stamped
/// so a future format change can migrate instead of guessing.
const SCHEMA: &str = "
    PRAGMA user_version = 1;
    CREATE TABLE IF NOT EXISTS sessions (
        session_id     TEXT PRIMARY KEY,
        cwd            TEXT NOT NULL,
        model          TEXT NOT NULL,
        started_at_ns  INTEGER NOT NULL,
        last_active_ns INTEGER NOT NULL,
        turns          INTEGER NOT NULL,
        input_tokens   INTEGER NOT NULL,
        output_tokens  INTEGER NOT NULL,
        total_tokens   INTEGER NOT NULL,
        cost_usd       REAL
    );
    CREATE INDEX IF NOT EXISTS sessions_by_last_active
        ON sessions (last_active_ns DESC);
";

/// The index could not be read or written. The CLI treats this as a
/// cache failure: warn and fall back to listing manifests.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// The sessions directory or index file could not be accessed.
    #[error("session store io error: {0}")]
    Io(#[from] std::io::Error),
    /// SQLite could not open or query the index.
    #[error("session index database error: {0}")]
    Sql(#[from] rusqlite::Error),
}

/// One row of `sessions list`, straight from the index.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    /// Session id (the transcript's file stem).
    pub session_id: String,
    /// Absolute working directory of the session.
    pub cwd: String,
    /// OpenRouter model id.
    pub model: String,
    /// When the session was created (`session_start`).
    pub started_at: OffsetDateTime,
    /// Transcript mtime — when the session last recorded anything.
    pub last_active: OffsetDateTime,
    /// Provider responses recorded in the transcript (lifetime, across
    /// resumed runs).
    pub turns: u64,
    /// Sum of `input_tokens` across every `usage` event.
    pub input_tokens: u64,
    /// Sum of `output_tokens` across every `usage` event.
    pub output_tokens: u64,
    /// Sum of `total_tokens` across every `usage` event.
    pub total_tokens: u64,
    /// Session cost in USD — `None` when no usage event reported a
    /// charge (summed from the transcript since stage 3.2).
    pub cost_usd: Option<f64>,
}

/// The SQLite index over the sessions directory.
pub struct SessionIndex {
    conn: Connection,
    root: PathBuf,
}

impl SessionIndex {
    /// Open (creating if needed) `<root>/index.db` and apply the schema.
    pub fn open(root: &Path) -> Result<Self, IndexError> {
        fs::create_dir_all(root)?;
        let conn = Connection::open(root.join(INDEX_FILE))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn,
            root: root.to_path_buf(),
        })
    }

    /// Re-sync with the JSONL transcripts: re-index every transcript
    /// whose mtime is newer than the indexed one (or all of them when
    /// `force`), prune rows whose transcript disappeared, and return
    /// how many rows were (re)written.
    pub fn refresh(&mut self, force: bool) -> Result<usize, IndexError> {
        let mut live: Vec<String> = Vec::new();
        let mut updated = 0usize;
        for entry in fs::read_dir(&self.root)?.flatten() {
            let file_name = entry.file_name();
            let Some(name) = file_name.to_str() else {
                continue;
            };
            let Some(id) = name.strip_suffix(".jsonl").filter(|stem| !stem.is_empty()) else {
                continue;
            };
            live.push(id.to_owned());
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let mtime_ns = mtime_ns(&meta);
            if !force && self.last_indexed(id)? == Some(mtime_ns) {
                continue; // cached row still matches the transcript
            }
            if self.index_one(id, &entry.path(), mtime_ns)? {
                updated += 1;
            }
        }
        self.prune(&live)?;
        Ok(updated)
    }

    /// All indexed sessions, most recently active first (ties broken by
    /// id descending so the order is total and stable).
    pub fn list(&self) -> Result<Vec<SessionRow>, IndexError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, cwd, model, started_at_ns, last_active_ns,
                    turns, input_tokens, output_tokens, total_tokens, cost_usd
             FROM sessions
             ORDER BY last_active_ns DESC, session_id DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionRow {
                session_id: row.get(0)?,
                cwd: row.get(1)?,
                model: row.get(2)?,
                started_at: timestamp(row.get(3)?),
                last_active: timestamp(row.get(4)?),
                turns: as_count(row.get(5)?),
                input_tokens: as_count(row.get(6)?),
                output_tokens: as_count(row.get(7)?),
                total_tokens: as_count(row.get(8)?),
                cost_usd: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The mtime recorded when this transcript was last indexed, if any.
    fn last_indexed(&self, id: &str) -> Result<Option<i64>, IndexError> {
        let mut stmt = self
            .conn
            .prepare("SELECT last_active_ns FROM sessions WHERE session_id = ?1")?;
        let mut rows = stmt.query_map([id], |row| row.get::<_, i64>(0))?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Index one transcript: aggregates from its events, static-field
    /// fallback from its manifest when the transcript itself is not
    /// recoverable. Returns `false` when neither source has anything —
    /// such junk is retried on the next refresh, never indexed.
    fn index_one(
        &mut self,
        id: &str,
        transcript: &Path,
        mtime_ns: i64,
    ) -> Result<bool, IndexError> {
        let derived = load(transcript)
            .ok()
            .and_then(|events| derive(&events))
            .or_else(|| {
                load_manifest(&self.root, id)
                    .ok()
                    .map(Derived::from_manifest)
            });
        let Some(derived) = derived else {
            return Ok(false);
        };
        self.conn.execute(
            "INSERT INTO sessions (session_id, cwd, model, started_at_ns, last_active_ns,
                                   turns, input_tokens, output_tokens, total_tokens, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_id) DO UPDATE SET
                 cwd = excluded.cwd,
                 model = excluded.model,
                 started_at_ns = excluded.started_at_ns,
                 last_active_ns = excluded.last_active_ns,
                 turns = excluded.turns,
                 input_tokens = excluded.input_tokens,
                 output_tokens = excluded.output_tokens,
                 total_tokens = excluded.total_tokens,
                 cost_usd = excluded.cost_usd",
            params![
                id,
                derived.cwd,
                derived.model,
                derived.started_at.unix_timestamp_nanos() as i64,
                mtime_ns,
                derived.turns as i64,
                derived.input_tokens as i64,
                derived.output_tokens as i64,
                derived.total_tokens as i64,
                derived.cost_usd,
            ],
        )?;
        Ok(true)
    }

    /// Drop rows whose transcript no longer exists — the directory is
    /// the truth about *which* sessions there are.
    fn prune(&mut self, live: &[String]) -> Result<(), IndexError> {
        let indexed: Vec<String> = {
            let mut stmt = self.conn.prepare("SELECT session_id FROM sessions")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for id in indexed {
            if !live.contains(&id) {
                self.conn
                    .execute("DELETE FROM sessions WHERE session_id = ?1", [&id])?;
            }
        }
        Ok(())
    }
}

/// What the index knows about a session once its transcript (or, as a
/// degraded fallback, its manifest) has been read.
struct Derived {
    cwd: String,
    model: String,
    started_at: OffsetDateTime,
    turns: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cost_usd: Option<f64>,
}

impl Derived {
    /// Manifest-only view: identity without history. Aggregates are
    /// zero because a broken transcript cannot be counted — the session
    /// is still listed rather than hidden.
    fn from_manifest(manifest: Manifest) -> Self {
        Self {
            cwd: manifest.cwd,
            model: manifest.model,
            started_at: manifest.started_at,
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
        }
    }
}

/// Aggregate a transcript: static fields from the mandatory
/// `session_start`, one turn and its tokens (plus reported cost) per
/// `usage` event (a session's lifetime across resumed runs). `None`
/// unless the transcript opens with `session_start` — anything else is
/// not an IRIS transcript and must fall back to the manifest.
fn derive(events: &[Event]) -> Option<Derived> {
    let Event::SessionStart {
        started_at,
        cwd,
        model,
        ..
    } = events.first()?
    else {
        return None;
    };
    let mut derived = Derived {
        cwd: cwd.clone(),
        model: model.clone(),
        started_at: *started_at,
        turns: 0,
        input_tokens: 0,
        output_tokens: 0,
        total_tokens: 0,
        cost_usd: None,
    };
    for event in events {
        if let Event::Usage { usage, .. } = event {
            derived.turns += 1;
            derived.input_tokens += usage.input_tokens;
            derived.output_tokens += usage.output_tokens;
            derived.total_tokens += usage.total_tokens;
            derived.cost_usd = sum_reported(derived.cost_usd, usage.cost_usd);
        }
    }
    Some(derived)
}

/// The transcript's mtime as nanoseconds since the epoch — the cache
/// key *and* the session's last-active stamp (the last append is when
/// it last recorded anything).
fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|since| since.as_nanos() as i64)
        .unwrap_or(0)
}

/// Nanoseconds since the epoch to a display timestamp (second
/// precision). Out-of-range stamps read as the epoch rather than
/// failing the listing over one bad cell.
fn timestamp(ns: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(ns.div_euclid(1_000_000_000))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// A non-negative count out of SQLite (a hostile hand-edited cell can
/// be anything; the listing reports zero instead of erroring).
fn as_count(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::writer::TranscriptWriter;
    use crate::types::{Message, Role, Usage};

    fn usage((input, output, total): (u64, u64, u64)) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: total,
            ..Usage::default()
        }
    }

    /// Write a real session through the crash-safe writer: start,
    /// system, user, then one `usage` event per response.
    fn write_session(
        root: &Path,
        id: &str,
        cwd: &str,
        usages: &[(u64, u64, u64)],
    ) -> (Manifest, TranscriptWriter) {
        let manifest = Manifest::new(id, cwd, "openai/gpt-4o-mini");
        let mut writer = TranscriptWriter::create(root, &manifest).expect("create session");
        writer.append(&manifest.start_event()).expect("start line");
        writer
            .append(&Event::Message(Message::text(Role::System, "sys")))
            .expect("system line");
        writer
            .append(&Event::Message(Message::text(Role::User, "do things")))
            .expect("user line");
        for (index, tuple) in usages.iter().enumerate() {
            writer
                .append(&Event::Usage {
                    turn: (index + 1) as u32,
                    usage: usage(*tuple),
                    input_token_delta: None,
                })
                .expect("usage line");
        }
        (manifest, writer)
    }

    fn transcript_of(root: &Path, id: &str) -> PathBuf {
        root.join(format!("{id}.jsonl"))
    }

    #[test]
    fn refresh_indexes_aggregates_from_the_transcript() {
        let dir = tempfile::tempdir().expect("temp store");
        let (manifest_a, _wa) = write_session(
            dir.path(),
            "ses_a",
            "/home/u/a",
            &[(10, 5, 15), (20, 5, 25)],
        );
        let (_manifest_b, _wb) = write_session(dir.path(), "ses_b", "/home/u/b", &[(6, 4, 10)]);

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        assert_eq!(index.refresh(false).expect("refresh"), 2);

        let rows = index.list().expect("list");
        assert_eq!(rows.len(), 2);
        let a = rows
            .iter()
            .find(|r| r.session_id == "ses_a")
            .expect("row a");
        assert_eq!(a.cwd, "/home/u/a");
        assert_eq!(a.model, "openai/gpt-4o-mini");
        assert_eq!(a.turns, 2, "one row per provider response");
        assert_eq!(
            (a.input_tokens, a.output_tokens, a.total_tokens),
            (30, 10, 40)
        );
        assert_eq!(
            a.started_at.unix_timestamp(),
            manifest_a.started_at.unix_timestamp(),
            "static fields come from session_start"
        );
        let now = OffsetDateTime::now_utc().unix_timestamp();
        assert!(
            (now - a.last_active.unix_timestamp()).abs() < 300,
            "last active tracks the transcript's mtime"
        );
        assert_eq!(a.cost_usd, None, "no usage event reported a charge → NULL");
    }

    #[test]
    fn refresh_only_rewrites_changed_transcripts() {
        let dir = tempfile::tempdir().expect("temp store");
        write_session(dir.path(), "ses_a", "/w/a", &[(1, 1, 2)]);
        write_session(dir.path(), "ses_b", "/w/b", &[(1, 1, 2)]);

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        assert_eq!(index.refresh(false).expect("first"), 2, "cold index");
        assert_eq!(
            index.refresh(false).expect("second"),
            0,
            "unchanged mtimes must not be re-parsed"
        );
        assert_eq!(
            index.refresh(true).expect("forced"),
            2,
            "force re-indexes everything"
        );
    }

    #[test]
    fn refresh_picks_up_a_grown_transcript() {
        let dir = tempfile::tempdir().expect("temp store");
        let (manifest, mut writer) = write_session(dir.path(), "ses_a", "/w/a", &[(10, 5, 15)]);

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        assert_eq!(index.refresh(false).expect("first"), 1);
        assert_eq!(index.list().expect("list")[0].turns, 1);

        // The session continues (append lands, mtime moves).
        std::thread::sleep(std::time::Duration::from_millis(5));
        writer
            .append(&Event::Usage {
                turn: 2,
                usage: usage((20, 5, 25)),
                input_token_delta: None,
            })
            .expect("append");
        drop(writer);

        assert_eq!(
            index.refresh(false).expect("second"),
            1,
            "the changed transcript is re-indexed"
        );
        let rows = index.list().expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turns, 2);
        assert_eq!(rows[0].total_tokens, 40);
        let on_disk = load_manifest(dir.path(), "ses_a").expect("manifest");
        assert_eq!(manifest.session_id, on_disk.session_id);
    }

    #[test]
    fn refresh_prunes_rows_whose_transcripts_are_gone() {
        let dir = tempfile::tempdir().expect("temp store");
        write_session(dir.path(), "ses_a", "/w/a", &[(1, 1, 2)]);
        let (_, writer_b) = write_session(dir.path(), "ses_b", "/w/b", &[(1, 1, 2)]);
        drop(writer_b);

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        assert_eq!(index.refresh(false).expect("first"), 2);

        std::fs::remove_file(transcript_of(dir.path(), "ses_b")).expect("delete transcript");
        index.refresh(false).expect("prune refresh");
        let ids: Vec<String> = index
            .list()
            .expect("list")
            .into_iter()
            .map(|row| row.session_id)
            .collect();
        assert_eq!(ids, ["ses_a"], "deleted transcript must not be listed");
    }

    #[test]
    fn deleting_the_index_rebuilds_it_from_jsonl() {
        let dir = tempfile::tempdir().expect("temp store");
        write_session(dir.path(), "ses_a", "/w/a", &[(10, 5, 15)]);
        write_session(dir.path(), "ses_b", "/w/b", &[(6, 4, 10)]);

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        index.refresh(false).expect("cold");
        let before = index.list().expect("list");
        assert_eq!(before.len(), 2);

        drop(index);
        std::fs::remove_file(dir.path().join(INDEX_FILE)).expect("throw the cache away");

        let mut rebuilt = SessionIndex::open(dir.path()).expect("reopen");
        assert_eq!(
            rebuilt.refresh(false).expect("rebuild"),
            2,
            "a fresh index is rebuilt entirely from the JSONL"
        );
        assert_eq!(
            rebuilt.list().expect("list"),
            before,
            "rebuild reproduces identical rows"
        );
    }

    #[test]
    fn list_orders_by_last_active_newest_first() {
        let dir = tempfile::tempdir().expect("temp store");
        write_session(dir.path(), "ses_old", "/w", &[(1, 1, 2)]);
        std::thread::sleep(std::time::Duration::from_millis(20));
        write_session(dir.path(), "ses_new", "/w", &[(1, 1, 2)]);

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        index.refresh(false).expect("refresh");
        let ids: Vec<String> = index
            .list()
            .expect("list")
            .into_iter()
            .map(|row| row.session_id)
            .collect();
        assert_eq!(ids, ["ses_new", "ses_old"], "most recent first: {ids:?}");
    }

    #[test]
    fn an_unparseable_transcript_falls_back_to_its_manifest() {
        let dir = tempfile::tempdir().expect("temp store");
        write_session(dir.path(), "ses_ok", "/w/ok", &[(7, 3, 10)]);
        let (_, writer_bad) = write_session(dir.path(), "ses_bad", "/w/bad", &[(1, 1, 2)]);
        drop(writer_bad);
        // External corruption: the transcript is no longer recoverable,
        // but its manifest still identifies the session.
        std::fs::write(transcript_of(dir.path(), "ses_bad"), "{not json").expect("corrupt");

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        index.refresh(false).expect("refresh");
        let rows = index.list().expect("list");
        assert_eq!(
            rows.len(),
            2,
            "a corrupt transcript must not hide its session"
        );
        let bad = rows
            .iter()
            .find(|row| row.session_id == "ses_bad")
            .expect("fallback row");
        assert_eq!(bad.cwd, "/w/bad");
        assert_eq!(
            (bad.turns, bad.total_tokens),
            (0, 0),
            "aggregates need the transcript; static fields come from the manifest"
        );
        let ok = rows
            .iter()
            .find(|row| row.session_id == "ses_ok")
            .expect("healthy row");
        assert_eq!((ok.turns, ok.total_tokens), (1, 10));
    }

    /// Stage 3.2 — the reserved `cost_usd` column is filled from the
    /// per-turn usage events (OpenRouter's reported charge); a session
    /// whose provider never reported a cost keeps `NULL`, so the list
    /// keeps showing `-`.
    #[test]
    fn refresh_sums_reported_costs_into_the_row() {
        let dir = tempfile::tempdir().expect("temp store");
        for (id, costs) in [
            ("ses_paid", vec![0.0025_f64, 0.0005]),
            ("ses_unrated", Vec::new()),
        ] {
            let manifest = Manifest::new(id, "/w", "openai/gpt-4o-mini");
            let mut writer = TranscriptWriter::create(dir.path(), &manifest).expect("create");
            writer.append(&manifest.start_event()).expect("start");
            for (index, cost) in costs.into_iter().enumerate() {
                writer
                    .append(&Event::Usage {
                        turn: (index + 1) as u32,
                        usage: Usage {
                            input_tokens: 10,
                            output_tokens: 5,
                            total_tokens: 15,
                            cached_input_tokens: Some(4),
                            cost_usd: Some(cost),
                        },
                        input_token_delta: None,
                    })
                    .expect("usage line");
            }
        }

        let mut index = SessionIndex::open(dir.path()).expect("open index");
        index.refresh(false).expect("refresh");
        let rows = index.list().expect("list");
        let paid = rows
            .iter()
            .find(|row| row.session_id == "ses_paid")
            .expect("paid row");
        assert_eq!(
            paid.cost_usd,
            Some(0.0025 + 0.0005),
            "lifetime cost = sum of the per-turn charges"
        );
        let uncharged = rows
            .iter()
            .find(|row| row.session_id == "ses_unrated")
            .expect("uncharged row");
        assert_eq!(
            uncharged.cost_usd, None,
            "no usage event reported a cost → NULL"
        );
    }

    #[test]
    fn an_empty_store_lists_quietly() {
        let dir = tempfile::tempdir().expect("temp store");
        let missing = dir.path().join("no-such-store");
        let mut index = SessionIndex::open(&missing).expect("open creates the store");
        assert_eq!(index.refresh(false).expect("refresh"), 0);
        assert!(index.list().expect("list").is_empty());
    }
}
