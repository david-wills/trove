# Nintendo Switch

- **id:** `nintendo-switch`
- **domains:** `gaming/` (raw-only per the taxonomy — no contract; sessions
  could join media-plays at read time *if* this ever becomes buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** Nintendo has no official playtime API; the only
  path (nxapi Parental Controls) requires an external relay service to
  spoof the Switch Online app, violating Trove's standalone rule. Revisit
  if an official API or self-hostable relay appears.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** community — nxapi (github.com/samuelthomas2774/nxapi) +
  the nxapi-auth.fancy.org.uk relay; nxapi issue #8 confirms no playtime
  API exists outside Parental Controls. Research confidence: 🟠 low
  feasibility, well-evidenced blocker.
- **effort / priority:** L / P2
- **needs:** none (catalogued for the hub's "why isn't X available?" answer)

## What it is

Console gaming playtime from Nintendo's Switch ecosystem. For Switch
owners this is the only record of what they played and when — there is no
local trace on the Mac. High sentimental value, hard-blocked access.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Playtime per game | only via Parental Controls API (unofficial) | game, first-played, total minutes | nxapi community docs |
| Monthly play reports | Parental Controls app, view-only | per-month summaries | research notes — no bulk export |

None reachable without the relay; nothing here is buildable today.

## Access & auth

The only programmatic route: Nintendo account login → ID token → send the
token to a third-party relay (which runs the real NSO app on Android to
produce Nintendo's device-attestation `f` parameter) → access token →
Parental Controls API. The relay is an external runtime service Trove
cannot absorb as a library, and it processes the user's Nintendo ID token
— a standalone-rule violation *and* a privacy concern. No TCC angle; no
local data exists on macOS.

## Vault mapping

- **Raw layer:** would be `gaming/nintendo-switch/` (raw-only domain) —
  not applicable while unavailable.
- **Contract layer:** none — `gaming/` is raw-only.

## Build plan

None. Ship as a `Behavior::Unavailable` catalog stub (greyed card +
reason). Re-evaluate only if (a) Nintendo ships an official personal API,
or (b) a self-hostable attestation relay emerges that can compile into
the binary. The research doc's OCR-the-monthly-report-screenshot
workaround was considered and rejected as not worth engineering.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the greyed Nintendo Switch entry with the reason copy above, sorted last within Gaming |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Nintendo
Switch (via nxapi Parental Controls) (L3599–L3605). Feasibility 🟠 low.
The relay exists because Nintendo's auth needs device attestation only the
real iOS/Android NSO app can generate. Monthly Parental Controls reports
are visible in Nintendo's app but have no export. Not time-sensitive —
nothing is being lost that we could otherwise capture.
