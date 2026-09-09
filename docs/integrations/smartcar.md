# Smartcar

- **id:** `smartcar`
- **domains:** `location/` (contract: **Phase 3 pending** — `location` trails
  shape now; Smartcar yields point snapshots, not trails)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll latest location + odometer; append a time series)
- **connection:** `smartcar` — OAuth (new connection; self-serve developer
  registration at smartcar.com; user connects the vehicle during onboarding).
  Not shared with other defs (Tesla has its own `tesla` connection).
- **evidence:** official-docs — smartcar.com `/v2.0` location + odometer
  endpoints, 40+ OEMs documented
- **effort / priority:** M / P2
- **needs:** privacy (location trail — opt-in with explicit acknowledgement)

## What it is

Connected-car API broker (successor to Automatic) that normalizes 40+ OEMs —
Tesla, GM/Chevy/GMC, Ford, BMW, Hyundai/Kia, Toyota, VW, Mercedes-Benz,
Stellantis (Jeep/Ram/Dodge), DS — behind one OAuth + REST surface. For Trove
it is a polled mileage/location time series, not a GPS-trail source: it returns
only the latest known location and odometer, never trip history or a trace
replay.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Latest location | per-OEM scope grant | lat, lon, timestamp | official docs |
| Odometer | per-OEM scope grant | mileage, timestamp | official docs |

All optional in the contract (omit-if-empty); a vehicle that only grants
odometer simply carries no coordinate. No trip-history capability exists —
trips can only be *reconstructed* from the polled point series at read time.

## Access & auth

- OAuth 2.0 at smartcar.com; `GET /v2.0/vehicles/{id}/location`,
  `GET /v2.0/vehicles/{id}/odometer`. Webhook streaming also exists but needs a
  public receiver (not local-first) — skip; poll instead.
- Self-serve developer registration → baked client creds (BYO per ConnectSpec
  doctrine). Compiled-in audience only; no broker.
- No TCC, no local files — plain HTTPS, standalone-clean.

## Vault mapping

- **Raw layer:** `location/smartcar/raw/YYYY-MM.jsonl` — the API location +
  odometer response objects, full fidelity, one row per poll.
- **Contract layer:** `location/` is trails-shaped (Phase 3 pending) and
  Smartcar yields *points*, not trails. Until a visits/points contract lands,
  write the normalized point series under `location/smartcar/` raw-only (one
  row per poll: `ts`, `source`, `lat`, `lon`, `odometer`, `guid`). The trails
  contract designer decides at Phase 3 whether polled points fold in or stay a
  sibling stream.
- **Dedupe:** `guid` = vehicle id + poll timestamp; cursor in
  `.trove/smartcar-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/smartcar.rs`: `DEF` (Periodic; poll cadence
   user-tunable — hourly default for a mileage log), `CONNECTION` (OAuth:
   label/help/scopes per the ConnectSpec affordance rule), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from smartcar.com example location + odometer responses; parser +
   store + cursor tests, unique temp dirs.
4. Privacy gate: ships opt-in (vehicle location is a location trail) — explicit
   acknowledgement on enable, default-off.
5. Vault writes via `store` helpers into `location/smartcar/` raw-only until the
   location contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Location snapshot | ✅ built | connect a real vehicle via OAuth; Sync now; confirm a row in `location/smartcar/raw/` + hub last-data (Needs-login, real-vehicle) |
| Odometer | ✅ built | same run; confirm `odometer_km` populated; poll twice and confirm two rows |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Smartcar (L2348–L2354).
Feasibility 🟡 medium. The hard limitation is no trip history — latest values
only; trip reconstruction requires Trove's own periodic polling stored as a
time series. Better as a daily-mileage log than a GPS-trail source; low
priority versus true trail sources (Overland, OwnTracks, Timeline). Shares the
connected-car space with Tesla but uses a distinct connection.
