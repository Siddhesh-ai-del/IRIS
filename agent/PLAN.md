# IRIS — Implementation Plan (TDD)

> **Terminal-based AI coding agent in Rust.** Derived from the deep-research brief
> `outputs/rust-terminal-coding-agent.md` (provenance: `outputs/rust-terminal-coding-agent.provenance.md`).
> **This file is the single source of truth for development progress.**

## How to resume (any time)

```
/tdd Read agent/PLAN.md and continue TDD development from the first unchecked stage below.
Follow the stage's Done-when check, RED→GREEN→REFACTOR, and pause for my sign-off after each stage.
```

(Or `/tdd ... stop after phase N` / `... only do stage X.Y` to scope it.)

## Confirmed decisions

| # | Decision | Status |
|---|---|---|
| D1 | Project/binary name: **iris** (crates: `iris-core`, `iris` binary) | ✅ confirmed |
| D2 | Execution mode: **pause after each stage** for sign-off | ✅ confirmed |
| D3 | v1 targets **Linux/macOS only** (Windows deferred) | ✅ confirmed (default) |
| D4 | Providers: **OpenRouter only for now** (OpenAI-compatible API, own SSE client — not `genai`); native Anthropic `/v1/messages` client deferred (OpenRouter serves Anthropic models through the same API). Base URL configurable, default `https://openrouter.ai/api/v1` | ✅ confirmed (revised) |
| D6 | No `OPENAI_API_KEY` / `ANTHROPIC_API_KEY` available — auth is **`OPENROUTER_API_KEY`** (env/keyring only, never config file) | ✅ confirmed |
| D5 | Transcript schema **documented publicly from day one** (`docs/transcript-schema.md`) | ✅ confirmed (default) |

## TDD conventions (per stage, mandatory)

- RED → GREEN → REFACTOR; a stage is not done until tests that were failing now pass.
- Coverage ≥80% on `iris-core` (security-critical code — path confinement, redaction, permission gate — targets 100%).
- Immutability: builder/`with_*` methods, no mutation of shared state.
- No hardcoded secrets: API keys only from env/keyring, never config files.
- Each stage: single binary deliverable + `Done when` check + risk rating.
- Stage size ≤1h. If a stage blows past it, split the stage and note it here.

## Session state (updated as work proceeds)

- **Next action:** Stage 7.3 (E2E: real repo, temp git worktree — prompt → read → patch → checkpoint → resume → rewind, headless)
- **Current phase:** 7 — Quality Gate (next: 7.3; 7.2 complete)
- **Completed stages:** 0.1 ✅, 0.2 ✅, 1.1 ✅, 1.2 ✅, 1.3 ✅ (2026-10-07/08), 1.5 ✅ (2026-10-09), 1.6 ✅ (2026-10-09), 1.7 ✅ (2026-10-09), 1.8 ✅ (2026-10-09; 1.4 skipped), 1.9 ✅ (2026-10-09), 1.10 ✅ (2026-10-10), 2.1 ✅ (2026-10-10), 2.2 ✅ (2026-10-10), 2.3 ✅ (2026-10-11), 2.4 ✅ (2026-10-12), 3.1 ✅ (2026-10-13), 3.2 ✅ (2026-10-13), 3.3 ✅ (2026-10-14), 3.4 ✅ (2026-10-14), 4.1 ✅ (2026-10-15), 4.2 ✅ (2026-10-15), 4.3 ✅ (2026-10-17), 4.4 ✅ (2026-10-18), 4.5 ✅ (2026-10-19), 4.6 ✅ (2026-10-19; Phase 4 complete), 5.1 ✅ (2026-10-19), 5.2 ✅ (2026-10-19), 5.3 ✅ (2026-10-19; Phase 5 complete — live-validated with a real OpenRouter free model), 6.1 ✅ (2026-10-20), 6.2 ✅ (2026-10-20), 6.3 ✅ (2026-10-20), 6.4 ✅ (2026-10-20), 6.5 ✅ (2026-10-20; Phase 6 complete), 7.1 ✅ (2026-10-25), 7.2 ✅ (2026-10-25)

### Environment notes (observed 2026-09-30)

- **Rust is NOT installed** (no `cargo`/`rustc`/`rustup`/`~/.cargo/bin`) — Stage 0.1 blocker.
- **Network was severely throttled** during research day: ~5–18 KB/s to static.rust-lang.org,
  static.crates.io, and GitHub (would make toolchain install take hours). Re-test before
  starting 0.1; if still slow, run the rustup download in a background command with a long timeout.
- OS: **CachyOS** (Arch-based), `pacman` available. `sudo` requires a password — any
  `sudo` step (e.g. `pacman -S rustup`) must be run by the user, not the agent.
- `git` available. `Project/` is **not** a git repo yet — `git init` is part of Stage 0.1.
- `just`/`cargo-nextest`/`cargo-llvm-cov` not installed yet.
- **API keys:** only OpenRouter available (D4/D6) — no direct OpenAI/Anthropic keys. All live
  smoke tests go through OpenRouter; CI uses httpmock cassettes regardless.
  User must supply `OPENROUTER_API_KEY` at runtime (never committed, never in config).

#### Update (2026-10-07)

- Network recovered: ~3.2 MB/s general, GitHub ~400 KB/s, static.rust-lang.org ~97 KB/s
  (5–10× faster than research day). Toolchain install completed in background.
- **Rust 1.99.0 installed** (rustup, minimal + clippy/rustfmt), pinned in `rust-toolchain.toml`.
  `just` 1.58.0, `cargo-nextest` 0.9.146, `cargo-llvm-cov` 0.9.1 — all prebuilt binaries in
  `~/.cargo/bin`. PATH needs `~/.cargo/bin` sourced per shell (`. "$HOME/.cargo/env"`).
- `agent/` is now a git repo (root commit `ff4831d`); `IRIS/` is a separate repo.
- Stage 0.1 gate: `just test` green (1 passed), `cargo clippy --all-targets -- -D warnings` clean.
- Stage 0.2 gate: `just test` green (26 passed — 21 config precedence + 5 doctor CLI),
  clippy clean, `iris doctor` smoke verified (no key leak). `cargo llvm-cov` deferred:
  needs `llvm-tools` component (slow download) — required by stage 7.2, not 0.2.
- Stage 1.1 gate: 38 tests green (17 types: round-trips, wire-shape, insta snapshot),
  clippy clean. insta needs `features = ["json"]` for `assert_json_snapshot!`.
- Stage 1.2 gate: 60 tests green (22 new: 14 wire translation + 8 httpmock client),
  fmt + clippy `-D warnings` clean. Deps added: `reqwest 0.13` (rustls, no openssl),
  `tokio`, `httpmock 0.8` (dev). Deviations: `trait Provider::complete` uses
  `async fn` in impl but RPITIT `+ Send` in trait definition (loop will run in a
  spawned task); empty tool `arguments` string → `{}`; missing `finish_reason`
  inferred (tool calls → tool_use), unknown value fails loud.
- Stage 1.3 gate: 85 tests green (25 new: 10 SSE decoder, 13 stream/parser/
  assembler/property/cancellation, 2 httpmock streaming), fmt + clippy clean.
  Deps added: `futures`, `bytes`, tokio `signal`, `libc` (dev), reqwest `stream`.
  Notes: nextest 0.9.146 defaults to **fail-fast** — RED evidence needs
  `cargo nextest run --no-fail-fast`; reqwest 0.13 has no `reqwest::Bytes`
  re-export and `connect_timeout` only on `ClientBuilder` (moved there, applies
  to both paths); streaming has no total deadline by design. EOF without
  `[DONE]` tolerated (synthesized Done, stop reason inferred); tool call missing
  id → `call_{index}` placeholder, missing name → Decode error.
