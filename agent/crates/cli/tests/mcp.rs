//! Stage 6.3 — MCP through the real stdio child-process transport:
//! the fixture server (`src/bin/iris_fake_mcp.rs`) is spawned exactly
//! like a configured server, its tools flow through the registry +
//! permission gate, and the per-server budget keeps a generous server
//! from eating the context window.
//!
//! The suite drives `iris_core::mcp` directly (the run/chat wiring is a
//! thin `block_on` around `connect`/`shutdown`); the fixture binary is
//! built under `--all-features`, which every official test path uses.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use iris_core::config::{McpConfig, McpServerConfig};
use iris_core::mcp;
use iris_core::tools::{AllowAll, Decision, Gate, PolicyGate, ToolContext, ToolRegistry};
use serde_json::json;

/// The fixture server binary — built with `--all-features`. Absent
/// otherwise (e.g. a bare `cargo test -p iris`), which skips politely
/// instead of red-herring.
fn fixture_bin() -> Option<PathBuf> {
    option_env!("CARGO_BIN_EXE_iris-fake-mcp")
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

/// A per-test runtime: `connect` spawns child processes and rmcp
/// drives them on background tasks (a `multi_thread` runtime keeps
/// those running across `block_on` gaps — same as the real runs).
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
}

fn allow() -> Gate {
    Arc::new(AllowAll)
}

fn ctx_in(root: &std::path::Path, gate: Gate) -> ToolContext {
    ToolContext::with_gate(root, gate)
}

/// Config for one fixture server with env knobs and default budgets.
fn fixture_server(bin: &std::path::Path, env: &[(&str, &str)]) -> McpServerConfig {
    let mut server = McpServerConfig::new(bin.display().to_string());
    for (key, value) in env {
        server.env.insert(key.to_string(), value.to_string());
    }
    server
}

fn mcp_config(servers: BTreeMap<String, McpServerConfig>) -> McpConfig {
    McpConfig { servers }
}

