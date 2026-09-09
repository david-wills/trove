# WeatherFlow Tempest

- **id:** `weatherflow-tempest`
- **domains:** `home/` (contract: **Phase 3 pending** — home readings shape;
  owned device, so `home/` not `environment/`; merges with `weather/` at
  read time)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (LAN UDP listen accumulates real-time readings;
  cloud REST backfills history)
- **connection:** `weatherflow-tempest` — TokenPaste (personal access token
  from tempestwx.com Settings → Data Authorizations → Create Token; cloud
  history only — the LAN UDP path needs no auth at all). Not shared.
- **evidence:** official-docs — published UDP broadcast spec (v171 current)
  + REST/WebSocket docs at weatherflow.github.io/Tempest/api
- **effort / priority:** S / P2
- **needs:** home contract not yet ratified (Needs-David) · real hardware
  for validation only (build proceeds from the published spec)

## What it is

Personal weather station (the Tempest unit + Wi-Fi hub). Owners get
hyper-local outdoor conditions — wind, rain, lightning, temperature,
humidity, UV, lux — far richer than any public feed for their exact
location. Niche audience (Tempest owners only) but a model dual-path
integration: zero-auth LAN capture plus cloud backfill.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Real-time readings (LAN) | none — same network, no account | wind speed/dir, rain, lightning, temp, humidity, UV, lux every 1–60s | published UDP spec v171 |
| Historical observations (cloud) | free w/ personal token | station observations by time range | official REST docs |
| Forecast (cloud) | free w/ token | forecast for station location | official REST docs |

All optional in the contract. A user who never pastes a token still gets
the live LAN stream; a user off-LAN still gets cloud history.

## Access & auth

- **LAN path:** UDP broadcast on port 50222 — hub broadcasts all sensor
  readings every few seconds, no auth, no TCC (plain socket listen). Hub
  stores ~1 week locally, so periodic capture tolerates gaps.
- **Cloud path:** REST at the documented base (weatherflow.github.io/
  Tempest/api); `GET /v1/observations/station/{station_id}`; personal
  access token (owner can query own stations only). Station ID comes from
  the account/device list.
- Standalone-clean both ways: local socket or plain outbound HTTPS.

## Vault mapping

- **Raw layer:** `home/weatherflow-tempest/YYYY-MM.jsonl` — observation
  rows (UDP packets normalized to the same reading shape as REST obs),
  full fidelity, partitioned by month.
- **Contract layer:** the pending Phase 3 home readings contract; expected
  one row per reading (`ts`, `source`, `guid`, device/station id, metric
  fields), overflow in `extra`. Read-time views merge these with the
  grandfathered `weather/` (Open-Meteo) stream — same-shaped readings,
  owned-device vs public-feed split per the taxonomy routing rule.
- **Dedupe:** `guid` = station id + observation epoch; cursor for the REST
  backfill in `.trove/`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/weatherflow_tempest.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste, with copy explaining the token is optional —
   LAN-only works without it), pull hook = REST backfill since cursor;
   UDP listener as the periodic LAN sample (bind, drain, store).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures: UDP packet samples + REST observation responses from the
   published spec; parser/store/dedupe tests, unique temp dirs.
4. UI: hub card notes the dual path; token field hinted, not required
   (disabled-affordance rule).
5. Contract rows wait on the Phase 3 home contract; raw layer can land
   first (per-source raw is always allowed).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| LAN UDP readings | — | on a network with a Tempest hub, enable; confirm rows accumulate in `home/weatherflow-tempest/` within a minute (requires real hardware — any Tempest-owning user's run validates) |
| Cloud history backfill | — | paste a personal token; Sync now; confirm historical rows + hub last-data |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §WeatherFlow
Tempest (L1776–L1782). Feasibility 🟢 high. Hub keeps ~1 week of local
buffer; the cloud token covers anything missed. Complements (never
replaces) the existing `weather` integration — taxonomy rule: owned
device → `home/`, public feed → `environment/`, merged at read time.
