//! Stage 6.3 test fixture — a fake MCP server speaking NDJSON-RPC over
//! stdio, spawned by `crates/cli/tests/mcp.rs` through the same
//! `TokioChildProcess` path real servers take.
//!
//! Env knobs (passed via the config's `[mcp.servers.*.env]` table):
//!   - `IRIS_FAKE_MCP_TOOLS`      comma-separated tool names
//!     (default `alpha,beta`)
//!   - `IRIS_FAKE_MCP_BIG_BYTES`  payload size for the `big` tool
//!     (default `100000`)

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn main() {
    let tools: Vec<String> = std::env::var("IRIS_FAKE_MCP_TOOLS")
        .unwrap_or_else(|_| "alpha,beta".to_string())
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect();
    let big_bytes: usize = std::env::var("IRIS_FAKE_MCP_BIG_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100_000);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        // One JSON message per line (MCP stdio framing).
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications (`notifications/initialized`) carry no id —
        // nothing to answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let response = match method.as_str() {
            // Echo the client's proposed version back — negotiation
            // succeeds whenever both sides share a version.
            "initialize" => ok(
                &id,
                json!({
                    "protocolVersion": params
                        .get("protocolVersion")
                        .and_then(Value::as_str)
                        .unwrap_or("2025-06-18"),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "iris-fake-mcp", "version": "0.1.0" },
                }),
            ),
            "ping" => ok(&id, json!({})),
            "tools/list" => ok(
                &id,
                json!({
                    "tools": tools
                        .iter()
                        .map(|name| json!({
                            "name": name,
                            "description": format!("fixture tool {name}"),
                            "inputSchema": {
                                "type": "object",
                                "properties": { "value": { "type": "string" } },
                            },
                        }))
                        .collect::<Vec<_>>(),
                }),
            ),
            "tools/call" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if name == "big" {
                    ok(
                        &id,
                        json!({ "content": [{ "type": "text", "text": "x".repeat(big_bytes) }] }),
                    )
                } else if tools.iter().any(|tool| tool == name) {
                    ok(
                        &id,
                        json!({ "content": [{ "type": "text", "text": format!("echo:{arguments}") }] }),
                    )
                } else {
                    err(&id, -32602, &format!("unknown tool: {name}"))
                }
            }
            other => err(&id, -32601, &format!("method not found: {other}")),
        };

        serde_json::to_writer(&mut stdout, &response).expect("write response");
        stdout.write_all(b"\n").expect("write newline");
        stdout.flush().expect("flush response");
    }
}
