# Awair

- **id:** `awair`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings
  shape drafted from Hue + Tempest + IAQ sensors + energy monitors together)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (LAN poll of the device's local API; optional cloud
  pull for history) + Import (dashboard CSV backfill)
- **connection:** none for the local LAN path (device IP only); `awair` —
  OAuth for the optional cloud history API (developer.getawair.com, free for
  personal use). Not shared with other defs.
- **evidence:** official-docs — local API documented at
  support.getawair.com (article 360049221014); cloud Developer API at
  developer.getawair.com; community Rust crate `awair-local-api-rs`
  (blog.yossarian.net 2023-03-20) confirms the shape. Dashboard CSV export
  officially available.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Awair (Element / 2nd Edition / Omni) is a popular prosumer indoor air
quality monitor: CO2, VOC, PM2.5, temperature, humidity, plus Awair's
composite score. Uniquely friendly for Trove: the device itself hosts a
local HTTP server on the LAN, so real-time air quality needs no account,
no cloud, no token.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Current reading (local API) | none — enable "Local API" in the Awair Home app (Omni/Enterprise: on by default) | CO2, VOC, PM2.5, temp, humidity, score | official support doc + Rust crate |
| Historical 5-min data (cloud API) | free OAuth for personal use | same fields, historical range | official developer API |
| Dashboard CSV export | none | same fields, any date range (>1yr via multiple exports) | official dashboard feature |

All optional in the contract; a LAN-only user simply accumulates from
enable-time forward, no special code paths.

## Access & auth

- **Local:** `http://<device-ip>/air-data/latest` — plain LAN HTTP, JSON,
  current reading only (no history on-device). User enables Local API in the
  Awair Home app once; Trove needs the device IP (manual entry or mDNS
  discovery later). No macOS TCC (plain outbound LAN HTTP).
- **Cloud (optional upgrade):** Awair Developer API, OAuth, free personal
  tier, 5-minute-resolution history.
- **Import:** dashboard CSV export for deep backfill.
- Standalone-clean on the primary path: no account, no cloud call.

## Vault mapping

- **Raw layer:** `home/awair/YYYY-MM.jsonl` — one timestamped row per poll
  (or per cloud-history sample / CSV row), `device` field for multi-device
  homes, full native fields.
- **Contract layer:** home contract is **Phase 3 pending**; raw-only until
  ratified (vault-wide conventions apply). Expected readings shape: `ts`,
  `source`, `device`, metric fields, score in `extra`.
- **Dedupe:** `guid` = device id + sample timestamp — local polls, cloud
  history, and CSV imports all converge on the same rows; cursor in
  `.trove/awair-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/awair.rs`: `DEF` (Periodic — poll the local
   endpoint on the IoT cadence; per cross-cutting note 2 this is a
   poll-and-accumulate source, the device keeps no history), config for the
   device IP, `pull` hook for Sync-now.
2. Local path ships first (no `CONNECTION` needed). Cloud-history OAuth
   (`CONNECTION` in this module) is a follow-up slice for backfill.
3. CSV import path via the generic import box: format is officially
   produced and column-stable per the dashboard; fixture from a real export
   when available, but parser can proceed from the documented field set.
4. Registration line(s) in `INTEGRATIONS` (+ `CONNECTIONS` when the cloud
   slice lands).
5. Fixtures from the documented local-API JSON (the Rust crate's tests are
   a usable reference); parser + store + cursor tests, unique temp dirs.
6. UI copy: "Enable Local API in the Awair Home app" setup step on the def
   (copy lives on the def, not in JSX).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Local poll | — | enable Local API on a real Element, enter device IP, Sync now; confirm rows in `home/awair/` + hub last-data |
| Cloud history | — | connect OAuth, backfill a week, confirm no duplicate guids against polled rows |
| CSV import | — | drop a dashboard export in the import box; confirm rows merge with no duplicates |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Awair Element
(L1824–L1830); at-a-glance L1740. Feasibility 🟢 high. Local API is
current-reading-only — Trove's polling builds the history (cross-cutting
note 2: shared poll-and-append scheduler in troved, ~5-min default for IAQ).
Pairs with Airthings for IAQ coverage (Airthings adds radon; cloud-only).
Models: Awair 2nd Ed and Element support the local API (beta enable
required).
