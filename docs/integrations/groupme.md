# GroupMe

- **id:** `groupme`
- **domains:** `correspondence` (contract: ✅ ratified)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll groups + DMs; paginate with `before_id`,
  watermark per conversation) + Import (official ZIP export)
- **connection:** `groupme` — TokenPaste (personal access token from
  dev.groupme.com; no app registration, no OAuth dance)
- **evidence:** official-docs — GroupMe API v3 at dev.groupme.com
  (`/v3/groups`, `/v3/groups/{id}/messages`); official ZIP export
  (Settings → Export Data)
- **effort / priority:** M / P2
- **needs:** privacy-sensitive (message bodies — opt-in) · Needs-login
  (validation needs a GroupMe account; build proceeds from documented shapes)

## What it is

Microsoft-owned group-messaging app, popular with US universities, sports
teams, and clubs. Years of group-chat history for that demographic —
correspondence that lives only on GroupMe's servers.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Group messages | free (all of GroupMe is free) | sender, text, ts, group id, attachments metadata, likes | official API docs |
| Direct messages | free | sender, text, ts, conversation id | official API docs |
| Full-history ZIP export | free (Settings → Export Data) | JSON per conversation | official export mechanism |

All optional in the contract; no tier gating exists.

## Access & auth

- REST: `https://api.groupme.com/v3/groups` (list),
  `/v3/groups/{id}/messages` (paginate with `before_id`); DMs via
  `/v3/chats` + `/v3/direct_messages`. Auth: personal access token in a
  request header — TokenPaste, no keys to ship.
- ToS caching clause: API consumers may only cache ~100 messages / 24h
  worth for 3 days. Awkward for archival; the official ZIP export
  (Settings → Export Data) carries no such clause and is the clean
  bulk-history path. Strategy: Import for history, Periodic API pull for
  the live tail.
- No TCC, no local files. Standalone-clean (plain HTTPS).
- **Privacy:** message bodies — ships opt-in with explicit acknowledgement.

## Vault mapping

- **Raw layer:** `correspondence/groupme/raw/YYYY-MM.jsonl` — API message
  objects / export JSON, full fidelity.
- **Contract layer:** `correspondence/groupme/YYYY-MM.jsonl` per the
  ratified correspondence contract — one row per message (`guid` = GroupMe
  message id, `thread` = group/conversation id, sender handle, body, ts;
  likes/attachments metadata in `extra`).
- **Dedupe:** message id as `guid` — the same id appears in API and export,
  so the two mechanisms dedupe against each other. Cursor in
  `.trove/groupme-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/groupme.rs`: `DEF` (Periodic + import
   hook), `CONNECTION` (TokenPaste: help copy points at dev.groupme.com →
   Access Token, per the SimpleFIN affordance rule), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. ZIP-export parser: the export's JSON schema is less documented than the
   API — build that parser last and flag **Needs-sample** for the export
   variant; the API path proceeds from official docs.
4. Fixtures from dev.groupme.com example responses (group message w/
   attachments + likes, DM); parser + store + cursor tests, unique temp dirs.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Group + direct messages (API) | 🧪 built | paste a real token in the connect card; Sync now; rows in `correspondence/groupme/` + hub last-data |
| ZIP export import | — (deferred) | a future Import arm will parse the ZIP and dedupe against API rows on the same message id |

## Build notes (2026-06-17)

- Implemented as `Behavior::Periodic` (30-min cadence). The `Behavior` enum is single-variant,
  so the ZIP-import arm is deferred to a separate integration entry (`groupme-import` or
  similar); the brief's "Periodic + Import" cannot be collapsed into one `DEF`.
- DM endpoint confirmed via community docs: `GET /v3/direct_messages?other_user_id=<id>` with
  `before_id` pagination; response key is `direct_messages` (not `messages`).
- Group messages: `GET /v3/groups/{id}/messages`, response key is `response.messages`.
  Pagination: `before_id` of the last id returned; stop when page is short or id is already seen.
- `other_user.id` arrives as a JSON integer in the `/chats` response — coerced to string.
- Token stored in `.trove/sync/groupme.json` (0600); watermarks in `.trove/groupme-sync.json`.
- Raw layer always written to `correspondence/groupme/raw/YYYY-MM.jsonl`; contract rows to
  `correspondence/groupme/YYYY-MM.jsonl` via `vault.append_messages`.
- 10 unit tests, all green; `cargo check -p trove-core` clean.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §GroupMe (L377-L383).
Feasibility 🟢 high — simple REST, keyless personal token. The ToS caching
clause is the one wrinkle: practical enforcement for personal archiving is
nil, but leading with the official export keeps the bulk-history path
unambiguous. GroupMe is Microsoft-owned but uses its own token system —
no relation to the `microsoft` OAuth connection.
