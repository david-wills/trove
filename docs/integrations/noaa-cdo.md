# NOAA Climate Data Online (CDO)

- **id:** `noaa-cdo`
- **domains:** `environment/` (contract: **Phase 3 pending** — environment
  ambient-readings contract drafted in the contract pass; until then,
  per-source raw under `environment/noaa-cdo/`)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (in practice a one-time/occasional historical
  backfill rather than a recurring poll; watermark on date range)
- **connection:** `noaa-cdo` — TokenPaste (free token, instant email
  registration at ncdc.noaa.gov/cdo-web/token). Not shared with other defs.
- **evidence:** official-docs — ncei.noaa.gov/cdo-web/api/v2 (documented
  datasets, params, rate limits)
- **effort / priority:** M / P2
- **needs:** Needs-David (icebox-leaning — Open-Meteo ERA5 archive, keyless and
  already planned, covers the same historical-backfill use case; build only on
  demand)

## What it is

NOAA's Climate Data Online — daily historical weather observations from actual
ground stations (GHCND, the Global Historical Climatology Network): temp
max/min, precipitation, snow, wind, going back decades. The draw over a
reanalysis archive is station-specific ground truth — verifying records against
a real nearby NOAA station rather than a model grid cell. A public feed →
`environment/`.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| GHCND daily observations | free token | date, station, TMAX/TMIN, PRCP, SNOW, wind | official docs |
| Station discovery | free token | station id, name, coords, data coverage | official docs |

All optional in the contract (omit-if-empty).

## Access & auth

- REST: `GET /cdo-web/api/v2/data?datasetid=GHCND&locationid=…&startdate=…&
  enddate=…&limit=1000` — token passed in a header. Station/location lookup
  endpoints support discovery.
- Auth: free token via instant email registration (arrives in a few minutes).
  Rate limits 5 req/sec, 10k req/day — generous for one-time backfill.
- No TCC, no local files. Standalone-clean (plain HTTPS). Query complexity is
  the real cost: station discovery is required before pulling observations.

## Vault mapping

- **Raw layer:** `environment/noaa-cdo/raw/YYYY-MM.jsonl` — the API observation
  records, full fidelity.
- **Contract layer:** `environment/noaa-cdo/YYYY-MM.jsonl` per the (pending)
  environment contract — expected shape: one row per station-day observation
  (`ts` = observation date, `source`, `guid`, the measured values, station id
  + overflow in `extra`). Same-shaped readings merge with other environment
  weather sources at read time.
- **Dedupe:** `guid` = (station id, date, datatype); cursor in
  `.trove/noaa-cdo-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/noaa_cdo.rs`: `DEF` (Periodic, but built for
   bounded backfill runs), `CONNECTION` (TokenPaste: label/help/placeholder per
   the SimpleFIN affordance rule), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Two-step query: nearest-station discovery from user coordinates, then the
   data pull. Fixtures from documented sample responses (station list + GHCND
   data); parser + store + cursor tests, unique temp dirs.
4. **Icebox gate:** do not build ahead of demand — Open-Meteo's keyless ERA5
   archive already covers historical backfill. Parked behind Needs-David until
   a user wants station-level ground truth.
5. Vault writes via `store` helpers once the environment contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| GHCND observations | — | register a token; paste it in the connect card; set a location near a known GHCND station; run a bounded backfill; confirm rows in `environment/noaa-cdo/` + hub last-data |
| Station discovery | — | confirm the nearest-station lookup resolves a station id from coordinates before the data pull |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §NOAA Climate Data
Online (L2168–L2175). Feasibility 🟡 medium. Explicitly recommended as icebox:
Open-Meteo ERA5 (back to 1940, keyless) covers the backfill use case; CDO adds
authoritative station ground truth but costs an API key and station-discovery
query complexity. Build only if a user demands station-level observations.
