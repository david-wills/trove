# Ecobee

- **id:** `ecobee`
- **domains:** `home/` (contract: **Phase 3 pending** — home telemetry shape)
- **status:** 🚫 unavailable
- **unavailable_reason:** Ecobee has paused new developer registrations (since
  2024, no reopening timeline) — no API keys can be issued, so new users can't
  connect. HomeKit-capable Ecobees show config via the HomeKit integration; no
  telemetry. Revisit if reopened.
- **behavior:** Unavailable
- **connection:** none — no key can be issued, so no connection exists.
- **evidence:** official — the Ecobee developer API exists and is documented,
  but new registrations have been closed since April 2024 with none issued as
  of October 2024
- **effort / priority:** L / P2
- **needs:** none (blocked upstream; nothing for us to flag)

## What it is

Ecobee makes smart thermostats with rich runtime, setpoint, and occupancy-
sensor history — premium features (energy reports, runtime history) are
cloud-only and exposed through the developer API. For users who can already
connect, this is a high-value home-telemetry source. Trove cannot onboard new
users today: the registration door is closed (see unavailable_reason).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Runtime history | — (blocked) | heat/cool runtime, setpoints | official API (registrations closed) |
| Occupancy | — (blocked) | sensor presence history | official API (registrations closed) |
| Device config (HomeKit) | HomeKit-capable models | current config only, no history | HomeKit `homed` DB |

No data is collectable while registrations are closed. The HomeKit path yields
config presence only — no telemetry — and is covered by the `apple-homekit`
integration, not this one.

## Access & auth

- Developer API at ecobee.com/en-us/developers — OAuth 2.0. **Blocked:** Ecobee
  stopped accepting new developer registrations in April 2024; no new keys as
  of October 2024. Existing keys still work, but Trove can't mint new ones for a
  fresh user.
- HomeKit workaround: HomeKit-capable Ecobees surface device config (not
  telemetry) via the local `homed` DB — that is the `apple-homekit` def's
  territory, kept separate.

## Vault mapping

- **Raw layer:** `home/ecobee/` reserved for the runtime/occupancy telemetry
  shape if registrations reopen. Nothing written while unavailable.
- **Contract layer:** would follow the (pending) Phase-3 home telemetry shape.

## Build plan

Parked. If Ecobee reopens developer registration, this becomes a medium-effort
OAuth pull (`ecobee` connection, OAuth 2.0) with rich runtime/occupancy
history; flip status to 📋 queued and write the capability rows then. Until
then it renders as a greyed unavailable card with the reason above.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| (all) | 🚫 unavailable | n/a — no key can be issued until Ecobee reopens registrations |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Ecobee Smart
Thermostat (L1936–L1942). Feasibility 🟠 low — new developer accounts not
accepted as of June 2026, no stated reopening timeline. Existing integrations
keep working; the HomeKit fallback gives config only, not telemetry. If
reopened, the upside is rich historical heat/cool runtime, setpoints, and
occupancy-sensor data.
