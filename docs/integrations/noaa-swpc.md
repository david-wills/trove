# NOAA Space Weather (SWPC)

- **id:** `noaa-swpc`
- **domains:** `environment/` (contract: **Phase 3 pending** — environment
  ambient-readings/events contract drafted in the contract pass; until then,
  per-source raw under `environment/noaa-swpc/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll every 3h matching the Kp cadence; watermark on
  observation timestamp)
- **connection:** none (keyless)
- **evidence:** official-docs — services.swpc.noaa.gov/json/ (keyless JSON
  endpoints, documented files and cadence)
- **effort / priority:** S / P1
- **needs:** none

## What it is

NOAA's Space Weather Prediction Center — near-real-time geomagnetic and solar
data: the planetary Kp index, geomagnetic storm alerts, and the OVATION aurora
footprint grid. The headline use is "can I see the aurora tonight?": Kp ≥ 5
means aurora is visible at mid-latitudes, and the OVATION grid gives a
latitude × longitude probability map. A fully keyless US government feed →
`environment/`.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Planetary Kp index | free, keyless | Kp value, 3-hour intervals, 30-day history | official docs |
| Geomagnetic alerts | free, keyless | storm watches/warnings, message text | official docs |
| Aurora footprint (OVATION) | free, keyless | lat × lon aurora probability grid, intensity | official docs |
| Forecast | free, keyless | 3-day / 45-day Kp forecast, solar probabilities | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- REST: keyless JSON at services.swpc.noaa.gov/json/ —
  `planetary_k_index_1m.json` (30-day Kp history), `ovation_aurora_latest.json`
  (aurora grid, ~5-min updates), `45-day-forecast.json`,
  `solar_probabilities.json`; alerts at `/products/alerts.json`.
- Rate limits: public service, generous; a 3-hourly poll is trivial.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `environment/noaa-swpc/raw/YYYY-MM.jsonl` — the API objects
  (Kp series, alerts, OVATION snapshots), full fidelity.
- **Contract layer:** `environment/noaa-swpc/YYYY-MM.jsonl` per the (pending)
  environment contract — expected shape: one row per Kp reading / alert (`ts`,
  `source`, `guid`, `kp`, alert fields + overflow in `extra`). The OVATION grid
  is a bulky snapshot (an artifact, not an event) — store it sidecar in the raw
  layer rather than flooding the event rows. Same-shaped readings merge with
  other environment sources at read time.
- **Dedupe:** `guid` = (metric, timestamp) for Kp; alert id for alerts; cursor
  in `.trove/noaa-swpc-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/noaa_swpc.rs`: `DEF` (Periodic, every 3h),
   `pull` hook. No connection (keyless).
2. Registration line in `INTEGRATIONS`.
3. Fixtures from documented sample responses (Kp series, an alert, an OVATION
   snapshot); parser + store + cursor tests, unique temp dirs.
4. OVATION grid stored sidecar (not expanded into event rows). A "can I see
   aurora tonight?" read (Kp ≥ 5 + user latitude vs the grid) is a read-time
   concern, not a write-time one.
5. Vault writes via `store` helpers once the environment contract is ratified;
   until then **parked behind Needs-David (contract)** while raw writes proceed.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Kp index | ✅ built | `cargo test -p trove-core noaa_swpc::` — parse_kp_maps_contract_fields, watermark tests |
| Alerts | ✅ built | parse_alerts_maps_contract_fields; alert_repoll_upserts_not_duplicates |
| OVATION grid | ✅ built (raw sidecar) | pull_writes_kp_readings_alerts_and_raw confirms "Observation Time" in raw; 6 lines total |

## Build notes (2026-06-15)

- Confirmed exact field names via live API: Kp → `time_tag`/`kp_index`/`estimated_kp`/`kp`; alerts → `product_id`/`issue_datetime`/`message`.
- Kp `time_tag` is UTC naive (`YYYY-MM-DDTHH:MM:SS`); parsed via `NaiveDateTime → Utc → Local`.
- Alert `issue_datetime` has fractional seconds (`2026-06-15 12:35:56.833`); tolerates both forms.
- Alert guid derived from `Serial Number:` line in message body; fallback is `product_id + issue_datetime`.
- OVATION grid stored as raw-only sidecar (raster array is too bulky for event rows).
- `contract_mode = reuse-bound` (`environment` domain): Kp → `EnvReading`, alerts → `EnvGeoEvent`.
- Watermark (`last_kp_ts`) advances only after all writes succeed; OVATION failure tolerated (primary data still lands).
- 17 tests, all passing. No new deps. No new ConnectionDef (keyless).

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §NOAA SWPC Space
Weather / Aurora (L2064–L2071). Feasibility 🟢 high — fully keyless, stable US
government service, near-real-time. Poll every 3h to match the Kp update
cadence. Kp ≥ 5 = aurora at mid-latitudes; the OVATION grid powers visibility
alerts. SWPC also offers email push subscriptions, but JSON polling is simpler
and standalone-clean for Trove.
