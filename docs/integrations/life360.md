# Life360

- **id:** `life360`
- **domains:** `location/` (would map here if available; no folder is
  written while unavailable)
- **status:** 🚫 unavailable
- **unavailable_reason:** Life360 offers no data export and no official API.
  The only path is an unofficial reverse-engineered API that Life360
  actively blocks and whose use is against their terms.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** community reverse-engineering only
  (krconv.github.io/life360-api-docs) · no official path · low confidence
- **effort / priority:** L / P2
- **needs:** privacy (live family-location trails — would be opt-in with
  explicit acknowledgement *if* it were buildable)

## What it is

Life360 is a family location-sharing app: circles of family members sharing
continuous precise location. The data (location trails per member) would be
high-value for the location domain — but there is no honest, durable way to
collect it.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Circle members + locations | — (no official access) | member, current lat/lon — only via unofficial API | community reverse-engineering |

Not catalogued as buildable capabilities — there is no supported access
path.

## Access & auth

- No official public API. No export feature in the app — Life360 stated
  (2014, still true in 2025) there is no way to export your data.
- The only documented path is an unofficial REST API
  (`life360.com/v3`, OAuth2 password grant) reverse-engineered by the
  community (krconv.github.io/life360-api-docs). Life360 **intermittently
  blocks** third-party access via Cloudflare, and use is against their ToS.
- This violates Trove's standalone + trust posture: a fragile, ToS-grey,
  actively-blocked scrape is not a foundation to build on.

## Vault mapping

- none — nothing is written while the source is unavailable. If a supported
  path ever appears, location trails would route to `location/` (Phase 3
  trails shape) and ship privacy-gated opt-in.

## Build plan

none — catalogued unavailable. Revisit only if Life360 ships an official
export or API. The research recommendation is explicit: **skip**.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| (none) | 🚫 | unavailable — no supported access path to validate |

## Research notes

`integrations-research.md` → "Geolocation & Travel" §Life360 (L2412–L2418).
Feasibility 🟠 low. Unofficial API is fragile and Cloudflare-blocked; no
export path; Life360 has faced data-selling controversies (precise location
sold to brokers). Research note: users privacy-conscious enough to choose
Trove are an unlikely Life360 overlap — low audience value on top of the
hard access block. Catalogued so the greyed card answers "why isn't Life360
available?" in-app.
