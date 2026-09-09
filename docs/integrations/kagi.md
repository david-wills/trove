# Kagi

- **id:** `kagi`
- **domains:** `browser/searches/` (would-be; nothing is written — raw-only
  domain, no contract implicated)
- **status:** 🚫 unavailable
- **unavailable_reason:** Kagi deliberately stores no search history
  server-side — privacy is the product, so there is nothing to export or
  fetch. Searches could only be captured in the browser as you make them.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** confirmed blocked — Kagi's own documentation states queries
  are never associated with accounts (temporarily logged for debugging,
  auto-purged); research entry L1680–L1687
- **effort / priority:** XL / P2 (effort reflects that the only conceivable
  path is building browser-side capture, not a Kagi integration)
- **needs:** none

## What it is

Kagi is a paid, privacy-first search engine. Its privacy model
intentionally prevents any server-side search history from existing: no
export, no history API, queries never tied to the account. For Trove this
is a by-design dead end — a feature, not a bug, from Kagi's perspective —
and the catalog records it so the app can answer "why isn't Kagi here?"
honestly.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Search history | — | none exists server-side | Kagi docs (confirmed blocked) |

none.

## Access & auth

none. Kagi does sell a Search API ($0.012/query) for *running* searches
programmatically — that is unrelated to retrieving personal history and is
not a path. No export, no local store, no workaround short of intercepting
queries in the browser at search time.

## Vault mapping

none. If browser-side query capture is ever built, it would be a
**browser-extension capability** (the extension sees the search as it
happens) writing `browser/searches/` — an extension feature, not a `kagi`
def. This card stays as the honest explainer.

## Build plan

none — renders as a greyed card via `Behavior::Unavailable` with the reason
above (Phase 1 unavailable-card rendering). Revisit only if Kagi ever ships
account-side history (against their stated model) or if the browser
extension grows query-time search capture, which would supersede this entry
for Kagi users.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the Kagi card dim, sorted last in its domain, with the unavailable_reason copy |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Kagi
Search History (L1680–L1687). Feasibility 🔴 blocked by design.
Recommendation: skip — unsolvable; users who care about search history
should know this gap exists when choosing Kagi. The browser-extension
capture idea is the only note worth carrying forward.
