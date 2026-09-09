# Google Chat

- **id:** `google-chat`
- **domains:** `correspondence/` (contract: **correspondence — ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Google Takeout ZIP; no live pull exists for
  personal history)
- **connection:** none — Takeout is user-initiated in the browser. The
  existing `google` OAuth connection does **not** help here: the Chat REST
  API is bot/Workspace-tenant-oriented, not a personal-history read.
- **evidence:** official Takeout mechanism, stable JSON per space/DM
  (research feasibility 🟢 high); exact field set confirmed from a real
  Takeout during the build
- **effort / priority:** M / P1
- **needs:** privacy-sensitive (message bodies — explicit opt-in
  acknowledgement at import time)

## What it is

Google's team-chat product (successor to Hangouts), ubiquitous in Google
Workspace orgs and present on every personal Google account. Years of work
DMs and space history — plus, for long-time users, legacy Hangouts history
— sit behind one Takeout checkbox.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Space/group messages | all accounts | text, sender, timestamp, attachments-metadata | official Takeout format |
| Direct messages | all accounts | same fields, `DMs/<conversation>/` folder | official Takeout format |
| Legacy Hangouts history | accounts that used Hangouts | Hangouts.json conversations | official Takeout format |

All optional in the contract; a DM-only export imports fine.

## Access & auth

- Google Takeout → Chat → download. ZIP layout:
  `Takeout/Google Chat/Groups/<Space>/` per space with `group_info.json` +
  `Messages/*.json`; DMs under `Takeout/Google Chat/DMs/<conversation>/`.
- Legacy: `Takeout/Hangouts/Hangouts.json` — parse in the same collector.
- Trove side: no auth, no network, no TCC. Pure ZIP/folder import.
- Standalone-clean. No live path: the Chat API
  (developers.google.com/workspace/chat) scopes are designed for app bots
  in Workspace tenants — recorded so nobody re-litigates it in Phase 4.

## Vault mapping

- **Raw layer:** none beyond the contract rows — the Takeout ZIP stays
  with the user; group_info.json metadata that has no contract field rides
  in `extra`.
- **Contract layer:** `correspondence/google-chat/YYYY-MM.jsonl` per the
  ratified correspondence contract — `kind:"message"`, `chat` = space/DM
  conversation id, `chat_name` from group_info.json, `sender` /
  `sender_name`, `text`, attachments as metadata only, `service` = the
  Google account address when the export states it. Hangouts rows write
  the same sink with `extra.legacy:"hangouts"`.
- **Dedupe:** message id from the Takeout JSON as `guid`; if a given
  export variant lacks ids, fall back to a stable hash of
  (chat, ts, sender, text) — decided against the first real fixture.

## Build plan

1. Module `crates/trove-core/src/google_chat.rs`: `DEF` (Import),
   format auto-detection for the `Google Chat/` and `Hangouts/` subtrees
   so a whole-Takeout drop routes correctly.
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Fixtures from a real Takeout (one space, one DM, one Hangouts.json
   slice) — confirm the message-id/guid choice there; parser + dedupe +
   store tests, unique temp dirs.
4. Import box shows the privacy acknowledgement (message bodies) before
   the first run.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Spaces + DMs | 🧪 unit-tested (synthetic fixture) | request Takeout → Chat on a real account; drop the ZIP; confirm rows in `correspondence/google-chat/`, chat names from group_info.json, dedupe on re-import |
| Legacy Hangouts | 🧪 unit-tested (synthetic fixture) | needs an account old enough to have Hangouts.json in its Takeout (any long-time Google user) |

## Build notes (2026-06-16)

- `google_chat.rs`: full `Behavior::Import` implementation replacing the NotWired stub.
- Two parser paths: Chat-native (`Messages/*.json` + `group_info.json`) and legacy Hangouts (`Hangouts.json`).
- **Parser parked**: exact field names for both formats confirmed against well-known community descriptions and the brief's research notes; NO real Takeout sample on disk. Fixtures are synthetic. A real Takeout drop should be validated against the parser when available — `Needs-sample` flag set.
- guid strategy: `message_id` for Chat-native, `{conv_id}/{event_id}` for Hangouts; stable hash fallback for missing ids.
- Hangouts `created_date` format (human-readable "Monday, DD Mon YYYY, HH:MM:SS UTC") and RFC3339 both handled.
- Hangouts microsecond timestamps (`i64` decimal string) → UTC.
- `from_me` driven by optional `me` param (the user's email address).
- Hangouts rows carry `service:"hangouts-legacy"` to distinguish them at query time.
- No new dependency, no new ConnectionDef (Import only; no network).
- 5 unit tests, all green.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Google Chat
(L321–L327). Feasibility 🟢 high. Same import pattern as the Slack/Discord
exports — the shared "import ZIP with auto-detection" flow (cross-cutting
note 4, L449) serves it. Not time-sensitive. Connection-sharing note: the
provider is Google but this entry needs no login; it stays a distinct
service entry per the combine-by-provider rule (Gmail ≠ Chat).
