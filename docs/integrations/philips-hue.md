# Philips Hue

- **id:** `philips-hue`
- **domains:** `home/` (contract: **Phase 3 pending** — `home/` shapes drafted
  from Hue + HomeKit + Tempest + Enphase + Green Button together)
- **status:** 🧪 built (fixture-tested, not validated — Needs-login)
- **unavailable_reason:** none
- **behavior:** Periodic (poll the bridge on a schedule; accumulate state —
  the bridge keeps no history)
- **connection:** `philips-hue` — TokenPaste of `ip|username` for the LAN
  bridge (no cloud). Auth is a one-time physical link-button press that mints
  the username token; the bridge IP + username are pasted as `ip|username` and
  stored locally (0600). Registered in `CONNECTIONS` (integrator added the
  line; the build's own worktree did not include it).
- **evidence:** official-docs — developers.meethue.com (v1 + CLIP v2 local
  API, full schemas, no rate limits for personal LAN use)
- **effort / priority:** S / P2
- **needs:** Needs-login (real-data validation needs a paired Hue bridge on
  the LAN)

## What it is

Philips Hue is the dominant consumer smart-lighting system: a LAN bridge that
controls lights, plus Hue motion/temperature/daylight sensors, rooms, zones,
and scenes. The data is a lightweight presence-and-environment signal — when
lights and rooms are active, motion events, and the temperature the Hue Motion
sensor reports — all captured locally with zero cloud dependency.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Light/room/zone state | all | on/off, brightness, color, reachable, grouping | official docs |
| Motion sensors | sensor-dependent | presence events, last-triggered | official docs |
| Temperature sensors | Hue Motion includes one | °C readings | official docs |
| Scenes | all | scene names, active scene | official docs |

All optional in the contract (omit-if-empty); a user with no Hue sensors just
gets light/room state.

## Access & auth

- HTTPS to `https://<bridge-ip>/api/` (v1) or `/clip/v2/` (v2). Bridge
  discovered via mDNS or `discovery.meethue.com`.
- Auth: press the bridge's physical link button once, POST to `/api` to
  receive a username token; store it locally and reuse.
- No cloud, no macOS TCC. Standalone-clean.
- Bridge ships a **self-signed TLS cert** — reqwest needs
  `accept_invalid_certs(true)` (or pin the bridge cert). Note this in the
  module.
- No history endpoint: the bridge exposes current state only. Trove builds the
  history by polling. CLIP v2 offers SSE push for state changes — a later
  upgrade over polling for live capture.

## Vault mapping

- **Raw layer:** `home/philips-hue/raw/YYYY-MM.jsonl` — full bridge state
  snapshots (lights, sensors, rooms, scenes) at each poll, full fidelity.
- **Contract layer:** `home/philips-hue/YYYY-MM.jsonl` per the pending
  Phase-3 `home/` contract — expected one row per reading/state-change
  (`ts`, `source`, `device`, `metric`, `value`), owned-device readings that
  merge with `environment/` weather at read time (temperature). Parked behind
  the contract until it ratifies.
- **Dedupe:** `guid` = bridge resource id + timestamp; cursor in
  `.trove/philips-hue-sync.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/philips-hue.rs`: `DEF` (Periodic), bridge
   discovery + link-button pairing, polling pull hook.
2. One registration line in `INTEGRATIONS`. No `CONNECTION` def — the
   link-button token is bridge-local, not a registry connection; store it in
   the def's own state.
3. `accept_invalid_certs(true)` on the reqwest client for the self-signed
   bridge cert.
4. Fixtures from developers.meethue.com example responses (v2 light + motion
   sensor); parser + store + cursor tests, unique temp dirs.
5. Vault writes via `store` helpers once the `home/` contract is ratified.

## Build notes (INDEX #166)

- **Behavior:** Periodic (hourly); `IntegrationKind::CloudSync` (required by registry since it has a `ConnectionDef`).
- **Connection:** new `philips-hue` `ConnectionDef` (TokenPaste, `ip|username`). The integrator must add `&crate::philips_hue::CONNECTION,` to `CONNECTIONS` in `integrations.rs`.
- **TLS:** Custom `rustls::ClientConfig` with `AcceptAnyCert` verifier; no new Cargo deps needed (rustls already a workspace dep).
- **Contract:** Temperature sensors → `HomeReading` (metric=`temperature`, unit=`C`) at `home/philips-hue/YYYY-MM.jsonl`. Motion events (presence=true) → plain `Value` at `home/philips-hue/events/YYYY-MM.jsonl`. Raw layer unconditional at `home/philips-hue/raw/YYYY-MM.jsonl`.
- **Dedupe:** guid = `philips-hue:{type}:{sensor-id}:{minute-key}` (minute-resolution; back-to-back polls dedupe cleanly).
- **Evidence:** Field names confirmed against openhab `Resource.java` bindings (CLIP v2 schema): `temperature.temperature` (°C), `motion.motion` (bool), `metadata.name`, `on.on`, `dimming.brightness` (0–100).
- **No OAuth port** (TokenPaste, no redirect).

## Validation matrix

_Status is 🧪 (fixture-tested, not validated) until David confirms against a real bridge — only David promotes to ✅._

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Light/room state | 🧪 raw | discover bridge IP; press the link button; POST to `https://<ip>/api` with `{"devicetype":"trove#trove"}` to mint a username; paste `ip\|username` in the Philips Hue connect card; Sync now; confirm rows in `home/philips-hue/raw/` + hub last-data |
| Temperature sensor | 🧪 contract | requires a Hue Motion sensor (has a temperature sub-sensor); confirm `home/philips-hue/YYYY-MM.jsonl` rows with metric=temperature |
| Motion events | 🧪 contract | walk past the sensor (motion=true); confirm `home/philips-hue/events/YYYY-MM.jsonl` rows with event=motion |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Philips Hue
(L1768–L1774). Feasibility 🟢 high. Purely local, zero-friction auth, clean
JSON — a model LAN integration. The only friction is the self-signed cert.
SSE push (CLIP v2) is the eventual live-capture path; ship polling first.
Owned device, so it routes `home/` (not `environment/`) and merges with the
weather stream at read time.
