# Simkl

- **id:** `simkl`
- **domains:** `media/plays/simkl/` (contract: **media-plays, ratified**) ·
  `media/simkl/` (ratings/watchlist — per-source raw, per the
  media-curation routing rule)
- **status:** 🧪 built (fixture-tested; OAuth PKCE; needs a Simkl app + account to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (poll watch history; watermark cursor)
- **connection:** `simkl` — OAuth (2.0 PKCE, free developer registration at
  simkl.com/settings/developer; compiled-in client_id). Not shared with
  other defs.
- **evidence:** official-docs — api.simkl.org (`GET
  /sync/all-items/watched`, PKCE flow documented). Note: the old Apiary
  docs are frozen (2026-05-22) and sunset Oct 2026 — build against
  api.simkl.org only.
- **effort / priority:** S / P1
- **needs:** none

## What it is

TV/film/anime watch tracker and universal scrobble hub — the Trakt
alternative, with notably better anime ID mapping (AniDB/AniList). Users
scrobble to it from Plex, Kodi, VLC, Emby, and browser extensions, so one
integration absorbs watch data from many services. Growing user base.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Watch history | free tier | movies/shows/anime with `watched_at` timestamps | official docs (`/sync/all-items/watched`) |
| Ratings | free tier | per-title ratings | official docs |
| Watchlist | free tier | plan-to-watch items | official docs |

All optional in the contract; cross-service IDs on items enable read-time
joins with Trakt/Letterboxd/IMDb data.

## Access & auth

- REST at `api.simkl.org`; OAuth 2.0 PKCE — desktop-app-safe with a
  compiled-in client_id (ConnectMethod::OAuth, loopback redirect).
- Free developer app registration; free user tier covers full history.
- No TCC, no local files. Standalone-clean (plain HTTPS).
- **Time-sensitive:** Apiary docs sunset Oct 2026; any URL or shape taken
  from Apiary must be re-checked against api.simkl.org at build time
  (Phase 4's live-docs verification covers this).

## Vault mapping

- **Raw layer:** `media/simkl/` — ratings + watchlist snapshots (dated
  JSONL); they are curation, not plays, and never enter the contract
  stream.
- **Contract layer:** `media/plays/simkl/YYYY-MM.jsonl` per the ratified
  media-plays contract — `ts` = `watched_at`, `category:"video"`,
  `kind:"play"`, `title` (episode/film), `subtitle` (show title /
  director-or-show grouping key), `seconds:0` (Simkl records the fact of a
  watch, not duration — honest unknowns beat invented numbers), `guid` =
  Simkl item id + `watched_at`. Cross-service IDs (IMDB/TMDB/anime IDs)
  ride in `extra`.
- **Dedupe:** `guid`; watermark cursor in `.trove/simkl-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/simkl.rs`: `DEF` (Periodic), `CONNECTION`
   (`simkl`, OAuth PKCE), pull hook for Sync-now; registration lines in
   `INTEGRATIONS` + `CONNECTIONS`.
2. **Sequence immediately after Trakt** — same shape (PKCE TV/film
   aggregator), low marginal effort; reuse the watch-history mapping and
   test patterns wholesale.
3. Fixtures from api.simkl.org documented responses (movie, show-episode,
   and anime variants — anime exercises the ID-mapping `extra` fields);
   parser + cursor tests, unique temp dirs.
4. Verify every endpoint against api.simkl.org, not Apiary (sunset).

## Build status — 🧪 2026-06-14

Shipped (`simkl.rs`, INDEX #12 — a *later* collector in the already-bound
`media-plays` domain, so no binding). `Behavior::Periodic` (hourly).
**"trakt.rs for Simkl."**

Auth (`CONNECTION` = `simkl`, OAuth, single-login): Simkl **public client +
PKCE** (no client secret), `simkl.com/oauth/authorize` +
`api.simkl.org/oauth/token`, redirect port **38578**, client_id from
`TROVE_SIMKL_CLIENT_ID` → empty baked default (BYO). Simkl tokens **never
expire** (no refresh). Every API call carries `simkl-api-key: <client_id>` +
`Authorization: Bearer`.

Pull: `GET /sync/all-items/?extended=full&episode_watched_at=yes&date_from=<watermark>`
→ `{movies, shows, anime}` → `MediaItem` (`category:"video"`, `kind:"play"`,
`seconds:0` — Simkl records the fact of a watch, not duration). Movies at the
wrapper `last_watched_at`; **shows AND anime both nest media under `"show"`**
with per-episode `seasons[].episodes[].watched_at` → one row per watched
episode; a series with only `last_watched_at` → one row per show. `ts` = watched
time → local; `title`/`subtitle` = episode title-or-SxxExx / show title; **anime
mal/anidb ids** (Simkl's strength) + cross-service ids in `extra`. `guid` =
`simkl-<showid|movieid>-[SxxExx-]<watched_at>`. Watermark cursor
`.trove/simkl-sync.json` (drain-then-advance, dedup by guid); raw
`media/plays/simkl/raw/` + contract `media/plays/simkl/`, partition by local
`ts` month. Plan-to-watch (null `last_watched_at`) skipped.

Evidence: the `/sync/all-items` shape was verified against the authoritative
`SIMKL/API` `apiary.apib` + the plexytrack client (the HTML docs are JS-blocked;
Apiary sunsets Oct 2026 → built against `api.simkl.org`).

Adversarial-verify confirmed the production mapping correct vs the apib; 0
blocking + 2 minor fixed — the episode fixtures were made realistic
(`{number, watched_at}`; real episodes carry no per-episode Simkl id, so the
guid uses the `showid-SxxExx` fallback) and the guid hardened against a
null-season collision.

**Deferred:** ratings + watchlist curation snapshots (`media/simkl/`) — like
trakt's P2; the watch-history (media-plays) is the deliverable.

Gate: trove-core 538/0 (+13 simkl tests), `cargo check` clean, `schedule_doc`
regenerated (simkl Periodic), `bindings.ts` up to date. **Live OAuth →
Needs-login**; the free Simkl app registration → **Needs-David**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Watch history | 🧪 (Needs-David app + Needs-login) | register the Simkl app (below), OAuth a real account; mark something watched; Sync now; confirm the row in `media/plays/simkl/` + Media tab |
| Ratings/watchlist | 🚫 deferred | curation snapshots (`media/simkl/`) not built in v1 (like trakt's P2) — follow-up |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Simkl
(L3350–L3356). Feasibility 🟢 high. The research doc's "check if the user
base warrants both vs. one [of Trakt/Simkl]" is settled by the
built-for-anyone rule: both are catalogued; Simkl's anime strength serves
users Trakt serves poorly. Like Last.fm/Trakt it's an aggregator — one
build absorbs scrobbles from many players.
