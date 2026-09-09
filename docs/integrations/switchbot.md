# SwitchBot

- **id:** `switchbot`
- **domains:** `home/` (contract: **Phase 3 pending** — home readings
  contract; temp/humidity/motion/contact readings fit it directly)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll device states; accumulate — cloud holds
  state, not queryable history)
- **connection:** `switchbot` — TokenPaste (token + secret from the
  SwitchBot app → Profile → Developer Options; requests signed
  HMAC-SHA256). Not shared with other defs.
- **evidence:** official-docs — open API v1.1,
  github.com/OpenWonderLabs/SwitchBotAPI; documented rate limit
  10,000 req/day
- **effort / priority:** S / P2
- **needs:** none — home contract (HomeReading) is bound; reuses ambient_weather pattern

## What it is

SwitchBot is the popular affordable sensor ecosystem: Hub 2 (with built-in
temperature/humidity), Meter Plus, outdoor meters, motion sensors, contact
sensors, curtains, plugs. An official, well-documented API makes it one of
the easiest indoor-environment sources to support — Hub 2 alone makes it a
common indoor air-comfort sensor.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Temp/humidity | Hub 2, Meter Plus, outdoor meter | temperature, humidity (Hub 2 adds light level) | official API v1.1 |
| Motion / contact | motion + contact sensors | state, last-triggered | official API v1.1 |
| Device states | plugs, curtains, Bot | on/off, position, battery | official API v1.1 |
| Webhook push | all (v1.1) | event push instead of poll | official API v1.1 |

All optional in the contract; a meter-only household yields only readings
rows. BLE-only devices (Bot, Curtain, some meters) are invisible to the
cloud API without a Hub — surface this honestly on the card.

## Access & auth

- Official REST API at `api.switch-bot.com`, v1.1. Auth: token + secret
  from the app's Developer Options; each request signed with HMAC-SHA256
  (token, timestamp, nonce).
- Rate limit 10,000 req/day — at a 5-min poll that supports ~30+ devices
  comfortably; back off gracefully near the cap.
- Returns current device state only — no history endpoint; Trove
  accumulates its own history by polling (gaps while the app is closed are
  expected; the troved daemon improves coverage later).
- Webhook push exists in v1.1 but needs a reachable endpoint — not a fit
  for a local-first app; polling is the design. An unofficial Hub 2 LAN
  API exists but the official cloud API is simpler and documented.
- No TCC. Network egress to SwitchBot cloud only, labeled on the card.

## Vault mapping

- **Raw layer:** `home/switchbot/raw/YYYY-MM.jsonl` — device-status
  responses as returned, one row per device per poll.
- **Contract layer:** `home/switchbot/YYYY-MM.jsonl` per the (pending)
  home readings contract — `ts`, `guid`, device id/name/type, `kind` =
  temperature/humidity/motion/contact/…, value, unit, `extra` overflow.
- **Dedupe:** `guid` from device id + poll ts (polled samples; no
  upstream history to re-fetch).

## Build plan

1. Module `crates/trove-core/src/switchbot.rs`: `DEF` (Periodic, ~5-min
   poll), `pull` hook; HMAC-SHA256 request signing (hmac + sha2 crates).
2. `CONNECTION` (TokenPaste: two fields — token + secret — with setup copy
   walking through app → Profile → Developer Options, per the SimpleFIN
   affordance rule).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from the official repo's documented response examples (Hub 2,
   Meter, motion, contact, plug variants); parser + store tests, unique
   temp dirs.
5. Device-list refresh each sync so new devices appear without
   reconnecting; BLE-only-without-Hub devices listed with an honest
   "needs a Hub" note in Recent data.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Connect | ✅ built | paste token:secret from a real SwitchBot app; connection card shows connected |
| Readings | ✅ built | with a Hub 2 or Meter: Sync now; temp/humidity rows in `home/switchbot/` match the app; hub last-data updates |
| Motion/contact | ✅ built | battery row + state in `extra`; trigger a sensor; next poll shows the state change |
| Raw layer | ✅ built | `home/switchbot/raw/YYYY-MM.jsonl` — full status object per device per poll |
| CO2 (Meter Pro CO2) | ✅ built | `CO2` field mapped to `co2`/`ppm` metric |

## Build notes (2026-06-17)

- Module: `crates/trove-core/src/switchbot.rs`; CONNECTION added to CONNECTIONS in `integrations.rs`.
- HMAC-SHA256 signing per API v1.1 spec: `Base64(HMAC-SHA256(secret, token+timestamp_ms+nonce))`.
- Nonce is a 32-hex-char string derived from nanoseconds + atomic counter (no uuid crate needed).
- Numeric metrics (temp, humidity, lightLevel, CO2, voltage, current, etc.) emit one HomeReading each.
- Motion/contact/lock/plug ON-OFF state: if a device has no numeric field, a sentinel reading
  (0.0/1.0) is emitted for the state; all non-numeric fields ride in `extra` on every reading.
- Battery is a numeric field → gets its own HomeReading row.
- The API returns current state only; guids embed the poll timestamp → no natural deduplication
  across polls (each poll is a new snapshot); the cursor dedupes within a single pull pass.

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §SwitchBot
(L1928–L1934). Feasibility 🟢 high — official published API with HMAC
auth, well-documented. Scene execution is available in the API but out of
scope (Trove collects, it doesn't control). Cross-cutting: SwitchBot
temp/humidity merges with other `home/` sensors (Tempest, Awair, Netatmo)
at read time under the same readings contract.
