# Flighty

- **id:** `flighty`
- **domains:** `travel/` (contract: **travel ✅ ratified** — reuse-bound to the
  trip-segment shape pioneered by `airbnb.rs`; no schema touch)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the local SQLite DB; watermark on last-seen
  flight)
- **connection:** none — reads a local SQLite DB inside the app's container
  (`~/Library/Containers/com.flightyapp.flighty/…`), which is user-accessible
  without Full Disk Access. No OAuth, no token.
- **evidence:** community-schema — the `flighty-mcp` GitHub project confirms
  the DB path and a readable, unencrypted schema (medium confidence; no
  official docs); the in-app CSV export is the documented fallback
- **effort / priority:** S / P2
- **needs:** Needs-sample (schema is reverse-engineered — parser is built
  parser-last against a real DB; gracefully skip when the DB is absent)

## What it is

Flight-tracker app (iOS-first, with a Mac App Store build). Tracks upcoming
and past flights with status, delays, gate, aircraft, airline/airport info,
and weather. Used by frequent flyers who want a tidy flight history; Flighty
can also import from myFlightRadar24, App in the Air, OpenFlights, and
FlightMemory, so it doubles as an aggregator for a user's lifetime flight log.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Past flights | free (local DB) | date, origin, destination, airline, flight no., aircraft, gate, delays | community (flighty-mcp schema) |
| Upcoming flights | free (local DB) | scheduled times, status | community (flighty-mcp schema) |
| CSV export | free (in-app) | per-flight rows (Settings → Export) | official UI feature |

All optional in the contract (omit-if-empty). No tier-specific code paths.

## Access & auth

- Local SQLite at
  `~/Library/Containers/com.flightyapp.flighty/Data/Documents/MainFlightyDatabase.db`
  — confirmed by `flighty-mcp`. Container path, user-readable, no FDA / no TCC
  prompt. Unencrypted.
- No API, no network. Standalone-clean (read a local file with a bundled
  SQLite library — no runtime dependency on the Flighty app being running).
- **iOS-only users:** the DB never lands on the Mac. Fallback is the in-app
  CSV export (`Settings → Export`) routed through the generic import box (M1).

## Vault mapping

- **Raw layer:** `travel/flighty/raw/` — the SQLite rows verbatim (one JSONL
  partition per month by flight date), full fidelity.
- **Contract layer:** `travel/flighty/YYYY.jsonl` per the (pending) travel
  trip-segment contract — expected one row per flight segment (`ts` =
  departure, `source`, `guid` = flight id, `kind` = "flight", `origin`,
  `destination`, `carrier`, `flight_no`), overflow (gate, aircraft, delay
  minutes, weather) in `extra`. Parked behind the contract draft (Needs-David).
- **Dedupe:** stable flight row id as `guid`; cursor in
  `.trove/flighty-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/flighty.rs`: `DEF` (Periodic, daily-ish),
   `permission`/`last_data` hooks, `pull` hook for Sync-now. No `CONNECTION`.
2. Registration line in `INTEGRATIONS`.
3. **Parser-last / Needs-sample:** reverse-engineer the schema against a real
   `MainFlightyDatabase.db`. Until a sample is in hand, the table/column
   mapping is provisional — flag Needs-sample. CSV-export parser is the M1
   fallback path for iOS-only users.
4. Graceful skip: when the DB is absent, the def reports no-data rather than
   erroring (the app's whole user base won't have Flighty installed).
5. Vault writes via `store` helpers once the travel contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Past/upcoming flights | ⚠️ parked (Needs-sample) | with the macOS app installed, Sync now; confirm rows in `travel/flighty/` + hub last-data; spot-check a known flight |
| CSV fallback | — | export CSV from the app, drop into the import box, confirm the same rows land |
| Absent-DB skip | ✅ built + tested | run on a machine without Flighty; confirm a clean no-data state, no error |

## Build status (2026-06-16)

- **Status:** 🧪 scaffold built — `Behavior::Periodic`, travel contract wired,
  raw layer + dedupe + absent-DB skip done. Parser parked pending a real
  `MainFlightyDatabase.db` sample (Needs-sample: schema is community-only,
  no public docs, no sample on disk). 7 tests green.
- **What's done:** `FlightRow` intermediate type, `row_to_segment` (contract
  mapping), `write_raw` (full-fidelity raw layer), `load_seen` (dedupe from
  existing JSONL), `collect` (copy-then-open Periodic pass with graceful
  absent-DB skip), `flighty_db_path` (container path discovery).
- **Only thing parked:** `rows_from_db` — the `SELECT` query needs real
  table/column names. When a sample lands: run `sqlite3 MainFlightyDatabase.db
  .tables` + `PRAGMA table_info(…)`, fill in the query, remove the
  `anyhow::bail!` guard.
- **No new connection, no new Cargo deps** (uses `rusqlite` + `dirs` already
  present, `browser::import_via_copy` for the copy-then-open pattern).

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Flighty (macOS) (L2292–
L2298). Feasibility 🟢 high. DB path confirmed by the `flighty-mcp` project;
schema must be reverse-engineered (no public docs) — hence Needs-sample. CSV
export covers iOS-only users. TripIt and airline-email parses share the travel
contract — sequence one after Flighty to exercise the contract with a second
source.
