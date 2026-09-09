# myFlightRadar24

- **id:** `myflightradar24`
- **domains:** `travel/` (contract: **Phase 3 pending** — trip-segment shape,
  drafted from Flighty + TripIt + myFlightRadar24 + airline-email parses
  together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-supplied CSV export through the generic import box)
- **connection:** none — no API, no login inside Trove. The export is fetched
  by the user from the Flightradar24 web settings page and dropped in.
- **evidence:** official-docs — the in-account CSV export at
  `my.flightradar24.com/settings/export` is documented and working on free
  accounts; columns are documented (Date/Origin/Destination mandatory, plus
  optional airline/flight no./aircraft). No personal-history REST API exists.
- **effort / priority:** S / P2
- **needs:** none — CSV column names confirmed from `imikailoby/fr24-csv-parser`
  `src/constants/csv.ts` (19 columns, YYYY-MM-DD dates). Parser + 11 tests green.

## What it is

myFlightRadar24 is Flightradar24's personal flight-logbook feature: users
record the flights they've taken and export the log as a CSV. Many flight
enthusiasts maintain their lifetime flight history here. It complements
Flighty — between the two, most frequent flyers get full historical coverage.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Flight log CSV | free account | Date, Origin, Destination (mandatory) + airline, flight no., aircraft (optional) | official export feature |

All optional in the contract (omit-if-empty) except the mandatory
date/origin/destination triple that every row carries. No tiering.

## Access & auth

- Web export: log in to `my.flightradar24.com` → Settings → Export → Download
  CSV (`https://my.flightradar24.com/settings/export`). Free accounts support
  it. The feature also accepts CSV *import* for bulk-loading past flights, so
  the same column shape round-trips.
- No personal-history REST API. (The public Flightradar24 API and the
  `pyfr24` library are about *live* aircraft tracking, not a user's own
  history — out of scope here.)
- Inside Trove: no auth, no network. The user obtains the CSV out-of-band and
  drops it into the generic import box (M1 path) — standalone-clean.

## Vault mapping

- **Raw layer:** `travel/myflightradar24/raw/` — the imported CSV rows
  verbatim (one JSONL partition per year by flight date), full fidelity.
- **Contract layer:** `travel/myflightradar24/YYYY.jsonl` per the (pending)
  travel trip-segment contract — expected one row per flight segment (`ts` =
  flight date, `source`, `guid` = stable hash of date+origin+destination+
  flight no., `kind` = "flight", `origin`, `destination`, `carrier`,
  `flight_no`), aircraft type and any extra columns in `extra`. Parked behind
  the contract draft (Needs-David).
- **Dedupe:** since rows carry no native id, `guid` = a stable hash of the
  mandatory triple plus flight number, so re-importing an overlapping export
  (or a row also present from Flighty) doesn't duplicate.

## Build plan

1. Module `crates/trove-core/src/myflightradar24.rs`: `DEF` (Import) with the
   import-box file matcher (`.csv`). No `CONNECTION`.
2. Registration line in `INTEGRATIONS`.
3. **Parser-last / Needs-sample:** the column names are documented but the
   exact header spelling, date format, and delimiter must be confirmed against
   a real exported CSV before the parser is locked — flag Needs-sample.
4. Fixtures: a small CSV with mandatory-only rows AND fully-populated rows
   (date-only origin/dest vs. with airline/flight/aircraft); parser + store +
   dedupe tests (re-import is idempotent), unique temp dirs.
5. Synthesize a stable `guid` from the row fields (no native id) so dedupe
   holds across re-imports and across the Flighty overlap.
6. Vault writes via `store` helpers once the travel contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import | — | export a real CSV from my.flightradar24.com; drop into the import box; confirm rows in `travel/myflightradar24/` + hub last-data; spot-check a known flight |
| Idempotent re-import | — | import the same CSV twice; confirm no duplicate rows (guid dedupe holds) |
| Sparse rows | — | import a CSV with date/origin/destination only; confirm the rows land with optional fields omitted |

## Build notes (INDEX #158)

- **CSV columns confirmed** from `imikailoby/fr24-csv-parser`
  (`src/constants/csv.ts` `EXPECTED_CSV_COLUMNS`): 19 columns — `Date, Flight
  number, From, To, Dep time, Arr time, Duration, Airline, Aircraft,
  Registration, Seat number, Seat type, Flight class, Flight reason, Note,
  Dep_id, Arr_id, Airline_id, Aircraft_id`. Date format `YYYY-MM-DD` confirmed
  from the parser's generator code.
- **Behavior:** `Import` (CSV only); no auth, no network.
- **Contract:** `travel.Segment` (`type:"flight"`); `start_place`=From,
  `end_place`=To, `vendor`=Airline, `number`=Flight number; aircraft,
  registration, seat, class, reason, note, dep_time, arr_time, duration,
  dep_id, arr_id → `extra`. Partitioned by local month of flight date.
- **Guid:** SHA-256(date | from | to | flight_number) — stable across
  re-imports; aligns with the Flighty dedup anchor for cross-source read-time
  reconciliation.
- **Raw layer:** `travel/myflightradar24/raw/YYYY.jsonl` — every CSV row as
  a full-fidelity JSON object, partitioned by flight year.
- **Tests:** 11 tests, all green (`cargo test -p trove-core myflightradar24::`)
- **cargo check:** clean (only pre-existing workspace warnings).

## Research notes

`integrations-research.md` → "Geolocation & Travel" §myFlightRadar24 (L2324–
L2330). Feasibility 🟢 high — documented, working CSV export on free accounts,
trivial to parse. Combine with the Flighty DB read for full flight coverage of
most users (the shared `guid` hash lets the two sources merge cleanly). No
official personal-history API exists; `pyfr24` / the public FR24 API are live
aircraft data, not personal logbooks, and are out of scope. TripIt and
airline-email parses share the travel contract — sequence one after to
exercise the contract with a second source.
