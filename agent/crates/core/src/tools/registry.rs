//! Tool trait, execution context, and registry (stage 1.5).
//!
//! The agent loop never calls tools directly — it dispatches through a
//! [`ToolRegistry`], which **validates raw arguments against the tool's
//! typed `Args` at the boundary** (serde deserialization; the same type
//! backs the `schemars` JSON Schema sent to the model) before the tool
//! body can run. Invalid JSON never reaches a tool.

use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::permissions::{AllowAll, Decision};
use crate::provider::ToolSpec;
use crate::types::ToolResult;

/// Type-erased tool future handed back by the registry's dispatch.
type ToolFuture = Pin<Box<dyn Future<Output = Result<ToolResult, ToolError>> + Send>>;

/// Boundary-check closure registered per tool (no dispatch).
type ValidateFn = Box<dyn Fn(&Value) -> Result<(), ToolError> + Send + Sync>;

/// Dispatch closure registered per tool: raw args → boxed future.
type RunFn = Box<dyn Fn(ToolContext, Value) -> ToolFuture + Send + Sync>;

/// Errors raised while locating, validating, or running a tool call.
///
/// Tools return these for *infrastructural* failures; content-level
/// failures (file not found, nonzero exit) come back as
/// [`ToolResult::is_error`] — the loop (stage 1.9) turns both into a
/// `tool` message the model can react to.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The model hallucinated a tool name.
    #[error("unknown tool: {name}")]
    UnknownTool { name: String },

    /// Raw JSON failed to deserialize into the tool's `Args`.
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArgs { tool: String, message: String },

    /// The resolved path left the workspace root — security event
    /// (stage 1.6 confinement: `..` or symlink escape).
    #[error("path escapes the workspace: {path}")]
    PathEscape { path: String },

    /// The permission gate refused the call — security event (stage
    /// 1.7). The tool body never ran.
    #[error("permission denied for `{tool}`: {detail}")]
    Denied { tool: String, detail: String },

    /// The gate returned `Ask`, but nothing here can prompt — dispatch
    /// fails closed until the caller resolves it (stage 1.9 wires the
    /// ask flow before dispatch).
    #[error(
        "permission requires confirmation for `{tool}` — resolve `ask` before dispatch: {detail}"
    )]
    NeedsConfirmation { tool: String, detail: String },

    /// The tool body failed (IO, spawn, patch apply, …).
    #[error("`{tool}` failed: {message}")]
    Failed { tool: String, message: String },
}

/// Context handed to every tool invocation.
///
/// Cheap to clone, so the registry can move one into each dispatch
/// future.
#[derive(Clone)]
pub struct ToolContext {
    /// Absolute workspace root. Filesystem tools must be confined to it
    /// (stage 1.6 rejects `..`/symlink escapes).
    pub workspace_root: PathBuf,

    /// Gate every dispatch passes through (stage 1.7). [`ToolContext::new`]
    /// installs [`AllowAll`] for tests and scaffolding; production wiring
    /// installs the config-backed policy gate (stage 1.9).
    pub permissions: super::permissions::Gate,
}

impl ToolContext {
    /// Policy-free context (`AllowAll`) — tests and scaffolding only.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self::with_gate(workspace_root, Arc::new(AllowAll))
    }

    /// Context carrying an explicit permission gate.
    pub fn with_gate(workspace_root: impl Into<PathBuf>, gate: super::permissions::Gate) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            permissions: gate,
        }
    }
}

impl fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolContext")
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

/// One tool the model can call.
///
/// Implementors provide a typed [`Self::Args`] struct deriving both
/// `serde::Deserialize` and `schemars::JsonSchema` — one source of truth
/// for boundary validation *and* the schema shown to the model.
pub trait Tool: Send + Sync {
    /// Typed argument object; deserialized (and thereby validated) by the
    /// registry before [`execute`][Self::execute] runs.
    type Args: DeserializeOwned + JsonSchema + Send + 'static;

