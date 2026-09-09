# Moen Flo

- **id:** `moen-flo`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings
  shape; water consumption is interval-shaped like energy, leak detections
  are event-shaped)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll consumption + telemetry; watermark cursor)
- **connection:** `moen-flo` — TokenPaste-shaped (Flo account email +
  password; the unofficial API exchanges them for a bearer token we store).
  Not shared with other defs.
- **evidence:** community-schema — the maintained Home Assistant `flo`
  integration (home-assistant.io/integrations/flo) is the working reference
  implementation of the reverse-engineered cloud API; **no official API, no
  local API, no export**
- **effort / priority:** M / P2
- **needs:** Needs-login (real Flo account for build + validation — no
  fixtures without one) · home contract not yet ratified (Phase 3)

## What it is

Whole-home smart water monitor (shutoff valve + flow sensor): flow rate,
water temperature, pressure, daily/weekly/monthly consumption, leak-detection
events, valve state. Water-usage history is unique personal data available
nowhere else — compelling for sustainability tracking — and the device is
entirely cloud-dependent, so the unofficial cloud API is the only path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Consumption | all | daily/weekly/monthly gallons (HA exposes flow_today, consumption_today_gallons) | community (HA flo) |
| Live telemetry | all | flow rate (gal/min), water temp, pressure | community (HA flo) |
| Leak events | all (richer alerting on FloProtect plan) | leak detection events, health-test results | community (HA flo) |
| Valve state | all | open/closed | community (HA flo) |

All optional in the contract; rows carry whatever the account returns. Plan
gating (FloProtect) never needs special code paths.

## Access & auth

- Unofficial reverse-engineered cloud API (api.meetflo.com per the HA
  integration). Auth: Flo account credentials → bearer token. No local API;
  the device talks only to Moen's cloud.
- Plain outbound HTTPS — passes the standalone rule. Port the HA
  integration's request shapes to a native Rust client; no Python runtime.
- Unofficial-API risk: Moen can change it any time. Card copy says so;
  on breakage surface the honest error, never silently fail
  (cross-cutting note 5).

## Vault mapping

- **Raw layer:** `home/moen-flo/raw/YYYY-MM.jsonl` — API responses
  (consumption queries, telemetry snapshots, alerts), full fidelity.
- **Contract layer:** `home/moen-flo/YYYY-MM.jsonl` per the (pending) home
  contract — consumption as interval rows (`ts`, `source`, `guid`, device
  id, gallons, interval), telemetry as reading rows, leak detections as
  event rows; overflow in `extra`.
- **Dedupe:** `guid` = device id + interval/alert timestamp (alert id where
  the API provides one); cursor in `.trove/moen-flo-sync.json`, rebuildable.

## Build notes (2026-06-17, updated 2026-06-17)

Built as a Periodic pull (hourly). Module `crates/trove-core/src/moen_flo.rs`:
- `pub static DEF` (Periodic/hourly) and `pub static CONNECTION` (TokenPaste: email:password).
- Auth: `POST /api/v1/users/auth` → bearer token stored in `.trove/sync/moen-flo.json`
  (access_token=bearer, refresh_token=password for re-auth on expiry, token_type=email for display).
- Pull: user info → locations → per-device telemetry snapshots + hourly consumption.
- Three output streams:
  - `home/moen-flo/YYYY-MM.jsonl` — HomeReading contract rows (flow_rate/gal_min,
    water_pressure/psi, water_temperature/F) from `telemetry.current`.
  - `home/moen-flo/energy/YYYY-MM.jsonl` — home.energy draft JSONL (water intervals,
    gallons/hour, deferred until home.energy contract is bound by a pioneer).
  - `home/moen-flo/events/YYYY-MM.jsonl` — home.event draft JSONL (alert state changes,
    deferred until home.event contract is bound).
  - `home/moen-flo/raw/YYYY-MM.jsonl` — full-fidelity API responses (unconditional).
- Cursor `.trove/moen-flo-sync.json`: per-location consumption watermarks + per-device
  alert states (for diff-based event emission) + last-sync time.
- **Alert dedup**: `notifications.pending` is a current-state snapshot, not an event
  stream. Events are emitted only when critical/warning counts *change* from the
  previous poll. First-run establishes a silent baseline (no row for standing alerts
  at connect time) to avoid retroactive phantom events.
- **Consumption window**: requests are chunked per day (matching the HA coordinator
  pattern) to avoid silent truncation if the API is window-scoped. First-sync default
  is 30 days (documented cap — no confirmed full-history backfill beyond this window).
- CONNECTION registered in CONNECTIONS (integrations.rs line added by fan-out integrator).
- Evidence sources: aioflo library fixtures (device_info_response.json,
  water_consumption_info_response.json, water_metric_info_response.json,
  user_info_expand_locations_response.json); HA flo coordinator.py field paths.
- 23 unit tests, all green. No new Cargo deps needed.
- Needs-login flag: no real Flo fixture can be captured without an account.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Consumption | — | connect a real Flo account; Sync now; confirm daily-gallons rows in `home/moen-flo/` + hub last-data (needs a Flo owner — David doesn't have one) |
| Telemetry + valve | — | same run; confirm flow/temp/pressure and valve-state rows |
| Leak events | — | account with a past alert (or run a manual health test); confirm event rows |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Moen Flo Smart
Water Monitor (L1888–L1894). Feasibility 🟡 medium — stable-in-practice
unofficial API with a maintained HA reference. Competitor note carried from
research: Phyn Plus has no API and no export planned (Jan 2025) — skip Phyn;
catalogue-level unavailable if ever requested. Green Button is the better
path for total household water where the utility supports it (separate
`green-button` provider).
