//! Domain types for the agent loop (stage 1.1).
//!
//! Provider-agnostic wire model: these types are what the loop, session
//! schema, and tools exchange. The OpenRouter client (stage 1.2) translates
//! to/from its wire format at the boundary.

use serde::{Deserialize, Serialize};

/// Who authored a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One unit of message content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
    ToolUse(ToolCall),
    ToolResult(ToolResult),
}

/// An assistant-issued tool invocation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The outcome of executing a tool call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// Id of the [`ToolCall`] this answers.
    pub tool_use_id: String,
    /// Captured output (stdout/diff/text). Redacted *before* persistence
    /// in stage 5.1.
    pub content: String,
    /// Whether execution failed (nonzero exit, IO error, …).
    pub is_error: bool,
}

/// A single conversation turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Convenience constructor for a plain-text message.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

/// Token accounting for one model response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

/// Why the model stopped generating.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    ContentFilter,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // --- round-trips (serialize → deserialize → equal) -------------------

    #[test]
    fn role_round_trip() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            let json = serde_json::to_string(&role).unwrap();
            let back: Role = serde_json::from_str(&json).unwrap();
            assert_eq!(role, back);
        }
    }

    #[test]
    fn content_block_round_trip_each_variant() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::Image {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
            ContentBlock::ToolUse(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({"path": "src/main.rs"}),
            }),
            ContentBlock::ToolResult(ToolResult {
                tool_use_id: "call_1".into(),
                content: "fn main() {}".into(),
                is_error: false,
            }),
        ];
        for block in blocks {
            let json = serde_json::to_string(&block).unwrap();
            let back: ContentBlock = serde_json::from_str(&json).unwrap();
            assert_eq!(block, back);
        }
    }

    #[test]
    fn tool_call_input_preserves_arbitrary_json() {
        let call = ToolCall {
            id: "c".into(),
            name: "shell".into(),
            input: json!({"cmd": ["ls", "-la"], "timeout_ms": 5000, "nested": {"a": [1, 2, 3]}}),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call.input, back.input);
    }

    #[test]
    fn message_round_trip() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Reading the file…".into(),
                },
                ContentBlock::ToolUse(ToolCall {
                    id: "call_9".into(),
                    name: "read".into(),
                    input: json!({"path": "Cargo.toml"}),
                }),
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn usage_round_trip_and_default() {
        let usage = Usage {
            input_tokens: 120,
            output_tokens: 34,
            total_tokens: 154,
        };
        let json = serde_json::to_string(&usage).unwrap();
        let back: Usage = serde_json::from_str(&json).unwrap();
        assert_eq!(usage, back);
        assert_eq!(Usage::default().total_tokens, 0);
    }

    #[test]
    fn stop_reason_round_trip() {
        for reason in [
            StopReason::EndTurn,
            StopReason::ToolUse,
            StopReason::MaxTokens,
            StopReason::StopSequence,
            StopReason::ContentFilter,
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            let back: StopReason = serde_json::from_str(&json).unwrap();
            assert_eq!(reason, back);
        }
    }

    // --- wire-shape assertions (lock the JSON contract) -------------------

    #[test]
    fn content_block_uses_tagged_type_field() {
        let block = ContentBlock::Text { text: "hi".into() };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"type": "text", "text": "hi"})
        );

        let block = ContentBlock::ToolUse(ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            input: json!({"path": "x"}),
        });
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({
                "type": "tool_use",
                "id": "call_1",
                "name": "read",
                "input": {"path": "x"}
            })
        );
    }

    #[test]
    fn role_uses_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_value(Role::Assistant).unwrap(),
            json!("assistant")
        );
        assert_eq!(serde_json::to_value(Role::Tool).unwrap(), json!("tool"));
    }

    #[test]
    fn stop_reason_uses_snake_case_wire_names() {
        assert_eq!(
            serde_json::to_value(StopReason::ToolUse).unwrap(),
            json!("tool_use")
        );
        assert_eq!(
            serde_json::to_value(StopReason::MaxTokens).unwrap(),
            json!("max_tokens")
        );
    }

    #[test]
    fn message_text_helper_builds_single_text_block() {
        let msg = Message::text(Role::User, "ping");
        assert_eq!(msg.role, Role::User);
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            json!({"role": "user", "content": [{"type": "text", "text": "ping"}]})
        );
    }

    #[test]
    fn unknown_stop_reason_fails_deserialization() {
        // Guard: silent fallback to a wrong reason would corrupt metrics.
        assert!(serde_json::from_str::<StopReason>("\"self_destruct\"").is_err());
    }

    // --- insta snapshot of the serialized JSON ----------------------------

    #[test]
    fn snapshot_full_conversation_json() {
        let convo = vec![
            Message::text(Role::System, "You are ferro."),
            Message::text(Role::User, "Read main.rs"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Reading it now.".into(),
                    },
                    ContentBlock::ToolUse(ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        input: json!({"path": "src/main.rs"}),
                    }),
                ],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "fn main() {}\n".into(),
                    is_error: false,
                })],
            },
        ];
        insta::assert_json_snapshot!(convo);
    }
}
