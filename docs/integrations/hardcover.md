# Hardcover

- **id:** `hardcover`
- **domains:** `reading/hardcover/` (contract: **reading.Item ✅** — library entries
  with state/progress) · `reading/hardcover/highlights/` (contract: **reading.Highlight
  ✅** — written reviews) · `reading/hardcover/raw/` (raw layer, unconditional).
  Brief originally mapped to media-plays; rerouted to reading.Item per REUSE MAP guidance
  that book-trackers fit reading better (state/progress fields are the right shape).
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (hourly-ish poll; cursor pagination)
- **connection:** `hardcover` — TokenPaste (personal API token from account
  settings → "Hardcover API"; no OAuth dance). Not shared with other defs.
- **evidence:** official-docs — GraphQL API documented at docs.hardcover.app,
  endpoint api.hardcover.app/v1/graphql; actively developed in 2026
- **effort / priority:** S / P2
- **needs:** none

## What it is

A rising Goodreads alternative that — unlike Goodreads, StoryGraph, and Literal —
ships a real, officially documented API: the same GraphQL endpoint its own apps
use, so personal tokens get full-fidelity library, reading dates, and reviews. The
only social-reading service in the catalog with a true sync path rather than a
re-export treadmill.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Library | free, all accounts | `user_books`: book, status, rating | official docs |
| Read sessions | free | `user_book_reads`: started/finished dates, pages | official docs |
| Reviews | free | `book_reviews` | official docs |

All optional in the contract. The API can also write (mark read, add reviews) —
out of scope for Trove, read-only token use.

## Access & auth

- GraphQL: `POST https://api.hardcover.app/v1/graphql`, Bearer token from account
  settings → "Hardcover API". Query `user_books`, `user_book_reads`,
  `book_reviews`; cursor pagination.
- No documented rate limit as of 2026 — be polite, poll hourly.
- No TCC, no local files. Standalone-clean (plain HTTPS, fixed first-party
  endpoint).

## Vault mapping

- **Raw layer:** `media/hardcover/raw/YYYY-MM.jsonl` — the GraphQL response
  objects, full fidelity.
- **Contract layer:** `media/plays/hardcover/YYYY-MM.jsonl` per media-plays — one
  row per `user_book_reads` entry with a finish date: `ts` = finished date,
  `category:"other"` with `extra.medium:"book"` (enum note shared with the other
  book sources), `kind:"play"`, `title` = book title, `subtitle` = author,
  `seconds: 0`, pages/started-date/rating in `extra`. Library statuses and reviews
  stay in the curation raw layer.
- **Dedupe:** `guid` = the `user_book_reads` row id (the API exposes real ids —
  use them). Watermark cursor in `.trove/hardcover-sync.json`, rebuildable from
  output files.

## Build plan

1. Module `crates/trove-core/src/hardcover.rs`: `pub static DEF` (Periodic,
   hourly), `pub static CONNECTION` (TokenPaste: setup copy pointing at account
   settings → "Hardcover API", per the SimpleFIN affordance rule), `pull` hook
   for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. GraphQL via plain `reqwest` POST — no GraphQL client crate needed for three
   fixed queries.
4. Fixtures from docs.hardcover.app documented shapes (a read with and without a
   finish date; multi-read rereads); parser + store + cursor tests, unique temp
   dirs.
5. Build before Literal — Literal's brief explicitly sequences after this one and
   reuses the GraphQL-poll pattern.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Library items | ✅ built | paste a real token; Sync now; rows in `reading/hardcover/YYYY-MM.jsonl`; counts match profile library count |
| Reviews as highlights | ✅ built | books with written reviews → rows in `reading/hardcover/highlights/YYYY-MM.jsonl` |
| Incremental poll | ✅ built | mark a book finished; wait a poll cycle; exactly one new item row, no duplicates on re-run |
| Raw layer | ✅ built | `reading/hardcover/raw/YYYY-MM.jsonl` has verbatim API objects |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Hardcover
(L3503–L3509). Feasibility 🟢 high; "build now — rising Goodreads alternative with
a real API; small extra effort over CSV-only sources." Same API the mobile/web
apps use, so coverage equals what the user sees in-app.
