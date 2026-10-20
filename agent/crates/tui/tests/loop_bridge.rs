//! Stage 4.1 — the channel boundary: core loop events cross into the
//! app in order, and the app reflects them.
//!
//! This is the crate's central promise ("channels from core loop →
//! UI") proven without a terminal: a scripted provider runs the real
//! `run_recorded`, the observer converts each borrowed `LoopEvent`
//! into an owned [`AppEvent`] and sends it, and the UI side simply
//! drains — exactly what [`iris_tui::term::spin`] does each tick.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures::stream;
use iris_core::agent_loop::{LoopOptions, run_recorded};
use iris_core::provider::{
    CompletionRequest, CompletionResponse, EventStream, Provider, ProviderError, StreamEvent,
};
use iris_core::tools::{DenyOnAsk, ToolContext, ToolRegistry};
use iris_core::types::{Message, Role, StopReason, Usage};
use iris_tui::App;
use iris_tui::events::AppEvent;

/// A provider that replays pre-scripted responses as streams.
struct Scripted {
    queue: Mutex<VecDeque<CompletionResponse>>,
}

impl Scripted {
    fn new(responses: Vec<CompletionResponse>) -> Self {
        Self {
            queue: Mutex::new(responses.into()),
        }
    }
}

impl Provider for Scripted {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        unimplemented!("the loop always streams")
    }

    async fn complete_stream(
        &self,
        _request: CompletionRequest,
    ) -> Result<EventStream, ProviderError> {
        let response = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .ok_or(ProviderError::NoChoices)?;
        let events: Vec<Result<StreamEvent, ProviderError>> = vec![
            Ok(StreamEvent::DeltaText("Hello ".into())),
            Ok(StreamEvent::DeltaText("from the loop.".into())),
            Ok(StreamEvent::Usage(response.usage)),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

fn options() -> LoopOptions {
    LoopOptions {
        model: "test-model".into(),
        max_turns: 3,
        max_tokens_budget: u64::MAX,
        max_tokens: None,
        temperature: None,
        price: None,
        compact: None,
    }
}

#[test]
fn loop_events_cross_the_channel_into_the_app_in_order() {
    let scripted = Scripted::new(vec![CompletionResponse {
        message: Message::text(Role::Assistant, "ignored — deltas are scripted"),
        stop_reason: StopReason::EndTurn,
        usage: Usage {
            input_tokens: 9,
            output_tokens: 3,
            total_tokens: 12,
            ..Usage::default()
        },
    }]);
    let registry = ToolRegistry::new();
    let ctx = ToolContext::new(".");
    let (tx, rx) = std::sync::mpsc::channel::<AppEvent>();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        run_recorded(
            &scripted,
            &registry,
            &ctx,
            &DenyOnAsk,
            &options(),
            vec![Message::text(Role::User, "hi")],
            |event| {
                let _ = tx.send(AppEvent::from(event));
            },
            |recorded| {
                let _ = recorded;
                // This test never fails a record; the real shell maps
                // writer failures into RunError::Transcript.
                Ok(())
            },
        )
        .await
        .expect("the run must complete")
    });

    // What the shell synthesizes after the loop returns.
    tx.send(AppEvent::RunEnded {
        summary: "completed in 1 turns (12 tokens)".into(),
        usage: Default::default(),
    })
    .expect("channel open");

    // Order, as the render path will see it: text fragments first,
    // then the turn's metrics, then the run's end.
    let events: Vec<AppEvent> = rx.try_iter().collect();
    assert_eq!(
        events[..2],
        [
            AppEvent::Delta("Hello ".into()),
            AppEvent::Delta("from the loop.".into())
        ],
        "deltas arrive in stream order"
    );
    assert!(matches!(events[2], AppEvent::Metrics(_)), "metrics follow");
    assert!(
        matches!(events.last(), Some(AppEvent::RunEnded { .. })),
        "the shell closes the run"
    );

    // Applying the same events to an app reproduces the session.
    let mut app = App::new();
    for event in events {
        app.handle(event);
    }
    assert_eq!(app.transcript(), "Hello from the loop.");
    assert_eq!(app.state(), iris_tui::AppState::Idle);
    assert_eq!(app.status(), "completed in 1 turns (12 tokens)");
    assert!(app.last_metrics().is_some());
}
