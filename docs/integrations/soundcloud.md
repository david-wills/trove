# SoundCloud

- **id:** `soundcloud`
- **domains:** `media/plays/` (contract: ✅ ratified media-plays — would
  apply if a history endpoint ever ships)
- **status:** 🚫 unavailable
- **unavailable_reason:** SoundCloud's API has no listening-history
  endpoint — activities are likes/reposts, not plays — and new
  developer-app approvals are slow or blocked. Enable SoundCloud's Last.fm
  scrobbling to capture listens instead.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** official docs — developers.soundcloud.com:
  `GET /me/activities` / `/me/activities/tracks` (OAuth 2.0) return social
  actions; no "recently listened" endpoint exists. Community reports of low
  developer-app approval rates 2025–2026.
- **effort / priority:** L / P2
- **needs:** none

## What it is

Streaming platform centered on independent and user-uploaded music. Its
listens would be ordinary `media/plays` music rows — but the platform never
built a playback-history surface, so the data Trove wants does not exist in
its API.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activity stream | OAuth, approved developer app | likes, reposts, social actions (not passive plays) | official docs |
| Listening history | — | does not exist as an endpoint | official docs (absence) |

The one reachable capability (social actions) isn't a listening history and
isn't worth an uncertain developer-app approval plus OAuth plumbing.

## Access & auth

Official API exists (OAuth 2.0, app registration at soundcloud.com/you/apps)
but is doubly blocked for Trove's purpose: no history endpoint, and new app
registrations are reportedly difficult to get approved. The service has
also had financial instability — a weak foundation for compiled-in
credentials.

## Vault mapping

- **Raw layer:** none (nothing to write).
- **Contract layer:** the supported path is indirect: SoundCloud has a
  built-in Last.fm scrobbling integration — a user who enables it gets
  SoundCloud listens through Trove's Last.fm integration as normal
  `media/plays` rows, no SoundCloud code needed.

## Build plan

None. Ship the `Behavior::Unavailable` stub with the reason copy and the
"enable Last.fm scrobbling in SoundCloud settings" pointer. Revisit only if
a reliable history endpoint emerges (research doc's explicit re-entry
condition).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the greyed card with the reason copy and the Last.fm-scrobbling suggestion |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV"
§SoundCloud (L3382–L3388). Feasibility 🟠 low. `/me/activities` is a social
feed (likes/reposts), not Spotify-style recently-played. Recommendation:
icebox; the Last.fm route covers the actual user need and reinforces the
catalog's "universal aggregators first" strategy (Last.fm/Trakt as hubs).
