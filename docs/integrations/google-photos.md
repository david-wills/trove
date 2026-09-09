# Google Photos

- **id:** `google-photos`
- **domains:** `photos/` (contract: **Phase 3 pending** — photos-metadata,
  drafted from Apple Photos + Takeout + EXIF together). Metadata only, never
  image copies.
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-initiated Takeout ZIP drop; no automatable pull
  exists)
- **connection:** none. The Google connection (`google`, six defs) is **not**
  reused here — the Library API read scopes are dead (see below), so there is
  nothing to OAuth into.
- **evidence:** official export — takeout.google.com (Google Photos ZIPs with
  per-item JSON sidecars); community-documented sidecar quirks —
  google-photos-exif, metadatafixer.com (medium-high confidence, widely
  reproduced)
- **effort / priority:** M / P2
- **needs:** privacy (sidecars carry GPS `geoData` = location trail + `people`
  face tags — opt-in with explicit acknowledgement)

## What it is

Google's cloud photo library — for many users their *primary* photo archive,
with a decade-plus of timestamps, GPS points, and face tags. The metadata is
a dense personal timeline (where you were, who you were with). The only
remaining bulk path is the Takeout export; Google permanently revoked
library-wide API reads on 2025-03-31.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Photo/video metadata | all accounts (Takeout) | title (filename), description (user caption → contract `title`), creationTime, photoTakenTime, googlePhotosOrigin, url | official export, research L3098–L3105 |
| Geo data | per-item, when present | geoData {lat, lon, altitude, spans} | official export |
| People / face tags | per-item, when user-tagged | people[] names | official export |
| Album membership | all accounts | separate album-metadata JSON files (not in per-photo sidecars) | research L3104 |
| Live API sync | **none** | Library API read scopes revoked 2025-03-31; Picker API returns user-picked items only, no GPS/dates | research L3106–L3113 |

All optional in the contract (omit-if-empty); items without GPS or people
tags simply carry no such fields.

## Access & auth

- takeout.google.com → Google Photos → ZIP(s). Large libraries split into
  **multiple ZIPs** — the import box must accept several archives as one run.
- Format: original media + per-item JSON sidecar. **Sidecar JSON is
  authoritative** for dates/GPS — EXIF in the files is often stripped or
  wrong. Sidecar naming has edge cases: `IMG_1234.jpg.json`,
  `IMG_1234.json`, `IMG_1234(1).json` for duplicates (per google-photos-exif
  / metadatafixer).
- No auth, no TCC, no network. Standalone-clean. Do **not** plan an API
  connector: the regression is permanent (research cross-cutting note 5).

## Vault mapping

- **Raw layer:** `photos/google-photos/raw/YYYY-MM.jsonl` (partitioned by
  photoTakenTime) — the sidecar JSON objects, full fidelity, plus album
  membership from the album-metadata files.
- **Contract layer:** `photos/google-photos/YYYY-MM.jsonl` per the
  photos-metadata contract — `ts` = photoTakenTime (falls back to
  creationTime), `guid` = sidecar `url` (stable per item; falls back to
  filename + best-available timestamp), `title` = sidecar `description`
  (the user-authored caption, per contract spec), `filename` = sidecar
  `title` (the media file name), geo fields, `people[]`, overflow in
  `extra` (including `tz_unknown=true` because Google Takeout exposes
  UTC epoch only). **Never copy image bytes into the vault** — metadata
  only; the media stays wherever the user unpacked it.
- **Dedupe:** guid as above makes re-imports of overlapping Takeouts safe.

## Build plan

1. Module `crates/trove-core/src/google_photos.rs`: `DEF`
   (Behavior::Import), generic import box handles the rest.
2. Sidecar resolver that handles all three naming variants + multi-ZIP
   inputs; album-metadata pass second.
3. Fixtures: hand-built sidecar variants (with/without geoData and people;
   each naming form; a duplicate `(1).json`). Parser + store tests, unique
   temp dirs.
4. Privacy gate: opt-in with explicit acknowledgement (GPS trail + face
   tags).
5. UI copy on the card: explain that Google killed API access and link the
   user to takeout.google.com (the honest "why is this manual" answer).
6. Parked behind the Phase 3 photos-metadata contract for the contract
   layer; raw import can land first (full fidelity first, normalize second).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Sidecar parse (all naming variants) | ✅ unit tests green | fixture ZIPs through the import box; confirm rows + correct ts/GPS |
| Multi-ZIP import | ✅ unit test two_zips_same_photo_dedupes | real Takeout from a large library split across ZIPs; one run, no dupes |
| Album membership | ✅ raw-only (no Photo row; albums.jsonl written) | confirm album names land in raw from a real export |
| Re-import dedupe | ✅ unit test re_import_same_zip_adds_zero_rows | import the same Takeout twice; row count unchanged |
| GPS sentinel (0,0) → no lat/lon | ✅ unit test gps_zero_sentinel_yields_no_lat_lon | — |
| geoDataExif fallback | ✅ unit test geodataexif_fallback_when_geodata_is_zero | — |
| People/face tags | ✅ unit test rich_sidecar_yields_gps_people_and_title | — |
| Caption → title mapping (description → Photo.title, filename stays Photo.filename) | ✅ unit test description_maps_to_title_not_extra | — |
| Enrichment-less album JSON → albums.jsonl | ✅ unit test enrichment_less_album_routes_to_albums_jsonl | — |
| creationTime fallback guid (photoTakenTime absent) | ✅ unit test guid_uses_creation_time_when_photo_taken_time_is_zero | — |
| tz_unknown flag in extra | ✅ unit test tz_unknown_flag_present_in_extra | — |
| Corrupt JSON in ZIP | ✅ unit test corrupt_json_in_zip_is_skipped_not_fatal | — |

## Research notes

`integrations-research.md` → "Photos & Visual Media" §Google Photos Takeout
(L3098–L3105, 🟡 medium) and §Library API / Picker API (L3106–L3113, 🟠 low
→ skip). Cross-cutting notes L3156: metadata-only stance, API regression is
permanent (gphotos-sync and peers all broke March 2025), generic
drop-ZIP-here import pattern hosts this parser. One-shot import; cannot be
automated — no API to trigger a new Takeout.
