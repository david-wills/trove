# TP-Link Kasa / Tapo

- **id:** `tplink-kasa`
- **domains:** `home/` (contract: **home.HomeReading** reused for
  power/voltage/current; daily/monthly kWh written raw per home.energy
  draft schema for forward compatibility)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (LAN discovery + per-device polling every 5 min;
  accumulate — devices keep only coarse totals)
- **connection:** none for classic Kasa (pure LAN); Tapo-branded models
  need a one-time TP-Link cloud auth — future slice.
- **evidence:** Verified against python-kasa source (github.com/python-kasa/
  python-kasa): XOR autokey cipher (key=171), port 9999, 4-byte big-endian
  length prefix. Field names confirmed from kasa/iot/iotdevice.py (sysinfo)
  and kasa/iot/modules/emeter.py (emeter realtime / daystat / monthstat).
  Both old (bare `power`/`voltage`/`current`) and new (`power_mw`/`voltage_mv`/
  `current_ma`) firmware response formats handled.
- **effort / priority:** S / P2
- **contract_mode:** reuse-bound (home.HomeReading) + raw-only for home.energy
  draft (daily/monthly kWh)

## What it is

Per-plug energy monitoring from TP-Link's Kasa and Tapo smart plugs: watts
right now, daily and monthly kWh per device. Complements whole-home
monitors (Sense, Emporia, Green Button) with appliance-level granularity,
and the classic-Kasa path is fully local — no cloud dependency once the
plugs are on the network.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Live power | emeter models only (HS110, KP115, KP125, EP25; Tapo P110/P115) | current W, voltage, amperage | python-kasa emeter |
| Energy totals | emeter models only | daily kWh, monthly kWh | python-kasa emeter |
| Device state | all models | on/off, device name/alias | python-kasa |

All optional in the contract; non-emeter plugs simply yield state rows
without energy fields. **Model-dependent:** some firmware versions removed
the local API entirely — detect at discovery and show per-device status
honestly.

## Access & auth

- Kasa: LAN UDP/TCP JSON protocol — broadcast discovery, then per-device
  polling by IP. No account, no cloud.
- Tapo: separate encrypted protocol; requires one-time TP-Link cloud auth
  to obtain local credentials, local thereafter.
- Implementation: reimplement the community-documented Kasa JSON protocol
  in Rust (research doc explicitly notes this option) — no Python sidecar,
  per the standalone rule. Tapo protocol is a follow-on slice.
- No TCC. Local-network permission prompt on macOS applies (LAN
  multicast/broadcast).

## Vault mapping

- **Raw layer:** `home/tplink-kasa/raw/YYYY-MM.jsonl` — polled device
  readings as returned, one row per device per poll.
- **Contract layer:** `home/tplink-kasa/YYYY-MM.jsonl` per the (pending)
  home readings contract — `ts`, `guid`, device id/alias, `kind` =
  power/energy, value, unit, `extra` overflow.
- **Dedupe:** `guid` from device id + poll ts (polled samples, not
  fetched history — devices hold only coarse totals, so Trove accumulates
  its own history; gaps when the app is closed are expected and honest).

## Build plan

1. Module `crates/trove-core/src/tplink_kasa.rs`: `DEF` (Periodic,
   ~5-min poll), discovery + per-device poll over LAN; Kasa JSON protocol
   in Rust.
2. No `CONNECTION` for v1 (Kasa LAN only); Tapo cloud-credential step
   ships as a later slice with a TokenPaste-shaped connect method.
3. Registration line in `INTEGRATIONS`.
4. Fixtures from python-kasa's documented response shapes (emeter and
   non-emeter variants); parser + store tests, unique temp dirs.
5. Per-device capability detection: emeter-less and local-API-removed
   models render honestly in Recent data rather than erroring.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Discovery | ✅ built (mock tested) | with a Kasa plug on LAN: enable; device appears with alias in Recent data |
| Energy readings (W/V/A) | ✅ built (mock tested) | emeter model (e.g. KP125) under load; Sync now; W/V/A rows in `home/tplink-kasa/YYYY-MM.jsonl` match Kasa app |
| Daily/monthly kWh | ✅ built (raw, mock tested) | `home/tplink-kasa/energy/YYYY-MM.jsonl` shows day_list + month_list entries |
| Non-ENE device (state only) | ✅ built (mock tested) | HS100/HS105 raw poll written; no emeter rows |
| Tapo slice | — | later slice: one-time cloud auth, then local poll of a P110 |
| Needs-login (Tapo) | — | Tapo requires cloud auth to derive local AES key; deferred |

## Build notes

- **XOR cipher**: implemented from scratch in pure Rust — `kasa_encrypt` / `kasa_decrypt`
  (key=171, autokey, 4-byte big-endian length prefix). No external crate needed;
  all transport via `std::net::TcpStream` and `std::net::UdpSocket`.
- **Contract layer**: `HomeReading` rows (power/voltage/current in W/V/A) for
  ENE devices; deduped by `extra.guid = "tplink-kasa:{deviceId}:{metric}:{ts_ms}"`.
- **Energy layer**: raw JSONL under `home/tplink-kasa/energy/` following home.energy
  draft schema (ts, source, device, circuit, kwh, direction, guid, interval_secs);
  NOT a bound Rust type — deferred_sibling_draft = home.energy.
- **Both firmware formats handled**: older firmware uses bare `power`/`voltage`/
  `current`/`total`; newer uses `power_mw`/`voltage_mv`/`current_ma`/`total_wh`
  (milli-units). Parser tries `_mw` suffixed first, then falls back.
- **18 tests pass**: cipher round-trips, both firmware formats, ENE/no-ENE routing,
  three-layer write, energy row shapes, dedup, cursor back-compat.

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §TP-Link Kasa
Smart Plugs (L1920–L1926). Feasibility 🟡 medium (model/firmware
fragmentation, not access difficulty). python-kasa now supports both Kasa
and Tapo; cloud API (tplink-cloud-api) exists but the local path is
preferred. Energy-monitoring model list carried from the research doc:
HS110, KP115, KP125, EP25 (Kasa); P110, P115, EP25 (Tapo).
