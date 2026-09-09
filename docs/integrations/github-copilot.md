# GitHub Copilot

- **id:** `github-copilot`
- **domains:** `developer/` (raw-only per taxonomy — heterogeneous shapes)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (scan VS Code workspaceStorage; per-session
  seen-set). Manual `Chat: Export Chat…` files import through the same
  parser (Import fallback).
- **connection:** none (local files — no GitHub login needed; distinct from
  the `github` cloud provider)
- **evidence:** community-schema — kafumanto/copilot-tokens confirms the
  chatSessions schema is legible; VS Code ≥1.109 `.jsonl` mutation-log
  format is the current standard (high confidence)
- **effort / priority:** S / P1
- **needs:** privacy (AI conversation content ≈ message bodies — opt-in
  with explicit acknowledgement)

## What it is

GitHub Copilot Chat conversations as stored locally by VS Code — the
largest-userbase AI coding assistant, so the broadest-reach AI-sessions
source after Claude Code. Plain JSON/JSONL files per workspace, no
permissions, no API. Captures the "what did I work on with AI" trail for
every repo the user chats over in VS Code.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Chat sessions (≥1.109) | any Copilot plan; local files | session id, ts, messages, model | community (kafumanto/copilot-tokens) |
| Chat sessions (older .json) | older VS Code | flat snapshot, same fields | community docs |
| Workspace context | n/a | hash → repo path via workspace.json | community docs |
| Manual export import | n/a | same format via `Chat: Export Chat…` | VS Code command |

All optional. Default capture is session metadata + summary (ts, workspace,
model, message_count); full conversation text is a separate opt-in toggle
(collection-depth rule shared with Claude Code).

## Access & auth

- `~/Library/Application Support/Code/User/workspaceStorage/<hash>/chatSessions/`
  — `.jsonl` (append-only mutation log, VS Code ≥1.109) or `.json` (older
  flat snapshot). `workspace.json` in each hash dir maps hash → repo/folder
  path.
- Cursor uses the same VS Code foundation: the identical pattern under
  `~/Library/Application Support/Cursor/…` belongs to the `cursor`
  provider, not this one.
- Home-dir paths; troved's existing FDA grant covers ~/Library. Plain file
  reads (no SQLite lock concerns, but re-read growing .jsonl files
  incrementally). No network, no auth. Standalone-clean.

## Vault mapping

- **Raw layer:** `developer/github-copilot/YYYY-MM.jsonl` — normalized
  session metadata per the shared AI-sessions shape (`ts`, `source`,
  `workspace_path`, `model`, `message_count`, `summary`); full text (when
  opted in) alongside. (Research doc's `developer/copilot/` path predates
  the identity convention; folder = provider id.)
- **Contract layer:** none — `developer/` is raw-only.
- **Dedupe:** `guid` = session id (+ workspace hash); mutation logs re-read
  from a per-file byte/line cursor in `.trove/github-copilot-sync.json`,
  rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/github_copilot.rs`: `DEF` (Periodic)
   scanning workspaceStorage; parse both `.jsonl`
   mutation-log and `.json` snapshot variants; join `workspace.json` for
   repo context.
2. Registration line in `INTEGRATIONS`. Import box accepts manually
   exported `Chat: Export Chat…` files through the same parser.
3. Privacy gate: ships opt-in (AI conversation content), metadata+summary
   default, full-text toggle.
4. Fixtures: synthetic chatSessions in both formats + a workspace.json;
   parser, hash→repo mapping, incremental-append, and dedupe tests in
   unique temp dirs.
5. Build alongside Claude Code / Cursor via the shared AI-sessions ingest
   helper (cross-cutting note 4) — this is the S-effort member of that
   batch.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Live sessions | — | enable the toggle (acknowledge opt-in); run a Copilot chat in VS Code; Sync now; confirm rows in `developer/github-copilot/` + hub last-data |
| Older .json format | — | fixture-only unless a machine with pre-1.109 sessions surfaces |
| Manual export import | — | `Chat: Export Chat…` in VS Code, drop the file on the import box, confirm the session row |

## Build notes (2026-06-15)

- Confirmed real VS Code workspaceStorage schema from local disk: JSONL mutation-log
  (`kind:0` snapshot + `kind:1` delta lines) for ≥1.109, flat JSON for older versions.
  Sessions present on this machine have empty `requests[]` (sessions opened but no messages
  sent); the parser is built for the full schema including populated `requests[]`.
- Both formats share: `sessionId`, `creationDate` (epoch ms), `version`,
  `initialLocation`, `requests[]`. VS Code ≥1.109 v3 schema serialises the session title
  as `customTitle` (confirmed in workbench.desktop.main.js); older/exported formats may
  use `computedTitle`, `title`, or `summary`. Model is per-request, not top-level.
- `developer/` raw-only — no contract. No connection needed (local files, FDA-gated path
  already covered by existing troved grant).
- Privacy gate implemented: metadata-only by default (ts, workspace, model, request_count,
  summary); full `requests[]` is opt-in via `github-copilot-transcripts` CoveredBy sub-toggle,
  mirroring `claude-code-transcripts`.
- Tested: 20 unit + integration tests in unique temp dirs. `cargo check` clean.
- No new deps added (uses existing chrono/serde_json/dirs).
- Fix (2026-06-15): adversarial review found parser was reading `title`/`summary` keys
  that VS Code never writes; corrected to `customTitle` > `computedTitle` > `title` >
  `summary` priority, updated fixture from fictional `"title"` to real `"customTitle"` key.
  Also added last_message_ts fallback from per-request `timestamp` field for .jsonl sessions.

## Research notes

`integrations-research.md` → "Computer & Developer Activity" §GitHub
Copilot Chat Sessions (L1412–L1418). Feasibility 🟢 high — local files, no
permissions, community-verified schema. Zero-friction batch member with
shell history / local git / Claude Code (cross-cutting note 3). Keep
boundaries clean: this provider is local VS Code chat files only; the
`github` provider owns the cloud API, and `cursor` owns Cursor's stores
despite the shared VS Code lineage.
