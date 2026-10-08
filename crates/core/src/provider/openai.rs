//! OpenAI-compatible HTTP client (OpenRouter by default — decision D4).
//!
//! Auth: `Authorization: Bearer $OPENROUTER_API_KEY` only (D6 — the key is
//! passed in by the caller, never read from config files here).

use std::time::Duration;

use super::wire::{self, WireRequest, WireResponse};
use super::{CompletionRequest, CompletionResponse, DEFAULT_TIMEOUT_SECS, Provider, ProviderError};

/// Non-streaming `POST {base_url}/chat/completions` client.
pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    http: reqwest::Client,
}

impl OpenAiClient {
    /// `base_url` without a trailing slash, e.g. `https://openrouter.ai/api/v1`.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            http: reqwest::Client::new(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn map_http_error(status: reqwest::StatusCode, body: String) -> ProviderError {
        // OpenRouter/OpenAI error body: {"error": {"message": "..."}}
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(|message| message.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    status.to_string()
                } else {
                    trimmed.chars().take(200).collect()
                }
            });
        ProviderError::Api {
            status: status.as_u16(),
            message,
        }
    }
}

impl Provider for OpenAiClient {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let body: WireRequest = wire::request_to_wire(request);
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(Self::map_http_error(status, text));
        }

        let wire_response: WireResponse = serde_json::from_str(&text)?;
        wire::response_to_domain(wire_response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, Role};
    use httpmock::prelude::*;
    use serde_json::json;

    fn request() -> CompletionRequest {
        CompletionRequest {
            model: "test-model".into(),
            messages: vec![Message::text(Role::User, "hello")],
            tools: vec![],
            max_tokens: None,
            temperature: None,
        }
    }

    /// Register one canned chat-completions reply on an existing server.
    fn mock_chat(server: &MockServer, status: u16, body: serde_json::Value) -> httpmock::Mock<'_> {
        server.mock(|when, then| {
            when.method(POST).path("/chat/completions");
            then.status(status).json_body(body);
        })
    }

    fn canned_text_reply() -> serde_json::Value {
        json!({
            "choices": [{
                "message": {"role": "assistant", "content": "Hi!"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        })
    }

    #[tokio::test]
    async fn non_streaming_completion_round_trip() {
        let server = MockServer::start();
        let mock = mock_chat(&server, 200, canned_text_reply());
        let client = OpenAiClient::new(server.base_url(), "test-key");

        let response = client.complete(request()).await.unwrap();

        assert_eq!(response.message, Message::text(Role::Assistant, "Hi!"));
        assert_eq!(response.usage.total_tokens, 7);
        mock.assert();
        // Exactly one POST to the documented endpoint.
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn sends_bearer_auth_and_wire_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/chat/completions")
                .header("Authorization", "Bearer sk-or-test-key")
                .body_includes("\"model\":\"test-model\"");
            then.status(200).json_body(canned_text_reply());
        });
        let client = OpenAiClient::new(server.base_url(), "sk-or-test-key");

        client.complete(request()).await.unwrap();
        mock.assert();
    }

    #[tokio::test]
    async fn api_error_status_becomes_typed_error_with_message() {
        let server = MockServer::start();
        let _mock = mock_chat(
            &server,
            401,
            json!({"error": {"message": "invalid api key", "code": 401}}),
        );
        let client = OpenAiClient::new(server.base_url(), "bad-key");

        let err = client.complete(request()).await.unwrap_err();
        match err {
            ProviderError::Api { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, "invalid api key");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_json_error_body_falls_back_to_truncated_body() {
        let server = MockServer::start();
        // Plain-text (non-JSON) error body, e.g. an HTML proxy error page.
        let _mock = server.mock(|when, then| {
            when.method(POST).path("/chat/completions");
            then.status(500).body("upstream exploded");
        });
        let client = OpenAiClient::new(server.base_url(), "k");

        let err = client.complete(request()).await.unwrap_err();
        match err {
            ProviderError::Api { status, message } => {
                assert_eq!(status, 500);
                assert_eq!(message, "upstream exploded");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn malformed_success_body_is_decode_error() {
        let server = MockServer::start();
        let _mock = mock_chat(&server, 200, json!({"unexpected": true}));
        let client = OpenAiClient::new(server.base_url(), "k");

        let err = client.complete(request()).await.unwrap_err();
        assert!(
            matches!(err, ProviderError::Decode(_)),
            "expected Decode, got {err:?}"
        );
    }

    #[tokio::test]
    async fn default_base_url_targets_openrouter() {
        // Stage 0.2 wires the config default; here we pin the endpoint shape.
        let client = OpenAiClient::new("https://openrouter.ai/api/v1", "k");
        assert_eq!(
            client.endpoint(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn trailing_slash_on_base_url_is_normalized() {
        let client = OpenAiClient::new("https://example.com/v1/", "k");
        assert_eq!(client.endpoint(), "https://example.com/v1/chat/completions");
    }

    #[tokio::test]
    async fn wire_request_body_is_valid_json_shape() {
        // Guard the exact JSON we put on the wire (in addition to wire tests).
        let body = wire::request_to_wire(request());
        let value = serde_json::to_value(&body).unwrap();
        assert_eq!(value["model"], "test-model");
        assert_eq!(value["messages"][0]["role"], "user");
        assert_eq!(value["messages"][0]["content"], "hello");
    }
}
