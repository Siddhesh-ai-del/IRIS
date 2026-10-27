//! Stage 2.3 — `sessions list` / `sessions resume`, end to end through
//! the real binary, plus the transcript `run` now persists.
//!
//! The done-when test (`run_kill_resume_rehydrates_prior_turns`) is the
//! KQ2.2 fix: run, SIGKILL mid-tool-call, resume — and the *only* mock
//! that will answer the resumed request demands the prior user turn,
//! the prior assistant tool call and the interrupted-call repair in the
//! request body. Amnesia cannot pass it.

mod common;

use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{iris, sse_body, text_delta, tool_delta};
use httpmock::prelude::*;
use iris_core::session::schema::{Event, Outcome};
use iris_core::session::writer::{load, load_manifest};
use iris_core::types::{Message, Role};
use serde_json::json;

/// `[iris] session <id>` on stderr — the run's durable identity, which
/// `sessions resume` takes back on the command line.
fn session_id_from(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("[iris] session "))
        .expect("a run must print its session id on stderr")
        .trim()
        .to_owned()
}

/// `iris run` now writes a resumable transcript: the manifest, the full
/// conversation (system, user, assistant), per-turn usage, and the
/// closing `session_end` that says the run finished cleanly.
#[test]
fn run_persists_a_resumable_session_transcript() {
    let work = tempfile::tempdir().expect("temp workdir");
    std::fs::write(work.path().join("notes.txt"), "fixture\n").unwrap();
    let sessions = tempfile::tempdir().expect("temp store");

    let server = MockServer::start();
    let done = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("summarize the workspace");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("All summarized."), "stop", (6, 4, 10)));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("summarize the workspace")
        .arg("--workdir")
        .arg(work.path())
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");

    // One session: transcript + manifest, no temp litter.
    let mut names: Vec<String> = std::fs::read_dir(sessions.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 2, "transcript + manifest, got {names:?}");
    assert_eq!(names.iter().filter(|n| n.ends_with(".jsonl")).count(), 1);
    assert!(
        names.iter().all(|n| !n.ends_with(".tmp")),
        "no publish litter: {names:?}"
    );

    let id = session_id_from(&stderr);
    let cwd = std::fs::canonicalize(work.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let manifest = load_manifest(sessions.path(), &id).expect("manifest must load");
    assert_eq!(manifest.cwd, cwd);
    assert_eq!(manifest.model, "openai/gpt-4o-mini");

    let events =
        load(&sessions.path().join(format!("{id}.jsonl"))).expect("transcript must recover");
    match &events[0] {
        Event::SessionStart {
            session_id,
            cwd: start_cwd,
            model,
            ..
        } => {
            assert_eq!(session_id, &id, "manifest and transcript agree");
            assert_eq!(start_cwd, &cwd);
            assert_eq!(model, "openai/gpt-4o-mini");
        }
        other => panic!("transcript must open with session_start, got {other:?}"),
    }

    let messages: Vec<&Message> = events
        .iter()
        .filter_map(|event| match event {
            Event::Message(msg) => Some(msg),
            _ => None,
        })
        .collect();
    let roles: Vec<Role> = messages.iter().map(|m| m.role.clone()).collect();
    assert_eq!(
        roles,
        [Role::System, Role::User, Role::Assistant],
        "the whole conversation is persisted, not just the prompt"
    );
    assert!(
        matches!(
            events.last(),
            Some(Event::SessionEnd {
                outcome: Outcome::Completed,
                ..
            })
        ),
        "a clean finish is recorded, got {:?}",
        events.last()
    );
    done.assert();
}

