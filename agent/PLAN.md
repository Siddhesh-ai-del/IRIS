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

- **Next action:** Stage 1.9 (Permission gate + the loop; 1.4 deferred under D4)
- **Current phase:** 1 — Headless Core Loop
- **Completed stages:** 0.1 ✅, 0.2 ✅, 1.1 ✅, 1.2 ✅, 1.3 ✅ (2026-10-07/08), 1.5 ✅ (2026-10-09), 1.6 ✅ (2026-10-09), 1.7 ✅ (2026-10-09), 1.8 ✅ (2026-10-09; 1.4 skipped)

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

- [ ] **1.9 Permission gate + the loop** (1h) — `crates/core/src/loop.rs`: explicit `loop`
  (never recursion), `max_turns`, `max_tokens_budget`, per-tool policy (`allow`/`deny`/`ask`)
  from config; **loop core must be TUI-free** (runs headless).
  **Done when:** `httpmock` cassette test runs a full 3-turn tool-using conversation to completion;
  loop file ≤250 LoC. Risk: **High** (core of the product) · Depends: 1.3, 1.6, 1.7, 1.8

- [ ] **1.10 Headless `run -p` mode** (1h) — `iris run -p "prompt" --workdir <dir>`: streams text
  to stdout, tool events to stderr, exit code = agent outcome. CI parity item + **test harness for
  everything after this**. Risk: Low · Depends: 1.9

### Phase 2 — Session Durability (4 stages, ~4h) — wedge #2 foundation

- [ ] **2.1 JSONL transcript schema v0** (1h) — `crates/core/src/session/schema.rs`: versioned
  envelope (`{"v":0,"type":...}`), one JSON object per line; write `docs/transcript-schema.md`
  from day one (D5). Risk: Low · Depends: 1.1

- [ ] **2.2 Crash-safe append writer** (1h) — append + `fsync`, session manifest via
  temp-file+rename; `proptest` crash injection (truncate at any byte → `sessions load` still
  recovers all complete lines). Risk: Medium · Depends: 2.1

- [ ] **2.3 `sessions list` / `sessions resume`** (1h) — resume rehydrates full history and resumes
  the loop mid-conversation (targets KQ2.2 "resume = amnesia").
  **Done when:** E2E test — run, kill, resume, assert the model sees prior turns.
  Risk: Medium · Depends: 2.2, 1.10

- [ ] **2.4 rusqlite session index** (1h) — fast `sessions list` (cwd, turns, tokens, cost,
  last-active); rebuildable from JSONL (index = cache, JSONL = source of truth).
  Risk: Low · Depends: 2.3

### Phase 3 — Cache-First Sessions (4 stages, ~4h) — **Wedge #1**

- [ ] **3.1 Prefix-invariant prompt assembly** (1h) — `crates/core/src/assembly.rs`: stable ordering
  (system blocks, tool schema sorted by name, history), **stable serialization** (no `HashMap`
  iteration in output), no volatile content (timestamps, absolute paths, random ids) in the prefix;
  tools/system changes detected and reported as a *cache-bust event*.
  **Done when:** same session + same config ⇒ byte-identical request prefix (golden test, run twice
  with randomized `HashMap` seeds). Risk: **High** · Depends: 1.9

