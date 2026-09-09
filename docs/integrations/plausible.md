# Plausible Analytics

- **id:** `plausible`
- **domains:** none assigned (out of scope — no vault folder)
- **status:** 🚫 unavailable
- **unavailable_reason:** Out of scope: Trove is a vault for your personal
  data, and site analytics are aggregate statistics about your website's
  visitors, not your own activity. Own-website analytics may return in a
  later pass.
- **behavior:** Unavailable
- **connection:** none (would be a simple TokenPaste `plausible` connection
  if ever rescoped in — API key, bearer header, no OAuth)
- **evidence:** official Stats API docs (plausible.io/docs); research-doc
  level: 🟢 High — "simplest analytics API integration"
- **effort / priority:** S / P2 (if ever rescoped)
- **needs:** none

## What it is

Privacy-respecting website analytics (cloud or self-hosted; Community
Edition v2.2 is fully open-source AGPL-3.0). Like GA4, what it measures is
aggregate, anonymous statistics about the user's site *visitors* — not the
user's own activity — so the taxonomy rules the category out of scope
(README: "own-website analytics … catalogued as out-of-scope, no folder
assigned"). It is by far the easier of the two if the category ever
returns.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Aggregate stats (if rescoped) | cloud or self-hosted; API key | visitors, pageviews, bounce_rate, visit_duration, events; filters by page/source/country/device | official docs |
| Raw event-level data | self-hosted only | direct ClickHouse queries on the user's own server | official docs |

none built — table records what a future rescope would draw on.

## Access & auth

Would be: API key from account settings, `Authorization: Bearer <key>`,
GET/POST `https://plausible.io/api/v1/stats/aggregate` (or the same path on
a self-hosted instance — instance URL as user config). No OAuth dance, no
PII in the data by design. Self-hosting actually fits Trove's ethos
unusually well — noted for the rescope case.

## Vault mapping

none — no folder assigned. Out-of-scope sources don't get taxonomy routes;
a later rescope would have to propose one.

## Build plan

none. Phase 2 ships only the catalog entry: an `Unavailable` stub card,
greyed, dim, sorted last in its domain group, showing the
unavailable_reason above.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | confirm the hub shows the greyed card with the out-of-scope reason; hidden by the "hide unavailable" filter |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Plausible
Analytics (L4160–L4166). Feasibility 🟢 High; research doc even said "build
now if Trove targets indie makers/bloggers" — the block is purely the
scope decision, and the reason copy says so honestly. If own-website
analytics is ever rescoped in, build Plausible first (S effort, TokenPaste,
clean JSON), with GA4 as the companion.
