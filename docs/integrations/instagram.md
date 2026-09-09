# Instagram

- **id:** `instagram`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  likes/followers/ad data stay per-source raw under `social/instagram/`) ·
  `correspondence/` (DMs — contract: **correspondence — ratified**) ·
  `photos/` (photo metadata — contract: **Phase 3 pending** —
  photos-metadata; metadata only, never image copies)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Meta Accounts Center "Export your information"
  ZIP, JSON format)
- **connection:** none — export is user-initiated. No API for personal
  accounts (Basic Display API EOL Dec 2024; Graph API is Business/Creator
  + app review only) — deliberately skipped.
- **evidence:** official export mechanism (Accounts Center → Export your
  information → JSON); well-structured, community-documented layout
  (`content/posts_1.json`, `messages/inbox/…`, same schema as Messenger);
  research feasibility 🟢 high
- **effort / priority:** S / P1
- **needs:** privacy-sensitive (DM message bodies — explicit opt-in
  acknowledgement at import time)

## What it is

Meta's photo/video network. The official JSON export is one ZIP covering
posts (captions, timestamps, location tags), stories, reels, DMs,
followers/following, comments, likes, and ad interactions — for many
users the richest personal photo-plus-conversation archive they have.
The same ZIP also bundles **Threads** data, so one importer serves both
catalog entries.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Posts/reels | all accounts | caption, timestamp, location tags, media refs | official export (`content/posts_1.json`, …) |
| Stories archive | all accounts | story media metadata + timestamps | official export |
| DMs | all accounts | sender_name, timestamp_ms, content, reactions | export (`messages/inbox/<thread>/`) — Messenger schema |
| Social graph | all accounts | followers/following | export (`connections/followers_and_following/`) |
| Likes/comments/ad data | all accounts | per-section JSON | export (`ads_information/`, …) |
| Threads posts | all accounts | `threads_and_replies.json` in the same ZIP | official export (see `threads` brief) |

All optional in the contract; sparse exports import fine.

## Access & auth

- Accounts Center → Your information and permissions → **Export your
  information** → Export to device → **JSON** (HTML variant is for human
  reading only — import copy says JSON). ZIP emailed, ~1 hour to 48
  hours; **download link expires after 4 days** (export-link expiry —
  import promptly; large accounts can take up to 14 days).
- Trove side: no auth, no network, no TCC. Standalone-clean.
- Same Meta **mojibake** caveat as Messenger (Latin-1-decoded-as-UTF-8
  strings) — reuse the shared decoder built in the `facebook-messenger`
  provider.

## Vault mapping

- **Raw layer:** `social/instagram/raw/` — decoded export sections;
  likes, followers/following, ad data stay per-source raw here.
- **Contract layer (posts):** `social/instagram/YYYY-MM.jsonl` once the
  Phase 3 social-posts contract is ratified — one row per post/reel/story
  (`ts`, `source`, `guid`, caption text, media metadata, location tags),
  overflow in `extra`. Posts route whole to `social/`.
- **Contract layer (DMs):** `correspondence/instagram/YYYY-MM.jsonl` per
  the **ratified** correspondence contract — same field mapping as the
  `facebook-messenger` brief (shared parser): `kind:"message"`/`"reaction"`,
  `chat` = thread folder, `sender_name`, `from_me` by owner name, `text`,
  `service:"Instagram"`.
- **Contract layer (photos):** `photos/instagram/` per the pending
  photos-metadata contract — photo-shaped metadata rows (timestamp,
  caption, user-added location tags). **Metadata only, never image
  copies** (taxonomy rule); the ZIP's media files are not copied into the
  vault.
- **Dedupe:** no stable ids on posts/DMs → `guid` = hash of (section,
  timestamp, key text), matching the Messenger approach.

## Build plan

1. Module `crates/trove-core/src/instagram.rs`: `DEF` (Import); walk the
   ZIP, route posts → social, DMs → correspondence, photo metadata →
   photos; parse `threads_and_replies.json` for the `threads` provider in
   the same pass (distinct def, shared mechanism).
2. Reuse the Messenger thread parser + Meta mojibake decoder from
   `facebook_messenger.rs` — sequence this provider after it.
3. One registration line per def in `INTEGRATIONS`. No `CONNECTION`.
4. Fixtures: export slice with a captioned post (location tag), a story,
   a DM thread with a reaction, a mojibake string, and a
   `threads_and_replies.json`; parser/decoder/routing/dedupe tests,
   unique temp dirs.
5. Privacy acknowledgement (DM bodies) before first import.
6. Posts/photos contract rows parked behind their Phase 3 contracts; DM
   rows ship against ratified correspondence; raw layer ships first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Posts import | ✅ built | cargo tests green; mojibake fixed; dedup by sha256(ts+caption+uri) |
| DM import | ✅ built | `from_me` keyed on optional `owner_name` param; reactions as `kind:"reaction"` rows; service="Instagram" |
| Photo metadata | ⚠️ raw-only | photos-metadata contract not yet ratified (Phase 3 pending); photo metadata lands in `social/instagram/raw/` |
| Threads passthrough | ✅ built | `threads_and_replies.json` → `social/threads/YYYY-MM.jsonl` (source="threads") in same pass |

## Build notes (fan-out agent — 2026-06-15)

- Replaced NotWired stub with full `Behavior::Import` implementation.
- Contract layer: posts/stories/reels → `social/instagram/` via `crate::social::Post`; DMs →
  `correspondence/instagram/` via `crate::correspondence::Message`. Photos stay raw (pending contract).
- Threads posts (`threads_and_replies.json`) route to `social/threads/` as `source="threads"` — the threads stub
  stays NotWired but the data lands in the correct vault location.
- Meta mojibake fixed via `crate::meta_encoding::fix_value` across all JSON values before parse.
- Raw layer full-fidelity for all other sections (followers/following, likes, comments, ads, …).
- No new connection or dependency; `zip` and `sha2` already in Cargo.toml.
- 10 unit tests green, `cargo check` clean.

## Research notes

`integrations-research.md` → "Email & Messaging Apps" §Instagram DMs
(L353–L359), "Photos & Visual Media" §Instagram account data export
(L3114–L3121), "Social Media & Web Presence" §Instagram (L4008–L4014).
Feasibility 🟢 high for import; API path researched and rejected for
personal accounts (no migration path since Dec 2024). Export media is
re-compressed (not original resolution) — irrelevant since Trove keeps
metadata only. Media-quality selector exists at export time.
