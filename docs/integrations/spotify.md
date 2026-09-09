# Spotify

- **id:** `spotify`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified**) ·
  `media/spotify/` (saved tracks / playlists, if the live path is connected
  — per-source raw, per the media-curation routing rule)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (GDPR Extended Streaming History — the primary,
  zero-auth path) + optional Periodic live arm via OAuth
- **connection:** none for the import path. Optional `spotify` — OAuth
  (Authorization Code + PKCE) for the live recently-played arm. Feb 2026
  policy: dev-mode apps require a Premium account and cap at 5 test users,
  so a compiled-in client_id cannot serve a distributed audience — the live
  arm ships **BYO developer app** (ConnectSpec supports BYO creds), clearly
  labeled as the power-user option.
- **evidence:** official-docs — GDPR export delivers documented
  `Streaming_History*.json`; official API reference for
  `/me/player/recently-played` (survived the Feb 2026 endpoint removals)
- **effort / priority:** M / P1
- **needs:** none

## What it is

The dominant music streaming service. Its GDPR "Extended Streaming History"
export is the gold standard of listening data: complete lifetime history
with `ms_played` per play — richer than any scrobbler (enables skip
detection). The live API adds ongoing capture between exports, but Feb 2026
developer-program changes make it a constrained, opt-in extra rather than
the main path. Users who scrobble Spotify to Last.fm are already covered
for ongoing capture; the export remains uniquely valuable for `ms_played`
and pre-scrobbling history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Lifetime play history (export) | any account; 1–5 day wait | ts, track, artist, album, ms_played, platform | official GDPR export |
| Recently played (live API) | BYO dev app + Premium (dev-mode rule) | ~50-item rolling window, cursor-paginated, ~1275-item max depth | official API docs |
| Saved tracks / playlists (live API) | same BYO gating | library snapshots | official API docs |
| Currently playing (live API) | same | now-playing state | official API docs |

All optional in the contract; an export-only user simply has no live rows.
No tier-specific code paths.

## Access & auth

- Export: Spotify Account → Privacy settings → Download your data → check
  **Extended Streaming History**; ZIP arrives by email in 1–5 days with
  multi-file `Streaming_History_Audio_*.json` (lifetime, `ms_played`, `ts`).
- Live API: `GET /me/player/recently-played` (scope
  `user-read-recently-played`), cursor-paginated, caps ~1275 items — never
  sufficient for backfill; export is the backfill. OAuth 2.0 PKCE against
  the user's own developer app (they register at developer.spotify.com,
  paste client_id; requires Premium per Feb 2026 dev-mode rules).
- No TCC, no local files. Standalone-clean (HTTPS only, and only when the
  user explicitly connects the live arm).

## Vault mapping

- **Raw layer:** `media/plays/spotify/raw/` — imported export JSON
  preserved month-partitioned; live API objects appended likewise.
  `media/spotify/` for library/playlist snapshots.
- **Contract layer:** `media/plays/spotify/YYYY-MM.jsonl` per media-plays:
  `ts`, `category:"music"`, `kind` = `"play"` when `ms_played` ≥ 30s else
  `"partial"` (the research doc's skip threshold), `title` = track,
  `subtitle` = artist, `detail` = album, `seconds` = ms_played/1000,
  `device` = platform string, episode/podcast rows map with
  `category:"podcast"`. Skip/shuffle/reason fields in `extra`.
- **Dedupe:** `guid` = hash of (ts, track_uri, ms_played) for export rows
  (the export has no play id); live rows use the cursor timestamp + track
  uri. Export re-imports are idempotent; export/live overlap windows dedupe
  on the same guid scheme.

## Build plan

1. Module `crates/trove-core/src/spotify.rs`: `DEF` (Import; registry-driven
   import box accepts the ZIP or loose JSON), parser for
   `Streaming_History_Audio_*.json` (audio + video/podcast variants).
2. Registration line in `INTEGRATIONS`. `letterboxd.rs` is the reference
   import module.
3. Phase-2 of the module (may ship later): optional live arm — `CONNECTION`
   (OAuth, BYO-creds flow with explicit setup copy for registering a dev
   app; disabled-with-hint until creds are pasted, per the SimpleFIN
   affordance rule), Periodic recently-played poll.
4. Fixtures: documented export shapes (incl. podcast episode rows and null
   track fields); parser + store tests, unique temp dirs.
5. UI copy must set expectations: export takes days to arrive; suggest
   Last.fm scrobbling for ongoing capture without the BYO-app hassle.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export import | — | request a real Extended Streaming History; drop the ZIP on the import box; counts match Spotify Wrapped-era totals; re-import produces no dupes |
| Live recently-played | — | register a personal dev app, paste creds, OAuth; play a track; confirm a row within a poll cycle |
| Skip detection | — | confirm <30s plays land as `kind:"partial"` in sample months |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Spotify
(L3214–L3220). 🟢 high for the export path, 🟡 medium for live API. The Feb
2026 removal of 15 endpoints did NOT touch recently-played / saved tracks /
profile, but the Premium-only dev-mode + 5-user cap reshapes distribution:
baked credentials are off the table, hence BYO. Time-insensitive: the
export is lifetime-complete whenever requested.
