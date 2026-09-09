# Home Assistant

- **id:** `home-assistant`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings
  shape; HA's entity state-change rows are the strongest input to that draft)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic — **opportunistic**: if the HA instance is
  unreachable, skip gracefully and retry next cycle; never error, never block
- **connection:** `home-assistant` — TokenPaste (HA Long-Lived Access Token
  from the user's HA profile page, plus instance URL). Not shared with other
  defs.
- **evidence:** official-docs — documented, stable REST + WebSocket API
  (`/api/history/period/{ts}`, `/api/logbook/{ts}`, `/api/states`)
- **effort / priority:** M / P2
- **needs:** time-sensitive (default HA recorder retention is 10 days —
  poll cadence must beat it) · home contract not yet ratified (Phase 3)

## What it is

The self-hosted smart-home hub. For users who already run Home Assistant,
one integration pulls state history for **every** device HA knows about —
Zigbee, Z-Wave, Nest, Hue, Ecobee-via-HomeKit, temperature, motion, locks,
lights, switches — the single highest-leverage source in the home domain
("superconnector", cross-cutting note 3). Strictly opt-in/opportunistic:
Trove never requires HA, and a user without HA never sees a dependency.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Entity state history | all (recorder retention, default 10 days) | per-entity timestamped state changes + attributes | official docs (`/api/history/period`) |
| Logbook | all | human-readable activity events | official docs (`/api/logbook`) |
| Current states | all | snapshot of all entities | official docs (`/api/states`) |

All optional in the contract; whatever entities a user's HA has are whatever
rows they get. No special code paths per device class — entity domain/class
ride along in the row.

## Access & auth

- REST at `http://<instance>:8123/api` (commonly `homeassistant.local`),
  Bearer = Long-Lived Access Token. WebSocket API exists (preferred for new
  HA integrations) — polling REST is sufficient and simpler for a periodic
  collector; revisit WebSocket only if a Live behavior is ever wanted.
- **Standalone rule:** HA must be running for the API to answer. Per the
  research doc this is acceptable only framed as "if you run Home Assistant,
  Trove can pull from it" — opt-in, opportunistic poll, skip-when-down,
  never a dependency. The hub card copy must carry that framing.
- LAN HTTP, no TCC, no cloud. User-supplied URL may be HTTPS with a
  self-signed cert — allow a per-connection trust override.

## Vault mapping

- **Raw layer:** `home/home-assistant/raw/YYYY-MM.jsonl` — history API
  responses, full fidelity (entity_id, state, attributes, last_changed).
- **Contract layer:** `home/home-assistant/YYYY-MM.jsonl` per the (pending)
  home contract — one row per state change (`ts`, `source`, `guid`,
  entity id, state, normalized reading fields where they map), attributes
  overflow in `extra`.
- **Dedupe:** `guid` = entity_id + last_changed timestamp (HA has no event
  id); cursor in `.trove/home-assistant-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/home_assistant.rs`: `DEF` (Periodic,
   hourly-ish — well inside the 10-day retention window), `CONNECTION`
   (TokenPaste: URL + LLAT; setup copy explains where the token lives and
   the opportunistic framing), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Unreachable-instance handling is the core test: connection refused /
   timeout → record "skipped, HA unreachable" status, no error state, no
   retry storm. Fixtures from the documented history/logbook response shapes.
4. Volume guard: an HA with thousands of entities returns big history
   payloads — pull per-period with the watermark, partition by month,
   stream-write (reads stay O(displayed) per the vault rules).
5. Vault writes via `store` helpers once the home contract is ratified;
   until then **parked behind the Phase 3 home contract**.

## Build notes (2026-06-16)

- Connection: TokenPaste, `url|token` composite (URL + Long-Lived Access Token). New `pub static CONNECTION` registered in CONNECTIONS.
- Raw layer: all entity state changes → `home/home-assistant/raw/YYYY-MM.jsonl` (unconditional).
- Contract layer: `sensor.*` entities with numeric states + a recognized `device_class` → `HomeReading` (home contract, reuse-bound). Non-numeric / non-sensor / unavailable → raw only.
- Opportunistic: HA unreachable (`HaError::Unreachable`) → `CollectOutcome::note(skipped)`, never errors the watcher loop. Auth/API errors (`HaError::Other`) propagate as real errors so they surface in the hub.
- Entity enumeration: pull_with() calls `GET /api/states` first to enumerate all entity IDs, then passes them as `filter_entity_id` to `GET /api/history/period` (chunked at 100 entities/request). Required: HA >= 2022.7 returns HTTP 400 when `filter_entity_id` is absent.
- First sync: silent baseline (cursor → current time, no rows emitted) to avoid re-importing stale HA recorder data on reconnect.
- Watermark in `.trove/home-assistant-sync.json` (non-secret); advances after successful drain. OVERLAP_SECS=5 overlap on each incremental window, deduped by guid. Watermark never advances on empty/error response.
- Unit normalization: all trove-vocabulary unit outputs are lowercase (hpa, kwh, c, f, etc.) to match home.reading schema and enable cross-source metric alignment without read-time case folding.
- Logbook endpoint NOT pulled (raw state history covers the same surface area more completely; logbook is additive revisit if desired).
- 24 unit+integration tests; all green. No new Cargo deps.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| State history | ✅ built | paste a real instance URL + LLAT; Sync now; confirm per-entity rows in `home/home-assistant/` + hub last-data (needs any user who self-hosts HA) |
| Skip-when-down | ✅ built | stop the HA instance; trigger a cycle; confirm graceful skip status and clean recovery on restart |
| First sync baseline | ✅ built | on first connect, cursor advances but zero rows emitted (silent baseline); second sync writes readings |
| Deduplication | ✅ built | re-run same window; confirm zero new rows on second pass |
| Logbook | deferred | use raw state history instead; logbook endpoint not wired |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Home Assistant
(L1872–L1878). Feasibility 🟡 medium — purely because of the
standalone-constraint framing, not the API (which is stable and official).
Time-sensitive: default recorder retention is 10 days (configurable) —
connect-time backfill grabs what exists, then steady polling preserves
everything forward. Overlap note: a user with both HA and a direct
device integration (e.g. Hue) will see the same device from two sources —
rows carry distinct `source` values; dedupe/merge is a read-time concern.
