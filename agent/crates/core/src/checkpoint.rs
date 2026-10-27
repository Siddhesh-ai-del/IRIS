//! Git-first checkpoints (stage 6.1). A checkpoint is an **orphan
//! commit** on a private ref (`refs/iris/checkpoints/<id>`) built from
//! the worktree with pure plumbing — the user's branch, index, and
//! HEAD are never touched, and `.gitignore`d files (secrets, build
//! output) are never snapshotted. Restore checks the tree back out
//! over the worktree and removes files the checkpoint doesn't know.
//!
//! Out of scope, by design: shell side-effects (services, databases,
//! env state). A checkpoint snapshots *files*, nothing else.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use thiserror::Error;

/// Private ref namespace — invisible to normal git workflows.
pub const REF_PREFIX: &str = "refs/iris/checkpoints";

/// The all-zero sha: `update-ref`'s "expected old value" that makes
/// ref creation fail instead of overwrite when the id already exists.
const CREATE_EXPECTED_OLD: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Error)]
pub enum CheckpointError {
    /// `git` is not installed / not on PATH.
    #[error("`git` is not installed or not on PATH — git-first checkpoints need git")]
    GitMissing,
    /// The workdir is not inside a git repository.
    #[error("not a git repository: {0} — run `git init` first (checkpoints are git-first)")]
    NotARepo(PathBuf),
    /// No checkpoint with that id.
    #[error("checkpoint not found: {0}")]
    NotFound(String),
    /// A git plumbing invocation failed.
    #[error("git {0} failed: {1}")]
    Git(String, String),
    /// Filesystem trouble.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// One checkpoint as `list` reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// Short id (`cp-<unix>` or `cp-<unix>-<n>` on collision).
    pub id: String,
    /// The orphan commit's sha.
    pub commit: String,
    /// The message passed to `create`.
    pub message: String,
    /// Commit time, unix seconds.
    pub created_at_unix: i64,
}

/// What a `restore` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    /// Files written (the checkpoint tree's full file count).
    pub written: usize,
    /// Files removed because the checkpoint doesn't contain them.
    pub removed: usize,
}

/// Snapshot the worktree (tracked + untracked, ignoring the ignored)
/// as a new orphan commit. Pure plumbing: a throwaway `GIT_INDEX_FILE`
/// stages the tree, `write-tree` + `commit-tree` build a parentless
/// commit, and `update-ref` parks it under [`REF_PREFIX`] — the real
/// index, HEAD, and branch never move.
pub fn create(repo: &Path, message: &str) -> Result<Checkpoint, CheckpointError> {
    ensure_repo(repo)?;
    let index = temp_index(repo)?;
    let _ = fs::remove_file(&index);
    let outcome = create_inner(repo, &index, message);
    let _ = fs::remove_file(&index);
    outcome
}

fn create_inner(repo: &Path, index: &Path, message: &str) -> Result<Checkpoint, CheckpointError> {
    git_indexed(repo, index, &["add", "-A", "--", "."])?;
    let tree = git_indexed(repo, index, &["write-tree"])?
        .trim()
        .to_string();
    let commit = git(
        repo,
        &[
            "-c",
            "user.name=iris",
            "-c",
            "user.email=iris@localhost",
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            &tree,
            "-m",
            message,
        ],
    )?
    .trim()
    .to_string();

    let created_at_unix = time::OffsetDateTime::now_utc().unix_timestamp();
    let base = format!("cp-{created_at_unix}");
    let mut id = base.clone();
    for n in 1..=100 {
        if !ref_exists(repo, &id)? {
            git(
                repo,
                &[
                    "update-ref",
                    &format!("{REF_PREFIX}/{id}"),
                    &commit,
                    CREATE_EXPECTED_OLD,
                ],
            )?;
            return Ok(Checkpoint {
                id,
                commit,
                message: message.to_string(),
                created_at_unix,
            });
        }
        id = format!("{base}-{n}");
    }
    Err(CheckpointError::Git(
        "update-ref".to_string(),
        "could not allocate a unique checkpoint id".to_string(),
    ))
}

/// Every checkpoint, newest first (id breaks same-second ties).
pub fn list(repo: &Path) -> Result<Vec<Checkpoint>, CheckpointError> {
    ensure_repo(repo)?;
    let sep = '\u{1f}';
    let format =
        format!("%(refname){sep}%(objectname){sep}%(creatordate:unix){sep}%(contents:subject)");
    let out = git(repo, &["for-each-ref", REF_PREFIX, "--format", &format])?;
    let ref_prefix = format!("{REF_PREFIX}/");
    let mut checkpoints: Vec<Checkpoint> = out
        .lines()
        .filter_map(|line| {
            let mut parts = line.split(sep);
            let id = parts.next()?.strip_prefix(&ref_prefix)?.to_string();
            Some(Checkpoint {
                id,
                commit: parts.next()?.to_string(),
                created_at_unix: parts.next()?.parse().ok()?,
                message: parts.next().unwrap_or_default().to_string(),
            })
        })
        .collect();
    checkpoints.sort_by(|a, b| {
        b.created_at_unix
            .cmp(&a.created_at_unix)
            .then_with(|| b.id.cmp(&a.id))
    });
    Ok(checkpoints)
}

