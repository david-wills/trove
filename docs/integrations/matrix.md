# Matrix / Element

- **id:** `matrix`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll `/sync` with the stored since-token as
  watermark)
- **connection:** `matrix` — TokenPaste (Matrix access token from any
  client login + homeserver URL; works for matrix.org, element.io, and
  self-hosted servers alike)
- **evidence:** official-docs — spec.matrix.org (published open spec;
  Client-Server API, `/sync` incremental endpoint)
- **effort / priority:** M / P2
- **needs:** privacy-sensitive (message bodies — ships opt-in with explicit
  acknowledgement) · Needs-login (validation needs a real homeserver
  account; build proceeds from the published spec)

## What it is

The open, federated messaging protocol; Element is its flagship client.
Niche outside open-source/tech communities but growing, and the user base
that has it tends to have *years* of room history there. The API is an
open published spec — the cleanest evidence level in the messaging
catalog.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Room messages (non-E2EE) | any homeserver, free | body, sender, room, ts, event id | spec.matrix.org |
| Room/membership events | any homeserver | joins, leaves, topic changes | spec.matrix.org |
| E2EE room messages | needs client encryption keys | same fields, post-decrypt | spec.matrix.org + matrix-rust-sdk |

All optional in the contract; an account with only encrypted rooms simply
yields nothing until the E2EE slice ships. No tier-specific code paths.

## Access & auth

- Matrix Client-Server API: `GET /_matrix/client/v3/sync` (incremental,
  `since` token), `GET /_matrix/client/v3/rooms/{roomId}/messages`.
- Auth: long-lived access token pasted from any Matrix client login; the
  homeserver URL is part of the connect card (no central service).
- No macOS TCC; plain HTTPS to the user's chosen homeserver —
  standalone-clean.
- E2EE rooms require the client's encryption keys (Element's local
  IndexedDB or a key backup) and the `vodozemac`/`matrix-sdk` Rust crates —
  a significant compile-time dependency. Phase the build: non-E2EE first,
  E2EE as a later slice (or via `element-hq/matrix-archive`-style export
  import as a stopgap).

## Vault mapping

- **Raw layer:** `correspondence/matrix/raw/YYYY-MM.jsonl` — sync event
  objects, full fidelity.
- **Contract layer:** `correspondence/matrix/YYYY-MM.jsonl` per the
  ratified correspondence contract — `chat` = `room_id`, `chat_name` =
  room display name, `sender` = mxid, `kind:"message"` for m.room.message,
  `kind:"event"` for membership/topic events, `service` = homeserver host.
  Overflow (event type, relations) in `extra`.
- **Dedupe:** Matrix event id as `guid`; the `/sync` since-token lives in
  `.trove/matrix-sync.json`, rebuildable by scanning output files for the
  newest event.

## Build plan

1. Module `crates/trove-core/src/matrix.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste: token + homeserver fields, setup copy
   explains where Element shows the access token), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Thin reqwest client against the spec — defer `matrix-sdk` until the
   E2EE slice; fixtures built from spec.matrix.org example responses
   (message, membership event, redaction).
4. Privacy: message bodies — opt-in toggle with explicit acknowledgement
   before first sync.
5. E2EE rooms: explicitly out of the first iteration; the hub card states
   "encrypted rooms not yet supported" so the gap is honest.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Non-E2EE messages | — | paste a matrix.org token; Sync now; confirm rows in `correspondence/matrix/` + hub last-data; second sync advances the since-token without duplicates |
| Membership events | — | same run; confirm `kind:"event"` rows for a room join |
| E2EE rooms | — | deferred slice — validate only once vodozemac lands |

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Matrix / Element
(L409-L415; at-a-glance L202). Feasibility 🟢 high. Cross-cutting notes:
store per-account watermarks in `.trove/sync/` like Gmail/Oura; Matrix is
rooms-not-threads, so `room_id` is the conversation key. Main risk is the
E2EE dependency weight — sequenced as a follow-up slice, not a blocker.
