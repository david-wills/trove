# Lutron Caséta

- **id:** `lutron-caseta`
- **domains:** `home/` (`home.event` unbound sibling draft — device state
  changes; no scalar HomeReading metrics from Caséta hardware)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (hourly LAN poll; diff consecutive snapshots to
  emit state-change events; bridge stores no history)
- **connection:** `lutron-caseta` (new TokenPaste ConnectionDef — pairing
  JSON from `pylutron-caseta` + bridge IP; mutual-TLS LEAP)
- **evidence:** `pylutron-caseta` test fixtures (`tests/responses/devices.json`,
  `occupancygroupsubscribe.json`) · medium confidence — community-documented
  protocol, no official Lutron schema
- **effort / priority:** M / P2
- **needs:** Needs-David (Smart Bridge **PRO** required — standard bridge
  has no local API) · contract_mode=deferred-sibling-draft (home.event)

## What it is

Lutron Caséta is a popular US smart-lighting / shade system. With the Smart
Bridge **PRO** model, the bridge exposes the local LEAP protocol over the
LAN — device list, current light/fan/shade levels, occupancy-sensor state,
and Pico remote button events. The standard (non-PRO) bridge is cloud-only
and has **no** local path. The bridge keeps no history, so Trove must poll
and accumulate the timeline itself.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Device list + current state | PRO bridge | device id, name, type, current level | community (pylutron-caseta) |
| Occupancy sensor state | PRO bridge | sensor id, occupied/unoccupied | community |
| Pico remote button events | PRO bridge — not polled | (requires LEAP event subscription, not a poll; momentary presses cannot be captured by hourly poller) | community — not built |

All optional in the contract (omit-if-empty). No history on the bridge —
every capability is a point-in-time read that Trove timestamps and appends.

## Access & auth

- LAN: LEAP protocol over TCP to the Smart Bridge PRO (port 8081/8083);
  `pylutron-caseta` (PyPI) is the reference client. Older systems also
  expose telnet on port 23 (Lutron Integration Protocol / LIP).
- Pairing: a one-time local pairing handshake with the bridge (button-press
  on the bridge); no cloud account, no OAuth. Standalone-clean — purely
  local-network, but **requires the PRO bridge**; surface that gate clearly
  in the connect card (per the disabled-controls affordance rule).
- Standard (non-PRO) bridge: cloud-only via Lutron's app; **out of scope** —
  no local path, and Trove never depends on an external service.

## Vault mapping

- **Raw layer:** `home/lutron-caseta/raw/YYYY-MM.jsonl` — each LEAP poll's
  device/sensor state snapshots, full fidelity, timestamped at read.
- **Contract layer:** `home/lutron-caseta/…` per the (pending) `home`
  contract — expected shape: one row per state-change event (`ts` = poll
  time, `source`, `guid` = device-id + timestamp, device name/type, level
  or occupancy state). Device subtype in `extra.device_type`; fan speed
  in `detail`. Overflow in `extra`. (`home/` = owned device, vs
  `environment/` for public feeds.) Pico button events require a LEAP
  subscription and are not collected by the hourly poll.
- **Dedupe:** synthesize `guid` from device id + change timestamp; since the
  bridge has no history, dedupe is against Trove's own accumulated output —
  cursor in `.trove/lutron-caseta-sync.json`, rebuildable by scanning files.

## Build plan (completed 2026-06-17)

1. Module `crates/trove-core/src/lutron_caseta.rs`: `DEF` (Periodic hourly)
   + `CONNECTION` (TokenPaste — bridge IP + pylutron-caseta pairing JSON).
2. `&crate::lutron_caseta::CONNECTION` added to `CONNECTIONS` in integrations.rs.
3. `rustls-pemfile = "2.2.0"` added to trove-core Cargo.toml.
4. LEAP poll-and-accumulate: mutual TLS (client cert from pairing), line-
   delimited JSON, diff consecutive snapshots to emit state-change events.
5. Raw unconditional layer at `home/lutron-caseta/raw/YYYY-MM.jsonl`.
6. Events layer at `home/lutron-caseta/events/YYYY-MM.jsonl` per the
   `home.event` draft schema (as `serde_json::Value` — type not yet bound).
7. `contract_mode = deferred-sibling-draft` (`home.event`) — Caséta data
   is pure device events; no scalar sensor readings → `HomeReading` does
   not apply.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Device state poll (lights/shades) | 🧪 built | pair with a real PRO bridge; Sync now; confirm zone-level events in `home/lutron-caseta/events/` + raw snapshots in `raw/` |
| Occupancy sensor events | 🧪 built | trigger occupancy sensor; confirm `"event":"motion"` row on next poll |
| Fan speed events | 🧪 built | change fan speed; confirm `"event":"on"/"off"` + detail=fanSpeed in events; confirm Level=-1 does not emit spurious level event |
| Silent baseline (first sync) | ✅ tested | first poll captures baseline; no events emitted until second poll shows change |
| Pico remote button events | ⛔ not built | requires LEAP subscription; momentary presses cannot be captured by hourly poll |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Lutron Caséta
(L1952–L1958). Feasibility 🟡 medium — gated on the PRO bridge ($70 more
than standard). `pylutron-caseta` is actively maintained; LEAP gives
device/sensor/Pico data but **no history** (poll + accumulate). Frame the
PRO-bridge requirement in the UI. Non-PRO owners have only the cloud path
(undocumented, would require an intermediary like Home Assistant) — out of
scope per the standalone rule. `home/` (owned device) vs `environment/`
(public feed): same-shaped readings merge at read time.
