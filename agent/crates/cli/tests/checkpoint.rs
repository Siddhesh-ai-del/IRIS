//! Stage 6.1 CLI surface: `iris checkpoint create|list|restore`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// A repo with one committed file, ready for checkpointing.
fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    fs::write(repo.join("a.txt"), "v1\n").unwrap();
    git(repo, &["add", "a.txt"]);
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

fn iris(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_iris"))
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::null())
        .output()
        .expect("iris runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn create_list_restore_roundtrip() {
    let dir = fixture_repo();
    let repo = dir.path();
    let workdir = repo.to_str().unwrap();

    // create
    let (ok, stdout, stderr) = iris(&[
        "checkpoint",
        "create",
        "-m",
        "before agent",
        "--workdir",
        workdir,
    ]);
    assert!(ok, "create failed: {stderr}");
    assert!(
        stdout.contains("before agent") && stdout.contains("checkpoint"),
        "{stdout}"
    );

    // The checkpoint exists in the private ref namespace…
    let refs = git(repo, &["for-each-ref", "refs/iris/checkpoints"]);
    assert_eq!(refs.trim().split('\n').count(), 1, "one checkpoint ref");

    // …and the user's HEAD never moved.
    let head_before = git(repo, &["rev-parse", "HEAD"]);

    // list: id is the first token of the line carrying the message
    let (ok, stdout, stderr) = iris(&["checkpoint", "list", "--workdir", workdir]);
    assert!(ok, "list failed: {stderr}");
    let line = stdout
        .lines()
        .find(|l| l.contains("before agent"))
        .expect("list shows the checkpoint");
    let id = line.split_whitespace().next().expect("id is first token");

    // mutate, then restore
    fs::write(repo.join("a.txt"), "agent broke this\n").unwrap();
    fs::write(repo.join("junk.txt"), "born after\n").unwrap();
    let (ok, stdout, stderr) = iris(&["checkpoint", "restore", id, "--workdir", workdir]);
    assert!(ok, "restore failed: {stderr}");
    assert!(stdout.contains("restored"), "{stdout}");
    assert_eq!(
        fs::read_to_string(repo.join("a.txt")).unwrap(),
        "v1\n",
        "restore must roll the file back"
    );
    assert!(
        !repo.join("junk.txt").exists(),
        "restore must remove files the checkpoint doesn't know"
    );
    assert_eq!(git(repo, &["rev-parse", "HEAD"]), head_before);
}

#[test]
fn errors_are_clear_and_exit_nonzero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let workdir = dir.path().to_str().unwrap();

    // Not a repo → nonzero + a message that says git init.
    let (ok, _stdout, stderr) = iris(&["checkpoint", "create", "-m", "x", "--workdir", workdir]);
    assert!(!ok, "non-repo must fail");
    assert!(
        stderr.contains("git init") || stderr.contains("not a git repository"),
        "{stderr}"
    );

    // Unknown checkpoint id → nonzero + NotFound text.
    let dir = fixture_repo();
    let workdir = dir.path().to_str().unwrap();
    let (ok, _stdout, stderr) = iris(&["checkpoint", "restore", "cp-nope", "--workdir", workdir]);
    assert!(!ok, "unknown id must fail");
    assert!(stderr.contains("checkpoint not found"), "{stderr}");
}