/// `sessions list` reads the manifests: every session, newest first,
/// with enough detail to recognize one (model + cwd).
#[test]
fn sessions_list_shows_saved_sessions_newest_first() {
    let work = tempfile::tempdir().expect("temp workdir");
    let sessions = tempfile::tempdir().expect("temp store");
    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("Done."), "stop", (3, 1, 4)));
    });

    let run_once = |prompt: &str| -> String {
        let out = iris()
            .arg("run")
            .arg("-p")
            .arg(prompt)
            .arg("--workdir")
            .arg(work.path())
            .env("IRIS_SESSIONS_DIR", sessions.path())
            .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
            .env("IRIS_PROVIDER_BASE_URL", server.base_url())
            .output()
            .expect("iris binary must run");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
        session_id_from(&stderr)
    };
    let first = run_once("first prompt");
    let second = run_once("second prompt");

    let out = iris()
        .arg("sessions")
        .arg("list")
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");

    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "one line per session, got: {stdout}");
    assert!(lines[0].contains(&second), "newest first: {stdout}");
    assert!(lines[1].contains(&first), "oldest last: {stdout}");
    // The manifest records the canonical workspace root — verbatim
    // (`\\?\C:\...`) on Windows, plain elsewhere. Compare in canonical
    // space, like the run-resume test above.
    let canonical_work = work.path().canonicalize().expect("canonical workdir");
    assert!(
        lines[0].contains("openai/gpt-4o-mini")
            && lines[0].contains(canonical_work.to_str().unwrap()),
        "model and cwd make sessions recognizable (want {}): {stdout}",
        canonical_work.display()
    );
    assert_eq!(reply.calls(), 2, "both runs hit the provider");
}

/// An empty (or missing) store lists quietly: exit 0, nothing on stdout.
#[test]
fn sessions_list_on_an_empty_store_succeeds_silently() {
    let sessions = tempfile::tempdir().expect("temp store");
    let out = iris()
        .arg("sessions")
        .arg("list")
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        out.stdout.is_empty(),
        "no sessions, no stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A command long enough that the process is still mid-call when the
/// test kills it (`ping` waits ~1s per hop — cmd.exe has no `sleep`).
#[cfg(unix)]
const LONG_SHELL: &str = "sleep 20";
#[cfg(windows)]
const LONG_SHELL: &str = "ping -n 20 127.0.0.1 > nul";

/// The stage's done-when: **run, kill, resume, the model sees prior
/// turns.**
///
/// Sequence: the model asks for a long `run_command`; the tool's result
/// is fsync'd only when it finishes, so SIGKILL mid-call leaves a
/// transcript ending in an unanswered `tool_use`. Resume rehydrates the
/// history, repairs the dangling call at read time, and continues — the
/// answering mock demands every prior turn *and* the repair in the
/// request body, so a resume that forgets anything gets a 404 and a
/// non-zero exit.
#[test]
fn run_kill_resume_rehydrates_prior_turns() {
    let work = tempfile::tempdir().expect("temp workdir");
    std::fs::write(work.path().join("notes.txt"), "fixture\n").unwrap();
    let sessions = tempfile::tempdir().expect("temp store");
    // run_command is `ask` by default (denied headlessly) — this config
    // lets the long call actually start, and caps both runs at two
    // provider calls so a *broken* repair fails in seconds, not hours.
    let cfg = tempfile::NamedTempFile::new().expect("config file");
    std::fs::write(
        cfg.path(),
        "max_turns = 2\n\n[permissions]\nrun_command = \"allow\"\n",
    )
    .unwrap();

    let server = MockServer::start();
    // Registered first (first match wins): the resumed request, which
    // must carry the prior user turn, the prior assistant tool call,
    // the new prompt AND the interrupted-call repair to be answered.
    let resume_turn = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("start the sleeper") // prior user turn
            .body_includes("run_command") // prior assistant tool_use
            .body_includes("carry on with the task") // the new prompt
            .body_includes("interrupted mid-call"); // the repair
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &text_delta("Resumed and done."),
                "stop",
                (40, 6, 46),
            ));
    });
    // The killed run: the model asks for a long shell command.
    let kill_turn = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("start the sleeper");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &format!(
                    "{}{}",
                    text_delta("Kicking off the sleeper."),
                    tool_delta(0, "call_1", "run_command", &json!({"command": LONG_SHELL}))
                ),
                "tool_calls",
                (10, 5, 15),
            ));
    });

    let mut child = iris()
        .arg("run")
        .arg("-p")
        .arg("start the sleeper")
        .arg("--workdir")
        .arg(work.path())
        .env("IRIS_CONFIG", cfg.path())
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("iris run must spawn");
    let stderr_sink = Arc::new(Mutex::new(Vec::new()));
    let reader = drain_stderr(&mut child, &stderr_sink);

    // The assistant tool_use is fsync'd *before* the tool dispatch, so
    // once it is in the file the process is durably mid-call: SIGKILL
    // can no longer lose the turn.
    let transcript =
        wait_for_transcript(sessions.path(), &["run_command"], &mut child, &stderr_sink);
    child.kill().expect("kill the mid-call run");
    child.wait().expect("reap the killed run");
    reader.join().expect("stderr reader");

    // What the kill left behind: a resumable transcript with no clean
    // ending and one unanswered tool call.
    let events = load(&transcript).expect("killed transcript must recover");
    assert!(
        !events.iter().any(|e| matches!(e, Event::SessionEnd { .. })),
        "SIGKILL leaves no session_end behind"
    );
    let id = transcript
        .file_stem()
        .expect("transcript has a stem")
        .to_string_lossy()
        .into_owned();

    // Resume: the model's second turn, through the real binary.
    let out = iris()
        .arg("sessions")
        .arg("resume")
        .arg(&id)
        .arg("-p")
        .arg("carry on with the task")
        // no --workdir / --model: resume restores both from the manifest
        .env("IRIS_CONFIG", cfg.path())
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris resume must run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("Resumed and done."), "{stdout}");

    resume_turn.assert();
    kill_turn.assert();
    assert_eq!(
        kill_turn.calls(),
        1,
        "the resumed request was answered by the resume mock, not the kill mock"
    );

    // The transcript continued: same session, now cleanly finished.
    let events = load(&transcript).expect("resumed transcript must recover");
    assert!(
        matches!(
            events.last(),
            Some(Event::SessionEnd {
                outcome: Outcome::Completed,
                ..
            })
        ),
        "resume ends the session cleanly, got {:?}",
        events.last()
    );
}

