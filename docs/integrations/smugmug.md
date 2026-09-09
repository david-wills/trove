# SmugMug

- **id:** `smugmug`
- **domains:** `photos/` (contract: **Phase 3 pending** — photos-metadata).
  Metadata only, never image copies.
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (API pull — the **only** path; SmugMug has no bulk
  export feature)
- **connection:** `smugmug` — OAuth 1.0a + TokenPaste for the user-supplied
  API key (no keyless access; every SmugMug subscriber can create a key).
  Not shared with other defs.
- **evidence:** official-docs — api.smugmug.com/api/v2 (documented REST
  endpoints); crates.io/crates/smugmug exists but is a low-maintenance
  wrapper — plan to hand-roll the few calls needed.
- **effort / priority:** M / P2
- **needs:** Needs-David (icebox until demand — very niche audience; build
  only when a user asks)

## What it is

Photo hosting/portfolio service used mainly by professional and serious
hobbyist photographers. Libraries carry EXIF passthrough, captions,
keywords, and geotags organized into albums. Very niche, but for its users
it is the canonical archive — and the integration pattern is near-identical
to Flickr's API path, so it's a cheap follow-on once Flickr exists.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Album list | all subscribers | `/api/v2/user/<nickname>!albums` | official-docs, research L3130–L3137 |
| Image metadata | all subscribers | `/api/v2/album/<id>!images` — EXIF passthrough, captions, keywords, geotags | official-docs |
| Size/download URLs | OAuth token needed for non-public albums | `/api/v2/image/<id>!sizedetails` (not used — metadata only) | official-docs |

All optional in the contract; images without geotags or keywords simply
carry no such fields.

## Access & auth

- REST: `api.smugmug.com/api/v2/` — user!albums → album!images walk.
- Auth: OAuth 1.0a; **API key is BYO** (user creates one in their SmugMug
  account — no keyless access, nothing to bake in). Connect card: TokenPaste
  for the key + OAuth 1.0a dance for the access token; per the disabled-
  controls rule, the gated Connect carries an inline hint pointing at where
  to create the key.
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `photos/smugmug/raw/YYYY-MM.jsonl` (partitioned by capture
  date) — API image objects full-fidelity, album context preserved.
- **Contract layer:** `photos/smugmug/YYYY-MM.jsonl` per the pending
  photos-metadata contract — `ts` = capture date, `guid` = SmugMug image id,
  caption/title, keywords as tags, geo fields, album + EXIF overflow in
  `extra`.
- **Dedupe:** image id as `guid`; sync cursor in `.trove/smugmug-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. **Iceboxed — do not start without demand (Needs-David).** When promoted:
2. Module `crates/trove-core/src/smugmug.rs`: `DEF` (Periodic, daily-ish),
   `CONNECTION` (OAuth 1.0a 3-leg + TokenPaste key), pull hook for Sync-now.
3. Reuse the OAuth 1.0a signing built for Flickr's API upgrade (sequence
   after Flickr if both get built; share the oauth1 helper in trove-core).
4. Fixtures from api.smugmug.com documented response shapes; parser + store
   + cursor tests, unique temp dirs.
5. Contract rows once photos-metadata is ratified; raw rows can land first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Album walk + image metadata | — | real subscriber pastes their key, completes OAuth, Sync now; confirm rows in `photos/smugmug/` + hub last-data |
| Geotags/keywords | — | confirm a geotagged, keyworded image round-trips into contract fields |
| Incremental sync | — | second Sync now adds only new images (cursor honored) |

## Research notes

`integrations-research.md` → "Photos & Visual Media" §SmugMug (L3130–L3137),
🟡 medium, recommendation: icebox. No bulk export exists — unlike Flickr,
the API is the only path, which is why this is Periodic rather than Import.
OAuth 1.0a is old but supported. Video items use the same URL structure
(irrelevant for metadata-only). Pattern is intentionally Flickr-shaped;
treat the two as one mini-family in the build order.
