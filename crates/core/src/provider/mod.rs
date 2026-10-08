//! Provider abstraction (stage 1.2).
//!
//! The agent loop depends only on the [`Provider`] trait; the
//! OpenAI-compatible client (OpenRouter, confirmed decision D4) lives in
//! [`openai`], and the OpenAI wire translation in [`wire`].

pub mod openai;
pub mod wire;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{Message, StopReason, Usage};

pub use openai::OpenAiClient;

/// Default request timeout for non-streaming completions.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Description of a callable tool (produced by the registry in stage 1.5).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema for the tool's input object.
    #[serde(default)]
    pub parameters: serde_json::Value,
}

/// One non-streaming completion call.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
}

/// One assistant turn: text and/or tool calls, with accounting.
#[derive(Clone, Debug, PartialEq)]
pub struct CompletionResponse {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    /// The API key is absent (D6: env/keyring only, never a config file).
    #[error("OPENROUTER_API_KEY is not set")]
    MissingApiKey,

    /// Non-2xx response from the provider.
    #[error("provider returned HTTP {status}: {message}")]
    Api { status: u16, message: String },

    /// Transport-level failure (connect/timeout/TLS).
    #[error("provider transport error: {0}")]
    Http(#[from] reqwest::Error),

    /// Response body did not match the expected wire shape.
    #[error("failed to decode provider response: {0}")]
    Decode(String),

    /// A 2xx response whose `choices` array was empty.
    #[error("provider response contained no choices")]
    NoChoices,

    /// `finish_reason` value with no domain mapping — fail loud rather
    /// than silently guessing a stop reason.
    #[error("unmapped finish_reason: {0}")]
    UnknownFinishReason(String),
}

impl From<serde_json::Error> for ProviderError {
    fn from(err: serde_json::Error) -> Self {
        ProviderError::Decode(err.to_string())
    }
}

/// Non-streaming chat completion. Streaming lands in stage 1.3 with the
/// same request/response domain types.
///
/// Desugared to `impl Future + Send` (instead of `async fn`) so the loop
/// can run inside a spawned tokio task — the concrete futures are `Send`
/// because reqwest/serde work is `Send`.
pub trait Provider {
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse, ProviderError>> + Send;
}
