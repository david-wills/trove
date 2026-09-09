# Open-Meteo

- **id:** `open-meteo`
- **domains:** `environment/` (existing `weather/` stream is **grandfathered** —
  pre-taxonomy path, never renamed; new sibling endpoints write under
  `environment/`); contract: **Phase 3 pending** for the new environment
  substreams (`weather/` itself stays as built — document)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (keyless HTTP poll; forecast + lazy historical backfill)
- **connection:** none — keyless API; location comes from CoreLocation TCC or
  manual config
- **evidence:** official-docs — api.open-meteo.com/v1/forecast + archive-api
  (ERA5 back to 1940); air-quality / marine / flood / climate sibling APIs, all
  keyless with documented variable lists
- **effort / priority:** M / P1
- **needs:** none (extensions are additive — air-quality endpoint, daily
  sunrise/sunset/golden-hour vars, marine + flood endpoints, UV risk
  categories in UI)

## What it is

Free, keyless weather and environmental forecast API (Copernicus CAMS / ERA5
under the hood). Already shipped as the `weather` def — the first collector and
the reference "world around the user" stream. One HTTP client fans out to a
family of sibling endpoints (forecast, historical archive, air quality, marine,
flood, climate) that all share the same lat/lon + variable-list shape.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Hourly forecast (shipped) | free | temp, apparent temp, humidity, dew point, precip, snow, weather code, cloud cover, pressure_msl, wind, uv_index, is_day | official docs |
| Daily forecast (shipped) | free | sunrise, sunset, precipitation_sum, wind_speed_max | official docs |
| Historical archive | free | ERA5 back to 1940, ERA5-Land to 1950 — same variable lists | official docs |
| Air quality (extension) | free | pm10, pm2_5, CO, NO2, SO2, ozone, AOD, dust, US/EU AQI; pollen (Europe-only, omit elsewhere) | official docs |
| Marine + flood (extension) | free | wave/swell; GloFAS river discharge (null inland, safe to call) | official docs |
| Climate projections (extension) | free | CMIP6 downscaled — icebox-leaning (analysis query, not a collector cadence) | official docs |

All capability fields are optional in the contract (omit-if-empty); a
non-European user simply carries no pollen fields.

## Access & auth

- Forecast: `api.open-meteo.com/v1/forecast`; archive:
  `archive-api.open-meteo.com/v1/archive`; siblings on `air-quality-api`,
  `marine-api`, `flood-api`, `climate-api` subdomains. Params: latitude,
  longitude, hourly/daily variable lists, timezone. **No key.**
- Rate limit: 10k calls/day free — trivial for a personal periodic pull
  (~every 3h for AQ given a 5-day window).
- No TCC for the API itself; location source is CoreLocation (granted) or
  manual config. Standalone-clean (plain HTTPS, no external app).

## Vault mapping

- **Raw layer (shipped):** `weather/YYYY-MM.jsonl` (grandfathered path — the
  schema identifier; not renamed). New sibling streams write
  `environment/<stream>/YYYY-MM.jsonl` (e.g. `environment/air-quality/`,
  `environment/marine/`, `environment/flood/`).
- **Contract layer:** `weather/` stays as built (document — the WeatherObservation
  struct evolves additively: daily sunrise/sunset/golden-hour vars fold into it).
  New environment substreams await the **Phase 3 environment contract** if
  shapes converge with the other environment sources (AQI, quakes, alerts);
  until then raw-only per source. Dedupe `guid` = (lat, lon, hour) tuple.

## Build plan

Already shipped pre-pipeline as the `weather` def. Remaining work is additive:
1. Extend the existing WeatherObservation struct with daily sunrise/sunset/
   golden-hour variables (already returned by the same forecast call).
2. Add a separate hourly AQ poll writing `environment/air-quality/` (trivial —
   same HTTP client, new variable list).
3. Marine + flood endpoints (null inland — safe to always call; degrade
   gracefully).
4. UV risk categories in UI (uv_index already collected).
5. Climate-projection API stays in the icebox (analysis query, not a collector).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Forecast + historical (shipped) | 🧪 built | shipped pre-pipeline as the `weather` def; David promotes to ✅ after confirming rows in `weather/` + hub last-data on a real run |
| Air quality / marine / flood | — | enable extension; Sync now; confirm `environment/<stream>/` rows; verify pollen absent gracefully for a US location |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §Open-Meteo
Forecast+Archive (L2016–L2023), Air Quality (L2024–L2031), Marine (L2056–L2063),
Flood (L2080–L2087), UV (L2120–L2127), Climate (L2208–L2215). Feasibility 🟢
high. Weather is always re-fetchable, so backfill runs lazily and safely. One
keyless client, several sibling endpoints — the canonical "absorb as a library"
example. Historical AQ is *not* available (forecast-only); NOAA CDO covers that
gap if demanded.
