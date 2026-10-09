//! Permission gate seam (stage 1.7): **every dispatch passes through a
//! [`PermissionGate`]** — the shell tool can never be a bypass path.
//!
//! Stage 1.9 installs the config-backed policy engine (`allow` / `deny` /
//! `ask` per tool) and resolves `ask` through the TUI prompt; until then
//! contexts built with [`ToolContext::new`] carry [`AllowAll`] (test and
//! scaffolding contexts). The registry fails **closed** on both `Deny`
//! and unresolved `Ask`.

use std::sync::Arc;

/// What the gate says about a proposed tool call — the plan's policy
/// vocabulary (`allow` / `deny` / `ask`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// Needs human confirmation. Tools/registry cannot prompt, so an
    /// `Ask` that reaches dispatch fails closed until the caller
    /// (loop, stage 1.9) resolves it to `Allow` or `Deny`.
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
}