    /// Stable snake_case name the model uses to call the tool.
    fn name(&self) -> &'static str;

    /// Shown to the model alongside the schema.
    fn description(&self) -> &'static str;

    /// JSON Schema for [`Self::Args`] (doc comments become property
    /// descriptions; `#[serde(default)]` marks properties optional).
    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(Self::Args))
            .expect("a JsonSchema type always serializes to a JSON Schema")
    }

    /// Provider-facing spec (name + description + schema) — feeds
    /// `CompletionRequest::tools`.
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name().to_string(),
            description: Some(self.description().to_string()),
            parameters: self.schema(),
        }
    }

    /// Boundary check: raw JSON → typed args. Any violation maps to
    /// [`ToolError::InvalidArgs`].
    fn parse(&self, args: &Value) -> Result<Self::Args, ToolError> {
        serde_json::from_value(args.clone()).map_err(|err| ToolError::InvalidArgs {
            tool: self.name().to_string(),
            message: err.to_string(),
        })
    }

    /// Run the tool. The registry stamps the real `tool_use_id` onto the
    /// returned [`ToolResult`] — tools may leave it empty.
    fn execute(
        &self,
        ctx: &ToolContext,
        args: Self::Args,
    ) -> impl Future<Output = Result<ToolResult, ToolError>> + Send;
}

/// A tool after type erasure: spec + validation + boxed dispatch.
struct RegisteredTool {
    spec: ToolSpec,
    validate: ValidateFn,
    run: RunFn,
}

