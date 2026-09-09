# Google Nest

- **id:** `google-nest`
- **domains:** `home/` (contract: **Phase 3 pending** — home readings shape)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (frequent poll, ~5 min — the API has **no history
  endpoint**, so Trove self-builds history from repeated state snapshots)
- **connection:** `google-nest` — OAuth via Google **Device Access** (SDM).
  A separate consent/registration from the existing `google` connection
  (different program: console.nest.google.com, one-time $5 developer fee,
  PCM consent flow) — recorded as related but **not shared** with the six
  google defs.
- **evidence:** official-docs — Smart Device Management API at
  smartdevicemanagement.googleapis.com, live and documented as of June 2026
- **effort / priority:** M / P2
- **needs:** Needs-login (Device Access registration + a real Nest device)
  · home contract not yet ratified (Needs-David)

## What it is

Nest thermostats, cameras, displays, and doorbells via Google's Smart
Device Management API. For Nest-home users this is the only programmatic
window into temperature, humidity, HVAC state, and device events. The
catch: the API is current-state only — the 10-day history shown in the
Nest app is not queryable — so the integration's value is the longitudinal
log Trove builds that Google itself never exposes.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Thermostat traits | all (Device Access) | ambient temp, humidity, setpoint, mode, HVAC status | official SDM docs |
| Device list/topology | all | devices, rooms, structures | official SDM docs |
| Live events (Pub/Sub) | all | connectivity, mode change, HVAC status events | official SDM docs |

All optional in the contract; camera/doorbell trait coverage varies by
device — store whatever traits the device reports, omit-if-empty.

## Access & auth

- REST: `https://smartdevicemanagement.googleapis.com/v1`; OAuth 2.0 with
  Google's Partner Connections Manager consent. One-time $5 registration
  per developer account at console.nest.google.com — Trove can ship baked
  credentials (we pay the $5 once) or BYOC per the ConnectSpec model.
- **No history endpoint** — poll ~5 min and persist.
- **Refresh token expires after 6 months idle** — the periodic pull
  itself keeps it alive; surface a reconnect prompt if it lapses.
- Pub/Sub event delivery exists but implies cloud subscription plumbing —
  polling first; events are a later upgrade.
- No TCC. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `home/google-nest/YYYY-MM.jsonl` — polled trait
  snapshots, full fidelity, monthly partitions.
- **Contract layer:** pending Phase 3 home contract — one row per snapshot
  (`ts`, `source`, `guid`, device id, temp/humidity/mode fields),
  overflow in `extra`.
- **Dedupe:** `guid` = device id + poll timestamp; store-on-change is a
  possible compaction later, but raw keeps every poll first.

## Build plan (completed 2026-06-17)

1. Module `crates/trove-core/src/google_nest.rs`: `DEF` (Periodic, 5-min
   cadence), `CONNECTION` (OAuth, Device Access; new `GOOGLE_NEST_PROVIDER`,
   port 38799), pull hook for Sync-now. ✅
2. `&crate::google_nest::CONNECTION` added to CONNECTIONS in integrations.rs. ✅
3. Reconnect UX: `needs_reconnect` when access token expired and no refresh
   token (surfaces in the hub card per the disabled-affordance rule). ✅
4. Fixtures from SDM documented trait responses (thermostat + sparse camera);
   11 unit tests, unique temp dirs. ✅
5. Contract layer writes `home.HomeReading` rows (temperature/humidity/
   setpoint_heat/setpoint_cool in °C or %). Non-numeric traits (mode, HVAC
   status, connectivity) ride in `extra`. ✅
6. Raw layer unconditional — every device object verbatim with `_poll_ts` tag. ✅

**Enterprise ID**: must be seeded into the cursor once (on first connect /
first poll). The `pull` function bails with a clear reconnect message when
the cursor has no `enterprise_id`. A real connect flow would need to discover
the ID from the first device list response and seed the cursor — flagged
as a Needs-login/Needs-David step since it requires a real Nest device. The
`enterprise_id_from_device` helper is in place for that bootstrap.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| OAuth + device list | — | complete Device Access consent with a Nest-owning account; devices appear; hub last-data set (Needs-login + real device — any Nest owner's run validates) |
| Self-built history | — | leave enabled ~1 hr; confirm multiple snapshot rows in `home/google-nest/` at the poll cadence |
| Token keep-alive | — | long-horizon: confirm pulls continue past idle periods without re-consent |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Google Nest
(L1792–L1798). Feasibility 🟡 medium — friction is registration ($5,
Gmail-only), not the API. Same poll-and-persist pattern as
Honeywell/Resideo — build the two against the same internal shape.
Connection-sharing note: this does **not** ride the existing `google`
connection (Device Access is a separate program with its own consent),
matching the rule that sharing is recorded but services stay distinct.
