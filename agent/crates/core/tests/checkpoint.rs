//! Stage 6.1: git-first checkpoints against real tempdir repositories.
//! The contract: a checkpoint snapshots tracked + untracked files (never
//! ignored ones), restore makes the worktree match the checkpoint
//! exactly, and none of it disturbs the user's branch, index, or HEAD.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use iris_core::checkpoint::{self, CheckpointError};

/// Run git in `repo` for fixture setup (with a test ident).
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

/// A fresh repo with one baseline commit: `a.txt` ("v1") committed,
/// `b.txt` untracked, `secret.env` ignored.
fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    git(repo, &["init", "-q"]);
    fs::write(repo.join("a.txt"), "v1\n").unwrap();
    fs::write(repo.join(".gitignore"), "secret.env\n").unwrap();
    fs::write(repo.join("secret.env"), "IGNORED=raw-secret\n").unwrap();
    fs::write(repo.join("b.txt"), "untracked\n").unwrap();
    git(repo, &["add", "a.txt", ".gitignore"]);
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

fn read(repo: &Path, name: &str) -> String {
    fs::read_to_string(repo.join(name)).unwrap()
}

#[test]
fn create_and_restore_roundtrip_tracked_and_untracked_only() {
    let dir = fixture_repo();
    let repo = dir.path();

    let created = checkpoint::create(repo, "before the agent").expect("create");
    assert!(created.id.starts_with("cp-"), "{}", created.id);
    assert_eq!(created.message, "before the agent");

    // Mutate everything: modify tracked, delete untracked, add new,
    // touch the ignored file.
    fs::write(repo.join("a.txt"), "v2\n").unwrap();
    fs::remove_file(repo.join("b.txt")).unwrap();
    fs::write(repo.join("d.txt"), "born after the checkpoint\n").unwrap();
    fs::write(repo.join("secret.env"), "IGNORED=changed-after\n").unwrap();

    let restored = checkpoint::restore(repo, &created.id).expect("restore");
    assert_eq!(read(repo, "a.txt"), "v1\n", "tracked file must roll back");
    assert_eq!(read(repo, "b.txt"), "untracked\n", "untracked must return");
    assert!(
        !repo.join("d.txt").exists(),
        "files born after the checkpoint must be removed"
    );
    assert_eq!(
        read(repo, "secret.env"),
        "IGNORED=changed-after\n",
        "ignored files are invisible to checkpoints — restore must not touch them"
    );
    assert_eq!(restored.written, 3, "a.txt, .gitignore, b.txt");
}

#[test]
fn list_is_newest_first_with_unique_ids() {
    let dir = fixture_repo();
    let repo = dir.path();

    let first = checkpoint::create(repo, "first").expect("create");
    let second = checkpoint::create(repo, "second").expect("create");
    assert_ne!(first.id, second.id, "ids must be unique");
    assert!(first.created_at_unix <= second.created_at_unix);

    let listed = checkpoint::list(repo).expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].message, "second", "newest first");
    assert_eq!(listed[1].message, "first");
    assert_eq!(listed[0].commit, second.commit);
}

#[test]
fn restore_unknown_id_is_a_clear_error() {
    let dir = fixture_repo();
    let err = checkpoint::restore(dir.path(), "cp-nope").expect_err("must fail");
    assert!(matches!(err, CheckpointError::NotFound(_)), "{err:?}");
}

#[test]
fn non_repo_workdir_is_a_clear_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let err = checkpoint::create(dir.path(), "m").expect_err("must fail");
    assert!(matches!(err, CheckpointError::NotARepo(_)), "{err:?}");
}

#[test]
fn create_never_disturbs_head_or_index() {
    let dir = fixture_repo();
    let repo = dir.path();
    fs::write(repo.join("a.txt"), "dirty\n").unwrap(); // workdir dirty on purpose

    let head_before = git(repo, &["rev-parse", "HEAD"]);
    let status_before = git(repo, &["status", "--porcelain"]);

    let created = checkpoint::create(repo, "non-invasive").expect("create");

    assert_eq!(git(repo, &["rev-parse", "HEAD"]), head_before, "HEAD moved");
    assert_eq!(
        git(repo, &["status", "--porcelain"]),
        status_before,
        "the real index/worktree view changed"
    );
    // And the checkpoint captured the *dirty* state, not the commit's.
    let restored = checkpoint::restore(repo, &created.id).expect("restore");
    assert_eq!(restored.written, 3);
    assert_eq!(read(repo, "a.txt"), "dirty\n");
}

#[test]
fn restore_is_byte_exact_even_when_the_repo_requests_conversion() {
    let dir = fixture_repo();
    let repo = dir.path();
    // Git for Windows and CI runners ship core.autocrlf=true; stated at
    // repo scope (plus eol=crlf, which forces the conversion on every
    // platform) so the contract is provable here, not only on Windows.
    // Checkpoints are undo snapshots of private orphan commits: restore
    // must return the captured bytes, never a line-ending policy's idea
    // of them.
    git(repo, &["config", "core.autocrlf", "true"]);
    git(repo, &["config", "core.eol", "crlf"]);

    let created = checkpoint::create(repo, "conversion-proof").expect("create");
    let blob = git(
        repo,
        &["cat-file", "blob", &format!("{}:a.txt", created.commit)],
    );
    assert_eq!(blob, "v1\n", "the checkpoint must store worktree bytes");

    fs::write(repo.join("a.txt"), "v2\r\n").unwrap();
    let restored = checkpoint::restore(repo, &created.id).expect("restore");
    assert_eq!(restored.written, 3);
    assert_eq!(
        read(repo, "a.txt"),
        "v1\n",
        "restore must not rewrite line endings"
    );
}
