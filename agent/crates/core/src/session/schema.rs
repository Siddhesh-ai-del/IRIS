//! JSONL transcript schema v0 (stage 2.1) — the durable, versioned,
//! append-only session format (D5: documented publicly from day one in
//! `docs/transcript-schema.md`).
//!
//! One JSON object per line, shaped `{"v":0,"type":...,...}`. The `v`
//! field is the schema version and this build only reads
//! [`SCHEMA_VERSION`]. Unknown event types are **rejected, never
//! skipped**: a reader that silently dropped lines it did not
//! understand would amnesiate a resume — the exact failure this
//! product exists to prevent.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::types::{Message, Usage};

/// Schema version written into every line and required by every reader.
pub const SCHEMA_VERSION: u32 = 0;

/// Every `type` value this v0 reader understands. Kept as a list so
/// [`Envelope::from_line`] can reject unknown types with a typed error
/// before serde does; a test asserts it matches [`Event`] exactly.
const KNOWN_TYPES: &[&str] = &["session_start", "message", "usage", "session_end"];

/// Why a run stopped (mirrors the loop's `LoopOutcome`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Completed,
    MaxTurns,
    BudgetExceeded,
}

/// One transcript line: the versioned envelope around an [`Event`].
///
/// Serialized as `{"v":<version>,...<event>}` — `v` first, then the
/// event's `"type"` tag and fields.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Envelope {
    /// Must be [`SCHEMA_VERSION`] when written.
    pub v: u32,
    /// The tagged event this line records.
    #[serde(flatten)]
    pub event: Event,
}

/// The event kinds a transcript line can carry (v0).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// First line of every transcript: what this session was.
    SessionStart {
        session_id: String,
        #[serde(with = "time::serde::rfc3339")]
        started_at: OffsetDateTime,
        cwd: String,
        model: String,
        iris_version: String,
    },
    /// A full conversation message — the resume primitive. Covers user,
    /// assistant (incl. tool_use) and tool (tool_result) messages.
    Message(Message),
    /// Token accounting for one provider response, in arrival order.
    Usage { turn: u32, usage: Usage },
    /// Last line of a cleanly finished transcript. Its **absence**
    /// means the run was interrupted (crash, kill, provider error) —
    /// readers must not treat a missing `session_end` as corruption.
    SessionEnd {
        outcome: Outcome,
        turns: u32,
        usage: Usage,
        #[serde(with = "time::serde::rfc3339")]
        ended_at: OffsetDateTime,
    },
}

/// A transcript line could not be encoded or decoded.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    /// Malformed JSON, or a structurally valid line whose fields fail
    /// to deserialize (missing/invalid event fields, bad timestamp).
    #[error("invalid transcript line: {0}")]
    Json(#[from] serde_json::Error),
    /// No integer `v` on the line.
    #[error("transcript line has no integer `v` schema version")]
    MissingVersion,
    /// `v` is present but not the version this build reads.
    #[error("transcript schema version {0} is not supported by this build")]
    UnsupportedVersion(u64),
    /// No `type` on an otherwise valid v0 line.
    #[error("transcript line has no `type` event type")]
    MissingType,
    /// `type` is well-formed but unknown to this build. Never skipped:
    /// dropping it would silently corrupt a resume.
    #[error("unknown transcript event type `{0}`")]
    UnknownType(String),
}

impl Envelope {
    /// An envelope stamped with the current [`SCHEMA_VERSION`].
    pub fn new(event: Event) -> Self {
        Self {
            v: SCHEMA_VERSION,
            event,
        }
    }

    /// Encode as one JSONL line: no trailing newline (the writer, stage
    /// 2.2, appends `\n`), embedded newlines escaped by JSON.
    pub fn to_line(&self) -> Result<String, SchemaError> {
        Ok(serde_json::to_string(self)?)
    }

