# Tidal

- **id:** `tidal`
- **domains:** `media/plays/tidal/` (contract: **media-plays, ratified**) ·
  `media/tidal/` (favorites/playlists — per-source raw, per the
  media-curation routing rule)
- **status:** 🧪 built (raw scaffold; parser parked pending GDPR sample)
- **unavailable_reason:** none
- **behavior:** Import (GDPR export for play history; favorites/playlists
  via API can follow as a Periodic extension)
- **connection:** `tidal` — OAuth (2.1 PKCE, developer account at
  developer.tidal.com) for favorites/playlists only; the history-export
  import path needs no login. Not shared with other defs.
- **evidence:** official-docs — developer.tidal.com (favorites/playlists
  API); community — tidal-music/discussions#10 confirms **no listening
  history endpoint** exists; GDPR export shape: sample-required
- **effort / priority:** M / P2
- **needs:** Needs-sample (GDPR export format undocumented — parser-last;
  per-play timestamps unconfirmed)

## What it is

Hi-fi music streaming service (lossless/Atmos catalog), a meaningful
Spotify alternative especially among audiophiles. Listening history is the
prize; favorites and playlists are the curation layer around it.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Play history | any plan, via GDPR export | per-play rows; timestamp granularity **unconfirmed** | sample-required |
| Favorites (tracks) | any plan, official API | liked tracks (`/v2/users/me/favorites/tracks`) | official docs |
| Playlists | any plan, official API | playlist membership | official docs |

All optional in the contract. CRITICAL: the official API has **no** history
or recent-plays endpoint as of 2026 (confirmed in tidal-music GitHub
discussions) — API-side capture of plays is impossible; don't burn an
iteration looking for it.

## Access & auth

- History: GDPR data request at tidal.com/account/privacy → export file
  (format/fields undocumented; needs a real sample).
- Favorites/playlists: REST at developer.tidal.com, OAuth 2.1 PKCE —
  desktop-app-safe with a compiled-in client_id (ConnectMethod::OAuth).
- No TCC, no local files. Standalone-clean. For *ongoing* play capture the
  honest answer is Tidal's built-in Last.fm scrobbling → the `lastfm`
  provider; the brief's app copy should say so.

## Vault mapping

- **Raw layer:** `media/tidal/raw/` — export files as received; API
  favorites/playlist snapshots as dated JSONL.
- **Contract layer:** `media/plays/tidal/YYYY-MM.jsonl` per the ratified
  media-plays contract — `category:"music"`, `kind:"play"`, `title`/
  `subtitle` (track/artist), `seconds` (`0` if the export lacks duration),
  `guid` from the export's row identity (synthesize `ts+title+artist` hash
  if it carries no id). Favorites/playlists stay in `media/tidal/` — they
  are curation, not plays, and never enter the contract stream.
- **Dedupe:** `guid` makes the import re-runnable.

## Build plan

1. **Spike first** (the research doc's own recommendation): obtain a real
   GDPR export and confirm per-play timestamps exist. **Parser-last** —
   no history parser is written until a sample lands (Needs-sample).
2. Module `crates/trove-core/src/tidal.rs`: `DEF` (Import for the export;
   the registry import box is free), `CONNECTION` (OAuth PKCE) only when
   the favorites/playlists extension is built — it can ship later without
   blocking the import.
3. Fixtures from the sample export; parser + store tests, unique temp dirs.
4. UI copy on the card: recommend enabling Tidal's built-in Last.fm
   scrobbling for live capture (pairs with the `lastfm` provider).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Play history import | 🟡 scaffold/parked | request GDPR export from a real Tidal account; drop into the import box; confirm file lands in `media/tidal/raw/`; parser parked until format confirmed |
| Favorites/playlists | — | OAuth connect (not yet built); Sync now; confirm snapshots in `media/tidal/` |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Tidal
(L3318–L3324). Feasibility 🟡 medium — solely because history is
export-only. The developer portal and PKCE flow are legitimate and
functional for catalog/favorites. The research doc's vault paths predate
the taxonomy; paths above follow the taxonomy table. Alternative for live
plays: Last.fm scrobbling (built into the Tidal desktop app).