/// Stage 2.4: `sessions list` reads the rebuildable sqlite index —
/// turns, tokens and last-active come from the JSONL, cost is a
/// reserved column (`-` until stage 3.2 records it).
#[test]
fn sessions_list_reports_turns_tokens_and_last_active() {
    let work = tempfile::tempdir().expect("temp workdir");
    let sessions = tempfile::tempdir().expect("temp store");
    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("Counted."), "stop", (6, 4, 10)));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("count my tokens")
        .arg("--workdir")
        .arg(work.path())
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    reply.assert();

    let listed = iris()
        .arg("sessions")
        .arg("list")
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .output()
        .expect("iris binary must run");
    assert_eq!(
        listed.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let stdout = String::from_utf8_lossy(&listed.stdout);
    let line = stdout.lines().next().expect("one session listed");
    let cols: Vec<&str> = line.split('\t').collect();
    assert_eq!(
        cols.len(),
        8,
        "id, started_at, model, cwd, turns, tokens, last_active, cost: {line}"
    );
    assert_eq!(cols[4], "1", "one provider response: {line}");
    assert_eq!(cols[5], "10", "tokens summed from the usage events: {line}");
    assert!(
        cols[6].contains('T') && cols[6].ends_with('Z'),
        "last-active as RFC3339: {line}"
    );
    assert_eq!(cols[7], "-", "cost is reserved for stage 3.2: {line}");
}

