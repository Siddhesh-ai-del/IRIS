//! Layered configuration (stage 0.2).
//!
//! Precedence (lowest → highest): defaults → config file
//! (`~/.config/iris/config.toml`) → `$IRIS_*` env vars → CLI flags.
//!
//! **Security invariant:** the API key is resolved *only* from
//! `OPENROUTER_API_KEY` in the process environment (or keyring, later) —
//! never from the config file. [`Config`] has no key field by design;
//! [`api_key_in_file`] exists so `doctor` can warn about keys mistakenly
//! stored there.
//!
//! Stage 1.9 adds loop knobs (`max_turns`, `max_tokens_budget`) and the
//! `[permissions]` per-tool policy table. Those are **file-layer only** —
//! env and flag layers carry scalars, not maps.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tools::permissions::{Decision, Gate, PolicyGate};

/// Default OpenRouter base URL (confirmed decision D4).
pub const DEFAULT_PROVIDER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Environment variable that carries the API key — the *only* source.
pub const API_KEY_ENV_VAR: &str = "OPENROUTER_API_KEY";

/// Default cap on provider calls per run (loop, stage 1.9).
pub const DEFAULT_MAX_TURNS: u32 = 50;

/// Default cumulative `total_tokens` budget per run (loop, stage 1.9) —
/// the sum of the usage reported by every response in the run.
pub const DEFAULT_MAX_TOKENS_BUDGET: u64 = 250_000;

/// Default context window for compaction (stage 6.2): 128k — a
/// common denominator across hosted models. Override with
/// `context_window` (`0` turns compaction off); there is no
/// per-model table yet, so a conservative guess compacts a big
/// window early (cheap) rather than a small window late (a provider
/// rejection).
pub const DEFAULT_CONTEXT_WINDOW: u64 = 131_072;

/// Default raw messages kept past a compaction (stage 6.2): the last
/// eight — roughly four exchanges — stay unpinned.
pub const DEFAULT_COMPACT_KEEP: usize = 8;

/// Default per-server tool budget for MCP (stage 6.3): at most this
/// many tools from one server reach the registry (the alphabetically
/// first ones, so the set is stable across restarts).
pub const DEFAULT_MCP_MAX_TOOLS: usize = 32;

/// Default per-tool result cap for MCP (stage 6.3): one tool call from
/// a server contributes at most this many bytes of content (plus a
/// fixed truncation marker), so a huge dump can't silently eat context.
pub const DEFAULT_MCP_MAX_RESULT_BYTES: usize = 32 * 1024;

/// Default total seconds one hook command may run before it is killed
/// (stage 6.4) — a hook hangs on the agent's critical path, so it gets
/// a hard deadline, not patience.
pub const DEFAULT_HOOK_TIMEOUT_SECS: u64 = 15;

/// Layered configuration values (no secrets, by design).
///
/// Not `Eq`: the stage-3.3 price table carries `f64` rates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// OpenAI-compatible provider endpoint.
    #[serde(default = "default_base_url")]
    pub provider_base_url: String,
    /// Hard cap on provider calls per run (loop, stage 1.9).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// Cumulative `total_tokens` budget per run (loop, stage 1.9).
    #[serde(default = "default_max_tokens_budget")]
    pub max_tokens_budget: u64,
    /// Model context window in tokens for compaction (stage 6.2).
    /// `0` disables compaction entirely.
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    /// Non-system messages kept raw past a compaction (stage 6.2).
    #[serde(default = "default_compact_keep")]
    pub compact_keep: usize,
    /// Per-tool permission policy overlay — `tool = "allow" | "deny" |
    /// "ask"` (stage 1.9). Unlisted tools fall back to the built-in
    /// policy: reads allowed, everything else asks.
    #[serde(default)]
    pub permissions: BTreeMap<String, Decision>,
    /// Pluggable price table (stage 3.3): model id → USD rates per
    /// million tokens. **File-layer only** (like `permissions` — env and
    /// flag layers carry scalars, not maps) and never hardcoded: a model
    /// missing from the table simply reports no estimated cost.
    #[serde(default)]
    pub pricing: BTreeMap<String, ModelPrice>,
    /// MCP servers to connect over stdio (stage 6.3): server name →
    /// spawn command + per-server tool budget. **File-layer only**
    /// (a map, like `permissions`). No secrets here either — server
    /// credentials belong in the child process's own environment,
    /// never in this file; the API key rule (D6) is unaffected: only
    /// `OPENROUTER_API_KEY` is ever read from the environment.
    #[serde(default)]
    pub mcp: McpConfig,
    /// Loop-lifecycle hooks (stage 6.4): shell commands fired at
    /// `on_tool_pre`, `on_tool_post`, `on_turn_end`, and `on_compact`
    /// with a redacted JSON payload on stdin. **File-layer only**
    /// (a table of commands, like `permissions`) and trusted exactly
    /// like `[mcp]` spawn commands: user-authored code in the user's
    /// own config file, run with the workspace root as cwd — no new
    /// secret surface (D6 untouched).
    #[serde(default)]
    pub hooks: Hooks,
}

