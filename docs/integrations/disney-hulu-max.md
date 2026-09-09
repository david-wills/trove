# Disney+ / Hulu / Max

- **id:** `disney-hulu-max`
- **domains:** `media/plays/` (contract: ✅ ratified media-plays — would apply
  if a path ever opens)
- **status:** 🚫 unavailable
- **unavailable_reason:** No API and no documented export. Privacy-portal
  requests take up to 30 days, return undocumented formats that vary by
  service and region, and download links expire quickly. Scrobble to Trakt
  for ongoing capture instead.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** research doc only — OneTrust privacy portals
  (privacyportal.onetrust.com for HBO Max; disneyplus.com/privacy;
  hulu.com/privacy); user reports of variable JSON/CSV responses. No
  official docs, no community schema, no sample.
- **effort / priority:** M / P2
- **needs:** none

## What it is

The big three subscription video streamers (Disney+, Hulu, HBO Max/Max),
grouped because they share the same non-situation: large watch histories
locked behind privacy portals with no API and no documented export. Watch
history from these services would be high-value `media/plays` rows — but no
standalone technical path exists today.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Watch history (privacy request) | any account; GDPR/CCPA region-dependent | undocumented; some users report detailed JSON, others minimal data | user reports only — no confirmed stable format for any of the three |

None buildable: format and completeness vary by service and region, the
request takes up to 30 days, and the download link expires quickly.

## Access & auth

No public API for any of the three. The only data path is a manual
GDPR/CCPA request via each service's OneTrust-hosted privacy portal — slow,
undocumented output, short-lived links. Trove cannot automate or even
reliably parse this; building a first-class importer on folklore formats
with no sample would violate the evidence hierarchy.

## Vault mapping

- **Raw layer:** none (nothing to write).
- **Contract layer:** if a stable export ever materializes, rows would land
  in `media/plays/disney-hulu-max/YYYY-MM.jsonl` per the ratified
  media-plays contract (`category:"video"`). Until then, users who scrobble
  these services to Trakt via browser extensions get the same plays through
  the Trakt integration — that is the supported path.

## Build plan

None. Ship the `Behavior::Unavailable` stub (greyed card, reason above,
pointer to Trakt). Revisit if Disney formalizes an export during the 2026
Disney+/Hulu integration rollout — the research doc flags this as the one
thing worth monitoring.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the greyed card with the reason copy and the Trakt suggestion |

## Research notes

`integrations-research.md` → "Media: Music, Podcasts, Video & TV" §Disney+
/ Hulu / HBO Max (L3366–L3372); cross-cutting note "STREAMING VIDEO API
DESERT" (same section). Feasibility 🟠 low. Recommendation: icebox; capture
via Trakt scrobbling (browser extensions cover Disney+/Hulu/HBO), use the
formal privacy request only as a manual, user-driven backfill outside
Trove. Paramount+ sits in the same desert per the cross-cutting note.
