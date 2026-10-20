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
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::{Value, json};

use crate::assembly::{Assembler, CacheBust};
use crate::config::Config;
use crate::metrics::TurnMetrics;
use crate::provider::{CompletionRequest, Provider, ProviderError, StreamAssembler, StreamEvent};
use crate::session::schema::Outcome;
use crate::session::writer::WriterError;
use crate::tokens;
use crate::tools::permissions::{AskResolver, Decision, Gate, Resolution};
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
    /// The config file's price-table entry for [`Self::model`] (stage
    /// 3.3), resolved once — `None` for unlisted models (no estimated
    /// cost, never a hardcoded price).
    pub price: Option<crate::config::ModelPrice>,
    /// Stage 6.2 compaction knobs — `None` disables folding entirely
    /// (config `context_window = 0`, or a test that must not compact).
    pub compact: Option<crate::compact::CompactOpts>,
    /// Stage 6.4 user hooks — shell commands fired at the loop's
    /// lifecycle points. Empty by default: nothing fires, nothing
    /// spawns. Failures surface as [`LoopEvent::HookWarning`] only.
    pub hooks: crate::config::Hooks,
}

impl LoopOptions {
    /// Options from the layered config (stage 0.2 fields + stage 1.9
    /// loop knobs + the stage-3.3 price table). The model is
    /// caller-chosen — config does not select models yet.
    pub fn from_config(config: &Config, model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            price: config.pricing.get(&model).cloned(),
            model,
            max_turns: config.max_turns,
            max_tokens_budget: config.max_tokens_budget,
            max_tokens: None,
            temperature: None,
            compact: (config.context_window > 0).then_some(crate::compact::CompactOpts {
                window: config.context_window,
                keep: config.compact_keep,
            }),
            hooks: config.hooks.clone(),
        }
    }
}

/// Progress the loop streams to its observer as it happens.
///
/// Not `Eq`: [`LoopEvent::Metrics`] carries `f64` readings (stage 3.3).
#[derive(Debug, Clone, PartialEq)]
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
    /// The cacheable prefix (system text + tool schemas) changed since
    /// the previous turn — any provider-side prompt cache for this
    /// conversation is invalidated (stage 3.1).
    CacheBust {
        system_changed: bool,
        tools_changed: bool,
    },
    /// Compaction fired before this turn's request (stage 6.2):
    /// `folded` older messages were pinned into a summary, `kept`
    /// raw ones remain. The cacheable prefix is unaffected — this is
    /// a message-history fold, not a system/tools change.
    Compact { folded: usize, kept: usize },
    /// This turn's metrics, right after they were recorded (stage 3.3)
    /// — what the stage-3.4 gauge and the TUI status line render.
    Metrics(TurnMetrics),
    /// A configured hook command failed, timed out, or could not start
    /// (stage 6.4). Advisory only: the run continues unaffected, and
    /// this is how the headless runner's stderr and the TUI's status
    /// line hear about it (core itself never prints).
    HookWarning(String),
}

/// How a run ended.
/// Why a run stopped.
///
/// Not `Eq`: its `usage` carries a `f64` cost (stage 3.2).
#[derive(Debug, Clone, PartialEq)]
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

/// One conversation event the loop mirrors for durability (stage 2.3).
///
/// `run_recorded` hands these to its recorder in transcript order —
/// assistant message, that turn's usage, that turn's metrics, then each
/// tool result as it is dispatched (see [`Recorded`]) — so a recorder
/// writing them as `message`/`usage`/`turn_metrics` events reproduces
/// the conversation a resume will rehydrate.
#[derive(Debug, Clone, PartialEq)]
pub enum Recorded {
    /// A message the loop just appended to the conversation.
    Message(Message),
    /// Token accounting for the response of this (1-based) turn, plus
    /// the estimate reconciliation (stage 3.2): actual input tokens
    /// minus our tiktoken estimate of the request prefix, `None` when
    /// the provider reported no usage to reconcile against.
    Usage {
        turn: u32,
        usage: Usage,
        input_token_delta: Option<i64>,
    },
    /// This turn's measurements (stage 3.3) — ttft, generation speed,
    /// cache hit and cost — recorded right after the turn's usage.
    Metrics(TurnMetrics),
    /// A compaction applied before the next request (stage 6.2): the
    /// pinned summary plus the fold it describes. Recorded as a
    /// `compact` transcript event — resume replays the same fold, so
    /// the compacted context survives a restart byte-identically.
    Compact {
        summary: String,
        folded: usize,
        keep: usize,
    },
}

