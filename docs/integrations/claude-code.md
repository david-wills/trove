# Claude Code

- **id:** `claude-code`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes, no
  shared contract)
- **status:** 🧪 built 2026-06-14 (fixture-tested; self-contained — validates
  against this machine's real `~/.claude`, no login/key/sample needed)
- **unavailable_reason:** none
- **behavior:** Periodic (hourly; scan `<base>/projects/*/*.jsonl`;
  per-session-id → file-mtime cursor, re-scan + **upsert** on mtime advance)
- **connection:** none (local JSONL files; no TCC — plain home-dir;
  `CLAUDE_CONFIG_DIR` env overrides the base)
- **evidence:** community-schema + **a real local sample** — the build sampled
  the on-disk shape and found it DEVIATES from the community docs: tool calls
  are content *blocks* inside `assistant.message.content[]`/`user…content[]`
  (not top-level records), and the auto-title is an **`ai-title`** record
  (field `aiTitle`), with no top-level `summary` type. Parser is tolerant of
  both forms + unknown/malformed records. (Refs: Yi Huang article,
  simonw/claude-code-transcripts, raine/claude-history.)
- **effort / priority:** S / P0
- **needs:** privacy (handled in-build) — full conversation text is gated
  behind a **default-off `claude-code-transcripts` opt-in sub-arm**
  (`CoveredBy("claude-code")`); the default rows carry metadata + the auto-title
  summary ONLY, verified to contain **zero** prompt/response/tool-IO content.

## What it is

Claude Code session history: every AI-assisted coding session, as plain
JSONL in the home directory. Uniquely valuable "what did I work on with AI"
signal that complements local git and shell history — the prompt/summary
trail captures intent that commits don't. Zero permissions, zero network.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Session metadata | none (local files) | session_id, project_path, start_ts, end_ts, message_count, tool-call counts by type | community-schema (Yi Huang article, OSS tools) |
| Auto-generated summary | none | summary text when present | same |
| Full transcript text | none — gated by Trove's own opt-in toggle, not by the source | user prompts, assistant responses, tool calls/results | same |

All fields optional; sessions without summaries simply carry none.

## Access & auth

- Files: `~/.claude/projects/<project-slug>/<session-id>.jsonl` (one typed
  message record per line); global index `~/.claude/history.jsonl` (prompt
  text, ts, project path, session id). Honor the `CLAUDE_CONFIG_DIR` env
  override for the base path.
- No TCC permission (plain home-dir files), no network, no auth.
  Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/claude-code/YYYY-MM.jsonl` — one row per
  session (metadata + summary), partitioned by start month. With the
  full-text toggle on, transcripts land as sidecar files under
  `developer/claude-code/transcripts/` (artifacts, not events — keeps the
  session stream scannable). Path from the taxonomy (`developer/`,
  raw-only); the research doc predates the taxonomy.
- **Contract layer:** none — `developer/` carries native shapes.
- **Dedupe:** `guid` = session id; per-session seen set + last-mtime cursor
  in `.trove/claude-code-sync.json`, rebuildable from output files. Re-scan
  sessions whose file mtime advanced (sessions append while live).

## Build plan

1. Module `crates/trove-core/src/claude_code.rs`: `DEF` (Periodic), pull
   hook reads `history.jsonl` for the index then per-session files; derive
   project name from the project slug/path.
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. **Privacy gate:** metadata + summary by default; full transcript text is
   a per-integration opt-in toggle with explicit acknowledgement (collection-
   depth-configurable pattern, like raw-email).
4. Fixtures: hand-built session JSONL covering user/assistant/tool-call/
   summary record types plus a summary-less session; tests for cursor,
   re-scan-on-append, and the opt-in toggle switching output shape.
5. Tolerant parsing: unknown record types must be skipped, not fatal — the
   format is community-documented, not contractual, and new record types
   appear across CLI releases.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Session metadata + summaries | 🧪 | enable Claude Code in the hub; Sync now; confirm rows in `~/Trove/developer/claude-code/YYYY-MM.jsonl` match sessions in the `claude` resume list (session_id, project, counts, summary); confirm NO prompt/response text in those rows; hub last-data updates |
| Full-text opt-in | 🧪 | enable the **Claude Code — full transcripts** toggle (`claude-code-transcripts`), Sync now, confirm sidecars under `developer/claude-code/transcripts/<session-id>.jsonl`; with it off, confirm that dir is never created |
| CLAUDE_CONFIG_DIR override | 🧪 | `export CLAUDE_CONFIG_DIR=<copied dir>`; Sync; confirm the scan follows it |

## Build notes (as-built, 2026-06-14)

- **First `developer/` (raw-only) provider** — no contract, no media-plays, no
  DOMAINS/spec change; the collector designs its own raw row shape.
- **Privacy gate via the `CoveredBy` pattern** (the registry-native per-arm opt-in,
  per `browser.rs`/`screen_time.rs`): a second DEF `claude-code-transcripts`
  (`CoveredBy("claude-code")`, default-off) is consulted via
  `vault.integration_enabled("claude-code-transcripts")`; only then are full
  transcripts written. Adversarial verifier traced every default-row field +
  ran sentinel probes → **zero conversation content can reach the default rows**;
  opt-in gating airtight.
- **Row shape** (raw): `session_id`(guid), `project`, `project_path` (from the
  authoritative per-record `cwd`; slug-decode is fallback only — the slug is
  lossy), `start_ts`/`end_ts`, `message_count`, `user`/`assistant` counts
  (tool_result-only user turns excluded), `tool_calls` (name→count, from content
  blocks), `summary` (from `ai-title`, omitted if absent).
- **Scan/cursor/upsert:** mtime cursor in `.trove/claude-code-sync.json`; re-scan
  on mtime advance; **upsert by session_id** within the stable start-month
  partition (read→replace→atomic rewrite) so live sessions update in place.
- **Adversarial-verify fix applied:** a timestamp-less (degenerate) session is now
  **skipped** rather than written with a `now()` start — the verifier showed the
  `now()` fallback could orphan a stale cross-month row on re-scan. 0 blocking
  defects remain; privacy contract holds.

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Claude Code
Session History (L1308–L1314). Feasibility 🟢 high; already "planned" in
data-sources.md. Format stability rests on OSS-tooling consensus rather
than official docs — keep the parser tolerant. Sibling AI-transcript
sources (`cursor`, `github-copilot`, `zed`, `windsurf`, `chatgpt`) each get
their own provider; shapes stay per-source raw, joined at read time if an
AI-sessions view ever wants them together.
