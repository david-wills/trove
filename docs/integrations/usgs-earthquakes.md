# USGS Earthquakes

- **id:** `usgs-earthquakes`
- **domains:** `environment/` (contract: **Phase 3 pending** — AQI, quakes,
  alerts, sun/moon, aurora, wildfire feeds converge here; existing
  `weather/` stays grandfathered)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll a radius around the user's location; watermark
  cursor)
- **connection:** none (fully keyless)
- **evidence:** official-docs — earthquake.usgs.gov FDSN `event/1/query`
  (GeoJSON) + realtime summary feeds; documented 20k-event cap
- **effort / priority:** S / P1
- **needs:** none (location is the device's coarse coords — a public-feed
  query parameter, not a stored location trail)

## What it is

The USGS Earthquake Catalog is the authoritative, government-maintained
global record of seismic events. It's a "world around the user" ambient feed:
the seismic sibling of the weather store. Anyone in a quake-prone region cares
which tremors happened near them and when; the data matters as context that
correlates with felt events, sleep disruption, and travel.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Local quake events | none (keyless) | time, mag, depth, lat/lon, place, event id, alert level | official FDSN docs |
| Realtime feeds | none | rolling hour/day/week summaries, updated ~1 min | official feed docs |
| Historical backfill | none | full catalog back decades, paginated | official docs (20k/req cap) |

All optional in the contract (omit-if-empty).

## Access & auth

- **Realtime feeds (no params):** `…/earthquakes/feed/v1.0/summary/all_hour.geojson`
  (≈1-min cadence), `all_day.geojson`, `significant_week.geojson`.
- **Custom/historical:** `…/fdsnws/event/1/query?format=geojson&starttime=…&endtime=…&minmagnitude=2.5&latitude=LAT&longitude=LON&maxradiuskm=500`.
  Paginated with `limit`+`offset`; **20,000 events max per request**.
- Truly keyless, global, plain HTTPS — standalone-clean, no TCC.

## Vault mapping

- **Raw layer:** `environment/usgs-earthquakes/raw/YYYY-MM.jsonl` — the
  GeoJSON feature objects, full fidelity.
- **Contract layer:** `environment/usgs-earthquakes/YYYY-MM.jsonl` per the
  (pending) environment contract — expected shape: one row per event (`ts` =
  origin time, `source`, `guid` = USGS event id, `mag`, `depth_km`, `lat`,
  `lon`, `place`), overflow (alert, tsunami flag, felt reports) in `extra`.
- **Dedupe:** USGS event id as `guid` (note USGS revises events post-hoc —
  upsert by id, keep latest); cursor in `.trove/usgs-earthquakes-sync.json`,
  rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/usgs_earthquakes.rs`: `DEF` (Periodic,
   hourly), `pull` hook for Sync-now. Query a radius (default ~500 km,
   `minmagnitude` ~2.5 to keep noise low) around the device's coarse coords.
2. Registration line in `INTEGRATIONS`. No `CONNECTION` (keyless).
3. First run: backfill historical events via paginated `event/1/query`
   (respect the 20k/req cap with `limit`+`offset`).
4. Fixtures from documented GeoJSON example responses; parser + store +
   pagination + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers binding the (now-ratified) `environment`
   contract — `EnvGeoEvent` with `event_type="quake"` (same shape as NWS
   alerts / NASA FIRMS). Shipped reuse-bound; no contract park.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Local events | ✅ built | Sync now with a location near a seismic region; confirm rows in `environment/usgs-earthquakes/events/` + hub last-data |
| Realtime feed | ✅ built | `all_hour.geojson` parsed and deduped by guid (confirmed via unit tests) |
| Historical backfill | ✅ built | FDSN paginated query drains from watermark; pagination loop stops on empty/short page; watermark advances only after full drain |

## Build notes (2026-06-16)

Contract fit: `reuse-bound` — `environment/EnvGeoEvent` (the geo-event shape for
earthquakes); same shape as NWS alerts and NASA FIRMS fire detections; `event_type="quake"`.

Field mappings confirmed against the official USGS GeoJSON feed documentation
(earthquake.usgs.gov/earthquakes/feed/v1.0/geojson.php):

- `guid` = feature top-level `id` (e.g. `ci40123456`)
- `ts` = `properties.time` (milliseconds since epoch → local RFC3339)
- `magnitude` = `properties.mag` (f64)
- `place` = `properties.place`
- `lat`/`lon` = `geometry.coordinates[1]`/`[0]` (GeoJSON lon-first order)
- `severity` = `properties.alert` (`"green"` / `"yellow"` / `"orange"` / `"red"`)
- `url` = `properties.url`
- `extra`: `depth_km` (coordinates[2]), `tsunami` (bool), `felt`, `sig`, `cdi`, `mmi`, `mag_type`, `status`, `net`, `updated`

Two-arm pull: (1) `all_hour.geojson` realtime feed (global, fast, ~1-min USGS
cadence); (2) FDSN `event/1/query` paginated backfill from watermark → now.
Cursor watermark advances only after a full drain (crash-safe). Upsert by guid
handles post-hoc USGS revisions (magnitude/location updates). 13 unit tests green.

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §USGS Earthquake
Catalog (L2040–L2047). Feasibility 🟢 high — fully keyless, FDSN-standard,
unlimited backfill, global. Realtime GeoJSON feeds are USGS-preferred for
performance. Good complement to the weather store: both are "world around the
user" streams that merge with `home/` owned-sensor readings at read time.
