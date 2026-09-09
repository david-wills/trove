# Lunch Money

- **id:** `lunch-money`
- **domains:** `finance/` (canonical ledger — contract: **document**, the
  shape exists in code as built; the recorded write-time-dedupe exception)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll transactions; date-window cursor)
- **connection:** `lunch-money` — TokenPaste (access token from the Lunch
  Money app's Developers page; user-generated, no Trove app credential).
  Not shared with other defs.
- **evidence:** official-docs — lunchmoney.dev (v1 stable; v2 alpha at
  alpha.lunchmoney.dev, GA expected 2026; typed SDK at
  github.com/lunch-money/lunch-money-js-v2)
- **effort / priority:** M / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement)

## What it is

Lunch Money is a web-first personal-finance app with the best-in-class API
among PF apps (the research doc's words). Its users carry years of
categorized transactions, tags, recurring-item detection, and budget data —
and its Plaid-connected accounts give near-real-time bank sync that Trove
can pull through the user's own token without touching Plaid itself.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transactions | all (paid app) | date, amount, payee, category, tags, notes, account, status | official docs |
| Accounts | all | name, type, balance, institution | official docs |
| Categories | all | category tree, exclude-from-budget flags | official docs |
| Recurring items | all | detected recurring transactions/subscriptions | official docs |
| Budgets | all | per-category monthly budget data | official docs |

All optional in the ledger shape; tags, recurring linkage, and budget
metadata ride in `extra` — no special code paths.

## Access & auth

- REST: `GET /v1/transactions`, `/v1/accounts` (`assets` + `plaid_accounts`),
  `/v1/categories` at lunchmoney.dev; Bearer access token.
- v2 (alpha, GA expected 2026) restructures endpoints — build on v1 stable,
  note v2 for a later migration.
- No documented punitive rate limit at personal-pull volumes.
- No TCC, no local files. Standalone-clean (plain HTTPS, user-owned token).

## Vault mapping

- **Raw layer:** `finance/lunch-money/raw/YYYY-MM.jsonl` — API transaction
  objects, full fidelity, per-source folder alongside the ledger rows.
- **Contract layer:** canonical `finance/` ledger rows (as built): `ts`,
  `amount`, `payee`, `account`, `guid` = Lunch Money transaction id;
  category/tags/recurring metadata in `extra`.
- **Dedupe:** ledger write-time dedupe applies — Lunch Money mirrors
  bank accounts (via its Plaid connections), so rows must fuzzy-match
  against SimpleFIN/csv-import rows for the same accounts rather than
  double-count.

## Build plan

1. Module `crates/trove-core/src/lunch_money.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste with setup copy pointing at the Developers
   page), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from lunchmoney.dev example responses; parser + store + cursor
   tests; fuzzy-dedupe tests against seeded bank rows.
4. Privacy gate: opt-in enable with explicit acknowledgement (financial
   detail).
5. Vault writes via existing `store` ledger helpers — no contract wait.

## Build notes (2026-06-17)

- Contract: canonical `finance/` ledger (`Transaction` + `resolve_finance_account` + `upsert_finance_transactions` + `record_finance_balance`), same machinery as SimpleFIN. Not `finance-purchases/LineItem` — Lunch Money holds bank transactions, not itemized purchases.
- Raw layer: `finance/lunch-money/raw/YYYY-MM.jsonl` (store stream, Partition::Month, deduped by numeric id).
- Account namespacing: `asset-<id>` for manual assets, `plaid-<id>` for Plaid-linked accounts — avoids collisions across the two APIs.
- Amounts follow the vault-wide outflow-negative convention: the pull requests `debit_as_negative=true` so the API returns expenses as negative strings (e.g. "-42.50") and income/credits as positive. Consistent with SimpleFIN, CSV import, and all other finance sources.
- Cursor: `.trove/lunch-money-sync.json`, `last_date` (YYYY-MM-DD); 7-day overlap window; 2-year first-sync backfill.
- Unlinked transactions (no asset_id, no plaid_account_id) appear in raw only; not filed in contract ledger.
- Connection requires a `&crate::lunch_money::CONNECTION,` line in integrations.rs CONNECTIONS (new_connection=true).
- Tests: 21 passed, 0 failed (includes 2 sign-convention assertion tests).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Transactions + accounts | built | paste a real token; Sync now; confirm ledger rows + `finance/lunch-money/raw/`; hub last-data updates |
| Dedupe vs bank-sync | — | account present in both Lunch Money and SimpleFIN shows each transaction once |
| Recurring/tags in extra | built | a tagged + recurring transaction lands with both visible in `extra` |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Lunch Money
(L3715–L3721). Feasibility 🟢 high; active development, strong developer
community. P2 for the same reason as YNAB: the ledger is already covered;
the win is the user's own categorization layer. Watch the v2 GA in 2026 —
v1 is the build target until then. Sibling source to YNAB; build them with
the same dedupe machinery.
