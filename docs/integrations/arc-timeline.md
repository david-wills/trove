# Arc Timeline

- **id:** `arc-timeline`
- **domains:** `location/` (contract: **Phase 3 pending** — trails shape now;
  ML-classified visits/trips await the visits-shaped contract)
- **status:** 🧪 built (raw preserved; Fix contract active — path samples mapped)
- **unavailable_reason:** none
- **behavior:** Import (user exports from the app; Trove parses the file)
- **connection:** none — user supplies an export file; no login/OAuth.
- **evidence:** community/folklore — Arc 4 export story unverified (spike
  needed); the old Arc iOS app exposed an iTunes-shared SQLite
- **effort / priority:** L / P2
- **needs:** privacy (location trail — opt-in with explicit acknowledgement)

## What it is

Arc Timeline (bigpaua.com, iOS-only) automatically classifies continuous GPS
into **visits** (with place names — "home", "office") and **trips** (with
detected transport mode). Its ML-classified place/trip data is materially
higher-quality than a raw GPS log — that classification is the whole reason to
want it over a plain GPX trail.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Visits | all | place name, coords, arrive/leave times | community (unverified for v4) |
| Trips | all | transport mode, path, start/end | community (unverified for v4) |
| Per-item GPX | all | trackpoints (lat/lon/ele/time) | historically supported |

All capability fields are optional in the contract (omit-if-empty).

## Access & auth

No public API. The current Arc Timeline 4 (2025 ground-up rebuild) has no
documented bulk export as of 2026 — the developer (Matt Greenfield / Big Paua)
has indicated export is on the roadmap. The older Arc iOS app kept a SQLite DB
at `~/Documents/Arc Timeline.sqlite`, shareable via iTunes file sharing; Arc 4
may have changed this. Historically Arc supported per-item GPX export but no
bulk path. No TCC concern — this is an Import of a user-supplied file.

## Vault mapping

- **Raw layer:** `location/arc-timeline/raw/…` — the exported file(s) at full
  fidelity (GPX trackpoints and/or the visit/trip records, whatever the export
  yields), partitioned by month.
- **Contract layer:** `location/arc-timeline/…` — GPS trails map to the
  existing `location/` trails shape now. ML-classified **visits** are the
  uniquely valuable slice but wait for the Phase 3 visits-shaped contract;
  until then they stay raw under the per-source folder.
- **Dedupe:** stable item id from the export as `guid` where present; else a
  hash of (start_ts, end_ts, coords).

## Build plan

1. **Spike first** (gating): determine what Arc 4 actually exports in 2026 —
   bulk SQLite? per-item GPX only? a new format? **Parser-last; flag
   Needs-sample.** ROI is low if only per-item GPX exists with no bulk path.
2. Module `crates/trove-core/src/arc_timeline.rs`: `DEF` (Import), an import
   hook over the confirmed export format, `store` writes via the location
   helpers.
3. Fixtures from a real export sample (required — no documented schema to
   build against blind); parser + store + dedupe tests, unique temp dirs.
4. Privacy gate: ships opt-in (location trail) with explicit acknowledgement
   on enable.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Visits / trips | — | obtain a real Arc 4 export; import; confirm visit/trip rows under `location/arc-timeline/` + hub last-data |
| GPX trails | — | import; confirm trail rows render on the location view |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Arc Timeline (iOS)
(L2316–L2322). Feasibility 🟡 medium — entirely gated on the export-format
spike. Big Paua is responsive to feature requests, so the export path may
improve. Do not commit build effort until a sample confirms a bulk export
exists.

## Build notes (2026-06-21)

- **SQLite schema verified** from LocoKit `TimelineStore+Migrations.swift`
  (github.com/sobri909/LocoKit). The `TimelineItem` table
  (columns: `itemId`, `isVisit`, `startDate`, `endDate`, `latitude`,
  `longitude`, `altitude`, `activityType`, `distance`, `stepCount`) and
  `LocomotionSample` table (columns: `sampleId`, `timelineItemId`, `date`,
  `secondsFromGMT`, `latitude`, `longitude`, `altitude`, `speed`, `course`,
  `horizontalAccuracy`, `classifiedType`, `confirmedType`) are the confirmed
  schema for the original Arc/LocoKit local-store format.
- **Import scaffold built**: accepts `.sqlite`/`.sqlite3`/`.db` files; detects
  LocoKit format via SQLite magic header + `TimelineItem` table probe; raw copy
  preserved verbatim under `location/arc-timeline/raw/locokit-sqlite-<hash>.sqlite`;
  idempotent (re-drop no-ops via content-hash naming).
- **Fix contract active**: `map_fixes` implemented — `PRAGMA table_info` probes
  both tables for optional columns (column-drift safe across LocoKit versions),
  SELECT non-deleted `LocomotionSample` rows joining `TimelineItem` WHERE
  `isVisit=0`. GRDB UTC `date` + nullable `secondsFromGMT` → RFC3339 (NULL →
  UTC Z fallback for pre-7.0.4 rows). Negative speed filtered (-1.0 CLLocation
  convention). Place visits (isVisit=1) excluded per location-domain ruling.
- **No new deps**: rusqlite already in Cargo.toml.
- **13 tests green**, cargo check clean.
