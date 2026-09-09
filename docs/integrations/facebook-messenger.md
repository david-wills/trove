# Facebook Messenger

- **id:** `facebook-messenger`
- **domains:** `correspondence/` (contract: **correspondence — ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Meta "Download Your Information" export)
- **connection:** none — export is user-initiated in Meta's Accounts
  Center; no personal API exists (Graph Messenger Platform is restricted
  to businesses/platforms with app review)
- **evidence:** official export mechanism (Accounts Center → Download Your
  Information → Messages; also messenger.com/your-data);
  community-documented JSON structure — research feasibility 🟢 high
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (message bodies — explicit opt-in
  acknowledgement at import time)

## What it is

One of the most used messengers globally. The official Meta export is
well-structured JSON covering **all** DMs and group chats (both sides of
every conversation — better than many platforms' sent-only packages).
For most users this is a decade-plus of social history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| DM + group messages | all accounts | sender_name, timestamp_ms, content | community-documented export JSON |
| Reactions | all accounts | reactions array per message | same |
| Shares / photos | all accounts | share URLs, photo references (metadata) | same |

All optional in the contract; sparse threads import fine.

## Access & auth

- Meta Accounts Center: Settings & Privacy → Your Facebook Information →
  **Download Your Information** → Messages, format **JSON** (HTML variant
  exists; Trove takes JSON only — say so in the import copy).
- ZIP layout: `messages/inbox/<ConversationName>_<hash>/message_<N>.json`,
  split across numbered files per conversation; each file has a
  `participants` array + `messages` array.
- Trove side: no auth, no network, no TCC. Standalone-clean.
- **Known data bug:** Meta exports mojibake-encode some string fields
  (Latin-1 decoded as UTF-8). Fix once in a shared decoder — the same fix
  serves the Instagram DMs provider (same Accounts Center, same schema).

## Vault mapping

- **Raw layer:** none beyond contract rows; export-only fields (share
  objects, sticker references) ride in `extra`.
- **Contract layer:** `correspondence/facebook-messenger/YYYY-MM.jsonl`
  per the ratified correspondence contract — `kind:"message"` /
  `"reaction"`, `chat` = conversation folder key, `chat_name` = display
  name, `sender_name` (the export carries names, not stable handles),
  `from_me` matched against the export owner's name, `text` from
  `content`, photos/shares as attachment metadata,
  `service:"Facebook Messenger"`.
- **Dedupe:** no message ids in the export → `guid` = hash of
  (chat, timestamp_ms, sender_name) per the research note; collision risk
  at identical-ms is negligible in practice and documented.

## Build plan

1. Module `crates/trove-core/src/facebook_messenger.rs`: `DEF` (Import),
   walks `messages/inbox/` (and `archived_threads/` if present), merges
   the numbered `message_N.json` splits per conversation.
2. Shared **Meta mojibake decoder** as a small helper usable by the
   future `instagram` provider — build it here, fix it once.
3. One registration line in `INTEGRATIONS`. No `CONNECTION`.
4. Fixtures: a two-conversation export slice including a reaction, a
   photo message, a multi-file split, and a mojibake string (é-class
   field); parser/decoder/dedupe/store tests, unique temp dirs.
5. Privacy acknowledgement (message bodies) before first import.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Inbox import | ✅ built | request a real JSON DYI export (Messages only); drop the ZIP; confirm rows in `correspondence/facebook-messenger/`, re-import dedupes |
| Mojibake fix | ✅ built | find a thread with emoji/accents in the real export; confirm text renders correctly in Recent data |
| Reactions | ✅ built | confirm `kind:"reaction"` rows attach to the right thread |
| Multi-shard merge | ✅ built | large thread split across message_1.json / message_2.json merges without duplication |
| Archived threads | ✅ built | messages/archived_threads/ parsed at same priority as inbox |

## Build notes (2026-06-16)

- Implemented `Import` behavior — replaces the `NotWired` stub.
- Reuses `crate::meta_encoding::fix_value` (already built in `meta_encoding.rs`).
- Reuses `crate::correspondence::Message` contract + `Vault::append_messages`.
- No new connection, no new deps (sha2/zip/serde_json/chrono already present).
- `guid` = sha256(chat_id | timestamp_ms | sender_name) — length-prefixed; documented in module.
- `me` param required for `from_me` attribution (no stable id in export).
- 11 unit tests green; `cargo check` clean.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Facebook Messenger
(L345–L351). Feasibility 🟢 high. Cross-cutting note 4 (L449): same
ZIP-import pattern as Instagram DMs / Discord / X archive / Google Chat —
the shared import-ZIP auto-detection flow serves all of them, and
Instagram DMs should be sequenced right after this provider to reuse the
parser + decoder. Export turnaround is hours-to-days (up to 14 for huge
accounts). Not time-sensitive.
