//! `iris sessions` — list and resume saved sessions (stage 2.3, index
//! 2.4).
//!
//! A session is its JSONL transcript plus manifest in the sessions
//! directory (2.2). `list` reads the rebuildable sqlite index (2.4) —
//! refreshed from the JSONL on every call, degrading to manifest-only
//! rows if SQLite is unavailable; `resume` rehydrates the full
//! conversation and continues it through the same headless loop as
//! `run` — resume is rehydration, never amnesia (KQ2.2), including
//! repair of a run killed mid-tool-call.

use std::collections::HashMap;
use std::path::Path;

use iris_core::session;
use iris_core::session::index::{IndexError, SessionIndex, SessionRow};
use iris_core::session::schema::Event;
use iris_core::session::writer::{TranscriptWriter, load, load_manifest, transcript_path};
use iris_core::types::{Message, Role};
use time::format_description::well_known::Rfc3339;

use crate::run::{self, EXIT_ERROR};

/// `iris sessions list` — every saved session, newest first (by
/// last-active), from the index. One tab-separated line per session:
/// id, started_at, model, cwd, turns, total_tokens, last_active, cost.
///
/// The index is a cache, not a database of record: `refresh`
/// re-reads any transcript whose mtime moved, prunes rows whose
/// transcript is gone, and `--rebuild` forces the full re-parse. If
/// SQLite itself cannot be used, the listing degrades to manifest rows
/// (identity without history) instead of failing.
pub fn list(rebuild: bool, env: &HashMap<String, String>) -> i32 {
    let root = match session::store::sessions_root(env) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("[iris] error: {err}");
            return EXIT_ERROR;
        }
    };
    if let Err(err) = list_indexed(&root, rebuild) {
        eprintln!("[iris] warning: session index unavailable: {err}");
        list_manifests(&root);
    }
    0
}

/// The indexed path: refresh (mtime-driven, or forced) then query.
fn list_indexed(root: &Path, rebuild: bool) -> Result<(), IndexError> {
    let mut index = SessionIndex::open(root)?;
    index.refresh(rebuild)?;
    for row in index.list()? {
        print_row(&row);
    }
    Ok(())
}

/// The degraded path: manifests only — the sessions are listed, with
/// zeroed aggregates, rather than hidden because a cache broke.
fn list_manifests(root: &Path) {
    for manifest in session::store::list_sessions(root) {
        print_row(&SessionRow {
            session_id: manifest.session_id,
            cwd: manifest.cwd,
            model: manifest.model,
            started_at: manifest.started_at,
            last_active: manifest.started_at,
            turns: 0,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost_usd: None,
        });
    }
}

/// One tab-separated row; both paths print identically so a fallback
/// never changes the format.
fn print_row(row: &SessionRow) {
    let started = row
        .started_at
        .format(&Rfc3339)
        .unwrap_or_else(|_| row.started_at.to_string());
    let last = row
        .last_active
        .format(&Rfc3339)
        .unwrap_or_else(|_| row.last_active.to_string());
    let cost = row
        .cost_usd
        .map(|cost| format!("{cost:.4}"))
        .unwrap_or_else(|| "-".into());
    println!(
        "{}\t{started}\t{}\t{}\t{}\t{}\t{last}\t{cost}",
        row.session_id, row.model, row.cwd, row.turns, row.total_tokens
    );
}

/// `iris sessions resume <id> -p …` — rehydrate the transcript and
/// continue it. `workdir` and `model` default to what the session was
/// created with, so a resume restores context, not just text.
pub fn resume(
    id: &str,
    prompt: &str,
    model: Option<&str>,
    workdir: Option<&Path>,
    base_url_flag: Option<&str>,
    env: &HashMap<String, String>,
) -> i32 {
    // Locate and validate the session *before* config/key, so a bad id
    // gets the precise error and never provider setup.
    let root = match session::store::sessions_root(env) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("[iris] error: {err}");
            return EXIT_ERROR;
        }
    };
    let manifest = match load_manifest(&root, id) {
        Ok(manifest) => manifest,
        Err(err) => {
            eprintln!("[iris] error: cannot resume session {id:?}: {err}");
            return EXIT_ERROR;
        }
    };
    let path = match transcript_path(&root, id) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("[iris] error: cannot resume session {id:?}: {err}");
            return EXIT_ERROR;
        }
    };
    let events = match load(&path) {
        Ok(events) => events,
        Err(err) => {
            eprintln!("[iris] error: cannot read transcript: {err}");
            return EXIT_ERROR;
        }
    };
    let mut messages = match session::resume::rehydrate(&events) {
        Ok(messages) => messages,
        Err(err) => {
            eprintln!("[iris] error: cannot resume session {id:?}: {err}");
            return EXIT_ERROR;
        }
    };

    let workdir = workdir.unwrap_or_else(|| Path::new(&manifest.cwd));
    let model = model.unwrap_or(&manifest.model);

    let prepared = match run::prepare(workdir, base_url_flag, env) {
        Ok(prepared) => prepared,
        Err(message) => {
            eprintln!("[iris] error: {message}");
            return EXIT_ERROR;
        }
    };
    let mut writer = match TranscriptWriter::create(&root, &manifest) {
        Ok(writer) => writer,
        Err(err) => {
            eprintln!("[iris] error: cannot reopen session {id:?}: {err}");
            return EXIT_ERROR;
        }
    };

    let prior = messages.len();
    let user = Message::text(Role::User, prompt);
    if let Err(err) = writer.append(&Event::Message(user.clone())) {
        eprintln!("[iris] error: cannot record session: {err}");
        return EXIT_ERROR;
    }
    messages.push(user);
    eprintln!("[iris] resumed {id} ({prior} prior messages)");

    run::execute(prepared, model, messages, writer)
}
