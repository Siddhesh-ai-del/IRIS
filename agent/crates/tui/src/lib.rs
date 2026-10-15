//! The IRIS terminal UI (stage 4.1) — app skeleton.
//!
//! Layered so the interesting parts are testable without a terminal:
//!
//! - [`app`] — a pure state machine ([`app::AppState`] as the plan
//!   names it: `Idle | Streaming | AwaitingPermission`), fed by owned
//!   [`events::AppEvent`]s and key presses. No I/O, fully unit-tested.
//! - [`ui`] — the render pass, a function of `&App` only. Proven
//!   against ratatui's `TestBackend`.
//! - [`events`] — the owned mirror of the core loop's `LoopEvent`
//!   (channel-safe: no borrowed deltas) plus run-lifecycle events.
//! - [`term`] — the thin shell: crossterm setup/teardown, the channel
//!   from a background loop task, and the poll → handle → draw cycle.
//!   **Nothing in the render path blocks**: loop events arrive on a
//!   channel and are drained with `try_recv`, input via a timed poll.
//!
//! The CLI (`iris chat`) owns config/session plumbing and hands this
//! crate already-prepared dependencies — layering matches `iris-core`:
//! this crate knows nothing about config files or API keys.

pub mod app;
pub mod events;
pub mod term;
pub mod ui;

pub use app::{App, AppState, PendingPermission};
pub use events::AppEvent;