/// USD price list for one model (stage 3.3) — per million tokens,
/// straight from the config file's `[pricing."<model>"]` table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Rate for uncached input tokens.
    pub input_per_million: f64,
    /// Rate for output (completion) tokens.
    pub output_per_million: f64,
    /// Discounted rate for input tokens read from the provider's
    /// prompt cache. Absent → the cached share bills at the input rate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_per_million: Option<f64>,
}

/// MCP server set (stage 6.3): `[mcp.servers."<name>"]` entries.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct McpConfig {
    /// Server name → spawn configuration. Sorted by name (a `BTreeMap`),
    /// so servers connect in a deterministic order.
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// How to spawn one MCP server over stdio, plus its tool budget
/// (stage 6.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Executable to spawn (looked up on `PATH` if not absolute).
    pub command: String,
    /// Arguments passed to `command` verbatim.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child process (e.g. a
    /// server-side token the server reads itself — never the OpenRouter
    /// key, which has its own rule).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Cap on tools this server may register: the alphabetically first
    /// `max_tools` win, anything beyond is dropped with a warning.
    #[serde(default = "default_mcp_max_tools")]
    pub max_tools: usize,
    /// Cap on bytes one tool result from this server may contribute
    /// (a fixed truncation marker is appended past the cap).
    #[serde(default = "default_mcp_max_result_bytes")]
    pub max_result_bytes: usize,
}

impl McpServerConfig {
    /// A server entry with default budgets — programmatic config and
    /// tests; TOML loads use serde defaults for the same values.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            max_tools: DEFAULT_MCP_MAX_TOOLS,
            max_result_bytes: DEFAULT_MCP_MAX_RESULT_BYTES,
        }
    }
}

/// User hooks (stage 6.4): the `[hooks]` table — shell commands fired
/// at the agent loop's four lifecycle points. Every command runs under
/// `sh -c` with the workspace root as its cwd, receives the event's
/// JSON payload (redacted through the stage-5.1 chokepoint) on stdin,
/// and is killed after `timeout_secs`; stdout and stderr are discarded.
/// Failures are advisories ([`LoopEvent::HookWarning`](crate::agent_loop::LoopEvent)),
/// never run errors. Unknown event names are a config error — a typo
/// must not silently do nothing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Before every tool dispatch — the model's request as asked,
    /// before the permission gate resolves it.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub on_tool_pre: Vec<String>,
    /// After every dispatch — the outcome the model will see
    /// (`is_error` covers denials, failures, and errors alike).
    #[serde(default, deserialize_with = "string_or_vec")]
    pub on_tool_post: Vec<String>,
    /// After every completed turn, the final one included.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub on_turn_end: Vec<String>,
    /// After a compaction folds the history, before the next request.
    #[serde(default, deserialize_with = "string_or_vec")]
    pub on_compact: Vec<String>,
    /// Total seconds one command may run before it is killed.
    /// `0` clamps to one second — there is no "wait forever".
    #[serde(default = "default_hook_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for Hooks {
    fn default() -> Self {
        Self {
            on_tool_pre: Vec::new(),
            on_tool_post: Vec::new(),
            on_turn_end: Vec::new(),
            on_compact: Vec::new(),
            timeout_secs: DEFAULT_HOOK_TIMEOUT_SECS,
        }
    }
}

