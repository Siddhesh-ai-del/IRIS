//! Stage 7.3 — the scripted E2E scenario, headless through the real
//! binary: **prompt → read → patch → checkpoint → resume → rewind**,
//! entirely against a temp git worktree and a local mock provider.
//!
//! This is the CI-safe proof that the whole pipeline works together:
//! the loop drives real tools against real files, checkpoints snapshot
//! the worktree with git-first orphan commits, resume rehydrates the
//! prior conversation verbatim and continues it, and rewind restores
//! the checkpointed state. No network, no real key, no TUI.

mod common;

use common::{iris, sse_body, text_delta, tool_delta};
use httpmock::prelude::*;
use serde_json::json;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// The file every step of the scenario acts on, and its states.
const ORIGINAL: &str = "first line\nORIGINAL_MARKER_73\nthird line\n";
const PATCHED: &str = "first line\nPATCHED_MARKER_91\nthird line\n";
const DIVERGED: &str = "DIVERGED_BY_RESUME\n";
const SCRATCH: &str = "born after the checkpoint\n";

/// The patch turn 2 applies — a real unified diff (context lines carry
/// their leading space, as unified diffs require), parsed and applied
/// by `apply_patch` exactly as stage 1.8 specifies.
const PATCH: &str = "--- a/note.txt\n+++ b/note.txt\n@@ -1,3 +1,3 @@\n\
                     \x20first line\n-ORIGINAL_MARKER_73\n+PATCHED_MARKER_91\n\x20third line\n";

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "fixture git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A temp git worktree with one committed file, ready for the agent.
fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    fs::write(repo.join("note.txt"), ORIGINAL).unwrap();
    git(repo, &["add", "note.txt"]);
    git(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "baseline",
        ],
    );
    dir
}

/// A hermetic `iris` for this scenario: fake key, mock provider, a
/// pinned sessions store, and a config that lets the headless gate
/// (ask-by-default, `DenyOnAsk`) approve the patch/write steps.
fn iris_env(base_url: &str, sessions: &Path, config: &Path) -> Command {
    let mut cmd = iris();
    cmd.env("OPENROUTER_API_KEY", "sk-or-v1-e2e-fake")
        .env("IRIS_PROVIDER_BASE_URL", base_url)
        .env("IRIS_SESSIONS_DIR", sessions)
        .env("IRIS_CONFIG", config)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null());
    cmd
}

/// The session id the run prints to stderr (`[iris] session <id>`).
fn session_id_from(stderr: &str) -> String {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("[iris] session "))
        .expect("a run must print its session id on stderr")
        .trim()
        .to_owned()
}

