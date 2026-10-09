//! `list_dir` tool (stage 1.6): confined, sorted, entry-capped listing.

use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;

use super::path::resolve_within;
use super::registry::{Tool, ToolContext, ToolError};
use crate::types::ToolResult;

/// Hard cap on entries returned by one listing (larger directories are
/// truncated with a visible notice).
pub const MAX_LIST_ENTRIES: usize = 500;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Directory to list, relative to the workspace root. Empty string or
    /// `.` lists the root itself.
    path: String,
}

pub struct ListTool;

impl Tool for ListTool {
    type Args = ListArgs;

    fn name(&self) -> &'static str {
        "list_dir"
    }

    fn description(&self) -> &'static str {
        "Lists one directory inside the workspace (non-recursive): sorted \
         names, directories suffixed with /"
    }

    async fn execute(&self, ctx: &ToolContext, args: ListArgs) -> Result<ToolResult, ToolError> {
        let requested = args.path.as_str();
        let path = resolve_within(self.name(), &ctx.workspace_root, Path::new(requested))?;

        let mut items: Vec<(String, bool)> = std::fs::read_dir(&path)
            .map_err(|err| ToolError::Failed {
                tool: self.name().to_string(),
                message: format!("cannot list {requested}: {err}"),
            })?
            .flatten() // an unreadable entry is skipped, never escalated
            .map(|entry| {
                let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                (entry.file_name().to_string_lossy().into_owned(), is_dir)
            })
            .collect();
        items.sort();

        let truncated = items.len() > MAX_LIST_ENTRIES;
        items.truncate(MAX_LIST_ENTRIES);

        let mut content = if items.is_empty() {
            "(empty)".to_string()
        } else {
            items
                .into_iter()
                .map(
                    |(name, is_dir)| {
                        if is_dir { format!("{name}/") } else { name }
                    },
                )
                .collect::<Vec<_>>()
                .join("\n")
        };
        if truncated {
            content.push_str(&format!(
                "\n[ferro: showing first {MAX_LIST_ENTRIES} entries]"
            ));
        }

        Ok(ToolResult {
            tool_use_id: String::new(),
            content,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolRegistry};
    use serde_json::json;

    struct Fixture {
        _dir: tempfile::TempDir,
        ctx: ToolContext,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            Self {
                ctx: ToolContext::new(dir.path()),
                _dir: dir,
            }
        }
    }

    fn list(fixture: &Fixture, path: &str) -> Result<ToolResult, ToolError> {
        let args: ListArgs = serde_json::from_value(json!({"path": path})).expect("valid args");
        futures::executor::block_on(ListTool.execute(&fixture.ctx, args))
    }

    #[test]
    fn lists_sorted_entries_with_directories_marked() {
        let fix = Fixture::new();
        let root = fix.ctx.workspace_root.clone();
        std::fs::write(root.join("b.txt"), "").unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::fs::create_dir(root.join("a_dir")).unwrap();
        std::fs::write(root.join("a_dir/inner.rs"), "").unwrap();

        let result = list(&fix, ".").unwrap();
        // Sorted by raw name: "a.txt" < "a_dir" < "b.txt".
        assert_eq!(result.content, "a.txt\na_dir/\nb.txt");
        assert!(!result.is_error);
    }

    #[test]
    fn nested_directory_lists_only_itself() {
        let fix = Fixture::new();
        let root = fix.ctx.workspace_root.clone();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/inner.rs"), "").unwrap();
        std::fs::write(root.join("root.rs"), "").unwrap();

        let result = list(&fix, "sub").unwrap();
        assert_eq!(result.content, "inner.rs");
    }

    #[test]
    fn empty_directory_reports_empty() {
        let fix = Fixture::new();
        let result = list(&fix, "").unwrap();
        assert_eq!(result.content, "(empty)");
    }

    #[test]
    fn huge_directory_is_truncated_with_visible_notice() {
        let fix = Fixture::new();
        let root = fix.ctx.workspace_root.clone();
        for i in 0..(MAX_LIST_ENTRIES + 5) {
            std::fs::write(root.join(format!("f{i:04}.txt")), "").unwrap();
        }

        let result = list(&fix, "").unwrap();
        assert!(
            result
                .content
                .contains("[ferro: showing first 500 entries]"),
            "truncation must be visible"
        );
        let lines = result.content.lines().count();
        assert_eq!(lines, MAX_LIST_ENTRIES + 1, "500 entries + notice line");
        // Alphabetical first 500 → f0000 … f0499.
        assert!(result.content.starts_with("f0000.txt"));
        assert!(result.content.contains("f0499.txt"));
        assert!(!result.content.contains("f0500.txt\n"));
    }

    #[test]
    fn missing_directory_fails_loudly() {
        let fix = Fixture::new();
        let err = list(&fix, "ghost").unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn escape_attempt_through_registry_is_rejected() {
        let fix = Fixture::new();
        let mut registry = ToolRegistry::new();
        registry.register(ListTool);

        let err = registry
            .execute("list_dir", &fix.ctx, "call_1", json!({"path": ".."}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }
}
