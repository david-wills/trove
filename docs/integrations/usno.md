# USNO Astronomy

- **id:** `usno`
- **domains:** `environment/` — **binds the `environment` *almanac* shape**
  (the third and final environment record shape; reading + geo-event were
  already bound). First collector to write an almanac; `sunrise-sunset` (a
  sun-only feed) is the planned second writer.
- **status:** 🧪 built — awaiting David validation
- **unavailable_reason:** none
- **behavior:** Periodic (checked every 6 h, runs once per local day, one
  daily astronomy pull per configured location)
- **connection:** `usno` (keyless, but a **location** must be configured — a
  `lat,lon[,Place]` coordinate pasted into the connect card, stored locally as
  plain non-secret config; nothing else is ever sent)
- **evidence:** official-docs + 5 live probes — `aa.usno.navy.mil/api/rstt/oneday`
  (valid 1700–2100); documented JSON responses, US Navy service
- **effort / priority:** S / P1
- **needs:** Needs-David — paste a location into the USNO connect card, then
  Sync now (see Validation matrix)

## What it is

The US Naval Observatory Astronomical Applications API is the authoritative,
keyless source for daily solar and lunar data: sunrise, sunset, solar noon,
**civil twilight** (begin/end), moonrise, moonset, moon phase, and
illumination. It's a daily ambient-context enrichment — golden hour, dark
hours, full moons — that correlates with photography, sleep, mood, and outdoor
activity. Globally valid for any coordinate.

> **Twilight scope (a deliberate capability bound):** `rstt/oneday` computes
> **civil twilight only** — confirmed by the official docs ("also computes the
> times at which civil twilight begins and ends") and by 5 live probes (LA
> summer/winter, London solstice, equator equinox, Svalbard winter), which
> return only `Begin/End Civil Twilight`. No USNO endpoint returns nautical or
> astronomical twilight, so the contract's `nautical_*`/`astronomical_*` fields
> stay empty for this source (omit-if-empty, available for a future feed that
> does report them). USNO's edge over a plain sunrise/sunset feed is its
> authoritative **moon** data + civil-twilight bounds, not extra twilight stages.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Sun/civil twilight | none (keyless) | sunrise, sunset, solar noon, **civil** twilight begin/end (nautical/astro not emitted by any USNO endpoint) | official docs (`rstt/oneday`) + 5 live probes |
| Moon (daily) | none | moonrise, moonset, phase, illumination fraction (verbatim + normalized to `extra.illumination` 0–1) | official docs |
| Golden hour | none (computed) | sunset − 1 h (`golden_hour`), computed since USNO doesn't name it | research note (derived) |

All optional in the contract (omit-if-empty). A polar day/night row (e.g. Svalbard summer) cleanly omits the rise/set events that don't occur and keeps transit + moon.

## Access & auth

- **One-day:** `https://aa.usno.navy.mil/api/rstt/oneday?date=YYYY-MM-DD&coords=LAT,LON&tz=OFFSET&dst=true`
  — sun, twilight, moon for the day.
- **Year phases:** `https://aa.usno.navy.mil/api/moon/phases/year?year=YYYY`;
  date-anchored: `…/moon/phases/date?date=YYYY-MM-DD&nump=N`.
- Truly keyless (optional 8-char id for tracking only). JSON. Standalone-clean
  plain HTTPS, no TCC. ~2 calls/day per location.

## Vault mapping

- **Raw layer:** `environment/usno/raw/YYYY-MM.jsonl` — the API day objects,
  full fidelity (unconditional).
- **Contract layer:** `environment/usno/almanac/YYYY-MM.jsonl` — the bound
  [`Almanac`] shape: one row per day per location, keyed by `date` (+ rounded
  `lat`/`lon`). An almanac has **no `ts` and no `guid`** — it is reference
  geometry partitioned by the month of `date`. Sun/civil-twilight/moon fields;
  `fracillum` rides verbatim in `extra` and is also normalized to a numeric
  `extra.illumination` (0–1); computed `golden_hour` (sunset − 1 h).