- Stage 1.5 gate: 92 tests green (7 new: dispatch/id-stamp, boundary-validation
  before dispatch, unknown tool, validate-without-dispatch, schemars schema +
  insta snapshot, no-arg `{}`, upsert), fmt + clippy clean. Deps added:
  `schemars 1.2` (derive). Design notes: `Tool::execute` is `impl Future + Send`
  (async-ready for the 1.7 shell tool); registry is closure-based (no
  `dyn Tool` — the associated `Args` type keeps the trait non-object-safe);
  registry stamps `tool_use_id` (tools may leave it empty).
- Stage 1.6 gate: 124 tests green (32 new), fmt + clippy clean. Deps added:
  `proptest 1.11` (dev). Security notes: `ToolError::PathEscape` variant added
  for auditable confinement failures; tool IO failures surface as
  `ToolError::Failed` (loop converts both to error `ToolResult`s at 1.9);
  **100% coverage target for this stage is verified at 7.2** (`cargo-llvm-cov`
  still deferred — llvm-tools download too slow here); proptest defaults
  (256 cases) — rerun-stable 3/3.
- Stage 1.7 gate: 149 tests green (25 new), fmt + clippy clean. Deps
  added: `portable-pty 0.9`, `libc` (promoted dev → prod, for
  `killpg`/`fcntl(O_NONBLOCK)`/`EIO`). portable-pty 0.9 notes: no
  `new_default_shell` — resolve via `CommandBuilder::new(..).get_shell()`
  ($SHELL → passwd db); its `ExitStatus` is crate-owned
  (`exit_code() -> u32`, `signal() -> Option<&str>`); unix spawn does
  `setsid` + `TIOCSCTTY` (child = session+group leader → `killpg`
  reaches grandchildren); `CommandBuilder::new` inherits the env by
  default. `ToolContext` is now gate-carrying (manual `Debug`).
- Stage 1.8 gate: 165 tests green (16 new), fmt + clippy clean. Deps
  added: `diffy 0.5.2`. Notes: diffy's `Patch::from_str` borrows the
  input (named lifetime in `target_of`); `patch.modified()/original()`
  expose header paths (no hand-rolled header parser); diffy has NO
  context-content fuzz — only positional offset search (plan's
  "fuzz failure" = exhausted offset search); hunk body lines need their
  leading space/context marker or the parser rejects them (caught in a
  test fixture); golden files are in-module const strings (self-contained
  unit tests, no external fixture dir).

---

## Phases

**Convention:** every stage is ≤1h, has a single deliverable and a `Done when` check.
Tick boxes are the progress meter. **Pause after each stage for sign-off (D2).**

### Phase 0 — Toolchain & Skeleton (2 stages, ~2h)

- [x] **0.1 Install toolchain + scaffold workspace** (1h) — done 2026-10-07
  - Install `rustup` (stable, minimal profile), `cargo-nextest`, `cargo-llvm-cov`, `just`
    (binary installers preferred over `cargo install` given slow network; `pacman` needs the user).
    Create workspace at `agent/` with `crates/core` (`iris-core`) and `crates/cli` (`iris` binary);
    `rust-toolchain.toml` pinned; `.cargo/config.toml` with `clippy -D warnings`; `justfile` targets:
    `fmt`, `lint`, `test`, `cov`; `git init` + conventional commits from here on.
  - **Done when:** `just test` runs an empty suite green; `cargo clippy -- -D warnings` clean.
  - Risk: Low (env setup) · Network-dependent.

- [x] **0.2 CLI + config skeleton** (1h) — done 2026-10-07
  - Deviation: layering hand-rolled in `iris-core::config` (defaults → file →
    `$IRIS_*` env → flags) instead of `config-rs`, because config-rs's env source
    reads process env directly (unsafe/racy to mutate under edition 2024 tests) and
    can't be tested hermetically. Same precedence, injected env-map inputs.
  - Extra: `IRIS_CONFIG` env override for config path; `--base-url` global flag;
    `doctor` warns if config file contains `api_key` (never reads it).
  - `crates/cli/src/main.rs` with `clap 4` subcommands `run`, `chat`, `sessions`, `doctor`;
    `config-rs` layering (defaults → `~/.config/iris/config.toml` → `$IRIS_*` env → flags);
    provider base URL config (default `https://openrouter.ai/api/v1`);
    API key loading **only** from env/keyring (`OPENROUTER_API_KEY`), never config file.
  - **Done when:** `iris doctor` prints provider/key status without leaking key material;
    unit tests for config precedence (RED first).
  - Risk: Low · Depends: 0.1

### Phase 1 — Headless Core Loop (10 stages, ~10h) — the agent before it has a face

- [x] **1.1 Domain types** (1h) — done 2026-10-08 — `crates/core/src/types.rs`:
  `Role`, `Message`, `ContentBlock` (text | image | tool_use | tool_result, tagged
  `type` field), `ToolCall`, `ToolResult`, `Usage`, `StopReason`;
  serde round-trip tests + wire-shape assertions + `insta` JSON snapshot
  (`snapshot_full_conversation_json`). 38 tests green.
  Risk: Low · Depends: 0.1

- [x] **1.2 Provider trait + OpenRouter (OpenAI-compatible) non-streaming** (1h) — done 2026-10-08
  - `crates/core/src/provider/mod.rs`: `trait Provider` (RPITIT `+ Send` `complete(...)`),
    `ToolSpec`, `CompletionRequest`/`CompletionResponse`, typed `ProviderError`;
    `wire.rs` domain⇄OpenAI translation (tool-role messages flatten per `tool_call_id`,
    `arguments` as JSON string, image → data-URL part, `finish_reason` mapping);
    `openai.rs` via `reqwest` with **configurable base URL** (OpenRouter default), bearer auth from
    `OPENROUTER_API_KEY` (passed in by caller, D6); `httpmock` fake `/chat/completions`.
    60 tests green, fmt + clippy clean.
  Risk: Low · Depends: 1.1

- [x] **1.3 SSE streaming** (1h) — done 2026-10-08
  - `sse.rs` incremental byte-level decoder (LF/CRLF, split lines, split UTF-8
    chars, comments, `data` quirk); `stream.rs` chunk DTOs + `StreamParser`
    (index-keyed tool fragments, args accumulated mid-JSON, finish/usage/
    `[DONE]`), `event_stream` unfold, `StreamAssembler` (events → domain);
    `Provider::complete_stream` + `collect_stream`; cancellation via
    `tokio::select!` (partial progress preserved) + real in-process SIGINT test.
  - **Done when:** property test over chunk sizes [len,64,13,7,5,2,1] —
    streamed reassembly byte-identical to non-streaming. 85 tests green, fmt +
    clippy clean.
  Risk: Medium · Depends: 1.2

- [ ] **1.4 Anthropic native client** (1h) — **DEFERRED under D4/D6 (OpenRouter-only):**
  second trait impl against `/v1/messages` with `cache_control` ephemeral blocks and Anthropic's
  tool-call delta shape, needed only if/when a direct `ANTHROPIC_API_KEY` is added.
  Skip this stage for now (keep the trait abstraction provider-agnostic so it can land later
  without rework); OpenRouter serves Anthropic models via stage 1.2's client.
  Risk: Medium · Depends: 1.3 · **Status: deferred**

- [x] **1.5 Tool registry + JSON schemas** (1h) — done 2026-10-09
  - `crates/core/src/tools/registry.rs`: `trait Tool` (`type Args: Deserialize +
    JsonSchema`; `name`/`description`/`schema`/`spec`/`parse`/`execute(ctx, args)`
    with `impl Future + Send`, mirroring `Provider`), `ToolContext`
    (workspace_root), `ToolError` (UnknownTool/InvalidArgs/Failed),
    `ToolRegistry` (register upsert, specs, contains, validate, async execute).
  - One `Args` type is the single source of truth: `schemars::schema_for!` feeds
    the model; `serde_json::from_value` validates **at the registry boundary**
    before the tool body runs (proven by a fixture tool whose body-flag never
    flips on invalid args); registry stamps `tool_use_id` onto the result.
  - 92 tests green (7 new incl. insta schema snapshot), fmt + clippy clean.
  Risk: Low · Depends: 1.1

