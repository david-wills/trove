# Snapchat

- **id:** `snapchat`
- **domains:** `correspondence/` (✅ ratified contract — saved chats, snap
  metadata), `photos/` (Phase 3 photos-metadata contract pending — Memories),
  `social/` (Phase 3 social-posts pending; friends/account/profile categories
  stay per-source raw under `social/snapchat/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user requests the "My Data" export ZIP and drops it
  in; no API exists)
- **connection:** none (the export is requested while logged in at
  accounts.snapchat.com — no credential ever touches Trove)
- **evidence:** official My Data export (accounts.snapchat.com → My Data);
  ZIP of HTML index + JSON files; community tooling exists for processing
  the ZIP — research doc rates it 🟢 comprehensive and JSON-parseable
- **effort / priority:** S / P2
- **needs:** privacy (saved chat bodies + snap metadata + the export's
  location/search categories — opt-in with explicit acknowledgement)

## What it is

Ephemeral messaging + photo app. Ephemeral snap *content* is gone by design
— the export yields snap **metadata** only — but two assets matter:
**Memories** (photos/videos the user explicitly chose to keep — the prize)
and **Saved Chat History** (messages saved in chats). For heavy Snapchat
users this is years of otherwise-uncapturable social history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Saved Chat History | export category opt-in | saved message text, sender, timestamp | official export |
| Snap History | export category opt-in | sent/received snap metadata (no content) | official export |
| Memories | export category opt-in | saved photos/videos + metadata; media referenced by URL, downloaded separately | official export |
| Friends / Account / Login / Search / Purchase / Location / Bitmoji | export category opt-ins | per-category JSON | official export |

All optional — users pick categories at export time; absent categories just
yield no rows.

## Access & auth

- accounts.snapchat.com → My Data (or in-app Settings → My Data); select
  categories; ZIP ready in 24–48h (up to 7 days for large exports).
- Format: HTML index + JSON files. **Media files are referenced by URL in
  the JSON, not included in the ZIP** (Memories media are separate files in
  the export per the research doc) — the importer must treat media links as
  potentially expired and never require them.
- No API, no TCC, no connection. Import-only; one-time snapshots —
  re-import dedupes.

## Vault mapping

- **Raw layer:** the export's JSON files preserved under
  `social/snapchat/raw/<export-date>/` (one folder per import, full
  fidelity).
- **Contract layer:**
  - `correspondence/snapchat/YYYY-MM.jsonl` (✅ ratified) — saved chat
    messages as correspondence rows (`ts`, `source`, `guid`, handles, body);
    snap history as content-less rows (direction + counterpart + timestamp,
    media flag in `extra`).
  - `photos/snapchat/YYYY-MM.jsonl` — Memories **metadata** per the pending
    photos-metadata contract (taxonomy rule: metadata only, never image
    copies in the vault contract layer).
  - `social/snapchat/` — friends list, account info, search/login/purchase
    history as per-source raw (no posts shape here, so the social-posts
    contract is unlikely to apply).
- **Dedupe:** `guid` from message/snap IDs where present, else hash of
  (ts, counterpart, body); Memories by media ID. Repeated exports overlap —
  dedupe is mandatory.

## Build plan

1. Module `crates/trove-core/src/snapchat.rs`: `DEF` (Import; registry-driven
   import box).
2. One registration line in `INTEGRATIONS`. No connection.
3. Parser for the ZIP: walk known JSON filenames per category; tolerate
   missing categories and unknown extras (the category set is
   user-selected and Snap can add categories).
4. **Needs-sample caveat:** the per-file JSON schemas are community-known,
   not officially documented — build the importer against a real export
   early and keep field mapping defensive (unknowns → `extra`).
5. Privacy gate: opt-in with explicit acknowledgement (message bodies;
   location and search categories if present in the ZIP).
6. Fixtures: a synthetic mini-export ZIP covering chats, snap history,
   memories, friends; a re-import dedupe test.

## Implementation notes (built 2026-06-17)

- **chat_history.json** confirmed field names from community tooling (SocialStats/snapchat.py):
  flat dict `{contact: [{From, IsSender, Created(microseconds), Content, Media Type, Media IDs}]}`.
  `Created(microseconds)` is divided by 1000 to get milliseconds for the timestamp.
- **memories_history.json** confirmed field names (noelaridan/Memories-Downloader,
  Yonni123/SnapChat_Memories_Extractor): `{"Saved Media": [{Date, Media Type, Location, Media Download Url}]}`.
  `Location` is `"Latitude, Longitude: X.X, Y.Y"` or empty string.
- **snap_history.json** not parsed beyond raw layer — no message content, only send/receive
  metadata; no correspondence contract shape fits bare snap metadata without content.
- Raw layer writes `social/snapchat/raw/<YYYY-MM-DD>/` one file per JSON category.
- Photos contract reused: `photos::Photo` written to `photos/snapchat/YYYY-MM.jsonl`.
- Media files (chat_media/, memories/) are never opened — URL-only references stored in `extra`.
- parser_parked_needs_sample = false: confirmed field names from real parser code.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Saved chats + snap metadata | ✅ built | request a real My Data export; drop the ZIP; rows in `correspondence/snapchat/` + hub last-data |
| Memories metadata | ✅ built | same export with Memories selected; rows in `photos/snapchat/`; confirm no image bytes written to the contract layer |
| Re-import dedupe | ✅ built | import the same ZIP twice; row counts unchanged (tested) |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Snapchat
(L4064–L4070). Feasibility 🟢 high; effort S; recommendation was "build
later" (lower priority than text-heavy platforms) — hence P2. Memories are
the most valuable asset. The research doc's note that media is URL-referenced
(not in the ZIP) is the main fragility: an optional, clearly-labeled media
download step would be the only networked path and belongs behind its own
opt-in if ever built. Vault paths here come from the taxonomy table, not
the research entry.
