//! The interactive ask resolver (stages 4.5/4.6).
//!
//! [`PromptingResolver`] is what the core loop calls when the config
//! gate says `ask`. It is deliberately the *only* blocking point in
//! the TUI: the loop task parks here while the frame cycle keeps
//! running — drawing, scrolling, and collecting the user's answer.
//!
//! Flow of one ask:
//!
//! 1. A **session rule** (from an earlier "always allow this
//!    session") answers it without prompting.
//! 2. Otherwise post [`AppEvent::PermissionRequested`] to the frame
//!    loop — for `write_file` carrying the file's **current content**
//!    (loaded through the shell's confined closure, stage 4.6 diff
//!    view) — and block on the answer channel.
//! 3. Apply the [`PermissionAnswer`]: once, session-scoped, or
//!    session + config write-back via
//!    [`iris_core::config::persist_permission`]; a partial accept
//!    (`AllowWith`, 4.6) carries its **replaced input** straight into
//!    [`Resolution::AllowWith`] and is deliberately never remembered.
//!    Then acknowledge with [`AppEvent::PermissionAnswered`] (its
//!    `note` reaches the status line).
//!
//! Every failure path — no UI, UI gone, answer channel disconnected —
//! returns [`Resolution::Deny`]: the headless
//! [`DenyOnAsk`](iris_core::tools::DenyOnAsk) contract (fail closed)
//! applies to the interactive resolver too.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Mutex, MutexGuard};

use iris_core::config;
use iris_core::tools::permissions::{AskResolver, Decision, Resolution};
use serde_json::Value;

use crate::app::PermissionAnswer;
use crate::events::AppEvent;

/// Loads a file's current content for the 4.6 diff view. The shell
/// provides the workspace-confined implementation (`resolve_within` +
/// a read); tests provide in-memory ones.
pub type OldContentLoader = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Ask resolver backed by the TUI prompt: posts the ask, blocks for
/// the answer, applies session/config scoping.
pub struct PromptingResolver {
    /// Ask events to the frame loop (unbounded std channel — sending
    /// never blocks, so the loop task cannot stall the UI either way).
    asks: Sender<AppEvent>,
    /// Answers from the frame loop, taken under `&self` (the loop
    /// calls `confirm` from its task; `Mutex` makes the receiver
    /// shareable without a background forwarder).
    answers: Mutex<Receiver<PermissionAnswer>>,
    /// `always-allow (session)` rules — in-memory, die with the
    /// process.
    session_rules: Mutex<BTreeMap<String, Decision>>,
    /// Where `always-allow (rule)` writes back; `None` when no config
    /// path could be resolved (nothing to persist to).
    config_path: Option<PathBuf>,
    /// Current-content lookup for `write_file` asks (stage 4.6) —
    /// workspace-confined by whoever wired it.
    load_old: OldContentLoader,
    /// Ask-id counter for the prompt's correlation id (`c1`, `c2`, …).
    next_id: AtomicU64,
}

