//! Project-local commands (stage 6.5): markdown prompt templates in
//! `.iris/commands/*.md` under the workspace root, dispatched from the
//! composer as `/name args…`.
//!
//! File format — an optional strict frontmatter followed by the
//! template body:
//!
//! ```markdown
//! ---
//! description: Review a diff
//! args: [scope]
//! ---
//! Review the {{scope}} diff in this repository and report findings.
//! ```
//!
//! - The file name is the command name (`review.md` → `/review`).
//! - `description` and `args` are the only frontmatter keys; `args`
//!   must be an inline list of identifiers. A file that starts with
//!   `---` must close it — anything else is a load warning, never a
//!   panic.
//! - The body is the prompt: `{{name}}` placeholders bind to the
//!   positional arguments (`/review backend` → `{{scope}}` =
//!   `backend`); exact arity is enforced (usage error otherwise), and
//!   an undeclared `{{x}}` stays literal.
//!
//! Loading is best-effort like MCP (stage 6.3): unreadable or broken
//! files become warnings and the rest still load; names reserved by
//! the builtins (`/help`, `/quit`, `/clear`) are skipped with a
//! warning. Nothing here ever fails a run.

use std::collections::BTreeMap;
use std::path::Path;

/// One project command: everything frontmatter + body, keyed by its
/// file stem in the map [`load`] returns.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectCommand {
    /// Frontmatter `description` — what `/help` shows. Empty when the
    /// file declared none.
    pub description: String,
    /// Frontmatter `args`, bound positionally at dispatch.
    pub args: Vec<String>,
    /// The markdown template body, `{{arg}}` placeholders intact.
    pub body: String,
}

/// Why one `.iris/commands` file could not become a command (surfaced
/// as a load warning, one per broken file).
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParseError {
    /// The file opened with `---` but never closed it.
    #[error("unterminated frontmatter (no closing ---)")]
    Unterminated,
    /// A frontmatter line that is not `key: value`.
    #[error("frontmatter lines must be `key: value`")]
    InvalidLine,
    /// A key other than `description`/`args` — a typo must not
    /// silently do nothing.
    #[error("unknown frontmatter key `{0}` (expected description or args)")]
    UnknownKey(String),
    /// The same key twice.
    #[error("duplicate frontmatter key `{0}`")]
    DuplicateKey(String),
    /// `args` was not `[a, b]` form.
    #[error("args must be an inline list like [scope, style]")]
    ArgsNotList,
    /// An empty item in the `args` list (e.g. `[a,]`).
    #[error("empty argument name")]
    EmptyArg,
    /// An argument name that cannot appear as `{{name}}`.
    #[error("invalid argument name `{0}`")]
    BadArgName(String),
    /// The same argument declared twice.
    #[error("duplicate argument `{0}`")]
    DuplicateArg(String),
    /// Nothing to dispatch: no body survived parsing/trimming.
    #[error("empty command body")]
    EmptyBody,
}

/// Why a template could not expand for the typed arguments.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("expected {expected} argument(s), got {got}")]
pub struct ExpandError {
    /// How many the frontmatter declares.
    pub expected: usize,
    /// How many were supplied.
    pub got: usize,
}

/// Load every `*.md` command from `<root>/.iris/commands`, sorted by
/// name (a `BTreeMap` — `/help` lists them deterministically).
///
/// `reserved` holds builtin names (without the slash) that project
/// files must not shadow. Best-effort: unreadable directory without
/// entries → empty map, missing directory → empty map, each broken
/// file → a warning string; the rest still load.
pub fn load(root: &Path, reserved: &[&str]) -> (BTreeMap<String, ProjectCommand>, Vec<String>) {
    let dir = root.join(".iris").join("commands");
    let mut commands = BTreeMap::new();
    let mut warnings = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        // No `.iris/commands` is simply no project commands.
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return (commands, warnings);
        }
        Err(err) => {
            warnings.push(format!("cannot read {}: {err}", dir.display()));
            return (commands, warnings);
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| !stem.is_empty())
        else {
            warnings.push(format!("{}: not a usable command name", path.display()));
            continue;
        };
        if reserved.contains(&name) {
            warnings.push(format!(
                ".iris/commands/{name}.md shadows a builtin command — skipped"
            ));
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                warnings.push(format!(".iris/commands/{name}.md: {err}"));
                continue;
            }
        };
        match parse(&text) {
            Ok(command) => {
                commands.insert(name.to_string(), command);
            }
            Err(err) => warnings.push(format!(".iris/commands/{name}.md: {err}")),
        }
    }
    (commands, warnings)
}

