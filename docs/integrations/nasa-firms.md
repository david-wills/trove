# NASA FIRMS Wildfire

- **id:** `nasa-firms`
- **domains:** `environment/` (contract: **Phase 3 pending** — environment
  feeds share an ambient-readings/events contract drafted in the contract
  pass; until then, per-source raw under `environment/nasa-firms/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily bounding-box query around user location;
  watermark on detection day-range)
- **connection:** `nasa-firms` — TokenPaste (free MAP_KEY, one-time email
  signup at firms.modaps.eosdis.nasa.gov/api/map_key/). Not shared with
  other defs.
- **evidence:** official-docs — firms.modaps.eosdis.nasa.gov/api (CSV area
  endpoint, documented BBOX/day-range params, rate limits)
- **effort / priority:** M / P1
- **needs:** none

## What it is

NASA's Fire Information for Resource Management System — satellite active-fire
detections (thermal anomalies) from MODIS and VIIRS, available globally within
~3 hours of satellite overpass. High value in fire-prone regions (western US,
Australia, Mediterranean): answers "is there an active fire near me?" and
records fire seasons against the user's location history. A public feed of the
ambient environment, not an owned sensor — routes to `environment/`.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Active-fire detections | free MAP_KEY | lat, lon, brightness, confidence, FRP (radiative power), acquisition date/time, satellite, day/night | official docs |
| Sensor choice | free | MODIS (1 km) vs VIIRS_SNPP_NRT (375 m, preferred) | official docs |
| Historical archive | free | same shape via date param | official docs |

All optional in the contract (omit-if-empty). VIIRS is the default sensor;
MODIS is a fallback if VIIRS coverage is stale.

## Access & auth

- REST: `GET /api/area/csv/{MAP_KEY}/{SENSOR}/{BBOX}/{DAY_RANGE}` — returns CSV
  rows of detections. Country endpoint and KML footprints also exist.
- Auth: free MAP_KEY via one-time email signup. Rate limit 5,000
  transactions / 10-minute interval — trivially fine for a daily personal pull.
- No TCC, no local files. Standalone-clean (plain HTTPS). CSV responses need a
  small parser (no JSON variant for the area endpoint).

## Vault mapping

- **Raw layer:** `environment/nasa-firms/raw/YYYY-MM.jsonl` — the parsed CSV
  detection rows, full fidelity (every field FIRMS returns).
- **Contract layer:** `environment/nasa-firms/YYYY-MM.jsonl` per the (pending)
  environment contract — expected shape: one row per detection event (`ts` =
  acquisition datetime, `source`, `guid`, `lat`, `lon`, plus
  intensity/confidence in `extra`). Smoke/AOD is **not** sourced here — it
  already arrives via Open-Meteo Air Quality; this def is fire detections only.
- **Dedupe:** `guid` = stable hash of (satellite, lat, lon, acq_date,
  acq_time); cursor in `.trove/nasa-firms-sync.json`, rebuildable by scanning
  output files.

## Build plan (completed)

1. ✅ Module `crates/trove-core/src/nasa_firms.rs`: `DEF` (Periodic, daily),
   `CONNECTION` (TokenPaste: MAP_KEY), `pull` hook for Sync-now. Sensor VIIRS_SNPP_NRT.
2. ✅ `CONNECTION` registered in `CONNECTIONS` (integrations.rs).
3. ✅ CSV parser for the area-endpoint response (VIIRS field names confirmed from
   official earthdata.nasa.gov attribute table); fixtures cover populated / empty /
   future-column / night cases; 16 tests green.
4. ✅ Location: ±1° bounding box from CoreLocation → manual weather location →
   cursor fallback; inert (no error) when no location is set.
5. ✅ Vault writes via `store` helpers — writes `EnvGeoEvent` to
   `environment/nasa-firms/events/YYYY-MM.jsonl` (contract layer, upsert by guid)
   and raw parsed-CSV objects to `environment/nasa-firms/raw/YYYY-MM.jsonl`
   (unconditional, full fidelity).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Active-fire detections | ✅ unit-tested | paste a real MAP_KEY in the connect card; set a location inside an active fire region (or use a known historical fire date/box); Sync now; confirm rows in `environment/nasa-firms/events/` + hub last-data |
| Empty-box handling | ✅ unit-tested | run with a location with no active fires; confirm a clean empty result (no error, last-data still updates) |
| Dedup on re-poll | ✅ unit-tested | re-poll same bbox/date; confirm event count unchanged in the JSONL file |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §NASA FIRMS
(L2088–L2095). Feasibility 🟢 high. VIIRS_SNPP_NRT (375 m) preferred over MODIS
(1 km) for recency. Detections land within ~3h of overpass globally; US/Canada
near-real-time. Wildfire smoke/AOD is deliberately out of scope here — covered
by the Open-Meteo Air Quality stream with no extra key.
