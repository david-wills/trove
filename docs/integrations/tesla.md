# Tesla (vehicle)

- **id:** `tesla`
- **domains:** `location/` (contract: **Phase 3 pending** — location;
  trip/trail shape)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `vehicle_data`; or read a local TeslaMate DB —
  see build plan)
- **connection:** `tesla` — OAuth (developer.tesla.com app; `vehicle_location`
  scope mandatory since Jan 2025). New connection, **shared with
  `tesla-energy`** (one Tesla login, multiple defs).
- **evidence:** official-docs — developer.tesla.com (`vehicle_data`);
  Fleet Telemetry on github.com/teslamotors/fleet-telemetry. High confidence
  on the API; the local-first path needs a spike.
- **effort / priority:** L / P2
- **needs:** privacy (vehicle location trail — opt-in with explicit
  acknowledgement) · Needs-login (validation; build proceeds from documented
  shapes) · spike (architecture choice — see below)

## What it is

Tesla's Fleet API exposes a connected vehicle's state, including location.
For a Trove user it is a "where my car has been" source — but the data shape
Tesla makes available locally is awkward: the cloud endpoint returns *current
state only*, and the historical trip stream requires an always-on public
server, which conflicts with local-first. This brief records the spike that
resolves the path before any build.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Current location/state (poll) | free API (app reg required) | lat/lon, odometer, charge, point-in-time | official docs |
| Live GPS stream (Fleet Telemetry) | free, but needs a public TLS server | Location @ ~10s intervals | official docs |
| Full trip history (via TeslaMate) | self-hosted | trips, GPS traces in local Postgres | community |

All optional in the contract (omit-if-empty).

## Access & auth

- **Fleet API:** OAuth 2.0 at developer.tesla.com (app registration
  required); `vehicle_location` scope mandatory since Jan 2025.
  `GET /api/1/vehicles/{id}/vehicle_data` returns **current** state only —
  Tesla does not store user-accessible trip history.
- **Fleet Telemetry:** vehicles stream to a TLS server endpoint *you* run
  (configurable fields incl. Location @ ~10s). Incompatible with purely
  local operation — out of scope for the compiled-in collector.
- **TeslaMate:** open-source self-hosted Docker that already absorbs
  telemetry into a local PostgreSQL DB with full trip/GPS history — the
  pragmatic local-first path for users who run it (read its DB, standalone).
- **Dead/alternatives:** Automatic (OBD) shut down May 2020. Smartcar
  (multi-brand OAuth) gives current location + odometer but no trip history.

## Vault mapping

- **Raw layer:** `location/tesla/raw/` — polled `vehicle_data` snapshots, or
  TeslaMate trip/position rows, full fidelity.
- **Contract layer:** `location/tesla/` per the (pending) **location**
  contract. Records route whole: a TeslaMate trip is a trail (`ts`, `source`,
  `guid` = trip id, ordered trackpoints), polled snapshots are point-in-time
  positions. Tesla-specific fields (odometer, charge state) in `extra`.
  Parked behind Needs-David until the location trail/visits shape ratifies.
- **Dedupe:** trip id (TeslaMate) or `(vehicle_id, timestamp)` (poll) as
  `guid`; cursor in `.trove/tesla-sync.json`.

## Build plan

1. **Spike first** (the L effort lives here): confirm the chosen path is the
   TeslaMate local DB read for trip history, with the Fleet API poll as a
   current-position-only secondary. Fleet Telemetry's public-server
   requirement rules it out for the standalone binary — document and drop.
2. Module `crates/trove-core/src/tesla.rs`: `DEF` (Periodic), `CONNECTION`
   (`tesla` OAuth, shared with `tesla-energy`), `pull` hook. TeslaMate path
   is a local-file read (no connection); Fleet poll uses the connection.
3. Register one line each in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures: documented `vehicle_data` JSON; a TeslaMate `positions`/`trips`
   row sample (**Needs-sample** — flag if no real TeslaMate DB on hand);
   parser + store + cursor tests, unique temp dirs.
5. Privacy gate: ships opt-in (vehicle location trail) — explicit
   acknowledgement on enable.
6. Vault writes via `store` once the location contract ratifies; until then
   **parked behind Needs-David (contract)**.

## Build notes (2026-06-16)

- **Architecture decision**: Fleet API poll only (current-state snapshots). Fleet Telemetry (public-server requirement) is out of scope for a standalone binary. TeslaMate (local PostgreSQL) omitted — requires a separate self-hosted service dependency, violating the standalone rule; noted as a future option for users who run it.
- **Contract**: location domain IS bound (google-timeline pioneered it); Tesla writes `Fix` rows directly. One Fix per vehicle per poll when `drive_state.latitude`/`.longitude` are present; sparse vehicle pings with `odometer_mi`, `battery_level_pct`, `battery_range_mi`, `charging_state`, `vin` in `extra`.
- **Raw layer**: full `vehicle_data` JSON in `location/tesla/raw/YYYY-MM.jsonl` (one row per vehicle per poll).
- **Connection**: NEW `CONNECTION` declared in this module (`id = "tesla"`, port 38694). Shared with `tesla-energy` (which will reference `connection: Some("tesla")` when built). Integrator must add `&crate::tesla::CONNECTION,` to CONNECTIONS.
- **Field provenance**: `drive_state.latitude/longitude` (f64 WGS84), `drive_state.heading` (int degrees), `drive_state.gps_as_of` (epoch seconds, used as Fix ts), `vehicle_state.odometer` (decimal miles), `charge_state.battery_level` (int %), `charge_state.battery_range` (miles), `charge_state.charging_state` (string) — all confirmed from tesla-api.timdorr.com community docs.
- **Asleep vehicles**: 408/503 → skip silently (normal for parked Teslas).
- **19 tests, all green.**

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Fleet API poll | built | complete OAuth (dev app + `vehicle_location` scope); Sync now; confirm Fix row in `location/tesla/YYYY-MM-DD.jsonl` + raw in `location/tesla/raw/YYYY-MM.jsonl` |
| TeslaMate trips | out of scope | requires self-hosted Docker + PostgreSQL — violates standalone rule; future enhancement for users who run TeslaMate |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Tesla Fleet API
(L2340–L2346). Feasibility 🟡 medium. Core gotcha: the cloud endpoint is
current-state-only and the streaming path needs a public server —
incompatible with local-first; TeslaMate's local DB is the recommended
pragmatic route. Smartcar noted as a multi-brand alternative but also lacks
trip history. Spike before committing the L build.
