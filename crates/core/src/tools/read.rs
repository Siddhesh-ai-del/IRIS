//! `read_file` tool (stage 1.6): confined read, size cap, lossy UTF-8.

use std::io::Read as _;
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;

use super::path::resolve_within;
use super::registry::{Tool, ToolContext, ToolError};
use crate::types::ToolResult;

/// Hard cap on bytes returned by one read. Larger files are **truncated
/// with a visible notice** — never silently, and never buffered whole.
pub const MAX_READ_BYTES: u64 = 64 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// Path relative to the workspace root. Absolute paths are only
    /// accepted when they stay inside the root.
    path: String,
}

pub struct ReadTool;

impl Tool for ReadTool {
    type Args = ReadArgs;

    fn name(&self) -> &'static str {
        "read_file"
    }

    fn description(&self) -> &'static str {
        "Reads a UTF-8 text file inside the workspace (invalid bytes are \
         replaced; files over 64 KiB are truncated with a notice)"
    }

    async fn execute(&self, ctx: &ToolContext, args: ReadArgs) -> Result<ToolResult, ToolError> {
        let requested = args.path.as_str();
        let path = resolve_within(self.name(), &ctx.workspace_root, Path::new(requested))?;

        let file = std::fs::File::open(&path).map_err(|err| ToolError::Failed {
            tool: self.name().to_string(),
            message: format!("cannot open {requested}: {err}"),
        })?;

        // Read at most CAP+1 bytes so truncation is detectable without
        // ever buffering a huge file.
        let mut buffer = Vec::new();
        file.take(MAX_READ_BYTES + 1)
            .read_to_end(&mut buffer)
            .map_err(|err| ToolError::Failed {
                tool: self.name().to_string(),
                message: format!("cannot read {requested}: {err}"),
            })?;

        let truncated = buffer.len() as u64 > MAX_READ_BYTES;
        if truncated {
            buffer.truncate(MAX_READ_BYTES as usize);
        }
        let mut content = String::from_utf8_lossy(&buffer).into_owned();
        if truncated {
            content.push_str(&format!(
                "\n\n[ferro: file truncated — showing first {MAX_READ_BYTES} bytes]"
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

    fn workspace() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path());
        (dir, ctx)
    }

    fn read(path: &str, ctx: &ToolContext) -> Result<ToolResult, ToolError> {
        let args: ReadArgs = serde_json::from_value(json!({"path": path})).expect("valid args");
        futures::executor::block_on(ReadTool.execute(ctx, args))
    }

    #[test]
    fn reads_relative_file() {
        let (dir, ctx) = workspace();
        std::fs::write(dir.path().join("hello.rs"), "fn main() {}").unwrap();

        let result = read("hello.rs", &ctx).unwrap();
        assert_eq!(result.content, "fn main() {}");
        assert!(!result.is_error);
        assert_eq!(result.tool_use_id, "", "registry stamps the id later");
    }

    #[test]
    fn truncates_large_files_with_visible_notice() {
        let (dir, ctx) = workspace();
        let big = "a".repeat(MAX_READ_BYTES as usize + 123);
        std::fs::write(dir.path().join("big.log"), &big).unwrap();

        let result = read("big.log", &ctx).unwrap();
        assert!(
            result
                .content
                .starts_with(&"a".repeat(MAX_READ_BYTES as usize))
        );
        assert!(
            result
                .content
                .contains("[ferro: file truncated — showing first"),
            "truncation must be visible, never silent"
        );
        assert!(result.content.len() < big.len());
    }

    #[test]
    fn invalid_utf8_is_lossy_not_an_error() {
        let (dir, ctx) = workspace();
        std::fs::write(dir.path().join("mixed.bin"), [0xFF, b'A', 0xFE, b'B']).unwrap();

        let result = read("mixed.bin", &ctx).unwrap();
        assert_eq!(result.content, "\u{FFFD}A\u{FFFD}B");
    }

    #[test]
    fn missing_file_fails_loudly() {
        let (_dir, ctx) = workspace();
        let err = read("ghost.txt", &ctx).unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
        assert!(err.to_string().contains("ghost.txt"));
    }

    #[test]
    fn directory_read_fails_loudly() {
        let (dir, ctx) = workspace();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let err = read("sub", &ctx).unwrap_err();
        assert!(matches!(err, ToolError::Failed { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn escape_attempt_through_registry_is_rejected() {
        // End-to-end: raw model JSON → registry → tool → confinement.
        let (_dir, ctx) = workspace();
        let mut registry = ToolRegistry::new();
        registry.register(ReadTool);

        let err = registry
            .execute(
                "read_file",
                &ctx,
                "call_1",
                json!({"path": "../../etc/passwd"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::PathEscape { .. }), "got {err:?}");
    }
}
