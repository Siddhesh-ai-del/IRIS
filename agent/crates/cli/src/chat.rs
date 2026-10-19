//! `iris chat` (stage 4.1) — the interactive TUI session.
//!
//! Same session contract as `run -p`: the fail-fast checks come
//! first, then a durable session (manifest + transcript) the loop
//! records into — so a killed chat is resumable with
//! `iris sessions resume`, exactly like a killed headless run. The
//! TUI itself lives in `iris-tui`; this module is the config/session
//! plumbing that stays on the CLI side of the layering.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use iris_core::agent_loop::LoopOptions;
use iris_core::assembly::SystemBlocks;
use iris_core::config::config_file_path;
use iris_core::provider::OpenAiClient;
use iris_core::session;
use iris_core::session::schema::Event;
use iris_core::session::writer::{Manifest, TranscriptWriter};
use iris_core::tools::ToolContext;
use iris_tui::term::Chat;

use crate::run::{self, EXIT_COMPLETED, EXIT_ERROR, SYSTEM_PROMPT};

/// Open a chat session and hand it to the TUI. Returns the process
/// exit code.
pub(crate) fn run(
    workdir: &Path,
    model: &str,
    base_url_flag: Option<&str>,
    env: &HashMap<String, String>,
) -> i32 {
    let prepared = match run::prepare(workdir, base_url_flag, env) {
        Ok(prepared) => prepared,
        Err(message) => {
            eprintln!("[iris] error: {message}");
            return EXIT_ERROR;
        }
    };
    let root = match session::store::sessions_root(env) {
        Ok(root) => root,
        Err(err) => {
            eprintln!("[iris] error: {err}");
            return EXIT_ERROR;
        }
    };

    // Open the session *before* the terminal: a session that cannot
    // be created must fail before any UI takes over the screen.
    let manifest = Manifest::new(
        session::id::new_session_id(),
        prepared.workdir.to_string_lossy().into_owned(),
        model,
    );
    let mut writer = match TranscriptWriter::create(&root, &manifest) {
        Ok(writer) => writer,
        Err(err) => {
            eprintln!("[iris] error: cannot create session: {err}");
            return EXIT_ERROR;
        }
    };
    // The same durable start as `run -p`: `session_start`, then the
    // system prompt as a named block (stage 3.1).
    let system = SystemBlocks::new()
        .with_block("core", SYSTEM_PROMPT)
        .message()
        .expect("the core system block is always present");
    for event in [manifest.start_event(), Event::Message(system.clone())] {
        if let Err(err) = writer.append(&event) {
            eprintln!("[iris] error: cannot record session start: {err}");
            return EXIT_ERROR;
        }
    }

    let run::Prepared {
        config,
        api_key,
        workdir,
        registry,
    } = prepared;
    let provider = OpenAiClient::new(&config.provider_base_url, api_key);
    let opts = LoopOptions::from_config(&config, model);
    let ctx = ToolContext::with_gate(workdir, config.permission_gate());

    match iris_tui::term::run(Chat {
        provider: Arc::new(provider),
        registry: Arc::new(registry),
        ctx,
        opts,
        writer,
        system,
        config_path: config_file_path(env),
    }) {
        Ok(()) => EXIT_COMPLETED,
        Err(message) => {
            eprintln!("[iris] error: {message}");
            EXIT_ERROR
        }
    }
}
