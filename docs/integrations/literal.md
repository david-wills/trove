# Literal

- **id:** `literal`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified** — read events)
  · `media/literal/` (curation raw: shelves, reading status — per the
  media-curation routing rule)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll the GraphQL endpoint; cadence TBD at spike)
- **connection:** `literal` — TokenPaste composite `email:password`; re-authenticates
  each sync via the `login` mutation to get a fresh JWT. Not shared with other defs.
- **evidence:** official docs at literal.club/developers — GraphQL endpoint
  `https://literal.club/graphql/`, login mutation, `myReadingStates` query, and
  Book/ReadingState type fields all confirmed from the published developer page.
  Marked undocumented in the original brief; the API is in fact officially documented.
- **effort / priority:** M / P2
- **needs:** Needs-sample (no docs and no export — stable query shapes must be
  spiked and captured as fixtures before parsing is trusted)

## What it is

A smaller, design-forward Goodreads competitor popular with indie-reading circles.
It has **no export option** as of 2026, so the unofficial GraphQL API its own app
uses is the only way a Literal user can get their library and reading history out —
which is exactly why it's worth carrying despite the risk.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Reading status / history | free, all accounts | books with statuses (want-to-read / reading / finished) | community-documented queries |
| Library & shelves | free | shelf contents, book metadata | community-documented queries |

Field-level detail is unconfirmed until the spike — the contract's omit-if-empty
rule absorbs whatever subset proves stable.

## Access & auth

- GraphQL endpoint inferred from app traffic (community reverse-engineering of the
  schema is the only path); auth is an API token / authenticated session from the
  user's credentials.
- No published rate limits; poll conservatively.
- No TCC, no local files. Standalone-clean HTTPS, but **unofficial**: breaking
  changes can land without notice — on schema errors, degrade gracefully (status
  line + disabled card hint), never hard-fail.

## Vault mapping

- **Raw layer:** `media/literal/raw/YYYY-MM.jsonl` — GraphQL response objects,
  full fidelity.
- **Contract layer:** `media/plays/literal/YYYY-MM.jsonl` per media-plays — one
  row per finished read with a date: `category:"other"` with
  `extra.medium:"book"` (shared enum note), `kind:"play"`, `title` / `subtitle`
  = book/author, `seconds: 0`. Statuses and shelves stay curation-raw.
- **Dedupe:** `guid` = Literal's object id if the schema exposes one (GraphQL
  almost certainly does); confirmed at spike. Cursor in
  `.trove/literal-sync.json`, rebuildable.

## Build plan

1. **Spike first** (the research doc's explicit recommendation): authenticate with
   a real account, capture the working queries for reading states + shelves, and
   freeze the responses as fixtures. Parser is written against captured fixtures
   only — Needs-sample gates the build.
2. Module `crates/trove-core/src/literal.rs`: `pub static DEF` (Periodic),
   `pub static CONNECTION` (TokenPaste, with honest card copy that this is an
   unofficial API), `pull` hook.
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Graceful-degradation path for schema drift (status JSONL line, card hint).
5. Sequence **after Hardcover** — same GraphQL-poll shape, with the official one
   exercised first.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Spike: query shapes | ✅ confirmed | literal.club/developers — endpoint, login mutation, myReadingStates, Book fields all official |
| Library + reads | 🧪 built | paste email:password in the connect card; Sync now; rows in `reading/literal/YYYY-MM.jsonl` match the in-app library count |
| Drift handling | 🧪 built | on schema errors the pull returns Err (logged, not crash); missing fields silently skip the row |

## Build notes (2026-06-17)

- Brief rerouted to `reading.Item` from media-plays per REUSE MAP guidance (book-trackers fit reading better); identical precedent as hardcover.
- Auth: email+password composite stored as TokenPaste — re-authenticates each sync via `login` mutation to get a fresh JWT (no permanent API key available).
- API turned out to be officially documented at literal.club/developers, not community-confirmed only; evidence is stronger than the brief assessed.
- `myReadingStates` returns all states at once (no pagination) — full drain each sync, guid-based dedupe.
- Vault paths: `reading/literal/YYYY-MM.jsonl` (contract) + `reading/literal/raw/YYYY-MM.jsonl` (raw).
- 8 tests, all green.

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Literal
(L3511–L3517). Feasibility 🟡 medium; "build later — smaller user base than
Hardcover/Goodreads; unofficial API risk. Implement after Hardcover." No export
exists, so this unofficial path is also the user's only escape hatch — worth the
maintenance cost, priced in as effort M for an S-sized data shape.
