# Navidrome / Subsonic

- **id:** `navidrome`
- **domains:** `media/plays/` (contract: ✅ ratified media-plays)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the user's Subsonic-API server for recent
  plays; watermark cursor)
- **connection:** `navidrome` — TokenPaste-style form: server URL + username
  + password (Subsonic auth derives a salted token per request; nothing
  novel for OAuth plumbing). Not shared with other defs.
- **evidence:** official-docs — Subsonic REST API (stable, well-documented;
  `getNowPlaying`, `getAlbumList2?type=recent`, standard since v1.16.1);
  Navidrome 0.59+ keeps native scrobble history in `scrobble_data` inside
  `navidrome.db` (SQLite). OpenSubsonic extensions add richer fields.
- **effort / priority:** M / P2
- **needs:** Needs-login (validation requires a running Navidrome/Subsonic
  server — build proceeds from the documented API shapes)

## What it is

Self-hosted music servers speaking the Subsonic REST API — Navidrome is the
modern flagship; Airsonic-Advanced, Funkwhale, and Ampache speak the same
protocol, so one integration covers the family. Audience is niche
(audiophiles / self-hosters) but exactly Trove's local-first crowd, and the
play history is otherwise invisible: local-library plays never touch a
streaming service. Users who already scrobble Navidrome to Last.fm or
ListenBrainz are covered by those aggregators; this direct path serves
those who don't.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Now playing | any Subsonic server | track, artist, album, player, minutes-ago | official Subsonic API (`getNowPlaying`) |
| Recently played albums | any Subsonic server | album-level recency (`getAlbumList2?type=recent`) — coarse, no per-play timestamps | official Subsonic API |
| Full per-play history | Navidrome 0.59+ only, local DB | per-play rows in `scrobble_data` (SQLite) | research doc (community-documented schema) |

All optional in the contract: an Airsonic user gets now-playing-derived
rows only; a local Navidrome user can opt into the richer DB backfill.

## Access & auth

- Subsonic REST over the user's own server URL, e.g.
  `GET http://localhost:4533/rest/getNowPlaying?v=1.16.1&c=trove&u=USER&p=PASS`
  (use the salted-token variant, not plaintext `p=`, when supported).
  User-supplied URL means it may be a LAN/localhost address — talking to the
  user's *own* server is standalone-clean (their data, their box; same
  spirit as SimpleFIN's user-directed endpoint).
- No TCC for the API path. Direct SQLite read of `navidrome.db` (macOS
  paths: `~/.config/navidrome/navidrome.db` or
  `~/Library/Application Support/navidrome/navidrome.db`) needs no TCC for
  home-dir paths but is only valid when the server runs on this Mac —
  ship it as an optional depth toggle, API-first.
- No published rate limits (it's the user's server); poll politely
  (minutes, not seconds).

## Vault mapping

- **Raw layer:** `media/plays/navidrome/` is the only folder; API responses
  worth keeping verbatim can ride in each row's `extra` (no separate raw
  stream needed for this shape).
- **Contract layer:** `media/plays/navidrome/YYYY-MM.jsonl` per the ratified
  media-plays contract — `category:"music"`, `kind:"play"`, `title` =
  track, `subtitle` = artist, `detail` = album, `seconds: 0` when the
  Subsonic endpoint can't measure (honest unknowns per the spec), `guid` =
  song id + timestamp. Per the spec's duplicate warning: if the user also
  scrobbles this server to Last.fm/ListenBrainz, the same plays arrive
  twice — surface a hint in the def's setup copy; guids keep re-runs safe
  but cross-source dedupe is a read-time concern.
- **Dedupe/cursor:** watermark in `.trove/navidrome-sync.json`, rebuildable
  by scanning output files.

## Build plan

1. Module `crates/trove-core/src/navidrome.rs`: `DEF` (Periodic),
   `CONNECTION` (server URL + username + password fields; setup copy on the
   def), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Now-playing poll first (universal across the Subsonic family), dedupe
   repeated polls of one continuing play into one row.
4. Optional Navidrome-local depth: detect `navidrome.db` at the known
   paths; offer a one-time `scrobble_data` backfill (community-documented
   schema — verify against a real DB before trusting column names;
   parser-last if no fixture materializes).
5. Fixtures: canned Subsonic XML/JSON responses from the official docs;
   parser + store + cursor tests, unique temp dirs.
6. Validation needs a live server (Needs-login) — `brew install navidrome`
   against a small library is sufficient.

## Build notes (2026-06-17)

- Connection: NEW `navidrome` TokenPaste connection — composite `SERVER_URL|USERNAME|PASSWORD`
  pasted as one string, stored in the secret store under `navidrome`.
- Auth: Subsonic salted-token auth — `t=md5(password+salt)&s=<salt>` per the Subsonic REST API spec;
  uses the `md5` crate (pure Rust, RustCrypto). Legacy plaintext `p=` not used.
- Collector: polls `getNowPlaying` every 5 minutes; dedupes plays within a 10-minute bucket
  via guid `navidrome-<songId>-<bucket>` so repeated polls of a single play produce one row.
- `getAlbumList2?type=recent` NOT implemented — it returns album-level coarse data with no
  per-play timestamps; including it would either duplicate now-playing rows or invent fake timestamps.
  Album data is in `extra` on the now-playing rows via the song's metadata.
- Local SQLite (`navidrome.db`) backfill NOT implemented — community-documented schema with no
  confirmed sample on disk; scoped out per brief note "verify against a real DB before trusting column names".
  Flagged Needs-login for live validation.
- 12 unit tests, all pass; `cargo check` green.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Now playing → plays | ✅ built | run a local Navidrome, play a track, Sync now; confirm a row in `media/plays/navidrome/` + hub last-data |
| Recent albums | — (out of scope) | getAlbumList2 returns no per-play timestamps; omitted by design |
| Local DB backfill | — (needs-sample) | Navidrome 0.59+ with history; verify `scrobble_data` column names first |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§Navidrome / Subsonic (L3406–L3412). Feasibility 🟡 medium (niche audience,
solid tech). Architecture rhymes with Plex/Jellyfin — share patterns when
those briefs build. The Subsonic `scrobble` *write* endpoint exists (push
plays to the server) but Trove collects, it doesn't publish — out of scope.
Covers Airsonic-Advanced, Funkwhale, Ampache via the same API; name the def
copy accordingly so non-Navidrome users find it.
