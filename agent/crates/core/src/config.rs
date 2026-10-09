//! Layered configuration (stage 0.2).
//!
//! Precedence (lowest → highest): defaults → config file
//! (`~/.config/ferro/config.toml`) → `$FERRO_*` env vars → CLI flags.
//!
//! **Security invariant:** the API key is resolved *only* from
//! `OPENROUTER_API_KEY` in the process environment (or keyring, later) —
//! never from the config file. [`Config`] has no key field by design;
//! [`api_key_in_file`] exists so `doctor` can warn about keys mistakenly
//! stored there.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default OpenRouter base URL (confirmed decision D4).
pub const DEFAULT_PROVIDER_BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Environment variable that carries the API key — the *only* source.
pub const API_KEY_ENV_VAR: &str = "OPENROUTER_API_KEY";

/// Layered configuration values (no secrets, by design).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// OpenAI-compatible provider endpoint.
    #[serde(default = "default_base_url")]
    pub provider_base_url: String,
}

fn default_base_url() -> String {
    DEFAULT_PROVIDER_BASE_URL.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider_base_url: default_base_url(),
        }
    }
}

/// Inputs for one layered load. Everything is injected so precedence is
/// testable without touching the real filesystem or process env.
#[derive(Clone, Debug)]
pub struct ConfigInputs<'a> {
    /// Raw TOML of the config file, if it exists.
    pub file_toml: Option<&'a str>,
    /// Process environment (or any map) for `$FERRO_*` lookups.
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

/// Resolve the config file path: `$FERRO_CONFIG` (explicit override) →
/// `$XDG_CONFIG_HOME/ferro/config.toml` → `$HOME/.config/ferro/config.toml`.
/// Returns `None` when no base directory can be determined. Empty values
/// are treated as unset.
pub fn config_file_path(env: &HashMap<String, String>) -> Option<PathBuf> {
    let non_empty = |key: &str| {
        env.get(key)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };

    if let Some(explicit) = non_empty("FERRO_CONFIG") {
        return Some(explicit);
    }
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        return Some(xdg.join("ferro").join("config.toml"));
    }
    non_empty("HOME").map(|home| home.join(".config").join("ferro").join("config.toml"))
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
    }

    if let Some(url) = inputs
        .env
        .get("FERRO_PROVIDER_BASE_URL")
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
    {
        config.provider_base_url = url.to_string();
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
            &map(&[("FERRO_PROVIDER_BASE_URL", "https://env.example/v1")]),
            None,
        );
        assert_eq!(cfg.provider_base_url, "https://env.example/v1");
    }

    #[test]
    fn flag_layer_overrides_env() {
        let cfg = load(
            Some("provider_base_url = \"https://file.example/v1\""),
            &map(&[("FERRO_PROVIDER_BASE_URL", "https://env.example/v1")]),
            Some("https://flag.example/v1"),
        );
        assert_eq!(cfg.provider_base_url, "https://flag.example/v1");
    }

    #[test]
    fn empty_env_value_is_ignored_not_downgraded_to_empty_string() {
        let cfg = load(
            Some("provider_base_url = \"https://file.example/v1\""),
            &map(&[("FERRO_PROVIDER_BASE_URL", "")]),
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
            Some(PathBuf::from("/home/u/.config/ferro/config.toml"))
        );
    }

    #[test]
    fn config_path_prefers_xdg_over_home() {
        let env = map(&[("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/custom/xdg")]);
        assert_eq!(
            config_file_path(&env),
            Some(PathBuf::from("/custom/xdg/ferro/config.toml"))
        );
    }

    #[test]
    fn config_path_ferro_config_overrides_all() {
        let env = map(&[
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/custom/xdg"),
            ("FERRO_CONFIG", "/explicit/config.toml"),
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
        let env = map(&[("FERRO_CONFIG", ""), ("HOME", "")]);
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
}
