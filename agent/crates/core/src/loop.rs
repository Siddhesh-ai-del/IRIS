//! The headless agent loop (stage 1.9) — the core of the product.
//!
//! One explicit `loop` (never recursion): stream a response from the
//! provider, run its tool calls through the permission-gated registry,
//! feed the results back into the transcript, and repeat until the model
//! finishes, [`LoopOptions::max_turns`] is reached, or
//! [`LoopOptions::max_tokens_budget`] would be overshot — both checked
//! **before** each provider call, so a limit never causes one more
//! expensive turn.
//!
//! **TUI-free by construction:** all UI concerns attach through the
//! [`LoopEvent`] observer callback. Stage 1.10 prints these to
//! stdout/stderr from the headless runner; the phase-4 TUI renders the
//! same events over channels.

use std::sync::Arc;

use futures::StreamExt;

use crate::config::Config;
use crate::provider::{CompletionRequest, Provider, ProviderError, StreamAssembler, StreamEvent};
use crate::tools::permissions::{AskResolver, Decision, Gate};
use crate::tools::{ToolContext, ToolRegistry};
use crate::types::{ContentBlock, Message, Role, ToolCall, ToolResult, Usage};

/// Knobs for one [`run`].
#[derive(Clone, Debug, PartialEq)]
pub struct LoopOptions {
    /// Model id sent to the provider (e.g. `openai/gpt-4o-mini`).
    pub model: String,
    /// Hard cap on provider calls for this run.
    pub max_turns: u32,
    /// Cap on cumulative `total_tokens` across the run's responses.
    pub max_tokens_budget: u64,
    /// Per-response output cap (`None` = provider default).
    pub max_tokens: Option<u32>,
    /// Sampling temperature (`None` = provider default).
    pub temperature: Option<f32>,
}

impl LoopOptions {
    /// Options from the layered config (stage 0.2 fields + stage 1.9
    /// loop knobs). The model is caller-chosen — config does not select
    /// models yet.
    pub fn from_config(config: &Config, model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_turns: config.max_turns,
            max_tokens_budget: config.max_tokens_budget,
            max_tokens: None,
            temperature: None,
        }
    }
}

/// Progress the loop streams to its observer as it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEvent<'a> {
    /// A fragment of assistant text, in arrival order.
    TextDelta(&'a str),
    /// About to dispatch a tool call through the permission gate.
    ToolStart { id: &'a str, name: &'a str },
    /// The call finished. Tool failures are **data** the model sees
    /// (`result.is_error`), not loop errors.
    ToolEnd {
        id: &'a str,
        name: &'a str,
        result: &'a ToolResult,
    },
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// The model stopped without asking for tools.
    Completed {
        text: String,
        turns: u32,
        usage: Usage,
    },
    /// `max_turns` was reached while the model still wanted tools.
    MaxTurns { turns: u32, usage: Usage },
    /// Cumulative usage passed `max_tokens_budget` — checked before the
    /// next provider call, so no overshooting call was made.
    BudgetExceeded { turns: u32, usage: Usage },
}

/// Run one conversation to a stopping condition.
///
/// `messages` is the initial transcript (system + user). `ctx` carries
/// the config-backed permission gate ([`Config::permission_gate`]);
/// `ask` resolves [`Decision::Ask`] — anything but `Allow` fails
/// closed, so the headless default ([`crate::tools::DenyOnAsk`])
/// refuses everything that asks. `observer` receives [`LoopEvent`]s as
/// they happen — pass `|_| {}` to ignore them. Tool errors (invalid
/// arguments, denials, hallucinated names) are converted into error
/// tool results the model can react to; only provider failures abort.
pub async fn run<P: Provider, F: FnMut(LoopEvent<'_>)>(
    provider: &P,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    ask: &dyn AskResolver,
    opts: &LoopOptions,
    messages: Vec<Message>,
    mut observer: F,
) -> Result<LoopOutcome, ProviderError> {
    let mut messages = messages;
    let mut usage = Usage::default();
    let mut turns = 0u32;

    loop {
        if turns >= opts.max_turns {
            return Ok(LoopOutcome::MaxTurns { turns, usage });
        }
        if usage.total_tokens > opts.max_tokens_budget {
            return Ok(LoopOutcome::BudgetExceeded { turns, usage });
        }
        turns += 1;

        let mut stream = provider
            .complete_stream(CompletionRequest {
                model: opts.model.clone(),
                messages: messages.clone(),
                tools: registry.specs(),
                max_tokens: opts.max_tokens,
                temperature: opts.temperature,
            })
            .await?;

        let mut assembler = StreamAssembler::new();
        while let Some(event) = stream.next().await {
            let event = event?;
            if let StreamEvent::DeltaText(text) = &event {
                observer(LoopEvent::TextDelta(text));
            }
            assembler.push(event);
        }
        let response = assembler.finish()?;

        usage = Usage {
            input_tokens: usage.input_tokens + response.usage.input_tokens,
            output_tokens: usage.output_tokens + response.usage.output_tokens,
            total_tokens: usage.total_tokens + response.usage.total_tokens,
        };

        let text = plain_text(&response.message);
        let calls: Vec<ToolCall> = response
            .message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse(call) => Some(call.clone()),
                _ => None,
            })
            .collect();
        messages.push(response.message);

        if calls.is_empty() {
            return Ok(LoopOutcome::Completed { text, turns, usage });
        }

        for call in &calls {
            observer(LoopEvent::ToolStart {
                id: &call.id,
                name: &call.name,
            });
            let detail = call.input.to_string();
            // The config gate decides allow/deny here; an ask goes to
            // the resolver (TUI prompt at stage 4.5) and fails closed
            // on anything but an explicit Allow. The resolved decision
            // becomes the gate the registry re-checks at dispatch —
            // one enforcement point (stage 1.7), nothing forgettable.
            let gate: Gate = match ctx.permissions.check(&call.name, &detail) {
                Decision::Allow => Arc::new(|_: &str, _: &str| Decision::Allow),
                Decision::Deny => Arc::new(|_: &str, _: &str| Decision::Deny),
                Decision::Ask => match ask.confirm(&call.name, &detail) {
                    Decision::Allow => Arc::new(|_: &str, _: &str| Decision::Allow),
                    _ => Arc::new(|_: &str, _: &str| Decision::Deny),
                },
            };
            let call_ctx = ToolContext::with_gate(ctx.workspace_root.clone(), gate);
            let result = registry
                .execute(&call.name, &call_ctx, &call.id, call.input.clone())
                .await
                .unwrap_or_else(|err| ToolResult {
                    tool_use_id: call.id.clone(),
                    content: err.to_string(),
                    is_error: true,
                });
            observer(LoopEvent::ToolEnd {
                id: &call.id,
                name: &call.name,
                result: &result,
            });
            messages.push(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(result)],
            });
        }
    }
}

/// The text blocks of an assistant message, concatenated — they are
/// fragments of one answer, not separate messages.
fn plain_text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}
