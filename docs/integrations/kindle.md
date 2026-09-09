# Kindle Highlights

- **id:** `kindle`
- **domains:** `reading/` (contract: **`reading.Highlight` bound** —
  highlights shape pioneered from Readwise + Instapaper + Kindle + Snipd;
  Kindle reuses it)
- **status:** 🧪 built (fixture-tested, not validated — Needs-sample: a real
  `My Clippings.txt` to confirm the dominant header format)
- **unavailable_reason:** none
- **behavior:** Import (user plugs the Kindle in over USB and drops
  `My Clippings.txt` on the import box; re-import appends new clippings)
- **connection:** none (no account, no API — a file off the device)
- **evidence:** community-documented, long-stable plain-text format at
  `/Volumes/<KindleName>/documents/My Clippings.txt`; widely parsed by
  existing tooling
- **effort / priority:** S / P2
- **needs:** Needs-sample — a real `My Clippings.txt` to confirm the
  dominant header format (reading contract `reading.Highlight` is bound)

## What it is

Highlights, notes, and bookmarks made on a **physical Kindle e-reader**,
accumulated in one plain-text file on the device. Amazon offers no API for
highlights, so for non-Readwise users this file is the only zero-dependency
way to get years of book annotations out. Device-only by design: highlights
made in the Kindle phone/tablet apps never appear here.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Highlights | none (device file) | book title/author, location range, added date, highlight text | community format docs |
| Notes | none | same header + the typed note | community format docs |
| Bookmarks | none | title, location, date (no text) | community format docs |

All optional in the contract. Purchased books are capped at ~10% of the
book's text — capped clippings carry a "You have reached the clipping
limit" marker instead of text; parse those as rows with empty text + a flag
in `extra`, never as errors. Sideloaded books have no cap.

## Access & auth

- File path on the mounted device:
  `/Volumes/<KindleName>/documents/My Clippings.txt`. Plain text; one
  clipping per block, `==========` separator; block = title/author line,
  `- Your Highlight on page X | location Y-Z | Added on <date>` line
  (three-segment dominant form; older firmware uses two-segment
  `"at location Y-Z | Added on <date>"`), blank line, text.
- No auth, no TCC (user-selected file through the generic import box), no
  network. Standalone-clean.
- The cloud alternative (`read.amazon.com/notebook`) has no official API —
  fragile scraping only, explicitly not a path. Readwise is the pragmatic
  cloud complement and is its own def.

## Vault mapping

- **Raw layer:** `reading/kindle/raw/` — imported `My Clippings.txt` copies
  kept verbatim (the file is append-only on device; keeping each import
  preserves provenance).
- **Contract layer:** `reading/kindle/YYYY-MM.jsonl` per the pending reading
  contract — one row per clipping (`ts` = Added date, `source`, `guid`,
  `title`, `author`, `highlight`/`note` text, kind), location range and
  clipping-limit flag in `extra`.
- **Dedupe:** `guid` = hash(title, location range, added date, kind) — the
  file has no ids and re-imports overlap heavily by design; dedupe makes
  re-import idempotent.

## Build plan

1. Module `crates/trove-core/src/kindle.rs`: `DEF` with
   `Behavior::Import` — `letterboxd.rs` is the reference import example.
   No connection.
2. Registration line in `INTEGRATIONS`.
3. Parser: block splitter on the `==========` separator; tolerant of the
   known per-locale/firmware date and header variants; fixtures covering
   highlight, note, bookmark, and clipping-limit blocks; tests in unique
   temp dirs.
4. Import-box copy: tell the user where the file lives on the mounted
   Kindle and that app-made highlights won't be in it.
5. Contract rows via `store` helpers — the `reading.Highlight` contract is
   bound, so nothing is parked; the parser awaits only a real sample
   (Needs-sample) for final header-format confirmation.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Highlights/notes/bookmarks import | 🧪 built | mount a real Kindle over USB, import `My Clippings.txt`, confirm rows in `reading/kindle/highlights/` + hub last-data; re-import the same file and confirm zero duplicate rows |
| Clipping-limit handling | 🧪 built | import a file containing a capped clipping; confirm a flagged row with `capped=true` in `extra`, not a parse error |

## Build notes (2026-06-16, updated 2026-06-16)

- **Behavior:** `Import` — `letterboxd.rs` pattern; no connection, no token.
- **Contract:** `reading.Highlight` (bound) under `reading/kindle/highlights/YYYY-MM.jsonl`; raw under `reading/kindle/raw/`.
- **Metadata line format:** The dominant real-world format is three pipe-segments:
  `"- Your Highlight on page 3 | location 429-430 | Added on <date>"`. The Kindle
  location range (`429-430`) is extracted as the contract `location` field; the
  physical page number (`3`) goes to `extra.page`. Older/alternate firmware uses a
  two-segment form (`"at location 142-145 | Added on <date>"`); both are handled.
  The initial build used only the two-segment form from an internal research note —
  the three-segment dominant shape was added in this fix commit.
- **Guid:** `sha256(title | kind | normalized_location | date_raw)` — dash-normalized
  (en/em-dash → ASCII hyphen) so firmware/locale variants that differ only in dash
  style produce the same guid and re-import stays idempotent.
- **Raw layer:** Stores `block_text` (the verbatim untrimmed block string) alongside
  parsed fields so a future re-parse can recover anything the current parser drops.
- **Date parsing:** Handles EU format (`8 June 2026 20:11:00` / `Monday, 8 June 2026 20:11:00`) and US format (`June 7, 2026 9:05:00 AM` / `Sunday, June 7, 2026 9:05:00 AM`). chrono's `%A` validates weekday vs. date; fallback strips the weekday prefix so incorrectly labeled firmware dates still parse.
- **Note vs. highlight:** Kindle "Note" blocks carry user text with no underlying passage — mapped to `note` field (not `text`). Highlight blocks go to `text`. Bookmarks have neither.
- **Capped clippings:** Amazon's ~10% cap produces a marker line instead of text; parsed as `text=""` + `extra.capped=true`, never as a skip or error.
- **BOM:** `\u{FEFF}` stripped from the first block (some firmware versions prepend it).
- **No sample on disk** (`~/Trove-samples/` empty for Kindle) — format verified against three independent primary sources (lvzon/kindle-clippings canonical parser, 2024 Medium walkthrough, KindleExport docs). Fixtures cover five block types (three-segment highlight, three-segment note, two-segment bookmark, three-segment capped, two-segment older-firmware) with both date families and capital-Location keyword variant.

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Kindle
Highlights (My Clippings.txt) (L3479–L3485). Feasibility 🟢 high — stable
plain-text format, fully parseable. Known limits: device-only (no app
highlights), ~10% clipping cap on purchased books, no official cloud API
(notebook scrape rejected as fragile). Readwise (`readwise` def) covers the
same highlights for its subscribers — dedupe at the contract layer when
both are enabled.
