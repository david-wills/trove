# NOAA Buoys (NDBC)

- **id:** `noaa-ndbc`
- **domains:** `environment/` (contract: **Phase 3 pending** — environment
  ambient-readings contract drafted in the contract pass; until then,
  per-source raw under `environment/noaa-ndbc/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (hourly fetch of the nearest station's realtime file;
  watermark on observation timestamp)
- **connection:** none (keyless)
- **evidence:** official data files — ndbc.noaa.gov/data/realtime2/<station>.txt
  (fixed-width plain text) + activestations.xml; **Needs-sample** (parser-last:
  the fixed-width format has no JSON variant; build the parser against a real
  fetched file)
- **effort / priority:** M / P2
- **needs:** Needs-sample (fixed-width text parser must be built against a real
  file)

## What it is

NOAA's National Data Buoy Center — real-time observations from ~1,000 physical
buoys and coastal stations across US waters and the Great Lakes: wave height,
wave period, swell, wind, air/water temperature, pressure. Complements
Open-Meteo Marine (a model forecast) with actual observed data. Narrow audience
(coastal/marine users). A public feed → `environment/`.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Standard met observations | free, keyless | wave height/period, swell, wind speed/dir, air/water temp, pressure | official data files |
| Station discovery | free, keyless | station id, name, coords, type | activestations.xml |

All optional in the contract (omit-if-empty).

## Access & auth

- HTTP data files: `GET /data/realtime2/{STATION}.txt` — standard
  meteorological file, updated hourly, **fixed-width plain text** (not JSON).
  Station list: `/activestations.xml`. Standard met files roll 45 days;
  decades of history at ndbc.noaa.gov/historical_data.shtml.
- Auth: none, keyless.
- No TCC, no local files. Standalone-clean (plain HTTPS). The fixed-width
  format requires a custom column parser — the main build cost here.

## Vault mapping

- **Raw layer:** `environment/noaa-ndbc/raw/YYYY-MM.jsonl` — each parsed
  observation row, full fidelity (all columns the .txt file carries).
- **Contract layer:** `environment/noaa-ndbc/YYYY-MM.jsonl` per the (pending)
  environment contract — expected shape: one row per observation timestamp
  (`ts`, `source`, `guid`, the measured values, station id + overflow in
  `extra`). Same-shaped readings merge with other environment sources at read
  time.
- **Dedupe:** `guid` = (station id, observation timestamp); cursor in
  `.trove/noaa-ndbc-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/noaa_ndbc.rs`: `DEF` (Periodic, hourly),
   `pull` hook. No connection (keyless).
2. Registration line in `INTEGRATIONS`.
3. **Parser-last:** the fixed-width realtime2 format needs a custom parser
   built against a real fetched `.txt` file — flagged Needs-sample. Header row
   gives column units; missing values are sentinel-coded (e.g. `MM`).
4. Nearest-station detection from user coordinates via activestations.xml; skip
   silently if no station is within a sensible coastal radius.
5. Fixtures from a captured real `.txt` file (and the XML station list);
   parser + store + cursor tests, unique temp dirs.
6. Vault writes via `store` helpers once the environment contract is ratified;
   until then **parked behind Needs-David (contract)** while raw writes proceed.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Met observations | ✅ tested | Parser confirmed against live `ndbc.noaa.gov/data/realtime2/46042.txt`; 15 unit tests covering column parsing, MM-sentinel skipping, watermark, upsert dedupe, store round-trip |
| Station discovery | ✅ tested | `activestations.xml` attribute parser + haversine nearest-station selection + 500 km inland no-op gate all covered by unit tests |

## Build notes (2026-06-17)

- **Contract mode:** `reuse-bound` — writes `EnvReading` rows (`environment/noaa-ndbc/YYYY-MM.jsonl`) + raw layer (`environment/noaa-ndbc/raw/YYYY-MM.jsonl`). No new connection or deps.
- **Parser:** built against a live-fetched `46042.txt` (Monterey buoy). Fixed-width whitespace-separated; 2 header lines (names + units); `MM` / `999` sentinels for missing values; `+0.0` leading-plus on PTDY handled.
- **Station selection:** parses `activestations.xml` attributes (met="y" filter), haversine nearest within 500 km; cursor caches the station_id + coords so the station list is only re-fetched when location changes >50 km.
- **Watermark:** newest observation ts persisted in `.trove/noaa-ndbc-sync.json`; re-poll skips at-or-before rows. Watermark only advances after successful full drain.
- **Metrics emitted:** `wind_dir`, `wind_speed`, `wind_gust`, `wave_height`, `wave_period_dominant`, `wave_period_avg`, `wave_dir`, `pressure`, `air_temp`, `water_temp`, `dew_point`, `visibility`, `pressure_tendency`, `tide` — any column that is `MM` is simply omitted (no null rows written).
- All 15 unit tests pass; `cargo check` clean.

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §NOAA NDBC
(L2144–L2151). Feasibility 🟡 medium — keyless but the fixed-width text format
needs a custom parser (the reason this is parser-last / Needs-sample). US
coastal and Great Lakes only; narrow audience. Complementary to Open-Meteo
Marine: NDBC is observed buoy data, Open-Meteo is model forecast.
