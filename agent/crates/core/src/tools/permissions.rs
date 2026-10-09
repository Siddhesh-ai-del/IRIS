//! Permission gate seam (stage 1.7): **every dispatch passes through a
//! [`PermissionGate`]** — the shell tool can never be a bypass path.
//!
//! Stage 1.9 completes the picture: [`PolicyGate`] applies the config's
//! per-tool `allow`/`deny`/`ask` policy (overlaying [`builtin_policy`]),
//! and the loop resolves `ask` through an [`AskResolver`] before
//! dispatch. Contexts built with [`ToolContext::new`] still carry
//! [`AllowAll`] (test and scaffolding contexts); the registry fails
//! **closed** on both `Deny` and unresolved `Ask`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// What the gate says about a proposed tool call — the plan's policy
/// vocabulary (`allow` / `deny` / `ask`), also serialized by the
/// config's `[permissions]` table (stage 1.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    /// Needs human confirmation. Tools/registry cannot prompt, so an
    /// `Ask` that reaches dispatch fails closed until the caller
    /// (the loop) resolves it to `Allow` or `Deny`.
    Ask,
}

/// Checkpoint every tool dispatch goes through.
pub trait PermissionGate: Send + Sync {
    /// `detail` is the raw argument JSON — enough for an audit line or an
    /// `ask` preview (the TUI renders command/diff previews at stage 4.5).
    fn check(&self, tool: &str, detail: &str) -> Decision;
}

/// Permit everything. Test scaffolding and explicitly opt-in contexts
/// only — production wiring installs a policy gate (stage 1.9).
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAll;

impl PermissionGate for AllowAll {
    fn check(&self, _tool: &str, _detail: &str) -> Decision {
        Decision::Allow
    }
}

/// Deny everything — locked-down contexts.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyAll;

impl PermissionGate for DenyAll {
    fn check(&self, _tool: &str, _detail: &str) -> Decision {
        Decision::Deny
    }
}

/// Closures are gates: `|tool, detail| Decision::…` (per-tool policies,
/// test fixtures, session-scoped overrides).
impl<F> PermissionGate for F
where
    F: Fn(&str, &str) -> Decision + Send + Sync,
{
    fn check(&self, tool: &str, detail: &str) -> Decision {
        self(tool, detail)
    }
}

/// Convenience for the common `Arc<dyn PermissionGate>` wiring.
pub type Gate = Arc<dyn PermissionGate>;

/// Built-in policy for tools **not** named in the config overlay (stage
/// 1.9): the workspace-confined, read-only tools are allowed —
/// everything else asks until configured, so a fresh install fails safe.
pub fn builtin_policy(tool: &str) -> Decision {
    match tool {
        "read_file" | "list_dir" => Decision::Allow,
        _ => Decision::Ask,
    }
}

/// The config-backed policy gate: `[permissions]` entries from the
/// config file overlay [`builtin_policy`]. Installed into the loop's
/// [`ToolContext`](crate::tools::ToolContext) so every dispatch passes
/// through it (stage 1.7's seam, stage 1.9's engine).
#[derive(Debug, Default, Clone)]
pub struct PolicyGate {
    overlay: BTreeMap<String, Decision>,
}

impl PolicyGate {
    pub fn new(overlay: BTreeMap<String, Decision>) -> Self {
        Self { overlay }
    }
}

impl PermissionGate for PolicyGate {
    fn check(&self, tool: &str, _detail: &str) -> Decision {
        self.overlay
            .get(tool)
            .copied()
            .unwrap_or_else(|| builtin_policy(tool))
    }
}

/// Resolves [`Decision::Ask`] where a human — or its stand-in — can be
/// consulted. The loop asks **before** dispatch; the TUI prompt (stage
/// 4.5) will implement this trait, while headless mode and tests use
/// the provided stand-ins and closures.
pub trait AskResolver: Send + Sync {
    /// Return `Allow` to run the call; **anything else is treated as a
    /// refusal** (fail closed).
    fn confirm(&self, tool: &str, detail: &str) -> Decision;
}

