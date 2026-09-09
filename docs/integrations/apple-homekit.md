# Apple HomeKit

- **id:** `apple-homekit`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT shape
  drafted from HomeKit + Hue + Tempest + Enphase + Green Button together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (snapshot the local homed database; write a new
  snapshot only when the config changed)
- **connection:** none (local file read; no login, no cloud)
- **evidence:** community-schema — github.com/tamengual/homekit-extractor
  (reverse-engineered `core.sqlite` schema; good confidence for topology
  tables, lower for the nested automation blobs)
- **effort / priority:** M / P2
- **needs:** Needs-David (FDA grant on his Mac to validate the read path)

## What it is

HomeKit's local daemon (`homed`) keeps the user's entire smart-home
configuration in a CoreData SQLite database: every accessory, room, zone,
scene, and automation — the whole home topology and its logic, across all
vendors that bridge into HomeKit. No cloud API exposes this; the local DB
is the only read. It is config, not telemetry: there is **no historical
state-change log**, so this captures "what my home is and how it's
automated," snapshot over time, not "what my lights did."

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Home topology | any HomeKit user | homes, rooms, zones, accessory list (names, vendors, categories) | community schema |
| Scenes | any | scene names + member actions | community schema |
| Automations | any | trigger/condition/action definitions (nested protobuf/NSKeyedArchiver blobs) | community schema (lower confidence) |
| Device state history | — does not exist in this DB | none | research doc |

All optional in the contract (omit-if-empty); no tier gating.

## Access & auth

- Local file: `~/Library/HomeKit/core.sqlite`, read via `rusqlite` in
  **read-only mode** (`immutable`/RO open — never take a write lock while
  homed is live).
- Permissions: user-space Library, so likely readable without a prompt in
  practice, but Full Disk Access covers edge cases — reuse the existing
  FDA permission-hook pattern (imessage/calls) and surface the same
  affordance copy when the read fails.
- No network, no credentials. Standalone-clean — the best kind of source.

## Vault mapping

- **Raw layer:** `home/apple-homekit/snapshots/YYYY-MM-DD.json` — full
  decoded topology per changed-day; undecodable automation blobs kept
  base64-raw inside the snapshot (full fidelity first).
- **Contract layer:** Phase 3 pending — the home contract will likely be
  reading/state rows (Hue, Tempest), which a config snapshot doesn't fit;
  expect HomeKit to stay raw-only (snapshot shape) with `guid` = home
  UUID + snapshot date. Decision belongs to the Phase 3 contract pass.
- **Dedupe:** hash the normalized snapshot; skip the write when unchanged
  since the last one, so the folder is a sparse change-log of the home.

## Build plan

1. Module `crates/trove-core/src/apple_homekit.rs`: `DEF` (Periodic,
   daily-ish; permission hook checking the DB is readable); one line in
   `INTEGRATIONS`.
2. Schema mapping from homekit-extractor; fixture = a sanitized copy of a
   real `core.sqlite` (community confidence is good but this is
   reverse-engineered — verify table names against a live DB early).
3. Automation decode is layered (CoreData → NSKeyedArchiver → protobuf):
   decode best-effort, never fail the snapshot on a blob — store raw and
   move on. Budget the M effort here.
4. Name-based matching only when correlating with other home sources:
   the DB's accessory UUIDs are namespaced away from HAP characteristic
   ids (research note) — don't promise device-level joins.
5. Snapshot-diff tests with unique temp dirs; read-only-open test against
   a locked DB.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Topology snapshot | Needs-David | enable on a Mac signed into a HomeKit home; Sync now; confirm `home/apple-homekit/snapshots/YYYY-MM-DD.json` + hub last-data; rename a room, re-sync, see a new snapshot file |
| Automation decode | Needs-David | a home with automations shows trigger/action JSON (condition blobs appear as base64 `condition_blob`); never an error |
| FDA edge case | Needs-David | revoke FDA; card shows permission affordance; collect returns a quiet log note, not an error |
| Dedup | ✅ tested | unit tests verify same hash → skip write; added accessory → new hash → write |

## Build notes (2026-06-17)

- contract_mode = raw-only. `HomeReading` is sensor telemetry; HomeKit is config topology — no contract fit.
- Behavior: `Periodic` with `Cadence::on_change(86400s, homekit_db_mtime)` — only runs when the DB mtime changed, preventing unnecessary reads.
- Schema-adaptive: reads `PRAGMA table_info` before each SELECT; columns absent in the live DB are silently skipped. Handles macOS schema drift gracefully.
- Automation blobs (`ZEVALUATIONCONDITION`, `ZMKFACTION.ZDATA`) stored base64-encoded; never fail the snapshot on a blob.
- Content dedup via SHA-256 of topology (excluding volatile `ts` field); unchanged topology does not produce a new file.
- 9 unit tests: topology read, hash stability, hash diff on mutation, dedup logic, blob base64 round-trip, schema-absent degrades to empty, CoreData ts conversion, DEF shape, snapshot JSON round-trip.
- Needs-David: FDA grant + a live HomeKit home for end-to-end validation of the real `core.sqlite` schema (junction tables ZMKFHOME, Z_41TRIGGERS_ names may vary by macOS version).

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Apple HomeKit
(L1760–L1766); at-a-glance L1732. Feasibility 🟢 high. Part of the P2
"Home/IoT local-first batch" the research recommends shipping together
(HomeKit + Hue + Tempest + Enphase + Green Button). Cross-cutting note:
HomeKit doubles as the workaround for blocked cloud vendors — e.g. Ecobee
(developer registrations closed) appears here as config even though its
telemetry is unreachable; Ecobee's card should point at this path. Schema
is Apple-internal and can shift across macOS releases — pin expectations
in tests and degrade gracefully.
