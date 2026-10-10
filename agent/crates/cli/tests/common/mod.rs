//! Shared E2E harness for the `iris` binary (stage 1.10+).
//!
//! Every stage's E2E suite builds on this: a hermetic [`iris`] command
//! (no real config file, no real key, no ambient `IRIS_*` overrides) and
//! SSE body builders matching the stage-1.3 wire shape. The builders are
//! copied from `crates/core/tests` rather than shared — integration test
//! crates cannot import each other without a dedicated test-support
//! crate, and ~30 lines of duplication beats a public test API.

#![allow(dead_code)] // each test binary uses a subset of the harness

use std::process::Command;

use serde_json::{Value, json};

/// A hermetic `iris` invocation: the real config file, the real API key
/// and any ambient `IRIS_*` overrides are stripped; each test adds back
/// exactly what it needs.
pub fn iris() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_iris"));
    cmd.env("IRIS_CONFIG", "/nonexistent/iris-e2e-config.toml")
        .env_remove("OPENROUTER_API_KEY")
        .env_remove("IRIS_PROVIDER_BASE_URL");
    cmd
}

/// SSE body in the stage-1.3 wire shape: role preamble, the given delta
/// lines, a finish chunk, a usage chunk, then `[DONE]`.
pub fn sse_body(delta_lines: &str, finish: &str, usage: (u64, u64, u64)) -> String {
    let (prompt, completion, total) = usage;
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"role\":\"assistant\"}},\"finish_reason\":null}}]}}\n\n\
         {delta_lines}\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"{finish}\"}}]}}\n\n\
         data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\
         \"completion_tokens\":{completion},\"total_tokens\":{total}}}}}\n\n\
         data: [DONE]\n\n"
    )
}

pub fn text_delta(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":null}}]}}\n\n",
        json!(text)
    )
}

pub fn tool_delta(index: u32, id: &str, name: &str, args: &Value) -> String {
    let call = json!({
        "index": index,
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": args.to_string()},
    });
    format!(
        "data: {}\n\n",
        json!({"choices": [{"delta": {"tool_calls": [call]}, "finish_reason": null}]})
    )
}
