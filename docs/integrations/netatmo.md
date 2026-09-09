# Netatmo Weather Station

- **id:** `netatmo`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings
  shape; the personal-weather-station sub-shape is shared with Tempest and
  Ambient Weather, cross-cutting note 8)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `/api/getmeasure` with a date-range watermark)
- **connection:** `netatmo` — OAuth (app registered at dev.netatmo.com —
  free; client_id + client_secret, standard OAuth 2.0 consent). Also covers
  Netatmo HOME Coach (indoor air quality) on the same API — a second def
  could share this connection later.
- **evidence:** official-docs — dev.netatmo.com/apidocumentation/weather
  (REST, OAuth 2.0, `/api/getmeasure` with start/end timestamps); mature
  ecosystem (pyatmo, PHP SDK, Go CLI)
- **effort / priority:** S / P2
- **needs:** Needs-login (dev.netatmo.com BYO app — David doesn't have a Netatmo station)

## What it is

Popular prosumer personal weather station: outdoor temperature, humidity,
pressure, rain, wind, plus indoor temperature, humidity, CO2 and noise from
the base station and add-on modules. Hyper-local ground truth that
complements the already-built Open-Meteo `weather/` collector (regional
context) at read time. One of the easiest wins in the home domain — official
API, full historical backfill, no stated retention limit.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Outdoor readings | all (module-dependent) | temp, humidity, pressure | official docs |
| Rain / wind | requires rain/wind modules | rain mm, wind speed/gusts | official docs |
| Indoor readings | base station + indoor modules | temp, humidity, CO2 ppm, noise dB | official docs |
| Historical backfill | all | `/getmeasure` by date range, configured intervals, no stated retention limit | official docs |

All optional in the contract — a station without a rain gauge simply carries
no rain fields. No module-specific code paths; module type rides along.

## Access & auth

- REST at api.netatmo.com per dev.netatmo.com docs; OAuth 2.0. The user (or
  Trove's baked app, per the ConnectSpec baked+BYO model) registers a free
  app at dev.netatmo.com for client_id + client_secret; user consent is
  mandatory.
- Key endpoint: `/api/getmeasure` — historical observations for any owned
  station, start/end timestamps, values at the station's configured
  intervals. Station list via the stations-data endpoint.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `home/netatmo/raw/YYYY-MM.jsonl` — getmeasure responses
  keyed by station/module, full fidelity.
- **Contract layer:** `home/netatmo/YYYY-MM.jsonl` per the (pending) home
  contract — one row per reading interval per module (`ts`, `source`,
  `guid`, station/module ids, normalized reading fields: temp_c,
  humidity_pct, pressure_hpa, wind_mps, rain_mm, co2_ppm, noise_db),
  overflow in `extra`. Owned device → `home/`, not `environment/`;
  merges with `weather/` at read time per the taxonomy rule.
- **Dedupe:** `guid` = module id + interval timestamp; cursor in
  `.trove/netatmo-sync.json`, rebuildable.

## Build notes (Phase 4)

Built as a `home.HomeReading` follower (reuse-bound contract, same shape as
ambient_weather). Module: `crates/trove-core/src/netatmo.rs`. Connection:
`pub static CONNECTION: ConnectionDef` (OAuth; BYO app at dev.netatmo.com;
redirect port 38739 = 38580 + 159).

API field names are CamelCase (`Temperature`, `Humidity`, `CO2`, `Noise`,
`Pressure`, `AbsolutePressure`, `WindStrength`, `WindAngle`, `GustStrength`,
`GustAngle`, `Rain`, `sum_rain_1`, `sum_rain_24`) confirmed from
philippelt/netatmo-api-python source. `getstationsdata` → module topology
snapshot; `getmeasure` (optimize=false, scale=30min) → historical per-module
per-type readings. Per-module per-type watermark in `.trove/netatmo-sync.json`.

14 unit tests pass (cargo test -p trove-core netatmo::). cargo check clean.
Validation blocked on Needs-login (real Netatmo station required).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Outdoor + indoor readings | — | OAuth with a real Netatmo account; Sync now; confirm reading rows in `home/netatmo/` + hub last-data (needs a station owner — David doesn't have one) |
| Historical backfill | — | fresh connect on a long-lived station; confirm rows reach back years |
| Rain/wind modules | — | a station with those modules; confirm the optional fields appear |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Netatmo Personal
Weather Station (L1880–L1886). Feasibility 🟢 high. Cross-cutting note 8:
Netatmo / Ambient Weather / Tempest share one personal-weather-station
sub-shape — draft it once in the Phase 3 home contract and map all three.
Netatmo HOME Coach (IAQ) uses the same API and connection; catalogue it as a
capability extension here rather than a separate provider if it's ever
prioritized.
