# National Weather Service

- **id:** `nws`
- **domains:** `environment/` (contract: **Phase 3 pending** — `environment/`
  shape drafted across AQI / quakes / alerts / sun-moon / space-weather feeds;
  existing `weather/` stays grandfathered where it is)
- **status:** 🧪 built (fixture-tested; first-in-domain binding of `environment`; keyless; validate from a US location)
- **unavailable_reason:** none
- **behavior:** Periodic (poll `/alerts/active` on a short cadence; live
  capture matters — see notes)
- **connection:** none — keyless public US-government API
- **evidence:** official-docs — api.weather.gov, OpenAPI 3.0 spec published at
  api.weather.gov/openapi.json
- **effort / priority:** S / P1
- **needs:** none · time-sensitive (only 7 days of alert history is retrievable
  — capture matters) · `environment/` contract not yet ratified (Needs-David)

## What it is

The US National Weather Service's official public API. It serves active
watches/warnings/advisories (tornado, flood, severe thunderstorm, winter
storm, fire weather, 100+ event types) plus hourly gridpoint forecasts. It is
the authoritative US severe-weather alert source — keyless, stable, government-
run. Also backstops sources Trove deliberately skips (e.g. Blitzortung
lightning is unavailable; NWS severe-thunderstorm alerts cover that gap).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Active alerts | none (keyless) | event type, severity, urgency, headline, area, onset/expiry | official docs |
| Hourly forecast | none (keyless) | temp, wind, precip prob, short forecast per hour (7-day) | official docs |

US and territories only. Non-US users degrade to Open-Meteo weather codes
(already shipped as the `weather` def) — no NWS rows, no failure. All fields
optional in the contract.

## Access & auth

- `GET /alerts/active?point=LAT,LON` — active alerts for a point.
  `GET /points/LAT,LON` → gridpoint; `GET /gridpoints/{office}/{x},{y}/forecast/hourly`
  → 7-day hourly. All JSON/GeoJSON, no key. CAP XML also available.
- Rate limit floor ~30-minute polling for alerts; trivial load.
- US-only; outside US the point lookup returns no gridpoint — skip silently.
- No TCC, no local files, plain HTTPS. Standalone-clean.

## Vault mapping

- **Raw layer:** `environment/nws/raw/YYYY-MM.jsonl` — full alert/forecast API
  objects, full fidelity.
- **Contract layer:** `environment/nws/…` per the (pending) `environment/`
  contract — expected shape: one row per observation/alert event
  (`ts`, `source`, `guid`, reading/alert fields), overflow in `extra`.
  Same-shaped readings (AQI, forecast) merge with sibling environment feeds at
  read time. (Note: the research doc's suggested `environment/alerts/…` paths
  predate the taxonomy — the taxonomy table wins; this provider writes under
  `environment/nws/`.)
- **Dedupe:** alert id as `guid` (alerts re-appear across polls until expiry);
  cursor in `.trove/nws-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/nws.rs`: `DEF` (Periodic, ~30-min cadence for
   alerts), no `CONNECTION` (keyless). Uses the user's location.
2. Registration line in `INTEGRATIONS`.
3. Fixtures from documented `/alerts/active` and forecast example responses
   (alert-present and no-alert variants); parser + store + dedupe tests, unique
   temp dirs.
4. Non-US graceful path: no gridpoint → no rows, surface nothing as an error.
5. Vault writes via `store` helpers once the `environment/` contract is
   ratified; until then **parked behind Needs-David (contract)**.

## Build status — 🧪 2026-06-14

Shipped (`nws.rs`, INDEX #11 — also the **first-in-domain binding** of the
`environment` contract). `Behavior::Periodic` (~30 min), keyless (a `User-Agent`
header is mandatory; api.weather.gov 403s without it), no connection. **US-only**;
outside the US the `/points` lookup 404s → inert (no rows, no error). Inert until
a location exists.

Location: reuses weather's ladder — `corelocation::current_location` →
`vault.weather_location()` (the manual setting) → nws cursor last-used. No second
picker (the user sets location once, for weather). Resolution is behind an
injectable `pull_at(point)` seam so tests are deterministic (no CoreLocation).

- **Alerts → `EnvGeoEvent`** (`environment/nws/events/YYYY-MM.jsonl`):
  `/alerts/active?point=LAT,LON` → `ts` = onset‖effective‖sent (→ local),
  `guid` = the alert id, `event_type` = "alert", `severity`/`headline`/`place`
  (areaDesc)/`expires`, `url` = the feature's http link, `extra` = the full alert
  properties. Upsert by `guid` (alerts re-appear each poll until they expire).
- **Hourly forecast → `EnvReading`** (`environment/nws/YYYY-MM.jsonl`):
  `/points` → `forecastHourly` → per-hour readings for `temperature`,
  `precip_probability`, `wind_speed` (the `windSpeed` string/range parsed safely),
  `extra.forecast=true`; upsert by metric+hour (the forecast updates each poll).
- **Raw layer** `environment/nws/raw/YYYY-MM.jsonl`: full API objects, full
  fidelity (keeps the alert polygon + forecast detail the contract drops).
- Cursor `.trove/nws-sync.json` (non-secret): last-used location + last poll.

Contract binding (first collector in `environment/`): added `EnvReading` (guid
optional) + `EnvGeoEvent` (guid required) structs, one `environment` `DOMAINS`
entry (EventStream, month of `ts`; readings + `events/`), and promoted the
`reading` + `geo-event` fixtures from the draft test to the ratified triad (5/5).
`almanac` stays a draft (a later sun/moon provider binds it).

Adversarial-verify: 0 blocking + 2 minor — fixed: alert `url` is now the http
event link (was a bare URN); location made injectable so the tests are
deterministic + fast (40s → 0.01s) + fire no CoreLocation prompts.

Gate: trove-core 525/0 (+18 environment/nws tests), `cargo check` clean,
`schedule_doc` regenerated (nws Periodic), `bindings.ts` up to date.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Active alerts | 🧪 (time-sensitive) | with a US location set (shared with `weather`), Sync now during/after an active alert; confirm rows in `environment/nws/events/` + hub last-data. Only ~7 days of alert history exists — capture is live |
| Hourly forecast | 🧪 | Sync now; confirm a 7-day hourly block in `environment/nws/` (`extra.forecast=true`); compare a value against api.weather.gov directly |
| Non-US degrade | 🧪 | set a non-US location; confirm no rows and no error (Open-Meteo `weather` still runs) |

## Research notes

`integrations-research.md` → "Environment & Ambient Context" §NWS
(L2072–L2079). Feasibility 🟢 high, effort S. Keyless, stable, official. Only
7 days of alert history available, so live polling is what captures the record
— time-sensitive. CAP XML format also offered; OpenAPI spec published. No free
global equivalent for alerts — non-US is Open-Meteo weather codes only.