/// Names of every dynamic (MCP) tool currently registered, sorted.
fn mcp_tool_names(registry: &ToolRegistry) -> Vec<String> {
    let mut names = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .filter(|name| name.starts_with("mcp_"))
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// Skip politely when the fixture bin wasn't built (only the
/// `--all-features` paths build it — every official one does).
macro_rules! fixture_bin_or_skip {
    ($bin:ident) => {
        let Some($bin) = fixture_bin() else {
            eprintln!("skip: fixture not built (run tests with --all-features)");
            return;
        };
    };
}

#[test]
fn stdio_server_tools_flow_through_registry_and_gate() {
    fixture_bin_or_skip!(bin);
    let rt = runtime();
    let mut registry = ToolRegistry::new();
    let config = mcp_config(BTreeMap::from([(
        "demo".to_string(),
        fixture_server(&bin, &[("IRIS_FAKE_MCP_TOOLS", "alpha,beta")]),
    )]));

    let (connections, warnings) = rt.block_on(mcp::connect(&config, &mut registry));
    assert!(
        warnings.is_empty(),
        "healthy server: no warnings {warnings:?}"
    );
    assert_eq!(
        mcp_tool_names(&registry),
        vec!["mcp_demo_alpha".to_string(), "mcp_demo_beta".to_string()],
        "namespaced names from tools/list"
    );

    // The spec carries the server's own schema through to the model.
    let spec = registry
        .specs()
        .into_iter()
        .find(|spec| spec.name == "mcp_demo_alpha")
        .expect("alpha registered");
    assert_eq!(spec.description.as_deref(), Some("fixture tool alpha"));
    assert_eq!(
        spec.parameters,
        json!({
            "type": "object",
            "properties": { "value": { "type": "string" } }
        })
    );

    // Call through the registry: args → server → result back.
    let dir = tempfile::tempdir().expect("tempdir");
    let result = rt
        .block_on(registry.execute(
            "mcp_demo_alpha",
            &ctx_in(dir.path(), allow()),
            "toolu_1",
            json!({ "value": "hi" }),
        ))
        .expect("call reaches the server");
    assert_eq!(result.tool_use_id, "toolu_1");
    assert!(!result.is_error);
    assert_eq!(result.content, "echo:{\"value\":\"hi\"}");

    rt.block_on(connections.shutdown());
}

#[test]
fn mcp_tools_ask_by_default_until_explicitly_allowed() {
    fixture_bin_or_skip!(bin);
    let rt = runtime();
    let mut registry = ToolRegistry::new();
    let config = mcp_config(BTreeMap::from([(
        "demo".to_string(),
        fixture_server(&bin, &[("IRIS_FAKE_MCP_TOOLS", "alpha")]),
    )]));
    let (connections, warnings) = rt.block_on(mcp::connect(&config, &mut registry));
    assert!(warnings.is_empty(), "{warnings:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    // No `[permissions]` rule → builtin policy → `ask` → fail closed.
    let ask = Arc::new(PolicyGate::new(BTreeMap::new()));
    let err = rt
        .block_on(registry.execute(
            "mcp_demo_alpha",
            &ctx_in(dir.path(), ask),
            "t1",
            json!({ "value": "hi" }),
        ))
        .expect_err("unconfigured MCP tool must ask");
    assert!(
        err.to_string().contains("confirmation"),
        "fails closed with the ask message: {err}"
    );

    // An explicit allow for the namespaced name unlocks it.
    let allow_gate = Arc::new(PolicyGate::new(BTreeMap::from([(
        "mcp_demo_alpha".to_string(),
        Decision::Allow,
    )])));
    let result = rt
        .block_on(registry.execute(
            "mcp_demo_alpha",
            &ctx_in(dir.path(), allow_gate),
            "t2",
            json!({ "value": "hi" }),
        ))
        .expect("configured allow dispatches");
    assert_eq!(result.content, "echo:{\"value\":\"hi\"}");

    rt.block_on(connections.shutdown());
}

#[test]
fn per_server_tool_budget_caps_exposure_and_warns() {
    fixture_bin_or_skip!(bin);
    let rt = runtime();
    let mut registry = ToolRegistry::new();
    let mut server = fixture_server(
        &bin,
        &[("IRIS_FAKE_MCP_TOOLS", "alpha,beta,gamma,delta,echo")],
    );
    server.max_tools = 2;
    let config = mcp_config(BTreeMap::from([("server".to_string(), server)]));

    let (connections, warnings) = rt.block_on(mcp::connect(&config, &mut registry));
    assert_eq!(
        mcp_tool_names(&registry),
        vec![
            "mcp_server_alpha".to_string(),
            "mcp_server_beta".to_string()
        ],
        "budget keeps the alphabetically first N — stable across restarts"
    );
    assert_eq!(
        warnings.len(),
        1,
        "overflow is reported, not silent: {warnings:?}"
    );
    assert!(
        warnings[0].contains("budget") && warnings[0].contains('5') && warnings[0].contains('2'),
        "warning names the overflow and the cap: {}",
        warnings[0]
    );

    rt.block_on(connections.shutdown());
}

#[test]
fn result_budget_truncates_a_huge_tool_result() {
    fixture_bin_or_skip!(bin);
    let rt = runtime();
    let mut registry = ToolRegistry::new();
    let mut server = fixture_server(
        &bin,
        &[
            ("IRIS_FAKE_MCP_TOOLS", "big"),
            ("IRIS_FAKE_MCP_BIG_BYTES", "100000"),
        ],
    );
    server.max_result_bytes = 64;
    let config = mcp_config(BTreeMap::from([("server".to_string(), server)]));

    let (connections, warnings) = rt.block_on(mcp::connect(&config, &mut registry));
    assert!(warnings.is_empty(), "{warnings:?}");

    let dir = tempfile::tempdir().expect("tempdir");
    let result = rt
        .block_on(registry.execute(
            "mcp_server_big",
            &ctx_in(dir.path(), allow()),
            "t1",
            json!({}),
        ))
        .expect("call dispatches");

    let payload = result.content.lines().next().expect("content");
    assert!(
        payload.len() <= 64 && payload.chars().all(|c| c == 'x'),
        "payload capped at max_result_bytes ({} bytes)",
        payload.len()
    );
    assert!(
        result.content.contains("truncated") && result.content.contains("of 100000 bytes"),
        "truncation is explicit in the result: {}",
        &result.content[payload.len()..]
    );

    rt.block_on(connections.shutdown());
}

#[test]
fn an_unreachable_server_warns_but_the_rest_still_register() {
    fixture_bin_or_skip!(bin);
    let rt = runtime();
    let mut registry = ToolRegistry::new();
    let config = mcp_config(BTreeMap::from([
        (
            "broken".to_string(),
            McpServerConfig::new("/nonexistent/iris-no-such-mcp-server"),
        ),
        (
            "demo".to_string(),
            fixture_server(&bin, &[("IRIS_FAKE_MCP_TOOLS", "alpha")]),
        ),
    ]));

    let (connections, warnings) = rt.block_on(mcp::connect(&config, &mut registry));
    assert_eq!(warnings.len(), 1, "one warning for the broken server");
    assert!(
        warnings[0].contains("broken"),
        "warning names it: {}",
        warnings[0]
    );
    assert_eq!(
        mcp_tool_names(&registry),
        vec!["mcp_demo_alpha".to_string()],
        "the healthy server still registered"
    );

    rt.block_on(connections.shutdown());
}

#[test]
fn shutdown_tears_every_connection_down() {
    fixture_bin_or_skip!(bin);
    let rt = runtime();
    let mut registry = ToolRegistry::new();
    let config = mcp_config(BTreeMap::from([
        (
            "one".to_string(),
            fixture_server(&bin, &[("IRIS_FAKE_MCP_TOOLS", "alpha")]),
        ),
        (
            "two".to_string(),
            fixture_server(&bin, &[("IRIS_FAKE_MCP_TOOLS", "beta")]),
        ),
    ]));

    let (connections, warnings) = rt.block_on(mcp::connect(&config, &mut registry));
    assert!(warnings.is_empty(), "{warnings:?}");
    // Cancels both services and kills both children; must return
    // promptly (a hang here would hang every real run).
    rt.block_on(connections.shutdown());
}
