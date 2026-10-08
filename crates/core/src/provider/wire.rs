//! OpenAI-compatible wire format (chat completions) ⇄ domain types.
//!
//! Translation rules live here so the client (`openai.rs`) only deals with
//! HTTP. Wire quirks handled:
//!
//! - assistant `tool_calls[].function.arguments` is a **JSON string**,
//!   not an object; empty string means no-arg call → `{}`
//! - one domain tool-role `Message` can hold several `ToolResult` blocks
//!   (parallel calls) → **several** wire messages, one per `tool_call_id`
//! - user content is a plain string when all-text, parts array with
//!   `image_url` data URLs when images are present
//! - missing `finish_reason` is inferred (tool calls → tool_use, else
//!   end_turn); an *unknown* value fails loud (no silent guessing)

use serde::{Deserialize, Serialize};

use super::{CompletionRequest, CompletionResponse, ProviderError};
use crate::types::{ContentBlock, Message, Role, StopReason, ToolCall, Usage};

// --- request -------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WireRequest {
    pub model: String,
    pub messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Omitted for non-streaming; `Some(true)` on the streaming path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Only meaningful with `stream`; asks for the trailing usage chunk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WireMessage {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<WireContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<WireToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireContent {
    Text(String),
    Parts(Vec<WirePart>),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WirePart {
    Text { text: String },
    ImageUrl { image_url: WireImageUrl },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireImageUrl {
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_kind")]
    pub kind: String,
    pub function: WireFunction,
}

fn default_function_kind() -> String {
    "function".to_string()
}

/// Request direction: `arguments` serializes from structured input.
/// Response direction: `arguments` arrives as a JSON string.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: WireToolSpec,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireToolSpec {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: serde_json::Value,
}

/// Domain request → OpenAI wire request (flattens tool-role messages).
pub fn request_to_wire(request: CompletionRequest) -> WireRequest {
    build_wire_request(request, false)
}

/// Same as [`request_to_wire`], but flagged for SSE streaming with a
/// trailing usage chunk (`stream_options.include_usage`).
pub fn request_to_streaming_wire(request: CompletionRequest) -> WireRequest {
    build_wire_request(request, true)
}

fn build_wire_request(request: CompletionRequest, stream: bool) -> WireRequest {
    let mut messages = Vec::new();
    for message in &request.messages {
        match message.role {
            Role::System => messages.push(WireMessage {
                role: Role::System,
                content: Some(WireContent::Text(join_text(&message.content))),
                tool_calls: None,
                tool_call_id: None,
            }),
            Role::User => messages.push(WireMessage {
                role: Role::User,
                content: Some(user_content(&message.content)),
                tool_calls: None,
                tool_call_id: None,
            }),
            Role::Assistant => {
                let text = join_text(&message.content);
                let tool_calls: Vec<WireToolCall> = message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolUse(call) => Some(WireToolCall {
                            id: call.id.clone(),
                            kind: default_function_kind(),
                            function: WireFunction {
                                name: call.name.clone(),
                                arguments: call.input.to_string(),
                            },
                        }),
                        _ => None,
                    })
                    .collect();
                messages.push(WireMessage {
                    role: Role::Assistant,
                    // No text → omit content (OpenAI treats absent as null).
                    content: if text.is_empty() {
                        None
                    } else {
                        Some(WireContent::Text(text))
                    },
                    tool_calls: if tool_calls.is_empty() {
                        None
                    } else {
                        Some(tool_calls)
                    },
                    tool_call_id: None,
                });
            }
            // One wire message per ToolResult (parallel calls each need
            // their own tool_call_id).
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult(result) = block {
                        messages.push(WireMessage {
                            role: Role::Tool,
                            content: Some(WireContent::Text(result.content.clone())),
                            tool_calls: None,
                            tool_call_id: Some(result.tool_use_id.clone()),
                        });
                    }
                }
            }
        }
    }

    WireRequest {
        model: request.model,
        messages,
        tools: request
            .tools
            .iter()
            .map(|tool| WireTool {
                kind: "function".to_string(),
                function: WireToolSpec {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.parameters.clone(),
                },
            })
            .collect(),
        max_tokens: request.max_tokens,
        temperature: request.temperature,
        stream: stream.then_some(true),
        stream_options: stream.then_some(StreamOptions {
            include_usage: true,
        }),
    }
}

/// Concatenate the text blocks (images excluded) for string-content roles.
fn join_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// User content: plain string when all-text, parts array when images.
fn user_content(blocks: &[ContentBlock]) -> WireContent {
    let has_image = blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    if !has_image {
        return WireContent::Text(join_text(blocks));
    }
    WireContent::Parts(
        blocks
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => WirePart::Text { text: text.clone() },
                ContentBlock::Image { media_type, data } => WirePart::ImageUrl {
                    image_url: WireImageUrl {
                        url: format!("data:{media_type};base64,{data}"),
                    },
                },
                // Tool blocks have no place in a user turn.
                other => panic!("unexpected block in user message: {other:?}"),
            })
            .collect(),
    )
}

