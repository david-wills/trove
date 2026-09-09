# Apple Photos

- **id:** `apple-photos`
- **domains:** `photos/` (contract: **Phase 3 pending** — photos-metadata,
  drafted from Apple Photos + Google Photos Takeout + EXIF import together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read the library SQLite on each sync)
- **connection:** none — local files under the existing Full Disk Access
  grant.
- **evidence:** community-schema, high confidence — actively
  reverse-engineered by osxphotos, theforensicscooter, dogsheep-photos;
  psi.sqlite UUID int-pair join documented (dogsheep-photos issue #16,
  forensic researchers); ZSHARE/ZSHAREPARTICIPANT documented.
- **effort / priority:** M / P1
- **needs:** privacy (GPS geotags are a location trail; faces/people data —
  opt-in with explicit acknowledgement)

## What it is

The richest local photo source on a Mac. `Photos.sqlite` holds per-asset
metadata for the whole library — timestamps, GPS, favorites, albums, face/
people clusters, Apple's ML quality scores — and the companion `psi.sqlite`
holds already-computed CoreML scene/object labels (dog, beach, sunset…) for
every photo, free of any inference cost. Geotags are the single best
location-history proxy Trove has; faces/people data is unique. **Metadata
only — Trove never duplicates the image store.**

## Capabilities (what data it can yield)