- [x] **1.6 Read/Write/List tools** (1h) — done 2026-10-09
  - `tools/path.rs` confinement core: **lexical `.`/`..` normalization first**
    (so `link/../x` never depends on symlink targets), then canonicalize the
    longest *lstat-existing* prefix (broken symlinks count as existing → never
    written through) and require **component-wise** containment vs the
    canonical root (`/ws` never prefixes `/ws_evil`); remainder is
    normalized + nonexistent → can't hold symlinks. Violations →
    `ToolError::PathEscape`. Residual TOCTOU documented (adversary = model,
    not concurrent local attacker).
  - `read_file` (64 KiB cap, truncation **with visible notice**, UTF-8 lossy),
    `write_file` (parent-dir creation, overwrite), `list_dir` (sorted,
    `/`-suffixed dirs, 500-entry cap + notice, non-recursive).
  - 124 tests green (32 new: 13 path incl. symlink/dir/file/sibling-prefix
    escapes + proptest property over generated `..`/`.`/segment paths, 7 read,
    6 write incl. outside-file-unchanged proofs, 6 list; end-to-end escape
    attempts through the registry). fmt + clippy clean.
  Risk: Medium (security) · Depends: 1.5

- [x] **1.7 Shell tool via `portable-pty`** (1h) — done 2026-10-09
  - **Permission gate seam landed now** (the stage demands the shell tool
    is *always routed through* it): `tools/permissions.rs`
    (`Decision{Allow,Deny,Ask}` + `PermissionGate` + `AllowAll`/`DenyAll` +
    closure impl); `ToolContext` gains `permissions` (`new()` = AllowAll
    for tests, `with_gate()` for wiring); **registry gates dispatch**
    (uniform + forget-proof, not per-tool): new auditable
    `ToolError::Denied` / `ToolError::NeedsConfirmation` (unresolved `Ask`
    fails closed). 1.9 later installs the config-backed policy gate.
  - `run_command` tool: real PTY (`portable-pty 0.9`), user's shell
    (`$SHELL` → passwd-db fallback) with `-c`; PTY merges stdout+stderr
    (inherent — schema says so); streamed capture on a worker thread over
    a **non-blocking** master fd (no pty-buffer deadlock, keeps draining
    past the 256 KiB cap so the child never stalls, hard deadline so the
    reader always returns even if a grandchild holds the pty); poll-loop
    `try_wait` reap (keeps `&mut child` for the kill); timeout →
    `killpg(SIGKILL)` (portable-pty `setsid`s the child = group leader)
    + direct kill; explicit `[iris: …]` trailers for exit code /
    signal death / timeout / truncation. Note: interactive stdin not
    supported (stdin reads block until timeout kill) — future work.
  - Tests: registry gate seam asserts the tool body **never ran**
    (executed flag); end-to-end marker-file proof that a denied command
    never spawns; kill proof (`sleep 2 && touch marker` under 1s timeout
    leaves no marker); pipeline proves shell parsing (shell-agnostic —
    `$SHELL` here is **fish**, so tests avoid bash-isms); CRLF
    normalization; timeout cap/clamp; signal-death trailer. 149 tests
    green (25 new), timing-sensitive tests rerun-stable 5/5.
  Risk: Medium · Depends: 1.5

