//! The pending-edit diff view (stage 4.6).
//!
//! When the loop asks permission for a file edit (`write_file` /
//! `apply_patch`), the prompt shows the edit as **hunks** instead of
//! raw JSON: `write_file` diffs the proposed content against the
//! file's current content (via `similar`), `apply_patch` splits the
//! patch text at its `@@` headers (the patch already *is* a diff).
//!
//! Each hunk can be accepted or rejected (`space` in the popup). The
//! aggregate feeds back to the loop through
//! [`PermissionAnswer::AllowWith`](crate::app::PermissionAnswer):
//! all accepted = plain allow of the model's call; any rejected =
//! [`EditDiff::replacement_input`] rebuilds the input so the call runs
//! with **only the accepted hunks** — the rejected parts never touch
//! the disk.

use std::ops::Range;

use serde_json::{Value, json};

/// Context lines kept around each change group (unified-diff style).
const CONTEXT: usize = 3;

/// A pending file edit, rendered as hunks, awaiting permission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditDiff {
    /// Target path — also the popup title's second half.
    pub path: String,
    /// What kind of edit this is (drives the partial-apply rebuild).
    pub kind: EditKind,
    /// The hunks, in source order.
    pub hunks: Vec<Hunk>,
}

/// How to interpret a hunk's ranges — and how a partial apply is
/// rebuilt from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditKind {
    /// `write_file`: a whole-file replacement; a partial apply replays
    /// accepted hunk blocks over the old lines.
    Write { old: Vec<String>, new: Vec<String> },
    /// `apply_patch`: the patch text carries its own hunks; a partial
    /// apply drops the rejected sections.
    Patch { text: String },
}

/// One hunk: what the popup renders, plus the source ranges a partial
/// apply needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    /// Unified-diff style header (`@@ -1,3 +1,4 @@`).
    pub header: String,
    /// Render lines: `(sign, text)` with `+` / `-` / ` `.
    pub lines: Vec<(char, String)>,
    /// Whether the user accepted this hunk (accepted by default).
    pub accepted: bool,
    /// Where this hunk's bytes/lines live in the source material.
    pub source: HunkSource,
}

/// The source ranges a partial apply needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HunkSource {
    /// `write_file`: the hunk spans these line ranges of old/new.
    Write {
        old_range: Range<usize>,
        new_range: Range<usize>,
    },
    /// `apply_patch`: byte range of this hunk's section in the patch
    /// text (its `@@` header through the lines before the next one).
    Patch { section: Range<usize> },
}

impl EditDiff {
    /// Whether every hunk is accepted — then the caller just allows
    /// the model's call unchanged.
    pub fn all_accepted(&self) -> bool {
        self.hunks.iter().all(|hunk| hunk.accepted)
    }

    /// `(accepted, total)` — for the popup's per-hunk bookkeeping and
    /// the status note on a partial apply.
    pub fn accepted_counts(&self) -> (usize, usize) {
        let total = self.hunks.len();
        let accepted = self.hunks.iter().filter(|hunk| hunk.accepted).count();
        (accepted, total)
    }

    /// Flip the hunk at `cursor` between accepted and rejected.
    pub fn toggle(&mut self, cursor: usize) {
        if let Some(hunk) = self.hunks.get_mut(cursor) {
            hunk.accepted = !hunk.accepted;
        }
    }

    /// The **replaced input JSON** the loop should run when only part
    /// of the edit was accepted. `None` when everything is accepted
    /// (the original call is correct as issued).
    pub fn replacement_input(&self) -> Option<String> {
        if self.all_accepted() {
            return None;
        }
        let value = match &self.kind {
            EditKind::Write { old, new } => {
                // Replay: gaps and rejected hunks keep the old lines,
                // accepted hunks contribute their new block.
                let mut out = String::new();
                let mut cursor = 0;
                for hunk in &self.hunks {
                    let HunkSource::Write {
                        old_range,
                        new_range,
                    } = &hunk.source
                    else {
                        continue;
                    };
                    for line in &old[cursor..old_range.start] {
                        out.push_str(line);
                    }
                    let block = if hunk.accepted { new } else { old };
                    let range = if hunk.accepted { new_range } else { old_range };
                    for line in &block[range.clone()] {
                        out.push_str(line);
                    }
                    cursor = old_range.end;
                }
                for line in &old[cursor..] {
                    out.push_str(line);
                }
                json!({"path": self.path, "content": out})
            }
            EditKind::Patch { text } => {
                // Keep accepted sections only — and drop any
                // whole-file `new_content` fallback: it would silently
                // undo the rejection (the tool writes it when the
                // patch fails to apply).
                let mut rebuilt = String::new();
                let mut cursor = 0;
                for hunk in &self.hunks {
                    let HunkSource::Patch { section } = &hunk.source else {
                        continue;
                    };
                    rebuilt.push_str(&text[cursor..section.start]);
                    if hunk.accepted {
                        rebuilt.push_str(&text[section.clone()]);
                    }
                    cursor = section.end;
                }
                rebuilt.push_str(&text[cursor..]);
                json!({"patch": rebuilt})
            }
        };
        Some(value.to_string())
    }
}

