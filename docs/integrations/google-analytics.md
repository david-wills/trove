# Google Analytics (GA4)

- **id:** `google-analytics`
- **domains:** none assigned (out of scope — no vault folder)
- **status:** 🚫 unavailable
- **unavailable_reason:** Out of scope: Trove is a vault for your personal
  data, and site analytics are aggregate statistics about your website's
  visitors, not your own activity. Own-website analytics may return in a
  later pass.
- **behavior:** Unavailable
- **connection:** none (would share the existing `google` OAuth connection
  if ever rescoped in — analytics.readonly scope)
- **evidence:** official GA4 Data API v1 docs
  (developers.google.com/analytics/devguides/reporting/data/v1); research-doc
  level: 🟡 Medium
- **effort / priority:** M / P2 (if ever rescoped)
- **needs:** none

## What it is

Google's website-analytics product. The GA4 Data API returns the site
*owner's* aggregate visitor statistics — sessions, pageviews, traffic
sources, countries. By GA4's own design this is anonymous aggregate data
about *other people* visiting the user's site, not a record of the user's
own life — which is exactly why the taxonomy rules it out of scope
(README: "own-website analytics … catalogued as out-of-scope, no folder
assigned").

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Report metrics (if rescoped) | free API; needs GA4 on user's site | sessions, pageviews, users, bounce rate, engagement, top pages, sources | official docs |
| Raw event-level data | requires GCP + BigQuery export | event stream | official docs — heavy setup, poor fit |

none built — table records what a future rescope would draw on.

## Access & auth

Would be: Google OAuth with `analytics.readonly` scope (shareable with the
existing `google` connection), POST `/v1beta/properties/{propertyId}:runReport`,
user-configured property ID, free API. BigQuery raw export needs a GCP
account — out of the question for a standalone local app's default path.

## Vault mapping

none — no folder assigned. Out-of-scope sources don't get taxonomy routes;
a later rescope would have to propose one (site-analytics is not
personal-activity-shaped, which is the whole problem).

## Build plan

none. Phase 2 ships only the catalog entry: an `Unavailable` stub card,
greyed, dim, sorted last in its domain group, showing the
unavailable_reason above.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | confirm the hub shows the greyed card with the out-of-scope reason; hidden by the "hide unavailable" filter |

## Research notes

`integrations-research.md` → "Social Media & Web Presence" §Google
Analytics / GA4 (L4152–L4158). Technically feasible (free API, clean
OAuth) — the block is scope, not access, and the reason copy says so
honestly. Niche audience (personal-site owners). If the category is ever
rescoped in, Plausible is the easier first build (single API key, no OAuth)
and the two would land together.
