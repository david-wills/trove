# Actual Budget

- **id:** `actual-budget`
- **domains:** `finance/` (canonical ledger — document contract, the
  as-built shape with write-time cross-source dedupe)
- **status:** 🧪 built (fixture-tested; Periodic/hourly; local SQLite copy-then-read; opt-in/default-off)
- **unavailable_reason:** none
- **behavior:** Periodic (local SQLite copy-then-read, the iMessage
  pattern; budget zip / CSV export as the M1 alternate)
- **connection:** none (local files; no login)
- **evidence:** official-docs — SQLite schema documented at
  actualbudget.org/docs/contributing/project-details/database/ (tables
  `transactions`, `accounts`, `categories`; migration history documented).
  High confidence.
- **effort / priority:** M / P2
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · path discovery (app-support location varies by
  Electron vs. self-hosted variant — handle both, plus a manual
  pick-the-file fallback)

## What it is

A local-first, open-source envelope-budgeting app — its users are exactly
Trove's audience (privacy-minded, files-on-my-machine people). Their data
already lives on disk as SQLite: transactions with full user-applied
categorization and budget context, readable without any network or
credential.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transactions | none (open source) | date, amount, payee, notes, account, category | official schema docs |
| Accounts | none | account names/types for alias mapping | official schema docs |
| Categories/budget context | none | category tree, budget assignments | official schema docs |

Budget assignments beyond the transaction row go in `extra` (the ledger is
transaction-shaped; Trove doesn't model envelopes).

## Access & auth

- Local SQLite under the Electron app's data dir
  (`~/Library/Application Support/Actual/...`; exact path varies by
  variant). No FDA needed — it's a normal user-domain app-support dir, not
  a TCC-protected location.
- Read via **copy-then-read with rusqlite** (the established iMessage
  chat.db pattern — never open the live DB in place). The official
  `@actual-app/api` Node package is not usable from a Rust binary and is
  not needed.
- M1 alternate: Actual's own export (Settings → Export Data → zip
  containing `db.sqlite`) — same parser, hand-fed file; also covers
  self-hosted users whose DB lives on a server.

## Vault mapping

- **Raw layer:** `finance/actual-budget/` — per-source raw rows (native
  transaction shape), partitioned by month.
- **Contract layer:** rows join the canonical `finance/` ledger (the
  recorded write-time-dedupe exception). `guid` = Actual's transaction id;
  category/payee/budget detail in `extra`; cross-source fuzzy dedupe vs.
  SimpleFIN/import rows for accounts covered by both.

## Build plan

1. Module `crates/trove-core/src/actual_budget.rs`: `DEF` (Periodic;
   permission hook = "data dir found?", last-data hook from vault). No
   `CONNECTION` needed.
2. Path discovery: probe known app-support locations for both variants;
   if none found, the def's import path accepts the exported zip /
   `db.sqlite` directly (registry import box).
3. rusqlite reader after tempdir copy; watermark on transaction id/date,
   rebuildable from vault output.
4. Fixtures: construct a small `db.sqlite` from the documented schema
   (official docs carry the table shapes — no sample blocker, but verify
   against a real DB during validation since migrations evolve).
5. Privacy gate: financial detail — ships opt-in (off by default, explicit
   enable on the hub card).
6. Dedupe tests against SimpleFIN-overlap fixtures.

## As-built (Phase 4 build — 2026-06-17)

**Status:** 🧪 (built, tests green)

- **Behavior:** `Periodic` (hourly; auto-discovers `~/Library/Application Support/Actual/*/db.sqlite`; also exposes `import_one_db` for direct/zip path)
- **Contract:** canonical `finance/` ledger — `finance::Transaction` via `upsert_finance_transactions`; raw layer at `finance/actual-budget/raw/YYYY-MM.jsonl`
- **Connection:** none (local-file read; no login)
- **Cargo deps added:** none (rusqlite already present)

### Vault output

- Contract: `finance/transactions/<actual-budget-<account-name>>/<year>.jsonl` (one `Transaction` per row; `source="actual-budget"`)
- Raw: `finance/actual-budget/raw/YYYY-MM.jsonl` (full join: transaction + payee + category + account names; `amount_milliunits` preserved)
- Cursor: `.trove/actual-budget-sync.json` (per-db-file rowid watermark; rebuildable)

### Schema mapping (verified against actualbudget.org AQL schema)

| Actual field | Contract field | Notes |
|---|---|---|
| `id` (UUID) | `Transaction.id` | stable dedupe key |
| `date` (YYYYMMDD int) | `Transaction.posted` (YYYY-MM-DD) | local midnight ts for partition |
| `amount` (milliunits) | `Transaction.amount` (decimal string) | signed; outflows negative |
| `payee` → name join | `Transaction.payee` | |
| `notes` | `Transaction.description` (or payee if empty) | |
| `category` → name join | `Transaction.category` | |
| parent split rows | skipped | child rows have the real amounts |
| tombstone=1 rows | skipped | soft-deleted in Actual |

### Design decisions

- Amounts stored as signed decimal strings (e.g. "-42.50"), never floats — consistent with SimpleFIN/CSV-import contract
- Milliunits (1 dollar = 1000 units) divided by 1000; 2 decimal places
- Off-budget accounts included in the import (flagged in raw `account_offbudget`)
- Schema-adaptive reads via `PRAGMA table_info` — survives Actual migrations that add/remove columns
- Path discovery using `dirs::data_local_dir()` (→ `~/Library/Application Support/`) + one-level glob; self-hosted users hand-feed via `import_one_db`

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Local DB sync | 🧪 unit-tested (synthetic db.sqlite fixtures) | install Actual, add test transactions, enable the card, Sync now; confirm ledger rows + hub last-data |
| Incremental cursor | 🧪 unit-tested | second sync on same DB writes 0 new rows; cursor advances only on success |
| Schema-adaptive | 🧪 unit-tested | reader handles minimal schema (missing optional columns) without error |
| Real-user data | — | needs a real Actual user; any user's run promotes this slice |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Actual
Budget (L3883–L3889). Feasibility 🟡 medium (niche but growing; strong
audience overlap), recommendation "build later" — hence P2. Schema is
stable per docs but the app migrates; pin the reader to the documented
tables and fail soft (surface "schema changed" rather than mis-parse).
Self-hosted variant means the local-dir probe alone isn't sufficient —
the export-zip path is the universal fallback, not an afterthought.
