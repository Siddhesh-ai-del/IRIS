//! Stage 6.3 — the registry's dynamic-registration seam: tools whose
//! spec arrives at runtime (MCP `tools/list`) get specs, dispatch, and
//! the permission gate exactly like typed builtins.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use iris_core::provider::ToolSpec;
use iris_core::tools::{
    AllowAll, Decision, Gate, PolicyGate, ToolContext, ToolError, ToolRegistry,
};
use iris_core::types::ToolResult;
use serde_json::{Value, json};

/// A dynamic spec as an MCP server would describe it.
fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: Some("a runtime-described tool".to_string()),
        parameters: json!({
            "type": "object",
            "properties": { "value": { "type": "string" } }
        }),
    }
}

fn ctx(root: &Path, gate: Gate) -> ToolContext {
    ToolContext::with_gate(root, gate)
}

fn allow() -> Gate {
    Arc::new(AllowAll)
}

fn policy(overlay: &[(&str, Decision)]) -> Gate {
    let map = overlay
        .iter()
        .map(|(name, decision)| (name.to_string(), *decision))
        .collect::<BTreeMap<String, Decision>>();
    Arc::new(PolicyGate::new(map))
}

/// A closure that records whether its body ever ran, for gate tests.
fn recording_runner(
    ran: &Arc<AtomicBool>,
) -> impl Fn(ToolContext, Value) -> iris_core::tools::ToolFuture + Send + Sync + 'static {
    let ran = Arc::clone(ran);
    move |_ctx: ToolContext, args: Value| {
        ran.store(true, Ordering::SeqCst);
        Box::pin(async move {
            Ok(ToolResult {
                tool_use_id: String::new(),
                content: format!("got {args}"),
                is_error: false,
            })
        })
    }
}

#[tokio::test]
async fn dynamic_spec_reaches_the_registry_intact() {
    let mut registry = ToolRegistry::new();
    registry.register_dynamic(
        spec("mcp_git_status"),
        recording_runner(&Arc::new(AtomicBool::new(false))),
    );

    let specs = registry.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "mcp_git_status");
    assert_eq!(
        specs[0].description.as_deref(),
        Some("a runtime-described tool")
    );
    assert_eq!(
        specs[0].parameters,
        json!({ "type": "object", "properties": { "value": { "type": "string" } } })
    );
    assert!(registry.contains("mcp_git_status"));
    // Boundary: the schema is the server's, so validation accepts any
    // JSON — the server itself rejects malformed arguments.
    registry
        .validate("mcp_git_status", &json!({ "anything": [1, 2, 3] }))
        .expect("dynamic tools accept any JSON object arguments");
}

#[tokio::test]
async fn dynamic_dispatch_runs_the_closure_and_stamps_the_id() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register_dynamic(spec("mcp_git_status"), recording_runner(&ran));

    let dir = tempfile::tempdir().expect("tempdir");
    let result = registry
        .execute(
            "mcp_git_status",
            &ctx(dir.path(), allow()),
            "toolu_9",
            json!({ "value": "hi" }),
        )
        .await
        .expect("dispatch");

    assert_eq!(result.tool_use_id, "toolu_9", "registry stamps the id");
    assert_eq!(result.content, "got {\"value\":\"hi\"}");
    assert!(ran.load(Ordering::SeqCst), "the closure body ran");
}

#[tokio::test]
async fn a_deny_stops_the_dynamic_call_before_the_closure() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register_dynamic(spec("mcp_git_push"), recording_runner(&ran));

    let dir = tempfile::tempdir().expect("tempdir");
    let gate = policy(&[("mcp_git_push", Decision::Deny)]);
    let err = registry
        .execute("mcp_git_push", &ctx(dir.path(), gate), "t1", json!({}))
        .await
        .expect_err("deny must refuse");

    assert!(matches!(err, ToolError::Denied { .. }), "{err}");
    assert!(!ran.load(Ordering::SeqCst), "body never ran");
}

#[tokio::test]
async fn an_mcp_name_without_a_rule_asks_and_fails_closed() {
    let ran = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register_dynamic(spec("mcp_git_push"), recording_runner(&ran));

    // Empty overlay → builtin policy → `ask` for anything that isn't a
    // read-only builtin, and nothing here can prompt at dispatch.
    let dir = tempfile::tempdir().expect("tempdir");
    let err = registry
        .execute(
            "mcp_git_push",
            &ctx(dir.path(), policy(&[])),
            "t1",
            json!({}),
        )
        .await
        .expect_err("unresolved ask must fail closed");

    assert!(matches!(err, ToolError::NeedsConfirmation { .. }), "{err}");
    assert!(!ran.load(Ordering::SeqCst), "body never ran");
}

#[tokio::test]
async fn re_registering_the_same_name_replaces_the_runner() {
    let first = Arc::new(AtomicBool::new(false));
    let second = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry.register_dynamic(spec("mcp_x_y"), recording_runner(&first));
    registry.register_dynamic(spec("mcp_x_y"), recording_runner(&second));

    assert_eq!(
        registry.specs().len(),
        1,
        "upsert keeps a single registration per name"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    registry
        .execute("mcp_x_y", &ctx(dir.path(), allow()), "t", json!({}))
        .await
        .expect("dispatch");
    assert!(!first.load(Ordering::SeqCst), "old runner replaced");
    assert!(second.load(Ordering::SeqCst), "new runner wins");
}
