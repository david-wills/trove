# Deezer

- **id:** `deezer`
- **domains:** `media/plays/deezer/` (contract: **media-plays, ratified**)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the history endpoint; watermark cursor)
- **connection:** `deezer` — OAuth (developer app at developers.deezer.com;
  connect.deezer.com/oauth/auth.php flow). Not shared with other defs.
- **evidence:** official-docs — api.deezer.com `GET /user/me/history`
  (recent plays); Deezer support page says full-history timestamp recovery
  is still in development
- **effort / priority:** M / P2
- **needs:** none

## What it is

Music streaming service with a large, EU-centric user base (France
especially) — less common on North American Macs but a real Spotify
alternative for a meaningful slice of potential users. Unlike Tidal it has
an official, documented user-history API endpoint.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Recent plays | any plan | track, artist, album metadata per recent play | official docs (`/user/me/history`) |
| Full timestamped history | not yet available | — (Deezer says in development) | Deezer support page |
| GDPR export | any plan | shape unconfirmed as of mid-2026 | research doc |

All optional in the contract. Recent-plays polling builds history forward
from connect time; lifetime backfill has no confirmed path yet — the brief
should not promise it.

## Access & auth

- REST: `GET https://api.deezer.com/user/me/history` with OAuth
  `access_token`. OAuth flow at connect.deezer.com; the flow expects a
  server-side redirect URI — the build must verify a loopback-redirect or
  implicit-grant variant works for a desktop app (same problem every
  desktop OAuth integration solves; ConnectMethod::OAuth handles the
  loopback listener).
- Rate limits apply (unspecified in the research doc — back off on 4xx).
- No TCC, no local files. Standalone-clean (plain HTTPS).

## Vault mapping

- **Raw layer:** `media/plays/deezer/raw/` optional — API responses are
  thin; raw retention only if responses carry fields the contract drops.
- **Contract layer:** `media/plays/deezer/YYYY-MM.jsonl` per the ratified
  media-plays contract — `category:"music"`, `kind:"play"`, `title`/
  `subtitle` (track/artist), `detail` = album, `seconds:0` if the endpoint
  reports no duration, `guid` = Deezer track id + play timestamp.
- **Dedupe:** `guid` + watermark cursor in `.trove/deezer-sync.json`,
  rebuildable by scanning output files.

## Build plan

1. Module `crates/trove-core/src/deezer.rs`: `DEF` (Periodic), pull hook
   polling `/user/me/history` with a since-watermark; drop already-seen
   guids (the endpoint is recent-plays, overlap is expected).
2. `CONNECTION` (`deezer`, OAuth) + registration lines in `INTEGRATIONS`
   and `CONNECTIONS`.
3. First loop task: confirm the desktop-redirect story for Deezer OAuth
   (loopback URI acceptance) — this is the only real unknown.
4. Fixtures from the documented response shape; parser + cursor tests,
   unique temp dirs.
5. Watch item: when Deezer ships full-history timestamp recovery (their
   support page says it's planned), add the GDPR export as an Import
   extension for lifetime backfill.

## Build notes (2026-06-17)

- Deezer OAuth is non-standard: uses `app_id`/`perms` params (not `client_id`/`scope`), and the token exchange returns URL-encoded form data (or JSON with `output=json`). A custom loopback flow replaces the shared `OauthFlow` helper.
- The `/user/me/history` endpoint returns `{data:[...], total:N, next:URL}` where each item is a track object plus a `timestamp` field (Unix seconds of the play). No server-side `since` filter exists — every poll fetches the recent window and guid dedupe drops already-seen plays.
- Track object fields confirmed from public API: `id`, `title`, `duration`, `isrc`, `link`, `rank`, `explicit_lyrics`, `preview`, `artist{id,name}`, `album{id,title,cover_medium}`.
- Deezer issues NO refresh token. Tokens expire (the `expires` field in seconds). On expiry the card shows reconnect.
- Port 38789 (38580 + 209 = 38789).
- CONTRACT: `reuse-bound` → `media-plays`, `category:"music"`, `kind:"play"`, `seconds:0` (play event not duration), `guid = deezer-<track_id>-<timestamp>`.
- 15 tests: all green.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Recent plays | built | OAuth connect a real Deezer account; play a track; Sync now; confirm the row in `media/plays/deezer/` + Media tab |
| Backfill | n/a | No server-side `since` filter; history only grows forward from connect time. Re-check when Deezer ships timestamped history export. |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Deezer
(L3326–L3332). Feasibility 🟡 medium — the API is official and alive; the
caveats are desktop OAuth ergonomics and the missing lifetime backfill.
Deezer apps also scrobble to Last.fm natively — users who do that are
already covered by the `lastfm` provider; say so on the card. P2 mostly on
the smaller macOS/NA user base, not on technical risk.
