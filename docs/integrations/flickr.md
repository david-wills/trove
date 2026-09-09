# Flickr

- **id:** `flickr`
- **domains:** `photos/` (contract: **Phase 3 pending** — photos-metadata).
  Metadata only, never image copies.
- **status:** 🧪 built (parser-parked/needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (account data export ZIP — v1 path, all accounts).
  Possible later upgrade: Periodic API pull for continuous sync (Pro-only,
  BYO key) — same def family, second iteration.
- **connection:** none for the v1 export path. API upgrade would add a
  `flickr` connection (OAuth 1.0a + TokenPaste for the user-supplied API
  key — Flickr API keys are Pro-only, so a compiled-in credential helps no
  one on free accounts).
- **evidence:** official-docs — flickr.com/services REST API (alive,
  documented); official export available to all accounts. No Rust crate
  (flickcurl is C; OAuth 1.0a via the `oauth1` crate).
- **effort / priority:** M / P2
- **needs:** none

## What it is

Long-running photo-hosting service; the audience is photographers with
years-deep libraries (often pre-dating smartphone photos), rich tags, album
structure, and per-photo GPS. Niche today but the archives are old and
irreplaceable — exactly the kind of history a vault should hold.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Photo metadata (export) | all accounts | JSON sidecars: id, title, tags, dates, GPS (when present), album membership | official export, research L3122–L3129 |
| Original media files (export) | all accounts | originals in the ZIP (not copied to vault — metadata only) | research L3128 |
| Photo metadata (API) | **Pro subscription** for the API key | flickr.photos.search w/ user_id=me: id, title, tags, dates, GPS if user-enabled, views, faves | official-docs |

All optional in the contract; free-account users get the full export path,
no special code for the missing API tier.

## Access & auth

- **Export (v1):** Settings → "Request my Flickr Data" → ZIP (original files
  + JSON sidecar metadata, same EXIF-passthrough pattern as Takeout).
  Processing takes hours to weeks — UI copy should set that expectation.
  No auth inside Trove, no TCC, no network. Standalone-clean.
- **API (upgrade):** `api.flickr.com/services/rest/`, OAuth 1.0a; API key
  requires a Pro subscription, so the connect card must take a user-supplied
  key (TokenPaste) — per the SimpleFIN affordance rule, the gated state
  carries an inline hint about the Pro requirement.

## Vault mapping

- **Raw layer:** `photos/flickr/raw/YYYY-MM.jsonl` (partitioned by taken
  date) — sidecar JSON objects full-fidelity; album membership preserved.
- **Contract layer:** `photos/flickr/YYYY-MM.jsonl` per the pending
  photos-metadata contract — `ts` = date taken, `guid` = Flickr photo id,
  `title`, `tags[]`, geo fields, overflow (views, faves, albums) in `extra`.
- **Dedupe:** photo id as `guid` — makes export re-imports and a later API
  upgrade write into the same stream without duplication.

## Build plan

1. Module `crates/trove-core/src/flickr.rs`: `DEF` (Behavior::Import);
   generic import box hosts the ZIP parser.
2. Fixtures from export sidecar shapes (with/without GPS, tags, albums);
   parser + store tests, unique temp dirs.
3. **Needs-sample caveat:** export sidecar layout is community-understood
   but not formally specced — confirm against one real export before
   declaring built; parser-last if no sample surfaces.
4. Later iteration (separate loop item, only on demand): `CONNECTION` with
   OAuth 1.0a + TokenPaste BYO key, Periodic pull via flickr.photos.search,
   cursor on last upload date. Same guid stream as the import.
5. Contract rows land once photos-metadata is ratified; raw import can ship
   first.

## Build notes (fan-out, 2026-06-17)

- Module replaced: `crates/trove-core/src/flickr.rs` fully implemented (was NotWired stub).
- Behavior: `Import` (ZIP file drop). Accepts `zip`.
- Raw layer: unconditional — every JSON file in the ZIP is written to `photos/flickr/raw/<stem>.jsonl`.
- Contract layer: scaffold fires and produces `Photo` rows via real-export field mapping (`id`, `name`, `description`, `date_taken`, `albums`, `tags`, `geo`). **PARKED / Needs-sample** — real-export field shapes confirmed from community analysis; real ZIP needed to finalize.
- `guid` = `"flickr:<id>"` (the photo id from the sidecar). Sidecars missing `id` are skipped for the contract layer but still written raw.
- `ts` = parsed from `date_taken` (`"YYYY-MM-DD HH:MM:SS"` or with nanosecond suffix); Flickr sentinel `"0000-00-00 00:00:00"` treated as absent; fallback to `date_imported` as STRING `"YYYY-MM-DD HH:MM:SS"` (NOT Unix epoch integer — real export shape); secondary fallback to i64 epoch for non-standard tools.
- GPS from `geo.latitude` / `geo.longitude` as STRING microdegree integers (e.g. `"49696401"` → 49.696401°, divides by 1_000_000); float decimal degree values also accepted.
- `tz_unknown=true` set in `extra` when `date_taken` is used as `ts` (Flickr exports carry no UTC offset — interpreted as UTC for portability/idempotency).
- Overflow (`count_views`, `count_faves`, `count_comments`, `count_tags`, `license`, `safety_level`, `url`, `original_format`) → `extra`.
- Re-import dedupe: guids tracked, second run adds 0 rows.
- 10 unit tests pass (added `sentinel_date_taken_falls_back_to_string_date_imported` + `geo_microdegree_string_parsing`); `cargo check` green.
- No new dependency (zip, sha2, chrono already in Cargo.toml).
- No new ConnectionDef; no CONNECTIONS line needed.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export ZIP import | 🧪 scaffold | Request a real Flickr export, drop into the import box, confirm rows + GPS/tags in `photos/flickr/` and hub last-data. Verify exact JSON field names against real sidecar (may require parser tweaks). |
| Re-import dedupe | ✅ unit-tested | Import the same ZIP twice; row count unchanged (verified in tests). |
| Parser field names | ⚠️ Needs-sample | Real export needed to confirm `name`/`date_taken`/`geo`/`tags`/`albums` key names. Parser parked until confirmed. |
| API pull (Pro) | — | Requires a Pro account + user-supplied key — not yet implemented (planned as a later upgrade). |

## Research notes

`integrations-research.md` → "Photos & Visual Media" §Flickr (L3122–L3129),
🟡 medium. Export is the better first-version path (all accounts); API is
the continuous-sync upgrade but Pro-only keys make it friction-heavy.
Free accounts can't bulk-download >1024px via API, but the export includes
originals — moot for Trove since we never copy image bytes anyway.
SmugMug follows the near-identical pattern (OAuth 1.0a, BYO key); building
Flickr first makes SmugMug a low-effort follow-on.
