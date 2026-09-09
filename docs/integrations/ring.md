# Ring

- **id:** `ring`
- **domains:** `home/` (contract: **Phase 3 pending** — home/IoT readings +
  event shape; doorbell/motion events are event-shaped, not interval-shaped)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll event history; watermark cursor)
- **connection:** `ring` — TokenPaste-shaped (Ring account email + password +
  2FA code; the unofficial API exchanges them for a refresh token we store).
  Not shared with other defs.
- **evidence:** community-schema — python-ring-doorbell v0.9.14 (Feb 2026,
  actively maintained) and ring-client-api (npm) document the
  reverse-engineered endpoints; **no official API and no export feature**
- **effort / priority:** M / P2
- **needs:** Needs-login (real Ring account for build + validation — no
  fixtures without one) · time-sensitive (~180-day cloud retention) ·
  home contract not yet ratified (Phase 3)

## What it is

Doorbell and security-camera event logs from Ring (Amazon): who rang, when
motion was detected, which device fired. High personal-context value —
a presence/activity record of the home's perimeter that exists nowhere else
and that Ring itself deletes after ~180 days. Metadata only; video clips are
out of scope for v1.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Event history | all (cloud keeps ~180 days) | ts, event type (ding/motion/on-demand), device id/name | community (python-ring-doorbell) |
| Device health | all | device list, battery, connectivity | community |
| Video thumbnails/clips | Ring Protect plans | authenticated URLs (not collected v1) | community |

All optional in the contract. Clip URLs are recorded in `extra` if present
but never downloaded in v1 (size + plan gating); a later opt-in slice could
fetch thumbnails.

## Access & auth

- Unofficial reverse-engineered cloud API only. Auth: Ring account email +
  password + 2FA, then a stored refresh token. Python library went through a
  firebase-messaging migration in 2025 — push paths churn; stick to polling.
- Plain outbound HTTPS — passes the standalone rule (no Ring app required).
  ring-client-api as a Tauri sidecar was floated in research; prefer a native
  Rust HTTP client against the same endpoints (no sidecar runtime).
- Amazon can change the private API at any time. Graceful-degradation UI
  required (cross-cutting note 5): on auth/endpoint breakage, surface
  "Ring changed their private API" on the card — never silently fail.

## Vault mapping

- **Raw layer:** `home/ring/raw/YYYY-MM.jsonl` — API event objects, full
  fidelity; device snapshots alongside.
- **Contract layer:** `home/ring/YYYY-MM.jsonl` per the (pending) home
  contract's event shape — one row per event (`ts`, `source`, `guid` =
  Ring event id, device, event type), overflow in `extra`.
- **Dedupe:** Ring event id as `guid`; cursor in `.trove/ring-sync.json`,
  rebuildable by scanning output files.

## Build notes (2026-06-16)

- **Behavior:** Periodic (12 h cadence — `RING_SYNC_SECS = 43200`).
- **Connection:** NEW `ring` ConnectionDef (TokenPaste: `email:password` or
  `email:password:2fa_code`). Ring's unofficial OAuth2 password-grant exchanges
  credentials for a `{access_token, refresh_token}` pair stored 0600 under
  `.trove/sync/ring.json`. Access tokens are auto-refreshed before each pull.
  2FA: on first paste Ring returns HTTP 412; the help copy instructs the user
  to re-paste with the OTP appended as the third colon-delimited field.
- **Vault layout:**
  - `home/ring/events/YYYY-MM.jsonl` — home.event shape (ts, source, device,
    event, guid, extra) written as plain `serde_json::Value` (no Rust struct
    — `home.event` is an unbound Phase-3 draft).
  - `home/ring/raw/YYYY-MM.jsonl` — verbatim event objects, full fidelity.
  - `home/ring/raw/devices/YYYY-MM.jsonl` — device-list snapshot per pull.
- **contract_mode:** `deferred-sibling-draft` (home.event) — Ring events
  are discrete events (motion / doorbell / livestream), NOT sensor readings.
  The `HomeReading` struct does not fit. When the `home.event` Rust type is
  ratified, a follower maps the events layer rows into it.
- **Endpoints confirmed** (python-ring-doorbell v0.9+ const.py + fixtures):
  - Auth: `POST https://oauth.ring.com/oauth/token`, `client_id=ring_official_android`,
    `scope=client`, `grant_type=password`; `2fa-support: true` header always sent.
  - Devices: `GET https://api.ring.com/clients_api/ring_devices`
  - History: `GET https://api.ring.com/clients_api/doorbots/{id}/history?limit=100&older_than={cursor}`
  - Event fields: `id` (u64, monotonically descending), `created_at` (ISO8601 UTC),
    `kind` (motion/ding/on_demand), `answered` (bool), `recording.status`, `snapshot_url`.
- **27 unit tests pass**, including: credential parsing (with/without 2FA),
  device list parsing, event → home.event mapping, full pull, incremental
  watermark, dedup re-pull, stickup cam, no-connection guard, token parsing.
- **Connection line to add to CONNECTIONS:** `&crate::ring::CONNECTION,` (already added
  to `integrations.rs` in this worktree; integrator merges it).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Event history | built, Needs-login | connect a real Ring account (email:password or with 2FA); Sync now; confirm motion/doorbell rows in `home/ring/events/` + hub last-data |
| Device health | raw only | same run; confirm device snapshot in `home/ring/raw/devices/` |
| 2FA flow | built, Needs-login | connect an account with 2FA; confirm 412 error guides user to re-paste with code |
| Breakage UX | built | revoke the token; confirm card shows "Ring changed their private API" error, not silent skip |
| home.event contract | deferred | when home.event Rust type is ratified, a follower maps `home/ring/events/` rows into the struct |

## Research notes

`integrations-research.md` → "Home, IoT & Smart Devices" §Ring (L1864–L1870).
Feasibility 🟡 medium. The Amazon Alexa voice-history export mentioned in the
same research entry is a **separate provider** (`amazon-alexa`, Import,
privacy-flagged) — do not fold it in here. Time-sensitive: 180-day cloud
retention means data older than the first connect is gone forever — the hub
copy should say so.
