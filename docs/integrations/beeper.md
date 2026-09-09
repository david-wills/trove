# Beeper

- **id:** `beeper`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic, opportunistic — polls the localhost Beeper Desktop
  API when Beeper is running; silently idle (never erroring) when it isn't
- **connection:** none (local HTTP on localhost; no token, no OAuth, no TCC)
- **evidence:** official-docs — Beeper Desktop API (local-only HTTP;
  `GET /api/v1/chats`, `GET /api/v1/chats/{id}/messages`; JS/Python/Go/PHP
  SDKs)
- **effort / priority:** M / P2
- **needs:** privacy-sensitive (message bodies across many networks —
  opt-in) · Needs-David (he'd need Beeper installed to validate; any
  Beeper-using owner of the app can)

## What it is

Paid unified-messaging app bridging WhatsApp, Signal, Telegram, Instagram,
Google Messages, LinkedIn, X, Discord, Slack and more into one client — and
exposing it all through a documented, local-only HTTP API. For a Beeper
user, one collector yields message history from a dozen networks Trove may
never reach individually (several are hard-blocked on their own).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Chats across all bridged networks | Beeper subscription (paid app) | chat id, network, participants | official Desktop API docs |
| Messages per chat | Beeper subscription | sender, text, ts, network, attachments metadata | official Desktop API docs |

All optional in the contract; which networks appear depends entirely on
what the user has bridged — no special code paths per network.

## Access & auth

- Local HTTP API on localhost; no auth beyond being local, no TCC. Read
  `GET /api/v1/chats`, then `GET /api/v1/chats/{id}/messages` per chat.
- **Standalone-rule tension, resolved by framing:** Beeper must be
  installed AND running. This ships as an explicitly opt-in "enhanced"
  collector — the card states the requirement, the collector is
  opportunistic (no-op when Beeper is absent), and nothing else in Trove
  depends on it. Never a dependency, per the hard rule.
- Privacy posture is decent: Beeper's On-Device Connections mode keeps
  WhatsApp/Signal traffic device-to-network; other bridges may relay
  through Beeper's servers — note this in the card copy.
- **Privacy:** message bodies, many networks at once — ships opt-in with
  explicit acknowledgement.

## Vault mapping

- **Raw layer:** `correspondence/beeper/raw/YYYY-MM.jsonl` — API message
  objects, full fidelity, tagged with their source network.
- **Contract layer:** `correspondence/beeper/YYYY-MM.jsonl` per the
  ratified correspondence contract — one row per message (`guid` = Beeper
  message id, `thread` = chat id, network recorded in `extra.network` plus
  the handle fields).
- **Overlap note:** a user may also run a direct Trove integration for a
  network Beeper bridges (e.g. Slack, Telegram archive). Rows stay in
  separate per-source folders (`correspondence/beeper/` vs
  `correspondence/slack/`) — raw vault data stays complete; read-time views
  handle cross-source overlap, not write-time suppression.
- **Dedupe:** Beeper message id as `guid`; cursor per chat in
  `.trove/beeper-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/beeper.rs`: `DEF` (Periodic, default-off
   per the privacy flag; permission hook = "is the localhost API
   reachable", surfacing a clear "Beeper not running" hint per the
   disabled-affordance rule), `pull` hook.
2. Registration line in `INTEGRATIONS`. No `CONNECTION` (localhost, no
   credentials).
3. Fixtures from the Desktop API docs/SDK examples (multi-network chat
   list, message page); parser + store + cursor tests, unique temp dirs.
4. Spike first per the research recommendation: confirm the localhost port/
   discovery mechanism and whether any local pairing step exists before
   committing the polling shape.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Chats + messages | — | with Beeper installed, running, and ≥2 networks bridged: enable the opt-in toggle; Sync now; rows in `correspondence/beeper/` with correct `extra.network`; hub last-data updates |
| Opportunistic idle | — | quit Beeper; collector goes quiet with the "Beeper not running" hint, no errors logged |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Beeper (L401-L407).
Feasibility 🟡 medium — the API itself is elegant; the constraint is the
running-app requirement, handled as opt-in opportunism rather than a
dependency. Remarkably high leverage: one local API covers networks that
are individually blocked (WeChat-class problems don't apply — Beeper
already did the bridging). Texts.app is the competitor but exposes no
local API. Time-sensitivity low; revisit pricing copy if Beeper's plans
change.
