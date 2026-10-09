//! Workspace path confinement (stage 1.6 — **security critical**).
//!
//! Threat model: the *model* crafts tool arguments and must never read or
//! write outside the workspace root. Two escape routes are closed:
//!
//! 1. `.`/`..` segments — handled **lexically** before touching the fs.
//!    A lexical pass is required first: `link/../x` must not depend on
//!    where `link` points.
//! 2. symlinks — handled by canonicalizing the longest *lstat-existing*
//!    prefix and requiring it to sit inside the canonical root,
//!    **component-wise** (so a root `/ws` can never prefix `/ws_evil`).
//!
//! The path handed back is the canonical prefix plus the (not-yet-existing)
//! remainder — the remainder is normalized (no `.`/`..`) and cannot contain
//! symlinks, because it does not exist yet.
//!
//! Residual risk: TOCTOU between check and use (a component swapped for a
//! symlink afterwards). Accepted for now — the adversary here is the model,
//! not a concurrent local attacker.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use super::registry::ToolError;

/// Resolve `requested` against workspace `root`, guaranteeing the result
/// stays inside `root`. Rejects with [`ToolError::PathEscape`] otherwise.
///
/// `tool` is only used for [`ToolError::Failed`] messages (missing root,
/// unresolvable symlink) so the model knows which tool broke.
pub(super) fn resolve_within(
    tool: &str,
    root: &Path,
    requested: &Path,
) -> Result<PathBuf, ToolError> {
    let canonical_root = root.canonicalize().map_err(|err| ToolError::Failed {
        tool: tool.to_string(),
        message: format!("cannot resolve workspace root {}: {err}", root.display()),
    })?;

    // Absolute inputs stand alone; relative ones anchor at the canonical
    // root. Lexical pass first — `..` must never depend on where symlinks
    // point.
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        canonical_root.join(requested)
    };
    let normalized = lexical_normalize(&joined);

    // Canonicalize the longest lstat-existing prefix. The remainder does
    // not exist yet (so it holds no symlinks) and is normalized (no `..`)
    // — reassembling the two is therefore safe.
    let (existing, missing) = split_existing_prefix(&normalized);
    let canonical_existing = existing.canonicalize().map_err(|err| ToolError::Failed {
        tool: tool.to_string(),
        message: format!("cannot resolve {}: {err}", requested.display()),
    })?;

    // Component-wise containment: `/ws` must never prefix `/ws_evil`, and
    // a symlink in the prefix resolving outside fails right here.
    if canonical_existing.strip_prefix(&canonical_root).is_err() {
        return Err(ToolError::PathEscape {
            path: requested.display().to_string(),
        });
    }

    let mut resolved = canonical_existing;
    for comp in missing {
        resolved.push(comp);
    }
    Ok(resolved)
}

