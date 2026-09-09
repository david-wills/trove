# Tesla Powerwall + Solar

- **id:** `tesla-energy`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings shape;
  the energy substream {ts, source, direction, watts_or_kwh, interval_minutes}
  is drafted from Enphase + Tesla + Green Button + Sense/Emporia together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll Fleet API for energy history; watermark cursor)
- **connection:** `tesla` — OAuth (Fleet API, developer.tesla.com app).
  **Shared** with a future Tesla vehicle provider — one Tesla login, two defs.
  A secondary local-gateway path (Tesla email + last 5 of gateway serial)
  is token-paste-shaped but firmware-fragile; see Research notes.
- **evidence:** official-docs — Fleet API at developer.tesla.com
  (`/api/1/energy_sites/{id}/calendar_history`, `/telemetry_history`);
  community — pypowerwall + vloschiavo/powerwall2 cover the local gateway path
- **effort / priority:** L / P2
- **needs:** Needs-login (Tesla account with Powerwall/solar + developer app with energy_device_data scope) ·
  home.energy contract not yet bound (deferred-sibling-draft: rows shaped per draft, raw-only for now)

## What it is

Solar production and Powerwall battery storage telemetry for Tesla Energy
owners: generation, home consumption, grid import/export, battery
charge/discharge, plus Wall Connector EV-charging history. Niche audience
(Powerwall owners) but the data is high-value and unavailable anywhere else
— the household's complete energy picture.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Energy history | Fleet API (tiered pricing; low-volume personal use may be free tier) | daily/weekly solar generation, battery charge/discharge, grid import/export aggregates via `calendar_history` | official docs |
| Wall Connector charging | Fleet API | charging session history via `/telemetry_history` | official docs |
| Real-time power flow | local gateway only | instantaneous watts per source (solar/battery/grid/home) | community (pypowerwall) |

All optional in the contract; a solar-only site simply carries no battery
fields. On a local-gateway failure, the Fleet API rows still flow — degrade
with a UI hint, never fail silently.

## Access & auth

- Cloud (primary): Tesla Fleet API, OAuth via developer.tesla.com app
  registration. Pricing is tiered by request volume (Jan 2025) — a periodic
  personal pull is low-volume.
- Local (fallback): HTTPS to the Powerwall gateway at its LAN IP or
  192.168.91.1 (Wi-Fi AP); auth = Tesla account email + last 5 digits of the
  gateway serial. FW 25.10.0 removed TEDAPI LAN routing — connecting to the
  Powerwall's own Wi-Fi AP is required on current firmware, which is hostile
  UX; treat local as expert-mode.
- No TCC, no local files. Standalone-clean (plain HTTPS both paths).

## Vault mapping

- **Raw layer:** `home/tesla-energy/raw/YYYY-MM.jsonl` — API responses
  (calendar_history periods, telemetry_history sessions), full fidelity.
- **Contract layer:** `home/tesla-energy/YYYY-MM.jsonl` per the (pending)
  home contract's energy shape — one row per interval per direction
  (generation / consumption / import / export / battery), overflow in `extra`.
- **Dedupe:** `guid` = site id + interval start + direction; cursor in
  `.trove/tesla-energy-sync.json`, rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/tesla_energy.rs`: `DEF` (Periodic; energy
   aggregates are daily — a few pulls/day suffices), `pull` hook for Sync-now.
2. `CONNECTION` `tesla` (OAuth) declared here for now; the vehicle provider
   later sets `connection: Some("tesla")` and shares it — design the scopes
   for both up front (Google model).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from documented `calendar_history` / `telemetry_history`
   response shapes; parser + store + cursor tests, unique temp dirs.
5. Local-gateway path is a later opt-in slice (pypowerwall as the shape
   reference), clearly labeled firmware-fragile; do not block v1 on it.
6. Vault writes via `store` helpers once the home contract is ratified;
   until then **parked behind the Phase 3 home contract**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Energy history (Fleet) | ✅ built — Needs-login | OAuth with a real Tesla Energy account (energy_device_data scope); Sync now; confirm rows in `home/tesla-energy/energy/` + `home/tesla-energy/raw/` + hub last-data. Needs a Powerwall/solar owner — David doesn't have one; any real user's run can validate. |
| Site discovery | ✅ built | `GET /api/1/products` filters energy_site_id entries; vehicles are skipped |
| Wall Connector sessions | ❌ not wired | calendar_history kind=energy does not include charging sessions; a separate endpoint would be needed |
| Local gateway | ❌ not wired — Needs-sample | LAN gateway path deferred (firmware-fragile); pypowerwall shape as reference when ready |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Tesla Powerwall +
Solar (L1856–L1862). Feasibility 🟡 medium. Fleet API is the stable primary;
the local API is unofficial and firmware updates break it (PW3 worse than
PW1/2/+). Cross-cutting note 4: the energy schema is shared with Enphase,
Green Button, Sense, Emporia — sequence one of those near this to exercise
the shape with a second source. Taxonomy: owned-device data routes `home/`
(the research doc's `energy/` suggestion predates the taxonomy).