/// Build the hunk view for a pending file edit, or `None` when this
/// ask has no diff to show (other tools, malformed detail).
pub fn build(name: &str, detail: &str, old: Option<&str>) -> Option<EditDiff> {
    let value: Value = serde_json::from_str(detail).ok()?;
    match name {
        "write_file" => {
            let path = value.get("path")?.as_str()?.to_string();
            let new = value.get("content")?.as_str()?;
            Some(write_diff(path, old.unwrap_or(""), new))
        }
        "apply_patch" => {
            let text = value.get("patch")?.as_str()?.to_string();
            let path = patch_path(&text)?;
            Some(patch_diff(path, text))
        }
        _ => None,
    }
}

/// A `write_file` edit: `similar` groups the line changes into hunks
/// (unified-diff style, [`CONTEXT`] lines around each change).
fn write_diff(path: String, old: &str, new: &str) -> EditDiff {
    let old_lines: Vec<String> = old.split_inclusive('\n').map(str::to_string).collect();
    let new_lines: Vec<String> = new.split_inclusive('\n').map(str::to_string).collect();
    let diff = similar::TextDiff::from_lines(old, new);
    let hunks = diff
        .grouped_ops(CONTEXT)
        .into_iter()
        // A group of pure context is not a hunk.
        .filter(|group| group.iter().any(|op| op.tag() != similar::DiffTag::Equal))
        .map(|group| {
            let old_start = group.first().map_or(0, |op| op.old_range().start);
            let old_end = group.last().map_or(0, |op| op.old_range().end);
            let new_start = group.first().map_or(0, |op| op.new_range().start);
            let new_end = group.last().map_or(0, |op| op.new_range().end);
            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_changes(op) {
                    let sign = match change.tag() {
                        similar::ChangeTag::Delete => '-',
                        similar::ChangeTag::Insert => '+',
                        similar::ChangeTag::Equal => ' ',
                    };
                    lines.push((sign, change.value().to_string()));
                }
            }
            let header = format!(
                "@@ -{},{} +{},{} @@",
                old_start + 1,
                old_end - old_start,
                new_start + 1,
                new_end - new_start
            );
            Hunk {
                header,
                lines,
                accepted: true,
                source: HunkSource::Write {
                    old_range: old_start..old_end,
                    new_range: new_start..new_end,
                },
            }
        })
        .collect();
    EditDiff {
        path,
        kind: EditKind::Write {
            old: old_lines,
            new: new_lines,
        },
        hunks,
    }
}

/// An `apply_patch` edit: the patch text already is a diff — split it
/// at its `@@` headers, one hunk per section.
fn patch_diff(path: String, text: String) -> EditDiff {
    // Each `@@` header line starts a section running to the next one
    // (or the end); the file headers before the first `@@` belong to
    // no hunk. Byte offsets into `text` are the partial-apply handles.
    let mut starts: Vec<usize> = Vec::new();
    let mut at = 0;
    for line in text.split_inclusive('\n') {
        if line.starts_with("@@") {
            starts.push(at);
        }
        at += line.len();
    }
    let hunks = starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(text.len());
            let section = &text[start..end];
            let lines = section
                .lines()
                .skip(1)
                .filter_map(|line| {
                    let sign = line.chars().next()?;
                    matches!(sign, '+' | '-' | ' ').then_some((sign, line.to_string()))
                })
                .collect::<Vec<_>>();
            let header = section.lines().next().unwrap_or("@@").to_string();
            Hunk {
                header,
                lines,
                accepted: true,
                source: HunkSource::Patch {
                    section: start..end,
                },
            }
        })
        .collect();
    EditDiff {
        path,
        kind: EditKind::Patch { text },
        hunks,
    }
}

/// Parse the patch headers for the target path (`+++`, falling back to
/// `---`; git `a/`/`b/` prefixes stripped — same rules as the tool).
fn patch_path(text: &str) -> Option<String> {
    let mut fallback: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            return Some(strip_git_prefix(rest));
        }
        if fallback.is_none()
            && let Some(rest) = line.strip_prefix("--- ")
        {
            fallback = Some(strip_git_prefix(rest));
        }
    }
    fallback
}

