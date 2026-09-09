# Aranet4

- **id:** `aranet`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings shape
  drafted from Hue + Tempest + IAQ sensors + energy monitors together)
- **status:** 🧪 built (CSV Import; BLE path parked — see build notes below)
- **unavailable_reason:** none
- **behavior:** Periodic (BLE read of current values + on-device history
  download when the sensor is in range)
- **connection:** none — BLE pairing, no account or token. One-time macOS
  Bluetooth pairing, then local-only forever.
- **evidence:** community-documented BLE protocol (characteristic UUIDs known;
  Rust crate github.com/cameronrye/aranet) — confidence medium; no official
  docs. CSV export from the Aranet cloud app exists as a fallback but its
  format is undocumented → sample-required.
- **effort / priority:** M / P2
- **needs:** Needs-sample (CSV fallback format) · BLE spike before commit
  (macOS M-series pairing reliability)

- **status:** 🧪 built (CSV Import, HomeReading contract; BLE path parked — needs tokio/async spike)
- **build notes:** CSV format confirmed from Anrijs/Aranet4-Python README (aranetctl output:
  `date,co2,temperature,humidity,pressure`; date = "YYYY-MM-DD HH:MM:SS" local time).
  BLE direct-read via btleplug requires tokio (async), incompatible with trove-core sync pull
  hooks — that path is parked pending an architectural spike. CSV is the primary path and is
  fully supported: aranetctl CLI or Aranet mobile app export → import CSV here.
  9/9 tests pass, cargo check clean. No new deps (csv crate already in Cargo.toml).

## What it is

The Aranet4 is the gold-standard consumer CO2 monitor — a battery-powered
BLE sensor reporting CO2, temperature, humidity, and pressure. It has no
cloud dependency at all: readings live on the device (~14 days of history)
and are read over Bluetooth. Indoor air quality is a strong personal-context
signal (sleep quality, focus, ventilation habits) that nothing else in the
vault captures.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Current reading | none (hardware only) | CO2 ppm, temp, humidity, pressure, battery | community BLE protocol + Rust crate |
| On-device history | none | same fields, ~14 days back at the configured interval | community BLE protocol (separate history characteristic) |
| Cloud-app CSV export | none | same fields, longer span | undocumented format — Needs-sample |

All optional in the (pending) home contract; a user who only ever polls
current readings simply has sparser rows.

## Access & auth

- BLE direct from the sensor: current-readings characteristic
  `f0cd3001-95da-4f4b-9ac8-aa55d312af0c`; separate characteristic for the
  history log download. No account, no cloud, no LAN.
- TCC: **Bluetooth** — CoreBluetooth entitlement for the Tauri app (and
  troved if it does the polling). New permission surface for Trove; the hub
  card needs the permission hook wired.
- Sensor must be physically in BLE range at collection time; the ~14-day
  on-device buffer means a Mac that's home daily never loses data.
- Standalone-clean: fully local, zero network. The community Rust crate is
  absorbable as a library dependency, consistent with the absorb-as-library
  rule.

## Vault mapping

- **Raw layer:** `home/aranet/YYYY-MM.jsonl` — one timestamped row per
  reading (poll or history-download row), full sensor fields, per-device
  partitioning via a `device` field (serial/name) since a user may own
  several sensors.
- **Contract layer:** home contract is **Phase 3 pending**; until ratified
  this is raw-only with vault-wide conventions (guids, timestamps,
  partitions). Expected to slot into a readings shape
  (`ts`, `source`, `device`, metric fields, `extra`).
- **Dedupe:** `guid` = device serial + reading timestamp (history downloads
  overlap prior polls by design); watermark cursor in
  `.trove/aranet-sync.json`, rebuildable from output files.

## Build plan

1. **Spike first** (research doc's explicit recommendation): verify
   CoreBluetooth pairing + read reliability on recent M-series hardware —
   known forum complaints about flaky pairing. The spike result gates the
   build.
2. Module `crates/trove-core/src/aranet.rs`: `DEF` (Periodic; permission
   hook reports Bluetooth TCC state), BLE read of current + history
   characteristics, history merge with dedupe-by-timestamp.
3. Registration line in `INTEGRATIONS`. No `CONNECTION` (no login).
4. Fixtures: captured BLE payloads from the community protocol docs/crate
   tests; parser + store + cursor tests with unique temp dirs.
5. CSV-import fallback (if BLE proves unreliable, or as a backfill path):
   **parser-last, Needs-sample** — the Aranet cloud-app export format is
   undocumented; do not write the parser until a real export file is in
   hand.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| BLE pairing + current reading | ⏸ parked | btleplug requires tokio/async — incompatible with sync pull hooks; architectural spike needed |
| History download (BLE) | ⏸ parked | same blocker as above |
| CSV import (aranetctl / mobile app) | ✅ built | export history with `aranetctl XX:XX:XX:XX:XX:XX -r -o aranet4.csv`; import in hub; confirm rows land in `home/aranet/YYYY-MM.jsonl` (4 metrics × N rows) and `home/aranet/raw/YYYY-MM.jsonl` |
| Re-import dedup | ✅ built | re-import same CSV; confirm 0 new readings added, 0 duplicates in vault |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Aranet4 CO2 / Air
Quality (L1816–L1822); at-a-glance L1739. Feasibility 🟡 medium — entirely
because of macOS BLE pairing reliability, not protocol availability.
Cross-cutting note 1: this sits in the fully-local tier (most robust, most
private). The research doc routes IAQ alongside Awair/Airthings — same
pending home contract; sequence near them so the readings shape is exercised
by multiple sources.
