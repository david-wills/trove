# Zed

- **id:** `zed`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read local stores; per-thread seen-set)
- **connection:** none (local files)
- **evidence:** Zed source confirmed (db.rs + paths/src/paths.rs via GitHub):
  SQLite `threads.db` at `~/Library/Application Support/Zed/threads/` (macOS);
  `data_type` column = "json" | "zstd"; zstd decompression via pure-Rust
  `ruzstd`; `DbThread` JSON includes `title`, `messages`, `model`, timestamps;
  legacy `~/.config/zed/conversations/*.json` from `legacy_thread.rs`.
- **effort / priority:** M / P2
- **needs:** privacy (AI conversation content ≈ message bodies — opt-in
  with explicit acknowledgement); live smoke-test recommended before shipping
  (threads.db schema confirmed from Zed source + unit-tested with rusqlite/ruzstd)

## What it is

Zed's AI assistant conversation history: the prompts, responses, and thread
metadata from the editor's agent panel. Same "what did I work on with AI"
signal as Claude Code session history, for a fast-growing editor popular
with Rust/systems developers. Storage has churned across versions (JSON
files → SQLite with compressed blobs), which is the main risk.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Legacy conversations | older Zed versions | per-conversation JSON: ts, messages | community (path documented) |
| Threads (current) | 2025+ versions | thread metadata + content blobs | community (discussion #32335; format unverified) |

All optional; a user with only one storage generation gets that slice.
Default capture is session metadata + summary; full conversation text is a
separate opt-in toggle (collection-depth rule shared with Claude Code).

## Access & auth

- Legacy: `~/.config/zed/conversations/*.json` — plain JSON per
  conversation, straightforward fallback.
- Current: `~/.local/share/zed/threads/threads.db` — SQLite with
  compressed blobs; encoding undocumented. macOS may also use
  `~/Library/Application Support/Zed/`. Copy-then-read (M3 pattern) while
  Zed runs.
- Home-dir paths, no TCC prompts beyond troved's existing grants. No
  network, no auth. Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/zed/YYYY-MM.jsonl` — normalized session
  metadata per the shared AI-sessions shape the developer domain converged
  on (`ts`, `source`, `project`, `model`, `message_count`, `summary`);
  full text (when opted in) alongside. Raw-only — no contract.
- **Contract layer:** none — `developer/` is raw-only.
- **Dedupe:** `guid` = thread/conversation id; seen-set cursor in
  `.trove/zed-sync.json`, rebuildable from output files.

## Build plan

1. **Spike first (parser-last, per the Needs-sample rule):** inspect
   `threads.db` on a real machine — if blobs are plain/zstd JSON, proceed;
   if heavily encoded, ship the legacy-JSON path only and park the SQLite
   path behind Needs-sample.
2. Module `crates/trove-core/src/zed.rs`: `DEF` (Periodic), pull hook that
   reads legacy JSON first, threads.db when the spike lands.
3. Registration line in `INTEGRATIONS`.
4. Privacy gate: ships opt-in (AI conversation content), metadata+summary
   default, full-text toggle.
5. Fixtures: real sample files captured during the spike (synthetic legacy
   JSON is fine immediately); parser + cursor tests, unique temp dirs.
6. Reuse the shared AI-sessions ingest helper (Claude Code / Cursor /
   Copilot batch) so this module is mostly format glue.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Legacy conversations | built — unit tested | `scan_legacy_writes_metadata_no_transcripts_by_default` + mtime/upsert tests; live: place `~/.config/zed/conversations/*.json`, Sync now, rows in `developer/zed/` |
| threads.db (json) | built — unit tested | `scan_db_json_row_parsed_correctly`: real rusqlite DB, asserts id/model/message_count/folder_paths; live: run a Zed agent thread, Sync now, confirm row in `developer/zed/YYYY-MM.jsonl` |
| threads.db (zstd) | built — unit tested | `scan_db_zstd_row_decompresses_and_parses`: ruzstd compress→decompress round-trip + field parse; live: same as json path when Zed writes compressed blobs |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Zed Editor AI
Conversation History (L1396–L1402). Feasibility 🟡 medium — paths changed
across versions, no official format docs. Build after the well-documented
AI-session sources (Claude Code, Copilot, Cursor) so the shared shape is
settled before tackling the undocumented store. Zed notes conversations
"may be used for training" only if feedback is sent — local storage stays
local regardless.
