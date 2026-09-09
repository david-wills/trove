# Drafts

- **id:** `drafts`
- **domains:** `notes/` (contract: **Phase 3 pending** — drafted with Apple
  Notes + Bear + Day One + Obsidian + Logseq together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (copy-then-read the group-container SQLite; watermark
  on modified timestamp)
- **connection:** none — local-only. Reads a macOS group container under Full
  Disk Access (already in Trove's TCC grant).
- **evidence:** community — the Drafts group-container SQLite is documented as
  readable by third-party tooling; the schema itself is **not** officially
  published (Needs-sample for one-time `.schema` introspection)
- **effort / priority:** S / P1
- **needs:** Needs-sample (schema unpublished — one-time `sqlite3 .schema`
  introspection before the parser) · notes contract not yet ratified
  (Needs-David)

## What it is

Drafts (Agiletortoise) is a "capture first, act later" notes app for Mac and
iOS: everything starts in one frictionless text field, then gets tagged, filed,
or routed via actions. Heavy users accumulate years of quick-capture text —
ideas, snippets, journal fragments, message stubs — that exists nowhere else.
High personal-knowledge value, low collection cost (a local DB read).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Draft bodies | all | full markdown/plain text per draft | community DB schema (needs introspection) |
| Timestamps | all | created, modified | community |
| Tags / workspaces | all | tag list, flagged/archived/trash state | community |

All optional in the contract (omit-if-empty). No tier gating — Drafts Pro
adds features, not data shapes.

## Access & auth

- Primary DB: `~/Library/Group Containers/GTFQ98J4YG.com.agiletortoise.Drafts/`
  — a SQLite store with all drafts, actions, and workspaces. App prefs live in
  `~/Library/Containers/com.agiletortoise.Drafts-OSX`.
- Fallback: `iCloud Drive/Drafts/` backup folder is a watch target if FDA is
  ungranted (M2 path) — not the primary route.
- Auth: none. Reads the group container under the existing Full Disk Access
  grant. Copy-then-read against the live WAL DB; open read-only.
- Standalone-clean: no network, no running app dependency. The Drafts URL
  scheme / AppleScript dictionary exist for write-back but are not needed for
  collection.

## Vault mapping

- **Raw layer:** `notes/drafts/raw/` — full-fidelity rows from the DB
  (body, created, modified, tags, flags), partitioned by month of created
  date.
- **Contract layer:** `notes/drafts/YYYY-MM.jsonl` per the (pending) notes
  contract — expected shape: one row per draft (`ts` = created, `source`,
  `guid` = draft UUID, `title` = first line, `body`, `tags[]`, `modified`),
  overflow in `extra`. Parked behind **Needs-David (contract)** until the notes
  shape is ratified.
- **Dedupe:** draft UUID as `guid`; modified-timestamp watermark in
  `.trove/drafts-sync.json`, rebuildable by scanning output files.

## Build plan

1. **Schema introspection first** (Needs-sample): run a one-time
   `sqlite3 .schema` on a real Drafts group-container DB to confirm table and
   column names — parser-last, do not guess the schema.
2. Module `crates/trove-core/src/drafts.rs`: `DEF` (Periodic), copy-then-read
   the WAL DB, `pull` hook for Sync-now. No connection (local).
3. Registration line in `INTEGRATIONS`.
4. Fixtures from a real (or introspected) DB snapshot; parser + store +
   watermark tests, unique temp dirs.
5. Vault writes via `store` helpers once the notes contract is ratified.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Draft bodies + tags | ✅ built | enable with FDA granted; Sync now; confirm rows in `notes/drafts/` + hub last-data; spot-check a known draft's body/tags |
| Schema correctness | ✅ confirmed | `sqlite3 .schema` introspected on real DB 2026-06-15; table is `ZMANAGEDDRAFT`, UUID in `ZUUID`, timestamps `ZCREATED_AT`/`ZMODIFIED_AT` (Core Data epoch), content in `ZCONTENT`, tags via `ZMANAGEDDRAFTTAG` join table + `ZCACHED_TAGS` fallback (`ZZZ<tag>ZZZ` delimited) |

## Schema introspection (2026-06-15)

Confirmed real Drafts DB at
`~/Library/Group Containers/GTFQ98J4YG.com.agiletortoise.Drafts/DraftStore.sqlite`:

- Primary table: `ZMANAGEDDRAFT`
- `ZUUID` — stable per-draft UUID (dedupe key)
- `ZCREATED_AT` / `ZMODIFIED_AT` — Core Data seconds since 2001 (same Apple
  epoch as Bear: +978 307 200 for Unix)
- `ZCONTENT` — full draft text (Markdown / plain)
- `ZTITLE` — always empty; display title derived from first line of `ZCONTENT`
- `ZFOLDER` — 0=inbox, 1=archive, 10000=trash
- `ZFLAGGED` — 0/1 boolean
- `ZHIDDEN` — 0=visible, 1=sync tombstone (skip ZHIDDEN=1 rows)
- `ZCACHED_TAGS` — `ZZZtag1ZZZtag2ZZZ` sentinel-delimited tag cache
- Tag join table: `ZMANAGEDDRAFTTAG(ZDRAFT_UUID, ZNAME, ZHIDDEN)` — authoritative
  source; `ZCACHED_TAGS` is a fallback

757 visible drafts (ZHIDDEN=0), 3145 hidden tombstones, on this install.

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§Drafts (L2811–L2817). Feasibility 🟢 high; FDA already in Trove's grant.
Schema confirmed via one-time `sqlite3 .schema` on 2026-06-15. The M6
AppleScript fallback is unnecessary; DB read is the primary path. iCloud Drive
backup folder is the M2 fallback if FDA is ungranted (not implemented — the
DB path is reliably available with FDA).

## Build notes (integration/drafts, 2026-06-15)

- Contract mode: `reuse-bound` (notes domain, `Note` type from `crate::notes`)
- Behavior: `Periodic` (hourly, same as Bear)
- No new connection — local SQLite, no auth
- Raw layer at `notes/drafts/raw/` — full fidelity, partitioned by `created`
  month (immutable key, same as Bear, prevents cross-month orphans)
- 11 tests, all green (`cargo test -p trove-core drafts::`)
- No new Cargo deps (reuses rusqlite + dirs + existing Bear infrastructure)
