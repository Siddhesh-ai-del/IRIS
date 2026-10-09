//! `write_file` tool (stage 1.6): confined write with parent-dir creation.

use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;

use super::path::resolve_within;
use super::registry::{Tool, ToolContext, ToolError};
use crate::types::ToolResult;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteArgs {
    /// Path relative to the workspace root. Absolute paths are only
    /// accepted when they stay inside the root.
    path: String,
    /// Full new contents of the file (existing contents are replaced).
    content: String,
}

pub struct WriteTool;

impl Tool for WriteTool {
    type Args = WriteArgs;

    fn name(&self) -> &'static str {
        "write_file"
    }

    fn description(&self) -> &'static str {
        "Creates or overwrites a UTF-8 file inside the workspace, creating \
         missing parent directories"
    }

    async fn execute(&self, ctx: &ToolContext, args: WriteArgs) -> Result<ToolResult, ToolError> {
        let requested = args.path.as_str();
        let path = resolve_within(self.name(), &ctx.workspace_root, Path::new(requested))?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| ToolError::Failed {
                tool: self.name().to_string(),
                message: format!("cannot create parent directories for {requested}: {err}"),
            })?;
        }
        std::fs::write(&path, &args.content).map_err(|err| ToolError::Failed {
            tool: self.name().to_string(),
            message: format!("cannot write {requested}: {err}"),
        })?;

        Ok(ToolResult {
            tool_use_id: String::new(),
            content: format!("wrote {} bytes to {requested}", args.content.len()),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolRegistry};
    use serde_json::json;

    /// Workspace root with an `outside/` sibling holding a victim file.
    struct Fixture {
        _parent: tempfile::TempDir,
        ctx: ToolContext,
        outside: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let parent = tempfile::tempdir().expect("tempdir");
            let root = parent.path().join("ws");
            let outside = parent.path().join("outside");
            std::fs::create_dir(&root).unwrap();
            std::fs::create_dir(&outside).unwrap();
            std::fs::write(outside.join("victim.txt"), "original").unwrap();
            Self {
                ctx: ToolContext::new(root),
                outside,
                _parent: parent,
            }
        }
    }

    fn write(fixture: &Fixture, path: &str, content: &str) -> Result<ToolResult, ToolError> {
        let args: WriteArgs =
            serde_json::from_value(json!({"path": path, "content": content})).expect("valid args");
        futures::executor::block_on(WriteTool.execute(&fixture.ctx, args))
    }

    #[test]
    fn writes_new_file_and_creates_parents() {
        let fix = Fixture::new();
        let result = write(&fix, "a/b/c.txt", "hello").unwrap();
        assert_eq!(result.content, "wrote 5 bytes to a/b/c.txt");
        let written = fix.ctx.workspace_root.join("a/b/c.txt");
        assert_eq!(std::fs::read_to_string(written).unwrap(), "hello");
    }

    #[test]
    fn overwrites_existing_file() {
        let fix = Fixture::new();
        write(&fix, "note.md", "first").unwrap();
        let result = write(&fix, "note.md", "second").unwrap();
        assert_eq!(result.content, "wrote 6 bytes to note.md");
        assert_eq!(
            std::fs::read_to_string(fix.ctx.workspace_root.join("note.md")).unwrap(),
            "second"
        );
    }

    #[test]
    fn empty_content_writes_empty_file() {
        let fix = Fixture::new();
        write(&fix, "empty.txt", "").unwrap();
        assert_eq!(
            std::fs::read_to_string(fix.ctx.workspace_root.join("empty.txt")).unwrap(),
            ""
        );
    }

    #[test]
    fn dotdot_escape_is_rejected_and_outside_untouched() {
        let fix = Fixture::new();
        let err = write(&fix, "../outside/victim.txt", "pwned").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
        assert_eq!(
            std::fs::read_to_string(fix.outside.join("victim.txt")).unwrap(),
            "original",
            "outside file must not change"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_and_outside_untouched() {
        let fix = Fixture::new();
        let victim = fix.outside.join("victim.txt");
        std::os::unix::fs::symlink(&victim, fix.ctx.workspace_root.join("flink")).unwrap();

        let err = write(&fix, "flink", "pwned").unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
    }

    #[tokio::test]
    async fn escape_attempt_through_registry_is_rejected() {
        let fix = Fixture::new();
        let mut registry = ToolRegistry::new();
        registry.register(WriteTool);

        let err = registry
            .execute(
                "write_file",
                &fix.ctx,
                "call_1",
                json!({"path": "/etc/cron.d/pwn", "content": "* * * * * sh"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
        assert!(!std::path::Path::new("/etc/cron.d/pwn").exists());
    }
}
