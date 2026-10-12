//! Prefix-invariant prompt assembly (stage 3.1).
//!
//! Provider prompt caches only hit when the *leading bytes* of the
//! request body are identical, so assembly must be canonical:
//!
//! - **system blocks** live in a name-sorted map — insertion order (or
//!   a `HashMap`'s randomized iteration order) can never leak into the
//!   prefix;
//! - **tool schemas** are sorted by name — registration order is an
//!   implementation detail of whoever built the registry;
//! - **serialization** is the wire format's own struct order over
//!   `serde_json`'s `BTreeMap`-backed values — no `HashMap` iteration
//!   anywhere in the bytes;
//! - **no volatile content** (timestamps, absolute paths, random ids)
//!   is added by assembly itself.
//!
//! When the cacheable prefix (system text + tool schemas) *changes*
//! between two assemblies of the same run, the [`Assembler`] reports a
//! [`CacheBust`] — anything the provider cached for this conversation
//! is invalidated and the caller should know why.

use std::collections::BTreeMap;

use crate::provider::{CompletionRequest, ToolSpec};
use crate::types::{ContentBlock, Message, Role};

/// Why the cacheable prefix changed between two assemblies.
///
/// Emitted by [`Assembler::assemble`] whenever the system text or the
/// tool schemas differ from the previous turn's — a cache bust, not an
/// error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheBust {
    /// The assembled system text differs from the previous assembly.
    pub system_changed: bool,
    /// The tool schemas differ (names, descriptions or parameters —
    /// incoming order never matters, they are canonicalized).
    pub tools_changed: bool,
}

/// Named system blocks in a canonical (name-sorted) order.
///
/// Stage 3.1 ships one "core" block; later stages add environment,
/// permission and project blocks without ever disturbing the prefix
/// order — the map decides, not the caller.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemBlocks {
    blocks: BTreeMap<String, String>,
}

impl SystemBlocks {
    /// No blocks.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add (or replace) a named block. Call order never matters.
    pub fn with_block(mut self, name: impl Into<String>, text: impl Into<String>) -> Self {
        self.blocks.insert(name.into(), text.into());
        self
    }

    /// The blocks joined in name order — the byte-stable system text.
    pub fn text(&self) -> String {
        self.blocks
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// The system message for the conversation prefix (`None` when
    /// there are no blocks).
    pub fn message(&self) -> Option<Message> {
        if self.blocks.is_empty() {
            return None;
        }
        Some(Message::text(Role::System, self.text()))
    }
}

/// Builds each turn's provider request with a canonical prefix, and
/// reports [`CacheBust`] when that prefix changes mid-run.
///
/// One assembler per run: the first assembly anchors the baseline
/// silently, every later one compares against the previous turn.
#[derive(Debug, Default)]
pub struct Assembler {
    baseline: Option<(String, String)>,
}

impl Assembler {
    /// An assembler with no baseline yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Canonicalize this turn's request and detect prefix drift: the
    /// returned request carries name-sorted tools; the cache-bust (if
    /// any) says whether the system text, the tool schemas, or both
    /// changed since the previous assembly.
    pub fn assemble(
        &mut self,
        mut request: CompletionRequest,
    ) -> (CompletionRequest, Option<CacheBust>) {
        request.tools = canonical_tools(&request.tools);
        let system = system_text(&request.messages);
        let tools_json =
            serde_json::to_string(&request.tools).expect("tool specs always serialize");

        let bust = match &self.baseline {
            None => None,
            Some((base_system, base_tools)) => {
                let system_changed = base_system != &system;
                let tools_changed = base_tools != &tools_json;
                if system_changed || tools_changed {
                    Some(CacheBust {
                        system_changed,
                        tools_changed,
                    })
                } else {
                    None
                }
            }
        };
        // The newest state re-anchors: a change is reported once, not
        // on every turn after it.
        self.baseline = Some((system, tools_json));
        (request, bust)
    }
}

/// Tool specs sorted by name — the one true order, regardless of
/// registration or map iteration order.
pub fn canonical_tools(tools: &[ToolSpec]) -> Vec<ToolSpec> {
    let mut tools = tools.to_vec();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
}

/// The request as canonical JSON bytes: model, messages, sorted tools
/// and the sampling knobs — no streaming flags, no volatile content.
/// Two runs with the same session and config must produce identical
/// bytes (the golden test enforces it).
pub fn prefix_bytes(request: &CompletionRequest) -> String {
    serde_json::to_string(&crate::provider::wire::request_to_wire(request.clone()))
        .expect("the wire request always serializes")
}

/// The system text of a conversation: every system message, in
/// conversation order, their text blocks joined — the part of the
/// prefix the cache-bust detector watches.
fn system_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;
    use serde_json::{Value, json};
    use std::collections::HashMap;

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: Some(format!("the {name} tool")),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    fn request(system: &str, tools: Vec<ToolSpec>) -> CompletionRequest {
        CompletionRequest {
            model: "openai/gpt-4o-mini".into(),
            messages: vec![
                Message::text(Role::System, system),
                Message::text(Role::User, "do the thing"),
            ],
            tools,
            max_tokens: None,
            temperature: None,
        }
    }

