# Apple Books

- **id:** `apple-books` (shipped def id: `books`)
- **domains:** `books/` (**grandfathered** pre-taxonomy path — closed set,
  post-wave tidy-up migration); taxonomy targets are `media/plays/`
  (contract: **media-plays — ratified**, for read events) and `reading/`
  (highlights — contract: **Phase 3 pending**, drafted with Readwise /
  Instapaper / Kindle clippings)
- **status:** 🧪 built (shipped pre-pipeline; David promotes to ✅)
- **unavailable_reason:** none
- **behavior:** Periodic (daily local-DB snapshot + diff)
- **connection:** none (local SQLite; no login)
- **evidence:** community-documented SQLite, high confidence —
  `BKLibrary*.sqlite` table `ZBKLIBRARYASSET` (ZTITLE, ZAUTHOR,
  ZDATEFINISHED, ZREADINGPROGRESS, …) and `AEAnnotation_*.sqlite`
  (ZANNOTATIONSELECTEDTEXT, ZANNOTATIONNOTE, epub CFI locations); validated
  in the shipped def
- **effort / priority:** S / P2 (remaining work only)
- **needs:** Needs-David (✅ promotion); contract-layer extension parked on
  the Phase 3 `reading/` contract (highlights → `reading/`, dedupe vs
  Readwise)

## What it is

Apple's built-in ebook reader on macOS/iOS. Its local SQLite databases hold
the user's library, per-book reading progress and finish dates, and every
highlight/note — local reading state that cloud aggregators miss for users
not on Readwise. **Already shipped** as the `books` def.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Library snapshot | none | title, author, added/finished dates, reading progress (0.0–1.0), page count | community schema; shipped |
| Annotations (highlights + notes) | none | selected text, note text, CFI location, timestamps | community schema; shipped |
| Change events | none | append-only diffs (`highlighted`, finished, …) derived by the snapshot pass | shipped (`books/events/`) |

All optional; books synced from iOS devices appear in the same DBs.

## Access & auth

- Local SQLite under `~/Library/Containers/com.apple.iBooksX/Data/Documents/`:
  `BKLibrary/BKLibrary*.sqlite` (filename varies — glob) and
  `AEAnnotation/AEAnnotation_*.sqlite`.
- **Full Disk Access** (Containers/ is TCC-protected) — already granted for
  iMessage etc. Copy-then-read to avoid locks. `ZDATEFINISHED` is Mac
  absolute time (seconds since 2001-01-01).
- No network. Standalone-clean.

## Vault mapping

- **Raw layer (as built, grandfathered):** `books/library.jsonl` (assets),
  `books/annotations.jsonl` (non-deleted annotations),
  `books/events/YYYY-MM.jsonl` (append-only diffs). The path is the schema
  identifier — no renames until the scheduled post-wave migration of the
  closed grandfathered set.
- **Contract layer (future):** finish/read events join `media/plays/`
  (ratified); highlights join `reading/` once that contract is ratified
  (Phase 3). **Dedupe vs Readwise:** Apple Books highlights also sync to
  Readwise if the user connects them there — the `reading/` mapping must
  dedupe across the two sources.
- **Dedupe `guid`:** annotation `uuid` / library `asset_id` (as built).

## Build plan

Shipped — remaining work only:

1. None for collection: library + annotations + events are live in
   `crates/trove-core/src/books.rs` (`Behavior::Periodic`, daily cadence).
2. Phase 3, after the `reading/` contract ratifies: add the contract-layer
   mapping (highlights → `reading/`, read events → `media/plays/`) with the
   Readwise dedupe rule. Parked until then.
3. Post-wave: the `books/` → taxonomy tidy-up migration (scheduled,
   pipeline doc After-the-wave section) — do not grow the grandfathered
   set meanwhile.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Library + annotations + events | 🧪 shipped pre-pipeline | open a book in Apple Books, highlight a passage; next daily pass (or Sync now) shows the new annotation in `books/annotations.jsonl` and a `highlighted` event; **only David promotes to ✅** |
| `reading/` contract mapping | — | not built; validate after Phase 3 ratification (incl. no-double-import with Readwise connected) |

## Research notes

`integrations-research.md` → "Media: Books, Reading & Gaming" §Apple Books
(local DB) (L3535–L3541). Feasibility 🟢 high. Cross-cutting note 3: shared
copy-then-open-SQLite utility with other FDA collectors; check Readwise
configuration to avoid duplicate highlight entries. Readwise (note 5) is
the aggregator complement — Apple Books local DB is the zero-dependency
path for non-Readwise users.