    /// Decode one line. Tolerates surrounding whitespace (so a line
    /// straight from a `BufRead`, including its `\n` or `\r\n`, parses);
    /// empty or malformed input is [`SchemaError::Json`].
    pub fn from_line(line: &str) -> Result<Self, SchemaError> {
        let value: serde_json::Value = serde_json::from_str(line.trim_end())?;
        let found = value
            .get("v")
            .and_then(serde_json::Value::as_u64)
            .ok_or(SchemaError::MissingVersion)?;
        if found != SCHEMA_VERSION as u64 {
            return Err(SchemaError::UnsupportedVersion(found));
        }
        let ty = value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or(SchemaError::MissingType)?;
        if !KNOWN_TYPES.contains(&ty) {
            return Err(SchemaError::UnknownType(ty.to_owned()));
        }
        let event: Event = serde_json::from_value(value)?;
        Ok(Self {
            v: SCHEMA_VERSION,
            event,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Role, ToolCall, ToolResult};
    use serde_json::json;

    /// 2023-11-14T22:13:20Z — fixed, so snapshots and RFC3339
    /// assertions are deterministic.
    fn ts() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn start_event() -> Event {
        Event::SessionStart {
            session_id: "ses_test".into(),
            started_at: ts(),
            cwd: "/home/u/proj".into(),
            model: "openai/gpt-4o-mini".into(),
            iris_version: "0.1.0".into(),
        }
    }

    fn user_message(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    #[test]
    fn session_start_line_is_the_versioned_envelope() {
        let line = Envelope::new(start_event()).to_line().unwrap();
        assert!(
            line.starts_with(r#"{"v":0,"type":"session_start","#),
            "envelope must lead with v then type, got: {line}"
        );
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "v": 0,
                "type": "session_start",
                "session_id": "ses_test",
                "started_at": "2023-11-14T22:13:20Z",
                "cwd": "/home/u/proj",
                "model": "openai/gpt-4o-mini",
                "iris_version": "0.1.0"
            })
        );
    }

    #[test]
    fn message_event_carries_the_domain_message_verbatim() {
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Reading it.".into(),
                },
                ContentBlock::ToolUse(ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: json!({"path": "Cargo.toml"}),
                }),
            ],
        };
        let line = Envelope::new(Event::Message(msg.clone()))
            .to_line()
            .unwrap();
        assert!(!line.contains('\n'), "one physical line, got: {line:?}");
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "v": 0,
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Reading it."},
                    {"type": "tool_use", "id": "call_1", "name": "read_file",
                     "input": {"path": "Cargo.toml"}}
                ]
            })
        );
        assert_eq!(
            Envelope::from_line(&line).unwrap(),
            Envelope::new(Event::Message(msg))
        );
    }

    #[test]
    fn tool_result_messages_round_trip() {
        let msg = Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult(ToolResult {
                tool_use_id: "call_1".into(),
                content: "line one\nline two\n".into(),
                is_error: false,
            })],
        };
        let line = Envelope::new(Event::Message(msg.clone()))
            .to_line()
            .unwrap();
        assert!(!line.contains('\n'), "escaped, not literal: {line:?}");
        assert_eq!(
            Envelope::from_line(&line).unwrap(),
            Envelope::new(Event::Message(msg))
        );
    }

    #[test]
    fn usage_event_shape() {
        let usage = Usage {
            input_tokens: 120,
            output_tokens: 34,
            total_tokens: 154,
        };
        let line = Envelope::new(Event::Usage { turn: 1, usage })
            .to_line()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "v": 0,
                "type": "usage",
                "turn": 1,
                "usage": {"input_tokens": 120, "output_tokens": 34, "total_tokens": 154}
            })
        );
    }

    #[test]
    fn session_end_event_shape() {
        let line = Envelope::new(Event::SessionEnd {
            outcome: Outcome::Completed,
            turns: 3,
            usage: Usage {
                input_tokens: 300,
                output_tokens: 60,
                total_tokens: 360,
            },
            ended_at: ts(),
        })
        .to_line()
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            value,
            json!({
                "v": 0,
                "type": "session_end",
                "outcome": "completed",
                "turns": 3,
                "usage": {"input_tokens": 300, "output_tokens": 60, "total_tokens": 360},
                "ended_at": "2023-11-14T22:13:20Z"
            })
        );
    }

    #[test]
    fn round_trip_every_event_type() {
        let events = vec![
            start_event(),
            Event::Message(user_message("list the files")),
            Event::Usage {
                turn: 2,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                },
            },
            Event::SessionEnd {
                outcome: Outcome::MaxTurns,
                turns: 50,
                usage: Usage::default(),
                ended_at: ts(),
            },
        ];
        for event in events {
            let line = Envelope::new(event.clone()).to_line().unwrap();
            assert_eq!(
                Envelope::from_line(&line).unwrap(),
                Envelope::new(event),
                "round trip must preserve the event"
            );
        }
    }

    #[test]
    fn round_trip_tolerates_line_terminators() {
        let line = Envelope::new(start_event()).to_line().unwrap();
        assert_eq!(
            Envelope::from_line(&format!("{line}\n")).unwrap(),
            Envelope::new(start_event()),
            "a line read straight from BufRead keeps its \\n"
        );
        assert_eq!(
            Envelope::from_line(&format!("{line}\r\n")).unwrap(),
            Envelope::new(start_event()),
            "and \\r\\n must not corrupt the parse"
        );
    }

    #[test]
    fn rejects_unsupported_version() {
        let err = Envelope::from_line(r#"{"v":1,"type":"message","role":"user","content":[]}"#)
            .unwrap_err();
        assert!(
            matches!(err, SchemaError::UnsupportedVersion(1)),
            "got: {err:?}"
        );
    }

    #[test]
    fn rejects_missing_version() {
        let err =
            Envelope::from_line(r#"{"type":"message","role":"user","content":[]}"#).unwrap_err();
        assert!(matches!(err, SchemaError::MissingVersion), "got: {err:?}");
    }

    #[test]
    fn rejects_non_integer_version() {
        let err = Envelope::from_line(r#"{"v":"0","type":"message","role":"user","content":[]}"#)
            .unwrap_err();
        assert!(matches!(err, SchemaError::MissingVersion), "got: {err:?}");
    }

    #[test]
    fn rejects_missing_type() {
        let err = Envelope::from_line(r#"{"v":0,"role":"user","content":[]}"#).unwrap_err();
        assert!(matches!(err, SchemaError::MissingType), "got: {err:?}");
    }

    #[test]
    fn rejects_unknown_event_type_never_skips_it() {
        let err = Envelope::from_line(r#"{"v":0,"type":"checkpoint","step":7}"#).unwrap_err();
        assert!(
            matches!(&err, SchemaError::UnknownType(t) if t == "checkpoint"),
            "silently skipping would amnesiate a resume; got: {err:?}"
        );
    }

    #[test]
    fn rejects_malformed_lines() {
        for line in ["", "{not json", r#"{"v":0,"type":"message"}"#] {
            assert!(Envelope::from_line(line).is_err(), "must reject {line:?}");
        }
    }

    #[test]
    fn envelope_new_pins_the_current_version() {
        let line = Envelope::new(start_event()).to_line().unwrap();
        assert!(
            line.starts_with(&format!(r#"{{"v":{},"#, SCHEMA_VERSION)),
            "Envelope::new must stamp v={SCHEMA_VERSION}, got: {line}"
        );
    }

    #[test]
    fn known_types_match_the_event_variants() {
        let events = [
            start_event(),
            Event::Message(user_message("x")),
            Event::Usage {
                turn: 1,
                usage: Usage::default(),
            },
            Event::SessionEnd {
                outcome: Outcome::Completed,
                turns: 1,
                usage: Usage::default(),
                ended_at: ts(),
            },
        ];
        for event in &events {
            let line = Envelope::new(event.clone()).to_line().unwrap();
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            let ty = value.get("type").and_then(|t| t.as_str()).unwrap();
            assert!(
                KNOWN_TYPES.contains(&ty),
                "decode rejects `{ty}` but it is a live Event variant — add it to KNOWN_TYPES"
            );
        }
        assert_eq!(
            KNOWN_TYPES.len(),
            events.len(),
            "KNOWN_TYPES and Event must grow together"
        );
    }

    #[test]
    fn transcript_schema_doc_is_published_from_day_one() {
        let doc_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/transcript-schema.md");
        let doc = std::fs::read_to_string(&doc_path)
            .expect("D5: docs/transcript-schema.md must exist from stage 2.1 on");
        assert!(doc.contains("JSONL"), "format must be named");
        for ty in KNOWN_TYPES {
            assert!(doc.contains(ty), "doc must document event type `{ty}`");
        }
        assert!(
            doc.contains("`v`"),
            "doc must document the envelope version field"
        );
    }

    #[test]
    fn snapshot_realistic_transcript() {
        let transcript = [
            Envelope::new(start_event()),
            Envelope::new(Event::Message(user_message("read Cargo.toml"))),
            Envelope::new(Event::Message(Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "Reading it now.".into(),
                    },
                    ContentBlock::ToolUse(ToolCall {
                        id: "call_1".into(),
                        name: "read_file".into(),
                        input: json!({"path": "Cargo.toml"}),
                    }),
                ],
            })),
            Envelope::new(Event::Message(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "[package]\nname = \"iris\"\n".into(),
                    is_error: false,
                })],
            })),
            Envelope::new(Event::Usage {
                turn: 1,
                usage: Usage {
                    input_tokens: 120,
                    output_tokens: 34,
                    total_tokens: 154,
                },
            }),
            Envelope::new(Event::SessionEnd {
                outcome: Outcome::Completed,
                turns: 1,
                usage: Usage {
                    input_tokens: 120,
                    output_tokens: 34,
                    total_tokens: 154,
                },
                ended_at: ts(),
            }),
        ];
        let lines: Vec<String> = transcript.iter().map(|e| e.to_line().unwrap()).collect();
        insta::assert_snapshot!(lines.join("\n"));
    }
}
