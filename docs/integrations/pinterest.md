# Pinterest

- **id:** `pinterest`
- **domains:** `social/` (pins/boards are saves/curation — they stay
  per-source raw under `social/pinterest/` per the taxonomy; social-posts
  contract is **Phase 3 pending** for anything post-shaped)
- **status:** 🧪 built (scaffold/raw only; parser parked — needs real export sample)
- **unavailable_reason:** none
- **behavior:** Import (data-download ZIP; API v5 incremental sync is a
  possible later upgrade)
- **connection:** none for the import path. A future `pinterest` connection
  (OAuth via API v5, requires developer-account registration) could add
  incremental board/pin sync — out of scope here.
- **evidence:** official-docs — official data download (Settings > Privacy
  and Data > Request your data; metadata only, images as CDN URLs) +
  Pinterest API v5 (developers.pinterest.com)
- **effort / priority:** S / P2
- **needs:** none

## What it is

Visual bookmarking: boards of saved pins representing the user's interests,
plans, and taste over time. The data that matters is the curation itself —
board names, pin descriptions, and link targets. Niche for Trove's
purposes, but cheap to support.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Boards | none | board metadata (names, structure) | official export |
| Pins | none | pin URLs (Pinterest CDN), descriptions, link targets | official export |
| Social graph | none | follower/following lists | official export |
| Account info | none | profile data | official export |

The export is notably thin: **metadata only, no images** — pin URLs point
at Pinterest's CDN. All fields optional in any future contract mapping.

## Access & auth

- Export: Settings > Privacy and Data > Request your data → email with a
  ZIP link within ~48 hours.
- API v5 (later): OAuth, `GET /boards`, `GET /pins`; free but requires a
  developer account — registration friction makes it a follow-up, not the
  M1 path.
- No TCC, no local files. Standalone-clean (user drags a ZIP in). Trove
  does **not** fetch the CDN images — that would be a networked enrichment
  step and the export's own scope is metadata; revisit only as an explicit
  opt-in if users ask.

## Vault mapping

- **Raw layer:** `social/pinterest/raw/` — the export's board/pin/graph
  files as shipped, one flat JSONL file per CSV/JSON section (e.g.
  `boards.jsonl`, `pins.jsonl`, `followers.jsonl`). Content-hash dedupe
  (scoped per section) makes re-imports idempotent. Date-partitioning is
  not applied at the scaffold stage since the export format is undocumented
  and no real sample is available; revisit when a real export confirms field
  names and any embedded date field that could anchor partitioning.
- **Contract layer:** none yet — pins are saves/curation, which the
  taxonomy keeps per-source raw under `social/<source>/` (saved posts never
  route to `reading/`). If the Phase 3 social-posts contract ends up
  covering save-shaped rows, map then; don't pre-normalize.
- **Dedupe:** pin id (or pin URL where id is absent) as `guid`; board name
  carried on each row.

## Build plan

1. Module `crates/trove-core/src/pinterest.rs`: `DEF` with
   `Behavior::Import` (letterboxd.rs reference shape). One line in
   `INTEGRATIONS`; no connection.
2. Parser for the export's metadata files; fixtures from a real export
   (format is officially produced but the exact file layout should be
   confirmed against a sample during the build — order parser work after
   the ZIP-handling scaffolding).
3. Slot into the shared social-archive-importer detection layer (drag ZIP →
   auto-detect platform) alongside the other archive platforms.
4. UI copy should set expectations: boards + pin links import; images do
   not (honest about the export's thinness).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Boards + pins import | scaffold green (unit tests pass with synthetic CSV) | request a real export, wait for the 48h email, drag the ZIP in, confirm rows under `social/pinterest/raw/` + hub last-data |
| Social graph | scaffold green | same export; confirm follower/following lists landed in raw |

## Build notes (2026-06-16)

- **contract_mode:** raw-only — pins are saves/curation, not authored posts; they do not map to the `social` Post contract (which is for authored content per `social.rs`)
- **parser_parked_needs_sample:** true — no real Pinterest export sample available on disk; exact CSV column names are undocumented. Scaffold walks all ZIP entries, parses CSV with the header row as keys, writes JSON objects to `social/pinterest/raw/<section>.jsonl`, content-hash-deduped for re-runnable imports.
- **connection:** none for the import path (no login needed)
- **cargo_deps_added:** none (sha2/csv/zip already present)
- **touched_shared_contract_files:** false

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Pinterest
(L4080–L4086). Feasibility 🟡 medium — solely because the export is thin
(metadata + CDN URLs, no images), not because access is hard. API v5 could
power incremental sync later but needs developer registration. Third-party
scrapers (Pinback bookmarklet) exist as alternatives — not pursued; the
official export is the supported path.
