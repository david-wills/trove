# Shazam

- **id:** `shazam`
- **domains:** `media/shazam/` (raw-only — Shazams are discovery *tags*, not
  play events; they never join the media-plays contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (read the local SQLite DB; copy-then-read; watermark
  on ROWID)
- **connection:** none (local DB needs no auth; the optional privacy-export
  backfill is a file the user requests from shazam.com themselves)
- **evidence:** community-documented schema — `ShazamDataModel.sqlite`,
  table `ZSHTAGRESULTMO` (stable, simple); official privacy export
  (`SyncedShazams.csv`) as a documented backfill format
- **effort / priority:** S / P2
- **needs:** none

## What it is

Apple's music-identification app. Every "what song is this?" moment is a
timestamped discovery event — a log of when and where the user *encountered*
music, distinct from listening history. iCloud sync means Shazams made on
the iPhone (including Control Center Music Recognition) appear in the Mac
app's local DB within minutes, so a Mac-side collector captures the whole
account's tags without any cloud call.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Tag history (live, local DB) | none | ZTRACKNAME/ZTITLE, artist, ZDATE (Core Data epoch), ZLATITUDE/ZLONGITUDE | community schema (4 independent sources) |
| Tag history (backfill, privacy export) | none — requires Shazam account login on their site | song, artist, date/time, Shazam link (`SyncedShazams.csv`) | official export |

All optional in the contract sense; the DB read alone is a complete
integration — the CSV is belt-and-braces backfill for users whose iCloud
sync was off historically.

## Access & auth

- Local SQLite at
  `~/Library/Containers/com.shazam.mac.Shazam/Data/Documents/ShazamDataModel.sqlite`
  (group-container variant: `~/Library/Group Containers/*.group.com.shazam/`).
  Requires the macOS Shazam app installed and iCloud sync enabled.
- FDA (already required by other Trove collectors); read-only copy before
  querying, like the shipped podcasts/imessage collectors.
- Privacy export: shazam.com/privacy → "Download Your Data" → email
  delivery of JSON/CSV. Manual, occasional.
- ShazamKit (the developer audio-recognition framework) is irrelevant here.
- Standalone-clean: no network, no external app dependency at runtime
  beyond the user's own Shazam install being the data source.

## Vault mapping

- **Raw layer:** `media/shazam/YYYY-MM.jsonl` — one row per tag, full DB
  fidelity (title, artist, ts, lat/lon where available). Research-entry path
  predates the taxonomy; the taxonomy's media-curation rule puts per-source
  non-play streams at `media/<source>/`.
- **Contract layer:** none — discovery tags are not plays. A read-time view
  may later join tags against `media/plays/` ("first heard → first
  listened"), but no write-time contract applies.
- **Dedupe:** ROWID-watermark (same mechanism as the iMessage collector —
  iCloud backfill inserts old-timestamped rows with fresh ROWIDs, so a
  ROWID cursor never misses them). The `shazam_id` field is emitted where
  the DB schema includes a ZSHAZAMID column (Needs-sample confirmation);
  it is left empty otherwise and is not relied on for dedupe.

## Build plan

1. Module `crates/trove-core/src/shazam.rs`: `DEF` (Periodic; permission
   hook checks FDA + DB-path existence so the card greys honestly when the
   Shazam app isn't installed).
2. Registration line in `INTEGRATIONS`. No connection.
3. Copy-then-read the SQLite (WAL-safe), watermark cursor in
   `.trove/shazam-sync.json`, rebuildable from output files.
4. Fixtures: a synthetic `ShazamDataModel.sqlite` with known rows; Core
   Data epoch (seconds since 2001-01-01) conversion tests.
5. Follow-on (separate small def if wanted, since one def = one Behavior
   shape): `SyncedShazams.csv` import via the generic import box for
   pre-iCloud backfill.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Local DB tags | 🧪 built | Shazam a song on iPhone; within minutes run Sync now; confirm the row in `media/shazam/YYYY-MM.jsonl` + hub last-data |
| Privacy-export backfill | — | request export at shazam.com/privacy, drop `SyncedShazams.csv` on the import box (separate def, future work) |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Shazam
(L3278–L3284). Feasibility 🟢 high — simple, stable schema; FDA already in
Trove's permission set. Low effort, high signal: discovery history exists
nowhere else. No time-sensitivity.

## Implementation notes (built 2026-06-17, corrected 2026-06-17)

- **Timestamp column:** `ZDATE` (not `ZTIMESTAMP`) confirmed by four independent
  community sources: Vaughan Harper verbatim SQL ("seconds since 2001-01-01 UTC"),
  sn3p/shazam-tags (`ORDER BY ZDATE`), sophiegblog gist (`ZDATE`+11323-day
  offset), TechTraumas (`zdate`). Apple epoch math unchanged.
- **Schema-adaptive:** Three column-name variants. **TrackSubtitle** (community-
  documented, 2017–2021): `ZTRACKNAME`+`ZSUBTITLE` in `ZSHTAGRESULTMO`, artist
  via `LEFT JOIN ZSHARTISTMO a ON a.ZTAGRESULT = t.Z_PK` (FK confirmed by
  Vaughan Harper, sn3p, sophiegblog). **TitleArtist** (post-2021 Mac app,
  Needs-sample): `ZTITLE`+`ZARTIST` directly on `ZSHTAGRESULTMO`. **Minimal**:
  neither recognised — advance cursor on `ZDATE` only. `PRAGMA table_info`
  at open time selects the right path; both variants tested with fixture DBs.
- **Artist JOIN key fixed:** `ON a.ZTAGRESULT = t.Z_PK` (ZSHARTISTMO carries
  the FK; `ON a.Z_PK = t.Z_PK` was wrong and would match no rows on a real DB).
  Fixture now uses deliberately divergent Z_PK values so the test validates the
  correct key path.
- **Location data:** `ZLATITUDE`/`ZLONGITUDE` read from `ZSHTAGRESULTMO`
  (confirmed by Vaughan Harper); NULL-safe — emitted as `null` when the column
  is absent or the value was not captured.
- **ZSHAZAMID:** Included in the TitleArtist path where the column is present
  (Needs-sample confirmation it exists in modern builds); empty for TrackSubtitle
  and Minimal paths where no source documents it. Not used for dedupe.
- **Watermark:** ROWID cursor (not timestamp), matching iMessage. iCloud
  backfill inserts old-timestamped rows with fresh ROWIDs.
- **Raw-only:** `media/shazam/YYYY-MM.jsonl`, month-partitioned by local tag
  time. No contract layer.
- **8 passing tests:** epoch conversion, TitleArtist schema round-trip,
  TrackSubtitle schema with artist JOIN (ZTAGRESULT key, divergent Z_PKs),
  incremental watermark (no duplicates), zero-ZDATE skip, cursor rebuild from
  JSONL, schema detection, serde back-compat for sparse rows.
- **CSV backfill** (`SyncedShazams.csv`): noted as a separate def/import box
  (not built here — distinct Behavior shape, low priority, Needs-sample).