/// Make the worktree match a checkpoint exactly: force-write every
/// file in its tree, then delete present-but-absent ones. Ignored
/// files are never deleted — the checkpoint never saw them.
pub fn restore(repo: &Path, id: &str) -> Result<Restored, CheckpointError> {
    ensure_repo(repo)?;
    let commit = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{REF_PREFIX}/{id}"),
        ],
    )
    .map_err(|_| CheckpointError::NotFound(id.to_string()))?
    .trim()
    .to_string();
    let index = temp_index(repo)?;
    let _ = fs::remove_file(&index);
    let outcome = restore_inner(repo, &index, &commit);
    let _ = fs::remove_file(&index);
    outcome
}

fn restore_inner(repo: &Path, index: &Path, commit: &str) -> Result<Restored, CheckpointError> {
    // The checkpoint's exact file list.
    let listing = git(repo, &["ls-tree", "-r", "--name-only", "-z", commit])?;
    let tree: HashSet<&str> = listing.split('\0').filter(|s| !s.is_empty()).collect();

    // Populate the throwaway index from the checkpoint, then force
    // every file over the worktree (`-f` overwrites modified and
    // untracked files alike).
    git_indexed(repo, index, &["read-tree", commit])?;
    git_indexed(repo, index, &["checkout-index", "-a", "-f"])?;

    // Removals: files present now (tracked per the *real* index, plus
    // untracked non-ignored) that the checkpoint doesn't contain.
    let mut current: Vec<String> = Vec::new();
    for args in [
        vec!["ls-files", "-z"],
        vec!["ls-files", "-z", "--others", "--exclude-standard"],
    ] {
        let out = git(repo, &args)?;
        current.extend(
            out.split('\0')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
    }
    let mut removed = 0usize;
    for path in current {
        if tree.contains(path.as_str()) {
            continue;
        }
        let full = repo.join(path);
        if (full.is_file() || full.is_symlink()) && fs::remove_file(&full).is_ok() {
            removed += 1;
            prune_empty_dirs(repo, &full);
        }
    }
    Ok(Restored {
        written: tree.len(),
        removed,
    })
}

/// Best-effort: drop directories a removal left empty, stopping at the
/// repo root.
fn prune_empty_dirs(repo: &Path, removed_file: &Path) {
    let mut dir = removed_file.parent();
    while let Some(candidate) = dir {
        if candidate == repo || !candidate.starts_with(repo) {
            break;
        }
        if fs::remove_dir(candidate).is_err() {
            break;
        }
        dir = candidate.parent();
    }
}

/// One plumbing invocation: `git <args>` in `repo`, stdout captured,
/// git's own error text preserved.
fn git(repo: &Path, args: &[&str]) -> Result<String, CheckpointError> {
    git_with_env(repo, None, args)
}

/// Same, with the given env var set — the `GIT_INDEX_FILE` indirection
/// that keeps the real index untouched.
fn git_indexed(repo: &Path, index: &Path, args: &[&str]) -> Result<String, CheckpointError> {
    git_with_env(repo, Some(("GIT_INDEX_FILE", index)), args)
}

fn git_with_env(
    repo: &Path,
    env: Option<(&str, &Path)>,
    args: &[&str],
) -> Result<String, CheckpointError> {
    let mut command = Command::new("git");
    // Checkpoints are undo snapshots: restore's whole contract is that
    // it returns the captured bytes. Git's line-ending normalisation
    // (core.autocrlf — true by default in Git for Windows and on CI
    // runners) would rewrite endings on `add` and again on
    // `checkout-index`, silently changing files an undo promised to put
    // back. The checkpoint commits are private orphan refs nothing else
    // ever reads, so there is nothing to normalise *for* — pin raw
    // behaviour on every call. (A repo's own .gitattributes text rules
    // still win over config: that is the repo's declared policy.)
    command
        .current_dir(repo)
        .args(["-c", "core.autocrlf=false"])
        .args(args)
        .stdin(Stdio::null());
    if let Some((key, path)) = env {
        command.env(key, path);
    }
    let output = command.output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => CheckpointError::GitMissing,
        _ => CheckpointError::Io(err),
    })?;
    if !output.status.success() {
        return Err(CheckpointError::Git(
            args.first().copied().unwrap_or_default().to_string(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ensure_repo(repo: &Path) -> Result<(), CheckpointError> {
    match git(repo, &["rev-parse", "--git-dir"]) {
        Ok(_) => Ok(()),
        Err(CheckpointError::Git(..)) => Err(CheckpointError::NotARepo(repo.to_path_buf())),
        Err(other) => Err(other),
    }
}

fn temp_index(repo: &Path) -> Result<PathBuf, CheckpointError> {
    let git_dir = git(repo, &["rev-parse", "--absolute-git-dir"])?
        .trim()
        .to_string();
    Ok(PathBuf::from(git_dir).join("iris-checkpoint-index"))
}

fn ref_exists(repo: &Path, id: &str) -> Result<bool, CheckpointError> {
    match git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{REF_PREFIX}/{id}"),
        ],
    ) {
        Ok(_) => Ok(true),
        Err(CheckpointError::Git(..)) => Ok(false),
        Err(other) => Err(other),
    }
}
