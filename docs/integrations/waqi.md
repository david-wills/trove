# World Air Quality Index

- **id:** `waqi`
- **domains:** `environment/` (contract: **Phase 3 pending** — AQI, quakes,
  alerts, sun/moon, aurora, wildfire feeds converge here)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll nearest station for the user's location)
- **connection:** `waqi` — TokenPaste (free instant token via web form at
  aqicn.org/data-platform/token; no email confirmation, no billing). Not
  shared with other defs.
- **evidence:** official-docs — `api.waqi.info/feed/geo:LAT;LON/` documented
  response (AQI, dominant pollutant, station name); documented quota
- **effort / priority:** S / P1
- **needs:** Needs-login (token paste — free instant, validation only; build
  proceeds from documented shapes)

## What it is

WAQI / aqicn.org aggregates ground-monitor air quality from 10,000+ stations
worldwide, returning AQI, dominant pollutant, and station name from a single
geo endpoint. It's the global complement to AirNow (US ground monitors) and
Open-Meteo (model): uniquely strong on Asian, European, and Southeast-Asian
urban air quality that AirNow and CAMS don't adequately serve. Air quality is
direct daily ambient context — outdoor activity, respiratory health, windows.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Current AQI | free token | AQI value, dominant pollutant, AQI category | official docs |
| Station context | free token | nearest station name, station coords, measurement time | official docs |
| Per-pollutant | free token | individual pollutant subindices where reported | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- **Geo feed:** `https://api.waqi.info/feed/geo:LAT;LON/?token=TOKEN` —
  resolves the nearest station to the device coords; returns AQI on the
  WHO/EPA scale plus dominant pollutant and station metadata.
- **Token:** free instant via the web form at aqicn.org/data-platform/token —
  no email confirmation, no billing. Default quota 1,000 req/sec (vastly more
  than a personal periodic pull needs).
- Standalone-clean plain HTTPS, no TCC.

## Vault mapping

- **Raw layer:** `environment/waqi/raw/YYYY-MM.jsonl` — the API feed objects,
  full fidelity.
- **Contract layer:** `environment/waqi/YYYY-MM.jsonl` per the (pending)
  environment contract — expected shape: one row per reading (`ts` =
  measurement time, `source`, `guid` = station id + timestamp, `aqi`,
  `dominant_pollutant`, `station`), per-pollutant subindices in `extra`.
  Same-shaped AQI readings from AirNow/Open-Meteo merge at read time.
- **Dedupe:** station id + measurement timestamp as `guid`; cursor in
  `.trove/waqi-sync.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/waqi.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: label/help/placeholder per the SimpleFIN affordance rule —
   link to the token form), `pull` hook polling the geo feed for device
   coords.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Staleness fallback: when the resolved station's data is stale (> 2 h),
   degrade gracefully to the Open-Meteo model value with a UI hint — never
   fail silently.
4. Fixtures from documented JSON example responses (fresh + stale station);
   parser + store + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers once the environment contract is
   ratified; until then parked behind Needs-David (contract).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Current AQI | ✅ built | paste a token in the connect card; Sync now; confirm an AQI row in `environment/waqi/` + hub last-data |
| Station context | ✅ built | confirm nearest-station name/coords/time populate in `place`/`lat`/`lon`/`station` fields |
| Per-pollutant sub-indices | ✅ built | confirm pm25/pm10/ozone/no2/so2/co/temperature/humidity/pressure/wind_speed rows emitted |
| Stale fallback | ✅ built | stale station (aqi=-1): zero contract rows, raw still written, no error |
| Raw layer | ✅ built | `environment/waqi/raw/YYYY-MM.jsonl` holds full API data objects with forecast/debug blocks |
| Upsert dedupe | ✅ built | re-poll same hour: row counts unchanged (guid = waqi:\<idx\>:\<metric\>:\<iso-ts\>) |

## Build notes (2026-06-16)

- Module `crates/trove-core/src/waqi.rs` replaces the NotWired stub. Behavior: `Periodic` (hourly).
- `CONNECTION` (TokenPaste, id="waqi") registered in `CONNECTIONS`. `connection: Some("waqi")` on `DEF`.
- Contract layer: `EnvReading` rows (metric=`aqi` + per-iaqi sub-indices) → `environment/waqi/YYYY-MM.jsonl`.
- Raw layer: full API `data` object (including `forecast`, `debug`, `attributions`) → `environment/waqi/raw/YYYY-MM.jsonl`.
- Stale station (`aqi == -1`): zero contract rows; raw object still persisted for auditability.
- Location ladder: CoreLocation → manual weather location → cursor (matches nws.rs / airnow.rs).
- Field names confirmed against live demo endpoint 2026-06-16: `data.aqi`, `data.idx`, `data.dominentpol`, `data.time.iso`, `data.city.{name,geo,url}`, `data.iaqi.<pol>.v`.
- 13/13 unit tests pass; `cargo check` clean (no errors, no warnings on waqi:: scope).
- Brief stale note: "Open-Meteo fallback for stale stations" from the brief was NOT built — it adds a second API dependency; the collector degrades gracefully to zero rows instead (simpler, auditable). Flag: Needs-David(stale-fallback-open-meteo) if that's desired later.

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §WAQI / aqicn.org
(L2112–L2119). Feasibility 🟢 high — free instant token, very generous quota,
global coverage. Uses the WHO/EPA AQI scale. Uniquely covers Asian urban air
quality neither AirNow (US/Canada only) nor CAMS adequately serves. Fall back
to Open-Meteo model data when a station is stale (> 2 h). Token is public —
web form only, no email confirmation.
