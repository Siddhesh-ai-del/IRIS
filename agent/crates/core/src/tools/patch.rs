//! `apply_patch` tool (stage 1.8 — **high risk**): the model emits a
//! unified diff, the tool applies it with `diffy` and only then writes.
//!
//! Targets KQ2.4 (edit-apply pain):
//! - **line offsets are tolerated** — diffy searches around the declared
//!   position for the hunk's content, so stale line numbers (files
//!   changed above the edit) still apply;
//! - **context must match** — a hunk whose context no longer exists
//!   cannot silently land in the wrong place;
//! - on apply failure the optional `new_content` argument provides a
//!   **whole-file write fallback**, reported loudly in the result;
//! - a *malformed* patch is rejected **before any fs write** — the file
//!   is never touched (golden tests pin this).
//!
//! The target path comes from the patch headers (`+++`, falling back to
//! `---`), git-style `a/`/`b/` prefixes are stripped, and the path goes
//! through the stage-1.6 confinement — a patch cannot escape the
//! workspace.

use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;

use super::path::resolve_within;
use super::registry::{Tool, ToolContext, ToolError};
use crate::types::ToolResult;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApplyPatchArgs {
    /// Unified diff (`--- a/path`, `+++ b/path`, `@@` hunks) describing
    /// the edit. Line offsets are tolerated; context lines must match.
    patch: String,
    /// Full new contents of the target file — used **only** when the
    /// patch parses but fails to apply: the file is then written whole
    /// and the result says so. Never used for malformed patches.
    #[serde(default)]
    new_content: Option<String>,
}

pub struct ApplyPatchTool;

impl Tool for ApplyPatchTool {
    type Args = ApplyPatchArgs;

    fn name(&self) -> &'static str {
        "apply_patch"
    }

    fn description(&self) -> &'static str {
        "Applies a unified diff to a workspace file (line offsets \\\n         tolerated; on apply failure an optional new_content is written \\\n         whole instead)"
    }

    async fn execute(
        &self,
        ctx: &ToolContext,
        args: ApplyPatchArgs,
    ) -> Result<ToolResult, ToolError> {
        // Parse first — a malformed patch never reaches the fs.
        let patch = diffy::Patch::from_str(&args.patch).map_err(|err| ToolError::Failed {
            tool: self.name().to_string(),
            message: format!("malformed patch: {err}"),
        })?;
        let requested = target_of(&patch).map_err(|message| ToolError::Failed {
            tool: self.name().to_string(),
            message,
        })?;
        // Confinement: a patch header cannot escape the workspace.
        let path = resolve_within(self.name(), &ctx.workspace_root, Path::new(requested))?;

        let original = std::fs::read_to_string(&path).map_err(|err| {
            let hint = if err.kind() == std::io::ErrorKind::NotFound {
                " — create it with write_file first"
            } else {
                ""
            };
            ToolError::Failed {
                tool: self.name().to_string(),
                message: format!("cannot read {requested}: {err}{hint}"),
            }
        })?;

        let updated = match diffy::apply(&original, &patch) {
            Ok(updated) => updated,
            Err(apply_err) => match args.new_content.as_deref() {
                Some(intended) => {
                    // Whole-file fallback: the model supplied the
                    // intended final state — write it, and say so loudly.
                    std::fs::write(&path, intended).map_err(|err| ToolError::Failed {
                        tool: self.name().to_string(),
                        message: format!("cannot write {requested}: {err}"),
                    })?;
                    return Ok(ToolResult {
                        tool_use_id: String::new(),
                        content: format!(
                            "patch failed to apply ({apply_err}) — wrote {} bytes to \
                             {requested} from new_content instead (whole file fallback)",
                            intended.len()
                        ),
                        is_error: false,
                    });
                }
                None => {
                    return Err(ToolError::Failed {
                        tool: self.name().to_string(),
                        message: format!(
                            "{apply_err} — patch did not apply to {requested}; pass \
                             new_content to fall back to a whole-file write, or retry \
                             with write_file"
                        ),
                    });
                }
            },
        };

        std::fs::write(&path, &updated).map_err(|err| ToolError::Failed {
            tool: self.name().to_string(),
            message: format!("cannot write {requested}: {err}"),
        })?;

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: format!(
                "applied {} hunks to {requested} ({} → {} bytes)",
                patch.hunks().len(),
                original.len(),
                updated.len()
            ),
            is_error: false,
        })
    }
}