- [ ] **3.2 Token accounting** (1h) — `tiktoken-rs` for OpenAI-accurate counts + parse OpenRouter's
  response `usage` (prompt/completion tokens, **cached-token and cost fields** — confirm exact
  field names against openrouter.ai/docs during this stage; caching depends on provider support
  per OpenRouter's sticky-routing prompt-caching guide);
  reconcile estimated vs actual, record delta. Risk: Medium · Depends: 1.3, 3.1

- [ ] **3.3 Metrics collector** (1h) — `crates/core/src/metrics.rs`: per-turn `ttft_ms`,
  `tokens_per_sec`, `cache_hit_pct`, `cost_usd` from a pluggable price table (config, not
  hardcoded); immutable `TurnMetrics` events appended to transcript. Risk: Low · Depends: 3.2

- [ ] **3.4 Live gauge rendering (headless first)** (1h) — status line in `run -p` and interactive
  console mode; validate numbers **before** any TUI work.
  **Done when:** against the `httpmock` fixture with cache usage, gauge prints expected hit % and
  cost to 4 dp. Risk: Low · Depends: 3.3

### Phase 4 — TUI (6 stages, ~7h)

- [ ] **4.1 Extract `crates/tui` + app skeleton** (1h) — ratatui + crossterm event loop; `App`
  state machine (`Idle | Streaming | AwaitingPermission`); channels from core loop → UI;
  **no blocking in the render path**. Risk: Medium · Depends: 1.10

- [ ] **4.2 Composer** (1h) — `ratatui-textarea`: multiline input, history recall, `/commands`,
  paste, scrollback of transcript. Risk: Low · Depends: 4.1

- [ ] **4.3 Streaming markdown pane** (1h) — `tui-markdown` incremental render, code blocks with
  language tag, auto-scroll that doesn't fight the user (pause on manual scroll).
  Risk: Medium (re-render cost/flicker — KQ2.5) · Depends: 4.2

- [ ] **4.4 Status bar with wedge #1 gauges** (1h) — cache-hit %, cost, TTFT, tok/s, model, turn #,
  token budget bar; turns red on budget/cost threshold. Risk: Low · Depends: 3.4, 4.1

- [ ] **4.5 Permission prompt widget** (1h) — command/diff preview +
  `y / n / always-allow (session) / always-allow (rule)`; persisted rules write back to config
  (targets claude-code #11380 "always allow ignored"). Risk: Low · Depends: 4.3, 1.9

- [ ] **4.6 Diff view** (1–2h) — `similar` hunk rendering of pending file edits, accept/reject per
  hunk feeding back to the loop; `insta` TUI snapshots via `TestBackend`.
  Risk: Medium · Depends: 4.3, 1.8

### Phase 5 — Privacy (3 stages, ~3h) — **Wedge #2**

- [ ] **5.1 Secret redactor at write time** (1h) — `crates/core/src/redact.rs`: pattern set
  (AWS/GitHub/Anthropic/OpenAI keys, `PASSWORD=`, private-key blocks, `.env` assignments) +
  entropy heuristic; applied to transcript **and** tool output *before* persistence
  (claude-code #44868 pain). Risk: **High** (false negatives leak; false positives corrupt) ·
  Depends: 2.1

- [ ] **5.2 Redaction test corpus** (1h) — golden fixtures: real-shaped secrets in tool output,
  patch hunks, error messages; `proptest` that redacted output never matches raw pattern; assert
  redaction is applied *before* fsync. Risk: Medium · Depends: 5.1

- [ ] **5.3 Telemetry-off guarantee** (0.5h) — no outbound host except configured LLM providers;
  test asserts zero extra sockets; `doctor` shows exactly what leaves the machine.
  Risk: Low · Depends: 1.10

### Phase 6 — Parity Features (5 stages, ~5h)

- [ ] **6.1 Git-first checkpoints** (1h) — `checkpoint create/list/restore`: snapshot of
  tracked+untracked state to `.iris/checkpoints` (or orphan commits); shell side-effects
  explicitly out of scope, documented. Risk: Medium · Depends: 1.9

- [ ] **6.2 Compaction that fires** (1h) — threshold on *estimated* tokens (pre-request, not after
  failure): summarize older turns into a pinned block, keep last N raw; must preserve
  prefix-invariance rules; test asserts it triggers at 70% and survives resume.
  Risk: **High** (the #1 documented pain) · Depends: 3.2, 3.1

- [ ] **6.3 MCP via `rmcp`** (1h) — stdio client, tools surfaced through the same registry +
  permission gate; per-server tool budget so MCP can't silently eat context.
  Risk: Medium · Depends: 1.5, 1.9

- [ ] **6.4 Hooks/events** (1h) — `on_tool_pre`, `on_tool_post`, `on_turn_end`, `on_compact`:
  shell commands with timeout + JSON payload. Risk: Low · Depends: 1.9

- [ ] **6.5 Custom commands/skills** (1h) — markdown files in `.iris/commands/*.md` with
  frontmatter args → prompt templates; `/help` lists them. Risk: Low · Depends: 4.2

### Phase 7 — Quality Gate (4 stages, ~4h)

- [ ] **7.1 HTTP cassette layer** (1h) — record/replay on `httpmock` covering streaming + tool loops
  for the OpenRouter (OpenAI-compatible) client; no live API needed in CI. Risk: Low · Depends: 1.3

- [ ] **7.2 Coverage to ≥80% on `iris-core`** (1h) — `cargo llvm-cov`; fill gaps (permission policy
  matrix, assembly edge cases, redaction). Risk: Low · Depends: all

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
