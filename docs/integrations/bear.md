# Bear

- **id:** `bear`
- **domains:** `notes/` (contract: **bound** — `bear` is the first-in-domain
  collector; the ratified draft was promoted to the Rust-type leg this build.
  Apple Notes + Drafts + Day One + Obsidian + Logseq follow the same shape)
- **status:** 🧪 built (fixture-tested; first-in-domain binding of `notes/`;
  ZSFNOTE read + dynamic tag-join; needs FDA + real Bear notes to validate)
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read the SQLite DB on a schedule;
  modification-date watermark)
- **connection:** none (reads a local group-container SQLite DB; gated by
  Full Disk Access, the existing M3 pattern Trove already holds for Safari /
  iMessage)
- **evidence:** community-schema — the `ZSFNOTE` CoreData schema is
  well-documented with community tooling; stable across Bear 1 and Bear 2
  (same container). Confidence: high.
- **effort / priority:** S / P1
- **needs:** 🔒 privacy (opt-in / default-off — notes can hold very sensitive
  free text: journals, passwords; **David's call 2026-06-15**, overriding the
  brief's original `needs: none`, matching the Voice-Memos precedent) · FDA
  (the shared local-DB grant, already in the Needs-David queue)

## What it is

Markdown notes app for Mac/iOS with a tag-based organization model, popular
with writers and developers. The vault wants the user's own notes as
first-class personal data. Schema is stable and thoroughly documented, the
read pattern (copy-then-read a CoreData SQLite under a group container) is
the same one Trove already uses for Safari and iMessage — making this an S
effort on a proven pattern.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Notes | all | title (`ZTITLE`), markdown body (`ZTEXT`), uid, created/modified, trashed/archived flags | community schema |
| Tags | all | tag names via `ZSFNOTETAG` joined on `ZSFNOTETAG.ZNOTE` | community schema |
| Encrypted notes | all | opaque blob — skipped/placeholdered, never decrypted | community schema |

All optional in the (pending) notes contract.

## Access & auth

- File: `~/Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application
  Data/database.sqlite` — table `ZSFNOTE`. Key columns: `ZTITLE`, `ZTEXT`,
  `ZUNIQUEIDENTIFIER`, `ZCREATIONDATE`, `ZMODIFICATIONDATE`, `ZTRASHED`,
  `ZARCHIVED`, `ZENCRYPTED`. Tags in `ZSFNOTETAG`; attachments in a separate
  table.
- CoreData timestamps are seconds since 2001-01-01 — **add 978307200** to
  get Unix time.
- Permissions: Full Disk Access (TCC) — already a Trove dependency for the
  M3 local-DB pattern. No network, standalone-clean.
- **Copy-then-read:** make a temp copy of the DB before querying (WAL may be
  active); never open the live file.

## Vault mapping

- **Raw layer:** `notes/bear/raw/…` — the parsed note rows, full fidelity
  (all columns incl. flags, partitioned by modification month).
- **Contract layer:** `notes/bear/YYYY-MM.jsonl` per the **now-bound notes
  contract** ([`Note`]) — one row per note keyed by `id` =
  `ZUNIQUEIDENTIFIER`: `source`, `title`, `body` (markdown), `created`/
  `modified` (RFC3339 local), `tags[]`, `archived`/`trashed`/`pinned` flags,
  overflow in `extra`. Partitioned by the **month of `created`** (immutable),
  a per-affected-month whole-file atomic rewrite (snapshot). No `folder` (Bear
  is tag-only).
- **Dedupe:** `ZUNIQUEIDENTIFIER` as `guid`; modification-date watermark in
  `.trove/`, rebuildable by re-scanning the DB.

## Build plan

1. Module `crates/trove-core/src/bear.rs`: `DEF` (Periodic, FDA-gated),
   copy-then-read of the group-container SQLite, `pull` hook.
2. Registration line in `INTEGRATIONS` (no connection — local DB).
3. Parse: query `ZSFNOTE`, **skip `ZENCRYPTED=1` rows** (or store a
   placeholder), join `ZSFNOTETAG` for tags, convert CoreData epoch
   (+978307200).
4. Fixtures: a small synthetic `database.sqlite` covering tagged, archived,
   trashed, and encrypted rows; parser + store + watermark tests, unique
   temp dirs.
5. Vault writes via `store` helpers once the notes contract is ratified.

## Build status — 🧪 2026-06-15

Shipped (`bear.rs`, INDEX #18 — also the **first-in-domain binding** of the
`notes/` contract). `Behavior::Periodic` (hourly), FDA-gated (the iMessage/
Voice-Memos M3 local-DB pattern), no connection. **Default-off (🔒 opt-in)** —
notes can hold very sensitive free text (journals, passwords), so collection is
opt-in with explicit acknowledgement (**David's call 2026-06-15**, overriding the
brief's original `needs: none`, matching the Voice-Memos precedent); raw fidelity
always preserved once enabled.

Reads `~/Library/Group Containers/9K33E3U3T4.net.shinyfrog.bear/Application Data/
database.sqlite` (SQLite, **copy-then-open** via rusqlite — WAL may be live;
`TROVE_HOME` override for tests), **schema-adaptive SELECT** (`PRAGMA table_info`)
for OS/Bear-version robustness. Per `ZSFNOTE` row → `Note`: `id` =
`ZUNIQUEIDENTIFIER` (stable UUID, **not** `Z_PK`), `created`/`modified` = Core
Data epoch (**+978307200** → local), `title`/`body`, `trashed`/`archived`/
`pinned` flags, `tags[]`. Output `notes/bear/YYYY-MM.jsonl` (contract, month of
`created`) + `notes/bear/raw/YYYY-MM.jsonl` (full fidelity, **also month of
`created`**), dedup/upsert by `id`, watermark on `ZMODIFICATIONDATE`
(`.trove/bear-sync.json`). FDA-unreadable → graceful no-op (available:false).

**Tag join:** many-to-many via a Core Data join table `Z_<N>TAGS` whose entity
number is **version-dependent** → discovered dynamically (`sqlite_master` +
column-suffix classification, never hardcoded); tag name = `ZSFNOTETAG.ZTITLE`.
Confirmed against `jakeswenson/bear-query`, `andymatuschak/Bear-Markdown-Export`,
`vasylenko/bear-notes-mcp` + a real table-list dump.

**Encrypted notes** (`ZENCRYPTED=1`): the body is an opaque blob — never written
as plaintext (triple-guarded: skip + rusqlite `text()`/`raw_value()` reject
blobs). The metadata row is still emitted with `extra.encrypted=true`.

Contract binding (first collector in `notes/`): added the `Note` struct + the
`notes` `DOMAINS` entry (**`ContractKind::Snapshot`**, month-partitioned — a
third snapshot shape beside contacts' account-named files) + a small additive
`scan_contract` refinement (the Snapshot branch now harvests date-looking stems
as first/last keys; account stems like contacts' contribute none, unchanged) +
promoted the fixture to the ratified triad (5/5). **No carry-forwards** (notes
ratified as-drafted).

Adversarial-verify: **1 BLOCKING + 3 minor, fixed.** (B) the raw layer first
partitioned by `modified`-month + upserted by `id` → a note edited into a later
month duplicated (old row stranded in the prior file); fixed by partitioning raw
by `created` (immutable), matching the contract layer. (m) NULL
`ZMODIFICATIONDATE` fabricated a 2001 row → now omits `modified`; (m) the
unstable `Z_PK` rowid was dropped from the contract `extra` (raw keeps it). The
tag-join, epoch, `id`-stability, encrypted-skip, copy-then-open, and
schema-adaptive SELECT were all independently confirmed correct.

Gate: trove-core 590/0 (+11 bear, +3 notes, +1 contracts tests), `cargo check`
clean, `schedule_doc` regenerated (bear Periodic), `bindings.ts` up to date.

**Known v1 limitation:** a *hard-deleted* Bear note (row removed) can't be
detected by a modification-watermark scan and lingers until a full rescan;
*trashed* notes ARE captured (`ZTRASHED`). Deferred: attachments table.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Notes + tags | 🧪 (Needs FDA + Bear) | grant FDA to troved (shared with imessage/books/voice-memos); install Bear with a few tagged notes; Sync now; confirm rows in `notes/bear/` + hub last-data; tags present |
| Encrypted rows | 🧪 (Needs FDA + Bear) | lock a note in Bear; confirm it's emitted with `extra.encrypted=true` and NO `body`, never plaintext |
| Bear 2 | 🧪 (Needs FDA + Bear) | same container path; confirm a Bear 2 DB reads identically |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Bear (L2803–L2809). Feasibility 🟢 high. CoreData epoch offset
(+978307200) and the WAL copy-then-read pattern are the two gotchas; both
are already solved in Trove's existing local-DB collectors. Apple Notes,
Drafts, Day One, Obsidian, and Logseq follow the same `notes/` contract —
sequence one alongside Bear to exercise the contract with a second source.