#[test]
fn the_scripted_scenario_runs_headless_from_prompt_to_rewind() {
    // --- fixtures: repo, sessions store, gate config --------------------
    let repo = fixture_repo();
    let workdir = repo.path().to_str().unwrap();
    let sessions = tempfile::tempdir().expect("sessions store");
    let config_dir = tempfile::tempdir().expect("config dir");
    let config = config_dir.path().join("scenario.toml");
    fs::write(
        &config,
        "[permissions]\napply_patch = \"allow\"\nwrite_file = \"allow\"\n",
    )
    .unwrap();

    let server = MockServer::start();

    // --- the scripted conversation -------------------------------------
    // First-match wins and history only grows, so mocks register from
    // the most-specific (latest) turn backwards. The resume mocks also
    // demand markers from the *first* run's transcript, so a resume
    // that forgets history gets a 404 and the scenario fails.
    let resume_final = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("DIVERGED_BY_RESUME");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &text_delta("SESSION_DIVERGED"),
                "stop",
                (9, 3, 12),
            ));
    });
    let resume_turn = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("Now diverge from the plan.")
            // Rehydration proof: the resumed request must carry the
            // prior conversation (the patch result from turn 2).
            .body_includes("applied 1");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &format!(
                    "{}{}",
                    tool_delta(
                        0,
                        "call_w1",
                        "write_file",
                        &json!({"path": "note.txt", "content": DIVERGED}),
                    ),
                    tool_delta(
                        1,
                        "call_w2",
                        "write_file",
                        &json!({"path": "scratch.txt", "content": SCRATCH}),
                    ),
                ),
                "tool_calls",
                (7, 4, 11),
            ));
    });
    let run_turn3 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            // The patch's result text, as recorded in the transcript.
            .body_includes("applied 1");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &text_delta("SCENARIO_COMPLETE"),
                "stop",
                (5, 5, 10),
            ));
    });
    let run_turn2 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            // The read_file result: turn 2 only exists because the
            // read landed in the transcript.
            .body_includes("ORIGINAL_MARKER_73");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &tool_delta(0, "call_2", "apply_patch", &json!({"patch": PATCH})),
                "tool_calls",
                (8, 6, 14),
            ));
    });
    let run_turn1 = server.mock(|when, then| {
        when.method(POST)
            .path("/chat/completions")
            .body_includes("Walk the scripted scenario.");
        then.status(200)
            .header("content-type", "text/event-stream")
            .body(sse_body(
                &format!(
                    "{}{}",
                    text_delta("Reading note.txt."),
                    tool_delta(0, "call_1", "read_file", &json!({"path": "note.txt"})),
                ),
                "tool_calls",
                (6, 4, 10),
            ));
    });

    // --- 1. prompt → read → patch through the real binary ---------------
    let out = iris_env(&server.base_url(), sessions.path(), &config)
        .args([
            "run",
            "-p",
            "Walk the scripted scenario.",
            "--workdir",
            workdir,
        ])
        .output()
        .expect("iris run must execute");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("SCENARIO_COMPLETE"), "stdout: {stdout}");
    assert_eq!(
        fs::read_to_string(repo.path().join("note.txt")).unwrap(),
        PATCHED,
        "the patch step changed the real file on disk"
    );
    let session = session_id_from(&stderr);
    run_turn1.assert();
    run_turn2.assert();
    run_turn3.assert();

    // --- 2. checkpoint the patched worktree -----------------------------
    let out = iris_env(&server.base_url(), sessions.path(), &config)
        .args([
            "checkpoint",
            "create",
            "-m",
            "after the agent patch",
            "--workdir",
            workdir,
        ])
        .output()
        .expect("iris checkpoint must execute");
    assert!(
        out.status.success(),
        "checkpoint create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = iris_env(&server.base_url(), sessions.path(), &config)
        .args(["checkpoint", "list", "--workdir", workdir])
        .output()
        .expect("iris checkpoint list must execute");
    let id = String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|line| line.contains("after the agent patch"))
        .expect("the checkpoint is listed")
        .split_whitespace()
        .next()
        .expect("id is the first token")
        .to_owned();

    // --- 3. resume: same session, prior turns replayed verbatim ---------
    let out = iris_env(&server.base_url(), sessions.path(), &config)
        .args([
            "sessions",
            "resume",
            &session,
            "-p",
            "Now diverge from the plan.",
        ])
        .output()
        .expect("iris resume must execute");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("SESSION_DIVERGED"), "stdout: {stdout}");
    assert_eq!(
        fs::read_to_string(repo.path().join("note.txt")).unwrap(),
        DIVERGED,
        "the resumed run kept acting on the same worktree"
    );
    assert!(
        repo.path().join("scratch.txt").exists(),
        "a new file after the checkpoint proves divergence"
    );
    resume_turn.assert();
    resume_final.assert();

    // --- 4. rewind: back to the checkpoint, branch untouched -------------
    let head_before = git(repo.path(), &["rev-parse", "HEAD"]);
    let out = iris_env(&server.base_url(), sessions.path(), &config)
        .args(["checkpoint", "restore", &id, "--workdir", workdir])
        .output()
        .expect("iris checkpoint restore must execute");
    assert!(
        out.status.success(),
        "checkpoint restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(repo.path().join("note.txt")).unwrap(),
        PATCHED,
        "rewind restores the checkpointed state, not the divergence"
    );
    assert!(
        !repo.path().join("scratch.txt").exists(),
        "files born after the checkpoint are removed"
    );
    assert_eq!(
        git(repo.path(), &["rev-parse", "HEAD"]),
        head_before,
        "the branch never moves — checkpoints are private refs"
    );
}
