# AirNow (EPA AQI)

- **id:** `airnow`
- **domains:** `environment/` — binds the ratified `environment::EnvReading`
  shape (the build confirmed it fits; existing `weather/` stays grandfathered
  where it is)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll current AQI for the user's location; watermark
  on observation time)
- **connection:** `airnow` — TokenPaste (free API key, issued instantly by
  email registration at docs.airnowapi.org; no OAuth dance). Not shared with
  other defs.
- **evidence:** official-docs — airnowapi.org (documented REST endpoints,
  example JSON responses, free key flow)
- **effort / priority:** S / P1
- **needs:** none (public feed, no privacy flag) · Needs-login (validation
  only — build proceeds from documented shapes; the free key is trivially
  obtained)

## What it is

AirNow is the US EPA's official air-quality service: authoritative AQI from
actual ground-level monitors (PM2.5, PM10, ozone, CO, NO2, SO2), plus
forecast and fire/smoke layers. For Trove it's a public ambient-context
feed — "what was the air like where I was" — and a better US source than
model-derived estimates because it reflects real monitors, not satellite
inference. Anyone in the US or Canada gets accurate local AQI; non-US users
fall back to Open-Meteo's global air-quality feed.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Current observation | free key | AQI value + category per pollutant, reporting area, observation time | official docs |
| Forecast | free key | forecast AQI by category for the area | official docs |
| Historical observations | free key | past AQI by lat/long + date | official docs |
| Fire & smoke conditions | free key | fire/smoke layer data | official docs |

All optional in the contract (omit-if-empty); a reporting area with only
PM2.5 simply carries the one pollutant.

## Access & auth

- REST: `GET /aq/observation/latLong/current/?latitude=LAT&longitude=LON&distance=25&format=application/json&API_KEY=KEY`
  (plus forecast and historical endpoints). Returns current AQI by pollutant
  for the nearest reporting area. Base URL airnowapi.org.
- Auth: API key as a query param; free, email-registered, issued instantly.
- Rate limits: no stated cap on the free tier — trivially fine for a periodic
  personal poll.
- No TCC, no local files. Standalone-clean (plain HTTPS). **US/Canada only** —
  for users elsewhere the runner should defer to the global Open-Meteo
  air-quality path rather than return empty rows.

## Vault mapping

- **Raw layer:** `environment/airnow/raw/YYYY-MM.jsonl` — the API observation
  objects at full fidelity, one row per poll, partitioned by month.
- **Contract layer:** `environment/airnow/YYYY-MM.jsonl` per the (pending)
  environment contract — expected one reading row per poll (`ts` = observation
  time, `source`, `guid` = area+pollutant+observation-time, `metric` =
  pollutant, `value` = AQI, `category`, `place` = reporting area), overflow in
  `extra`. Same-shaped readings from owned home sensors (`home/`) and other
  public feeds merge at read time — AirNow is a *public feed*, so it writes
  `environment/`, never `home/`.
- **Dedupe:** area + pollutant + observation-time as `guid`; cursor in
  `.trove/airnow-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/airnow.rs`: `DEF` (Periodic, hourly-ish),
   `CONNECTION` (TokenPaste: label/help/placeholder per the SimpleFIN
   affordance rule, pointing at docs.airnowapi.org for the free key), `pull`
   hook for Sync-now. Location comes from the user's configured coordinates.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from airnowapi.org example responses (multi-pollutant current
   observation, plus a sparse single-pollutant variant); parser + store +
   cursor tests, unique temp dirs.
4. Non-US guard: when AirNow returns no reporting area, log a UI hint to use
   Open-Meteo air quality rather than failing.
5. Vault writes via `store` helpers binding `environment::EnvReading`
   (RESOLVED — the contract is ratified and bound; no longer parked).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Current observation | ✅ built | register a free key, paste it in the connect card; Sync now; confirm AQI rows in `environment/airnow/` + hub last-data |
| Forecast / historical | — (out of scope for v1) | call the forecast + historical endpoints; currently only current observations are pulled |

## Implementation notes (2026-06-15)

- Behavior: `Periodic` (hourly cadence via `Cadence::every_on_run(3600)`).
- Connection: new `TokenPaste` `CONNECTION` for the free AirNow API key.
  Registered as `&crate::airnow::CONNECTION` in `CONNECTIONS`.
- Field names confirmed from the Home Assistant AirNow integration
  (`const.py`): `DateObserved`, `HourObserved`, `LocalTimeZone`,
  `ReportingArea`, `StateCode`, `Latitude`, `Longitude`, `ParameterName`,
  `AQI`, `Category.{Number,Name}`.
- Contract: reuses `environment::EnvReading` (bound). Writes
  `environment/airnow/YYYY-MM.jsonl` (contract) +
  `environment/airnow/raw/YYYY-MM.jsonl` (raw, unconditional).
- `metric`: normalized from API `ParameterName` (`PM2.5` → `pm25`,
  `O3` → `o3`, etc.). `value` = AQI integer. `unit` = `"aqi"`.
- `guid` = `airnow-<ParameterName>-<DateObserved HH:00 TZ>` — upserts
  on re-poll of the same hour+pollutant.
- Non-US guard: empty API array → zero rows, no error (degrades silently).
- `-1` AQI sentinel (no data) skipped from contract rows; raw layer keeps
  all objects unconditionally.
- Location ladder reuses `corelocation::current_location` →
  `vault.weather_location()` → cursor (same as `nws.rs`).
- 14 unit tests, all green. `cargo check` and `cargo test -p trove-core
  airnow::` pass.

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §AirNow API
(US EPA AQI) (L2104–L2111). Feasibility 🟢 high. Ground monitors beat model
estimates locally — pairs with Open-Meteo (global) and WAQI (worldwide
crowdsourced) under the same environment shape; AirNow is the
US-authoritative slice. Free key, no rate cap, no privacy concern. Build as a
US-specific complement, not a replacement.