/// Drop `.` segments and resolve `..` lexically. On an absolute path the
/// walk is clamped at `/` (popping the root is a no-op), so `..` can climb
/// *to* `/` but never *past* it — the escape is caught later by the
/// canonical prefix check, and on relative inputs the caller has already
/// joined the root first.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Split `path` at the longest prefix that exists **by lstat** (so even a
/// broken symlink counts as existing and is never silently *created
/// through*). Returns (existing prefix, not-yet-existing remainder).
fn split_existing_prefix(path: &Path) -> (PathBuf, Vec<OsString>) {
    let mut existing = PathBuf::new();
    let mut missing: Vec<OsString> = Vec::new();
    for comp in path.components() {
        let candidate = existing.join(comp.as_os_str());
        if missing.is_empty() && candidate.symlink_metadata().is_ok() {
            existing = candidate;
        } else {
            missing.push(comp.as_os_str().to_os_string());
        }
    }
    (existing, missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Sibling of the workspace root — everything outside must stay untouched.
    struct Fixture {
        _parent: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = tempfile::tempdir().expect("tempdir");
            let root = parent.path().join("ws");
            let outside = parent.path().join("outside");
            std::fs::create_dir(&root).expect("mkdir ws");
            std::fs::create_dir(&outside).expect("mkdir outside");
            Self {
                _parent: parent,
                root,
                outside,
            }
        }

        fn resolve(&self, requested: &str) -> Result<PathBuf, ToolError> {
            resolve_within("test", &self.root, Path::new(requested))
        }
    }

    #[test]
    fn relative_path_inside_root_resolves() {
        let fix = Fixture::new();
        std::fs::create_dir(fix.root.join("sub")).unwrap();
        std::fs::write(fix.root.join("sub/file.rs"), "x").unwrap();

        let got = fix.resolve("sub/file.rs").unwrap();
        assert_eq!(got, fix.root.join("sub/file.rs"));
    }

    #[test]
    fn not_yet_existing_path_resolves_for_writes() {
        let fix = Fixture::new();
        let got = fix.resolve("brand/new/file.txt").unwrap();
        assert_eq!(got, fix.root.join("brand/new/file.txt"));
    }

    #[test]
    fn dot_segments_are_normalized_inside_root() {
        let fix = Fixture::new();
        std::fs::create_dir(fix.root.join("sub")).unwrap();
        let got = fix.resolve("sub/../sub/./file.rs").unwrap();
        assert_eq!(got, fix.root.join("sub/file.rs"));
    }

    #[test]
    fn root_itself_is_allowed() {
        let fix = Fixture::new();
        let canonical_root = fix.root.canonicalize().unwrap();
        assert_eq!(fix.resolve("").unwrap(), canonical_root);
        assert_eq!(fix.resolve(".").unwrap(), canonical_root);
    }

    #[test]
    fn absolute_path_inside_root_is_allowed() {
        let fix = Fixture::new();
        let got = fix
            .resolve(fix.root.join("ok.txt").to_str().unwrap())
            .unwrap();
        assert_eq!(got, fix.root.join("ok.txt"));
    }

    #[test]
    fn relative_dotdot_escape_is_rejected() {
        let fix = Fixture::new();
        let err = fix.resolve("../../../etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }

    #[test]
    fn absolute_outside_path_is_rejected() {
        let fix = Fixture::new();
        let err = fix.resolve("/etc/passwd").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }

    #[test]
    fn sibling_prefix_confusion_is_rejected() {
        // `/ws_evil` shares a string prefix with `/ws` — component-wise
        // comparison must still refuse it.
        let fix = Fixture::new();
        let evil = fix.root.parent().unwrap().join(format!(
            "{}_evil",
            fix.root.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::create_dir(&evil).unwrap();
        std::fs::write(evil.join("secret.txt"), "x").unwrap();

        let err = fix.resolve("../ws_evil/secret.txt").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }

    #[test]
    fn directory_symlink_escape_is_rejected() {
        let fix = Fixture::new();
        std::fs::write(fix.outside.join("secret.txt"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&fix.outside, fix.root.join("link")).unwrap();

        let err = fix.resolve("link/secret.txt").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }

    #[test]
    fn file_symlink_escape_is_rejected() {
        let fix = Fixture::new();
        let victim = fix.outside.join("victim.txt");
        std::fs::write(&victim, "original").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&victim, fix.root.join("flink")).unwrap();

        let err = fix.resolve("flink").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }

    #[test]
    fn in_root_symlink_stays_allowed() {
        let fix = Fixture::new();
        std::fs::create_dir(fix.root.join("real")).unwrap();
        std::fs::write(fix.root.join("real/file.rs"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(fix.root.join("real"), fix.root.join("alias")).unwrap();

        let got = fix.resolve("alias/file.rs").unwrap();
        assert_eq!(got, fix.root.join("real/file.rs"));
    }

    #[test]
    fn broken_symlink_is_an_error_not_an_escape() {
        let fix = Fixture::new();
        #[cfg(unix)]
        std::os::unix::fs::symlink(fix.outside.join("nowhere"), fix.root.join("bl")).unwrap();

        let err = fix.resolve("bl").unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
    }

    #[test]
    fn missing_workspace_root_is_an_error() {
        let ghost = Path::new("/definitely/not/a/workspace");
        let err = resolve_within("test", ghost, Path::new("x")).unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
    }

    /// Property: for any generated path, resolution either fails loudly or
    /// lands component-wise inside the canonical root — never outside.
    fn arb_segments() -> impl Strategy<Value = String> {
        proptest::collection::vec(
            prop_oneof![
                Just("..".to_string()),
                Just(".".to_string()),
                Just("sub".to_string()),
                "[a-z]{1,5}",
            ],
            0..8,
        )
        .prop_map(|parts| parts.join("/"))
    }

    #[test]
    fn no_generated_path_escapes_the_root() {
        let fix = Fixture::new();
        let canonical_root = fix.root.canonicalize().unwrap();

        proptest!(|(requested in arb_segments())| {
            match fix.resolve(&requested) {
                Ok(resolved) => prop_assert!(
                    resolved.starts_with(&canonical_root),
                    "{requested} resolved to {}, outside {}",
                    resolved.display(),
                    canonical_root.display()
                ),
                Err(ToolError::PathEscape { .. }) => {}
                Err(other) => prop_assert!(false, "unexpected error for {requested}: {other}"),
            }
        });
    }
}
