# Sunrise-Sunset.org

- **id:** `sunrise-sunset`
- **domains:** `environment/` (contract: **environment.almanac** — reuses
  `crate::environment::Almanac`, the same shape USNO writes)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (one daily pull per location; checked every 6 h, gate
  fires at most once per local day)
- **connection:** none — fully keyless REST. Location shared from USNO's
  non-secret cursor (`.trove/usno-sync.json`).
- **evidence:** official-docs — api.sunrise-sunset.org (confirmed live
  2026-06-17; UTC RFC3339 times, day_length in seconds) + api.sunrisesunset.io
  (confirmed live 2026-06-17 via curl; real field shape: `utc_offset` integer
  minutes, `dawn`/`dusk` = civil, `first_light`/`last_light` = astronomical,
  `nautical_twilight_begin/end` real fields; moon data present).
- **effort / priority:** S / P2
- **needs:** none

## What it is

A keyless public REST feed of daily solar geometry — sunrise, sunset, solar
noon, civil/nautical/astronomical twilight, day length, and golden hour — for
any lat/lon and date. Daily enrichment data that contextualizes the rest of the
vault. **Keyless fallback for USNO Astronomy**, which is authoritative and adds
moon data. Uses two sibling endpoints:

- Primary: `api.sunrisesunset.io` — adds `golden_hour` over the .org baseline.
- Fallback: `api.sunrise-sunset.org` — confirmed live; all twilight stages
  (civil/nautical/astronomical — a superset of USNO's civil-only output); no
  `golden_hour`.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Solar times | none (keyless) | sunrise, sunset, solar_noon | confirmed live (.org) |
| Day length | none (keyless) | day_length (seconds→"HH:MM") | confirmed live (.org) |
| Civil twilight | none (keyless) | civil_twilight_begin/end | confirmed live (.org) |
| Nautical twilight | none (keyless) | nautical_twilight_begin/end | confirmed live (.org) |
| Astronomical twilight | none (keyless) | astronomical_twilight_begin/end | confirmed live (.org) |
| Golden hour | sunrisesunset.io only | golden_hour | confirmed live (.io, 2026-06-17) |
| Moon times | sunrisesunset.io only | moonrise, moonset, moon_phase | confirmed live (.io, 2026-06-17) |
| Moon illumination | sunrisesunset.io only | extra.moon_illumination | confirmed live (.io, 2026-06-17) |
| Solar geometry | sunrisesunset.io only | extra.sun_altitude/azimuth, elevation | confirmed live (.io, 2026-06-17) |

## Access & auth

- Primary: `GET https://api.sunrisesunset.io/json?lat=LAT&lng=LNG&date=YYYY-MM-DD`
  → times in 12-hour local format + `utc_offset` **integer minutes** (e.g. -420)
  + `golden_hour` + moon data + solar geometry. Civil twilight: `dawn`/`dusk`.
  Astronomical twilight: `first_light`/`last_light`. Nautical: `nautical_twilight_begin/end`.
- Fallback: `GET https://api.sunrise-sunset.org/json?lat=LAT&lng=LNG&date=YYYY-MM-DD&formatted=0`
  → UTC RFC3339 timestamps (confirmed live 2026-06-17).
- Keyless, no auth, no TCC, plain HTTPS. Location shared from USNO's cursor.

## Location sharing

No connect card (no `CONNECTION`). Reads lat/lon from USNO's non-secret cursor
(`.trove/usno-sync.json`, `location.lat` / `location.lon`). If USNO is not
configured, the pull quietly skips with a clear error. One lat/lon configured
for both.

## Vault mapping

- **Raw layer:** `environment/sunrise-sunset/raw/YYYY-MM.jsonl` — full API
  response, one row per (date, location), partitioned by month of `date`.
- **Contract layer:** `environment/sunrise-sunset/almanac/YYYY-MM.jsonl` —
  `environment::Almanac` rows, deduped by `date` + rounded lat/lon (same dedupe
  key as USNO). `day_length` normalized to `"H:MM"` from both endpoints
  (.org: seconds→H:MM; .io: "H:MM:SS"→"H:MM"; raw seconds / raw string in
  `extra`). `.io` populates `moonrise`/`moonset`/`moon_phase` from confirmed
  live fields; `moon_illumination`, `sun_altitude/azimuth`, `elevation` go to
  `extra`. `.org` has no moon data — use USNO for moon when .io is unavailable.
  `.org` UTC timestamps are converted to local-offset RFC3339 when the `.io`
  offset is available; otherwise stored as UTC (valid RFC3339 instants).
- **Dedupe:** `date@lat,lon` (4 decimal places); idempotent re-pulls skip
  already-stored days.

## Build notes (2026-06-17, updated 2026-06-17 fix)

- Replaced NotWired stub with full Periodic implementation.
- `Behavior::Periodic { cadence: Cadence::daily(6h), collect, pull }`.
- Location from USNO cursor — no ConnectionDef; `connection: None`.
- `.io` as primary (golden_hour + moon data), `.org` as fallback.
- **Fallback logic (fixed):** If `.io` returns HTTP 200 but fails to map into
  an Almanac, the code now falls back to `.org` (previously the silent-zero bug:
  mapping failure was not retried). The `utc_offset` from `.io` is extracted
  from the raw response body and passed to `.org` for local-offset conversion.
- **utc_offset (fixed):** Real `.io` API sends `utc_offset` as integer minutes
  (e.g. -420), NOT a `"+HH:MM"` string. `IoResults.utc_offset` is now `Option<i64>`.
- **Twilight field names (fixed):** Confirmed live: `.io` uses `dawn`/`dusk` for
  civil twilight and `first_light`/`last_light` for astronomical twilight.
  `nautical_twilight_begin/end` are real fields. The six `*_twilight_*` keys
  (civil/nautical/astronomical) are NOT in `.io` responses; `IoResults` updated.
- **UTC→local conversion (fixed):** `.org` UTC RFC3339 timestamps converted to
  local-offset RFC3339 using the `.io` offset when available. Contract specifies
  RFC3339 local time.
- **day_length normalization (fixed):** `.io` emits "H:MM:SS"; truncated to
  "H:MM" to match `.org` and the contract schema example. Raw value in extra.
- **Moon data (fixed):** `.io` confirmed live: `moonrise`, `moonset`,
  `moon_phase`, `moon_illumination` all present. Now mapped to contract fields /
  extra instead of empty strings.
- Raw layer unconditional; contract via `crate::environment::Almanac`.
- Backfill capped at 35 days (same as USNO).
- 18 tests green; cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Solar + twilight times (.org) | ✅ tested | unit tests against confirmed live .org response |
| All twilight stages | ✅ tested | civil/nautical/astronomical confirmed in .org fixture |
| day_length conversion | ✅ tested | 51948s → "14:25" in unit tests |
| Fallback .org when .io fails | ✅ tested | mock API test: io error → falls back |
| Golden hour (.io) | ✅ tested | confirmed live; unit test validates golden_hour in almanac |
| Moon times (.io) | ✅ tested | confirmed live; unit test validates moonrise/moonset/moon_phase |
| .io map-fail falls back to .org | ✅ tested | regression test: string utc_offset → io None → org row written |
| .org UTC→local conversion | ✅ tested | unit test with -420 offset; sunset crosses midnight correctly |
| day_length normalization | ✅ tested | "14:25:48" → "14:25" in H:MM form, raw in extra |
| Dedup re-runs | ✅ tested | mock double-pull writes 0 new rows |
| Backfill cap | ✅ tested | year-old watermark → MAX_BACKFILL_DAYS rows |

## Research notes

`integrations-research.md` → "Environment & Ambient Context"
§sunrise-sunset.org / SunriseSunset.io (L2128–L2135). Feasibility 🟢 high.
USNO's `rstt/oneday` endpoint covers the same data plus moon phases. Sunrise-
sunset fills the gap with golden_hour (via .io) and all three twilight stages
(civil/nautical/astronomical vs. USNO's civil-only). Both keyless; build USNO
primary, this as a keyless backup. Contract: `environment::Almanac` (same shape
as USNO).
