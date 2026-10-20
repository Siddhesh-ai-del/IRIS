//! Secret redaction at write time (stage 5.1, **Wedge #2**).
//!
//! Everything persisted to a transcript passes through here *before*
//! serialization and fsync, so transcripts never contain raw credentials
//! (claude-code #44868, closed by default). Two overlapping mechanisms:
//!
//! - **Pattern set** — high-confidence shapes: AWS access keys, GitHub
//!   tokens, Anthropic/OpenAI/OpenRouter `sk-*` keys, Google/Slack
//!   tokens, `PASSWORD=`/`token:`-style assignments (`.env` lines), and
//!   PEM private-key blocks.
//! - **Entropy heuristic** — long mixed-case alphanumeric runs that look
//!   like opaque tokens. False-positive guards: requires upper+lower+
//!   digits, rejects pure-hex (git SHAs, UUIDs) and snake_case
//!   identifiers, Shannon ≥ 4.5 bits/char.
//!
//! Redaction touches **persistence only**: the loop executes tools and
//! sends provider requests with the *original* content — only bytes on
//! disk (and replayed on resume) are scrubbed. [`ContentBlock::Image`]
//! passes through: base64 pixels are entropy by construction. Every
//! replacement keeps the credential *kind* visible —
//! `[REDACTED:github-token]` — so transcripts stay debuggable.

use std::borrow::Cow;

use serde_json::{Map, Value};

use crate::session::schema::Event;
use crate::types::{ContentBlock, Message, ToolCall, ToolResult};

/// The marker written in place of a secret: `[REDACTED:<kind>]`.
fn marker(kind: &str) -> String {
    format!("[REDACTED:{kind}]")
}

/// Redact secrets in free text. Borrowed (zero-copy) when nothing
/// changed — the common case for clean text.
pub fn redact_text(text: &str) -> Cow<'_, str> {
    let mut out = text.to_owned();
    let mut changed = false;
    changed |= private_key_blocks(&mut out);
    changed |= scan_keys(&mut out);
    changed |= scan_assignments(&mut out);
    changed |= scan_entropy(&mut out);
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

/// Collapse `-----BEGIN … PRIVATE KEY-----` blocks to one marker.
fn private_key_blocks(text: &mut String) -> bool {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut in_block = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if in_block {
            if trimmed.starts_with("-----END ") {
                in_block = false;
            }
            continue; // drop the body and the END line
        }
        if trimmed.starts_with("-----BEGIN ") && trimmed.contains("PRIVATE KEY") {
            out.push_str(&marker("private-key"));
            out.push('\n');
            changed = true;
            in_block = true;
            continue;
        }
        out.push_str(line);
    }
    if changed {
        *text = out;
    }
    changed
}

/// Which charset a fixed-shape rule's body uses.
#[derive(Clone, Copy)]
enum Body {
    Upper,
    Alnum,
    AlnumDash,
    AlnumUnderscore,
    AlnumUnderscoreDash,
}

impl Body {
    fn consumes(self, c: char) -> bool {
        match self {
            Body::Upper => c.is_ascii_uppercase() || c.is_ascii_digit(),
            Body::Alnum => c.is_ascii_alphanumeric(),
            Body::AlnumDash => c.is_ascii_alphanumeric() || c == '-',
            Body::AlnumUnderscore => c.is_ascii_alphanumeric() || c == '_',
            Body::AlnumUnderscoreDash => c.is_ascii_alphanumeric() || c == '_' || c == '-',
        }
    }
}

/// Fixed-shape credential patterns: (prefix, body charset, min body
/// length, marker kind). The generic `sk-` rule goes last so the
/// more specific `sk-ant-`/`sk-or-v1-` labels win on their tokens.
const KEY_RULES: &[(&str, Body, usize, &str)] = &[
    ("github_pat_", Body::AlnumUnderscore, 40, "github-token"),
    ("sk-ant-", Body::AlnumUnderscoreDash, 16, "anthropic-key"),
    ("sk-or-v1-", Body::Alnum, 32, "openrouter-key"),
    ("AKIA", Body::Upper, 16, "aws-access-key"),
    ("ASIA", Body::Upper, 16, "aws-access-key"),
    ("ghp_", Body::Alnum, 36, "github-token"),
    ("gho_", Body::Alnum, 36, "github-token"),
    ("ghu_", Body::Alnum, 36, "github-token"),
    ("ghs_", Body::Alnum, 36, "github-token"),
    ("ghr_", Body::Alnum, 36, "github-token"),
    ("AIza", Body::AlnumUnderscoreDash, 35, "google-key"),
    ("xoxb-", Body::AlnumDash, 10, "slack-token"),
    ("xoxp-", Body::AlnumDash, 10, "slack-token"),
    ("xoxa-", Body::AlnumDash, 10, "slack-token"),
    ("xoxs-", Body::AlnumDash, 10, "slack-token"),
    ("sk-", Body::AlnumUnderscoreDash, 24, "api-key"),
];