impl PromptingResolver {
    pub fn new(
        asks: Sender<AppEvent>,
        answers: Receiver<PermissionAnswer>,
        config_path: Option<PathBuf>,
        load_old: OldContentLoader,
    ) -> Self {
        Self {
            asks,
            answers: Mutex::new(answers),
            session_rules: Mutex::new(BTreeMap::new()),
            config_path,
            load_old,
            next_id: AtomicU64::new(1),
        }
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The file's current content for a `write_file` ask (stage 4.6) —
    /// `None` for other tools, unparsable details, or unreadable
    /// (missing, non-UTF-8, escaping) paths: the popup then falls back
    /// to the plain prompt.
    fn old_content(&self, tool: &str, detail: &str) -> Option<String> {
        if tool != "write_file" {
            return None;
        }
        let path = serde_json::from_str::<Value>(detail)
            .ok()?
            .get("path")?
            .as_str()?
            .to_string();
        (self.load_old)(&path)
    }
}

impl AskResolver for PromptingResolver {
    fn confirm(&self, tool: &str, detail: &str) -> Resolution {
        // 1. A session rule answers without prompting anyone.
        if let Some(decision) = Self::lock(&self.session_rules).get(tool).copied() {
            return match decision {
                Decision::Allow => Resolution::Allow,
                Decision::Deny | Decision::Ask => Resolution::Deny,
            };
        }

        // 2. Post the ask to the frame loop — no live UI (send on a
        // dropped receiver) fails closed, like DenyOnAsk. A
        // `write_file` ask carries the current content so the popup
        // can render a diff (stage 4.6).
        let id = format!("c{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let old = self.old_content(tool, detail);
        if self
            .asks
            .send(AppEvent::PermissionRequested {
                id,
                name: tool.to_string(),
                detail: detail.to_string(),
                old,
            })
            .is_err()
        {
            return Resolution::Deny;
        }

        // 3. Park until the user answers; a disconnected answer
        // channel (the shell is gone) fails closed too.
        let answer = match Self::lock(&self.answers).recv() {
            Ok(answer) => answer,
            Err(_) => return Resolution::Deny,
        };

        // 4. Apply the scope the user chose.
        let (resolution, note) = match answer {
            PermissionAnswer::Allow => (Resolution::Allow, None),
            PermissionAnswer::Deny => (Resolution::Deny, None),
            PermissionAnswer::AllowWith { input, note } => {
                // Stage 4.6 partial accept: the loop runs `input`
                // instead of the model's original JSON. A partial
                // choice is never remembered — auto-allowing the full
                // edit later would betray it.
                (Resolution::AllowWith(input), Some(note))
            }
            PermissionAnswer::AllowSession => {
                Self::lock(&self.session_rules).insert(tool.to_string(), Decision::Allow);
                (
                    Resolution::Allow,
                    Some(format!("session: {tool} always allowed")),
                )
            }
            PermissionAnswer::AllowRule => {
                // The session remembers it either way — a rule that
                // cannot be written must still not re-prompt *today*.
                Self::lock(&self.session_rules).insert(tool.to_string(), Decision::Allow);
                let note = match &self.config_path {
                    Some(path) => match config::persist_permission(path, tool, Decision::Allow) {
                        Ok(()) => format!("rule saved: {tool} = allow"),
                        Err(err) => {
                            format!("rule not saved ({err}) — allowed this session only")
                        }
                    },
                    None => {
                        "rule not saved (no config path) — allowed this session only".to_string()
                    }
                };
                (Resolution::Allow, Some(note))
            }
        };

        // 5. Acknowledge — the prompt closes and the note (if any)
        // reaches the status line.
        let _ = self.asks.send(AppEvent::PermissionAnswered { note });
        resolution
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const WAIT: Duration = Duration::from_secs(2);

    /// A resolver wired to a test-held request receiver and answer
    /// sender — the same two ends `term::run` connects to the frame
    /// loop.
    fn wiring(
        config_path: Option<PathBuf>,
    ) -> (
        PromptingResolver,
        Receiver<AppEvent>,
        Sender<PermissionAnswer>,
    ) {
        wiring_with(config_path, Box::new(|_| None))
    }

    /// Same wiring with a custom current-content loader (4.6).
    fn wiring_with(
        config_path: Option<PathBuf>,
        load_old: OldContentLoader,
    ) -> (
        PromptingResolver,
        Receiver<AppEvent>,
        Sender<PermissionAnswer>,
    ) {
        let (ask_tx, ask_rx) = mpsc::channel();
        let (ans_tx, ans_rx) = mpsc::channel();
        (
            PromptingResolver::new(ask_tx, ans_rx, config_path, load_old),
            ask_rx,
            ans_tx,
        )
    }

    #[test]
    fn without_a_live_ui_an_ask_fails_closed() {
        let (resolver, asks, answers) = wiring(None);
        drop(answers); // nobody can ever answer
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Deny);
        assert!(
            matches!(asks.try_recv(), Ok(AppEvent::PermissionRequested { .. })),
            "the ask was still posted to the UI first"
        );
    }

    #[test]
    fn allow_and_deny_map_straight_through() {
        let (resolver, asks, answers) = wiring(None);
        let responder = std::thread::spawn(move || {
            asks.recv_timeout(WAIT).expect("ask posted");
            answers.send(PermissionAnswer::Allow).expect("waiting");
            asks.recv_timeout(WAIT).expect("acknowledged")
        });
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
        assert!(
            matches!(
                responder.join().expect("responder"),
                AppEvent::PermissionAnswered { note: None }
            ),
            "plain allow acknowledges silently"
        );

        let (resolver, asks, answers) = wiring(None);
        let responder = std::thread::spawn(move || {
            asks.recv_timeout(WAIT).expect("ask posted");
            answers.send(PermissionAnswer::Deny).expect("waiting");
            asks.recv_timeout(WAIT).expect("acknowledged")
        });
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Deny);
        assert!(
            matches!(
                responder.join().expect("responder"),
                AppEvent::PermissionAnswered { note: None }
            ),
            "plain deny acknowledges silently"
        );
    }

