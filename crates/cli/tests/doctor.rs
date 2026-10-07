//! Stage 0.2 integration tests: `ferro doctor` must report provider/key
//! status *without* ever printing key material.

use std::process::Command;

fn ferro() -> Command {
    Command::new(env!("CARGO_BIN_EXE_ferro"))
}

fn stdout_of(cmd: &mut Command) -> String {
    let out = cmd.output().expect("ferro binary must run");
    assert!(
        out.status.success(),
        "doctor exited with {:?}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn doctor_reports_default_provider_and_missing_key() {
    let mut cmd = ferro();
    cmd.arg("doctor")
        .env("FERRO_CONFIG", "/nonexistent/ferro-config.toml")
        .env_remove("OPENROUTER_API_KEY")
        .env("FERRO_PROVIDER_BASE_URL", "")
        .env_remove("FERRO_PROVIDER_BASE_URL");
    let stdout = stdout_of(&mut cmd);

    assert!(
        stdout.contains("https://openrouter.ai/api/v1"),
        "provider base URL missing from: {stdout}"
    );
    assert!(
        stdout.to_lowercase().contains("key") && stdout.contains("not set"),
        "key status missing from: {stdout}"
    );
}

#[test]
fn doctor_never_leaks_api_key_material() {
    let secret = "sk-or-v1-SUPERSECRET-DO-NOT-PRINT";
    let mut cmd = ferro();
    cmd.arg("doctor")
        .env("FERRO_CONFIG", "/nonexistent/ferro-config.toml")
        .env("OPENROUTER_API_KEY", secret);
    let out = cmd.output().expect("ferro binary must run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(out.status.success(), "doctor failed: {combined}");
    assert!(
        !combined.contains("SUPERSECRET"),
        "key material leaked in doctor output"
    );
    assert!(
        combined.contains("set") && !combined.contains("not set"),
        "key presence status missing: {combined}"
    );
}

#[test]
fn doctor_shows_effective_base_url_from_env_layer() {
    let mut cmd = ferro();
    cmd.arg("doctor")
        .env("FERRO_CONFIG", "/nonexistent/ferro-config.toml")
        .env("FERRO_PROVIDER_BASE_URL", "https://custom.example/v1")
        .env_remove("OPENROUTER_API_KEY");
    let stdout = stdout_of(&mut cmd);
    assert!(
        stdout.contains("https://custom.example/v1"),
        "env-layer base URL missing from: {stdout}"
    );
}

#[test]
fn help_lists_all_stage_0_2_subcommands() {
    let out = ferro().arg("--help").output().expect("ferro --help runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for sub in ["run", "chat", "sessions", "doctor"] {
        assert!(stdout.contains(sub), "`{sub}` missing from help: {stdout}");
    }
}

#[test]
fn run_accepts_prompt_and_workdir_flags() {
    // Stage 0.2 only defines the CLI surface; execution lands in 1.10.
    let out = ferro()
        .arg("run")
        .arg("--help")
        .output()
        .expect("ferro run --help runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("-p") || stdout.contains("--prompt"));
    assert!(stdout.contains("--workdir"));
}