// --- response ------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WireResponse {
    pub choices: Vec<WireChoice>,
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
pub struct WireChoice {
    pub message: WireAssistantMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WireAssistantMessage {
    #[serde(default)]
    pub content: Option<WireContent>,
    #[serde(default)]
    pub tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// OpenAI wire response → domain completion.
pub fn response_to_domain(response: WireResponse) -> Result<CompletionResponse, ProviderError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(ProviderError::NoChoices)?;

    let mut content = Vec::new();
    if let Some(wire_content) = choice.message.content {
        let text = match wire_content {
            WireContent::Text(text) => text,
            WireContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    WirePart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        if !text.is_empty() {
            content.push(ContentBlock::Text { text });
        }
    }

    let has_tool_calls = choice
        .message
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if let Some(calls) = choice.message.tool_calls {
        for call in calls {
            // arguments is a JSON *string*; "" (no-arg call) → {}.
            let input = if call.function.arguments.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&call.function.arguments)?
            };
            content.push(ContentBlock::ToolUse(ToolCall {
                id: call.id,
                name: call.function.name,
                input,
            }));
        }
    }

    let stop_reason = map_finish_reason(choice.finish_reason.as_deref(), has_tool_calls)?;
    let usage = usage_from_wire(response.usage);

    Ok(CompletionResponse {
        message: Message {
            role: Role::Assistant,
            content,
        },
        stop_reason,
        usage,
    })
}

/// `finish_reason` → [`StopReason`] (shared by both streaming and
/// non-streaming paths). Unknown values fail loud; missing values infer
/// from the message shape.
pub(crate) fn map_finish_reason(
    finish_reason: Option<&str>,
    has_tool_calls: bool,
) -> Result<StopReason, ProviderError> {
    match finish_reason {
        Some("stop") => Ok(StopReason::EndTurn),
        Some("length") => Ok(StopReason::MaxTokens),
        Some("tool_calls") => Ok(StopReason::ToolUse),
        Some("content_filter") => Ok(StopReason::ContentFilter),
        Some("stop_sequence") => Ok(StopReason::StopSequence),
        Some(other) => Err(ProviderError::UnknownFinishReason(other.to_string())),
        None if has_tool_calls => Ok(StopReason::ToolUse),
        None => Ok(StopReason::EndTurn),
    }
}

