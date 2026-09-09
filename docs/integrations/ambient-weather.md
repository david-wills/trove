# Ambient Weather

- **id:** `ambient-weather`
- **domains:** `home/` — **first collector in the `home` domain**, binds the
  `home.reading` contract (Rust type `HomeReading`, RATIFIED this build). One
  owned-device scalar reading per metric; merges with `environment/` (public
  feeds) and other `home/` sources at read time by `metric` (ownership, not
  shape, decides the folder).
- **status:** 🧪 built (fixture-tested, not validated — needs a real Ambient
  Weather account + station to confirm)
- **unavailable_reason:** none
- **behavior:** Periodic (hourly cloud REST poll + first-sync history backfill)
- **connection:** `ambient-weather` — TokenPaste (**two** keys, both required:
  API Key + Application Key, generated from the user's ambientweather.net
  account; pasted as one `apiKey:applicationKey` string). Not shared with other
  defs.
- **evidence:** official-docs — REST API documented at
  ambientweather.docs.apiary.io
- **effort / priority:** S / P2
- **needs:** **time-sensitive** (cloud deletes data after 1 year — backfill
  promptly on connect) · **Needs-login** (real account + station for
  real-data validation; the build proceeds from the documented REST shapes)

## What it is

Ambient Weather personal weather stations (WS-2902 family and kin), read
through the ambientweather.net cloud. Hyper-local outdoor
temp/humidity/wind/rain/UV/solar plus indoor temp/humidity from the
console. The companion to WeatherFlow Tempest for the other big PWS brand
— simple key auth, no OAuth friction. The cloud's 1-year retention makes
Trove the only place this data survives long-term.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Device list | free account | station IDs + metadata | official REST docs |
| Real-time observations | free | outdoor temp/humidity/wind/rain/UV/solar, indoor temp/humidity | official REST docs |
| Historical observations | free — 5-min res ≤1 yr, 30-min older, **deleted after 1 year** | same fields by time range | official REST docs |

All optional in the contract (sensor mix varies by station model;
omit-if-empty).

## Access & auth

- REST: `api.ambientweather.net/v1/devices` — requires API Key +
  Application Key (both generated in the ambientweather.net account; the
  connect card must ask for both and say so plainly).
- Station IDs come from the device-list response.
- Retention: 1 year at 5-min resolution, 30-min beyond that, then deleted
  — first sync does a full backfill; the periodic pull keeps current.
- No TCC. Standalone-clean (plain outbound HTTPS).

## Vault mapping (as built)

- **Contract layer:** `home/ambient-weather/YYYY-MM.jsonl` — one
  [`HomeReading`] per sensor metric per observation (each API observation
  fans out: `tempf` → `temperature`/`F`, `humidity` → `humidity`/`percent`,
  `windspeedmph` → `wind_speed`/`mph`, indoor temp/humidity get an `_indoor`
  metric suffix so they don't collide, …). `ts` = the row's `date` (UTC →
  local), `device` = station MAC, `place` = station name, `lat`/`lon` from the
  device's coords; source-specific overflow rides in `extra`. Monthly
  partitions keyed on `ts`. Read-time views merge with `environment/` (public
  feeds) and other `home/` sources by `metric` — same scalar core.
- **Raw layer:** `home/ambient-weather/raw/YYYY-MM.jsonl` — the verbatim API
  observation object, full fidelity, unconditional.
- **Dedupe:** stable per-device-metric-time key
  `ambient-weather:{mac}:{metric}:{dateutc}`. Per-device watermark (max
  `dateutc` written) in a rebuildable, **non-secret** cursor at
  `.trove/ambient-weather-sync.json` (not under `.trove/sync/`); a device's
  watermark only advances after its full backward drain, so a crash re-drains
  rather than skips. Secrets (the two keys) live under `.trove/sync/` (0600),
  never in the cursor or any non-secret file.

## Build plan

1. Module `crates/trove-core/src/ambient_weather.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste with **two** fields — API Key + Application
   Key — help copy pointing at the account page), pull hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. First-sync full backfill (walk history to the 1-year horizon), then
   incremental — the time-sensitivity is the whole point; don't defer
   backfill to a later iteration.
4. Fixtures from the apiary-documented JSON (device list + observation
   pages, sparse-sensor variant included); parser/store/cursor tests,
   unique temp dirs.
5. Contract rows wait on the home contract; raw layer can land first.

## Validation matrix

Fixture-tested (parser/store/cursor + the `home.reading` contract round-trip,
all green). **Real-data validation is Needs-David / Needs-login:** it requires
an Ambient Weather account *and* a physical station reporting to it (no
hardware-free path exists — any Ambient Weather owner's run validates equally).

| Capability | Status | How David validates (exact steps) |
|---|---|---|
| Keys + device list | — | On ambientweather.net → account page, create an **API Key** and an **Application Key**. In Trove, open the Ambient Weather card → Connect, and into the single **"API Key and Application Key"** field paste them as `apiKey:applicationKey` (colon-separated, API key first). Connect should succeed (it calls `GET /v1/devices`) and the hub card's last-data should populate. |
| Full backfill | — | Click **Sync now** once. Confirm a `home/ambient-weather/YYYY-MM.jsonl` partition appears for each recent month and the verbatim mirror lands under `home/ambient-weather/raw/`. Spot-check that the oldest readings sit near the 1-year retention horizon (older data is already deleted cloud-side). |
| Per-metric fan-out | — | Open any one observation's month file and confirm one row per sensor metric (e.g. `temperature`/`F`, `humidity`/`percent`, `wind_speed`/`mph`, indoor as `temperature_indoor`), each carrying `device` (station MAC), `place` (station name), and `lat`/`lon`. |
| Incremental pull | — | Sync now again after the station reports new data; confirm only new observations append (no duplicate `ambient-weather:{mac}:{metric}:{dateutc}` keys) and the cursor `.trove/ambient-weather-sync.json` advances. |

**Needs-David:** decide whether to bake an `applicationKey` into the build
(`TROVE_AMBIENT_WEATHER_APPLICATION_KEY`, currently empty) so users paste only
their `apiKey`; today both keys must be pasted.

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Ambient Weather
(L1808–L1814). Feasibility 🟢 high. The 1-year deletion policy is the
standing risk: data older than a year is already gone at connect time, so
"import-soon" framing matters in the UI. Both keys are required — a
single-field token form would fail confusingly. Complements WeatherFlow
Tempest (different brand, same Phase 3 home-readings shape).
