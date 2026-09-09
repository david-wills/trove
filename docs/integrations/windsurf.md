# Windsurf

- **id:** `windsurf`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes)
- **status:** 🧪 built (raw scaffold — parser parked; see Research notes)
- **unavailable_reason:** none
- **behavior:** Periodic (read local cascade store; per-session seen-set)
- **connection:** none (local files)
- **evidence:** community-confirmed — `~/.codeium/windsurf/cascade/<uuid>.pb`
  (protobuf binary; schema not public; header bytes may be non-standard /
  encrypted per community investigation); VS Code state.vscdb contains
  only UI state, NOT chat history (unlike Cursor); Devin Desktop rebrand
  path `~/.codeium/windsurf-next/cascade/` also probed
- **effort / priority:** M / P2
- **needs:** privacy (AI conversation content ≈ message bodies — opt-in
  with explicit acknowledgement) · Needs-sample (cascade/ format
  undocumented; rebrand-driven path churn risk — spike before committing)

## What it is

Windsurf's Cascade chat history: the AI pair-programming conversations from
the Codeium-built, VS Code-based editor. Same "what did I work on with AI"
signal as Claude Code / Cursor history. Windsurf was acquired by Cognition
and is being rebranded as Devin Desktop (2026), so storage paths carry real
churn risk; there is no official export (issue #127 went unanswered), so
local-file reverse-engineering is the only path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Cascade chat sessions | all plans (local store) | expected: ts, session, messages, model | community (path only; format unverified) |
| VS Code-style state | unverified | possibly state.vscdb blobs (Cursor pattern) | inference from VS Code base |

Field list is provisional until a sample is inspected. Default capture is
session metadata + summary; full conversation text is a separate opt-in
toggle (collection-depth rule shared with the other AI-session sources).

## Access & auth

- `~/.codeium/windsurf/cascade/` on macOS — the `codeium/windsurf` prefix
  appears stable through the rebrand so far. Windsurf is VS Code-based, so
  the `state.vscdb` / workspaceStorage patterns from Cursor and Copilot may
  also apply — the spike checks both.
- Home-dir paths, no TCC prompts beyond troved's existing grants.
  Copy-then-read (M3 pattern) for anything SQLite-shaped. No network, no
  auth. Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/windsurf/YYYY-MM.jsonl` — normalized session
  metadata per the shared AI-sessions shape (`ts`, `source`, `project`,
  `model`, `message_count`, `summary`); full text (when opted in)
  alongside.
- **Contract layer:** none — `developer/` is raw-only.
- **Dedupe:** `guid` = session id (or content hash if no stable id
  surfaces); seen-set cursor in `.trove/windsurf-sync.json`, rebuildable
  from output files.

## Build plan

1. **Spike first (parser-last, per the Needs-sample rule):** inspect the
   cascade/ directory on a machine with Windsurf installed — identify file
   format (JSON? protobuf? SQLite?) and whether the VS Code state.vscdb
   pattern applies. No parser is written before a real sample exists.
2. Module `crates/trove-core/src/windsurf.rs`: `DEF` (Periodic), pull hook
   over whichever store the spike confirms.
3. Registration line in `INTEGRATIONS`.
4. Privacy gate: ships opt-in (AI conversation content), metadata+summary
   default, full-text toggle.
5. Fixtures from the spike's captured samples; parser + cursor tests,
   unique temp dirs. Reuse the shared AI-sessions ingest helper.
6. Watch the Devin Desktop rebrand: the def should probe both old and new
   paths rather than hardcoding one.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Cascade sessions | — | blocked on the spike sample; once parsed: run a Cascade chat, enable the toggle (acknowledge opt-in), Sync now, confirm rows in `developer/windsurf/` + hub last-data |

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §Windsurf
(Cascade) Chat History (L1404–L1410). Feasibility 🟡 medium — path
community-confirmed, format undocumented, rebrand instability.

### 2026-06 spike findings

Evidence gathered from community code (agentlytics, hyxnj666-creator/ai-memory,
rsvedant/opencode-windsurf-auth, voidcraft-dev/memory-forge-rs):

- **Storage path confirmed**: `~/.codeium/windsurf/cascade/<uuid>.pb`
  (macOS). Devin Desktop rebrand variant: `~/.codeium/windsurf-next/cascade/`.
- **File format**: Protobuf binary (`.pb`). The proto schema is NOT publicly
  documented. Community investigation found header bytes `40 7B D3 BE D0 3D...`
  which appear non-standard (possibly encrypted or custom-wrapped protobuf).
- **VS Code state.vscdb**: Does NOT contain Windsurf chat history. Contains
  only UI state (auth tokens, editor state). Unlike Cursor, the vscdb is
  not a source for Cascade sessions.
- **Live gRPC**: `GetAllCascadeTrajectories` / `GetCascadeTrajectory` via
  `http://127.0.0.1:{port}/exa.language_server_pb.LanguageServerService/`
  with `x-codeium-csrf-token`. Requires running language server — violates
  standalone rule; not pursued.
- **Session file sizes**: ~20 MB active trajectory, ~240 KB after archival
  (community reports). UUIDs are the stable session ids.
- **No public export API**: GitHub issue #127 (Exafunction/codeium) went
  unanswered; no official export mechanism exists as of 2026-06.

### Built

Raw scaffold landed: Periodic collector that enumerates `*.pb` files in
both cascade directories and writes one metadata row per session
(`session_id`, `first_seen`, `last_modified`, `file_bytes`, `variant`) to
`developer/windsurf/YYYY-MM.jsonl`. Rows are partitioned by `first_seen`
(stable; captured once and frozen in the cursor) so sessions never move
between month files when the file's mtime changes. Dedup key is
`"variant/session_id"` to avoid aliasing between the windsurf and
windsurf-next rebrand variants. No protobuf content is read (schema
unknown/encrypted). Parser parked; 15/15 unit tests green.

**To unpark the real parser**: obtain a `.pb` sample + either the proto schema
or a successful heuristic string-extraction approach (see
zstnbb/PCE-Core/windsurf_cascade.py for network-capture approach as a
possible complement). Re-spike if the Devin Desktop rebrand moves storage.