/// Why a recording run stopped.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The provider failed (transport, HTTP status, wire shape).
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The transcript recorder could not persist a conversation event.
    /// The loop **aborts** rather than continue into silent amnesia —
    /// a transcript missing middle lines is worse than a failed run.
    #[error("cannot record session transcript: {0}")]
    Transcript(#[from] WriterError),
    /// Compaction failed (stage 6.2): the summarizer's provider call
    /// errored, or it returned an empty summary. Aborting beats
    /// sending a request the context window cannot hold.
    #[error("cannot compact context: {0}")]
    Compact(#[from] crate::compact::CompactError),
}

impl RunError {
    /// Back to the provider-only error type. [`run`]'s recorder is a
    /// no-op, so only a provider failure can surface.
    fn into_provider(self) -> ProviderError {
        match self {
            RunError::Provider(err) => err,
            RunError::Transcript(err) => {
                panic!("agent_loop::run records nothing, got transcript error: {err}")
            }
            // A compaction provider failure maps straight back; an
            // empty summary is the closest thing to "the provider gave
            // us nothing usable".
            RunError::Compact(crate::compact::CompactError::Provider(err)) => err,
            RunError::Compact(crate::compact::CompactError::Empty) => ProviderError::NoChoices,
        }
    }
}

impl From<LoopOutcome> for Outcome {
    fn from(outcome: LoopOutcome) -> Self {
        match outcome {
            LoopOutcome::Completed { .. } => Outcome::Completed,
            LoopOutcome::MaxTurns { .. } => Outcome::MaxTurns,
            LoopOutcome::BudgetExceeded { .. } => Outcome::BudgetExceeded,
        }
    }
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
    observer: F,
) -> Result<LoopOutcome, ProviderError> {
    // Durability recorder: [`run`] records nothing — callers that want
    // a transcript (stage 2.3) call [`run_recorded`] directly.
    run_recorded(
        provider,
        registry,
        ctx,
        ask,
        opts,
        messages,
        observer,
        |_recorded| Ok(()),
    )
    .await
    .map_err(RunError::into_provider)
}

/// [`run`], plus a durability recorder: the same conversation loop, but
/// every conversation event is also handed to `recorder` as a
/// [`Recorded`] in transcript order — assistant message, that turn's
/// usage, then each tool result as it is dispatched (see [`Recorded`]).
///
/// `recorder` errors abort the run with [`RunError::Transcript`] — the
/// transcript is the resume primitive (KQ2.2), so a run that cannot
/// record its history must stop, not continue into amnesia.
#[allow(clippy::too_many_arguments)] // one natural parameter set, not a growing bag
pub async fn run_recorded<P, F, R>(
    provider: &P,
    registry: &ToolRegistry,
    ctx: &ToolContext,
    ask: &dyn AskResolver,
    opts: &LoopOptions,
    messages: Vec<Message>,
    mut observer: F,
    mut recorder: R,
) -> Result<LoopOutcome, RunError>
where
    P: Provider,
    F: FnMut(LoopEvent<'_>),
    R: FnMut(Recorded) -> Result<(), RunError>,
{
    let mut messages = messages;
    let mut usage = Usage::default();
    let mut turns = 0u32;
    // One assembler per run (stage 3.1): every turn goes out with
    // canonical name-sorted tools, and a change of system text or tool
    // schemas mid-conversation is surfaced as a cache-bust event.
    let mut prompt = Assembler::new();

    loop {
        if turns >= opts.max_turns {
            return Ok(LoopOutcome::MaxTurns { turns, usage });
        }
        if usage.total_tokens > opts.max_tokens_budget {
            return Ok(LoopOutcome::BudgetExceeded { turns, usage });
        }

        // Assemble the turn's canonical view first: the compaction
        // decision (stage 6.2) runs on the estimate of exactly what
        // would go out, before it goes out.
        let (mut request, bust) = prompt.assemble(CompletionRequest {
            model: opts.model.clone(),
            messages: messages.clone(),
            tools: registry.specs(),
            max_tokens: opts.max_tokens,
            temperature: opts.temperature,
        });
        // Stage 3.2: count the canonical prefix before it is sent —
        // both the stage-6.2 compaction decision and (after it, if it
        // re-assembled) this turn's reconcile use this number.
        let mut estimated_input = tokens::estimate_input(&request);

        // Stage 6.2: fold at/over the trigger — a pre-request decision
        // on an estimate, never a reaction to a provider failure.
        // One fold per turn: after it, `plan` sees only the kept tail
        // and refuses (`None`), so a huge kept message can never spin
        // the loop — the request goes out and the provider's own
        // error (if any) is honest.
        if let Some(knobs) = opts.compact
            && crate::compact::due(estimated_input, knobs)
            && let Some(plan) = crate::compact::plan(&messages, knobs.keep)
        {
            let (summary, summary_usage) = crate::compact::summarize(
                provider,
                &opts.model,
                &messages[plan.start..plan.boundary],
            )
            .await?;
            // The summarizer is real usage too — charge it to the
            // run's budget (not recorded as a conversational turn;
            // there is none).
            usage = usage.plus(&summary_usage);
            recorder(Recorded::Compact {
                summary: summary.clone(),
                folded: plan.folded,
                keep: knobs.keep,
            })?;
            observer(LoopEvent::Compact {
                folded: plan.folded,
                kept: knobs.keep,
            });
            // Stage 6.4: `on_compact` reports the fold just performed,
            // for the turn about to start (`turns + 1`). Before
            // `apply` consumes `plan`.
            for warning in crate::hooks::fire(
                &opts.hooks,
                crate::hooks::Hook::Compact,
                &ctx.workspace_root,
                &json!({
                    "turn": turns + 1,
                    "model": opts.model.as_str(),
                    "workspace_root": &ctx.workspace_root,
                    "folded": plan.folded,
                    "keep": knobs.keep,
                    "summary": crate::hooks::preview(&summary),
                }),
            )
            .await
            {
                observer(LoopEvent::HookWarning(warning));
            }
            messages = crate::compact::apply(messages, plan, &summary);
            // The fold never touches system text or tool schemas, so
            // re-assembly cannot bust the prefix.
            let (reassembled, bust_after) = prompt.assemble(CompletionRequest {
                model: opts.model.clone(),
                messages: messages.clone(),
                tools: registry.specs(),
                max_tokens: opts.max_tokens,
                temperature: opts.temperature,
            });
            debug_assert!(
                bust_after.is_none(),
                "compaction must not change the cacheable prefix"
            );
            request = reassembled;
            estimated_input = tokens::estimate_input(&request);
        }
        if let Some(CacheBust {
            system_changed,
            tools_changed,
        }) = bust
        {
            observer(LoopEvent::CacheBust {
                system_changed,
                tools_changed,
            });
        }
        turns += 1;

        // Stage 3.3: time the request → first token → stream end.
        let request_sent = Instant::now();
        let mut stream = provider.complete_stream(request).await?;

        let mut assembler = StreamAssembler::new();
        let mut first_token: Option<Instant> = None;
        while let Some(event) = stream.next().await {
            let event = event?;
            if first_token.is_none()
                && matches!(
                    event,
                    StreamEvent::DeltaText(_) | StreamEvent::DeltaToolCall { .. }
                )
            {
                // Tool-call fragments are tokens too — a tool-only turn
                // still has a first token.
                first_token = Some(Instant::now());
            }
            if let StreamEvent::DeltaText(text) = &event {
                observer(LoopEvent::TextDelta(text));
            }
            assembler.push(event);
        }
        let stream_ended = Instant::now();
        let response = assembler.finish()?;

        usage = usage.plus(&response.usage);
        let metrics = TurnMetrics::compute(
            turns,
            &response.usage,
            opts.price.as_ref(),
            first_token.map(|token| token.duration_since(request_sent)),
            first_token.map_or(Duration::ZERO, |token| stream_ended.duration_since(token)),
        );

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
        recorder(Recorded::Message(response.message.clone()))?;
        messages.push(response.message);
        recorder(Recorded::Usage {
            turn: turns,
            usage: response.usage.clone(),
            // Stage 3.2: actual input tokens − our tiktoken estimate of
            // the request prefix; `None` when the provider reported no
            // usage to reconcile against.
            input_token_delta: (response.usage.input_tokens > 0)
                .then(|| tokens::reconcile(estimated_input, response.usage.input_tokens)),
        })?;
        // Stage 3.3: record the measurements, then surface them — the
        // transcript is the source of truth, the gauge is a view.
        recorder(Recorded::Metrics(metrics.clone()))?;
        observer(LoopEvent::Metrics(metrics));

        // Stage 6.4: `on_turn_end` — every completed turn, the final
        // one included (it fires before the loop returns below).
        for warning in crate::hooks::fire(
            &opts.hooks,
            crate::hooks::Hook::TurnEnd,
            &ctx.workspace_root,
            &json!({
                "turn": turns,
                "model": opts.model.as_str(),
                "workspace_root": &ctx.workspace_root,
                "stop_reason": &response.stop_reason,
                "text": crate::hooks::preview(&text),
                "usage": {
                    "input_tokens": response.usage.input_tokens,
                    "output_tokens": response.usage.output_tokens,
                    "total_tokens": response.usage.total_tokens,
                    "cached_input_tokens": response.usage.cached_input_tokens,
                    "cost_usd": response.usage.cost_usd,
                },
            }),
        )
        .await
        {
            observer(LoopEvent::HookWarning(warning));
        }

        if calls.is_empty() {
            return Ok(LoopOutcome::Completed { text, turns, usage });
        }

        for call in &calls {
            observer(LoopEvent::ToolStart {
                id: &call.id,
                name: &call.name,
            });
            // Stage 6.4: `on_tool_pre` sees the model's request as
            // asked — before the gate resolves it — redacted on the
            // way out (the stage-5.1 chokepoint).
            for warning in crate::hooks::fire(
                &opts.hooks,
                crate::hooks::Hook::ToolPre,
                &ctx.workspace_root,
                &json!({
                    "turn": turns,
                    "model": opts.model.as_str(),
                    "workspace_root": &ctx.workspace_root,
                    "tool": {
                        "name": call.name.as_str(),
                        "id": call.id.as_str(),
                        "input": &call.input,
                    },
                }),
            )
            .await
            {
                observer(LoopEvent::HookWarning(warning));
            }
            let detail = call.input.to_string();
            // The config gate decides allow/deny here; an ask goes to
            // the resolver (TUI prompt at stages 4.5/4.6) and fails
            // closed on anything but an explicit Allow/AllowWith. A
            // resolver's replacement input (4.6 partial accept) is what
            // actually runs; unparsable replacements fail closed and
            // the original never runs. The resolved decision becomes
            // the gate the registry re-checks at dispatch — one
            // enforcement point (stage 1.7), nothing forgettable.
            let mut input = call.input.clone();
            let gate: Gate = match ctx.permissions.check(&call.name, &detail) {
                Decision::Allow => Arc::new(|_: &str, _: &str| Decision::Allow),
                Decision::Deny => Arc::new(|_: &str, _: &str| Decision::Deny),
                Decision::Ask => match ask.confirm(&call.name, &detail) {
                    Resolution::Allow => Arc::new(|_: &str, _: &str| Decision::Allow),
                    // The user accepted only part of a file edit (4.6):
                    // run the replacement input — an unparsable one
                    // fails closed, and the original never runs.
                    Resolution::AllowWith(replacement) => {
                        match serde_json::from_str::<Value>(&replacement) {
                            Ok(value) => {
                                input = value;
                                Arc::new(|_: &str, _: &str| Decision::Allow)
                            }
                            Err(_) => Arc::new(|_: &str, _: &str| Decision::Deny),
                        }
                    }
                    Resolution::Deny => Arc::new(|_: &str, _: &str| Decision::Deny),
                },
            };
            let call_ctx = ToolContext::with_gate(ctx.workspace_root.clone(), gate);
            let result = registry
                .execute(&call.name, &call_ctx, &call.id, input)
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
            // Stage 6.4: `on_tool_post` — the outcome the model will
            // see (`is_error` covers denials, failures, and errors).
            for warning in crate::hooks::fire(
                &opts.hooks,
                crate::hooks::Hook::ToolPost,
                &ctx.workspace_root,
                &json!({
                    "turn": turns,
                    "model": opts.model.as_str(),
                    "workspace_root": &ctx.workspace_root,
                    "tool": {
                        "name": call.name.as_str(),
                        "id": call.id.as_str(),
                        "is_error": result.is_error,
                        "content": crate::hooks::preview(&result.content),
                    },
                }),
            )
            .await
            {
                observer(LoopEvent::HookWarning(warning));
            }
            let tool_message = Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(result)],
            };
            recorder(Recorded::Message(tool_message.clone()))?;
            messages.push(tool_message);
        }
    }
}

/// The text blocks of an assistant message, concatenated — they are
/// fragments of one answer, not separate messages. `pub(crate)` for
/// the stage-6.2 summarizer, which reads the same shape.
pub(crate) fn plain_text(message: &Message) -> String {
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