- **Dedupe:** `(date, lat, lon)` — the almanac's natural key (rounded coords);
  cursor in `.trove/usno-sync.json` (non-secret: location + date watermark),
  rebuildable by scanning output. Backfill from the watermark through today is
  capped (`MAX_BACKFILL_DAYS`) so a long gap can't storm the API.

## How it shipped

- Module `crates/trove-core/src/usno.rs`: `DEF` (Periodic — checked every 6 h,
  runs once per local day). `pull` issues `rstt/oneday` per configured
  location, assembles each day into an [`Almanac`], writes raw + contract.
- Binds the `environment` **almanac** shape (`crate::environment::Almanac`,
  re-exported from `contracts`/`lib`) — a sub-bind under the already-bound
  `environment` domain (no new `DOMAINS` entry; the almanac shape was the last
  draft in the environment contract, now ratified).
- `CONNECTION` (`usno`): keyless, but a **location** (`lat,lon[,Place]`) is
  pasted via the connect card and stored as plain non-secret config. The pull
  skips with a clear error until a location is set.
- Golden hour computed at parse time (sunset − 1 h) and stored in the row.
  `fracillum` normalized to `extra.illumination` (0–1) alongside the verbatim
  string; `closestphase` rides in `extra`.
- Fixtures + tests (unique temp dirs): 3 `Almanac` round-trip tests in
  `environment.rs` (minimal/full/unknown-field) + 18 collector tests in
  `usno.rs` (LA full day, Arctic midnight-sun, civil-twilight-only, backfill
  cap, dedupe re-run, location validation/rounding, connection token-paste).

## Validation matrix

**Setup (once):** Open the USNO connect card in the hub. In the
**Location (latitude, longitude)** field, paste a coordinate — e.g.
`34.05,-118.25` (or `34.05,-118.25,Los Angeles` to attach a place label).
There is **no key, token, or account** — USNO is keyless; only this coordinate
is ever sent, and it is stored locally as plain config (not a secret). Then
press **Sync now**.

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Sun/civil twilight | 🧪 awaiting David | After Sync now, open `environment/usno/almanac/YYYY-MM.jsonl` (current month). Confirm a row for today (and any backfilled days) with `sunrise`/`sunset`/`solar_noon` + `civil_twilight_begin`/`civil_twilight_end`, all RFC3339 *local* with the location's offset. `nautical_*`/`astronomical_*` stay absent (USNO emits civil only — expected). Hub card shows last-data. |
| Moon | 🧪 awaiting David | Same row: confirm `moonrise`/`moonset`/`moon_phase` populate and `extra.illumination` is a 0–1 number (with the verbatim `fracillum` string also in `extra`). |
| Golden hour | 🧪 awaiting David | Same row: confirm `golden_hour` ≈ 1 h before `sunset` (computed, not from USNO). |
| Raw fidelity | 🧪 awaiting David | Confirm `environment/usno/raw/YYYY-MM.jsonl` holds the unmodified API day object(s). |
| Idempotent re-pull | 🧪 awaiting David | Press Sync now again the same day; confirm no duplicate row is appended for a `(date, lat, lon)` already stored. |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §USNO
Astronomical Applications API (L2032–L2039) + §USNO Moon Phases API
(L2136–L2143). Feasibility 🟢 high — no auth, US-government-maintained, valid
1700–2100. As shipped, **one** `rstt/oneday` call per day returns sun, **civil**
twilight, and same-day moon together — one call/day per location. Golden hour
is computed (sunset − 1 h), not a named USNO field. sunrise-sunset.org /
SunriseSunset.io are keyless fallbacks (sun-only — the planned second almanac
writer); prefer USNO (authoritative + moon data). Open-Meteo already supplies
daily sunrise/sunset in the weather stream — USNO layers the authoritative
**moon** data (rise/set/phase/illumination) and **civil**-twilight bounds on
top. (Verified by docs + 5 live probes: no USNO endpoint returns nautical or
astronomical twilight; the moon data, not extra twilight stages, is the edge.)