/// Wire usage accounting → domain (missing → zero, both paths).
pub(crate) fn usage_from_wire(usage: Option<WireUsage>) -> Usage {
    match usage {
        Some(wire) => Usage {
            input_tokens: wire.prompt_tokens,
            output_tokens: wire.completion_tokens,
            total_tokens: wire.total_tokens,
        },
        None => Usage::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolSpec;
    use crate::types::ToolResult;
    use serde_json::json;

    fn request(messages: Vec<Message>) -> CompletionRequest {
        CompletionRequest {
            model: "test-model".into(),
            messages,
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    // --- request direction ---------------------------------------------

    #[test]
    fn plain_text_conversation_serializes_to_wire_shape() {
        let wire = request_to_wire(request(vec![
            Message::text(Role::System, "You are ferro."),
            Message::text(Role::User, "hello"),
        ]));
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({
                "model": "test-model",
                "messages": [
                    {"role": "system", "content": "You are ferro."},
                    {"role": "user", "content": "hello"}
                ]
            })
        );
    }

    #[test]
    fn user_image_becomes_data_url_part() {
        let msg = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this?".into(),
                },
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".into(),
                },
            ],
        };
        let wire = request_to_wire(request(vec![msg]));
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            value["messages"][0]["content"],
            json!([
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aGVsbG8="}}
            ])
        );
    }

    #[test]
    fn assistant_tool_use_becomes_tool_calls_with_string_arguments() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse(ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({"path": "src/main.rs"}),
            })],
        };
        let wire = request_to_wire(request(vec![msg]));
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(
            value["messages"][0]["tool_calls"],
            json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"src/main.rs\"}"}
            }])
        );
        // no text → content omitted entirely (OpenAI: null/absent both ok)
        assert!(value["messages"][0].get("content").is_none());
    }

    #[test]
    fn parallel_tool_results_flatten_to_one_wire_message_each() {
        let msg = Message {
            role: Role::Tool,
            content: vec![
                ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "first".into(),
                    is_error: false,
                }),
                ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "call_2".into(),
                    content: "second".into(),
                    is_error: true,
                }),
            ],
        };
        let wire = request_to_wire(request(vec![msg]));
        assert_eq!(
            serde_json::to_value(&wire).unwrap()["messages"],
            json!([
                {"role": "tool", "tool_call_id": "call_1", "content": "first"},
                {"role": "tool", "tool_call_id": "call_2", "content": "second"}
            ])
        );
    }

    #[test]
    fn tools_serialize_in_openai_function_shape() {
        let mut req = request(vec![]);
        req.tools = vec![ToolSpec {
            name: "read".into(),
            description: Some("Read a file".into()),
            parameters: json!({"type": "object", "properties": {}}),
        }];
        let value = serde_json::to_value(request_to_wire(req)).unwrap();
        assert_eq!(
            value["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {}}
                }
            }])
        );
    }

    #[test]
    fn absent_optionals_are_omitted_from_json() {
        let value = serde_json::to_value(request_to_wire(request(vec![]))).unwrap();
        assert!(value.get("tools").is_none());
        assert!(value.get("max_tokens").is_none());
        assert!(value.get("temperature").is_none());
    }

    // --- response direction ---------------------------------------------

    #[test]
    fn text_response_maps_to_end_turn_with_usage() {
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hi there!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
        }))
        .unwrap();
        let domain = response_to_domain(response).unwrap();
        assert_eq!(domain.message, Message::text(Role::Assistant, "Hi there!"));
        assert_eq!(domain.stop_reason, StopReason::EndTurn);
        assert_eq!(
            domain.usage,
            Usage {
                input_tokens: 12,
                output_tokens: 4,
                total_tokens: 16
            }
        );
    }

    #[test]
    fn tool_call_response_maps_to_tool_use_blocks() {
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"}},
                        {"id": "c2", "type": "function",
                         "function": {"name": "shell", "arguments": ""}}
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        let domain = response_to_domain(response).unwrap();
        let blocks: Vec<_> = domain.message.content.iter().collect();
        assert_eq!(blocks.len(), 2);
        match &domain.message.content[0] {
            ContentBlock::ToolUse(call) => {
                assert_eq!(call.id, "c1");
                assert_eq!(call.input, json!({"path": "a.rs"}));
            }
            other => panic!("expected tool_use, got {other:?}"),
        }
        match &domain.message.content[1] {
            // empty arguments string = no-arg call → {}
            ContentBlock::ToolUse(call) => assert_eq!(call.input, json!({})),
            other => panic!("expected tool_use, got {other:?}"),
        }
        assert_eq!(domain.message.role, Role::Assistant);
        assert_eq!(domain.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn assistant_text_and_tool_calls_coexist() {
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {
                    "content": "Let me read that.",
                    "tool_calls": [{"id": "c1", "function": {"name": "read", "arguments": "{}"}}]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        let domain = response_to_domain(response).unwrap();
        assert_eq!(domain.message.content.len(), 2);
        assert!(matches!(
            domain.message.content[0],
            ContentBlock::Text { .. }
        ));
        assert!(matches!(
            domain.message.content[1],
            ContentBlock::ToolUse(_)
        ));
    }

    #[test]
    fn missing_finish_reason_is_inferred_not_guessed_silently() {
        // tool calls present → tool_use
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {"tool_calls": [{"id": "c", "function": {"name": "x", "arguments": "{}"}}]}
            }]
        }))
        .unwrap();
        assert_eq!(
            response_to_domain(response).unwrap().stop_reason,
            StopReason::ToolUse
        );

        // no tool calls → end_turn
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "done"}}]
        }))
        .unwrap();
        assert_eq!(
            response_to_domain(response).unwrap().stop_reason,
            StopReason::EndTurn
        );
    }

    #[test]
    fn unknown_finish_reason_fails_loud() {
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "quantum_flux"}]
        }))
        .unwrap();
        assert!(matches!(
            response_to_domain(response),
            Err(ProviderError::UnknownFinishReason(reason)) if reason == "quantum_flux"
        ));
    }

    #[test]
    fn missing_usage_defaults_to_zero() {
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{"message": {"content": "x"}, "finish_reason": "stop"}]
        }))
        .unwrap();
        assert_eq!(
            response_to_domain(response).unwrap().usage,
            Usage::default()
        );
    }

    #[test]
    fn empty_choices_is_no_choices_error() {
        let response: WireResponse =
            serde_json::from_value(json!({"choices": [], "usage": null})).unwrap();
        assert!(matches!(
            response_to_domain(response),
            Err(ProviderError::NoChoices)
        ));
    }

    #[test]
    fn malformed_arguments_fail_as_decode_not_panic() {
        let response: WireResponse = serde_json::from_value(json!({
            "choices": [{
                "message": {"tool_calls": [
                    {"id": "c", "function": {"name": "x", "arguments": "{not json"}}
                ]},
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        assert!(matches!(
            response_to_domain(response),
            Err(ProviderError::Decode(_))
        ));
    }
}
