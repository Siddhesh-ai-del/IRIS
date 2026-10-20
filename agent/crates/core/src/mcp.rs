//! MCP servers through `rmcp` (stage 6.3): a **stdio** child-process
//! client whose tools surface through the same [`ToolRegistry`] and
//! permission gate the builtins use, with a per-server tool budget so a
//! generous server cannot silently eat the context window.
//!
//! Design notes:
//! - Names are namespaced `mcp_<server>_<tool>` — no builtin can be
//!   shadowed, and the gate keys `[permissions]` entries by that exact
//!   name. Unlisted names fall back to the built-in policy: **ask**, so
//!   MCP tools require confirmation until explicitly allowed.
//! - `rmcp` runs with `default-features = false` + `client` +
//!   `transport-child-process` only: stdio transports, no HTTP/SSE, no
//!   second network stack (pinned by `tests/telemetry_off.rs`).
//! - Budgets (per server, `[mcp.servers.*]`): `max_tools` caps how many
//!   tools reach the registry — the alphabetically first ones, so the
//!   set is deterministic across restarts; `max_result_bytes` caps each
//!   call's result content, with an explicit truncation marker past it.
//! - A server that won't spawn, initialize, or list tools produces a
//!   warning and is skipped: a broken MCP server degrades the run, it
//!   never blocks it.

use rmcp::Peer;
use rmcp::model::{CallToolRequestParams, ContentBlock, Tool as McpTool};
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

use crate::config::{McpConfig, McpServerConfig};
use crate::provider::ToolSpec;
use crate::tools::{ToolContext, ToolError, ToolFuture, ToolRegistry};
use crate::types::ToolResult;

/// One live MCP client service, held so its background task and child
/// process survive for the whole run (dropping it would tear the
/// connection down mid-conversation).
type Service = RunningService<RoleClient, ()>;

/// Every MCP server that connected during [`connect`], kept for the
/// run's lifetime and cancelled by [`Connections::shutdown`].
pub struct Connections {
    services: Vec<Service>,
}

impl Connections {
    /// Cancel every server connection gracefully — in-flight requests
    /// flush, then each child process is killed. Call this when the
    /// run ends, before the runtime goes away.
    pub async fn shutdown(self) {
        for service in self.services {
            let _ = service.cancel().await;
        }
    }
}

/// The registry-facing name of a server tool: `mcp_<server>_<tool>`,
/// with characters outside `[A-Za-z0-9_-]` folded to `_` so the result
/// is always a valid tool name for every provider.
pub fn tool_name(server: &str, tool: &str) -> String {
    fn sanitize(segment: &str) -> String {
        segment
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }
    format!("mcp_{}_{}", sanitize(server), sanitize(tool))
}

/// Connect every configured MCP server and register its tools into
/// `registry`. Returns the live connections plus one warning per server
/// that failed — a broken server degrades the run (fewer tools), it
/// never blocks it. Servers connect in name order (a `BTreeMap`), so
/// registration order is deterministic.
pub async fn connect(mcp: &McpConfig, registry: &mut ToolRegistry) -> (Connections, Vec<String>) {
    let mut services = Vec::new();
    let mut warnings = Vec::new();
    for (server, cfg) in &mcp.servers {
        match connect_one(server, cfg, registry, &mut warnings).await {
            Ok(service) => services.push(service),
            Err(err) => warnings.push(format!("server `{server}` unavailable: {err}")),
        }
    }
    (Connections { services }, warnings)
}

/// Spawn one server, finish the initialize handshake, list its tools,
/// apply the per-server budget, and register survivors.
async fn connect_one(
    server: &str,
    cfg: &McpServerConfig,
    registry: &mut ToolRegistry,
    warnings: &mut Vec<String>,
) -> Result<Service, String> {
    let mut command = tokio::process::Command::new(&cfg.command);
    command.args(&cfg.args);
    for (key, value) in &cfg.env {
        command.env(key, value);
    }
    let transport = TokioChildProcess::new(command)
        .map_err(|err| format!("cannot spawn `{}`: {err}", cfg.command))?;
    let service = ().serve(transport).await.map_err(|err| format!("initialize failed: {err}"))?;

    let mut tools = match service.list_all_tools().await {
        Ok(tools) => tools,
        Err(err) => {
            // Don't leave a half-connected child behind.
            let _ = service.cancel().await;
            return Err(format!("tools/list failed: {err}"));
        }
    };

    // Per-server tool budget: sort (deterministic across restarts),
    // keep the first `max_tools`, and say so — never silently.
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let exposed = tools.len();
    if exposed > cfg.max_tools {
        warnings.push(format!(
            "server `{server}` exposes {exposed} tools; the tool budget \
             (max_tools = {budget}) keeps the alphabetically first {budget} \
             — raise max_tools to register more",
            budget = cfg.max_tools,
        ));
        tools.truncate(cfg.max_tools);
    }

    for tool in tools {
        register_server_tool(registry, server, &tool, cfg, service.peer().clone());
    }
    Ok(service)
}

/// One server tool → one dynamic registry entry. The runner closure
/// holds a cheap [`Peer`] clone: dispatch (on the run's runtime) sends
/// over a channel to the service task (on the runtime [`connect`] was
/// called on), so both ends stay decoupled.
fn register_server_tool(
    registry: &mut ToolRegistry,
    server: &str,
    tool: &McpTool,
    cfg: &McpServerConfig,
    peer: Peer<RoleClient>,
) {
    let remote = tool.name.to_string();
    let name = tool_name(server, &remote);
    let spec = ToolSpec {
        name: name.clone(),
        description: tool.description.as_ref().map(|desc| desc.to_string()),
        parameters: Value::Object((*tool.input_schema).clone()),
    };
    let limit = cfg.max_result_bytes;

    registry.register_dynamic(spec, move |_ctx: ToolContext, args: Value| -> ToolFuture {
        let peer = peer.clone();
        let remote = remote.clone();
        let name = name.clone();
        Box::pin(async move {
            let arguments = match args {
                Value::Object(map) => Some(map),
                Value::Null => None,
                other => {
                    return Err(ToolError::InvalidArgs {
                        tool: name,
                        message: format!("expected a JSON object, got {other}"),
                    });
                }
            };
            let mut params = CallToolRequestParams::new(remote);
            params.arguments = arguments;
            let result = peer
                .call_tool(params)
                .await
                .map_err(|err| ToolError::Failed {
                    tool: name.clone(),
                    message: format!("MCP call failed: {err}"),
                })?;
            Ok(ToolResult {
                tool_use_id: String::new(),
                content: truncate_to(render_content(&result.content), limit),
                is_error: result.is_error.unwrap_or(false),
            })
        })
    });
}

/// MCP content → one string: text blocks joined by newlines; anything
/// else (image, audio, resource) falls back to its JSON encoding so no
/// block is silently dropped.
fn render_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            other => serde_json::to_string(other).unwrap_or_else(|_| "[unrenderable]".to_string()),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Per-server result budget: keep at most `limit` **bytes** of content
/// (on a char boundary), then say explicitly what was dropped.
fn truncate_to(mut content: String, limit: usize) -> String {
    if content.len() <= limit {
        return content;
    }
    let total = content.len();
    let mut cut = limit;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    content.push_str(&format!(
        "\n[iris truncated this result: showing {cut} of {total} bytes]"
    ));
    content
}
