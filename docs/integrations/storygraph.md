# StoryGraph

- **id:** `storygraph`
- **domains:** `media/plays/` (contract: **media-plays, ✅ ratified** — finished-read
  events) · `media/storygraph/` (curation raw: shelves/TBR, ratings, tags — per the
  media-curation routing rule)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (user-initiated CSV export from account settings; re-export
  for incremental updates)
- **connection:** none
- **evidence:** official-docs — officially supported export at app.thestorygraph.com
  → Manage Account → "Manage Your Data" → "Export StoryGraph Library"; no API
- **effort / priority:** S / P2
- **needs:** none

## What it is

The de-facto Goodreads replacement: an independent (non-Amazon) social-reading
tracker known for mood/pace stats and half-star ratings, growing fast since 2020.
For readers who migrated off Goodreads, this export is where their recent reading
history lives; many users will import both services.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Library + read status | free, all accounts | Title, Author, Read Status (read/reading/TBR) | official export |
| Curation | free | Star Rating, Review, Tags, Formats | official export |
| Reading stats | free | book-level stats columns in the export | official export |

Book-level only: the per-session Reading Journal export (pages/duration/dates per
sitting) is a requested but **unshipped** feature as of mid-2026 — if it ships, it
slots in as additive columns, no special code paths now.

## Access & auth

- Export mechanism: app.thestorygraph.com → Manage Account → "Manage Your Data" →
  "Export StoryGraph Library" → CSV. User drops the file on the import box.
- No API, no auth, no TCC, no rate limits. Standalone-clean (local file parse).
- Card copy: re-export to refresh — same no-sync caveat as Goodreads.

## Vault mapping

- **Raw layer:** `media/storygraph/library.csv` snapshots (verbatim export) plus
  `media/storygraph/books.jsonl` — one row per book, every column preserved.
- **Contract layer:** `media/plays/storygraph/YYYY-MM.jsonl` per media-plays — one
  row per finished read where a read date exists: `category:"other"` with
  `extra.medium:"book"` (same enum note as Goodreads), `kind:"play"`, `title` =
  Title, `subtitle` = Author, `seconds: 0`, rating/tags in `extra`. TBR and
  currently-reading rows stay curation-only.
- **Dedupe:** `guid` = `storygraph-<title-author slug>-<date>` — the export has no
  stable book id column confirmed in the research doc, so slug from
  title+author; verify against a real export and prefer an id column if one exists.

## Build plan

1. Module `crates/trove-core/src/storygraph.rs`: `pub static DEF` with
   `Behavior::Import` — same `letterboxd.rs`-pattern importer as Goodreads; build
   the two together to share scaffolding.
2. One registration line in `INTEGRATIONS`; no connection.
3. The research doc gives the column list but not a sample file — confirm exact
   headers, date format, and any id column against a real export early in the
   iteration (cheap: any free account can produce one); fixture the empty-date and
   multi-tag cases.
4. Parser + store + re-import-idempotence tests, unique temp dirs.

## Vault mapping (as-built)

- **Raw layer:** `media/storygraph/books.jsonl` — one row per book, every CSV column verbatim plus `_slug` (dedup key). Includes to-read and currently-reading rows.
- **Contract layer:** `media/plays/storygraph/YYYY-MM.jsonl` — one row per finished read date (`Read Status == "read"` with parseable date). Uses `MediaItem` with `category:"other"`, `kind:"play"`, `extra.medium:"book"`.
- **Guid:** `storygraph-<title|authors slug>-<YYYY-MM-DD>`. Multiple read dates emit one row each (e.g. a re-read book).
- **Date strategy:** iterates `Dates Read` (comma-separated) then `Last Date Read` as fallback; falls back to `Date Added` if still empty.
- **Confirmed CSV columns:** Title, Authors, Read Status, Date Added, Last Date Read, Dates Read, Star Rating, ISBN/UID, Format, Review, Read Count, Owned? (confirmed via two open-source converters: rinsdoc/storygraph_to_goodreads + szilard-dobai/shelved).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Library import | ✅ tested | 8 unit tests pass; cargo check green |
| Re-import idempotence | ✅ tested | `reimport_is_idempotent` test: import twice → zero new rows |
| Half-star ratings | ✅ tested | `half_star_rating_preserved` test: "4.5" preserved verbatim in extra |
| Future columns | ✅ tested | `unknown_future_columns_preserved_in_raw` test |
| Goodreads coexistence | — | import both services' exports; rows stay per-source (no cross-source dedupe per the contract) |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §StoryGraph
(L3495–L3501). Feasibility 🟢 high; "build now alongside Goodreads — shares the
same M1 import pattern." Watch the Reading Journal (session-level) export feature:
when it ships, it upgrades this source from book-level events to real reading
sessions with durations.

**Export format notes:** Star Rating uses decimal half-star values (e.g. "4.5"),
not the integer-only Goodreads "My Rating". The export includes `Dates Read`
(comma-separated, all read dates for a re-read book) and `Last Date Read`
(most recent date only). A `Contributors` column (translators/narrators/illustrators)
has been reported in some exports (BookWyrm import issue) but is not mapped by the main
open-source converters; it is preserved verbatim in the raw layer if present.
Whether `Tags`, `Moods`, `Pace`, or `Content Warnings` columns appear in the export
could not be confirmed from public evidence (the referenced openreads issue #525 was
not accessible); these fields are visible in-app but their presence in the CSV export
is unconfirmed — they will be preserved in the raw layer if present.

**Date formats:** The StoryGraph CSV uses at least three date formats depending on
account locale (confirmed from rinsdoc/storygraph_to_goodreads `convertDate()`):
`YYYY/MM/DD`, `YYYY-MM-DD`, `MM/DD/YYYY`, and `Month DD, YYYY`. All four are handled
by `parse_date()`.

**Guid note:** No stable numeric book id column exists in the export (confirmed).
The slug strategy `storygraph-<title|authors>-<date>` is stable across re-exports.
If StoryGraph ever adds an id column, the importer can adopt it additively.
