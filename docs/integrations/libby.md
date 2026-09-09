# Libby / OverDrive

- **id:** `libby`
- **domains:** `media/plays/` (contract: **media-plays — ratified**; raw CSV
  kept per-source under `media/plays/libby/raw/`)
- **status:** 🧪 built (fixture-tested, not validated — Needs-sample: a real
  OverDrive email-history CSV to confirm Borrow Date format + Type values)
- **unavailable_reason:** none
- **behavior:** Import (user-initiated CSV; no programmatic pull exists)
- **connection:** none (no login — the export arrives by email and the user
  drops the file in the import box)
- **evidence:** official export mechanism (any library's OverDrive site →
  History → "Email history" → CSV) but **no documented schema with
  examples** — field list (title, author, format, borrow/return dates) is
  research-level only. Sample-required.
- **effort / priority:** S / P2
- **needs:** Needs-sample (CSV columns unverified — parser built last,
  against a real export)

## What it is

Libby (by OverDrive) is the dominant app for borrowing ebooks and
audiobooks from public libraries. Borrow history is a reading record that
exists nowhere else — library reads never show up in Goodreads, Kindle, or
Apple Books unless the user logs them manually. Niche but cheap: it reuses
the Goodreads/StoryGraph CSV-import pattern wholesale.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Borrow history | patron must have history enabled (opt-in, per library) | title, author, format (ebook/audiobook), borrow date, return date | research doc field list; sample-required |

All optional in the contract (omit-if-empty). Users whose library disables
reading history simply get an empty export — the import UI should say so
rather than look broken.

## Access & auth

- **M1 export only.** Library's OverDrive website → History → "Email
  history" → CSV delivered to the user's email; they save it and import.
- **No API.** Libby exposes no developer API and no keys. The Libby
  Timeline share/export produces a plain URL list, not structured data —
  not worth parsing.
- No TCC, no network calls from Trove. Standalone-clean by construction.

## Vault mapping

- **Raw layer:** `media/plays/libby/raw/` — imported CSVs kept verbatim
  (full fidelity first).
- **Contract layer:** `media/plays/libby/YYYY-MM.jsonl` per the ratified
  media-plays contract — one row per borrow (`ts` = borrow date, `source` =
  libby, `title`, `creator` = author, kind from format; return date and
  format detail in `extra`). A borrow is a coarse "play" — readers should
  treat it as "checked out", not "finished".
- **Dedupe `guid`:** no stable id is documented in the export — derive from
  `(title, author, borrow_date)` hash; re-imports of overlapping exports
  must be idempotent.

## Build plan

1. **Parser-last / Needs-sample:** the CSV column set is undocumented —
   acquire a real OverDrive email-export (any library account) before
   writing the parser; header-sniff like the generic CSV importer.
2. Module `crates/trove-core/src/libby.rs`: `DEF` with
   `Behavior::Import` — the registry gives the import box, hub card, and
   Recent-data view for free. `letterboxd.rs` is the reference.
3. No `CONNECTION` (nothing to log into).
4. Fixtures: the sample CSV (redacted) + an empty-history variant; tests
   for idempotent re-import and the derived guid.
5. Vault writes via `store` helpers; media-plays contract is ratified, so
   nothing is parked.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Borrow-history import | 🧪 built | trigger "Email history" from a library OverDrive account; import the CSV; confirm rows in `media/plays/libby/` + hub last-data; re-import the same file and confirm zero new rows |
| History-disabled case | 🧪 built | import an empty/headers-only CSV; confirm friendly empty-state, no error |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Libby /
OverDrive (L3527–L3533). Feasibility 🟡 medium — the export exists but the
email-to-self loop is real friction, and history is opt-in per patron and
per library. No time-sensitivity (history accumulates server-side once
enabled; the export is repeatable). Recommendation was "build later" —
hence P2. Same import pattern as Goodreads/StoryGraph; sequence after one
of those so the CSV-book-import muscle already exists.

## Build notes (Phase 4 fan-out)

- **Status:** 🧪 built (Behavior::Import, reuses media-plays contract)
- **Evidence source:** iamdav.in blog post showing the raw CSV header string
  confirmed 13 columns: `Title, Sub Title, Author, Series, Publisher,
  Publish Date, Star Rating, Star Rating Count, Maturity Level, ISBN,
  Cover Art URL, Borrow Date, Type`
- **No return date in export.** The OverDrive email-history CSV does not
  include a return/due date; only borrow date is present.
- **GUID:** derived as SHA-256 prefix over normalised (title, author,
  borrow_date) because no stable row ID is emitted by the export.
- **Date format:** `%m/%d/%Y` (US locale) attempted first; ISO-8601
  fallback. Exact format unconfirmed — Needs-sample flag set.
- **Type → category:** anything containing "audio" → `"audiobook"`;
  everything else → `"other"` (with `extra.medium = "book"` for books).
  Exact Type string values (e.g. "ebook-overdrive", "audiobook-overdrive")
  are undocumented — parser accepts them as-is.
- **Raw layer:** `media/libby/borrows.jsonl` — all 13 columns verbatim
  plus `_guid` for re-import dedup. Schema-agnostic: future OverDrive
  column additions are automatically preserved.
- **Contract layer:** `media/plays/libby/YYYY-MM.jsonl` — one row per
  borrow with a parseable date; dateless rows go raw-only.
- **No connection, no OAuth, no network calls.** Import is user-triggered.
- **9 unit tests,** all passing: basic import, idempotency, raw layer,
  guid stability, ISO date fallback, empty CSV, media timeline, hub card,
  unknown future columns.