    /// A request whose tool parameters were built by iterating a
    /// `HashMap` — the map's randomized seed decides insertion order,
    /// which the canonical (BTreeMap-backed) serialization must erase.
    fn seeded_request() -> CompletionRequest {
        let mut properties = HashMap::new();
        properties.insert("zeta", json!({"type": "string"}));
        properties.insert("alpha", json!({"type": "integer"}));
        properties.insert("middle", json!({"type": "boolean"}));
        let mut object = serde_json::Map::new();
        for (key, value) in properties {
            object.insert(key.to_string(), value);
        }
        let mut tools = vec![spec("write"), spec("read"), spec("list")];
        tools[1] = ToolSpec {
            name: "read".into(),
            description: Some("Read a file".into()),
            parameters: Value::Object(object),
        };
        tools.reverse(); // yet another order the prefix must ignore
        request("You are iris.", tools)
    }

    #[test]
    fn system_block_order_never_depends_on_insertion_order() {
        let a = SystemBlocks::new()
            .with_block("zeta", "z text")
            .with_block("core", "c text")
            .with_block("alpha", "a text");
        let b = SystemBlocks::new()
            .with_block("alpha", "a text")
            .with_block("zeta", "z text")
            .with_block("core", "c text");
        assert_eq!(a, b, "same blocks, same value, whatever the order");
        assert_eq!(a.text(), "a text\n\nc text\n\nz text", "name-sorted");
        let message = a.message().expect("blocks exist");
        assert_eq!(message.role, Role::System);
        assert_eq!(message.content, vec![ContentBlock::Text { text: a.text() }]);
    }

    #[test]
    fn no_blocks_means_no_system_message() {
        assert!(SystemBlocks::new().message().is_none());
        assert_eq!(SystemBlocks::new().text(), "");
    }

    #[test]
    fn assembled_tools_are_sorted_by_name_not_registration_order() {
        let mut assembler = Assembler::new();
        let (assembled, bust) = assembler.assemble(request(
            "s",
            vec![spec("write"), spec("read"), spec("list")],
        ));
        let names: Vec<&str> = assembled
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, ["list", "read", "write"]);
        assert_eq!(bust, None, "the first assembly is a baseline, not a bust");
        // everything else survives untouched.
        assert_eq!(assembled.model, "openai/gpt-4o-mini");
        assert_eq!(assembled.messages.len(), 2);
    }

    /// The stage's Done-when: same session + same config ⇒
    /// byte-identical request prefix, even when the inputs were built
    /// through `HashMap`s with different random seeds.
    #[test]
    fn prefix_bytes_are_byte_identical_across_randomized_hashmap_seeds() {
        let bytes_one = prefix_bytes(&seeded_request());
        let bytes_two = prefix_bytes(&seeded_request());
        assert_eq!(
            bytes_one, bytes_two,
            "same session + same config ⇒ byte-identical prefix"
        );
        assert!(
            !bytes_one.contains("\"stream\""),
            "the canonical body carries no streaming flags: {bytes_one}"
        );
        insta::assert_snapshot!(bytes_one);
    }

    #[test]
    fn a_changed_system_prompt_is_reported_as_a_cache_bust() {
        let mut assembler = Assembler::new();
        assembler.assemble(request("version one", vec![spec("read")]));
        let (_, bust) = assembler.assemble(request("version two", vec![spec("read")]));
        assert_eq!(
            bust,
            Some(CacheBust {
                system_changed: true,
                tools_changed: false,
            })
        );
    }

    #[test]
    fn changed_tool_schemas_are_reported_as_a_cache_bust_then_reanchored() {
        let mut assembler = Assembler::new();
        assembler.assemble(request("s", vec![spec("read")]));
        let documented = ToolSpec {
            name: "read".into(),
            description: Some("Read a file, now with docs".into()),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        };
        let (_, bust) = assembler.assemble(request("s", vec![documented.clone()]));
        assert_eq!(
            bust,
            Some(CacheBust {
                system_changed: false,
                tools_changed: true,
            }),
            "same name, different schema — still a bust"
        );
        let (_, again) = assembler.assemble(request("s", vec![documented]));
        assert_eq!(again, None, "the newest state re-anchors the baseline");
    }

    #[test]
    fn tool_order_alone_is_never_a_cache_bust() {
        let mut assembler = Assembler::new();
        assembler.assemble(request("s", vec![spec("read"), spec("list")]));
        let (_, bust) = assembler.assemble(request("s", vec![spec("list"), spec("read")]));
        assert_eq!(
            bust, None,
            "canonical sorting makes registration order invisible"
        );
    }

    #[test]
    fn growing_history_is_never_a_cache_bust() {
        let mut assembler = Assembler::new();
        let (first, _) = assembler.assemble(request("s", vec![spec("read")]));

        let mut grown = request("s", vec![spec("read")]);
        grown.messages = first.messages.clone();
        grown.messages.push(Message::text(Role::Assistant, "on it"));
        grown.messages.push(Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult(crate::types::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file contents".into(),
                is_error: false,
            })],
        });
        let (_, bust) = assembler.assemble(grown);
        assert_eq!(
            bust, None,
            "history growth extends the prefix, not busts it"
        );
    }
}