/// The index is a cache, not a database of record: deleting
/// `index.db` and re-listing rebuilds identical output from the JSONL.
#[test]
fn sessions_list_rebuilds_a_deleted_index() {
    let work = tempfile::tempdir().expect("temp workdir");
    let sessions = tempfile::tempdir().expect("temp store");
    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("Done."), "stop", (3, 2, 5)));
    });

    let out = iris()
        .arg("run")
        .arg("-p")
        .arg("warm the cache")
        .arg("--workdir")
        .arg(work.path())
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", server.base_url())
        .output()
        .expect("iris binary must run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    reply.assert();

    let list = |label: &str| -> String {
        let out = iris()
            .arg("sessions")
            .arg("list")
            .env("IRIS_SESSIONS_DIR", sessions.path())
            .output()
            .expect("iris binary must run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "{label} stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let first = list("cold");
    let index_file = sessions.path().join("index.db");
    assert!(
        index_file.exists(),
        "list maintains its cache at <store>/index.db"
    );

    std::fs::remove_file(&index_file).expect("throw the cache away");
    let second = list("rebuild");
    assert!(!first.is_empty());
    assert_eq!(
        first, second,
        "the JSONL is the source of truth: a rebuilt index lists the same"
    );
}

/// `sessions list --rebuild` forces a full re-parse regardless of the
/// cached mtimes — and changes nothing about the answer.
#[test]
fn sessions_list_rebuild_flag_forces_a_refresh() {
    let work = tempfile::tempdir().expect("temp workdir");
    let sessions = tempfile::tempdir().expect("temp store");
    let server = MockServer::start();
    let reply = server.mock(|when, then| {
        when.method(POST).path("/chat/completions");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(&text_delta("Done."), "stop", (3, 2, 5)));
    });

    for prompt in ["first prompt", "second prompt"] {
        let out = iris()
            .arg("run")
            .arg("-p")
            .arg(prompt)
            .arg("--workdir")
            .arg(work.path())
            .env("IRIS_SESSIONS_DIR", sessions.path())
            .env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
            .env("IRIS_PROVIDER_BASE_URL", server.base_url())
            .output()
            .expect("iris binary must run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(reply.calls(), 2, "both runs hit the provider");

    let list = |rebuild: bool| -> String {
        let mut command = iris();
        command.arg("sessions").arg("list");
        if rebuild {
            command.arg("--rebuild");
        }
        let out = command
            .env("IRIS_SESSIONS_DIR", sessions.path())
            .output()
            .expect("iris binary must run");
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let plain = list(false);
    let rebuilt = list(true);
    assert_eq!(plain.lines().count(), 2, "two sessions: {plain}");
    assert_eq!(
        plain, rebuilt,
        "forced re-parse from the JSONL reaches the same listing"
    );
}

/// Resuming a session that does not exist is a clean error, not a
/// panic or a new session.
#[test]
fn sessions_resume_unknown_id_fails_cleanly() {
    let sessions = tempfile::tempdir().expect("temp store");
    let out = iris()
        .arg("sessions")
        .arg("resume")
        .arg("ses_nope")
        .arg("-p")
        .arg("hi")
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("ses_nope"), "name the session: {stderr}");
}

/// A resume id is path-confinement input (writer 2.2's rule reaches the
/// CLI): `../evil` never touches the filesystem.
#[test]
fn sessions_resume_rejects_ids_that_escape_the_store() {
    let sessions = tempfile::tempdir().expect("temp store");
    let out = iris()
        .arg("sessions")
        .arg("resume")
        .arg("../evil")
        .arg("-p")
        .arg("hi")
        .env("IRIS_SESSIONS_DIR", sessions.path())
        .output()
        .expect("iris binary must run");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("invalid session id"), "{stderr}");
    let escaped = sessions
        .path()
        .parent()
        .expect("store has a parent")
        .join("evil.manifest.json");
    assert!(
        !escaped.exists(),
        "traversal must never touch the filesystem"
    );
}

/// Read a child's piped stderr in the background so a long-lived child
/// can never fill the pipe.
fn drain_stderr(child: &mut Child, sink: &Arc<Mutex<Vec<u8>>>) -> std::thread::JoinHandle<()> {
    let mut pipe = child.stderr.take().expect("stderr was piped");
    let sink = Arc::clone(sink);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        *sink.lock().unwrap() = buf;
    })
}

/// Poll the store until the single transcript contains every marker.
fn wait_for_transcript(
    store: &Path,
    markers: &[&str],
    child: &mut Child,
    stderr: &Arc<Mutex<Vec<u8>>>,
) -> std::path::PathBuf {
    for _ in 0..500 {
        let candidate = std::fs::read_dir(store)
            .ok()
            .and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .find(|entry| entry.file_name().to_string_lossy().ends_with(".jsonl"))
                    .map(|entry| entry.path())
            })
            .filter(|path| {
                std::fs::read_to_string(path)
                    .map(|text| markers.iter().all(|marker| text.contains(marker)))
                    .unwrap_or(false)
            });
        if let Some(path) = candidate {
            return path;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    let captured = String::from_utf8_lossy(&stderr.lock().unwrap()).into_owned();
    panic!("transcript never reached {markers:?}; stderr: {captured}");
}
