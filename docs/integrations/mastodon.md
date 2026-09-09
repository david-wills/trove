# Mastodon

- **id:** `mastodon`
- **domains:** `social/` (contract: **Phase 3 pending** — social-posts;
  likes/bookmarks/follow CSVs stay per-source raw under `social/mastodon/`)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (REST API poll, `max_id` cursor) + archive ZIP
  Import for full-history backfill
- **connection:** `mastodon` — OAuth (against the **user's instance**;
  instance URL is required config on the connect card; scopes
  `read:statuses`, `read:favourites`, `read:accounts`). Not shared with
  other defs.
- **evidence:** official-docs — docs.joinmastodon.org (REST API) +
  built-in archive export (ActivityStreams 2.0 JSON-LD); research
  feasibility 🟢 high
- **effort / priority:** M / P2
- **needs:** none

## What it is

The flagship Fediverse/ActivityPub microblogging network — thousands of
independent instances, one open API. Completely open: no cost, no
approval gate, strong data portability by design. The archive export
gives full history; the API gives incremental freshness — pair both.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full post history (incl. boosts, media) | none | `outbox.json` — ActivityStreams JSON-LD | official archive export |
| Likes/bookmarks | none | `likes.json`, `bookmarks.json` | official archive export |
| Following/blocks/mutes | none | CSVs in archive (followers list **not** exported) | official docs |
| Incremental posts/favourites | none | `/api/v1/statuses`, `/api/v1/favourites` | docs.joinmastodon.org |

All optional in the contract; the followers-list gap is an honest
limitation, not a code path.

## Access & auth

- Archive: Settings → Import and Export → Request archive — ZIP with
  `actor.json`, `outbox.json` (all posts), `bookmarks.json`, `likes.json`,
  `media_attachments/`. Requestable every **7 days** (so archive alone is
  never fresh — hence the API pairing).
- API: OAuth 2 user token against the user-supplied instance URL;
  documented at docs.joinmastodon.org; poll with a `max_id` cursor.
- ActivityPub JSON-LD parses fine with `serde_json` (per research). Plain
  HTTPS, no TCC, standalone-clean. No baked credential problem — Mastodon
  apps register against the user's own instance.

## Vault mapping

- **Raw layer:** `social/mastodon/raw/` — archive JSON-LD + API status
  objects, full fidelity; likes/bookmarks/follow CSVs stay here per the
  taxonomy (saved posts never route to `reading/`).
- **Contract layer:** `social/mastodon/YYYY-MM.jsonl` once the Phase 3
  social-posts contract is ratified — one row per post/boost (`ts`,
  `source`, `guid` = status URI/id, text, visibility, boost ref, media
  metadata), overflow in `extra`.
- **Dedupe:** status id/URI as `guid` — archive rows and API rows dedupe
  against each other naturally; API cursor in `.trove/mastodon-sync.json`,
  rebuildable.

## Build plan

1. Module `crates/trove-core/src/mastodon.rs`: archive Import def +
   Periodic API def in one module (the Google many-defs-one-module
   pattern), `CONNECTION` (OAuth; instance-URL field on the connect card —
   per the disabled-controls rule, the connect button hints when the URL
   is missing).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures: archive slice (outbox with a boost + media post + likes) and
   API status JSON; parser, cursor, cross-source dedupe, store tests,
   unique temp dirs.
4. Contract rows parked behind the Phase 3 social-posts contract; raw
   layer can ship first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Archive import | ✅ unit-tested | request a real archive on any instance; drop the ZIP; confirm rows in `social/mastodon/`, re-import dedupes |
| API incremental | 🔒 Needs-login | Connect with instance_url\|token; post; Sync now; confirm new row + hub last-data |
| Cross-path dedupe | ✅ unit-tested | guid = status URI; archive and API rows match on the same key |

## Build notes (2026-06-17)

- Auth: TokenPaste `https://instance|token` (Mastodon requires dynamic per-instance app
  registration, incompatible with the static `Provider` OAuth model; personal access tokens
  generated in Settings → Development are the practical equivalent).
- Periodic DEF (`mastodon`) + Import DEF (`mastodon-archive`) in one module; both registered.
- Contract: `social.Post` reused; guid = status URI (stable cross-path key for API↔archive dedup).
- Raw: full-fidelity status objects + archive Activities in `social/mastodon/raw/YYYY-MM.jsonl`.
- Boosts (reblogs/Announce): raw only (not authored content).
- HTML → plain text via lightweight tag-stripper (no new deps).
- 26 tests pass; cargo check green.

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Mastodon
(L3992–L3998). Feasibility 🟢 high. Mastodon v4.5.6 current as of the
research pass. 7-day archive cadence is why M1 alone is insufficient.
Followers list is not in the archive (only following/blocks/mutes CSVs).
Sibling open-protocol provider to Bluesky — sequence them together to
exercise the social-posts contract with two sources.
