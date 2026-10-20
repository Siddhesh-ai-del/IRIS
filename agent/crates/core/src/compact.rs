//! Compaction that fires (stage 6.2) — pre-request context folding.
//!
//! When the *estimated* input of the next request reaches
//! [`COMPACT_AT_PCT`] percent of the model's context window, the loop
//! folds every raw message beyond the last [`CompactOpts::keep`] into
//! a pinned summary message **before the request goes out** — a
//! pre-request decision on an estimate, never a reaction to a provider
//! failure (the documented pain this stage exists to end).
//!
//! Three invariants the fold may never break:
//!
//! - **System messages are untouchable** — they are the cacheable
//!   prefix the stage-3.1 assembler watches; a fold that moved them
//!   would bust the prompt cache for no reason.
//! - **`tool_use`/`tool_result` pairs stay together** — an
//!   unanswered tool result is a hard provider error, so the fold
//!   boundary walks back rather than split a pair ([`plan`]).
//! - **The transcript stays append-only** — raw events remain on
//!   disk; a `compact` event records the decision, and resume
//!   re-applies the *same* [`plan`] over the *same* messages with the
//!   *same* [`summary_message`] builder, so the compacted context is
//!   byte-identical across a restart.
//!
//! Side state (services, databases, environment) is out of scope: a
//! compaction is a view over the conversation, nothing else.

use thiserror::Error;

use crate::provider::{CompletionRequest, Provider, ProviderError};
use crate::types::{ContentBlock, Message, Role, Usage};

/// The trigger: compact when the estimated input reaches this percent
/// of the context window. The plan pins 70 — test at the boundary.
pub const COMPACT_AT_PCT: u64 = 70;

/// Instruction text for the summarizer call (deterministic — no
/// timestamps, no volatile content).
const SUMMARIZER_SYSTEM: &str = "You are iris, a coding agent, condensing a \
conversation so the session can continue inside its context window. Reply \
with ONLY the continuation brief: the task and its current state, decisions \
made, files touched, errors unresolved, and what should happen next. Plain \
text, no preamble, no markdown headers.";

/// Compaction knobs resolved from config (stage 6.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactOpts {
    /// The model's context window in tokens. `0` never triggers
    /// ([`due`] refuses) — the config layer treats it as "off".
    pub window: u64,
    /// Non-system messages kept raw after a fold; the rest are pinned
    /// into the summary.
    pub keep: usize,
}

/// Why a compaction could not produce a summary.
#[derive(Debug, Error)]
pub enum CompactError {
    /// The summarizer call failed (transport, status, wire shape).
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The summarizer returned nothing usable — pinning an empty
    /// summary would silently destroy context, so the run stops.
    #[error("the summarizer returned an empty summary — refusing to pin nothing")]
    Empty,
}

/// Is the next request already at or past the trigger?
///
/// Exact boundary: at exactly [`COMPACT_AT_PCT`] percent this is
/// `true` (`estimated * 100 >= 70 * window`), one token below is not.
/// A zero window never fires — compaction is off.
pub fn due(estimated_input: u64, opts: CompactOpts) -> bool {
    opts.window > 0
        && estimated_input.saturating_mul(100) >= COMPACT_AT_PCT.saturating_mul(opts.window)
}

/// Where a fold would cut the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldPlan {
    /// First folded message's index — the summary is pinned here.
    pub start: usize,
    /// First *kept* message's index; every non-system message in
    /// `start..boundary` is dropped from the view.
    pub boundary: usize,
    /// How many messages the fold actually removes (non-system in
    /// `start..boundary` — less than naive arithmetic when the
    /// tool-pair walk pulled the boundary back).
    pub folded: usize,
}

/// The exact fold for `keep` — or `None` when nothing can fold
/// safely: fewer messages than `keep`, or a boundary the tool-pair
/// walk could not place without orphaning a `tool_result`.
pub fn plan(messages: &[Message], keep: usize) -> Option<FoldPlan> {
    let non_system: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role != Role::System)
        .map(|(index, _)| index)
        .collect();
    if non_system.len() <= keep {
        return None;
    }
    let start = non_system[0];
    let mut boundary = non_system[non_system.len() - keep];
    // Never orphan a `tool_result`: walk the boundary back to the
    // assistant that issued the call — pairs arrive contiguous, so
    // stepping back over tool messages lands on its `tool_use`.
    while boundary > start && messages[boundary].role == Role::Tool {
        boundary -= 1;
    }
    let folded = non_system.iter().filter(|&&index| index < boundary).count();
    (folded > 0).then_some(FoldPlan {
        start,
        boundary,
        folded,
    })
}

/// The pinned summary message — **the one builder** both the live
/// loop and resume use, so their contexts are byte-identical.
pub fn summary_message(summary: &str) -> Message {
    Message::text(
        Role::User,
        format!(
            "(conversation summary — iris compacted earlier turns to fit the context window)\n\
             {summary}"
        ),
    )
}

/// Apply a plan: drop the folded non-system messages, pin the summary
/// at [`FoldPlan::start`]. System messages keep their original
/// positions, so the assembler's system text (and cache-bust baseline)
/// is unchanged by construction.
pub fn apply(messages: Vec<Message>, plan: FoldPlan, summary: &str) -> Vec<Message> {
    let pinned = summary_message(summary);
    let mut out = Vec::with_capacity(messages.len() + 1 - plan.folded);
    let mut pinned_at = false;
    for (index, message) in messages.into_iter().enumerate() {
        if index == plan.start && !pinned_at {
            out.push(pinned.clone());
            pinned_at = true;
        }
        let folded = index >= plan.start && index < plan.boundary && message.role != Role::System;
        if !folded {
            out.push(message);
        }
    }
    if !pinned_at {
        // Unreachable for a [`plan`]-built fold (`start` is always a
        // real index) — keep the builder total rather than panic.
        out.push(pinned);
    }
    out
}

/// The fold rendered for the summarizer: deterministic, role-labelled
/// lines — the model sees the conversation, not our wire shapes.
pub fn render(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        };
        for block in &message.content {
            match block {
                ContentBlock::Text { text } => out.push_str(&format!("{role}: {text}\n")),
                ContentBlock::ToolUse(call) => out.push_str(&format!(
                    "assistant requested tool {}({}) with {}\n",
                    call.name, call.id, call.input
                )),
                ContentBlock::ToolResult(result) => out.push_str(&format!(
                    "tool result {}: {}{}\n",
                    result.tool_use_id,
                    result.content,
                    if result.is_error { " (error)" } else { "" }
                )),
                ContentBlock::Image { .. } => out.push_str("image content omitted\n"),
            }
        }
    }
    out
}

/// Summarize the fold with the run's own provider: one non-streaming
/// call, no tools, temperature 0, a small output cap. The tokens it
/// spends are returned so the loop can charge them to the run's
/// budget — compaction is real usage, not free.
pub async fn summarize<P: Provider>(
    provider: &P,
    model: &str,
    folded: &[Message],
) -> Result<(String, Usage), CompactError> {
    let request = CompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message::text(Role::System, SUMMARIZER_SYSTEM),
            Message::text(
                Role::User,
                format!(
                    "Condense this conversation into the continuation brief:\n\n{}",
                    render(folded)
                ),
            ),
        ],
        tools: vec![],
        max_tokens: Some(1024),
        temperature: Some(0.0),
    };
    let response = provider.complete(request).await?;
    let summary = crate::agent_loop::plain_text(&response.message)
        .trim()
        .to_string();
    if summary.is_empty() {
        return Err(CompactError::Empty);
    }
    Ok((summary, response.usage))
}
