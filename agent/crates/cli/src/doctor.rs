//! `iris doctor` — report effective configuration and key *presence*
//! only. Key material is never formatted into output (stage 0.2 Done-when).

use std::collections::HashMap;

use iris_core::config::{self, ConfigInputs};

/// Print provider/key status. Returns the process exit code.
pub fn run(base_url_flag: Option<&str>, env: &HashMap<String, String>) -> i32 {
    let path = config::config_file_path(env);
    let mut file_desc = String::from("none (HOME/XDG not set)");

    let file_text = match &path {
        Some(p) => match std::fs::read_to_string(p) {
            Ok(text) => {
                file_desc = format!("loaded ({})", p.display());
                Some(text)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                file_desc = format!("not found ({})", p.display());
                None
            }
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", p.display());
                return 1;
            }
        },
        None => None,
    };

    let config = match config::load_config(&ConfigInputs {
        file_toml: file_text.as_deref(),
        env,
        flag_base_url: base_url_flag,
    }) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    println!("iris doctor");
    println!("  config file       : {file_desc}");
    println!(
        "  provider base URL : {} (source: {})",
        config.provider_base_url,
        describe_base_url_source(base_url_flag, env, file_text.as_deref())
    );
    println!(
        "  API key           : {}",
        match config::resolve_api_key(env) {
            Some(_) => "set via OPENROUTER_API_KEY (value redacted)",
            None => "not set",
        }
    );

    if file_text.as_deref().is_some_and(config::api_key_in_file) {
        println!(
            "  warning           : config file contains 'api_key' — keys are only \
             read from OPENROUTER_API_KEY; remove it from the file"
        );
    }

    0
}

/// Which layer won for `provider_base_url` (diagnostic display only).
fn describe_base_url_source(
    flag: Option<&str>,
    env: &HashMap<String, String>,
    file_toml: Option<&str>,
) -> &'static str {
    let env_sets = env
        .get("IRIS_PROVIDER_BASE_URL")
        .is_some_and(|v| !v.trim().is_empty());

    let file_sets = file_toml
        .and_then(|text| toml::from_str::<toml::Table>(text).ok())
        .is_some_and(|table| table.contains_key("provider_base_url"));

    if flag.is_some() {
        "flag"
    } else if env_sets {
        "env"
    } else if file_sets {
        "file"
    } else {
        "default"
    }
}
