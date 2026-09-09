# Cursor

- **id:** `cursor`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes, no
  shared contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read local SQLite; per-session seen set)
- **connection:** none (local files)
- **evidence:** community-schema — vibe-replay.com deep-dive confirms the
  exact `store.db` schema (meta + blobs tables) and the `state.vscdb`
  `cursorDiskKV` key-prefix scheme; no official docs
- **effort / priority:** M / P1
- **needs:** privacy (AI chat/agent transcripts are conversation content —
  opt-in for full text, metadata-first default)

## What it is

Cursor's AI chat and agent session history — the AI-pairing trail for one
of the most popular AI IDEs. Same value proposition as Claude Code session
history (intent, prompts, what was built with AI), but harder to extract:
the data is spread across multiple SQLite stores with community-documented
rather than official schemas. M effort, sequenced after Claude Code.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Session metadata (v1 target) | none (local files) | agentId, name, mode, lastUsedModel, ts, rough message count | community-schema (`~/.cursor/chats/*/*/store.db` meta table, vibe-replay.com) |
| Conversation content | none — gated by Trove's opt-in toggle | message/blob content from store.db blobs | community-schema, medium confidence |
| Agent transcripts | none | JSONL transcripts at `~/.cursor/projects/*/agent-transcripts/*.jsonl` | community-confirmed path |
| Full replay (later) | none | composerData/bubbleId blobs | `state.vscdb` cursorDiskKV — compressed JSON, needs decoding; deferred |

All optional; v1 ships metadata-only and degrades gracefully where blob
decoding fails.

## Access & auth

- Primary (v1): `~/.cursor/chats/*/*/store.db` — SQLite, tables `meta`
  (JSON values) + `blobs`. Plus `~/.cursor/projects/*/agent-transcripts/*.jsonl`.
- Later: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
  (1+ GB; `cursorDiskKV` with composerData/bubbleId/agentKv key prefixes,
  compressed JSON blobs) and per-workspace `workspaceStorage/<hash>/state.vscdb`.
- No TCC permission (home-dir paths, no FDA). **Copy-then-read** — Cursor
  locks its DBs while running (established M3 pattern in the codebase).
- No network, no auth. Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/cursor/YYYY-MM.jsonl` — one row per session
  (metadata: id, name, mode, model, ts, message_count). Opt-in full text
  lands as sidecars under `developer/cursor/transcripts/`. Path from the
  taxonomy (`developer/`, raw-only).
- **Contract layer:** none — `developer/` carries native shapes.
- **Dedupe:** `guid` = agent/session id; seen set + mtime cursor in
  `.trove/cursor-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/cursor.rs`: `DEF` (Periodic), pull hook
   globs the chats dirs, copies each `store.db` to a temp path, reads via
   rusqlite.
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. v1 scope: `store.db` meta rows + agent-transcript JSONL only. The
   `state.vscdb` cursorDiskKV blob decoding is a follow-up slice — don't
   block shipping on it.
4. **Privacy gate:** metadata-only by default; conversation text opt-in
   with explicit acknowledgement (mirror the `claude-code` toggle).
5. Fixtures: construct miniature store.db files in tests (rusqlite can
   build them) matching the documented schema, plus a malformed-blob case
   asserting graceful skip. Community schema, no official contract —
   parser must tolerate unknown keys/shapes across Cursor releases.
6. Note: Cursor is VS Code-based; the `vscode` provider's
   recently-opened pattern and the `github-copilot` chatSessions pattern
   both also apply under `~/Library/Application Support/Cursor/` — those
   stay in their own providers, not duplicated here.

## Build notes (2026-06-16)

- Primary source is `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
  (`cursorDiskKV` table, `composerData:<uuid>` keys). The brief's `~/.cursor/chats/*/*/store.db`
  schema is also supported (legacy/alternative path) but absent on the test machine.
- **Real on-disk schema (verified 7/7 rows in state.vscdb, current Cursor ≥v0.40):**
  - `createdAt` → native JSON **int** (unix ms), e.g. `1779815196792`
  - `isAgentic` → native JSON **bool**, e.g. `false`
  - `fullConversationHeadersOnly` → native JSON **array**, e.g. `[]` or `[{"bubbleId":…}]`
  - `modelConfig` → native JSON **object**, e.g. `{"modelName":"claude-sonnet-4-5","maxMode":false}`
  - `unifiedMode`, `name`, `composerId`, `_v` → strings as expected
- The parser also accepts older string-encoded forms (string `"True"`/`"False"` for
  `isAgentic`; JSON-string-wrapped arrays/objects for the others) as fallbacks for
  pre-v0.40 Cursor installations. All 7 real-machine rows use native types.
- The 7 sessions on the test machine are all empty (message_count=0, modelConfig has
  no active model), so data-loss from the original string-read bug was invisible during
  first-pass testing. The adversarial review caught it; parser now reads native types first.
- Added `TRANSCRIPTS_DEF` (CoveredBy opt-in) following the claude_code/github_copilot pattern,
  plus its registration line in integrations.rs.
- No new Cargo deps added (rusqlite already bundled).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Session metadata | ✅ built | run on a machine with real Cursor history while Cursor is open (proves copy-then-read); Sync now; rows in `developer/cursor/` have correct `model` and `message_count` (non-zero for active sessions) |
| model / message_count accuracy | ✅ fixed (2026-06-16) | parser now reads native JSON object/array for modelConfig/fullConversationHeadersOnly; re-validate with a session that has a model set and ≥1 message |
| Agent transcripts | ✅ built | run an agent session in Cursor; Sync now; transcript row appears |
| Full-text opt-in | ✅ built | flip `cursor-transcripts` toggle, confirm sidecars appear; default-off runs never wrote content |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Cursor IDE
Chat History (L1324–L1330). Feasibility 🟢 high but M effort: data spread
across multiple DBs, joins of meta + blobs, undecoded key-prefix scheme in
the global store. Schema is community-reverse-engineered and may drift with
Cursor releases — keep the validation matrix re-runnable and the parser
tolerant. Sequenced after `claude-code` (same value, S effort there).
