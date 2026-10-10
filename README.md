# IRIS

**A cache-aware, replayable, redacting, local-first AI coding agent for your terminal — in a single Rust binary.**

[![License](https://img.shields.io/badge/License-Apache--2.0-0F172A.svg?style=for-the-badge&labelColor=0F172A&logo=apache&logoColor=white)](LICENSE)
[![Status](https://img.shields.io/badge/Status-Pre--release-0F172A.svg?style=for-the-badge&labelColor=0F172A&logo=rocket&logoColor=white)](#project-status)
[![Platform](https://img.shields.io/badge/Platform-Linux%20%7C%20macOS-0F172A.svg?style=for-the-badge&labelColor=0F172A&logo=gnometerminal&logoColor=white)](#roadmap)
[![Language](https://img.shields.io/badge/Language-Rust-0F172A.svg?style=for-the-badge&labelColor=0F172A&logo=rust&logoColor=white)](#architecture)

---

## Overview

IRIS is an AI coding agent that runs entirely in your terminal. It connects to your
model provider of choice, reads and edits your codebase, runs commands behind an
explicit permission gate, and keeps a durable, portable record of every session.

The category already exists — and it has well-documented problems. IRIS is being
built around three principles that incumbents treat as afterthoughts:

| Principle | What it means |
|---|---|
| **Cache-first** | Prompt assembly is byte-stable by design, so provider prompt caches actually hit. Cache hit %, cost, TTFT, and tokens/sec are visible *live*, not reconstructed after the bill arrives. |
| **Local-first & private** | Your transcripts stay on your machine, secrets are redacted *before* they are written, and telemetry is off by default. Nothing leaves your machine except calls to the model provider you configured. |
| **Replayable & durable** | Sessions survive crashes, resume without amnesia, and are stored in a documented, versioned open format — no lock-in to one vendor's session files. |

## Why IRIS

The problems IRIS targets are not hypothetical; they are documented across the
public issue trackers of every major terminal coding agent:

- **Context bloat.** Compaction that fires too late — or never — after a handful of exchanges.
- **Cost anxiety.** No visibility into prompt-cache hit rates or per-turn spend while you work.
- **Resume amnesia.** `--resume` that silently loses history.
- **Secrets on disk.** `.env` contents and credentials echoed verbatim into local transcripts.
- **Review friction.** Diffs that are hard to comprehend and permissions that are either noisy or silently ignored.

## Capabilities

Development status is tracked in the [implementation plan](agent/PLAN.md). Items marked
✅ have landed in `main` but are **not part of a release yet** — nothing here is
released software.

- ✅ **Headless mode** — `iris run -p "…"` non-interactive runs: model text streams to
  stdout, tool events go to stderr, and the exit code is the agent outcome (CI-scriptable).
- ✅ **Permission gate** — per-tool `allow` / `deny` / `ask` policy from config, enforced
  for every tool call; headless runs fail closed.
- 🚧 **Crash-safe sessions** — append-only, versioned JSONL transcripts with resume and a
  rebuildable local index (Phase 2, in progress).
- 📋 **Live cost gauges** — cache hit %, cost, TTFT, and tokens/sec in the status bar.
- 📋 **Interactive TUI** — streaming markdown, diff views, and a permission prompt you can actually trust.
- 📋 **Workspace checkpoints** — snapshot and restore your working state around agent activity.
- 📋 **Default-on secret redaction** — patterns and entropy heuristics applied before anything is persisted.
- 📋 **MCP support** — external tools surfaced through the same registry and permission gate.
- 📋 **Hooks and custom commands** — project-local automation without forking the agent.

## Project status

**IRIS is pre-release and under active development.** Phases 0 and 1 of the
[implementation plan](agent/PLAN.md) are complete: the Cargo workspace, layered
configuration, the OpenRouter client with SSE streaming, five tools behind a
config-backed permission gate, and the headless agent loop have landed and are
runnable via `iris run -p` — all under test-driven development (190+ tests).
There are no versioned releases yet.

### Roadmap

Details are tracked in [agent/PLAN.md](agent/PLAN.md); this table reflects it at a high level.

| Phase | Scope | Status |
|---|---|---|
| 0 | Toolchain and workspace skeleton | ✅ Complete |
| 1 | Headless agent core loop (providers, streaming, tools, permissions) | ✅ Complete |
| 2 | Session durability (transcript schema, crash-safe writes, resume) | 🚧 In progress |
| 3 | Cache-first sessions (prefix-invariant assembly, token accounting, live metrics) | Planned |
| 4 | Terminal UI (composer, streaming pane, status bar, diff review) | Planned |
| 5 | Privacy (secret redaction, telemetry-off guarantee) | Planned |
| 6 | Parity features (checkpoints, compaction, MCP, hooks, custom commands) | Planned |
| 7 | Quality gate (coverage ≥ 80%, E2E scenarios, release hardening) | Planned |

Providers: OpenRouter only for now (its OpenAI-compatible API serves OpenAI, Anthropic,
and other models); a native Anthropic client is deferred. Deferred beyond v1: Windows
support, worktree replay A/B, local (GGUF) inference mode.

## Installation

**No releases yet** — IRIS has no published binaries. When a first release ships,
install instructions will appear here and on the [releases page](../../releases).

To use it today, build from source (Rust toolchain is pinned in
[`agent/rust-toolchain.toml`](agent/rust-toolchain.toml)):

```bash
git clone https://github.com/Siddhesh-ai-del/IRIS
cd IRIS/agent
cargo build --release
```

Then export your OpenRouter key and try the headless mode:

```bash
export OPENROUTER_API_KEY=sk-or-v1-…   # never read from a config file
./target/release/iris doctor
./target/release/iris run -p "list the files here and summarize what this project is"
```

`iris run -p` streams model text to stdout, tool events to stderr, and exits with
the agent outcome (`0` completed · `1` error · `3` max turns · `4` token budget).

## Architecture

IRIS is a single Rust binary organized as a Cargo workspace under `agent/`:

- **`iris-core`** — provider-agnostic agent loop, tool registry, permission gate,
  session store, prompt assembly, and metrics. Fully headless: no TUI code in the core.
- **`iris` (CLI)** — argument parsing, configuration layering (defaults → file →
  environment → flags), and the headless runner.
- **TUI** — a thin presentation layer driven by events emitted from the core
  (planned, Phase 4).

Provider access today is OpenRouter's OpenAI-compatible API with a hand-rolled SSE
client. The transcript format is a versioned, append-only JSONL schema, documented
publicly from day one so that other tools can read it.

## Repository layout

```
IRIS/
├── README.md          # You are here
├── CHANGELOG.md       # Release history (nothing released yet)
├── CONTRIBUTING.md    # How to contribute
├── CODE_OF_CONDUCT.md # Community standards
├── SECURITY.md        # Vulnerability reporting
├── SUPPORT.md         # Where to get help
├── LICENSE            # Apache License 2.0
├── docs/              # Product documentation (growing with the project)
├── .github/           # Issue and pull request templates
└── agent/             # Cargo workspace — all source code lives here
    ├── PLAN.md        # Implementation plan (single source of truth)
    ├── justfile       # fmt / lint / test / cov / ci
    └── crates/
        ├── core/      # iris-core — loop, tools, config, provider, sessions
        └── cli/       # iris — the terminal binary (run, chat, sessions, doctor)
```

## Contributing

IRIS is in early development and follows a staged TDD plan. The most useful
contributions right now are reproducible bug reports and well-scoped feature
ideas — see [CONTRIBUTING.md](CONTRIBUTING.md) and the issue templates. Code
contributions are welcome for planned stages: read the
[implementation plan](agent/PLAN.md) first so your work lines up with what is
being built next.

## Security

Please do not report security vulnerabilities through public issues. Follow the
process in [SECURITY.md](SECURITY.md). For anything involving credentials or
provider keys: IRIS will only ever read API keys from environment variables or
the system keyring — never from a config file, and never into a transcript.

## Support

Questions, discussion, and announcements live in
[GitHub Discussions](../../discussions) and
[GitHub Issues](../../issues). See [SUPPORT.md](SUPPORT.md) for details.

## License

Licensed under the [Apache License 2.0](LICENSE).
