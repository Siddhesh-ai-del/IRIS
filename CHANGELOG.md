# Changelog

All notable changes to IRIS will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Cargo workspace under `agent/` (`iris-core` + `iris` binary) with pinned Rust
  toolchain, `just` task runner, and conventional commits.
- Layered configuration (defaults → file → `$IRIS_*` env → flags) and `iris doctor`
  for provider/key status without exposing secrets.
- OpenRouter provider client (OpenAI-compatible) with SSE streaming, cancellation,
  and a mockable `Provider` trait.
- Tool registry with `read_file`, `list_dir`, `write_file`, `run_command`, and
  `apply_patch`, all confined to the session workdir.
- Config-backed permission gate (`allow` / `deny` / `ask` per tool, fails safe) and
  a headless agent loop with `max_turns` / token-budget limits.
- Headless `iris run -p` mode: model text on stdout, tool events on stderr,
  exit code = agent outcome (0 / 1 / 3 / 4).
- E2E harness spawning the real binary against a mock provider; 190+ tests.

- Initial product repository: documentation and community health files.

[Unreleased]: https://github.com/Siddhesh-ai-del/IRIS/compare/HEAD...HEAD
