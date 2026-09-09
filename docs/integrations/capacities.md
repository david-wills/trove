# Capacities

- **id:** `capacities`
- **domains:** `notes/` (contract: **notes** — bound, reuses `crate::notes::Note`)
- **status:** 🧪 built (parser parked — see Needs-sample below)
- **unavailable_reason:** none
- **behavior:** Import (user drops an export ZIP; contract parser parked pending real sample)
- **connection:** none
- **evidence:** official-docs — docs.capacities.io (REST API + the May 2025
  automated local-export feature: up to 5 schedules, free tier supported)
- **effort / priority:** S / P2
- **needs:** Needs-sample (exact YAML frontmatter field names for object `id`,
  `createdAt`, `modifiedAt`, `type` are undocumented — raw layer ships, contract
  parser parked until a real export ZIP is obtained and inspected)

## What it is

Object-based notes / personal-knowledge-base app. Users build a "space" of
typed objects (notes, daily notes, tags, collections). As of May 2025 it
can run scheduled local export ZIPs straight to disk with no cloud
round-trip — which makes it a clean watch-folder import rather than an API
poll. Free tier supports the export.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full-space export ZIP | all plans incl. free | structured object content (notes, daily notes, tags) | official docs |
| API (optional) | Bearer token | search/read of docs + daily notes | official docs |

All optional in the contract; omit-if-empty.

## Access & auth

- **Export path (primary):** user configures Settings > Export to dump
  ZIPs (daily/weekly/monthly, up to 5 schedules) into a watched folder
  (e.g. `~/Downloads` or a Trove-watched directory). The offline export
  engine runs on the Mac — no server round-trip. No auth needed.
- **API path (optional later):** Bearer-token REST at docs.capacities.io;
  obtainable in the desktop app.
- TCC: only ordinary file-read on the user-chosen folder. Standalone-clean.

## Vault mapping

- **Raw layer:** `notes/capacities/raw/` — extracted export contents,
  preserving the object structure; re-import deduplicates globally across all
  month partitions by `path` (ZIP-relative, title-derived). Because the ZIP
  entry mtime is mutable, `_created` (and therefore the target month) can
  shift between re-imports; the raw upsert scans all existing partitions
  before writing to prevent cross-month duplicates. When the parser is
  unparked the dedup key will switch to the stable frontmatter object `id`.
- **Contract layer:** `notes/capacities/YYYY-MM.jsonl` per the (pending)
  notes contract — expected shape: one row per note/object (`ts` = created
  or modified, `source`, `guid` = object id, `title`, `body` markdown,
  `tags[]`), object-type + collection refs in `extra`.
- **Dedupe (contract layer, once unparked):** stable frontmatter object `id`
  as `guid`. Until then, raw layer uses `path` (best-effort; changes on
  rename).

## Build plan

**SHIPPED (scaffolded, raw layer):**
1. `DEF` is `Behavior::Import(&IMPORT)`, accepts `zip`. Registration line in
   `INTEGRATIONS` already existed (was a NotWired stub).
2. **Raw layer:** `notes/capacities/raw/YYYY-MM.jsonl` — every `.md` entry
   from the export ZIP as `{source, path, content, zip_mtime}`, deduped by
   `path`, partitioned by ZIP entry mtime. Unconditional, full fidelity.
3. **Contract layer (PARKED):** `notes/capacities/YYYY-MM.jsonl` —
   structurally complete (`upsert_capacities_notes`) but the parser
   (`parse_object`) returns `None` until a real export ZIP is inspected.
4. 7 tests green, `cargo check` clean.

**To unpark the contract parser:**
- Obtain a real Capacities export ZIP.
- `unzip -l <file.zip>` to confirm directory structure.
- Open one `.md` file; note the exact YAML frontmatter field names for id,
  created date, modified date, and object type.
- Replace the `parse_object` stub in `capacities.rs` with a real
  frontmatter parser (Obsidian's `parse_frontmatter` helper is reusable).
- Add fixtures from the real export; update the `contract_layer_parked`
  test to assert `Some(...)`.
- Set `parser_parked_needs_sample=false` in the INDEX build result.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export import (raw) | ✅ built | drop a real Capacities export ZIP in the import box; confirm rows in `notes/capacities/raw/` |
| Export import (contract) | ⏸ parked | unpark `parse_object`, then drop a ZIP; confirm rows in `notes/capacities/YYYY-MM.jsonl` |
| Re-import dedup | ✅ tested | drop a second overlapping export; confirm no duplicate rows in raw layer |
| Hub last-data | ⏸ parked | shows once contract layer is unparked (raw layer stem visible in filesystem) |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Capacities (L2867–L2873). Feasibility 🟢 high. The standout is the
local-export-to-disk feature (no cloud round-trip) — the standalone-clean
watch-folder pattern beats the API. ZIP internal format is undocumented in
the research doc → parser-last, Needs-sample.