/// Bind `argv` positionally to the command's declared args and return
/// the expanded prompt — exact arity or [`ExpandError`]; undeclared
/// `{{x}}` placeholders stay literal.
pub fn expand(command: &ProjectCommand, argv: &[String]) -> Result<String, ExpandError> {
    if argv.len() != command.args.len() {
        return Err(ExpandError {
            expected: command.args.len(),
            got: argv.len(),
        });
    }
    let mut body = command.body.clone();
    for (arg, value) in command.args.iter().zip(argv) {
        body = body.replace(&format!("{{{{{arg}}}}}"), value);
    }
    Ok(body)
}

/// Parse one file's contents: strict `---` frontmatter + body, or a
/// body-only template when the file doesn't open with `---`.
fn parse(text: &str) -> Result<ProjectCommand, ParseError> {
    // Tolerate a BOM and leading blank lines; the body gets trimmed
    // anyway, and Windows editors love BOMs.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text).trim_start();
    let first = text.split('\n').next().unwrap_or_default();
    if first.trim_end() != "---" {
        return body_only(text);
    }
    // A file that opens with `---` must close it — even a lone `---`
    // is an unfinished frontmatter, not a horizontal rule.
    let Some((_, remainder)) = text.split_once('\n') else {
        return Err(ParseError::Unterminated);
    };
    let mut consumed = 0usize;
    let mut frontmatter = None;
    for line in remainder.split_inclusive('\n') {
        if line.trim_end() == "---" {
            frontmatter = Some(&remainder[..consumed]);
            consumed += line.len();
            break;
        }
        consumed += line.len();
    }
    let Some(frontmatter) = frontmatter else {
        return Err(ParseError::Unterminated);
    };
    let body = remainder[consumed..].trim();
    if body.is_empty() {
        return Err(ParseError::EmptyBody);
    }
    let mut description = String::new();
    let mut args: Vec<String> = Vec::new();
    let mut seen_description = false;
    let mut seen_args = false;
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            return Err(ParseError::InvalidLine);
        };
        match key.trim() {
            "description" => {
                if seen_description {
                    return Err(ParseError::DuplicateKey("description".into()));
                }
                seen_description = true;
                description = value.trim().to_string();
            }
            "args" => {
                if seen_args {
                    return Err(ParseError::DuplicateKey("args".into()));
                }
                seen_args = true;
                args = parse_args(value.trim())?;
            }
            other => return Err(ParseError::UnknownKey(other.to_string())),
        }
    }
    Ok(ProjectCommand {
        description,
        args,
        body: body.to_string(),
    })
}

/// A template with no frontmatter at all: the whole (trimmed) file.
fn body_only(text: &str) -> Result<ProjectCommand, ParseError> {
    let body = text.trim();
    if body.is_empty() {
        return Err(ParseError::EmptyBody);
    }
    Ok(ProjectCommand {
        description: String::new(),
        args: Vec::new(),
        body: body.to_string(),
    })
}

/// Parse the `args` value: an inline `[a, b]` list of identifiers —
/// `{{name}}` placeholders only accept `[A-Za-z0-9_-]+` names.
fn parse_args(value: &str) -> Result<Vec<String>, ParseError> {
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .ok_or(ParseError::ArgsNotList)?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut args: Vec<String> = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        if item.is_empty() {
            return Err(ParseError::EmptyArg);
        }
        if !item
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(ParseError::BadArgName(item.to_string()));
        }
        if args.iter().any(|seen| seen == item) {
            return Err(ParseError::DuplicateArg(item.to_string()));
        }
        args.push(item.to_string());
    }
    Ok(args)
}
