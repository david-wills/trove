# YNAB

- **id:** `ynab`
- **domains:** `finance/` (canonical ledger — contract: **document**, the
  shape exists in code as built; the recorded write-time-dedupe exception)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll transactions with server-knowledge delta cursor)
- **connection:** `ynab` — TokenPaste (Personal Access Token the user
  generates in YNAB → Account Settings → Developer Settings; no OAuth app,
  no Trove-held credential). Not shared with other defs.
- **evidence:** official-docs — api.ynab.com/v1 (stable, well-documented
  REST API; documented ~200 req/hr rate limit)
- **effort / priority:** M / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement)

## What it is

You Need A Budget — the long-running envelope-budgeting app. Its users
manually review and categorize every transaction, so a YNAB account holds
years of high-quality, merchant-normalized, categorized financial history.
For those users, Trove gets the categorization layer for free on top of
(or instead of) raw bank sync.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transactions | all (paid app) | date, amount, payee, category, memo, account, cleared | official docs |
| Accounts | all | name, type, balance | official docs |
| Categories & budgets | all | category groups, monthly budgeted/activity | official docs |
| Scheduled transactions | all | upcoming recurring rows | official docs |

All optional in the ledger shape; YNAB-specific fields (category, budget
month, flag color) ride in `extra` — no special code paths.

## Access & auth

- REST: `GET /budgets`, `/budgets/{id}/accounts`, `/budgets/{id}/transactions`,
  `/budgets/{id}/categories`, `/budgets/{id}/months` at `https://api.ynab.com/v1`.
- Bearer PAT; the API supports `last_knowledge_of_server` delta requests —
  use it as the sync cursor.
- Rate limit ~200 req/hour — far above a personal periodic pull.
- API is read/write; Trove uses read only.
- No TCC, no local files. Standalone-clean (plain HTTPS, user-owned token).

## Vault mapping

- **Raw layer:** `finance/ynab/raw/YYYY-MM.jsonl` — API transaction objects,
  full fidelity, per-source folder alongside the ledger rows.
- **Contract layer:** canonical `finance/` ledger rows (as built for
  bank-sync/csv-import): `ts`, `amount`, `payee`, `account`, `guid` = YNAB
  transaction id; YNAB category/budget metadata in `extra`.
- **Dedupe:** write-time fuzzy dedupe against existing bank-sync rows is
  deferred (see validation matrix). YNAB rows are stored under source key
  `ynab`; a transaction present in both YNAB and SimpleFIN appears in each
  source folder. Read-time dedup by amount+date at the query layer is planned.

## Build plan

1. Module `crates/trove-core/src/ynab.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste with setup copy pointing at Developer Settings), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the official docs' example responses; parser + store +
   delta-cursor tests; fuzzy-dedupe tests against seeded bank rows.
4. Privacy gate: opt-in enable with explicit acknowledgement (financial
   detail).
5. Vault writes via existing `store` ledger helpers — no contract wait.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Transactions + accounts | ✅ built | paste a real PAT; Sync now; confirm ledger rows + `finance/ynab/raw/`; hub last-data updates |
| Dedupe vs bank-sync | 🔲 deferred | YNAB rows filed under `ynab` source key; a txn present in both YNAB and SimpleFIN appears in each source folder. Write-time fuzzy dedupe (date+amount+import_id window against bank rows) is deferred; read-time dedup at query layer is planned but not yet implemented |
| Delta cursor | ✅ built | `server_knowledge` cursor persisted per budget in `.trove/ynab-sync.json`; second sync passes it |

## Implementation notes

- **Auth:** TokenPaste (`CONNECTION` id `ynab`); token verified via `GET /user` on connect.
- **Budgets:** All budgets fetched via `GET /budgets`; transactions pulled per budget.
- **Cursor:** `server_knowledge` integer per budget (not a date window); stored in `.trove/ynab-sync.json`.
- **Amounts:** YNAB milliunits divided by 1000 → two-decimal string; outflow-negative convention preserved.
- **Deleted rows:** Included in raw layer (full fidelity); skipped in the contract layer (`deleted: true` → `None` from `map_transaction`).
- **Split transactions:** `subtransactions` array present in `extra`; parent carries the total amount.
- **Accounts:** Balance fetched from `GET /budgets/{id}/accounts`; `balance` field is running balance (milliunits), `cleared_balance` stored as `available`.
- **Tests:** 20 unit tests — milliunits conversion, field mapping, deleted filtering, cursor advancement, idempotency, no-budgets graceful path.

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §YNAB
(L3707–L3713). Feasibility 🟢 high. PAT is the simplest auth shape in the
finance catalog — no OAuth dance, no app credential. Sequencing: P2 because
SimpleFIN + csv-import already cover the transaction ledger for most users;
YNAB's value is the human-reviewed categorization for its own user base.
Lunch Money is the sibling PF-app source with the same shape — share the
fuzzy-dedupe approach.