- [x] **1.8 `apply_patch` tool with `diffy`** (1h) — done 2026-10-09
  - `tools/patch.rs`: model emits unified diff → `diffy 0.5.2` parse →
    confined target (headers `+++`→`---` fallback, git `a/`/`b/` prefix
    strip, `/dev/null` = deletion → unsupported, 1.6 `resolve_within`
    guards escapes) → read whole file (UTF-8 required, no lossy) →
    `diffy::apply` → write only on success.
  - **diffy semantics (source-verified): positional offset search** around
    the declared hunk position (stale line numbers still land — the
    KQ2.4 painkiller, tested), but **no context-content fuzz** — a hunk
    whose context vanished fails cleanly (`ApplyError` = "error applying
    hunk #N").
  - **Fallback:** optional `new_content` arg — used **only** when the
    patch parses but fails to apply: whole-file write, reported loudly
    ("…whole file fallback"). Malformed patch → rejected **before any
    fs write**, `new_content` deliberately ignored (model bug ≠ apply
    failure).
  - Golden tests pin "never silent corruption": mismatch without
    fallback, malformed prose patch, garbage `@@` header → file
    **byte-identical**; path-escape header → `PathEscape` + outside file
    untouched; non-UTF-8 target untouched. 165 tests green (16 new).
  Risk: High · Depends: 1.6

- [x] **1.9 Permission gate + the loop** (1h) — done 2026-10-09
  - `crates/core/src/loop.rs` (exposed as `agent_loop` via `#[path]` —
    `loop` is a keyword; file keeps the plan's name): one explicit
    `loop`, never recursion; `max_turns` + `max_tokens_budget` +
    per-tool `allow`/`deny`/`ask` policy from config; TUI-free (the
    [`LoopEvent`] observer is the only seam — 1.10 stdout, 4.x TUI).
  - Config gains `max_turns` (default 50), `max_tokens_budget`
    (default 250 000, cumulative `total_tokens`) and the
    `[permissions]` overlay — **file-layer only** (env/flag layers
    carry scalars, not maps). `Decision` is now serde, so the config
    vocabulary *is* the gate vocabulary. `Config::permission_gate()`
    builds a `PolicyGate` (overlay ∪ built-in: `read_file`/`list_dir`
    allow, everything else **ask** — fails safe) for the 1.7 seam.
  - The loop resolves `ask` through `AskResolver` before dispatch
    (`DenyOnAsk` headless default, closures for tests, TUI prompt at
    4.5); the resolved decision becomes the gate the registry re-checks
    at dispatch — one enforcement point, nothing forgettable. Tool
    errors (unknown tool, invalid args, denial) become error
    `ToolResult`s the model reacts to; only provider errors abort.
  - Limits are checked **before** each provider call — a budget never
    causes an overshooting call; a finishing response completes even if
    it crossed the budget.
  - **Done when:** `httpmock` cassette runs a full 3-turn tool
    conversation (read → list → final text) to completion through the
    real streaming client; each turn-mock matched on a body marker that
    only exists once prior tool results were fed back (mocks prove
    transcript growth). httpmock serves the first registered match →
    layered mocks registered in reverse. Loop file **215 LoC ≤ 250**
    (loop tests live in `tests/agent_loop.rs`). 183 tests green
    (18 new), fmt + clippy clean, loop suite rerun-stable 3/3.
  Risk: High · Depends: 1.3, 1.6, 1.7, 1.8

- [x] **1.10 Headless `run -p` mode** (1h) — done 2026-10-10
  - `crates/cli/src/run.rs`: `iris run -p "prompt" --workdir <dir>
    [--model <slug>]` runs the 1.9 loop with `DenyOnAsk` and all five
    tools registered. Model text streams to **stdout** (flushed per
    delta, trailing newline ensured); tool events + the run summary go
    to **stderr** (`[iris] …` lines), so stdout stays pipeable.
  - **Exit code = agent outcome** (documented in `--help`): 0
    completed · 1 error (config/key/provider/IO) · 3 `max_turns` · 4
    token budget — 2 stays reserved for clap usage errors. Missing key
    and bad workdir fail **before** any provider traffic.
  - **E2E test harness** (`crates/cli/tests/common/mod.rs` +
    `tests/run.rs`): hermetic binary spawn (`IRIS_CONFIG` → nonexistent
    path, key stripped) + httpmock provider + SSE builders — the
    harness every later stage's E2E test builds on. 7 E2E tests: the
    Done-when cassette (tool conversation through the real binary,
    stdout/stderr separation, turn mocks matched on transcript growth),
    headless `write_file` denial (file never created, denial fed back,
    run still completes), missing key → exit 1 with **zero** provider
    hits, `max_turns`/budget → exit 3/4 with **exactly one** provider
    hit (limits checked before the call, through the binary), provider
    500 → exit 1, missing workdir → exit 1.
  - `--model` flag (default `openai/gpt-4o-mini`) — config still does
    not select models (1.9 note); the flag is the caller-chosen seam
    `LoopOptions::from_config` expects. 193 tests green (10 new: 7
    E2E + 3 `first_line` unit), fmt + clippy clean, E2E rerun-stable
    3/3, manual smoke (`run` without key → exit 1, `--help` documents
    the exit-code table).
  Risk: Low · Depends: 1.9

### Phase 2 — Session Durability (4 stages, ~4h) — wedge #2 foundation

- [x] **2.1 JSONL transcript schema v0** (1h) — done 2026-10-10
  - `crates/core/src/session/schema.rs`: versioned envelope `{"v":0,"type":...}`
    (`Envelope`/`Event`/`SchemaError`), one JSON object per line; events
    `session_start` / `message` / `usage` / `session_end`; RFC3339 timestamps
    via `time::serde::rfc3339` (new dep: `time` 0.3).
  - Reader **rejects** unknown `v`, missing `v`/`type`, and unknown `type`
    (`UnknownType`) — never skips a line (silent skips = resume amnesia);
    tolerates `\n`/`\r\n` from `BufRead`.
  - `docs/transcript-schema.md` written from day one (D5), enforced by a test
    that fails if the doc is missing or drops an event type.
  - **Done when:** tests above red → green; 210 total green, clippy/fmt clean.
  - Risk: Low · Depends: 1.1

- [x] **2.2 Crash-safe append writer** (1h) — done 2026-10-10
  - `crates/core/src/session/writer.rs`: `TranscriptWriter::create` (manifest
    via tmp + fsync + rename + dir fsync) / `append` (one `write_all` of
    `line + \n`, then `fsync`) / `load` recovery / `load_manifest`; `Manifest`
    mirrors `session_start` so `sessions` can list without parsing transcripts.
  - Session ids are path-confinement input: validated to `[A-Za-z0-9_-]+`
    before any `join` (traversal tests: `../evil`, `a/b`, `..`, empty…).
  - Recovery rule: newline-terminated lines must parse (else `CorruptLine`,
    fail loudly); an unterminated tail is kept **iff** it parses — the only
    parseable prefix of a single top-level JSON object is the whole line, so a
    torn write can never be accepted.
  - `proptest` crash injection: truncate at any byte → recovered == prefix of
    original, ≥ all terminated lines, ≤ +1 tail line, exact when uncut.
  - **Done when:** tests above red (10 failed) → green; 220 total green,
    clippy/fmt clean.
  - Risk: Medium · Depends: 2.1

- [x] **2.3 `sessions list` / `sessions resume`** (1h) — resume rehydrates full history and resumes
  the loop mid-conversation (targets KQ2.2 "resume = amnesia"). ✅ 2026-10-11
  **Done when:** E2E test — run, kill, resume, assert the model sees prior turns. ✅
  `run_kill_resume_rehydrates_prior_turns`: run → SIGKILL mid-`run_command` →
  `sessions resume` — the only answering mock demands the prior user turn, the
  prior assistant tool call *and* the dangling-call repair in the request body.
  Notes: `run` now persists every session (`session_start`/messages/usage/
  `session_end`, id printed as `[iris] session <id>`); loop gained
  `run_recorded` + `Recorded` (recorder errors abort the run —
  `RunError::Transcript`, no silent amnesia); resume repairs an unanswered
  `tool_use` at read time (synthetic error `tool_result`, never written back);
  sessions root: `IRIS_SESSIONS_DIR` → `XDG_DATA_HOME` → `HOME/.local/share`;
  resume defaults workdir/model from the manifest. 26 new tests
  (19 core + 7 E2E); 246 total green, clippy/fmt clean.
  Risk: Medium · Depends: 2.2, 1.10

- [x] **2.4 rusqlite session index** (1h) — fast `sessions list` (cwd, turns, tokens, cost,
  last-active); rebuildable from JSONL (index = cache, JSONL = source of truth).
  ✅ 2026-10-12
  Done when: `sessions list` reports turns/tokens/last-active from the index, and
  deleting `index.db` + re-listing reproduces identical output. ✅
  `sessions_list_reports_turns_tokens_and_last_active` (8 columns, cost `-`
  until 3.2) and `sessions_list_rebuilds_a_deleted_index` (byte-identical
  rebuild) / `sessions_list_rebuild_flag_forces_a_refresh`.
  Notes: `iris_core::session::index` — rusqlite (`bundled`, no system dep),
  `<store>/index.db`, one `sessions` table (`PRAGMA user_version = 1`); rows
  keyed by transcript stem, aggregates summed from `usage` events (turns =
  provider calls, lifetime across resumed runs), `last_active` = transcript
  mtime (doubles as the cache key), `cost_usd` NULL until 3.2 fills it.
  `refresh` re-parses changed mtimes, prunes vanished transcripts, `force`
  re-parses all (`sessions list --rebuild`); a transcript that will not parse
  falls back to its manifest (listed with zeroed aggregates — never hidden);
  rows ordered last-active desc. CLI degrades to manifest rows with a warning
  if SQLite itself is unavailable (cache failure ≠ list failure). 11 new tests
  (8 core + 3 E2E); 257 total green, clippy/fmt clean.
  Risk: Low · Depends: 2.3

### Phase 3 — Cache-First Sessions (4 stages, ~4h) — **Wedge #1**

- [x] **3.1 Prefix-invariant prompt assembly** (1h) — `crates/core/src/assembly.rs`: stable ordering
  (system blocks, tool schema sorted by name, history), **stable serialization** (no `HashMap`
  iteration in output), no volatile content (timestamps, absolute paths, random ids) in the prefix;
  tools/system changes detected and reported as a *cache-bust event*. ✅ 2026-10-13
  **Done when:** same session + same config ⇒ byte-identical request prefix (golden test, run twice
  with randomized `HashMap` seeds). ✅ `prefix_bytes_are_byte_identical_across_randomized_hashmap_seeds`
  (golden insta snapshot; tool params built by iterating fresh `HashMap`s — seeds erase to identical
  bytes) + `identical_config_runs_send_byte_identical_prefixes_with_sorted_tools` (two real loop runs,
  only registration order differs → name-sorted tools, byte-identical prefixes).
  Notes: `assembly::{SystemBlocks, Assembler, CacheBust, canonical_tools, prefix_bytes}`.
  SystemBlocks = name-sorted BTreeMap of named blocks (CLI prompt travels as the
  "core" block — call order can never reorder the prefix); Assembler canonicalizes
  each turn's request (tools name-sorted; registration order invisible) and
  compares system-text + canonical tool JSON against the previous turn — a change
  is `CacheBust { system_changed, tools_changed }`, surfaced as
  `LoopEvent::CacheBust` (re-anchored per assembly: one change = one event;
  history growth is never a bust). `prefix_bytes` = wire JSON — struct order over
  serde_json's BTreeMap-backed maps (no `preserve_order` anywhere in the tree,
  verified via `cargo tree`), no streaming flags. Headless prints cache-busts to
  stderr. No volatile content added by assembly (E2E guard: request body must not
  contain the workspace path or today's date). 10 new tests (8 core + 1 loop +
  1 E2E guard); 267 total green, clippy/fmt clean. Known gap: the loop's
  cache-bust *emission* branch is unreachable while registries are static (tools
  can't change mid-run) — the detection logic is unit-tested on the Assembler;
  MCP-era dynamic tools (stage 6) will exercise the emission path.
  Risk: High · Depends: 1.9

- [x] **3.2 Token accounting** (1h) — `tiktoken-rs` for OpenAI-accurate counts + parse OpenRouter's
  response `usage` (prompt/completion tokens, **cached-token and cost fields** — confirm exact
  field names against openrouter.ai/docs during this stage; caching depends on provider support
  per OpenRouter's sticky-routing prompt-caching guide);
  reconcile estimated vs actual, record delta. Risk: Medium · Depends: 1.3, 3.1
  — **Done (2026-10-13):** OpenRouter field names confirmed against openrouter.ai/docs:
  `usage.cost` (USD, float) and `usage.prompt_tokens_details.cached_tokens` (cache reads; absent
  for providers that don't report caching). New `iris_core::tokens` counts with `tiktoken-rs`
  cl100k_base over `assembly::prefix_bytes` (3.1's canonical prefix) and reconciles
  actual − estimate; the loop records the signed delta as the optional `input_token_delta` on
  usage events and `Usage` gains optional `cached_input_tokens` / `cost_usd` (additive within
  schema v0 — old lines still decode, absent fields keep the exact pre-3.2 shape, so
  `docs/transcript-schema.md` updated per D5). The loop's lifetime totals now accumulate via
  `Usage::plus`; the stage-2.4 index fills its reserved `cost_usd` column from the per-turn
  usage events, so `sessions list` prints real cost (4 dp) instead of `-`. TDD: RED = 9 failures
  (4 tokens stubs, wire/stream mapping, loop delta, index cost, CLI E2E) + 3 shape guards;
  279 total green, clippy/fmt clean. Note: `Usage`/`LoopOutcome` dropped `Eq` (cost is f64).

- [x] **3.3 Metrics collector** (1h) — `crates/core/src/metrics.rs`: per-turn `ttft_ms`,
  `tokens_per_sec`, `cache_hit_pct`, `cost_usd` from a pluggable price table (config, not
  hardcoded); immutable `TurnMetrics` events appended to transcript. Risk: Low · Depends: 3.2
  **Done (2026-10-14):** `TurnMetrics::compute` + file-layer `[pricing."<model>"]` map
  (`ModelPrice`, per-million USD, defaults empty — no hardcoded prices); cost precedence =
  provider report (3.2) → price-table estimate → `None`; loop times request → first token →
  stream end and emits `Recorded::Metrics` + `LoopEvent::Metrics` after each usage record;
  new `turn_metrics` event type (flat under the tag, schema v0 additive — KNOWN_TYPES + doc
  test + realistic-transcript snapshot updated per D5). `Config` dropped `Eq` (f64 rates).
  289 total green, clippy/fmt clean.

- [x] **3.4 Live gauge rendering (headless first)** (1h) — status line in `run -p` and interactive
  console mode; validate numbers **before** any TUI work.
  **Done when:** against the `httpmock` fixture with cache usage, gauge prints expected hit % and
  cost to 4 dp. Risk: Low · Depends: 3.3
  **Done (2026-10-14):** `crates/cli/src/gauge.rs::format_turn` — one `[iris] turn N · ttft … ·
  … tok/s · cache …% · cost $…` line per `LoopEvent::Metrics` on stderr (stdout stays the
  model's, pipeable), wired in `run::execute` so `run -p` **and** `sessions resume` both print
  it; unreported fields render `–` (never fake zeroes). Interactive chat (4.x) and the TUI status
  bar (4.4) consume the same `TurnMetrics`. 293 total green, clippy/fmt clean.
  **Phase 3 closed.**

### Phase 4 — TUI (6 stages, ~7h)

- [x] **4.1 Extract `crates/tui` + app skeleton** (1h) — ratatui + crossterm event loop; `App`
  state machine (`Idle | Streaming | AwaitingPermission`); channels from core loop → UI;
  **no blocking in the render path**. Risk: Medium · Depends: 1.10
  **Done (2026-10-15):** new `crates/tui` (`iris-tui`): pure `App` state machine (owned
  `AppEvent` mirrors `LoopEvent` + permission/run lifecycle; keys: Enter submits, Esc clears/
  quits, Ctrl-C quits); `ui::render(&App)` proven on `TestBackend` (header / transcript / status
  / composer panes); `term::run` shell — raw-mode + restore guard, per-run tokio task feeding a
  std mpsc channel, frame cycle = `try_recv` drain → draw → timed `poll` (nothing blocks the
  render path); recorder mapping is a total `transcript_event` fn. `iris chat` wired through CLI
  plumbing (same fail-fast + durable session as `run -p`; `--model` flag added). E2E: no-key
  fails fast pre-terminal; no-tty path proven via `setsid -w` (never hijacks/hangs the dev's
  terminal). Known 4.x gaps documented in `term.rs`: DenyOnAsk until 4.5, mid-run quit =
  interrupted (resumable) run, dangling `tool_use` after max_turns in-memory. 311 total green,
  clippy/fmt clean.

- [x] **4.2 Composer** (1h) — `ratatui-textarea`: multiline input, history recall, `/commands`,
  paste, scrollback of transcript. Risk: Low · Depends: 4.1 — **Done 2026-10-15.** RED = 7 failures
  (5 app, 2 ui) → GREEN. Dependency note: no `ratatui-textarea` release supports ratatui 0.29 (the
  0.8+/0.9 line targets the ratatui 0.30 split family; 0.4.1 is deprecated), so the plan's named
  crate was honored by bumping `ratatui` 0.29 → 0.30.2 — migration compiled clean and all 4.1 tests
  stayed green. Composer is a `TextArea` (Enter sends, Shift+Enter/Ctrl-J newline, textarea editing
  keys); Up/Down recall history only at the composer's edges with draft restore; `/help` `/quit`
  `/clear` dispatch locally (never reach the loop; `COMMANDS` table drives both `/help` and the
  hints); bracketed paste (`EnableBracketedPaste` + `Event::Paste` → `handle_paste`, dropped
  mid-run like keys); transcript scrollback `PgUp`/`PgDn` (`PAGE_LINES` = 10, works mid-run, fresh
  output re-anchors to the tail). Composer pane grew to 6 rows so multiline drafts are visible.
  7 new tests → 318/318 green, clippy/fmt clean.

- [x] **4.3 Streaming markdown pane** (1h) — `tui-markdown` incremental render, code blocks with
  language tag, auto-scroll that doesn't fight the user (pause on manual scroll).
  Risk: Medium (re-render cost/flicker — KQ2.5) · Depends: 4.2 — **Done 2026-10-17.** RED = 3
  failures (1 app, 2 ui) → GREEN. `tui-markdown` 0.3.11 targets `ratatui-core ^0.1` (same split
  family as 4.2's ratatui 0.30 bump — types unify, single `ratatui-core` in tree). Built with
  `default-features = false`: the `highlight-code` (syntect) default pulls a heavy syntax set and
  swaps the deterministic code styles — the plan's requirement is the fence's language tag
  (```` ```lang ````), which the plain path renders. Markdown contract: only assistant text enters
  the transcript (`Delta` never carries tool output), so the whole buffer parses; prose soft
  breaks reflow to spaces — the scrollback fixture moved into a fenced block (lines preserved
  verbatim), pane wrap is `trim: false` so code indentation survives. Auto-scroll: fresh output no
  longer re-anchors a paged-up reader (offset untouched; following resumes at the tail); parked
  view still slides as output accumulates — true pinning needs the render-pass height, documented
  as a seam. Scroll math counts *rendered* lines (markdown reflow changes them). Cost: per-frame
  re-parse (pulldown-cmark is linear; transcripts are human-scale; caching seam documented for
  KQ2.5). 6 new tests (1 app, 2 ui, 3 markdown module) → 324/324 green, clippy/fmt clean.

- [x] **4.4 Status bar with wedge #1 gauges** (1h) — cache-hit %, cost, TTFT, tok/s, model, turn #,
  token budget bar; turns red on budget/cost threshold. Risk: Low · Depends: 3.4, 4.1 —
  **Done 2026-10-18.** RED = 3 failures (1 app, 2 ui) → GREEN. `AppEvent::RunEnded` now carries the
  run's cumulative `Usage` (total match over all three `LoopOutcome` variants) — the budget gauge's
  source. `App::with_run_config(model, max_turns, max_tokens_budget)` injects the run identity from
  `LoopOptions` at the shell (CLI side of the boundary, matching the layering); session cost
  accumulates *reported* turn costs (unreported adds nothing — no fabricated zero). The status
  block is two rows: `[State] status` on row A, the gauge bar on row B — `model · turn n/max ·
  tok used/budget ▓▓░░░░ · $cost · TTFT · tok/s · cache %`, with the budget segment (digits +
  six-cell bar) red when the last run met/passed its token budget. Honest notes: no cost-cap
  config knob exists (3.x shipped a token budget only), so cost displays but is not a red
  trigger; on frames narrower than ~75 cols the trailing cache % truncates gracefully (asserted
  through app state). 3 new tests → 327/327 green, clippy/fmt clean.

- [x] **4.5 Permission prompt widget** (1h) — command/diff preview +
  `y / n / always-allow (session) / always-allow (rule)`; persisted rules write back to config
  (targets claude-code #11380 "always allow ignored"). Risk: Low · Depends: 4.3, 1.9
  **Design recon done 2026-10-18 (no code yet):** blocking `PromptingResolver` in new
  `crates/tui/src/ask.rs` implementing `AskResolver` — checks an in-memory session-rules map
  first (no prompt), else sends `AppEvent::PermissionRequested{id,name,detail}` and blocks on an
  answer channel; disconnect → `Deny` (fail closed). `AppEvent::PermissionAnswered` gains
  `note: Option<String>` (one test construction site to update; note lands in status — "rule
  saved" / "degraded to session"). App gains `PermissionAnswer{Allow,Deny,AllowSession,AllowRule}`
  + `answer`/`answer_in_flight` fields + `take_answer()`; prompt keys `y/n/a/r` are modal while
  pending (ctrl-c still quits first); `spin()` forwards the answer to the resolver, which applies
  the session rule, persists `AllowRule` via new `iris_core::config::persist_permission(path,
  tool, decision)` (toml read-modify-write, `create_dir_all`, tmp+rename — `tempfile` already a
  core dev-dep), then emits `PermissionAnswered`. `Chat` carries `config_path: Option<PathBuf>`
  from the CLI. Popup widget: centered rect, preview = `run_command` → `command` field,
  `write_file`/`apply_patch` → path + body, else pretty JSON; key-hint line y/n/a/r. RED ≈ 12
  (2 app, 2 ui, 5 ask, 3 config).
  **Done 2026-10-19.** RED = 11 failures (2 app, 1 ui, 5 ask, 3 config) → GREEN. New
  `crates/tui/src/ask.rs`: blocking `PromptingResolver` (session rules first, else post
  `PermissionRequested` + block on a std channel; every failure path → `Deny`; `AllowSession`/
  `AllowRule` insert the in-session rule, `AllowRule` also calls core's new
  `config::persist_permission` — TOML read-modify-write, `create_dir_all`, tmp+rename).
  `PermissionAnswered` carries `note: Option<String>` (status line: "rule saved …" or the
  degraded-to-session note); `Chat` gained `config_path` (CLI resolves via `config_file_path`);
  the prompt is modal in `handle_key` (`y/n/a/r`, double-press suppressed by
  `answer_in_flight`, Ctrl-C still quits; `RunEnded`/`Error` clear prompt + stale answer);
  centered popup previews `command` / path+content / patch / pretty JSON with the key hints,
  clipped so hints always fit; status row A keeps the one-line ask. Honest caveat: the TOML
  round-trip **drops comments** from the rewritten config (documented in `persist_permission`;
  a comment-preserving write is a follow-up). 11 new tests → 338/338 green, clippy/fmt clean.
  Phase 4: 4.1–4.5 done — only 4.6 (diff view) remains.

- [x] **4.6 Diff view** (1–2h) — done 2026-10-19
  `similar` hunk rendering of pending file edits, accept/reject per
  hunk feeding back to the loop; `insta` TUI snapshots via `TestBackend`.
  Risk: Medium · Depends: 4.3, 1.8
  - **Done 2026-10-19.** RED = 11 failures (2 core-loop, 3 diff, 2 app,
    2 ask, 2 ui) → GREEN. Core: `AskResolver::confirm` returns
    `Resolution{Allow, AllowWith(String), Deny}` — `AllowWith` carries
    the **replaced input JSON** the loop executes (unparsable
    replacement fails closed and the original never runs; closures
    still resolve via `Decision`). `resolve_within` made public so the
    prompt's preview read is confined too. `crates/tui/src/diff.rs`:
    `EditDiff`/`Hunk` — `write_file` hunks via `similar`
    `grouped_ops(3)`, `apply_patch` hunks = `@@` sections byte-ranged
    in the patch text; `replacement_input()` replays accepted hunks
    over the old lines (write) or drops rejected sections *and* the
    `new_content` fallback (patch — it would undo the rejection).
    `PermissionRequested` carries `old: Option<String>`: the resolver
    loads it for `write_file` through a confined closure
    (`resolve_within` + read, wired in `term::run`). App: `pending_diff`
    + clamped `j/k` cursor + `space` toggle; `y` = plain Allow when all
    accepted, else `PermissionAnswer::AllowWith{input, note}`
    ("applied N/M hunks — partial, not remembered"; a partial choice is
    never remembered, by design). Popup: taller rect in diff mode,
    `✓/✗` marks, green/red lines, yellow cursor header, ↑/↓ scroll
    markers, nav-hint row; title shows target path + counts. `insta`
    snapshot `permission_diff_popup` pins the frame. 12 new tests →
    350/350 green, clippy/fmt clean. **Phase 4 complete (4.1–4.6).**

### Phase 5 — Privacy (3 stages, ~3h) — **Wedge #2**

- [x] **5.1 Secret redactor at write time** (1h) — `crates/core/src/redact.rs`: pattern set
  (AWS/GitHub/Anthropic/OpenAI keys, `PASSWORD=`, private-key blocks, `.env` assignments) +
  entropy heuristic; applied to transcript **and** tool output *before* persistence
  (claude-code #44868 pain). Risk: **High** (false negatives leak; false positives corrupt) ·
  Depends: 2.1
  Done 2026-10-19 — four ordered passes over free text: PEM blocks → fixed-shape keys
  (`KEY_RULES`: AWS AKIA/ASIA, GitHub ghp_/gho_/…/github_pat_, `sk-ant-`/`sk-or-v1-`/
  generic `sk-`, Google `AIza`, Slack `xox*`, longest-prefix-first so kind labels stay
  precise) → `NAME=value`/`NAME: "value"` assignments (segment-matched secret names with
  a documented `public` guard, so `public_key` survives) → entropy heuristic (Shannon
  ≥ 4.5, requires upper+lower+digits; pure-hex SHAs/UUIDs and snake_case can never
  fire). `redact_json` uses **key context**: values under secret-ish keys and array
  elements after `--password`-style flags redact by position. `redact_event` walks
  `Event::Message` (text, tool-call inputs, tool results); `ContentBlock::Image`
  passes through (documented — base64 pixels are entropy by construction). Wired into
  `TranscriptWriter::append` — the single fsync chokepoint — so tool output and the
  conversation are scrubbed *before* bytes hit disk; in-memory loop + provider wire
  keep the original content. Markers keep the kind: `[REDACTED:github-token]`; redact
  is idempotent; clean text stays zero-copy `Cow::Borrowed`. 16 new tests → 366/366
  green, clippy/fmt clean, secret-scan clean.

- [x] **5.2 Redaction test corpus** (1h) — golden fixtures: real-shaped secrets in tool output,
  patch hunks, error messages; `proptest` that redacted output never matches raw pattern; assert
  redaction is applied *before* fsync. Risk: Medium · Depends: 5.1
  Done 2026-10-19 — six golden fixtures in `crates/core/tests/corpus/` (env dump, curl
  headers with `Authorization: Bearer`, `.env` patch hunk, git remote-URL error, SSH-key
  paste, clean-prose negative control) pinned by `insta` snapshots — the corpus caught
  one real over-redaction during review ("a credential: the" in prose → assignment rule;
  fixture uses an em-dash, the rule is kept — it's what catches headers/JSON). Two
  proptests: planted real-shaped secrets (per-family body lengths ≥ each rule's
  documented minimum) never survive any surrounding text; arbitrary-Unicode input never
  panics and redaction is idempotent (resume-safe). "Before fsync" pinned directly:
  `TranscriptWriter::append` now seals through a testable `sealed_line(event)` helper,
  unit-asserted to return a scrubbed line while the caller's in-memory event keeps the
  original — so the bytes `write_all`/`sync_all` receive are redacted by construction.
  4 new tests → 370/370 green, clippy/fmt clean, secret-scan clean.

- [x] **5.3 Telemetry-off guarantee** (0.5h) — no outbound host except configured LLM providers;
  test asserts zero extra sockets; `doctor` shows exactly what leaves the machine.
  Risk: Low · Depends: 1.10
  Done 2026-10-19 — two pins in `crates/core/tests/telemetry_off.rs`. Runtime: a full
  scripted run (streaming + real `read_file`) against httpmock — exactly the two
  scripted calls hit the configured host, and every socket the run opened drains back
  to the harness's own listener footprint within 3s (measured per-fd from
  `/proc/self/fd`; httpmock pools its servers by design, so the harness baseline is
  subtracted — the agent adds nothing that lingers). Dependency graph: the workspace
  may not grow a second HTTP client (hyper/ureq/curl/… — reqwest, core-only, is the
  only network stack) or a telemetry SDK (opentelemetry/sentry/…), asserted by
  parsing every crate's Cargo.toml. `doctor` gains `egress:` (names the resolved
  provider host — the only host iris contacts) and `telemetry: none` lines.
  Validated live against the real OpenRouter API with a free model
  (nvidia/nemotron-3.5-lightning:free): real streaming, real tool call, $0.00 cost —
  and the transcript showed `AWS_ACCESS_KEY_ID=[REDACTED:aws-access-key]` in both the
  user prompt and the tool-call input while the on-disk file kept the raw value the
  user asked for. 3 new tests → 373/373 green, clippy/fmt clean, secret-scan clean.
  **Phase 5 complete (5.1–5.3).**

### Phase 6 — Parity Features (5 stages, ~5h)

- [x] **6.1 Git-first checkpoints** (1h) — `checkpoint create/list/restore`: snapshot of
  tracked+untracked state to `.iris/checkpoints` (or orphan commits); shell side-effects
  explicitly out of scope, documented. Risk: Medium · Depends: 1.9
  **Done (2026-10-20):** orphan commits on `refs/iris/checkpoints/<id>` via pure plumbing
  (throwaway `GIT_INDEX_FILE` → `add -A` → `write-tree` → `commit-tree` → `update-ref`
  with expected-old-zero create): the user's branch, index, and HEAD never move, and
  `.gitignore`d files are never snapshotted — they survive `restore` untouched too
  (privacy-consistent with 5.1). `restore` force-checks the tree out over the worktree
  and deletes present-but-absent files (with empty-dir pruning) for exact-state rewind;
  shell side-effects documented out of scope. `iris checkpoint create|list|restore` CLI
  (unknown ids / non-repo workdirs fail with actionable messages). 7 new tests →
  **380/380 green**, clippy/fmt/secret-scan clean.

- [x] **6.2 Compaction that fires** (1h) — threshold on *estimated* tokens (pre-request, not after
  failure): summarize older turns into a pinned block, keep last N raw; must preserve
  prefix-invariance rules; test asserts it triggers at 70% and survives resume.
  Risk: **High** (the #1 documented pain) · Depends: 3.2, 3.1
  **Done (2026-10-20):** the loop assembles and estimates **before** every request and folds
  at ≥70% of `context_window` (exact integer boundary: `due()` fires at 700/1000 per 1000,
  refuses 699; `window = 0` = off). Summarizer = one non-streaming, no-tools,
  temperature-0 call through the run's own provider — no signature churn — with its tokens
  charged to the run budget; the summary is pinned as a message through the one shared
  builder. `plan()` never splits a `tool_use`/`tool_result` pair (boundary walks back, one
  fewer fold when it must), never touches system messages (no cache bust — asserted), keeps
  the last `keep` raw. New `compact` transcript event (schema v0 + KNOWN_TYPES + D5 doc);
  resume re-applies the *same* plan/builder — the resumed context is byte-identical to the
  request the provider actually saw (asserted end-to-end). Summary passes the 5.1
  redaction chokepoint (asserted). Config: `context_window` (default 131_072, 0 disables)
  + `compact_keep` (default 8), layered from TOML and `IRIS_CONTEXT_WINDOW` /
  `IRIS_COMPACT_KEEP`. 7 new tests → **387/387 green**, clippy/fmt/secret-scan clean.

- [x] **6.3 MCP via `rmcp`** (1h) — stdio client, tools surfaced through the same registry +
  permission gate; per-server tool budget so MCP can't silently eat context.
  Risk: Medium · Depends: 1.5, 1.9
  **Done:** `iris_core::mcp` on `rmcp` 3.6 pinned `default-features = false` +
  `client`/`transport-child-process` only — stdio child-process transport, no HTTP/SSE,
  no second network stack (new `telemetry_off` pin asserts the feature set). New
  `ToolRegistry::register_dynamic` seam: specs arrive at runtime from `tools/list`
  (typed boundary reduced to "JSON object or null"), and the gate consults
  `spec.name` — so `[permissions] "mcp_<server>_<tool>" = "allow"` works unchanged and
  unlisted MCP tools are **ask-by-default** (fresh install fails safe; builtin policy
  never matches an MCP name). Servers are configured as `[mcp.servers.<name>]` =
  `command`/`args`/`env` (file-layer, like `permissions`; `command` required), with
  per-server budgets: `max_tools` (default 32, alphabetically-first so the set is
  stable across restarts, overflow warns) and `max_result_bytes` (default 32 KiB, with
  an explicit truncation marker). Spawn/initialize/list failures produce a warning and
  skip that server — a broken MCP server degrades the run, never blocks it. Wired into
  `run` + `sessions resume` (same runtime, cancelled before transcript close) and
  `chat` (dedicated runtime across the TUI's lifetime). Fixture bin `iris-fake-mcp`
  (`required-features = ["test-fixtures"]`) speaks NDJSON-RPC over stdio for the new
  e2e suite. 16 new tests → **403/403 green**, clippy/fmt/secret-scan clean.
  **Live-validated** against the real world: the official
  `@modelcontextprotocol/server-filesystem` over npx + `nvidia/nemotron-3.5-lightning:free`
  — the model called `mcp_demo_read_file` through the gate (allowed by config rule),
  the fixture file's exact contents came back, 2 turns / 7,408 tokens / $0.00, key
  never touched disk or transcript.

- [x] **6.4 Hooks/events** (1h) — `on_tool_pre`, `on_tool_post`, `on_turn_end`, `on_compact`:
  shell commands with timeout + JSON payload. Risk: Low · Depends: 1.9
  **Done:** `[hooks]` file-layer table — each event takes a command string *or* list
  (`string_or_vec`); unknown event names are a **config error** (a typo can't silently
  do nothing); `timeout_secs` defaults to 15 (`0` clamps to 1s — no "wait forever").
  `iris_core::hooks::fire` stamps `event` into the payload, redacts the whole object at
  the stage-5.1 chokepoint, delivers it **newline-terminated on stdin** (cwd = workspace
  root; stdout/stderr discarded), and enforces one shared deadline across delivery +
  wait — the write end is *closed* after the payload (EOF, not flush), so reading hooks
  finish immediately; hangs are SIGKILLed. Failures are **advisory**: non-zero exit,
  timeout, or spawn error → `LoopEvent::HookWarning` (core still never prints) →
  headless `[iris] hook: …` on stderr, TUI status line. No deny-via-hook — the gate
  stays the one enforcement point (1.7); hooks observe. Payload shapes: `tool_pre` =
  the request as asked (pre-gate), `tool_post` = the outcome (`is_error` covers
  denials) with a 4 KiB content preview, `turn_end` = stop_reason + usage + text
  preview every turn (final included), `compact` = fold + pinned summary for the turn
  about to start. Wired at the loop's four sites; `LoopOptions.hooks` flows from
  `from_config`, so `run`/`resume`/`chat` all share it. 10 new tests → **413/413
  green**, clippy/fmt/secret-scan clean.

- [x] **6.5 Custom commands/skills** (1h) — markdown files in `.iris/commands/*.md` with
  frontmatter args → prompt templates; `/help` lists them. Risk: Low · Depends: 4.2
  **Done (2026-10-20):** `iris_core::commands` — best-effort loader (missing dir →
  empty map, each broken file → warning, rest still load — 6.3's MCP posture) over a
  strict small dialect: optional `---` frontmatter with only `description` and
  `args: [a, b]` (inline identifier list; unknown keys, unterminated frontmatter,
  bad/duplicate args, empty body fail *that one file* loudly). File stem = command
  name; builtin names (`/help` `/quit` `/clear`) are reserved — shadowing files are
  skipped with a warning. `expand` binds argv positionally with **exact arity**
  (mismatch → status-line `usage: /name <arg>…`, nothing sent) and replaces `{{arg}}`
  (undeclared `{{x}}` stays literal). The shell loads the map before the screen
  switch (warnings eprintln'd like MCP's) and the App stores it via
  `with_commands`; `/name args…` expands to a prompt that goes out like any other
  input (history keeps what was typed; the transcript records the expansion), and
  `/help` lists project commands after the builtins. Headless run stays out of scope
  (composer-only, per plan). Also fixed a latent ~5% flake in 5.2's redaction
  property: the generator planted `github_pat_` bodies from a charset wider than the
  rule's documented `[A-Za-z0-9_]` (hyphens cannot occur in real PATs) — aligned the
  generator to the contract and pinned the near-miss boundary as a unit test. 12 new
  tests → **426/426 green**, clippy/fmt/secret-scan clean.

### Phase 7 — Quality Gate (4 stages, ~4h)

- [x] **7.1 HTTP cassette layer** (1h) — record/replay on `httpmock` covering streaming + tool loops
  for the OpenRouter (OpenAI-compatible) client; no live API needed in CI. Risk: Low · Depends: 1.3
  **Done (2026-10-25):** `OpenAiClient::with_recorder` — a record seam that captures
  request body + status + content-type + **raw response bytes** (a tee under the SSE
  parser), **never headers**, so an API key can't reach a committed file; a streamed
  exchange fires exactly once (terminal `Done` seen or stream drained) — a cancelled
  run records nothing. Test-side layer (`crates/core/tests/cassettes.rs`): JSON
  cassettes `{name, model, exchanges}` under `tests/cassettes/`, replayed on httpmock
  with **exact request-body matching** — any divergence 404s the run, so a wrong turn
  can never be served. One shared `run_scenario` drives record *and* replay
  byte-identically (same system/user text, fixture workspace, config, registry; model
  slug read from the cassette); record asserts no workspace path or key ever enters a
  request body. Two `#[ignore]`d record tests ran **live** against OpenRouter (env-only
  key, `nvidia/nemotron-3.5-lightning:free`) → committed cassettes: `streaming.json`
  (1 exchange — real SSE with id field, deltas, usage, `[DONE]`) and `tool_loop.json`
  (2 exchanges — the model really called `read_file` and fed the content back; both
  asserts enforced at record time). Replay tests pin the real model's verbatim answer,
  per-turn exact serving, delta-streaming, and turn counts; key-free by design (no
  network in CI). 7 new tests → **431/431 green**, clippy/fmt/secret-scan clean.

- [x] **7.2 Coverage to ≥80% on `iris-core`** (1h) — `cargo llvm-cov`; fill gaps (permission policy
  matrix, assembly edge cases, redaction). Risk: Low · Depends: all
  Done 2026-10-25: baseline was already 93.55% lines / 92.46% regions — the plan's named gaps
  (permissions 99%, assembly 99.7%, redaction 98%) were pinned by earlier stages, so the
  **measured** gaps were filled instead: `iris-core` **95.22% lines / 94.10% regions**
  (workspace 93.41%), gate ≥80% ✓. 23 new tests → **454/454 green**, clippy/fmt/secret-scan
  clean. Fills: `mcp.rs` 0% → hermetic unit tests (`tool_name` sanitization, `render_content`,
  `truncate_to` char boundaries) + new `tests/mcp.rs` connect-failure suite (unspawnable server,
  dies-before-handshake, per-server warnings; success path stays covered by the CLI fixture
  server); `loop.rs` → recorder-fails-on-usage abort, `into_provider`/`Outcome` mapping unit
  tests; hooks → failing `on_tool_post`/`on_turn_end`/`on_compact` warnings, missing-binary
  (exit 127) warning, 4096-char preview cap; `commands.rs` → full parse/discovery error matrix
  (unterminated, invalid line, duplicate keys, bad/duplicate/empty args, empty body, unreadable
  dir, non-UTF8 name, directory-as-file); compaction → `render()` every role/block, total
  `apply()` edge, empty-summarizer refusal. Remainder is unreachable by construction (loop
  cache-bust arm: registry is `&`-immutable and stream reassembly forces Assistant role —
  mechanism pinned by assembly tests; hooks spawn/IO arms need EPIPE races; `mcp` connect-success
  needs a live server, covered cross-package).

- [ ] **7.3 E2E: real repo, temp git worktree** (1h) — scripted scenario: prompt → read → patch →
  checkpoint → resume → rewind; run headless in CI. Risk: Medium · Depends: 6.1, 6.2

- [ ] **7.4 Security + release hardening** (1h) — `ecc:security-audit`, dependency audit
  (`cargo vet`/`cargo deny`), strip + `opt-level` release, single-binary smoke test, `README` with
  3-step onboarding. Risk: Low · Depends: all

## Deferred (post-v1 backlog)

Worktree replay A/B (wedge #3) · supervised GGUF mode · Windows support · ACP/app-server mode ·
in-TUI hunk annotation loop (wedge #4) · cross-agent transcript importers ·
native Anthropic `/v1/messages` client (stage 1.4, when a direct `ANTHROPIC_API_KEY` exists).

## Dependencies & risks (summary)

- **Blocker:** Rust toolchain install (stage 0.1) — plus slow-network caveat above.
- **API keys:** `OPENROUTER_API_KEY` (OpenRouter-only for now, D4/D6) for opt-in live smoke tests
  (httpmock cassettes cover CI). No direct OpenAI/Anthropic keys.
- **HIGH risks:** `apply_patch` correctness (golden corpus + `diffy`, temp+rename, never partial
  writes) · prefix-invariance (byte-identical golden test under randomized seeds) · compaction
  (pre-emptive threshold + pinned prefix + resume test) · redaction (written corpus + proptest +
  redact-before-fsync).
- **MEDIUM:** SSE tool-delta assembly · TUI flicker (event-driven render, `TestBackend` snapshots,
  core TUI-free) · scope creep (deferred backlog; each phase ends runnable).
- **Estimated:** ~46 one-hour stages ≈ 46 focused hours (~1.5 weeks full-time).

- Repo layout (2026-10-09): the project was **renamed ferro → IRIS** and
  lives in the IRIS monorepo at `agent/` (github.com/Siddhesh-ai-del/IRIS).
  Stage commits are pushed to IRIS `origin/main` after each sign-off (D2).
  `/home/siddhesh/Desktop/CLI/agent` is a stale pre-monorepo snapshot —
  do not edit there.