One collector, four passes (the research doc's four entries combine here):

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Library metadata (Photos.sqlite) | none | per-asset filename, dates, GPS lat/lon/altitude, favorite/hidden/burst flags, dimensions, albums, faces/people, ML quality scores | community schema (osxphotos et al.) |
| ML scene/object labels (psi.sqlite) | none | word/extended_word labels per photo UUID | community schema (forensic researchers) |
| iCloud Shared Photo Library | macOS 13+ feature | share creation date, owner, up to 5 participants, per-asset contributor attribution | community schema (ZSHARE tables) |
| Live/Cinematic/Spatial flags | none | boolean flags per asset (extra ZASSET columns) | community schema |

All optional in the contract; a library without shared-library or spatial
assets simply yields no such rows.

## Access & auth

- `~/Pictures/Photos Library.photoslibrary/database/Photos.sqlite` and
  `database/search/psi.sqlite` — copy-then-read (same WAL pattern as
  Safari/iMessage), read-only, rides troved's existing FDA grant. No new
  permission prompt.
- Key tables: ZASSET/ZGENERICASSET, ZADDITIONALASSETATTRIBUTES,
  ZCOMPUTEDASSETATTRIBUTES, ZPERSON/ZDETECTEDFACE, Z_26ALBUMS/Z_26ASSETS,
  ZSHARE/ZSHAREPARTICIPANT.
- Gotcha: album-table prefix integers (Z_26…) increment each macOS major
  release — probe Z_PRIMARYKEY at open time, as osxphotos does.
- psi.sqlite photo UUIDs are two signed int64 halves of the 128-bit UUID —
  byteswap/format to join against Photos.sqlite UUID strings.
- Multiple `.photoslibrary` bundles possible (scan `~/Pictures/`); note the
  System Photo Library setting (one iCloud-synced library + extras).
- Standalone-clean: no network, no external process.

## Vault mapping

- **Raw layer:** `photos/apple-photos/assets/YYYY-MM.jsonl` (partitioned by
  capture month) — one row per asset, full metadata fidelity including ML
  labels, face/people cluster names, share attribution, flags. Library/
  album structure snapshots in `photos/apple-photos/albums.jsonl`. **Never
  image bytes or copies.**
- **Contract layer:** photos-metadata contract is Phase 3 pending; expected
  shape (one row per asset: `ts`, `source`, `guid` = asset UUID, GPS,
  device, labels, overflow in `extra`) to be drafted alongside Google
  Photos Takeout + EXIF import. Until ratified, raw layer only.
- **Dedupe:** asset UUID as `guid`; sync re-reads are idempotent.

## Build plan

1. Module `crates/trove-core/src/apple_photos.rs`: `DEF` (Periodic),
   copy-then-read helper reuse, schema-probe at open (Z_PRIMARYKEY) so a
   macOS bump degrades to a clear error, not silent garbage.
2. Pass 2: psi.sqlite labels with the UUID int-pair join; probe table
   structure at open (schema may shift across macOS versions).
3. Passes 3–4 (ZSHARE join, Live/Cinematic/Spatial columns) are extra
   columns/joins in the same read — near-zero incremental effort.
4. Registration line in `INTEGRATIONS`; registry provides hub card,
   toggle, Sync-now, Recent-data.
5. Fixtures: hand-built miniature Photos.sqlite + psi.sqlite capturing the
   prefix-probe and UUID-join cases; tests in unique temp dirs.
6. Privacy gate: ships opt-in (location trail + faces) with explicit
   acknowledgement on enable.
7. Contract rows wait on Phase 3 photos-metadata ratification (raw layer
   ships first — full fidelity first, normalization second).

## Build notes (2026-06-16)

- **Contract mode:** `raw-only` — the photos-metadata contract (Phase 3) is
  still pending; we write `photos/apple-photos/YYYY-MM.jsonl` via the existing
  `Photo` type, which exactly matches the planned contract shape.  No contract
  struct, DOMAINS entry, or spec_validation row added.
- **psi UUID encoding verified:** the int64 pair is **little-endian**, not
  big-endian (confirmed against the real psi.sqlite on disk; the dogsheep
  issue #16 description of a big-endian variant does not match this library).
- **Camera make/model absent:** Photos.sqlite doesn't expose camera make/model
  as plain columns (they live in a serialized plist blob in
  ZCLOUDMASTERMEDIAMETADATA); those fields are left empty.  The EXIF import
  collector (`exif-import`) covers that data path for loose files.
- **Shared library (ZSHARE/ZSHAREPARTICIPANT):** ZSHARE exists in the real DB
  (13 rows on the test machine) but the contributor-attribution join would
  require ZASSETCONTRIBUTOR, which is empty here.  Shared-library attribution
  rows land in the `extra` map if the schema carries them; not an error if
  absent.
- **15 unit tests:** all pass, covering epoch conversion, UTI/MIME mapping,
  kind detection, GPS omission, album/person joins, psi label loading, dedupe,
  back-compat deserialization, and the no-library no-op path.
- **Adversarial-review fixes (2026-06-16):**
  - psi join key corrected from `g.owning_groupid = ga.groupid` to
    `g.rowid = ga.groupid` (owning_groupid is a separate parent-group pointer,
    not the join target; osxphotos and dogsheep-photos both use rowid).
    Test fixture rebuilt with explicit rowids distinct from owning_groupid so
    the wrong join would yield zero rows.
  - NUL bytes stripped from psi content_string before storing (trailing `\x00`
    causes dedup/compare failures; dogsheep applies the same fix).
  - Asset table name detected at open time via sqlite_master (`ZASSET` for
    Ventura+, `ZGENERICASSET` for older macOS) rather than hard-coded.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Library metadata | ✅ built | enable on a real library; Sync now; spot-check a known photo's row (date, GPS, album) in `photos/apple-photos/` against Photos.app Info panel |
| ML labels | ✅ built | pick a photo with an obvious subject (dog/beach); confirm matching label words on its row under `extra.labels` |
| Shared library | ⚠️ best-effort | requires an iCloud Shared Photo Library participant account; contributor attribution only if ZASSETCONTRIBUTOR is populated |
| Live/Video flags | ✅ built | a Live Photo carries `kind:"live"`; a video carries `kind:"video"` and `duration_secs` |

## Research notes

`integrations-research.md` → "Photos & Visual Media" §§Apple Photos
(L3042–L3073, four entries). Feasibility 🟢 high across all four. Research
catalog framing carried whole; the taxonomy routes everything to `photos/`
(Phase 3 photos-metadata). The schema-probe requirement is the standing
maintenance cost: every macOS major release may shift table prefixes.
Time-sensitivity: low — the library is durable; deleted photos' metadata
is lost on purge, so periodic sync (not import-once) is the right shape.
CLIP embeddings and Vision OCR enrichment are separate catalog entries
(`clip-embeddings`, `macos-screenshots`), not part of this collector.
