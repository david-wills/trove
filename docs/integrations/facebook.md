# Facebook

- **id:** `facebook`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  likes/reactions/friends/search history/events stay per-source raw under
  `social/facebook/`) · `correspondence/` (the same ZIP's Messenger
  threads — owned by the **`facebook-messenger`** brief, ratified
  correspondence contract; this importer detects `messages/` and hands
  off to that parser)
- **status:** 🧪 built (fixture-tested; first-in-domain binding of `social/`; DYI JSON import + Meta-mojibake fix; needs a real export to validate)
- **unavailable_reason:** none
- **behavior:** Import (official "Download Your Information" ZIP, JSON
  format)
- **connection:** none — export is user-initiated at facebook.com/dyi.
  Graph API is not a viable path for personal data (app review, gated for
  consumer use) — deliberately skipped.
- **evidence:** official export mechanism (Settings → Your Facebook
  Information → Download Your Information / facebook.com/dyi);
  community-documented JSON structure (`posts/your_posts_1.json`,
  `friends/friends.json`, …); research feasibility 🟢 high
- **effort / priority:** S / P1
- **needs:** privacy-sensitive (the ZIP carries message bodies and other
  intimate categories — explicit opt-in acknowledgement at import time)

## What it is

The largest social network's comprehensive personal export: posts,
photos/videos metadata, comments, reactions, friends, search history,
marketplace, events, groups, ad data, location — selectable by category
and date range ("All Time" available). For long-time users this is 15–20
years of social history in well-structured JSON.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Posts | all accounts | text, timestamps, attachments metadata | export (`posts/your_posts_1.json`) |
| Comments/reactions | all accounts | per-section JSON | official export categories |
| Friends | all accounts | friend list + timestamps | export (`friends/friends.json`) |
| Events/groups/search history/ads | all accounts | per-section JSON | official export categories |
| Messenger threads | all accounts | full conversation history | same ZIP — handled by the `facebook-messenger` brief |

All optional in the contract; users who export only some categories
import fine.

## Access & auth

- Settings → Your Facebook Information → **Download Your Information**
  (facebook.com/dyi). Select **JSON** (import copy says so), choose
  categories + date range; ZIP delivered within hours via
  notification/email.
- Trove side: no auth, no network, no TCC. Standalone-clean.
- Same Meta **mojibake** caveat (Latin-1-decoded-as-UTF-8 strings) —
  reuse the shared decoder from `facebook_messenger.rs`.
- Media files in the ZIP are not copied into the vault (metadata only,
  consistent with the photos-domain rule).

## Vault mapping

- **Raw layer:** `social/facebook/raw/` — decoded export sections;
  likes/reactions, friends, search history, events, groups, ad data stay
  per-source raw here per the taxonomy (saved/liked items never route to
  `reading/`).
- **Contract layer (posts):** `social/facebook/YYYY-MM.jsonl` once the
  Phase 3 social-posts contract is ratified — one row per post (`ts`,
  `source`, `guid`, text, attachment metadata), overflow in `extra`.