    #[test]
    fn allow_session_scopes_to_the_session_and_stops_prompting() {
        let (resolver, asks, answers) = wiring(None);
        let responder = std::thread::spawn(move || {
            asks.recv_timeout(WAIT).expect("ask posted");
            answers
                .send(PermissionAnswer::AllowSession)
                .expect("waiting");
            asks.recv_timeout(WAIT).expect("acknowledged")
        });
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
        responder.join().expect("responder");

        // Second ask: the session rule answers it. The answer channel
        // is now disconnected, so a re-prompt would fail closed (Deny)
        // — an Allow proves no prompt happened.
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
        // Other tools still ask (the rule is per-tool) — with the UI
        // gone, that ask fails closed.
        assert_eq!(resolver.confirm("write_file", "{}"), Resolution::Deny);
    }

    #[test]
    fn allow_rule_writes_the_config_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        let (resolver, asks, answers) = wiring(Some(path.clone()));
        let responder = std::thread::spawn(move || {
            asks.recv_timeout(WAIT).expect("ask posted");
            answers.send(PermissionAnswer::AllowRule).expect("waiting");
            let ack = asks.recv_timeout(WAIT).expect("acknowledged");
            match ack {
                AppEvent::PermissionAnswered { note: Some(note) } => {
                    assert!(note.contains("saved"), "success note: {note}")
                }
                other => panic!("expected a saved note, got {other:?}"),
            }
        });
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
        responder.join().expect("responder");

        let text = std::fs::read_to_string(&path).expect("config written");
        let table: toml::Table = toml::from_str(&text).expect("parses");
        assert_eq!(
            table["permissions"]["run_command"].as_str(),
            Some("allow"),
            "rule persisted to config:\n{text}"
        );

        // The session stops prompting too (rule + session scope).
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
    }

    #[test]
    fn a_failed_rule_write_degrades_to_the_session_with_a_note() {
        // No config path → nothing to persist to.
        let (resolver, asks, answers) = wiring(None);
        let responder = std::thread::spawn(move || {
            asks.recv_timeout(WAIT).expect("ask posted");
            answers.send(PermissionAnswer::AllowRule).expect("waiting");
            let ack = asks.recv_timeout(WAIT).expect("acknowledged");
            match ack {
                AppEvent::PermissionAnswered { note: Some(note) } => {
                    assert!(note.contains("session"), "degraded note: {note}")
                }
                other => panic!("expected a degraded note, got {other:?}"),
            }
        });
        // The user said allow — the call runs even though the rule
        // could not be persisted.
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
        responder.join().expect("responder");

        // …and the session still remembers it.
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Allow);
    }

    #[test]
    fn write_file_asks_carry_the_current_file_content_for_the_diff() {
        // Stage 4.6: the popup renders a diff, so the ask carries the
        // file's current content — loaded through the shell's
        // workspace-confined closure.
        let (resolver, asks, answers) = wiring_with(
            None,
            Box::new(|path: &str| (path == "src/main.rs").then(|| "old content\n".to_string())),
        );
        let responder = std::thread::spawn(move || {
            let ask = asks.recv_timeout(WAIT).expect("ask posted");
            match ask {
                AppEvent::PermissionRequested { old, detail, .. } => {
                    assert_eq!(old.as_deref(), Some("old content\n"), "current content");
                    assert!(detail.contains("main.rs"), "detail: {detail}");
                }
                other => panic!("expected an ask, got {other:?}"),
            }
            answers.send(PermissionAnswer::Deny).expect("waiting");
        });
        let detail = serde_json::json!({"path": "src/main.rs", "content": "new\n"}).to_string();
        assert_eq!(resolver.confirm("write_file", &detail), Resolution::Deny);
        responder.join().expect("responder");
    }

    #[test]
    fn allow_with_carries_the_replacement_and_remembers_nothing() {
        let (resolver, asks, answers) = wiring(None);
        let responder = std::thread::spawn(move || {
            asks.recv_timeout(WAIT).expect("ask posted");
            answers
                .send(PermissionAnswer::AllowWith {
                    input: "{\"content\":\"partial\"}".into(),
                    note: "applied 1/2 hunks".into(),
                })
                .expect("waiting");
            asks.recv_timeout(WAIT).expect("acknowledged")
        });
        assert_eq!(
            resolver.confirm("run_command", "{}"),
            Resolution::AllowWith("{\"content\":\"partial\"}".to_string()),
            "the replacement reaches the loop"
        );
        assert!(
            matches!(
                responder.join().expect("responder"),
                AppEvent::PermissionAnswered { note: Some(note) } if note.contains("1/2")
            ),
            "the app's note reaches the status line"
        );
        // Nothing was remembered: a second ask would prompt again —
        // with no UI left it fails closed, never auto-allowing.
        assert_eq!(resolver.confirm("run_command", "{}"), Resolution::Deny);
    }
}
