//! Stage 5.1 end-to-end: redaction lands in the bytes on disk. The
//! writer's `append` is the single chokepoint every persisted event
//! passes through — this pins that the file it fsyncs is already scrubbed.

use iris_core::session::schema::Event;
use iris_core::session::writer::{Manifest, TranscriptWriter};
use iris_core::types::{ContentBlock, Message, Role, ToolResult};

#[test]
fn secrets_never_reach_the_transcript_file() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = Manifest::new("t1", "/tmp", "demo/model");
    let mut writer = TranscriptWriter::create(dir.path(), &manifest).unwrap();

    let key = "sk-or-v1-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4";
    let event = Event::Message(Message {
        role: Role::User,
        content: vec![
            ContentBlock::Text {
                text: format!("the key is {key}"),
            },
            ContentBlock::ToolResult(ToolResult {
                tool_use_id: "c1".into(),
                content: "DB_PASSWORD=hunter2 written".into(),
                is_error: false,
            }),
        ],
    });
    writer.append(&event).unwrap();

    let raw = std::fs::read_to_string(writer.path()).unwrap();
    assert!(!raw.contains(key), "raw key on disk: {raw}");
    assert!(!raw.contains("hunter2"), "raw password on disk: {raw}");
    assert!(raw.contains("[REDACTED:openrouter-key]"), "{raw}");
    assert!(raw.contains("[REDACTED:credential]"), "{raw}");
}

#[test]
fn clean_events_pass_through_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = Manifest::new("t2", "/tmp", "demo/model");
    let mut writer = TranscriptWriter::create(dir.path(), &manifest).unwrap();

    let event = Event::Message(Message::text(
        Role::Assistant,
        "plain prose, nothing secret",
    ));
    writer.append(&event).unwrap();

    let raw = std::fs::read_to_string(writer.path()).unwrap();
    assert!(raw.contains("plain prose, nothing secret"), "{raw}");
    assert!(!raw.contains("REDACTED"), "{raw}");
}
