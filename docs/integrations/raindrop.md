# Raindrop.io

- **id:** `raindrop`
- **domains:** `reading/` — **reuses the `reading/` contract bound by
  `readwise` (INDEX #24)**; reads `crate::reading::Item` (no struct / `DOMAINS`
  / `spec_validation` change — not first-in-domain).
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-login**
- **unavailable_reason:** none
- **behavior:** `Behavior::Periodic` — hourly (`RAINDROP_SYNC_SECS = 3600`),
  every-on-run cadence (the timer only advances when it actually runs, so
  re-enabling fires immediately). Drains `GET /rest/v1/raindrops/0` page by
  page (perpage=50) newest-first; an incremental run stops early once a page's
  items are all `lastUpdate ≤` the stored cursor.
- **connection:** `raindrop` — TokenPaste (a personal **test token** from
  **app.raindrop.io/settings/integrations** → "For Developers" → "Create test
  token"; free tier, no OAuth dance), Bearer auth, verified at connect with a
  real 1-item fetch, stored 0600 at `.trove/sync/raindrop`. Not shared with
  other defs.
- **default:** off (`default_on: false`) — a Needs-login cloud sync, off until
  the user pastes a token; the def is toggleable once connected.
- **evidence:** official-docs — developer.raindrop.io (documented REST API,
  active service); raindrop-io-py and others as reference implementations.
  **Field-map confirmed from primary docs before parse: `_id` (not `id`),
  `link` (not `url`), `lastUpdate`, `collection.$id`/`title`.**
- **effort / priority:** S / P2
- **needs:** none — the reading contract is already ratified; live validation
  needs a real Raindrop personal token (a Needs-login item, no app
  registration).

## What it is

Bookmark manager with collections, tags, and notes — one of the main
landing spots for users displaced by the Pocket shutdown (July 2025), so
the user base is large and growing. Yields the curated-links layer of web
activity: what the user chose to keep, organized how they think.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Bookmarks (API) | free tier (personal token, read) | id, url, title, tags, note, created, lastUpdate, collection, cover | official docs, research L1560–L1567 |
| Backup export (CSV/HTML/TXT) | always available in Settings → Backup; some backup features Pro-gated | url, title, tags, folders | research L1565–L1567 |

All optional in the contract; no tier-specific code paths — the free-tier
personal token covers the whole read path.

## Access & auth

- REST: `GET https://api.raindrop.io/rest/v1/raindrops/{collectionId}`
  (collection 0 = all), Bearer token. OAuth 2.0 exists but the personal
  test token makes TokenPaste the right method for a personal vault.
- Export fallback: Settings → Backup → Export (HTML/CSV) — feed it to the
  same parser via the generic import box as a no-login path.
- No published rate-limit concerns at personal scale.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `reading/raindrop/raw/YYYY-MM.jsonl` — API raindrop
  objects full-fidelity, partitioned by `created`.
- **Contract layer:** `reading/raindrop/YYYY-MM.jsonl` per the pending
  reading contract — `ts` = created, `guid` = raindrop id, `url`, `title`,
  `tags[]`, note; collection name + cover in `extra`.
- **Dedupe:** raindrop id as `guid` (URL hash for export-file rows that
  lack ids — reconcile at the API pull, same stream); `lastUpdate` cursor
  in `.trove/raindrop-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/raindrop.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: label/help/placeholder pointing at the
   developer-integrations settings page, per the SimpleFIN affordance
   rule), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from documented response shapes (with/without note, tags,
   collection); parser + store + cursor tests, unique temp dirs.
4. Optional same-module Import path for the CSV/HTML backup (no-login
   onboarding + Pro-gated-backup escape hatch).
5. Contract rows once the reading contract is ratified; raw can ship first.

## Validation matrix

Promotion to ✅ needs David's real token (Needs-login).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect + auth | 🧪 fixture | `connection_exposes_token_paste_method`, `connection_stores_token_0600_and_absent_from_cursor`, `empty_token_rejected_and_pull_needs_connection`. **David:** open **app.raindrop.io/settings/integrations** (signed in) → under **"For Developers"** click **"Create test token"** → copy it → paste into the **Raindrop.io** connect card → it verifies with a real 1-item fetch (Bearer auth) and stores 0600 at `.trove/sync/raindrop`. A **free-tier** account suffices. |
| Bookmark pull | 🧪 fixture | `full_pull_writes_both_layers_and_advances_watermark`, `maps_full_raindrop_to_item`, `maps_minimal_raindrop_omits_empty_fields`, `starred_bookmark_maps_to_favorite_state_and_note_fallback`, `multi_page_pull_drains_all_pages`. **David:** enable the **Raindrop.io** toggle (default-off) → **Sync now** → confirm `Item` rows in `reading/raindrop/YYYY-MM.jsonl` (url + title + tags; `state` = `favorite` for starred items else `saved`; collection name / cover / note in `extra`) + the lossless `reading/raindrop/raw/` mirror + the hub "last data" date. |
| Incremental cursor (`lastUpdate`) | 🧪 fixture | `incremental_stops_at_cursor`, `parse_page_short_page_is_last`, `cursor_back_compat_empty_and_partial_deserialize`. **David:** add a bookmark in Raindrop → **Sync now** again → exactly one new row appears, and the run stops paging once it reaches already-seen items (it does not re-walk the whole archive). |
| Secret hygiene | 🧪 fixture | `connection_stores_token_0600_and_absent_from_cursor` (asserts 0600 mode on the stored token), `full_pull_writes_both_layers_and_advances_watermark` (asserts the token never lands in `.trove/raindrop-sync.json`). **David:** none — automated. |
| Backup CSV/HTML import | deferred | not built in this pass — the API read path covers the whole free tier. A same-module import for the Settings → Backup export is a clean follow-up slice (logged in the journal). |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption"
§Raindrop.io (L1560–L1567), 🟢 high, "build now". Personal tokens keep auth
trivial; the Pro paywall touches only some backup/export features, not API
reads. Post-Pocket migration makes this the highest-population bookmark
manager in the catalog — sequence it early among the P2 reading sources,
right after Readwise exercises the contract.
