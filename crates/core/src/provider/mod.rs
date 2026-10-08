//! Provider abstraction (stages 1.2–1.3).
//!
//! The agent loop depends only on the [`Provider`] trait; the
//! OpenAI-compatible client (OpenRouter, confirmed decision D4) lives in
//! [`openai`], the OpenAI wire translation in [`wire`], and SSE streaming
//! in [`sse`] + [`stream`].

pub mod openai;
pub mod wire;

mod sse;
mod stream;

use std::pin::Pin;

use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::{Message, StopReason, Usage};

pub use openai::OpenAiClient;
pub use stream::StreamAssembler;

/// Default request timeout for non-streaming completions.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Connection-establishment cap for every request. Streaming deliberately
/// has **no total deadline** — a generation can run for minutes.
pub const CONNECT_TIMEOUT_SECS: u64 = 30;

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

/// Non-streaming chat completion.
///
/// Desugared to `impl Future + Send` (instead of `async fn`) so the loop
/// can run inside a spawned tokio task — the concrete futures are `Send`
/// because reqwest/serde work is `Send`.
pub trait Provider {
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse, ProviderError>> + Send;

    /// SSE streaming variant (stage 1.3). Dropping the returned stream
    /// cancels the HTTP request — the loop uses `tokio::select!` with a
    /// Ctrl-C future for that.
    fn complete_stream(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<EventStream, ProviderError>> + Send;
}

/// One incremental event from a streaming completion.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// A fragment of assistant text, in arrival order.
    DeltaText(String),
    /// A fragment of one tool call, keyed by `index` (stable across the
    /// fragments of a single call). `id`/`name` arrive once, in the first
    /// fragment; `arguments_delta` accumulates into a JSON string.
    DeltaToolCall {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    /// Cumulative token accounting (sent once, just before `Done`).
    Usage(Usage),
    /// Terminal event; `stop_reason` mirrors the non-streaming mapping.
    Done { stop_reason: StopReason },
}

/// Boxed stream of [`StreamEvent`]s yielded by [`Provider::complete_stream`].
pub type EventStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, ProviderError>> + Send>>;

/// Drain a stream into a single [`CompletionResponse`] (headless path).
/// The live TUI instead consumes events as they arrive and folds them
/// with [`StreamAssembler`] itself.
pub async fn collect_stream(mut stream: EventStream) -> Result<CompletionResponse, ProviderError> {
    let mut assembler = StreamAssembler::new();
    while let Some(event) = stream.next().await {
        assembler.push(event?);
    }
    assembler.finish()
}
