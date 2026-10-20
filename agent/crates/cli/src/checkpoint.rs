//! `iris checkpoint create|list|restore` — git-first checkpoints
//! (stage 6.1). Thin wiring over [`iris_core::checkpoint`]: snapshot
//! the worktree as an orphan commit on a private ref, list them, put
//! the files back. Shell side-effects (services, databases, env) are
//! out of scope by design — a checkpoint is *files*.

use std::path::Path;

use iris_core::checkpoint;
use time::format_description::well_known::Rfc3339;

fn fail(err: impl std::fmt::Display) -> i32 {
    eprintln!("[iris] error: {err}");
    1
}

/// `checkpoint create -m <msg>` — snapshot the worktree.
pub fn create(workdir: &Path, message: &str) -> i32 {
    match checkpoint::create(workdir, message) {
        Ok(created) => {
            println!(
                "[iris] checkpoint {} created: {}",
                created.id, created.message
            );
            0
        }
        Err(err) => fail(err),
    }
}

/// `checkpoint list` — newest first, id first on every line.
pub fn list(workdir: &Path) -> i32 {
    match checkpoint::list(workdir) {
        Ok(items) => {
            if items.is_empty() {
                println!("[iris] no checkpoints");
                return 0;
            }
            for item in items {
                let when = time::OffsetDateTime::from_unix_timestamp(item.created_at_unix)
                    .ok()
                    .and_then(|at| at.format(&Rfc3339).ok())
                    .unwrap_or_default();
                println!("{}  {when}  {}", item.id, item.message);
            }
            0
        }
        Err(err) => fail(err),
    }
}

/// `checkpoint restore <id>` — make the worktree match exactly.
pub fn restore(workdir: &Path, id: &str) -> i32 {
    match checkpoint::restore(workdir, id) {
        Ok(done) => {
            println!(
                "[iris] restored {id} ({} files written, {} removed)",
                done.written, done.removed
            );
            0
        }
        Err(err) => fail(err),
    }
}