/// Registry of tools available to the agent loop.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `tool`, replacing any prior tool with the same name.
    pub fn register<T: Tool + 'static>(&mut self, tool: T) {
        let tool = Arc::new(tool);
        let spec = tool.spec();

        let validate: ValidateFn = {
            let tool = Arc::clone(&tool);
            Box::new(move |args| tool.parse(args).map(|_| ()))
        };
        let run: RunFn = {
            let tool = Arc::clone(&tool);
            Box::new(move |ctx, args| {
                let tool = Arc::clone(&tool);
                Box::pin(async move {
                    // The boundary: invalid JSON never reaches the body.
                    let args = tool.parse(&args)?;
                    tool.execute(&ctx, args).await
                })
            })
        };

        // Upsert: same name replaces the previous registration.
        self.tools.retain(|old| old.spec.name != spec.name);
        self.tools.push(RegisteredTool {
            spec,
            validate,
            run,
        });
    }

    /// Specs of every registered tool, in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|tool| tool.spec.clone()).collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool.spec.name == name)
    }

    /// Validate arguments **without** dispatching (preflight / tests).
    pub fn validate(&self, name: &str, args: &Value) -> Result<(), ToolError> {
        (self.lookup(name)?.validate)(args)
    }

    /// Look up → validate at the boundary → dispatch, stamping
    /// `tool_use_id` on the result.
    pub async fn execute(
        &self,
        name: &str,
        ctx: &ToolContext,
        tool_use_id: &str,
        args: Value,
    ) -> Result<ToolResult, ToolError> {
        let tool = self.lookup(name)?;
        // The gate guards dispatch (stage 1.7): nothing reaches a tool
        // body past a `Deny`, and an unresolved `Ask` fails closed.
        let detail = args.to_string();
        match ctx.permissions.check(name, &detail) {
            Decision::Allow => {}
            Decision::Deny => {
                return Err(ToolError::Denied {
                    tool: name.to_string(),
                    detail,
                });
            }
            Decision::Ask => {
                return Err(ToolError::NeedsConfirmation {
                    tool: name.to_string(),
                    detail,
                });
            }
        }
        let result = (tool.run)(ctx.clone(), args).await?;
        Ok(ToolResult {
            tool_use_id: tool_use_id.to_string(),
            ..result
        })
    }

    fn lookup(&self, name: &str) -> Result<&RegisteredTool, ToolError> {
        self.tools
            .iter()
            .find(|tool| tool.spec.name == name)
            .ok_or_else(|| ToolError::UnknownTool {
                name: name.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolResult;
    use schemars::JsonSchema;
    use serde::Deserialize;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Debug, Deserialize, JsonSchema)]
    struct EchoArgs {
        /// Text to echo back.
        text: String,
        /// Uppercase the echo.
        #[serde(default)]
        shout: bool,
    }

    /// Fixture tool that records whether its body ever ran.
    struct EchoTool {
        executed: Arc<AtomicBool>,
    }

    impl Tool for EchoTool {
        type Args = EchoArgs;

        fn name(&self) -> &'static str {
            "echo"
        }

        fn description(&self) -> &'static str {
            "Echoes text back, optionally shouting"
        }

        async fn execute(
            &self,
            _ctx: &ToolContext,
            args: EchoArgs,
        ) -> Result<ToolResult, ToolError> {
            self.executed.store(true, Ordering::SeqCst);
            let content = if args.shout {
                args.text.to_uppercase()
            } else {
                args.text
            };
            Ok(ToolResult {
                tool_use_id: String::new(),
                content,
                is_error: false,
            })
        }
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    struct PingArgs {}

    struct PingTool;

    impl Tool for PingTool {
        type Args = PingArgs;

        fn name(&self) -> &'static str {
            "ping"
        }

        fn description(&self) -> &'static str {
            "Answers pong"
        }

        async fn execute(
            &self,
            _ctx: &ToolContext,
            _args: PingArgs,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                tool_use_id: String::new(),
                content: "pong".into(),
                is_error: false,
            })
        }
    }

    fn ctx() -> ToolContext {
        ToolContext::new("/tmp/iris-workspace")
    }

    #[tokio::test]
    async fn dispatch_invokes_tool_and_stamps_call_id() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&flag),
        });

        let result = registry
            .execute(
                "echo",
                &ctx(),
                "call_abc",
                serde_json::json!({"text": "hi", "shout": true}),
            )
            .await
            .unwrap();

        assert_eq!(
            result,
            ToolResult {
                tool_use_id: "call_abc".into(),
                content: "HI".into(),
                is_error: false,
            }
        );
        assert!(flag.load(Ordering::SeqCst), "tool body must run");
    }

    #[tokio::test]
    async fn unknown_tool_fails_without_dispatch() {
        let mut registry = ToolRegistry::new();
        registry.register(PingTool);

        let err = registry
            .execute("ghost", &ctx(), "c1", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::UnknownTool { ref name } if name == "ghost"),
            "got {err:?}"
        );
        assert!(registry.validate("ghost", &serde_json::json!({})).is_err());
        assert!(!registry.contains("ghost"));
    }

    #[tokio::test]
    async fn invalid_args_rejected_at_boundary_before_dispatch() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&flag),
        });

        // Wrong type …
        let err = registry
            .execute("echo", &ctx(), "c1", serde_json::json!({"text": 42}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::InvalidArgs { ref tool, .. } if tool == "echo"),
            "got {err:?}"
        );

        // … and missing required field.
        let err = registry
            .execute("echo", &ctx(), "c2", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "got {err:?}");

        assert!(
            !flag.load(Ordering::SeqCst),
            "tool body must never run on invalid args"
        );
    }

    #[tokio::test]
    async fn validate_checks_without_running() {
        let flag = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&flag),
        });

        registry
            .validate("echo", &serde_json::json!({"text": "x"}))
            .unwrap();
        assert!(
            registry
                .validate("echo", &serde_json::json!({"text": 7}))
                .is_err()
        );
        assert!(!flag.load(Ordering::SeqCst), "validate must not dispatch");
    }

    #[tokio::test]
    async fn specs_expose_schemars_schema_for_each_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::new(AtomicBool::new(false)),
        });
        registry.register(PingTool);

        let specs = registry.specs();
        assert_eq!(specs.len(), 2);

        let echo = &specs[0];
        assert_eq!(echo.name, "echo");
        assert_eq!(
            echo.description.as_deref(),
            Some("Echoes text back, optionally shouting")
        );
        assert_eq!(echo.parameters["type"], "object");
        assert_eq!(echo.parameters["properties"]["text"]["type"], "string");
        assert_eq!(echo.parameters["properties"]["shout"]["type"], "boolean");
        // serde(default) → optional → must not be required.
        assert_eq!(echo.parameters["required"], serde_json::json!(["text"]));
        insta::assert_json_snapshot!("echo_tool_parameters", echo.parameters);

        assert_eq!(specs[1].name, "ping");
        assert!(registry.contains("echo"));
        assert!(registry.contains("ping"));
    }

    #[tokio::test]
    async fn no_arg_tool_accepts_empty_json_object() {
        // OpenAI convention (stage 1.2): no-arg calls arrive as "{}".
        let mut registry = ToolRegistry::new();
        registry.register(PingTool);

        let result = registry
            .execute("ping", &ctx(), "call_1", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result.content, "pong");
        assert_eq!(result.tool_use_id, "call_1");
    }

    #[tokio::test]
    async fn re_registering_a_name_replaces_the_previous_tool() {
        let first = Arc::new(AtomicBool::new(false));
        let second = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&first),
        });
        registry.register(EchoTool {
            executed: Arc::clone(&second),
        });

        assert_eq!(registry.specs().len(), 1);
        registry
            .execute("echo", &ctx(), "c", serde_json::json!({"text": "x"}))
            .await
            .unwrap();
        assert!(!first.load(Ordering::SeqCst), "first registration replaced");
        assert!(second.load(Ordering::SeqCst), "second registration active");
    }

    // --- Permission gate seam (stage 1.7) -------------------------------

    fn gated_ctx(gate: super::super::permissions::Gate) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::with_gate(dir.path(), gate);
        (dir, ctx)
    }

    #[tokio::test]
    async fn deny_gate_blocks_dispatch_before_the_body_runs() {
        let executed = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&executed),
        });
        let (_tmp, ctx) = gated_ctx(Arc::new(super::super::permissions::DenyAll));

        let err = registry
            .execute("echo", &ctx, "c", serde_json::json!({"text": "hi"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
        assert!(
            !executed.load(Ordering::SeqCst),
            "denied tool body must never run"
        );
    }

    #[tokio::test]
    async fn unresolved_ask_fails_closed_never_runs_the_body() {
        let executed = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&executed),
        });
        let (_tmp, ctx) = gated_ctx(Arc::new(|_: &str, _: &str| Decision::Ask));

        let err = registry
            .execute("echo", &ctx, "c", serde_json::json!({"text": "hi"}))
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::NeedsConfirmation { .. }),
            "got {err:?}"
        );
        assert!(!executed.load(Ordering::SeqCst), "ask must fail closed");
    }

    #[tokio::test]
    async fn closure_gate_can_target_a_single_tool() {
        let executed = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&executed),
        });
        // Per-tool policy: only "echo" is denied; anything else allowed.
        let (_tmp, ctx) = gated_ctx(Arc::new(|tool: &str, _detail: &str| {
            if tool == "echo" {
                Decision::Deny
            } else {
                Decision::Allow
            }
        }));

        let err = registry
            .execute("echo", &ctx, "c", serde_json::json!({"text": "hi"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "got {err:?}");
        assert!(
            registry
                .execute("nope", &ctx, "c", serde_json::json!({}))
                .await
                .is_err_and(|e| matches!(e, ToolError::UnknownTool { .. }))
        );
    }

    #[tokio::test]
    async fn allow_context_still_dispatches_normally() {
        let executed = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(EchoTool {
            executed: Arc::clone(&executed),
        });
        let (_tmp, ctx) = gated_ctx(Arc::new(|_: &str, _: &str| Decision::Allow));

        let result = registry
            .execute("echo", &ctx, "c", serde_json::json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(result.content, "hi");
        assert!(executed.load(Ordering::SeqCst));
    }
}
