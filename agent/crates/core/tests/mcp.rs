//! Stage 7.2 — hermetic MCP connection coverage: every failure mode
//! before a server speaks (unspawnable command, a child that dies
//! before the `initialize` handshake) degrades to **one warning and no
//! registered tools** — a broken MCP server degrades the run, it never
//! blocks it. No real server, network, or node needed; the success
//! path is pinned by the CLI suite's fixture server
//! (`crates/cli/tests/mcp.rs`).

use std::collections::BTreeMap;

use iris_core::config::{McpConfig, McpServerConfig};
use iris_core::mcp;
use iris_core::tools::ToolRegistry;

/// The immediate-exit server command: spawns fine, exits before the
/// handshake — cmd.exe's own `exit`, the Windows twin of `sh -c`.
#[cfg(unix)]
const INSTANT_EXIT: (&str, &[&str]) = ("sh", &["-c", "exit 0"]);
#[cfg(windows)]
const INSTANT_EXIT: (&str, &[&str]) = ("cmd", &["/C", "exit 0"]);

fn server(command: &str, args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        command: command.to_string(),
        args: args.iter().map(|arg| (*arg).to_string()).collect(),
        env: BTreeMap::new(),
        max_tools: 8,
        max_result_bytes: 65_536,
    }
}

fn config(servers: &[(&str, McpServerConfig)]) -> McpConfig {
    McpConfig {
        servers: servers
            .iter()
            .map(|(name, cfg)| ((*name).to_string(), cfg.clone()))
            .collect(),
    }
}

fn mcp_tool_names(registry: &ToolRegistry) -> Vec<String> {
    registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .filter(|name| name.starts_with("mcp_"))
        .collect()
}

#[tokio::test]
async fn an_empty_config_connects_to_nothing_and_shuts_down_quietly() {
    let mut registry = ToolRegistry::new();
    let (connections, warnings) = mcp::connect(&McpConfig::default(), &mut registry).await;
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(mcp_tool_names(&registry).is_empty());
    connections.shutdown().await;
}

#[tokio::test]
async fn an_unreachable_server_degrades_to_a_warning() {
    let mut registry = ToolRegistry::new();
    let cfg = config(&[("ghost", server("/nonexistent/iris-mcp-fixture", &[]))]);

    let (connections, warnings) = mcp::connect(&cfg, &mut registry).await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("`ghost` unavailable"),
        "{}",
        warnings[0]
    );
    assert!(warnings[0].contains("cannot spawn"), "{}", warnings[0]);
    assert!(
        mcp_tool_names(&registry).is_empty(),
        "a server that never spawned registers nothing"
    );
    connections.shutdown().await;
}

#[tokio::test]
async fn a_server_that_dies_before_the_handshake_degrades_to_a_warning() {
    let mut registry = ToolRegistry::new();
    // Spawns fine, exits immediately — `initialize` never completes.
    let cfg = config(&[("flaky", server(INSTANT_EXIT.0, INSTANT_EXIT.1))]);

    let (connections, warnings) = mcp::connect(&cfg, &mut registry).await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("`flaky` unavailable"),
        "{}",
        warnings[0]
    );
    assert!(warnings[0].contains("initialize failed"), "{}", warnings[0]);
    assert!(mcp_tool_names(&registry).is_empty());
    connections.shutdown().await;
}

#[tokio::test]
async fn one_broken_server_never_blocks_the_others_from_the_config() {
    let mut registry = ToolRegistry::new();
    // Two broken servers: each gets its own warning, both in name
    // order (BTreeMap), and neither stops the other's attempt.
    let cfg = config(&[
        ("a_ghost", server("/nonexistent/iris-mcp-fixture", &[])),
        ("b_flaky", server(INSTANT_EXIT.0, INSTANT_EXIT.1)),
    ]);

    let (connections, warnings) = mcp::connect(&cfg, &mut registry).await;
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].contains("`a_ghost`"), "{}", warnings[0]);
    assert!(warnings[1].contains("`b_flaky`"), "{}", warnings[1]);
    assert!(mcp_tool_names(&registry).is_empty());
    connections.shutdown().await;
}
