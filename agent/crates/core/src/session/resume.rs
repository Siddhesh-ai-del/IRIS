//! Rehydrating a conversation from its transcript (stage 2.3).
//!
//! `iris sessions resume` must send the provider *exactly* the prior
//! conversation — resume = rehydration, never amnesia (KQ2.2). Three
//! rules make that safe:
//!
//! 1. A resumable transcript **begins** with `session_start`; anything
//!    else is not a session (every IRIS transcript starts with one, so
//!    this catches "wrong file" cheaply).
//! 2. Conversation = the `message` events, in file order. `usage`,
//!    `session_end` and `session_start` are accounting, not context.
//! 3. A run killed mid-tool-call leaves a transcript ending in an
//!    assistant `tool_use` with **no** `tool_result`. Providers reject
//!    that shape outright (HTTP 400), so resume would fail exactly when
//!    the user needs it most — right after a crash. [`rehydrate`]
//!    repairs the gap at *read time* by synthesizing an error
//!    `tool_result` for each unanswered call: the provider sees a valid
//!    conversation, and the transcript keeps recording what actually
//!    happened (the repair is never written back).

use crate::session::SessionError;
use crate::session::schema::Event;
use crate::types::{ContentBlock, Message, Role, ToolResult};

/// The conversation a transcript holds: its `message` events in order,
/// with every dangling tool call answered (see module docs).
pub fn rehydrate(events: &[Event]) -> Result<Vec<Message>, SessionError> {
    let Some(Event::SessionStart { .. }) = events.first() else {
        return Err(SessionError::NoSessionStart);
    };
    let mut messages: Vec<Message> = Vec::new();
    for event in events {
        match event {
            Event::Message(msg) => messages.push(msg.clone()),
            Event::Compact { summary, keep, .. } => {
                // Re-apply the recorded fold with the same planner the
                // live loop used: same messages, same `keep`, same
                // [`summary_message`] builder — the resumed context is
                // byte-identical to what the loop actually sent.
                // `None` means nothing is safely foldable here (an
                // already-compacted or inconsistent transcript) — leave
                // the conversation untouched rather than guess.
                if let Some(plan) = crate::compact::plan(&messages, *keep) {
                    messages = crate::compact::apply(messages, plan, summary);
                }
            }
            _ => {}
        }
    }

    // Find every assistant message with unanswered tool calls (most
    // often the last one — SIGKILL mid-call), then splice the repairs
    // in from the back so earlier indices stay valid.
    let repairs: Vec<(usize, Vec<(String, String)>)> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::Assistant)
        .filter_map(|(index, _)| {
            let missing = unanswered_calls(&messages, index);
            (!missing.is_empty()).then_some((index, missing))
        })
        .collect();
    for (index, missing) in repairs.into_iter().rev() {
        // Real results (from a partial dispatch) come first; the
        // synthesized ones follow them.
        let mut insert_at = index + 1;
        while insert_at < messages.len() && messages[insert_at].role == Role::Tool {
            insert_at += 1;
        }
        let replies = missing
            .into_iter()
            .map(|(id, name)| synthetic_reply(&id, &name))
            .collect::<Vec<_>>();
        messages.splice(insert_at..insert_at, replies);
    }
    Ok(messages)
}

/// For one assistant message: the tool_use ids it issued that no
/// *following* tool message answers.
fn unanswered_calls(messages: &[Message], assistant_index: usize) -> Vec<(String, String)> {
    let Some(assistant) = messages.get(assistant_index) else {
        return Vec::new();
    };
    let calls: Vec<(String, String)> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse(call) => Some((call.id.clone(), call.name.clone())),
            _ => None,
        })
        .collect();
    if calls.is_empty() {
        return Vec::new();
    }
    let answered: Vec<String> = messages[assistant_index + 1..]
        .iter()
        .take_while(|message| message.role == Role::Tool)
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult(result) => Some(result.tool_use_id.clone()),
            _ => None,
        })
        .collect();
    calls
        .into_iter()
        .filter(|(id, _)| !answered.contains(id))
        .collect()
}

