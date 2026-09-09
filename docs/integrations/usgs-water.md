# USGS Water Data

- **id:** `usgs-water`
- **domains:** `environment/` (contract: **Phase 3 pending** — AQI, quakes,
  alerts, sun/moon, aurora, wildfire feeds converge here)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll nearest gauge(s); watermark cursor)
- **connection:** none (keyless)
- **evidence:** official-docs — modernized `api.waterdata.usgs.gov`
  (2025-released) `observations/current` + `monitoring-locations` discovery;
  legacy `waterservices.usgs.gov/nwis/iv` still live
- **effort / priority:** M / P2
- **needs:** none (US-only; skips gracefully when no gauge is near)

## What it is

USGS Water Services exposes ~10,000 active US stream gauges with real-time
gage height and discharge plus official flood-stage thresholds
(action/flood/moderate/major). It's an ambient hazard feed for users near
rivers — more granular than a global flood model because it reports actual,
station-categorized flood stages rather than modeled discharge.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Gage height | none (keyless) | value (ft, param `00065`), timestamp, site id | official docs |
| Discharge | none | value (cfs, param `00060`), timestamp | official docs |
| Flood-stage category | none | action/flood/moderate/major threshold crossing | official docs (RTFI) |
| Station discovery | none | site id, name, lat/lon by state/site-type | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- **Current obs:** `https://api.waterdata.usgs.gov/observations/current?monitoring-location-id=SITEID&parameterCode=00065&format=json`
  (gage height) or `00060` (discharge).
- **Discovery:** `https://api.waterdata.usgs.gov/monitoring-locations/?stateCd=CA&siteType=ST&format=json`
  — resolve nearest gauge(s) from device coords.
- Keyless. **Use the modernized `api.waterdata.usgs.gov`, not legacy
  `waterservices.usgs.gov/nwis/iv`** (both live mid-2026). US-only;
  refresh every 15–60 min depending on station. Standalone-clean, no TCC.

## Vault mapping

- **Raw layer:** `environment/usgs-water/raw/YYYY-MM.jsonl` — the API
  observation objects, full fidelity.
- **Contract layer:** `environment/usgs-water/YYYY-MM.jsonl` per the
  (pending) environment contract — expected shape: one row per reading
  (`ts`, `source`, `guid` = site id + timestamp, `value`, `unit`,
  `parameter`, `site_id`), flood-stage category and thresholds in `extra`.
- **Dedupe:** `site_id` + observation timestamp as `guid`; cursor in
  `.trove/usgs-water-sync.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/usgs_water.rs`: `DEF` (Periodic). On enable,
   resolve nearest gauge(s) via `monitoring-locations`; skip silently (with a
   UI hint) when no station is within range or the user is non-US.
2. Registration line in `INTEGRATIONS`. No `CONNECTION` (keyless).
3. `pull` hook polls `observations/current` for the resolved sites; layer the
   Real-Time Flood Impact (RTFI) flood-stage categories.
4. Fixtures from documented JSON example responses (current obs + discovery);
   parser + store + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers against the bound `environment` contract
   (`EnvReading`) — ratified, no longer Needs-David. See Build notes below.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Gage height / discharge | ✅ built | Sync now with a location near a US river; confirm rows in `environment/usgs-water/` + hub last-data |
| Flood-stage category | ✅ built | confirm action/flood/moderate/major thresholds populate `extra.flood_stage` |
| Non-US / no-gauge | ✅ tested | enable with a location with no nearby gauge; confirm graceful skip + UI hint, no error |

## Build notes (2026-06-21)

Built as a Periodic follower using the bound `environment` contract (`EnvReading`). Key
decisions:

- **API confirmed live:** modernized OGC API at `api.waterdata.usgs.gov/ogcapi/v0`.
  Collections used: `monitoring-locations` (bbox + CQL `site_type_code='ST'`),
  `latest-continuous` (CQL `monitoring_location_id='...' AND parameter_code='...'`),
  `time-series-metadata` (flood stage thresholds — PascalCase keys: `Name`, `Type`,
  `Periods[].ReferenceValue`).
- **Field names confirmed from live API:** `monitoring_location_id`, `parameter_code`,
  `time` (UTC, `+00:00`), `value` (string), `unit_of_measure`. Agency is
  `agency_code`; site number is `monitoring_location_number`; combined id is
  `<agency_code>-<monitoring_location_number>`.
- **Metrics:** `water_level`/`ft` (param `00065`), `discharge`/`cfs` (param `00060`).
- **Flood-stage annotation:** best-effort via `time-series-metadata`; `ThresholdAbove`
  entries with names containing "action", "flood stage", "moderate", "major" yield
  `extra.flood_stage = "action"|"flood"|"moderate"|"major"|"below_action"`.
- **No new dep:** inline `urlencoding` mod handles CQL filter percent-encoding.
- **14 tests, all green.** `cargo check` clean (pre-existing warnings only).

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §USGS Water
Services (L2096–L2103). Feasibility 🟢 high but US-only. More granular than
the Open-Meteo GloFAS flood model for US users (real flood-stage categories
vs. modeled discharge). Pairs with USGS earthquakes + NWS alerts for a full
US ambient-hazard monitor. Build later — narrower audience than AQI or
quakes. Use the modern API; legacy endpoint is fallback only.
