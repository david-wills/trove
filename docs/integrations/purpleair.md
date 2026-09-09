# PurpleAir

- **id:** `purpleair`
- **domains:** `environment/` (contract: **Phase 3 pending** — `environment/`
  shapes drafted across AQI, quakes, alerts, sun/moon, aurora feeds; existing
  `weather/` is grandfathered and stays where it is)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll nearby sensors on a schedule)
- **connection:** `purpleair` — TokenPaste (free read key from
  develop.purpleair.com via Google SSO; no credit card). Not shared with other
  defs.
- **evidence:** official-docs — api.purpleair.com/v1/sensors (documented
  fields, EPA correction option, historical queries)
- **effort / priority:** M / P2
- **needs:** none

## What it is

PurpleAir is a network of community-run laser particle-counter sensors that
report hyperlocal PM2.5. In dense metros it beats model-based air-quality
estimates because it reads the actual air on the user's block. Coverage is
uneven — dense in wealthy US/EU cities, sparse elsewhere — so it complements
rather than replaces AirNow and Open-Meteo's model AQI.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Nearby PM2.5 | free key | pm2.5 (raw ug/m3), sensor id/distance | official docs |
| Co-measured | free key | temperature, humidity | official docs |
| Historical | free key | back to 2016 per sensor | official docs |

All optional in the contract (omit-if-empty). Users with no nearby PurpleAir
sensor get empty results — degrade gracefully, never fail.

## Access & auth

- `GET https://api.purpleair.com/v1/sensors?fields=pm2.5,temperature,humidity&location_type=0&nwlng=…&nwlat=…&selng=…&selat=…`
  — bounding-box query around the user's location returns nearby readings.
- API Read Key required, free from develop.purpleair.com (Google SSO, no card).
- No `correction=EPA` API parameter exists; EPA correction is a client-side formula over `pm2.5_cf_1 + humidity` applied at read time if needed.
- No TCC, no local files. Standalone-clean (plain HTTPS).
- 2-minute sensor update rate; poll on a light schedule.

## Vault mapping

- **Raw layer:** `environment/purpleair/raw/YYYY-MM.jsonl` — the API sensor
  objects, full fidelity.
- **Contract layer:** `environment/purpleair/YYYY-MM.jsonl` per the pending
  Phase-3 `environment/` contract — expected one row per reading (`ts`,
  `source`, `metric` = pm2.5/etc., `value`, sensor id + distance in `extra`).
  Same-shaped readings merge with AirNow / Open-Meteo AQ at read time. Parked
  behind the contract until it ratifies.
- **Dedupe:** `guid` = sensor id + reading timestamp; cursor in
  `.trove/purpleair-sync.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/purpleair.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: label/help/placeholder per the affordance rule —
   point at develop.purpleair.com), bounding-box pull hook around user
   location.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from api.purpleair.com example responses (sensor-present AND
   empty-bounding-box variants); parser + store + cursor tests, unique temp
   dirs.
4. Vault writes via `store` helpers once the `environment/` contract is
   ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Nearby PM2.5 | ✅ built | paste a real read key in the connect card; Sync now; confirm rows in `environment/purpleair/` + hub last-data |
| Empty-area degrade | ✅ built | empty data array returns 0 readings, no error; tested in `empty_bbox_is_graceful` |

## Defect fixes (2026-06-17)

- **Raw-layer dedup fixed (major):** `upsert_raw` now keys on `sensor_index + UTC_hour(last_seen)` (not sensor_index alone). Before the fix, every hourly poll of the same sensor within a month overwrote the prior raw row, collapsing a full month of hourly history to one row per sensor. The new key mirrors airnow's `ParameterName + DateObserved + HourObserved` approach. UTC derivation from the `last_seen` epoch also makes the key stable across machine-TZ changes and DST transitions (fixes the minor TZ-dependency defect simultaneously). Tests: `raw_different_hours_produce_separate_rows` + `raw_same_hour_upserts_not_duplicates`.
- **Parse-error surfacing fixed (minor):** `parse_envelope` now returns `Result<…>` and propagates structural API-shape errors rather than collapsing them to an empty result. The caller writes the error to `state.error` before propagating so the hub card can surface it. A legitimate empty bounding box (valid envelope, empty `data` array) still yields `Ok(([], []))` with no error.
- **Brief EPA correction claim corrected (minor):** The `correction=EPA` query parameter does not exist in the PurpleAir API (the EPA formula is client-side over `pm2.5_cf_1 + humidity`). Brief and capability table updated; code comment at L255-264 (which already correctly debunked this) unchanged.

## Build notes (2026-06-17)

- **Behavior:** Periodic, hourly (`PURPLEAIR_SYNC_SECS = 3600`).
- **Connection:** new `purpleair` TokenPaste connection (API Read Key from develop.purpleair.com).
- **Contract:** `environment/purpleair/YYYY-MM.jsonl` via `EnvReading` — one row per metric (pm25/temperature/humidity) per sensor per hour poll. Unit: `ug_m3` for pm25 (raw concentration, NOT AQI), `F` for temperature, `percent` for humidity.
- **Raw layer:** `environment/purpleair/raw/YYYY-MM.jsonl` — full sensor objects, unconditional.
- **Deduplication:** `guid = purpleair-<sensor_index>-<metric>-<YYYY-MM-DDTHH>` — upserts on same sensor+metric+hour.
- **API fields confirmed:** `sensor_index`, `name`, `latitude`, `longitude`, `last_seen` (unix ts), `pm2.5`, `pm1.0`, `pm10.0`, `temperature`, `humidity`, `pressure`, `confidence` — from Home Assistant coordinator.py + aiopurpleair library.
- **Envelope format confirmed:** `{"fields":[...], "data":[[values], ...]}` — zip-based reconstruction per aiopurpleair/models/sensors.py.
- **Integrator must add:** `&crate::purpleair::CONNECTION,` to `CONNECTIONS` in `integrations.rs`.

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §PurpleAir API
(L2176–L2183). Feasibility 🟡 medium — the only friction is uneven coverage
and the API key. **Follow-on after AirNow + Open-Meteo AQ ship** (those cover
the general case; PurpleAir adds hyperlocal precision in metros). Public feed,
so it routes `environment/` (not `home/`) and merges with other AQI sources at
read time.
