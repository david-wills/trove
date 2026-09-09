# Notion

- **id:** `notion`
- **domains:** `notes/` and `tasks/` (notes pages → `notes/` **Phase 3
  pending**; database task items → `tasks/` ✅ **ratified** — data routes by
  shape, not provider)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (API sync once a connection exists). Workspace-export
  ZIP (Markdown+CSV) is the pragmatic Import first step.
- **connection:** `notion` — TokenPaste (Personal Access Token / integration
  token from app.notion.com/developers; user must share specific pages/
  databases with the integration). Not shared with other defs.
- **evidence:** official-docs — Notion REST API + PATs (May 2026 Markdown API);
  workspace export ZIP (Markdown+CSV) documented
- **effort / priority:** M / P2
- **needs:** Needs-login (validation only — build proceeds from documented
  shapes + export ZIP) · `notes/` contract not yet ratified (Needs-David)

## What it is

Notion is a hugely popular block-based workspace: pages, wikis, and databases
that double as task managers. Two record types come out of it and route by
shape — free-form pages land in `notes/`, while database rows that are task
items (status/checkbox/due) land in `tasks/`. Very popular, but extraction has
friction: per-page sharing, a 3 req/s rate limit, no bulk-export endpoint, and
recursive block-tree traversal to rebuild a page.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Page content | all (shared with integration) | title, Markdown body (block tree), created/edited times | official docs |
| Database items (tasks) | all (shared) | properties: status/checkbox, due date, title, relations | official docs |
| Workspace export | all | Markdown + CSV ZIP of shared content | official docs |

All optional in the contract. No tier-specific code paths — unshared pages
simply don't appear (the integration only sees what's shared with it).

## Access & auth

- REST: `POST /v1/databases/{id}/query` (filter by status/checkbox for task
  items), `GET /v1/pages/{id}`, `GET /v1/blocks/{id}/children` (recursive). New
  May 2026: a Markdown API for reading pages as Markdown (page-markdown
  endpoint currently restricted to public integrations — not relied on).
- Auth: Personal Access Token from app.notion.com/developers (user-scoped, no
  OAuth app registration). User must share each target page/database with the
  integration in Notion Settings > Connections.
- Rate limit: ~3 req/s (2,700 req / 15 min). No bulk-export endpoint — must
  traverse the page tree. No local DB exists.
- Export path: Settings > Export all workspace content → ZIP of Markdown + CSV.
  Large workspaces can take up to ~30h; download link expires in 7 days.
- Standalone-clean: plain HTTPS; export is a user-downloaded file.

## Vault mapping

- **Raw layer:** `notes/notion/raw/…` (page blocks) and `tasks/notion/raw/…`
  (database rows) — native API/export objects, full fidelity. Records route
  whole and split by *type*, never one record across folders.
- **Contract layer:**
  - Pages → `notes/notion/YYYY-MM.jsonl` per the (pending) `notes/` contract
    (`ts`, `source`, `guid` = page id, `title`, `body` markdown, `tags`/props
    in `extra`).
  - Task database items → `tasks/notion/YYYY-MM.jsonl` per the **ratified tasks
    contract** (title, status/completed, due date, project = database name,
    `guid` = page id; overflow props in `extra`).
- **Dedupe:** Notion page id as `guid` (stable across export and API); cursor /
  `last_edited_time` watermark in `.trove/notion-sync.json`, rebuildable by
  scanning output files.

## Build plan

1. **Import first:** generic workspace-export ZIP parser (Markdown pages →
   `notes/`, CSV database tables → `tasks/` task rows where shape matches).
   Markdown is already there — pragmatic snapshot path.
2. Module `crates/trove-core/src/notion.rs`: `DEF` (Periodic for the API
   substream), `CONNECTION` (TokenPaste: label/help/placeholder per the
   SimpleFIN affordance rule, with the per-page-sharing instruction in setup
   copy).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. API sync as the second phase: recursive block fetch with pagination,
   3 req/s throttle, `last_edited_time` incremental cursor.
5. Fixtures from documented API responses (a page with nested blocks; a task
   database query) and a small export ZIP; parser + store + dedupe tests,
   unique temp dirs.
6. Vault writes via `store` helpers — tasks rows now (contract ratified); notes
   rows **parked behind Needs-David (`notes/` contract)**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Page content | raw-only | paste a PAT, share a page; Sync now; confirm raw rows in `notes/notion/raw/` + hub last-data |
| Database tasks | ✅ built | share a task database; Sync now; confirm task rows in `tasks/notion/tasks.jsonl` with status/due mapped; events in `tasks/notion/events/YYYY-MM.jsonl` |
| Export ZIP | deferred | not built in this pass — API path is the primary; export ZIP parser parked |

## Build notes (2026-06-17)

- Behavior: Periodic (30 min). Connection: TokenPaste (notion integration secret `ntn_…`).
- Raw layer: all pages in `notes/notion/raw/YYYY-MM.jsonl` (partitioned by `created_time`, upserted by id).
- Tasks contract: database rows whose properties include a `status`, `checkbox`, or due-named `date` property are mapped to `tasks/notion/` via `apply_tasks_sync`. Fate for vanished rows = Deleted (no completed-list endpoint exists in the Notion API).
- Notes contract write: parked — `notes/` contract pending ratification (Needs-David). Raw layer is full fidelity.
- Export ZIP: not built. API path is the primary integration. Parked for a future import box.
- 18 unit tests: mapping, pagination drain, raw dedup, delete diffing, cursor back-compat, token store, connection token-paste method. All green.

## Research notes

`integrations-research.md` → "Calendar, Tasks, Habits & Productivity" §Notion
(Tasks/Databases) (L2642–L2648) + "Artifacts: Notes, Documents, Drafts & Files"
§Notion (L2859–L2865). Feasibility 🟢/🟡 — both paths live and documented in
2026. PATs avoid OAuth app registration for personal use. Build later: API
pagination + block-tree recursion is the cost; start with the export-ZIP
parser, add incremental API sync second. Big exports take ~30h; no offline/
local DB.
