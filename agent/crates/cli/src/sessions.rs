//! `iris sessions` — list and resume saved sessions (stage 2.3).
//!
//! A session is its JSONL transcript plus manifest in the sessions
//! directory (2.2). `list` reads the manifests (no transcript parsing —
//! stage 2.4 adds the rebuildable index for richer columns); `resume`
//! rehydrates the full conversation and continues it through the same
//! headless loop as `run` — resume is rehydration, never amnesia
//! (KQ2.2), including repair of a run killed mid-tool-call.

use std::collections::HashMap;
use std::path::Path;

use iris_core::session;
use iris_core::session::schema::Event;
use iris_core::session::writer::{TranscriptWriter, load, load_manifest, transcript_path};
use iris_core::types::{Message, Role};
use time::format_description::well_known::Rfc3339;

use crate::run::{self, EXIT_ERROR};

/// `iris sessions list` — every saved session, newest first. One
/// tab-separated line per session: id, started_at, model, cwd.
pub fn list(env: &HashMap<String, String>) -> i32 {
    let root = match session::store::sessions_root(env) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("[iris] error: {err}");
            return EXIT_ERROR;
        }
    };
    for manifest in session::store::list_sessions(&root) {
        let started = manifest
            .started_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| manifest.started_at.to_string());
        println!(
            "{}\t{started}\t{}\t{}",
            manifest.session_id, manifest.model, manifest.cwd
        );
    }
    0
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
