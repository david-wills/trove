# Matter

- **id:** `matter`
- **domains:** `reading/` (would write here if it ever becomes available;
  contract: Phase 3 pending — reading)
- **status:** 🚫 unavailable
- **unavailable_reason:** Matter has no public API, no programmatic export,
  and no macOS local data store; its Premium Notion/Obsidian export requires
  manual app interaction. Revisit if Matter ships an API.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** research doc, confidence low *for any access path existing* —
  no developer API documented anywhere (hq.getmatter.com has no API page);
  iOS-first with no documented macOS local store
- **effort / priority:** M / P2 (effort is the estimate if an API ever ships)
- **needs:** none

## What it is

Matter is a polished iOS-first read-later app with a sizable user base,
which grew further after Pocket's 2025 shutdown. Saved-article history here
would slot straight into the reading domain — but there is currently no
sanctioned (or even unsanctioned-but-stable) way to get the data out
programmatically.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| none reachable | Premium ($60/yr) has manual Notion/Obsidian export only | n/a — no programmatic path | research doc |

## Access & auth

None. No public developer API, no export endpoint, no macOS app or local
data store to read. The Premium Notion/Obsidian export requires manual
interaction inside the iOS app, and routing Trove through a user's Notion
to reach Matter data is a multi-hop workaround the research doc rejects.

## Vault mapping

None today. If an API or stable export ships: raw layer
`reading/matter/raw/`, contract rows per the Phase 3 reading contract,
matching the other read-later providers.

## Build plan

None — catalogued so the app can show the honest reason on a greyed card.
Stub ships as `Behavior::Unavailable` with the `unavailable_reason` above.
Re-evaluate if Matter announces an API; the post-Pocket cohort makes it
worth monitoring (Phase 4 build-loop verification will recheck live docs).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows Matter dimmed, sorted last in the reading group, with the unavailable_reason copy |

## Research notes

`integrations-research.md` → "Web Activity & Content Consumption" §Matter
(Read-Later) (L1664–L1671). Feasibility 🟠 low. Matter has repeatedly stated
it is iOS-first with no macOS app. If a user manually runs the Premium
Obsidian export to a folder, the generic notes/files ingestion may pick the
markdown up incidentally — but that is not a Matter integration and we
don't advertise it as one. Alternatives for the same need: Readwise Reader,
Raindrop, Wallabag (all queued in this domain).