fn strip_git_prefix(header: &str) -> String {
    // Headers may carry a trailing tab-separated timestamp.
    let path = header.split_whitespace().next().unwrap_or(header);
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_builds_hunks_and_replays_only_accepted_parts() {
        let detail = json!({
            "path": "src/main.rs",
            "content": "fn main() {\n    let name = \"iris\";\n    println!(\"{name}\");\n}\n",
        })
        .to_string();
        let old = "fn main() {\n    println!(\"hi\");\n}\n";

        let mut diff = build("write_file", &detail, Some(old)).expect("diff built");
        assert_eq!(diff.path, "src/main.rs");
        assert_eq!(diff.hunks.len(), 1, "one change: {:?}", diff.hunks);
        let hunk = &diff.hunks[0];
        assert!(hunk.header.starts_with("@@"), "header: {}", hunk.header);
        let signs: String = hunk.lines.iter().map(|(sign, _)| sign).collect();
        assert!(
            signs.contains('-') && signs.contains('+'),
            "lines: {signs:?}"
        );

        // Accepted by default → nothing partial to send.
        assert!(diff.all_accepted());
        assert!(diff.replacement_input().is_none());

        // Reject the hunk → the replacement replays the old content
        // untouched (the write becomes a no-op instead of an edit).
        diff.hunks[0].accepted = false;
        let input: Value =
            serde_json::from_str(&diff.replacement_input().expect("partial")).expect("json");
        assert_eq!(input["path"], "src/main.rs");
        assert_eq!(input["content"], old, "rejected hunk → old content");
    }

    #[test]
    fn write_file_partial_apply_keeps_accepted_hunks_only() {
        // Two distant changes → two hunks.
        let old = "a1\na2\na3\na4\na5\na6\na7\na8\na9\na10\n";
        let new = "a1\nCHANGED\na3\na4\na5\na6\na7\na8\na9\nCHANGED2\n";
        let detail = json!({"path": "f.txt", "content": new}).to_string();

        let mut diff = build("write_file", &detail, Some(old)).expect("diff built");
        assert_eq!(diff.hunks.len(), 2, "hunks: {:?}", diff.hunks);

        // Reject the first change, keep the second.
        diff.hunks[0].accepted = false;
        let input: Value =
            serde_json::from_str(&diff.replacement_input().expect("partial")).expect("json");
        let expected = "a1\na2\na3\na4\na5\na6\na7\na8\na9\nCHANGED2\n";
        assert_eq!(
            input["content"], expected,
            "accepted hunk kept, rejected reverted"
        );
    }

    #[test]
    fn apply_patch_sections_become_hunks_and_partial_drops_rejected() {
        let patch = "--- src/lib.rs\n\
             +++ src/lib.rs\n\
             @@ -1,2 +1,2 @@\n\
             fn a() {}\n\
             -fn b() {}\n\
             +fn b2() {}\n\
             @@ -9,2 +9,2 @@\n\
             -fn c() {}\n\
             +fn c2() {}\n\
             fn d() {}\n";
        let detail = json!({"patch": patch, "new_content": "whole-file fallback"}).to_string();

        let mut diff = build("apply_patch", &detail, None).expect("diff built");
        assert_eq!(diff.path, "src/lib.rs");
        assert_eq!(diff.hunks.len(), 2, "hunks: {:?}", diff.hunks);
        assert!(
            diff.hunks[0].header.contains("-1,2"),
            "header: {}",
            diff.hunks[0].header
        );
        assert!(diff.all_accepted());

        // Reject the second hunk → its section is gone from the patch
        // AND the new_content fallback goes with it (it would undo
        // the rejection by writing the whole file anyway).
        diff.hunks[1].accepted = false;
        let input: Value =
            serde_json::from_str(&diff.replacement_input().expect("partial")).expect("json");
        let rebuilt = input["patch"].as_str().expect("patch");
        assert!(rebuilt.contains("b2"), "kept hunk stays: {rebuilt}");
        assert!(!rebuilt.contains("c2"), "rejected hunk dropped: {rebuilt}");
        assert!(
            input.get("new_content").is_none(),
            "fallback dropped: {input}"
        );
    }

    #[test]
    fn non_file_tools_have_no_diff() {
        assert!(build("run_command", "{\"cmd\":\"ls\"}", None).is_none());
        assert!(build("read_file", "{\"path\":\"a.txt\"}", None).is_none());
    }
}