/// Headless default: there is no UI to ask, so every `Ask` is refused.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenyOnAsk;

impl AskResolver for DenyOnAsk {
    fn confirm(&self, _tool: &str, _detail: &str) -> Decision {
        Decision::Deny
    }
}

/// Closures are resolvers: `|tool, detail| Decision::…` (test doubles,
/// scripted approvals, session-wide rules).
impl<F> AskResolver for F
where
    F: Fn(&str, &str) -> Decision + Send + Sync,
{
    fn confirm(&self, tool: &str, detail: &str) -> Decision {
        self(tool, detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_all_permits_anything() {
        let gate = AllowAll;
        assert_eq!(
            gate.check("run_command", "{\"command\":\"rm -rf /\"}"),
            Decision::Allow
        );
        assert_eq!(gate.check("write_file", "{}"), Decision::Allow);
    }

    #[test]
    fn deny_all_refuses_anything() {
        let gate = DenyAll;
        assert_eq!(gate.check("read_file", "{}"), Decision::Deny);
    }

    #[test]
    fn closures_implement_the_gate() {
        let gate = |tool: &str, _detail: &str| {
            if tool == "run_command" {
                Decision::Ask
            } else {
                Decision::Allow
            }
        };
        assert_eq!(gate.check("run_command", "{}"), Decision::Ask);
        assert_eq!(gate.check("read_file", "{}"), Decision::Allow);
    }

    #[test]
    fn gates_are_object_safe_and_arc_shareable() {
        let gate: Gate = Arc::new(DenyAll);
        assert_eq!(gate.check("any", ""), Decision::Deny);
    }

    // --- config-backed policy + ask resolution (stage 1.9) ----------------

    #[test]
    fn policy_gate_overlay_beats_the_builtin_policy() {
        let mut overlay = BTreeMap::new();
        overlay.insert("run_command".to_string(), Decision::Allow);
        let gate = PolicyGate::new(overlay);
        assert_eq!(gate.check("run_command", "{}"), Decision::Allow);
        assert_eq!(gate.check("write_file", "{}"), Decision::Ask);
    }

    #[test]
    fn builtin_policy_allows_reads_and_asks_for_everything_else() {
        assert_eq!(builtin_policy("read_file"), Decision::Allow);
        assert_eq!(builtin_policy("list_dir"), Decision::Allow);
        assert_eq!(builtin_policy("write_file"), Decision::Ask);
        assert_eq!(builtin_policy("apply_patch"), Decision::Ask);
        assert_eq!(builtin_policy("run_command"), Decision::Ask);
        assert_eq!(builtin_policy("future_tool"), Decision::Ask);
    }

    #[test]
    fn empty_policy_gate_falls_back_to_builtin_policy() {
        let gate = PolicyGate::new(BTreeMap::new());
        assert_eq!(gate.check("read_file", "{}"), Decision::Allow);
        assert_eq!(gate.check("run_command", "{}"), Decision::Ask);
    }

    #[test]
    fn decision_serializes_as_lowercase_policy_names() {
        assert_eq!(
            serde_json::to_value(Decision::Deny).unwrap(),
            serde_json::json!("deny")
        );
        let parsed: Decision = serde_json::from_str("\"allow\"").unwrap();
        assert_eq!(parsed, Decision::Allow);
    }

    #[test]
    fn deny_on_ask_refuses_without_a_ui() {
        // Headless default: nothing can prompt → fail closed.
        assert_eq!(DenyOnAsk.confirm("run_command", "{}"), Decision::Deny);
    }

    #[test]
    fn closures_can_resolve_asks() {
        let resolver: &dyn AskResolver = &|_tool: &str, _detail: &str| Decision::Allow;
        assert_eq!(resolver.confirm("run_command", "{}"), Decision::Allow);
    }
}