/// One command string or a list of them — `on_tool_pre = "cmd"` and
/// `on_tool_pre = ["a", "b"]` both work.
fn string_or_vec<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(de)? {
        OneOrMany::One(one) => vec![one],
        OneOrMany::Many(many) => many,
    })
}

fn default_base_url() -> String {
    DEFAULT_PROVIDER_BASE_URL.to_string()
}
fn default_max_turns() -> u32 {
    DEFAULT_MAX_TURNS
}

fn default_max_tokens_budget() -> u64 {
    DEFAULT_MAX_TOKENS_BUDGET
}

fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

fn default_compact_keep() -> usize {
    DEFAULT_COMPACT_KEEP
}

fn default_mcp_max_tools() -> usize {
    DEFAULT_MCP_MAX_TOOLS
}

fn default_mcp_max_result_bytes() -> usize {
    DEFAULT_MCP_MAX_RESULT_BYTES
}

fn default_hook_timeout_secs() -> u64 {
    DEFAULT_HOOK_TIMEOUT_SECS
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider_base_url: default_base_url(),
            max_turns: default_max_turns(),
            max_tokens_budget: default_max_tokens_budget(),
            context_window: default_context_window(),
            compact_keep: default_compact_keep(),
            permissions: BTreeMap::new(),
            pricing: BTreeMap::new(),
            mcp: McpConfig::default(),
            hooks: Hooks::default(),
        }
    }
}

impl Config {
    /// The permission gate these settings describe: `[permissions]`
    /// entries overlay the built-in per-tool policy (stage 1.9). The
    /// loop installs it in the [`ToolContext`](crate::tools::ToolContext)
    /// so every dispatch passes through it.
    pub fn permission_gate(&self) -> Gate {
        Arc::new(PolicyGate::new(self.permissions.clone()))
    }
}

/// Inputs for one layered load. Everything is injected so precedence is
/// testable without touching the real filesystem or process env.
#[derive(Clone, Debug)]
pub struct ConfigInputs<'a> {
    /// Raw TOML of the config file, if it exists.
    pub file_toml: Option<&'a str>,
    /// Process environment (or any map) for `$IRIS_*` lookups.
    pub env: &'a HashMap<String, String>,
    /// CLI `--base-url` value, the highest-precedence layer.
    pub flag_base_url: Option<&'a str>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid provider base URL `{0}` (expected an http/https URL)")]
    InvalidBaseUrl(String),
}

/// Resolve the config file path: `$IRIS_CONFIG` (explicit override) →
/// `$XDG_CONFIG_HOME/iris/config.toml` → `$HOME/.config/iris/config.toml`.
/// Returns `None` when no base directory can be determined. Empty values
/// are treated as unset.
pub fn config_file_path(env: &HashMap<String, String>) -> Option<PathBuf> {
    let non_empty = |key: &str| {
        env.get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };

    if let Some(explicit) = non_empty("IRIS_CONFIG") {
        return Some(explicit);
    }
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        return Some(xdg.join("iris").join("config.toml"));
    }
    non_empty("HOME").map(|home| home.join(".config").join("iris").join("config.toml"))
}

/// Why a config **write-back** failed (stage 4.5: "always allow"
/// rules persisted to the config file).
#[derive(Debug)]
pub enum PersistError {
    /// The existing file is not valid TOML — it is left untouched,
    /// because overwriting a config we cannot read would lose settings.
    Corrupt(String),
    /// The file could not be written (directory creation, write, or
    /// atomic rename failed).
    Io(String),
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Corrupt(err) => write!(f, "config is not valid TOML: {err}"),
            PersistError::Io(err) => write!(f, "config write failed: {err}"),
        }
    }
}

