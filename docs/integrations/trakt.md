# Trakt

- **id:** `trakt`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified**) ·
  `media/trakt/` (ratings, watchlist, collection — per-source raw, per the
  media-curation routing rule)
- **status:** 🧪 built 2026-06-14 — **history slice** (media-plays) done +
  tested; **curation slice deferred** (P2, see build notes); validation pending
  the OAuth app (see needs)
- **unavailable_reason:** none
- **behavior:** Periodic (hourly; watermark poll on `start_at=`; paginated
  backfill draining all pages before advancing the cursor)
- **connection:** `trakt` — OAuth 2.0 **PKCE** (ConnectSpec baked + BYO creds).
  **As built:** `Provider TRAKT` (auth `trakt.tv/oauth/authorize`, token
  `api.trakt.tv/oauth/token`, **redirect port 38576**, `use_pkce:true`,
  `basic_auth:false`, client_id+secret from `TROVE_TRAKT_CLIENT_ID/_SECRET`
  env → empty baked default), cloned from `sync/ticktick.rs` + token **refresh**
  via `oauth::refresh_token` (Trakt issues refresh tokens, so expiry self-heals
  — unlike ticktick). `trakt-api-key`/`trakt-api-version:2` headers on every
  call. Public-profile (no-OAuth username) fallback is **deferred** (still needs
  the app's client_id header, so it unblocks no validation). Not shared.
- **evidence:** official-docs — api.trakt.tv API v2; live docs are JS-rendered
  (unreachable via fetch), so the build used the documented v2 shape and the
  adversarial verifier **independently confirmed every field path** against the
  real `mfederowicz/trakt-sync` Go library (community schema). 100k history cap
  per the 2026 forum.
- **effort / priority:** S / P0
- **needs:** **Needs-login** for validation — register a free Trakt OAuth app
  (no keyless path: every call needs `trakt-api-key`). Build is complete +
  fixture-green; live history backfill/poll waits on the app creds + redirect
  URI (journal Needs-David queue). Time-sensitive: aggregation hub
  (Plex/Kodi/Infuse scrobble into it).

## What it is

The Last.fm of TV and movies: a watch-history aggregator that media players
(Plex, Kodi, Infuse, Emby) scrobble into automatically, plus manual
check-ins. One integration captures viewing across every player a user has
connected. Also holds ratings, watchlist, and collection. Trakt IDs map to
IMDB/TMDB/TVDB, making it the cross-reference spine for the whole video
slice (Netflix/Prime imports backfill the pre-Trakt era).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Watch history | free (capped at 100k items, free and VIP alike) | watched_at, movie/episode title, TMDB/IMDB/TVDB ids | official docs + 2026 forum |
| Ratings | free | item, rating, rated_at | official docs |
| Watchlist | free (VIP $3/mo for >250 items) | item, listed_at | official docs |
| Collection | free | item, collected_at | official docs |

All optional in the contract; history is the only stream that joins
media-plays — curation stays raw. No tier-specific code paths.

## Access & auth

- REST API v2: `GET https://api.trakt.tv/sync/history?type=movies|shows
  &start_at=ISO&end_at=ISO` (OAuth) or `GET /users/{username}/history/…`
  (public profiles, no user auth). `client_id` header always required.
- OAuth 2.0 PKCE — app-distribution-safe with compiled-in credentials
  (free registration at trakt.tv/oauth/applications/new).
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `media/plays/trakt/raw/YYYY-MM.jsonl` — full API history
  objects; `media/trakt/` snapshots for ratings/watchlist/collection.
- **Contract layer:** `media/plays/trakt/YYYY-MM.jsonl` per media-plays:
  `ts` = watched_at, `category:"video"`, `kind:"play"`, `title` = episode
  or film title, `subtitle` = show title for episodes / "title (year)" for
  movies (Trakt doesn't supply director; `subtitle` is the chart grouping
  key and must group a film's rewatches together — the real joining is via
  ids in `extra`), `seconds: 0` (Trakt records events, not durations),
  TMDB/IMDB/TVDB ids + rewatch ordinal in `extra`.
- **Dedupe:** `guid` = Trakt history-entry `id` (stable, unique per play).
  Watermark cursor in `.trove/trakt-sync.json`, rebuildable.

## Build plan

1. Module `crates/trove-core/src/trakt.rs`: `DEF` (Periodic, ~hourly),
   `CONNECTION` (OAuth PKCE per the ConnectSpec pattern; `sync/ticktick.rs`
   is the reference connection), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Backfill via `start_at`-paginated history, then incremental from the
   newest `watched_at` watermark.
4. Ratings/watchlist/collection: snapshot pulls into `media/trakt/`,
   replace-on-sync (curation, not events).
5. Fixtures from the documented JSON response shapes (movie AND episode
   variants); parser + store + cursor tests, unique temp dirs.
6. Subtitle decision above (movies vs episodes) is a contract-fit detail —
   settle it in review, not ad hoc per source. Simkl reuses this design.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| History backfill + poll | 🧪 | register the OAuth app + `export TROVE_TRAKT_CLIENT_ID/_SECRET` (journal); Connect → Trakt (browser consent at localhost:38576); toggle on + Sync now; confirm `~/Trove/media/plays/trakt/YYYY-MM.jsonl` (+ `raw/`) rows match the trakt.tv history page (movies AND episodes), re-run produces no dupes; let the token age past expiry and confirm it refreshes (no reconnect prompt) |
| Curation snapshots | ⬜ deferred (P2) | NOT built this iteration — `// TODO(trakt P2)` in `trakt.rs` lists the exact endpoints (`/sync/ratings`, `/sync/watchlist`, `/sync/collection/{movies,shows}`) → `media/trakt/{ratings,watchlist,collection}.jsonl` replace-on-sync (raw, no contract). A small follow-up; pick up any iteration |
| Public-profile fallback | ⬜ deferred | OAuth is the default path; the no-OAuth username path still needs the app's client_id header so it unblocks no validation — additive enhancement |

## Build notes (as-built, 2026-06-14)

- **First OAuth provider built by the loop.** Cloned `sync/ticktick.rs` (Provider +
  ConnectionDef + connect/status/disconnect, token in the sync-token store) and added
  PKCE + **token refresh** (`oauth::refresh_token`, modeled on `sync/google.rs::fresh_token`
  but single-account). No change to shared `oauth.rs` — its form-POST + PKCE + refresh
  handle Trakt as-is (Trakt's token endpoint accepts form-encoding).
- **History → media-plays** (`media/plays/trakt/`): clone of the lastfm pull idioms
  (watermark cursor `.trove/trakt-sync.json`, raw + contract layers, on-disk-guid dedup,
  multi-page drain before advancing the watermark). `ts`←`watched_at` (UTC→local offset),
  `category`="video", `kind`="play", `seconds`=0, `guid`=`trakt-<history id>` (unique per
  play → rewatches kept as distinct rows). Movie: title=movie title, subtitle=`"Title (Year)"`;
  episode: title=episode title, subtitle=show title, detail=`S{season:02}E{number:02}`.
  ids in `extra` (movie ids from `movie.ids`; episode ids from `episode.ids` + `show_*` from
  `show.ids` — verified not swapped). **Watermark uses the raw UTC `watched_at`** for `start_at`
  (not the local `ts`); partition uses the local `ts` month per the contract's "month of `ts`"
  (consistent with lastfm/listenbrainz). No write-time "rewatch ordinal" (read-time derivation).
- **Adversarial verify:** 0 blocking; the 1 "minor" raised (partition by local vs UTC month)
  is a non-defect — the media-plays contract specifies "month of `ts`" (local), which is what
  all three media collectors do.
- **media-plays already Rust-bound** → no struct/`DOMAINS`/`spec_validation` change; `video` is
  just a category value.

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Trakt.tv
(L3206–L3212). Feasibility 🟢 high; "build now — the Last.fm equivalent for
TV and movies". 100k-item cap is plenty for personal use. VIP needed only
for >250-item watchlists, not history. The unavailable streaming services
(Disney+/Hulu/Max, TV Time) all point at Trakt scrobbling as their
recommended capture path — this brief is their dependency.
