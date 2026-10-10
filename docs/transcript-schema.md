# IRIS Transcript Schema

**Status:** v0 — live since stage 2.1. **Stability:** the schema is versioned;
this document is published from day one (decision D5) so external tools can read
IRIS transcripts without reverse-engineering them.

## Format

Transcripts are **JSONL**: one JSON object per line, UTF-8, `\n`-terminated,
append-only. A session is exactly one transcript file.

Every line carries a versioned envelope:

```json
{"v":0,"type":"session_start","session_id":"ses_2f1a…","started_at":"2026-10-10T09:14:22Z","cwd":"/home/u/proj","model":"openai/gpt-4o-mini","iris_version":"0.1.0"}
```

| Envelope field | Type | Meaning |
|---|---|---|
| `v` | integer | Schema version. This document describes **v0**. Readers must reject versions they do not know rather than guess. |
| `type` | string | Event type, one of the four below. |

## Versioning policy

- **Additive changes** (new optional fields, new event types) may appear within a
  version only if old readers can safely reject or surface them. IRIS's own
  reader **never skips an unknown `type`** — it fails loudly, because silently
  dropping lines is how resume loses history.
- **Breaking changes** (renaming/removing a field, changing a field's meaning or
  type) bump `v`.
- The transcript file is the **source of truth**; any index built from it
  (stage 2.4) must be rebuildable by re-reading the JSONL.

## Events (v0)

### `session_start`

First line of every transcript.

| Field | Type | Notes |
|---|---|---|
| `session_id` | string | Stable id for resume (`sessions resume`, stage 2.3). |
| `started_at` | string | RFC 3339 / UTC (`…Z`). |
| `cwd` | string | Absolute working directory of the session. |
| `model` | string | OpenRouter model id used for this session. |
| `iris_version` | string | Binary version that opened the transcript. |

### `message`

One conversation message — the resume primitive. Fields are exactly the domain
message type: `role` (`system` \| `user` \| `assistant` \| `tool`) and `content`
(array of content blocks). Content block types:

| `type` | Fields |
|---|---|
| `text` | `text` |
| `image` | `media_type`, `data` (base64) |
| `tool_use` | `id`, `name`, `input` (arbitrary JSON) |
| `tool_result` | `tool_use_id`, `content`, `is_error` |

Example (assistant issuing a tool call):

```json
{"v":0,"type":"message","role":"assistant","content":[{"type":"text","text":"Reading it."},{"type":"tool_use","id":"call_1","name":"read_file","input":{"path":"Cargo.toml"}}]}
```

### `usage`

Token accounting for one provider response, written once per turn in arrival
order.

| Field | Type | Notes |
|---|---|---|
| `turn` | integer | 1-based turn number within the run. |
| `usage` | object | `input_tokens`, `output_tokens`, `total_tokens` (integers, cumulative across the run). |

```json
{"v":0,"type":"usage","turn":1,"usage":{"input_tokens":120,"output_tokens":34,"total_tokens":154}}
```

### `session_end`

Last line of a **cleanly finished** transcript.

| Field | Type | Notes |
|---|---|---|
| `outcome` | string | `completed` \| `max_turns` \| `budget_exceeded`. |
| `turns` | integer | Provider calls made. |
| `usage` | object | Cumulative totals (same shape as `usage`). |
| `ended_at` | string | RFC 3339 / UTC. |

```json
{"v":0,"type":"session_end","outcome":"completed","turns":1,"usage":{"input_tokens":120,"output_tokens":34,"total_tokens":154},"ended_at":"2023-11-14T22:13:20Z"}
```

**Absence of `session_end` is meaningful**: it means the run was interrupted
(crash, `SIGKILL`, provider error). Readers must treat such transcripts as
resumable, not corrupt.

## Guarantees & non-guarantees

- **Guaranteed:** complete lines are self-contained; a reader that can parse one
  line can parse every line. After a crash, the final line may be truncated —
  readers recover every *complete* line (stage 2.2's contract) and must ignore a
  partial trailing line rather than fail the whole file.
- **Guaranteed:** secret redaction happens *before* persistence (stage 5.x);
  transcripts never contain API keys.
- **Not guaranteed:** inter-line ordering beyond "append order"; transcripts are
  single-writer.

## Reading transcripts

Any JSONL tool works. Extract the conversation with `jq`:

```bash
jq -r 'select(.type == "message") | "\(.role): \(.content[] | select(.type == "text") | .text)"' transcript.jsonl
```

Detect interrupted sessions:

```bash
tail -1 transcript.jsonl | jq -e '.type == "session_end"' >/dev/null || echo "interrupted"
```