/// The read-time reply to an interrupted tool call: an error tool
/// result, so the provider sees a valid conversation. Never written
/// back to the transcript — the file keeps recording what actually
/// happened.
fn synthetic_reply(tool_use_id: &str, name: &str) -> Message {
    Message {
        role: Role::Tool,
        content: vec![ContentBlock::ToolResult(ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: format!(
                "{name} was interrupted mid-call: the session ended before it \
                 finished, and no result was recorded. Treat the call's effects \
                 as unknown."
            ),
            is_error: true,
        })],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::schema::Outcome;
    use crate::types::{ToolCall, Usage};
    use serde_json::json;
    use time::OffsetDateTime;

    fn ts() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn start() -> Event {
        Event::SessionStart {
            session_id: "ses_ab12".into(),
            started_at: ts(),
            cwd: "/home/u/proj".into(),
            model: "openai/gpt-4o-mini".into(),
            iris_version: "0.1.0".into(),
        }
    }

    fn assistant_tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse(ToolCall {
                id: id.into(),
                name: name.into(),
                input: json!({"command": "sleep 20"}),
            })],
        }
    }

    fn tool_result(id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
            })],
        }
    }

    #[test]
    fn a_transcript_must_begin_with_session_start() {
        let events = vec![
            Event::Message(Message::text(Role::User, "hi")),
            Event::Usage {
                turn: 1,
                usage: Usage::default(),
                input_token_delta: None,
            },
        ];
        let err = rehydrate(&events).unwrap_err();
        assert!(
            matches!(err, SessionError::NoSessionStart),
            "first line is the session's identity; got {err}"
        );
        assert!(rehydrate(&[]).is_err(), "an empty file is not a session");
    }

    #[test]
    fn messages_come_back_in_order_and_accounting_is_dropped() {
        let events = vec![
            start(),
            Event::Message(Message::text(Role::System, "you are iris")),
            Event::Message(Message::text(Role::User, "read Cargo.toml")),
            Event::Message(assistant_tool_use("call_1", "read_file")),
            Event::Usage {
                turn: 1,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Usage::default()
                },
                input_token_delta: None,
            },
            Event::Message(tool_result("call_1", "[package]")),
            Event::Usage {
                turn: 2,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 3,
                    total_tokens: 23,
                    ..Usage::default()
                },
                input_token_delta: None,
            },
            Event::Message(Message::text(Role::Assistant, "Done.")),
            Event::SessionEnd {
                outcome: Outcome::Completed,
                turns: 2,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                    total_tokens: 38,
                    ..Usage::default()
                },
                ended_at: ts(),
            },
        ];
        let messages = rehydrate(&events).unwrap();
        let roles: Vec<Role> = messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(
            roles,
            [
                Role::System,
                Role::User,
                Role::Assistant,
                Role::Tool,
                Role::Assistant
            ],
            "messages in transcript order, accounting events dropped"
        );
        assert_eq!(
            messages[0].content[0],
            ContentBlock::Text {
                text: "you are iris".into()
            }
        );
    }

    #[test]
    fn a_killed_tool_call_is_answered_at_read_time() {
        // Exactly what SIGKILL mid-tool leaves behind: start, user,
        // assistant tool_use, usage — and nothing else.
        let events = vec![
            start(),
            Event::Message(Message::text(Role::User, "start the sleeper")),
            Event::Message(assistant_tool_use("call_1", "run_command")),
            Event::Usage {
                turn: 1,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    ..Usage::default()
                },
                input_token_delta: None,
            },
        ];
        let messages = rehydrate(&events).unwrap();
        assert_eq!(messages.len(), 3, "repair adds exactly one message");
        let repair = messages.last().unwrap();
        assert_eq!(
            repair.role,
            Role::Tool,
            "a dangling tool_use needs a tool reply"
        );
        let ContentBlock::ToolResult(result) = &repair.content[0] else {
            panic!("repair must be a tool_result, got {:?}", repair.content);
        };
        assert_eq!(result.tool_use_id, "call_1");
        assert!(result.is_error, "the call never produced a real result");
        assert!(
            result.content.contains("interrupted mid-call")
                && result.content.contains("run_command"),
            "the model must learn what happened and to which tool: {}",
            result.content
        );
    }

    #[test]
    fn answered_calls_are_never_repaired() {
        let events = vec![
            start(),
            Event::Message(Message::text(Role::User, "go")),
            Event::Message(assistant_tool_use("call_1", "read_file")),
            Event::Message(tool_result("call_1", "content")),
        ];
        let messages = rehydrate(&events).unwrap();
        assert_eq!(messages.len(), 3, "no synthetic message for answered calls");
    }

    #[test]
    fn only_the_missing_results_are_synthesized() {
        let two_calls = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse(ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "a.txt"}),
                }),
                ContentBlock::ToolUse(ToolCall {
                    id: "call_2".into(),
                    name: "read_file".into(),
                    input: json!({"path": "b.txt"}),
                }),
            ],
        };
        let events = vec![
            start(),
            Event::Message(Message::text(Role::User, "read both")),
            Event::Message(two_calls),
            Event::Message(tool_result("call_1", "A")),
            // call_2 never came back — killed between dispatches.
        ];
        let messages = rehydrate(&events).unwrap();
        assert_eq!(messages.len(), 4, "one repair for the one missing id");
        let ContentBlock::ToolResult(result) = &messages[3].content[0] else {
            panic!("expected a tool_result, got {:?}", messages[3].content);
        };
        assert_eq!(result.tool_use_id, "call_2");
    }

    #[test]
    fn repair_is_idempotent_across_resumes() {
        let events = vec![
            start(),
            Event::Message(Message::text(Role::User, "start the sleeper")),
            Event::Message(assistant_tool_use("call_1", "run_command")),
        ];
        let once = rehydrate(&events).unwrap();
        // A second resume of the same transcript produces the same
        // conversation — the repair is derived, never compounded.
        let twice = rehydrate(&events).unwrap();
        assert_eq!(once, twice, "rehydrate must be a pure function of events");
        // And a transcript that was *continued* after a repair has a
        // real tool_result for the id — nothing left to repair there.
        let continued = vec![
            start(),
            Event::Message(Message::text(Role::User, "start the sleeper")),
            Event::Message(assistant_tool_use("call_1", "run_command")),
            Event::Message(once.last().unwrap().clone()),
            Event::Message(Message::text(Role::User, "carry on")),
        ];
        let mut expected = once.clone();
        expected.push(Message::text(Role::User, "carry on"));
        assert_eq!(rehydrate(&continued).unwrap(), expected);
    }
}