/// Replace fixed-shape credential matches with kind markers.
fn scan_keys(text: &mut String) -> bool {
    let mut changed = false;
    for (prefix, body, min, kind) in KEY_RULES {
        changed |= replace_rule(text, prefix, *body, *min, kind);
    }
    changed
}

/// Replace every `prefix` + body(≥ min) occurrence with the rule's
/// marker; the prefix must not trail an identifier character.
fn replace_rule(text: &mut String, prefix: &str, body: Body, min: usize, kind: &str) -> bool {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text.as_str();
    while let Some(pos) = rest.find(prefix) {
        let boundary_ok = pos == 0
            || !rest[..pos]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let after = pos + prefix.len();
        let end = after
            + rest[after..]
                .chars()
                .take_while(|c| body.consumes(*c))
                .map(char::len_utf8)
                .sum::<usize>();
        if boundary_ok && end - after >= min {
            out.push_str(&rest[..pos]);
            out.push_str(&marker(kind));
            changed = true;
            rest = &rest[end..];
        } else {
            out.push_str(&rest[..after]);
            rest = &rest[after..];
        }
    }
    out.push_str(rest);
    if changed {
        *text = out;
    }
    changed
}

/// A name that flags its value as a credential: known word segments,
/// with a documented `public` guard (`public_key` isn't secret).
fn is_secret_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    if lowered.contains("public") {
        return false;
    }
    lowered.split('_').any(|segment| {
        matches!(
            segment,
            "password"
                | "passwd"
                | "pwd"
                | "secret"
                | "token"
                | "credential"
                | "credentials"
                | "apikey"
                | "auth"
                | "authorization"
                | "bearer"
                | "key"
                | "accesstoken"
                | "refreshtoken"
                | "clientsecret"
        )
    })
}

/// `NAME=value` / `NAME: "value"` assignments (`.env` lines included).
fn scan_assignments(text: &mut String) -> bool {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut tail = line;
        while let Some(start) = tail.find(|c: char| c.is_ascii_alphabetic() || c == '_') {
            out.push_str(&tail[..start]);
            let rest = &tail[start..];
            let name_len = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .map(char::len_utf8)
                .sum::<usize>();
            let name = &rest[..name_len];
            let after_name = start + name_len;
            // Optional spaces around the separator.
            let gap = skip_spaces(&tail[after_name..]);
            let sep_pos = after_name + gap;
            let Some(sep) = tail[sep_pos..].chars().next() else {
                out.push_str(name);
                tail = &tail[after_name..];
                continue;
            };
            if !matches!(sep, '=' | ':') || !is_secret_name(name) {
                out.push_str(name);
                tail = &tail[after_name..];
                continue;
            }
            let value_start =
                sep_pos + sep.len_utf8() + skip_spaces(&tail[sep_pos + sep.len_utf8()..]);
            let value = &tail[value_start..];
            let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'');
            let (value_len, skip) = match quote {
                Some(q) => match value[q.len_utf8()..].find(q) {
                    Some(close) => {
                        let inner_end = q.len_utf8() + close;
                        (inner_end + q.len_utf8(), inner_end + q.len_utf8())
                    }
                    None => (value.len(), value.len()),
                },
                None => {
                    let len = value
                        .chars()
                        .take_while(|c| {
                            !c.is_whitespace()
                                && !matches!(c, ',' | ';' | ')' | '"' | '\'' | '`' | '[')
                        })
                        .map(char::len_utf8)
                        .sum::<usize>();
                    (len, len)
                }
            };
            if value_len == 0 {
                out.push_str(&tail[start..value_start]);
                tail = &tail[value_start..];
                continue;
            }
            out.push_str(&tail[start..value_start]);
            out.push_str(&marker("credential"));
            changed = true;
            tail = &tail[value_start + skip..];
        }
        out.push_str(tail);
    }
    if changed {
        *text = out;
    }
    changed
}

/// Spaces (not newlines) after an identifier or separator.
fn skip_spaces(text: &str) -> usize {
    text.chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .map(char::len_utf8)
        .sum()
}

