# Pandora

- **id:** `pandora`
- **domains:** `media/plays/` (contract: ✅ ratified media-plays — moot; no
  per-track history exists to map)
- **status:** 🚫 unavailable
- **unavailable_reason:** Pandora's developer API is closed to new
  applicants and there is no official data export. As a radio-style service
  it keeps thumbed stations, not per-track listening history, so even a
  privacy request yields little.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** research doc front-matter hard block — developer portal
  explicitly not accepting new API requests; no documented export; privacy
  request (privacy@pandora.com) has no documented format.
- **effort / priority:** L / P2
- **needs:** none

## What it is

Radio-style music streaming (stations seeded from artists/songs, tuned by
thumbs). Doubly blocked for Trove: access is closed (API shut to new
applicants, no export), and the data model itself doesn't contain what
Trove wants — there is no "you listened to X at Y time" history, only
thumbed stations.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Per-track listening history | — | does not exist in Pandora's data model | research doc |
| Station thumbs / ratings | visible in-app only | not exportable officially; Soundiiz (paid third-party) can export playlists/thumbs to CSV | research doc |

## Access & auth

No public API (developer access closed to new applicants), no official data
download, undocumented privacy-request path. The one third-party escape
hatch (Soundiiz) is a paid external service — not something Trove can
depend on (standalone rule) and the data it yields (station thumbs) is low
precision anyway.

## Vault mapping

- **Raw layer:** none (nothing to write).
- **Contract layer:** none. Unlike SoundCloud there is no scrobbling
  side-door to recommend; the underlying per-play data simply isn't kept.

## Build plan

None. Ship the `Behavior::Unavailable` stub with the reason copy. No
re-entry condition worth monitoring — the research doc's recommendation is
an unqualified skip (closed API, no export, radio model, shrinking
relevance post-SiriusXM acquisition).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the greyed card with the reason copy |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Pandora
(L3390–L3396). Feasibility 🔴 blocked (one of the front-matter hard blocks,
alongside Pocket/Omnivore/Skype: "services dead or API-closed"). Users who
want their thumbs out can run Soundiiz themselves; the resulting CSV could
in principle reach the generic CSV importer, but it isn't plays data and
gets no first-class support.