impl std::error::Error for PersistError {}

/// Persist one `[permissions]` entry — the stage-4.5 "always allow
/// (rule)" write-back: read the existing config as a TOML table,
/// set/overwrite `[permissions].<tool>`, write `config.toml.tmp` and
/// rename over the original (read-modify-write, so every other
/// *setting* survives). Creates the parent directory when missing.
///
/// Known skeleton caveat: the TOML round-trip **drops comments** from
/// the rewritten file (the parser does not keep them) — a
/// comment-preserving write is a listed follow-up, not implemented.
pub fn persist_permission(path: &Path, tool: &str, decision: Decision) -> Result<(), PersistError> {
    // Read-modify-write: a config we cannot parse is never
    // overwritten — that would lose the user's other settings.
    let mut table = match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<toml::Table>(&text)
            .map_err(|err| PersistError::Corrupt(err.to_string()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(err) => return Err(PersistError::Io(err.to_string())),
    };

    let permissions = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or_else(|| PersistError::Corrupt("`permissions` is not a table".into()))?;
    let name = match decision {
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::Ask => "ask",
    };
    permissions.insert(tool.to_string(), toml::Value::String(name.to_string()));

    let text = toml::to_string(&table).map_err(|err| PersistError::Io(err.to_string()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| PersistError::Io(err.to_string()))?;
    }
    // Write beside the target, then rename — a crash mid-write leaves
    // the old config intact.
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).map_err(|err| PersistError::Io(err.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|err| PersistError::Io(err.to_string()))
}

/// Apply the four configuration layers and validate the result.
pub fn load_config(inputs: &ConfigInputs<'_>) -> Result<Config, ConfigError> {
    let mut config = Config::default();

    if let Some(toml_text) = inputs.file_toml {
        let file: Config = toml::from_str(toml_text)?;
        // Layer only the fields the file actually set (serde fills defaults
        // for the rest, which would silently mask lower layers otherwise —
        // re-parse as a table to check presence).
        let table: toml::Table = toml::from_str(toml_text)?;
        if table.contains_key("provider_base_url") {
            config.provider_base_url = file.provider_base_url;
        }
        if table.contains_key("max_turns") {
            config.max_turns = file.max_turns;
        }
        if table.contains_key("max_tokens_budget") {
            config.max_tokens_budget = file.max_tokens_budget;
        }
        if table.contains_key("context_window") {
            config.context_window = file.context_window;
        }
        if table.contains_key("compact_keep") {
            config.compact_keep = file.compact_keep;
        }
        if table.contains_key("permissions") {
            config.permissions = file.permissions;
        }
        if table.contains_key("pricing") {
            config.pricing = file.pricing;
        }
        if table.contains_key("mcp") {
            config.mcp = file.mcp;
        }
        if table.contains_key("hooks") {
            config.hooks = file.hooks;
        }
    }

    if let Some(url) = inputs
        .env
        .get("IRIS_PROVIDER_BASE_URL")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        config.provider_base_url = url.to_string();
    }

    // Stage 6.2: scalar compaction knobs. A value that doesn't parse
    // is ignored — a typo must not take the whole config load down.
    if let Some(window) = inputs
        .env
        .get("IRIS_CONTEXT_WINDOW")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        && let Ok(parsed) = window.parse::<u64>()
    {
        config.context_window = parsed;
    }
    if let Some(keep) = inputs
        .env
        .get("IRIS_COMPACT_KEEP")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        && let Ok(parsed) = keep.parse::<usize>()
    {
        config.compact_keep = parsed;
    }

    if let Some(url) = inputs
        .flag_base_url
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        config.provider_base_url = url.to_string();
    }

    validate_base_url(&config.provider_base_url)?;
    Ok(config)
}

fn validate_base_url(url: &str) -> Result<(), ConfigError> {
    match url::Url::parse(url) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(()),
        _ => Err(ConfigError::InvalidBaseUrl(url.to_string())),
    }
}

