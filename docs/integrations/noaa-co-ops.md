# NOAA Tides & Currents (CO-OPS)

- **id:** `noaa-co-ops`
- **domains:** `environment/` (contract: **Phase 3 pending** — environment
  ambient-readings contract drafted in the contract pass; until then,
  per-source raw under `environment/noaa-co-ops/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily pull of high/low predictions for the nearest
  station; watermark on prediction date range)
- **connection:** none (keyless)
- **evidence:** official-docs — api.tidesandcurrents.noaa.gov (CO-OPS
  datagetter + station metadata API, documented products and params)
- **effort / priority:** S / P2
- **needs:** none

## What it is

NOAA's Center for Operational Oceanographic Products and Services — tide
predictions and water-level/met observations from 3,000+ US coastal stations.
High/low tide tables predict up to 10 years out; observations roll 45 days.
Useful for coastal users (tide-aware planning, correlating activity with
tides); silently irrelevant for inland locations. A public feed →
`environment/`.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Tide predictions (hi/lo) | free, keyless | time, height, type (H/L), datum | official docs |
| Water-level observations | free, keyless | observed level, 45-day rolling | official docs |
| Station met (wind, pressure, water temp) | free, keyless | per-station readings | official docs |
| Station discovery | free, keyless | station id, name, coords | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- REST: `GET /api/prod/datagetter?product=predictions&datum=MLLW&station=…&
  begin_date=…&end_date=…&interval=hilo&format=json` — no key. Station list at
  `/mdapi/prod/webapi/stations.json`. Products: water_level, predictions, wind,
  air_pressure, water_temperature.
- Rate limits: generous public service; a daily personal pull is trivial.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `environment/noaa-co-ops/raw/YYYY-MM.jsonl` — the API
  prediction/observation objects, full fidelity.
- **Contract layer:** `environment/noaa-co-ops/YYYY-MM.jsonl` per the (pending)
  environment contract — expected shape: one row per high/low event (`ts` =
  predicted time, `source`, `guid`, `height`, `type`, station id + overflow in
  `extra`). Same-shaped readings merge with other environment sources at read
  time.
- **Dedupe:** `guid` = (station id, prediction time, type); cursor in
  `.trove/noaa-co-ops-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/noaa_co_ops.rs`: `DEF` (Periodic, daily),
   `pull` hook. No connection (keyless).
2. Registration line in `INTEGRATIONS`.
3. Nearest-station detection from user coordinates via the station metadata
   API; **skip the pull silently if >50 km inland** (no error, no row spam).
4. Fixtures from documented sample responses (prediction set + station list);
   parser + store + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers once the environment contract is ratified;
   until then **parked behind Needs-David (contract)** while raw writes proceed.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Tide predictions | ✅ built | set a coastal location; Sync now; confirm hi/lo rows in `environment/noaa-co-ops/` + hub last-data |
| Inland skip | ✅ built | set an inland (>50 km) location; confirm the def skips cleanly with no error and a clear "no nearby tidal station" state |

## Build notes (2026-06-16)

Implemented as Periodic/daily, reusing `environment` contract's `EnvReading` shape.
Field names confirmed from live NOAA CO-OPS API:
- Station list (`mdapi`): `id`/`name`/`lat`/`lng` (note: `lng` not `lon`)
- Predictions (`datagetter?interval=hilo`): `t` (timestamp), `v` (height string), `type` ("H"/"L")
- Water level observations: `t`/`v`/`s`/`f`/`q` — not collected (hi/lo predictions only, per brief)

Metrics written: `water_level` (ft, datum=MLLW). Station-level observations (wind, pressure,
water temp) omitted from the initial build — the brief scopes to predictions; observations
can be added as a follow-on pull within the same module.

18 tests: parser round-trips, haversine, nearest-station, dedup upserts, inland skip,
API error degradation, cursor persistence, back-compat sparse lines.

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §NOAA CO-OPS Tides
& Currents (L2048–L2055). Feasibility 🟢 high. US and territories only —
Open-Meteo has no tides endpoint; global tides would need the paid WorldTides
API, out of scope. Note gracefully for non-coastal and non-US users rather than
erroring.