- **Correspondence:** `messages/inbox/…` in the same ZIP routes to
  `correspondence/facebook-messenger/` via the `facebook-messenger`
  provider — one ZIP, two catalog entries, shared mechanism (the Google
  model: connection/mechanism sharing doesn't merge distinct services).
- **Dedupe:** no stable post ids in the export → `guid` = hash of
  (section, timestamp, key text), matching the Messenger approach.

## Build plan

1. Module `crates/trove-core/src/facebook.rs`: `DEF` (Import); walk the
   ZIP sections, posts → social contract rows (when ratified), everything
   else → per-source raw; detect `messages/` and invoke the
   `facebook_messenger` parser rather than duplicating it.
2. Reuse the Meta mojibake decoder built in `facebook_messenger.rs` —
   sequence this provider after it.
3. One registration line in `INTEGRATIONS`. No `CONNECTION`.
4. Fixtures: export slice with posts (plain + attachment), reactions,
   friends.json, a mojibake string, and a `messages/` stub to prove the
   hand-off; parser/routing/dedupe/store tests, unique temp dirs.
5. Privacy acknowledgement before first import.
6. Posts contract rows parked behind Phase 3 social-posts; raw layer
   ships first.

## Build status — 🧪 2026-06-15

Shipped (`facebook.rs`, INDEX #21 — also the **first-in-domain binding** of the
`social/` contract). `Behavior::Import` (the official "Download Your Information"
JSON ZIP; no auth/network), **🔒 default-off** (the ZIP carries intimate
categories — a required `acknowledge` import param). Built **Opus** (first-in-domain
contract bind, per the model policy); a Sonnet evidence spike fed it.

- **Binding (first in `social/`):** new `social.rs` `Post` + `Media` structs
  matching `social.post.schema.json` (required `ts`/`source`/`guid`; `kind`/`text`/
  `title`/`url`/`lang`/`reply_to`/`repost_of`/`quote_of`/`thread`/`context`/`tags`/
  `media`/`extra` omit-empty); `social` `DOMAINS` entry (**EventStream**, month of
  `ts`, `social/<source>/YYYY-MM.jsonl`); promoted the draft fixture to the ratified
  triad (5/5). `social.kind` was already in-schema (no change); the 🔒 flag lives on
  the DEF. **This binds the shape 11 future social sources reuse** (bluesky/mastodon/
  reddit/instagram/threads/tumblr/pinterest/tiktok/hacker-news/wikipedia/substack).
- **Posts** (`posts/your_posts_1.json`, both the bare-array and the old
  `{"status_updates":…}` shapes) → `social/facebook/YYYY-MM.jsonl` (kind="post",
  `ts`=unix→local, `text`=`data[0].post` [omitted when absent], `media[]` from
  `attachments[]`, `tags`) **AND** a full-fidelity raw copy at
  `social/facebook/raw/posts.jsonl`. FB's synthesized `title` → `extra.fb_title`
  (not `Post.title`). `guid` = **injective length-prefixed `sha256(ts,text,fb_title)`**
  (the export has no stable post ids). Other sections (friends/reactions/search/
  events/groups/ads) → `social/facebook/raw/<section>.jsonl`, generic/full-fidelity.
- **Meta mojibake:** new `meta_encoding.rs` `fix_meta_encoding` (Latin-1→UTF-8
  repair, conservative — emoji/CJK/correctly-stored accents pass through unchanged),
  applied **recursively to every parsed string**. `pub(crate)` so
  facebook-messenger #72 reuses it (it's an unbuilt stub — facebook built the
  decoder the brief assumed already existed).
- **Messenger:** `messages/inbox|archived_threads|filtered_threads|e2ee_cutover`
  detected → logged + **skipped** (deferred to facebook-messenger #72; nothing
  parsed/leaked). Media files never copied (metadata-only `uri`).

Evidence: the DYI JSON shapes + the mojibake fix confirmed (Sonnet spike + the Opus
verifier, both vs community FB-export parsers — `fb-json2table` et al.).
Adversarial-verify (Opus): **NO BLOCKING defects** + 2 minor fixed (posts now also
write a lossless raw copy so `data[]` siblings / `external_context.name` survive;
guid made injective). The binding triad, the mojibake non-corruption of legit
strings (emoji/CJK/accents), the post-text path, and the messages-skip privacy were
all independently confirmed.

Gate (my run, serial): trove-core 643/0, `cargo check` clean, spec_validation 5/5
(social ratified), `schedule_doc` regenerated (facebook Import), `bindings.ts` up to
date. **Deferred:** Messenger threads (facebook-messenger #21→#72); the `data[]`
sibling timestamps live in the raw copy (not the contract row).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Posts import | 🧪 (needs a real export) | facebook.com/dyi → request a **JSON** export (All Time); drop the ZIP; confirm rows in `social/facebook/` + the full copy in `social/facebook/raw/posts.jsonl`; re-import dedupes (0 new) |
| Raw sections | 🧪 (needs a real export) | confirm friends/reactions/search-history land under `social/facebook/raw/` |
| Mojibake | 🧪 (needs a real export) | confirm accented/emoji posts read correctly (not `Ã©`-style) in the imported rows |
| Messenger deferral | 🧪 | export WITH Messages; confirm the import logs "Messenger detected — deferred" and writes nothing under `correspondence/` (facebook-messenger #72 handles it when built) |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Facebook
(L4016–L4022); Messenger detail at §Facebook Messenger (L345–L351).
Feasibility 🟢 high. Graph API researched and rejected for personal data
— M1 import is the only viable path for general users. Same import-ZIP
pattern as Instagram/X — the shared import-box auto-detection flow serves
all of them. Not time-sensitive.
