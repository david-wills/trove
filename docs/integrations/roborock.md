# Roborock

- **id:** `roborock`
- **domains:** `home/` (contract: **Phase 3 pending** — home contract;
  cleaning sessions are event-shaped and may stay per-source raw if the
  readings contract doesn't fit)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll cleaning history over LAN)
- **connection:** `roborock` — TokenPaste-shaped: one-time Xiaomi cloud
  login to extract the device token, then local-only forever. Not shared
  with other defs.
- **evidence:** community-schema — python-miio library (MIIO protocol, UDP
  54321), widely used; HA Roborock integration as reference. Confidence
  medium-high (protocol stable, official API closed).
- **effort / priority:** M / P2
- **needs:** none (token extraction UX needs care, but no contract or
  sample blockers beyond the home contract: Needs-David)

## What it is

Robot-vacuum cleaning history: when each clean ran, how long, area covered,
errors, and map snapshots. Daily-rhythm context (cleaning happens when the
home is empty or on schedule) plus a record of home maintenance. Roborock
is the recommended brand bet — iRobot filed Chapter 11 in December 2025
(research doc), making Roomba's future uncertain.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Cleaning history | all devices | start ts, duration, area cleaned, error code | python-miio `clean_history()` |
| Last clean detail | all devices | per-run detail | python-miio `last_clean_details()` |
| Maps | all devices | binary map snapshot (proprietary format) | python-miio `get_maps()` |

All optional in the contract. Maps are stored as opaque raw artifacts —
decoding the proprietary binary format is explicitly out of scope for v1.

## Access & auth

- Local MIIO protocol on UDP port 54321 to the vacuum's LAN IP; auth is the
  per-device token.
- Token extraction requires a **one-time** Xiaomi cloud login (the same
  flow HA uses) or manual extraction from the Android app. Once the token
  is in the vault, communication is local-only forever — strongly
  standalone-aligned.
- Implementation is a Rust async UDP client speaking the
  community-documented MIIO protocol (no Python sidecar — absorb as a
  library, per the standalone rule).
- No TCC. Network is LAN-only after token extraction; the one-time cloud
  call is labeled in the connect flow.

## Vault mapping

- **Raw layer:** `home/roborock/raw/YYYY-MM.jsonl` — clean-history records
  as returned; map snapshots as binary sidecar files under
  `home/roborock/maps/`.
- **Contract layer:** cleaning runs are session events, not sensor
  readings — if the (pending) home contract is readings-shaped, Roborock
  stays per-source raw (allowed alongside contract rows); decide at
  contract ratification.
- **Dedupe:** `guid` from device id + clean start ts; cursor = latest
  clean start, rebuildable from output files.

## Build plan

1. Spike first: MIIO protocol handshake + token auth in Rust (UDP, AES);
   verify `clean_history` against a real device before building the module.
2. Module `crates/trove-core/src/roborock.rs`: `DEF` (Periodic, ~hourly),
   `CONNECTION` (token-extraction flow: cloud-login step clearly labeled
   as one-time, with manual token paste as the fallback method).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Fixtures from python-miio's documented response shapes; parser + store
   + cursor tests, unique temp dirs.
5. Maps: store raw bytes only; no decode.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Token extraction | — | run the connect flow against a real Roborock account; token lands in vault config |
| Cleaning history | — | Sync now on LAN with the vacuum; confirm rows in `home/roborock/` + hub last-data; run a clean and re-sync to see the new row |
| Local-only operation | — | after connect, block WAN for the app and confirm sync still works on LAN |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Roborock Robot
Vacuum (L1904–L1910). Feasibility 🟡 medium. Official Roborock API is
closed; the community MIIO protocol is the only path and is widely used
(python-miio, HA). iRobot Roomba considered and deprioritized
(Chapter 11 Dec 2025; Roborock is the safer 2026+ bet). Maps are
proprietary binary — raw-store only.
