# Epic Games Store

- **id:** `epic-games`
- **domains:** `gaming/` (raw-only per the taxonomy — no contract)
- **status:** 🚫 unavailable
- **unavailable_reason:** Epic offers no personal API or playtime export.
  Epic games do appear in GOG Galaxy's local database when its Epic
  integration plugin is enabled — covered there; no standalone path
  exists.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** research-verified absence — no public personal API or
  structured export; Epic's dev.epicgames.com API is publisher-facing;
  community GraphQL reverse-engineering is fragile. Indirect coverage via
  the GOG Galaxy DB (schema via GOG-Galaxy-Export-Script on GitHub).
- **effort / priority:** M / P2
- **needs:** none (catalogued for the in-app "why isn't X available?" answer)

## What it is

PC game library and playtime from the Epic Games Store launcher. Playtime
is visible in the launcher UI ("You've Played" per game) but Epic provides
no programmatic export of it, and the privacy-portal data request returns
account history without structured playtime.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Library + playtime (indirect) | requires GOG Galaxy + Epic plugin installed | game titles, playtime minutes | GOG-Galaxy-Export-Script schema |
| Account history (data request) | free | account events, no structured playtime | Epic privacy portal, research notes |

Neither yields a standalone Epic integration; the first is the
`gog-galaxy` provider's job, the second isn't worth a parser.

## Access & auth

No personal API. No export. The one real route is reading GOG Galaxy's
local `galaxy-2.0.db` (Galaxy's plugin system aggregates Epic library +
playtime into it) — that path belongs to the `gog-galaxy` brief and
requires the user to run Galaxy with the Epic plugin. Community attempts
at Epic's private GraphQL backend are fragile and ToS-risky — rejected.

## Vault mapping

- **Raw layer:** none of its own. Epic rows arriving via GOG Galaxy land
  under `gaming/gog-galaxy/` tagged with their source platform — they are
  GOG Galaxy's records, not a separate Epic stream.
- **Contract layer:** none — `gaming/` is raw-only.

## Build plan

None. Ship as a `Behavior::Unavailable` catalog stub whose card copy
points users at the GOG Galaxy path ("install GOG Galaxy, enable its Epic
integration, then enable Trove's GOG Galaxy integration"). Revisit if
Epic ships a personal API or adds playtime to its data export.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the greyed Epic entry with the reason + GOG Galaxy pointer, sorted last within Gaming |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Epic Games
Store (L3607–L3613). Feasibility 🟠 low. The GOG Galaxy DB
(`gaming.json` catalog, `gog-galaxy` provider, L3591–L3597) is queued
separately with a Needs-sample spike on the macOS DB path — Epic coverage
is a side effect of that build, worth cross-linking in both cards' copy.
Not time-sensitive.