/// Long mixed-case opaque tokens (Shannon ≥ 4.5 bits/char). Guards:
/// upper+lower+digits required — plain words and snake_case identifiers
/// (no uppercase) can never fire; pure hex (SHAs, UUIDs) is rejected.
fn scan_entropy(text: &mut String) -> bool {
    let mut changed = false;
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(start) = rest.find(|c: char| c.is_ascii_alphanumeric() || "+/=_-".contains(c)) {
        out.push_str(&rest[..start]);
        let body = &rest[start..];
        let len = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || "+/=_-".contains(*c))
            .map(char::len_utf8)
            .sum::<usize>();
        let run = &body[..len];
        let opaque = run.len() >= 20
            && run.chars().any(|c| c.is_ascii_uppercase())
            && run.chars().any(|c| c.is_ascii_lowercase())
            && run.chars().any(|c| c.is_ascii_digit())
            && !run.chars().all(|c| c.is_ascii_hexdigit())
            && shannon(run) >= 4.5;
        if opaque {
            out.push_str(&marker("high-entropy"));
            changed = true;
        } else {
            out.push_str(run);
        }
        rest = &body[len..];
    }
    out.push_str(rest);
    if changed {
        *text = out;
    }
    changed
}

/// Shannon entropy (bits/char) over a token's bytes.
fn shannon(token: &str) -> f64 {
    let mut counts = [0usize; 256];
    for byte in token.bytes() {
        counts[byte as usize] += 1;
    }
    let len = token.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

/// Recursively redact every string in a JSON value (object keys are
/// schema, values are content). Key context matters: a value under a
/// secret-ish key, or right after a `--password`-style flag in an args
/// array, is a credential by position even without a recognizable shape.
pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(text).into_owned()),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            let mut redact_next = false;
            for item in items {
                if redact_next && matches!(item, Value::String(_)) {
                    redact_next = false;
                    out.push(Value::String(marker("credential")));
                    continue;
                }
                if let Value::String(text) = item {
                    redact_next = is_secret_flag(text);
                }
                out.push(redact_json(item));
            }
            Value::Array(out)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, val) in map {
                let value = match val {
                    Value::String(_) if is_secret_name(key) => Value::String(marker("credential")),
                    other => redact_json(other),
                };
                out.insert(key.clone(), value);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// A CLI-style flag naming a credential: `--password`, `-token`.
fn is_secret_flag(text: &str) -> bool {
    text.starts_with('-') && is_secret_name(text.trim_start_matches('-'))
}

/// Redact a chat message: text blocks, tool-call inputs, tool results.
pub fn redact_message(message: &Message) -> Message {
    Message {
        role: message.role.clone(),
        content: message.content.iter().map(redact_block).collect(),
    }
}

fn redact_block(block: &ContentBlock) -> ContentBlock {
    match block {
        ContentBlock::Text { text } => ContentBlock::Text {
            text: redact_text(text).into_owned(),
        },
        ContentBlock::ToolUse(call) => ContentBlock::ToolUse(ToolCall {
            id: call.id.clone(),
            name: call.name.clone(),
            input: redact_json(&call.input),
        }),
        ContentBlock::ToolResult(result) => ContentBlock::ToolResult(ToolResult {
            tool_use_id: result.tool_use_id.clone(),
            content: redact_text(&result.content).into_owned(),
            is_error: result.is_error,
        }),
        // Documented: base64 pixels are entropy by construction —
        // redacting them would corrupt every screenshot.
        image @ ContentBlock::Image { .. } => image.clone(),
    }
}

/// Redact an event destined for the transcript.
pub fn redact_event(event: Event) -> Event {
    match event {
        Event::Message(message) => Event::Message(redact_message(&message)),
        // Stage 6.2: a summary *describes* the conversation and can
        // echo a credential verbatim — it passes the same text passes
        // the chokepoint runs on messages.
        Event::Compact {
            summary,
            folded,
            keep,
        } => Event::Compact {
            summary: redact_text(&summary).into_owned(),
            folded,
            keep,
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    // --- pattern set ----------------------------------------------------

    #[test]
    fn aws_access_keys_are_redacted() {
        // The canonical AWS docs example key (famous, fake) + a random one.
        for key in ["AKIAIOSFODNN7EXAMPLE", "ASIA2TYJ5V6U3H7QW9R4KZ"] {
            let text = format!("creds: {key} in env");
            let out = redact_text(&text);
            assert!(!out.contains(key), "raw key survived: {out}");
            assert!(out.contains("[REDACTED:aws-access-key]"), "{out}");
        }
    }

    #[test]
    fn github_tokens_are_redacted() {
        let pat = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7r8");
        let fine = format!("github_pat_{}b9C0", "A".repeat(40));
        let text = format!("push with {pat} or {fine}");
        let out = redact_text(&text);
        assert!(!out.contains("a1B2c3D4"), "{out}");
        assert!(!out.contains(&fine), "{out}");
        assert_eq!(out.matches("[REDACTED:github-token]").count(), 2, "{out}");
    }

    #[test]
    fn sk_keys_are_redacted_with_kind_labels() {
        let cases = [
            ("sk-ant-api03-7xQ9wZ2mK4pL6nR8tV1yB5cD", "anthropic-key"),
            (
                "sk-or-v1-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
                "openrouter-key",
            ),
            ("sk-proj-4Kx8Qm2Vn7Pw3Ry9Lt5Bz1Cd6Ef8Hj", "api-key"),
        ];
        for (key, kind) in cases {
            let text = format!("key={key}");
            let out = redact_text(&text);
            assert!(!out.contains(key), "raw survived in: {out}");
            assert!(out.contains(&format!("[REDACTED:{kind}]")), "{out}");
        }
    }

    #[test]
    fn credential_assignments_are_redacted() {
        let text = "DB_PASSWORD=hunter2\napi_key: \"s3cr3t-value\"\nAUTH_TOKEN = qwerty123\n";
        let out = redact_text(text);
        assert!(!out.contains("hunter2"), "{out}");
        assert!(!out.contains("s3cr3t-value"), "{out}");
        assert!(!out.contains("qwerty123"), "{out}");
        assert_eq!(out.matches("[REDACTED:credential]").count(), 3, "{out}");
    }

    #[test]
    fn public_keys_are_not_redacted() {
        // Documented false-positive guard: public key material isn't secret.
        let text = "ssh public_key: ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn private_key_blocks_are_redacted() {
        let text = "here is a key:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA\n0Zx8mQ\n-----END RSA PRIVATE KEY-----\ndone";
        let out = redact_text(text);
        assert!(!out.contains("MIIEpAIBAAKCAQEA"), "{out}");
        assert!(!out.contains("BEGIN"), "{out}");
        assert!(out.contains("[REDACTED:private-key]"), "{out}");
        assert!(out.contains("done"), "{out}");
    }

    // --- entropy heuristic ----------------------------------------------

    #[test]
    fn high_entropy_tokens_are_redacted() {
        let out = redact_text("session=Xk39Qa7pLm2Zt8Vr4Nb6Cw1DyHgF");
        assert!(!out.contains("Xk39Qa7p"), "{out}");
        assert!(out.contains("[REDACTED:high-entropy]"), "{out}");
    }

    #[test]
    fn plain_words_and_snake_case_survive() {
        for text in [
            "hello world ordinary prose",
            "vec3_transform_point_input_buffer",
            "this_is_a_very_long_snake_case_identifier_name",
        ] {
            assert_eq!(redact_text(text), text);
        }
    }

    #[test]
    fn hex_tokens_survive() {
        // sha256 + commit SHAs: all-hex, documented low-signal → kept.
        let sha = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(redact_text(sha), sha);
    }

    #[test]
    fn redaction_is_idempotent() {
        let messy = "PASSWORD=hunter2 key=sk-or-v1-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4 tok=Xk39Qa7pLm2Zt8Vr4Nb6Cw1DyHgF";
        let once = redact_text(messy).into_owned();
        let twice = redact_text(&once).into_owned();
        assert_eq!(once, twice);
    }

    #[test]
    fn clean_text_is_borrowed() {
        assert!(matches!(
            redact_text("plain, clean prose"),
            Cow::Borrowed(_)
        ));
    }

    // --- structured redaction -------------------------------------------

    #[test]
    fn json_strings_are_redacted_recursively() {
        let value = json!({
            "tool": "write_file",
            "nested": {"args": ["--password", "hunter2"], "n": 7, "flag": true}
        });
        let out = redact_json(&value);
        let text = out.to_string();
        assert!(!text.contains("hunter2"), "{text}");
        assert_eq!(out["nested"]["n"], json!(7));
        assert_eq!(out["nested"]["flag"], json!(true));
        assert_eq!(out["tool"], "write_file");
    }

    #[test]
    fn messages_redact_every_text_bearing_block() {
        let message = Message {
            role: crate::types::Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "key=sk-or-v1-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4".into(),
                },
                ContentBlock::ToolUse(ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: json!({"path": ".env", "content": "OPENROUTER_API_KEY=sk-or-v1-a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"}),
                }),
                ContentBlock::ToolResult(ToolResult {
                    tool_use_id: "c1".into(),
                    content: "wrote PASSWORD=hunter2".into(),
                    is_error: false,
                }),
                ContentBlock::Image {
                    media_type: "image/png".into(),
                    data: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk".into(),
                },
            ],
        };
        let out = redact_message(&message);
        let text = format!("{out:?}");
        assert!(!text.contains("a1b2c3d4e5f6"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
        // Image base64 passes through untouched.
        assert!(text.contains("iVBORw0KGgo"), "{text}");
        assert_eq!(out.role, message.role);
        assert_eq!(out.content.len(), message.content.len());
    }

    #[test]
    fn message_events_are_redacted_end_to_end() {
        let event = Event::Message(Message::text(crate::types::Role::User, "password=hunter2"));
        let out = redact_event(event);
        let Event::Message(message) = &out else {
            panic!("expected a message event");
        };
        let ContentBlock::Text { text } = &message.content[0] else {
            panic!("expected text");
        };
        assert!(!text.contains("hunter2"), "{text}");
    }
}