/// Resolve the API key from `OPENROUTER_API_KEY` only. Empty/whitespace
/// values yield `None`. Never reads the config file.
pub fn resolve_api_key(env: &HashMap<String, String>) -> Option<String> {
    env.get(API_KEY_ENV_VAR)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Detect an `api_key`-like key in config file contents so `doctor` can
/// warn that the file is not a supported key location.
pub fn api_key_in_file(file_toml: &str) -> bool {
    toml::from_str::<toml::Table>(file_toml)
        .map(|table| table.keys().any(|key| key.eq_ignore_ascii_case("api_key")))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn load(file: Option<&str>, env: &HashMap<String, String>, flag: Option<&str>) -> Config {
        load_config(&ConfigInputs {
            file_toml: file,
            env,
            flag_base_url: flag,
        })
        .unwrap()
    }

    // --- precedence -----------------------------------------------------

    #[test]
    fn defaults_apply_when_no_source_sets_base_url() {
        let cfg = load(None, &map(&[]), None);
        assert_eq!(cfg.provider_base_url, DEFAULT_PROVIDER_BASE_URL);
    }

    #[test]
    fn file_layer_overrides_default() {
        let cfg = load(
            Some("provider_base_url = \"https://file.example/v1\""),
            &map(&[]),
            None,
        );
        assert_eq!(cfg.provider_base_url, "https://file.example/v1");
    }

    #[test]
    fn env_layer_overrides_file() {
        let cfg = load(
            Some("provider_base_url = \"https://file.example/v1\""),
            &map(&[("IRIS_PROVIDER_BASE_URL", "https://env.example/v1")]),
            None,
        );
        assert_eq!(cfg.provider_base_url, "https://env.example/v1");
    }

    #[test]
    fn flag_layer_overrides_env() {
        let cfg = load(
            Some("provider_base_url = \"https://file.example/v1\""),
            &map(&[("IRIS_PROVIDER_BASE_URL", "https://env.example/v1")]),
            Some("https://flag.example/v1"),
        );
        assert_eq!(cfg.provider_base_url, "https://flag.example/v1");
    }

    #[test]
    fn empty_env_value_is_ignored_not_downgraded_to_empty_string() {
        let cfg = load(
            Some("provider_base_url = \"https://file.example/v1\""),
            &map(&[("IRIS_PROVIDER_BASE_URL", "")]),
            None,
        );
        assert_eq!(cfg.provider_base_url, "https://file.example/v1");
    }

    #[test]
    fn unknown_file_keys_are_ignored_for_forward_compatibility() {
        let cfg = load(Some("future_knob = true\n"), &map(&[]), None);
        assert_eq!(cfg.provider_base_url, DEFAULT_PROVIDER_BASE_URL);
    }

    // --- validation -------------------------------------------------------

    #[test]
    fn rejects_non_http_base_url_from_flag() {
        let err = load_config(&ConfigInputs {
            file_toml: None,
            env: &map(&[]),
            flag_base_url: Some("ftp://nope.example"),
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidBaseUrl(_)));
    }

    #[test]
    fn rejects_garbage_base_url() {
        let err = load_config(&ConfigInputs {
            file_toml: None,
            env: &map(&[]),
            flag_base_url: Some("not a url"),
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::InvalidBaseUrl(_)));
    }

    #[test]
    fn malformed_toml_reports_parse_error() {
        let err = load_config(&ConfigInputs {
            file_toml: Some("this is not toml ="),
            env: &map(&[]),
            flag_base_url: None,
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)));
    }

    // --- config file path resolution ---------------------------------------

    #[test]
    fn config_path_falls_back_to_home_dot_config() {
        let env = map(&[("HOME", "/home/u")]);
        assert_eq!(
            config_file_path(&env),
            Some(PathBuf::from("/home/u/.config/iris/config.toml"))
        );
    }

    #[test]
    fn config_path_prefers_xdg_over_home() {
        let env = map(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/custom/xdg")]);
        assert_eq!(
            config_file_path(&env),
            Some(PathBuf::from("/custom/xdg/iris/config.toml"))
        );
    }

    #[test]
    fn config_path_iris_config_overrides_all() {
        let env = map(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/custom/xdg"),
            ("IRIS_CONFIG", "/explicit/config.toml"),
        ]);
        assert_eq!(
            config_file_path(&env),
            Some(PathBuf::from("/explicit/config.toml"))
        );
    }

    #[test]
    fn config_path_none_without_any_base() {
        assert_eq!(config_file_path(&map(&[])), None);
    }

    #[test]
    fn config_path_treats_empty_values_as_unset() {
        let env = map(&[("IRIS_CONFIG", ""), ("HOME", "")]);
        assert_eq!(config_file_path(&env), None);
    }

    // --- API key: env only -------------------------------------------------

    #[test]
    fn api_key_reads_openrouter_env_var() {
        let env = map(&[(API_KEY_ENV_VAR, "sk-or-v1-abc123")]);
        assert_eq!(resolve_api_key(&env).as_deref(), Some("sk-or-v1-abc123"));
    }

    #[test]
    fn api_key_trims_accidental_whitespace() {
        let env = map(&[(API_KEY_ENV_VAR, "  sk-or-v1-abc123\n")]);
        assert_eq!(resolve_api_key(&env).as_deref(), Some("sk-or-v1-abc123"));
    }

    #[test]
    fn api_key_empty_value_is_none() {
        let env = map(&[(API_KEY_ENV_VAR, "   ")]);
        assert_eq!(resolve_api_key(&env), None);
    }

    #[test]
    fn api_key_never_comes_from_config_file() {
        // A key smuggled into the config file must not resolve.
        let file = "api_key = \"sk-or-v1-LEAKED\"\n";
        let cfg = load(Some(file), &map(&[]), None);
        assert_eq!(cfg.provider_base_url, DEFAULT_PROVIDER_BASE_URL);
        // The Config struct has no key field; resolution ignores the file.
        assert_eq!(resolve_api_key(&map(&[])), None);
        // And doctor gets a warning signal.
        assert!(api_key_in_file(file));
    }

    #[test]
    fn api_key_not_flagged_in_clean_file() {
        assert!(!api_key_in_file(
            "provider_base_url = \"https://x.example/v1\"\n"
        ));
    }

    #[test]
    fn api_key_detection_is_case_insensitive_on_key_name() {
        assert!(api_key_in_file("API_KEY = \"x\"\n"));
    }

    // --- loop limits + permission policy (stage 1.9) ----------------------

    #[test]
    fn loop_limits_and_permissions_have_sane_defaults() {
        let cfg = load(None, &map(&[]), None);
        assert_eq!(cfg.max_turns, DEFAULT_MAX_TURNS);
        assert_eq!(cfg.max_tokens_budget, DEFAULT_MAX_TOKENS_BUDGET);
        assert!(cfg.permissions.is_empty());
    }

    #[test]
    fn file_layer_sets_loop_limits_and_permission_policy() {
        let cfg = load(
            Some(
                r#"
                max_turns = 7
                max_tokens_budget = 12345

                [permissions]
                run_command = "deny"
                read_file = "allow"
                "#,
            ),
            &map(&[]),
            None,
        );
        assert_eq!(cfg.max_turns, 7);
        assert_eq!(cfg.max_tokens_budget, 12345);
        assert_eq!(cfg.permissions.get("run_command"), Some(&Decision::Deny));
        assert_eq!(cfg.permissions.get("read_file"), Some(&Decision::Allow));
    }

    #[test]
    fn invalid_permission_value_fails_the_load() {
        let err = load_config(&ConfigInputs {
            file_toml: Some("[permissions]\nread_file = \"maybe\"\n"),
            env: &map(&[]),
            flag_base_url: None,
        })
        .unwrap_err();
        assert!(matches!(err, ConfigError::Parse(_)), "got {err:?}");
    }

    /// Stage 3.3 — the price table is a file-layer map (env/flag layers
    /// carry scalars), absent by default: an unlisted model simply has
    /// no table entry to estimate cost from.
    #[test]
    fn price_table_is_file_layer_and_defaults_empty() {
        let cfg = load(None, &map(&[]), None);
        assert!(cfg.pricing.is_empty(), "no hardcoded prices by default");

        let cfg = load(
            Some(
                r#"
                [pricing."openai/gpt-4o-mini"]
                input_per_million = 0.15
                output_per_million = 0.60
                cached_input_per_million = 0.075

                [pricing."anthropic/claude-3.5-sonnet"]
                input_per_million = 3.0
                output_per_million = 15.0
                "#,
            ),
            &map(&[]),
            None,
        );
        assert_eq!(
            cfg.pricing.get("openai/gpt-4o-mini"),
            Some(&ModelPrice {
                input_per_million: 0.15,
                output_per_million: 0.60,
                cached_input_per_million: Some(0.075),
            }),
            "per-model rates come from the file, not the code"
        );
        assert_eq!(
            cfg.pricing.get("anthropic/claude-3.5-sonnet"),
            Some(&ModelPrice {
                input_per_million: 3.0,
                output_per_million: 15.0,
                cached_input_per_million: None,
            }),
            "cached rate is optional — absent bills at the input rate"
        );
        assert_eq!(cfg.pricing.get("some/unknown-model"), None);
    }

    #[test]
    fn permission_gate_applies_overlay_and_builtin_defaults() {
        let cfg = load(
            Some("[permissions]\nrun_command = \"allow\"\n"),
            &map(&[]),
            None,
        );
        let gate = cfg.permission_gate();
        // Config overlay wins …
        assert_eq!(gate.check("run_command", "{}"), Decision::Allow);
        // … builtin defaults cover the rest: reads allowed, the rest asks.
        assert_eq!(gate.check("read_file", "{}"), Decision::Allow);
        assert_eq!(gate.check("list_dir", "{}"), Decision::Allow);
        assert_eq!(gate.check("write_file", "{}"), Decision::Ask);
    }

    // --- always-allow rule write-back (stage 4.5) -------------------------

    fn table_at(path: &std::path::Path) -> toml::Table {
        let text = std::fs::read_to_string(path).expect("config written");
        toml::from_str(&text).expect("written config parses")
    }

    #[test]
    fn persist_permission_creates_the_config_when_missing() {
        let dir = tempfile::tempdir().expect("temp dir");
        // Parent `iris/` does not exist yet — write-back creates it.
        let path = dir.path().join("iris").join("config.toml");
        persist_permission(&path, "run_command", Decision::Allow).expect("persisted");
        let table = table_at(&path);
        assert_eq!(
            table["permissions"]["run_command"].as_str(),
            Some("allow"),
            "rule lands under [permissions]"
        );
    }

    #[test]
    fn persist_permission_preserves_every_other_setting() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "max_turns = 42\n\n[permissions]\nread_file = \"allow\"\n",
        )
        .expect("seed config");

        persist_permission(&path, "run_command", Decision::Allow).expect("persisted");

        let table = table_at(&path);
        assert_eq!(table["max_turns"].as_integer(), Some(42), "kept max_turns");
        assert_eq!(
            table["permissions"]["read_file"].as_str(),
            Some("allow"),
            "kept the other permission"
        );
        assert_eq!(
            table["permissions"]["run_command"].as_str(),
            Some("allow"),
            "added the new rule"
        );
    }

    #[test]
    fn persist_permission_overwrites_a_previous_rule() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[permissions]\nrun_command = \"deny\"\n").expect("seed");

        persist_permission(&path, "run_command", Decision::Allow).expect("persisted");

        let table = table_at(&path);
        assert_eq!(
            table["permissions"]["run_command"].as_str(),
            Some("allow"),
            "deny becomes allow"
        );
    }

    // --- mcp servers (stage 6.3) ------------------------------------------

    #[test]
    fn file_layer_loads_mcp_servers_with_budget_defaults() {
        let cfg = load(
            Some(
                "[mcp.servers.demo]\n\
                 command = \"npx\"\n\
                 args = [\"-y\", \"server-thing\"]\n",
            ),
            &map(&[]),
            None,
        );
        let demo = cfg.mcp.servers.get("demo").expect("server entry");
        assert_eq!(demo.command, "npx");
        assert_eq!(demo.args, vec!["-y", "server-thing"]);
        assert!(demo.env.is_empty());
        assert_eq!(demo.max_tools, DEFAULT_MCP_MAX_TOOLS);
        assert_eq!(demo.max_result_bytes, DEFAULT_MCP_MAX_RESULT_BYTES);
    }

    #[test]
    fn file_layer_mcp_budgets_and_env_override_defaults() {
        let cfg = load(
            Some(
                "[mcp.servers.demo]\n\
                 command = \"uvx\"\n\
                 max_tools = 3\n\
                 max_result_bytes = 512\n\
                 env = { TOKEN = \"t\" }\n",
            ),
            &map(&[]),
            None,
        );
        let demo = cfg.mcp.servers.get("demo").expect("server entry");
        assert_eq!(demo.max_tools, 3);
        assert_eq!(demo.max_result_bytes, 512);
        assert_eq!(demo.env.get("TOKEN").map(String::as_str), Some("t"));
    }

    #[test]
    fn mcp_server_without_a_command_is_a_config_error() {
        // `command` is required — a server that can't be spawned must
        // not look "configured" and silently do nothing.
        let err = load_config(&ConfigInputs {
            file_toml: Some("[mcp.servers.broken]\nargs = [\"x\"]\n"),
            env: &map(&[]),
            flag_base_url: None,
        })
        .expect_err("missing command must not parse");
        assert!(
            err.to_string().contains("command"),
            "error names the missing field: {err}"
        );
    }

    #[test]
    fn no_mcp_table_leaves_servers_empty() {
        let cfg = load(Some("max_turns = 7"), &map(&[]), None);
        assert!(cfg.mcp.servers.is_empty());
    }

    // --- hooks (stage 6.4) ------------------------------------------------

    #[test]
    fn file_layer_loads_hooks_accepting_string_or_array_commands() {
        let cfg = load(
            Some(
                "[hooks]\n\
                 on_tool_pre = \"say pre\"\n\
                 on_turn_end = [\"a.sh\", \"b.sh\"]\n\
                 timeout_secs = 3\n",
            ),
            &map(&[]),
            None,
        );
        assert_eq!(cfg.hooks.on_tool_pre, vec!["say pre"]);
        assert_eq!(cfg.hooks.on_tool_post, Vec::<String>::new());
        assert_eq!(cfg.hooks.on_turn_end, vec!["a.sh", "b.sh"]);
        assert_eq!(cfg.hooks.on_compact, Vec::<String>::new());
        assert_eq!(cfg.hooks.timeout_secs, 3);
    }

    #[test]
    fn no_hooks_table_leaves_hooks_empty_with_the_default_timeout() {
        let cfg = load(Some("max_turns = 7"), &map(&[]), None);
        assert_eq!(cfg.hooks, Hooks::default());
        assert!(cfg.hooks.on_tool_pre.is_empty());
        assert_eq!(cfg.hooks.timeout_secs, DEFAULT_HOOK_TIMEOUT_SECS);
    }

    #[test]
    fn an_unknown_hook_event_is_a_config_error() {
        // A typo (`on_tool_prre`) must fail loudly, not silently do
        // nothing — hooks are a trust surface, not a scratch pad.
        let err = load_config(&ConfigInputs {
            file_toml: Some("[hooks]\non_tool_prre = \"x\"\n"),
            env: &map(&[]),
            flag_base_url: None,
        })
        .expect_err("unknown hook names must not parse");
        assert!(err.to_string().contains("on_tool_prre"), "{err}");
    }
}
