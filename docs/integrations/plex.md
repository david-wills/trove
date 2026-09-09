# Plex

- **id:** `plex`
- **domains:** `media/plays/` (contract: ✅ ratified — watch events) with
  per-source raw at `media/plays/plex/raw/`
- **status:** 🧪 built (fixture-tested, not validated — needs a real Plex `com.plexapp.plugins.library.db` to confirm `metadata_item_views`)
- **unavailable_reason:** none
- **behavior:** Periodic (read the server's SQLite directly; works whether
  or not Plex is running)
- **connection:** none for the SQLite path. The richer local-API fallback
  needs an `X-Plex-Token` — if that path ships, a `plex` TokenPaste
  connection (token from Plex Web → Account → "Get the Plex token").
- **evidence:** community-documented schema —
  `com.plexapp.plugins.library.db`, table `metadata_item_settings`; local
  history API endpoint documented by the community
- **effort / priority:** M / P2
- **needs:** none

## What it is

Self-hosted media server: movies, TV, and music the user owns, served to
their devices. For users who run a Plex server on this Mac, its database is
a first-party record of what they watched and when — data that never
touches a streaming service's export portal. Niche audience (self-hosting
enthusiasts) but exactly Trove's local-first crowd. Plex Pass is needed for
Plex's own history dashboard but **not** for reading the raw DB.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Per-item view state (SQLite) | none (no Plex Pass needed) | view_count, last_viewed_at, view_offset per media item; joins to item title/type | community schema |
| Play history (local API) | none; needs X-Plex-Token | timestamped history rows via `/status/sessions/history/all` | community-documented endpoint |

All optional; the SQLite read alone yields last-viewed/view-count rows
(same shape limitation as Apple Podcasts — state, not a full per-play log).
The API path adds true timestamped history when the user pastes a token.

## Access & auth

- SQLite at `~/Library/Application Support/Plex Media Server/Plug-in
  Support/Databases/com.plexapp.plugins.library.db`. Key table
  `metadata_item_settings` (view_count, last_viewed_at, view_offset);
  metadata_type 1=movie, 2=show, 4=episode, 8=trailer. Read-only copy
  before querying; Plex does not need to be running.
- No TCC beyond what file access requires (path is in `~/Library/Application
  Support`, not a protected container).
- Local API: `GET http://localhost:32400/status/sessions/history/all?X-Plex-Token=…`.
- Standalone rule: Trove reads the user's own existing server — never
  requires installing Plex; the card greys when no server DB is present.
  Tautulli would be richer but requires its own running service — rejected.

## Vault mapping

- **Raw layer:** `media/plays/plex/raw/` — native rows (view-state
  snapshots and/or API history JSON), full fidelity.
- **Contract layer:** `media/plays/plex/YYYY-MM.jsonl` per the ratified
  media-plays contract: `ts` = last_viewed_at (or API row timestamp),
  kind = movie/episode/track from metadata_type, title + show/season
  detail, `view_offset`/`view_count` in `extra`.
- **Dedupe:** `guid` = hash(account, metadata item id/ratingKey,
  viewed-at). View-state rows re-emit only when last_viewed_at advances
  (watermark cursor in `.trove/plex-sync.json`, rebuildable).

## Build plan

1. Module `crates/trove-core/src/plex.rs`: `DEF` (Periodic; permission/
   presence hook = DB file exists). SQLite read first — no auth, no
   running-server requirement.
2. Registration line in `INTEGRATIONS`.
3. Fixtures: synthetic `com.plexapp.plugins.library.db` with
   metadata_item_settings + metadata_items rows across media types; join +
   type-mapping + watermark tests, unique temp dirs.
4. Follow-on iteration: TokenPaste `CONNECTION` + local-API history pull
   for timestamped rows (disabled-button affordance until token pasted).
5. Share extraction architecture with Jellyfin (same audience; build
   adjacent), per the research recommendation.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| SQLite view state | ✅ built (🧪) | on a Mac running a Plex server, watch something, Sync now with Plex quit; confirm row in `media/plays/plex/` + hub last-data |
| API history | deferred | API path (X-Plex-Token TokenPaste) deferred to a follow-on iteration |

Needs a real Plex install to validate — David may not run one; any
self-hosting user's run can promote slices.

## Build notes (2026-06-17)

- **Behavior**: `Periodic` (hourly, mtime-gated on the DB file) — emits on DB change only.
- **Schema (corrected 2026-06-17)**: Primary source is `metadata_item_views` — the
  denormalised per-view table (columns: guid, metadata_type, grandparent_title,
  parent_title, parent_index, index, title, viewed_at). `metadata_item_settings` is
  LEFT-JOINed on `guid` (NOT on a metadata_item_id FK — that column does not exist;
  the real schema keys mis by guid). `metadata_items` is LEFT-JOINed on guid for year
  and item_id. Types emitted: 1=movie, 4=episode, 10=track.
- **Metadata type mapping**: verified against python-plexapi `SEARCHTYPES` dict. The brief
  erroneously listed 8=trailer; 8=artist in the actual Plex schema. Brief corrected here.
- **viewed_at**: Unix epoch seconds in `metadata_item_views` (not Apple epoch). `view_offset`
  from `metadata_item_settings` in milliseconds.
- **Watermark**: per-guid `viewed_at` cursor in `.trove/plex-sync.json`; a new row emits only
  when the timestamp strictly advances (rewatches produce a new `metadata_item_views` row with
  advancing viewed_at, which produces a new contract row).
- **show_title / season**: resolved directly from `metadata_item_views.grandparent_title`
  (show/artist) and `parent_index` (season number). No parent-chain join needed.
- **Connection**: none (local SQLite; no token needed for the DB path). The API path
  (X-Plex-Token) is deferred.
- **Tests**: 11 passing; cover read, parse (movie/episode/track/partial/empty-title-skip),
  pull cycle, idempotence, rewatch advance, absent-DB no-op, cursor back-compat.
  All pass under --test-threads=4 (parallel-safe stem with path hash).
- **Narrower than brief**: API history path (local API + X-Plex-Token TokenPaste connection)
  deferred. The SQLite path is the primary value; the API path adds true per-play timestamps
  but requires a token.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Plex
Media Server (L3286–L3292). Feasibility 🟡 medium — purely because the
audience is niche; the mechanism itself is robust. Plex Pass NOT required
for raw DB reads. Build later alongside Jellyfin/Navidrome (shared
local-media-server pattern). No time-sensitivity.
