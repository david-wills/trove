# Goodreads

- **id:** `goodreads`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified** — finished-read
  events) · `media/goodreads/` (curation raw: shelves, ratings, reviews — per the
  media-curation routing rule)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-initiated CSV export; re-import periodically — no sync path)
- **connection:** none
- **evidence:** official-docs — stable account export at goodreads.com/review/import
  ("Export Library" → CSV) with a fully enumerated column list in the research doc;
  public API deprecated December 2020, no new keys issued
- **effort / priority:** S / P2
- **needs:** none

## What it is

The dominant social-reading service (Amazon-owned): shelves, star ratings, reviews,
and read-dates for tens of millions of readers, often going back 15+ years. Its API
died in 2020, but the account CSV export is comprehensive and stable — for most
long-time readers this file is the single best record of every book they've ever
logged.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Read history | free, all accounts | Title, Author, ISBN, Date Read, Date Added, Read Count | official export (column list in research doc) |
| Curation | free | Bookshelves, Exclusive Shelf, My Rating, My Review, Private Notes, Owned Copies | official export |
| Acquisition trivia | free | Original Purchase Date/Location, Condition, BCID | official export |

All optional in the contract; sparse rows (no Date Read, no rating) are normal.

## Access & auth

- Export mechanism: goodreads.com/review/import → "Export Library" → CSV download.
  User drops the file on the registry-driven import box.
- No auth, no API, no TCC, no rate limits. Standalone-clean (a local file parse).
- Limitations stated on the card: no friend activity, no in-book progress, and no
  automatic sync — re-export to refresh (the import must be re-runnable).

## Vault mapping

- **Raw layer:** `media/goodreads/library.csv` snapshots (keep the original export
  verbatim, full fidelity first) plus `media/goodreads/books.jsonl` — one row per
  book with every CSV column preserved.
- **Contract layer:** `media/plays/goodreads/YYYY-MM.jsonl` per media-plays — one
  row per *finished read*: `ts` = Date Read (date-only; midnight local),
  `category:"other"` (the ratified enum has no `book` value — carry
  `extra.medium:"book"` and raise the additive enum question in Phase 3),
  `kind:"play"`, `title` = Title, `subtitle` = Author (chart grouping key),
  `seconds: 0`, rating/review-flag/Read Count in `extra`. Books with no Date Read
  stay curation-only — never invent timestamps.
- **Dedupe:** `guid` = `goodreads-<Book Id>-<Date Read>` (Book Id alone for the
  curation rows) — re-imports of overlapping exports are no-ops.

## Build plan

1. Module `crates/trove-core/src/goodreads.rs`: `pub static DEF` with
   `Behavior::Import` — follow `letterboxd.rs`, the reference import example.
2. One registration line in `INTEGRATIONS`; no connection.
3. CSV parser against the documented column list; quirks to fixture: quoted
   multi-line reviews, `="ISBN"` Excel-guard formatting on ISBN columns, empty
   Date Read, Read Count > 1 with only one date.
4. Parser + store + re-import-idempotence tests, unique temp dirs.
5. Ship StoryGraph in the same iteration — same M1 CSV-import pattern, shared test
   scaffolding.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Library import | ✅ tested | export a real Goodreads library; drop on the import box; row count matches the export; finished reads appear in the Media tab |
| Re-import idempotence | ✅ tested | import the same CSV twice; zero new rows the second time |

## Implementation notes (added 2026-06-16, updated 2026-06-16)

- `Behavior::Import` — CSV only (no zip; Goodreads exports a bare CSV, not a zip).
- Raw layer: `media/goodreads/books.jsonl` — flat append-only JSONL, **schema-agnostic**: each row is serialised as a complete `header→value` JSON map built directly from `StringRecord + headers`. All columns present in the export are captured verbatim regardless of what struct fields are defined in the parser. This means future Goodreads column additions are never silently dropped.
- The real Goodreads "Export Library" CSV has **31 columns** (not 27 as originally coded): the four previously-missing fields are `Author l-f`, `Additional Authors`, `Number of Pages`, and `Bookshelves with positions`. All are now parsed in `BookRow` and flow into `extra` for finished-read contract rows.
- Contract layer: `media/plays/goodreads/YYYY-MM.jsonl` — one `MediaItem` per finished read (`Date Read` present). `category:"other"`, `kind:"play"`, `subtitle=Author` (grouping key), `seconds=0`. All 31 CSV columns flow into `extra`; `extra.medium="book"` marks the medium for a future additive enum extension.
- Excel-guard prefix (`="ISBN"`) stripped from `ISBN` and `ISBN13` columns in both raw and contract layers.
- `guid = "goodreads-<Book Id>-YYYY-MM-DD"` — uses the canonical ISO date (not the raw export string) so a re-export with `YYYY-MM-DD` punctuation deduplicates against one originally exported as `YYYY/MM/DD`.
- Books with no `Date Read` (unread/DNF/shelved) appear in raw only; no contract row is written (never fabricate timestamps).
- Date format: `YYYY/MM/DD` (primary); `YYYY-MM-DD` as fallback. Both formats produce the same guid.
- Short records (e.g. trailing BCID column omitted) are padded to header width before deserialization — real exports are tolerant of trailing-column variation.
- 7 unit tests: happy-path import, raw layer written unconditionally, idempotent re-import, hub card, media timeline integration, guid date-punctuation independence, unknown future columns preserved in raw.

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Goodreads
(L3487–L3493). Feasibility 🟢 high; "build now — simple M1 import, large user base,
clean CSV." No automatic sync is possible without brittle scraping — don't try.
StoryGraph is the de-facto Goodreads successor with its own export; many users will
import both (different guid namespaces keep them separate per the contract).
