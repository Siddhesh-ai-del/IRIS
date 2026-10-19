//! Live demo of the stage-4.6 **diff view** — no API key needed.
//!
//! A scripted provider proposes exactly one `write_file` edit: two
//! distant changes in `greeting.txt`. The permission prompt opens the
//! hunk view; rejecting one hunk and pressing `y` runs *only* the
//! accepted change (the loop executes the replaced input).
//!
//! Run it in a real terminal:
//!
//! ```text
//! cargo run --example diff_demo -p iris-tui
//! ```
//!
//! At the prompt: `j`/`k` move between hunks · `space` accept/reject ·
//! `y` runs the accepted parts · `n` denies · `a`/`r` always-allow.
//! Inspect the result afterwards — the demo dir is printed on startup.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;
use iris_core::agent_loop::LoopOptions;
use iris_core::config::{ConfigInputs, load_config};
use iris_core::provider::{
    CompletionRequest, CompletionResponse, EventStream, Provider, ProviderError, StreamEvent,
};
use iris_core::session::writer::{Manifest, TranscriptWriter};
use iris_core::tools::{ToolContext, ToolRegistry, WriteTool};
use iris_core::types::{ContentBlock, Message, Role, StopReason, ToolCall, Usage};
use iris_tui::term::Chat;

/// What the file looks like before the demo run.
const OLD: &str = "\
hello world
line two
line three
line four
line five
line six
line seven
line eight
line nine
line ten
line eleven
goodbye
";

/// What the model *proposes*: two distant changes → two hunks.
const NEW: &str = "\
hello IRIS
line two
line three
line four
line five
line six
line seven
line eight
line nine
line ten
line eleven
goodbye universe
";

/// The scripted provider: one tool call, then a closing remark.
struct DemoProvider {
    calls: AtomicUsize,
}

impl Provider for DemoProvider {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Ok(self.next_response())
    }

    fn complete_stream(
        &self,
        _request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<EventStream, ProviderError>> + Send {
        let events = events_for(&self.next_response());
        async move { Ok(futures::stream::iter(events).boxed()) }
    }
}

impl DemoProvider {
    fn next_response(&self) -> CompletionResponse {
        if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            CompletionResponse {
                message: Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::ToolUse(ToolCall {
                        id: "demo1".to_string(),
                        name: "write_file".to_string(),
                        input: serde_json::json!({"path": "greeting.txt", "content": NEW}),
                    })],
                },
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 8,
                    total_tokens: 20,
                    ..Usage::default()
                },
            }
        } else {
            CompletionResponse {
                message: Message::text(
                    Role::Assistant,
                    "Done — greeting.txt now has the accepted hunks only.",
                ),
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 10,
                    total_tokens: 40,
                    ..Usage::default()
                },
            }
        }
    }
}

/// Re-encode a response as the stream events its SSE body would carry.
fn events_for(response: &CompletionResponse) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut events: Vec<Result<StreamEvent, ProviderError>> = response
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(Ok(StreamEvent::DeltaText(text.clone()))),
            ContentBlock::ToolUse(call) => Some(Ok(StreamEvent::DeltaToolCall {
                index: 0,
                id: Some(call.id.clone()),
                name: Some(call.name.clone()),
                arguments_delta: call.input.to_string(),
            })),
            ContentBlock::Image { .. } | ContentBlock::ToolResult(_) => None,
        })
        .collect();
    events.push(Ok(StreamEvent::Usage(response.usage.clone())));
    events.push(Ok(StreamEvent::Done {
        stop_reason: response.stop_reason.clone(),
    }));
    events
}

fn main() -> Result<(), String> {
    // A throwaway workspace, reseeded on every run so the diff is
    // always the same.
    let workdir = PathBuf::from("/tmp/opencode/iris-diff-demo/workspace");
    std::fs::create_dir_all(&workdir).map_err(|err| err.to_string())?;
    std::fs::write(workdir.join("greeting.txt"), OLD).map_err(|err| err.to_string())?;
    let sessions = PathBuf::from("/tmp/opencode/iris-diff-demo/sessions");

    let mut registry = ToolRegistry::new();
    registry.register(WriteTool);
    // Default config: `write_file` is an `ask` — the prompt fires.
    let config = load_config(&ConfigInputs {
        file_toml: None,
        env: &Default::default(),
        flag_base_url: None,
    })
    .map_err(|err| err.to_string())?;
    let ctx = ToolContext::with_gate(&workdir, config.permission_gate());

    let manifest = Manifest::new(
        format!("demo-{}", std::process::id()),
        workdir.display().to_string(),
        "demo/model",
    );
    let writer = TranscriptWriter::create(&sessions, &manifest).map_err(|err| err.to_string())?;

    eprintln!("[demo] workspace: {}", workdir.display());
    eprintln!("[demo] at the prompt: j/k hunks · space accept/reject · y applies accepted");
    iris_tui::term::run(Chat {
        provider: Arc::new(DemoProvider {
            calls: AtomicUsize::new(0),
        }),
        registry: Arc::new(registry),
        ctx,
        opts: LoopOptions {
            model: "demo/model".into(),
            max_turns: 5,
            max_tokens_budget: u64::MAX,
            max_tokens: None,
            temperature: None,
            price: None,
        },
        writer,
        system: Message::text(Role::System, "You are IRIS in demo mode."),
        config_path: None,
    })?;
    Ok(())
}