/// The file a parsed patch targets: `+++` (new name) with `---` (old
/// name) as fallback. Git-style `a/`/`b/` prefixes are stripped;
/// `/dev/null` means deletion, which this tool does not do.
fn target_of<'a>(patch: &'a diffy::Patch<'a, str>) -> Result<&'a str, String> {
    let raw = patch
        .modified()
        .or_else(|| patch.original())
        .ok_or_else(|| "patch has no ---/+++ file headers".to_string())?;
    if raw == "/dev/null" {
        return Err("deleting files via apply_patch is not supported".to_string());
    }
    Ok(raw
        .strip_prefix("b/")
        .or_else(|| raw.strip_prefix("a/"))
        .unwrap_or(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;
    use serde_json::json;
    use std::path::PathBuf;

    /// The file every golden test starts from — and must either change
    /// exactly as the patch says or stay **byte-identical**.
    const ORIGINAL: &str = "line one\nline two\nline three\n";

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: ToolContext,
        target: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let target = dir.path().join("note.txt");
            std::fs::write(&target, ORIGINAL).unwrap();
            Self {
                ctx: ToolContext::new(dir.path()),
                target,
                _dir: dir,
            }
        }

        fn golden(&self) {
            assert_eq!(
                std::fs::read_to_string(&self.target).unwrap(),
                ORIGINAL,
                "file must be byte-identical (never silent corruption)"
            );
        }
    }

    async fn apply(
        fixture: &Fixture,
        patch: &str,
        new_content: Option<&str>,
    ) -> Result<ToolResult, ToolError> {
        let mut registry = ToolRegistry::new();
        registry.register(ApplyPatchTool);
        registry
            .execute(
                "apply_patch",
                &fixture.ctx,
                "call_1",
                json!({"patch": patch, "new_content": new_content}),
            )
            .await
    }

    fn git_patch(old_line: &str, new_line: &str, at: &str) -> String {
        format!(
            "--- a/note.txt\n+++ b/note.txt\n@@ {at} @@\n line one\n-{old_line}\n+{new_line}\n line three\n"
        )
    }

    // --- happy paths ----------------------------------------------------

    #[tokio::test]
    async fn git_style_hunk_applies_exactly() {
        let fix = Fixture::new();
        let patch = git_patch("line two", "LINE TWO", "-1,3 +1,3");
        let result = apply(&fix, &patch, None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&fix.target).unwrap(),
            "line one\nLINE TWO\nline three\n"
        );
        assert!(result.content.contains("applied 1"), "{:?}", result.content);
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn wrong_line_numbers_still_apply_via_offset_search() {
        // The edit-apply painkiller: the hunk is declared at lines 1-3
        // but its content actually lives at lines 3-5.
        let fix = Fixture::new();
        std::fs::write(&fix.target, "a\nb\nc\nd\ne\n").unwrap();
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1,3 +1,3 @@\n c\n d\n-e\n+f\n";
        let result = apply(&fix, patch, None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&fix.target).unwrap(),
            "a\nb\nc\nd\nf\n"
        );
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn multi_hunk_patch_applies_fully() {
        let fix = Fixture::new();
        std::fs::write(&fix.target, "a\nb\nc\nd\ne\nf\n").unwrap();
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -4,3 +4,3 @@\n d\n-e\n+E\n f\n";
        let result = apply(&fix, patch, None).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(&fix.target).unwrap(),
            "a\nB\nc\nd\nE\nf\n"
        );
        assert!(result.content.contains("applied 2"), "{:?}", result.content);
    }

    // --- failure paths: never silent corruption -------------------------

    #[tokio::test]
    async fn apply_failure_without_fallback_leaves_file_byte_identical() {
        let fix = Fixture::new();
        // Context claims "line TWO" (different case) — will never match.
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1,3 +1,3 @@\n line one\n-line TWO\n+CHANGED\n line three\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
        assert!(
            err.to_string().contains("new_content"),
            "hint the fallback: {err}"
        );
        fix.golden();
    }

    #[tokio::test]
    async fn apply_failure_falls_back_to_whole_file_write_loudly() {
        let fix = Fixture::new();
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1,3 +1,3 @@\n line one\n-line TWO\n+CHANGED\n line three\n";
        let intended = "the complete intended file\n";
        let result = apply(&fix, patch, Some(intended)).await.unwrap();
        assert_eq!(std::fs::read_to_string(&fix.target).unwrap(), intended);
        assert!(!result.is_error, "the fallback recovered the edit");
        assert!(
            result.content.contains("failed to apply") && result.content.contains("whole file"),
            "fallback must be loud: {:?}",
            result.content
        );
    }

    #[tokio::test]
    async fn malformed_patch_is_rejected_without_touching_the_file() {
        let fix = Fixture::new();
        let err = apply(&fix, "just some prose, not a diff\n", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
        fix.golden();
    }

    #[tokio::test]
    async fn garbage_hunk_header_is_rejected_without_touching_the_file() {
        let fix = Fixture::new();
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ total nonsense @@\n line one\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
        fix.golden();
    }

    #[tokio::test]
    async fn malformed_patch_never_uses_the_fallback() {
        // new_content must only rescue *apply* failures — a malformed
        // patch is the model's bug and fails loudly.
        let fix = Fixture::new();
        let err = apply(&fix, "not a diff", Some("intended\n"))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
        fix.golden();
    }

    // --- security & unsupported shapes ----------------------------------

    #[tokio::test]
    async fn path_escape_via_patch_header_is_rejected() {
        let fix = Fixture::new();
        let outside = fix.target.parent().unwrap().join("outside.txt");
        std::fs::write(&outside, "precious").unwrap();
        let patch = "--- a/x.txt\n+++ b/../outside.txt\n@@ -1,1 +1,1 @@\n-precious\n+pwned\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "precious");
        fix.golden();
    }

    #[tokio::test]
    async fn deletion_via_dev_null_is_unsupported() {
        let fix = Fixture::new();
        let patch =
            "--- a/note.txt\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-line one\n-line two\n-line three\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(err.to_string().contains("deleting"), "got {err}");
        fix.golden();
    }

    #[tokio::test]
    async fn missing_target_file_fails_with_write_file_guidance() {
        let fix = Fixture::new();
        let patch = "--- a/ghost.txt\n+++ b/ghost.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(err.to_string().contains("write_file"), "got {err}");
    }

    #[tokio::test]
    async fn new_file_creation_patch_fails_with_guidance() {
        let fix = Fixture::new();
        let patch = "--- /dev/null\n+++ b/brand_new.txt\n@@ -0,0 +1,1 @@\n+hello\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(err.to_string().contains("write_file"), "got {err}");
    }

    #[tokio::test]
    async fn non_utf8_target_fails_gracefully() {
        let fix = Fixture::new();
        std::fs::write(&fix.target, [0xFF, 0xFE, 0x00, 0x01]).unwrap();
        let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let err = apply(&fix, patch, None).await.unwrap_err();
        assert!(err.to_string().contains("UTF-8"), "got {err}");
        fix.golden_non_utf8();
    }

    impl Fixture {
        fn golden_non_utf8(&self) {
            assert_eq!(
                std::fs::read(&self.target).unwrap(),
                [0xFF, 0xFE, 0x00, 0x01],
                "binary file must be untouched"
            );
        }
    }

    // --- header extraction ----------------------------------------------

    #[test]
    fn target_prefers_new_name_and_strips_git_prefixes() {
        let patch =
            diffy::Patch::from_str("--- a/note.txt\n+++ b/note.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n")
                .expect("valid patch");
        assert_eq!(target_of(&patch).unwrap(), "note.txt");
    }

    #[test]
    fn target_falls_back_to_old_name() {
        let patch =
            diffy::Patch::from_str("--- only.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n").expect("valid patch");
        assert_eq!(target_of(&patch).unwrap(), "only.txt");
    }

    #[test]
    fn target_keeps_nested_paths() {
        let patch = diffy::Patch::from_str(
            "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n",
        )
        .expect("valid patch");
        assert_eq!(target_of(&patch).unwrap(), "src/main.rs");
    }
}
