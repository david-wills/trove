# Honeywell Home (Resideo)

- **id:** `honeywell-resideo`
- **domains:** `home/` (contract: **Phase 3 pending** — home readings shape)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll + persist — the API has **no history
  endpoint**, same pattern as Nest)
- **connection:** `honeywell-resideo` — OAuth (BYO app: user registers a
  free Consumer Key + Secret at developer.honeywellhome.com; registration
  open as of 2026). Not shared with other defs.
- **evidence:** official-docs — developer.honeywellhome.com portal live,
  registration open, API functional as of 2026
- **effort / priority:** M / P2
- **needs:** home contract not yet ratified (Needs-David) · real
  thermostat for validation only (no simulator exists — build proceeds
  from documented shapes)

## What it is

Resideo's Honeywell Home cloud API for the large Honeywell Wi-Fi
thermostat installed base — T-Series (T9/T10), Lyric Round, and other
Honeywell Home models. Yields current temperature, setpoint, mode,
humidity, and fan status; with smart room sensors attached, per-room
readings too. Like Nest, current-state only — Trove's polling builds the
longitudinal comfort/HVAC log the vendor never exposes.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Thermostat state | free developer account | indoor temp, setpoint, mode, humidity, fan status | official portal docs |
| Device list | free | thermostats per location | `GET /v2/devices/thermostats` |
| Room sensor readings | free, if sensors installed | per-room temp/occupancy from smart room sensors | official portal docs |

All optional in the contract; sensor-less homes simply carry thermostat
rows only. No tier code paths.

## Access & auth

- REST at developer.honeywellhome.com: `GET /v2/devices/thermostats`,
  `GET /v2/devices/thermostats/{deviceId}`. OAuth 2.0 with Consumer Key +
  Secret from a free developer registration — BYO-app model (setup copy
  walks the user through registering once; low ongoing cost).
- No history endpoint — poll on a steady cadence and persist.
- No simulator; a physical device is required to exercise the API.
- No TCC. Standalone-clean (plain outbound HTTPS).

## Vault mapping

- **Raw layer:** `home/honeywell-resideo/YYYY-MM.jsonl` — polled device
  snapshots, full fidelity, monthly partitions.
- **Contract layer:** pending Phase 3 home contract — one row per snapshot
  (`ts`, `source`, `guid`, device id, temp/setpoint/mode/humidity),
  overflow in `extra`. Same internal shape as `google-nest` rows.
- **Dedupe:** `guid` = device id + poll timestamp.

## Build plan

1. Module `crates/trove-core/src/honeywell_resideo.rs`: `DEF` (Periodic),
   `CONNECTION` (OAuth, BYO credentials — connect card explains the
   developer-portal registration per the disabled-affordance rule), pull
   hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Share the poll-and-persist snapshot shape with `google-nest` (build
   whichever lands second against the first's internal record).
4. Fixtures from the portal's documented v2 responses (thermostat with
   and without room sensors); parser/store tests, unique temp dirs.
5. Contract rows wait on the home contract; raw layer can land first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| OAuth + device list | — | register a developer app, paste key/secret, complete consent with a real thermostat on the account; devices appear; hub last-data set (no simulator — only a Honeywell-owning user's run validates) |
| Self-built history | — | leave enabled ~1 hr; confirm snapshot rows accumulate in `home/honeywell-resideo/` |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices"
§Honeywell/Resideo Thermostat (L1800–L1806). Feasibility 🟡 medium. This
covers the main Honeywell/Resideo market; notable because Ecobee (the
other big thermostat) is 🚫 unavailable — developer registrations closed —
so Nest + Honeywell are the two buildable thermostat paths. Third-party
wrappers exist (Go `gohoneywellapi`) as shape references; no fresh
research needed.
