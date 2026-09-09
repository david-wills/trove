# Craft

- **id:** `craft`
- **domains:** `notes/` (contract: **Phase 3 pending** — notes shape, drafted
  with Apple Notes / Bear / Drafts / Day One / Obsidian / Logseq)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll the documents/daily-notes endpoints; watermark
  cursor on modified time)
- **connection:** `craft` — TokenPaste, **two fields**: a per-connection API
  *endpoint* AND a Bearer *token*, both generated in craft.do Settings > API.
  The endpoint is not a global base URL — the user copies both. Not shared
  with other defs.
- **evidence:** official-docs — docs.craft.co (Craft API, 2025+; per-
  connection endpoint with user-controlled read permissions: specific docs,
  all daily notes, or full space; regex search, timezone-aware date filters)
- **effort / priority:** S / P2
- **needs:** notes contract not yet ratified (Needs-David)

## What it is

Polished document/notes app (Mac App Store, CloudKit backend). Users write
structured documents and daily notes. Its own API (2025+) is the clean
read path — no reverse-engineering. Because the backend is CloudKit and the
app is sandboxed, there is **no accessible local SQLite**, so the API (or
manual Markdown export) is the only standalone path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Documents | per-connection scope | title, body (markdown), collection, created/modified | official docs |
| Daily notes | per-connection scope | date-keyed note body | official docs |
| Search | all | regex + timezone-aware date filtering | official docs |

All optional in the contract; the user's chosen connection scope (specific
docs / all daily notes / full space) determines coverage — no special code
paths, the pull just reads what the token can see.

## Access & auth

- REST: per-connection endpoint (user pastes it) + Bearer token. Supports
  search/list/read of documents, collections, and daily notes. Permissions
  are fixed at connection-creation time in the app.
- No documented hard rate limit in the research doc — be conservative.
- No TCC, no local files (CloudKit/sandboxed). Standalone-clean (plain
  HTTPS). Manual Markdown export (Share > Export) is an M1 fallback.

## Vault mapping

- **Raw layer:** `notes/craft/raw/YYYY-MM.jsonl` — the API document objects,
  full fidelity.
- **Contract layer:** `notes/craft/YYYY-MM.jsonl` per the (pending) notes
  contract — expected shape: one row per document/daily-note (`ts` =
  created or modified, `source`, `guid` = document id, `title`, `body`
  markdown, `date` for daily notes), collection refs + scope in `extra`.
- **Dedupe:** document id as `guid`; cursor in `.trove/craft-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/craft.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste with **two fields** — endpoint + token; label/help/
   placeholder per the SimpleFIN affordance rule, make clear both come from
   Settings > API), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`; `connection:
   Some("craft")`.
3. Fixtures from docs.craft.co example responses (documents + daily notes);
   parser + store + cursor tests, unique temp dirs.
4. Vault writes via `store` helpers once the notes contract is ratified;
   until then **parked behind Needs-David (contract)**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Documents | — | paste endpoint + token in the connect card; Sync now; confirm rows in `notes/craft/` + hub last-data |
| Daily notes | — | confirm date-keyed daily notes appear with their `date` field |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Craft (L2915–L2921). Feasibility 🟢 high. The defining wrinkle: the API
endpoint is **per-connection**, not a global base URL — the connection def
must collect both endpoint and token. CloudKit backend rules out a local-DB
path. A Claude Code skill for Craft already exists (reference for shapes).
