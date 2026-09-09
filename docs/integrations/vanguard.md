# Vanguard

- **id:** `vanguard`
- **domains:** `finance/` (contract: **document** — the canonical ledger as
  built; Phase 3 writes the spec page) + `finance/holdings/` (contract:
  **Phase 3 pending** — holdings-snapshot shape)
- **status:** 🧪 built (Needs-sample: column names unconfirmed; parser parked until a real export confirms the shape — live dispatch falls through to the generic CSV mapper meanwhile)
- **unavailable_reason:** none
- **behavior:** Import (CSV — Vanguard preset in `finance/import.rs`)
- **connection:** none (user downloads files from investor.vanguard.com)
- **evidence:** portal export path documented in the research doc (transaction
  history, 18-month range per download; holdings via Portfolio → Export); no
  official consumer API. Exact column formats undocumented — sample-required.
- **effort / priority:** S / P1
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-sample (transaction + holdings CSV columns — column
  names are scaffolded from community reports; verify against a real export)

## What it is

The largest US retirement/mutual-fund custodian — Vanguard dominates 401k
and IRA assets, so for many users it holds the bulk of their net worth.
There is no official consumer API (Plaid can reach Vanguard but requires
developer-held keys — the catalogued hard block), so the clean, well-
structured portal CSV export is the path. Research rates the export quality
high.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transaction history | none (incl. 401k accounts) | trades, dividends, contributions (columns unconfirmed) | research doc; Needs-sample |
| Holdings snapshot | none | positions per account (format unconfirmed) | research doc; Needs-sample |

All optional in the contract (omit-if-empty); no tier code paths.

## Access & auth

- Transactions: investor.vanguard.com → My Accounts → Transaction History →
  Download → CSV. **18-month range per download** — 2 files for ~3 years of
  history; onboarding copy says so.
- Holdings: Portfolio → Export (different format — second preset).
- 401k accounts held at Vanguard export the same way.
- No auth, no TCC, no network. Standalone-clean.

## Vault mapping

- **Raw layer:** imported files preserved under `finance/imports/` per the
  existing import pipeline; holdings raw under `finance/holdings/vanguard/`.
- **Contract layer:** transaction rows normalize into the canonical finance
  ledger (the recorded write-time-dedupe exception); fund-specific fields
  (symbol/fund, shares, share price) ride in `extra` until the ledger spec
  page formalizes a trade sub-shape. Position snapshots land in
  `finance/holdings/` once the Phase 3 holdings-snapshot contract is
  drafted — trades stay in the ledger.
- **Dedupe:** the 18-month windows overlap when users download generously —
  guid from (account, trade date, fund/symbol, type, amount) makes
  re-imports and overlaps idempotent.

## Build plan

1. **Parser-last** for both presets — column formats are undocumented;
   flagged Needs-sample. Generic header-sniff CSV import is the interim
   route.
2. `crates/trove-core/src/vanguard.rs` `DEF` (Import) + one registration
   line; card copy: "export each 18-month window and drop them all here."
3. Fixtures from the first real samples (a brokerage account AND a 401k
   export, which may differ); overlap-window dedupe test.
4. Holdings preset writes raw first; contract rows wait on the Phase 3
   holdings-snapshot shape.
5. Privacy: financial detail — standard finance opt-in acknowledgement.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Transaction import | 🧪 built (Needs-sample) | download a real 18-month Transaction History CSV; drop on the import box; confirm ledger rows + hub last-data; import two overlapping windows → zero dupes. If the file is rejected, confirm column names and update `vanguard_columns` in `finance/import.rs` |
| Holdings import | — | needs a real Portfolio export sample before the parser is written |
| 401k coverage | — | repeat transaction validation with a 401k account export |

## Build notes (2026-06-16)

- `Behavior::Import` wired; `IMPORT_SPEC` calls `finance_import_csv`.
- **PARSER IS PARKED (Needs-sample)**: the `vanguard_columns` recognizer and
  `import_vanguard` function exist in `crates/trove-core/src/finance/import.rs`
  as a scaffold but are NOT wired into the live dispatch path. Files fall
  through to the generic `detect_mapping` path in `finance_import_csv`. The
  Vanguard-specific dispatch block is present but commented out — re-enable it
  only after a real export confirms the column shape.
- Scaffold column recognizer: `Trade Date` + `Net Amount` (or `Amount`) +
  `Transaction Type` is the mandatory signature. Optional scaffolded columns:
  Transaction Description (unconfirmed), Symbol, Shares, Share Price,
  Principal Amount (or `Gross Amount` for MF variant), Commission Fees,
  Accrued Interest, Settlement Date (or `Process Date` for MF variant),
  Account Number, Account Name.
- Scaffold raw layer: `finance/vanguard/raw/<account>/<year>.jsonl` — verbatim
  original CSV column names and values, all columns preserved regardless of
  whether the mapper knows them.
- Contract (when activated): `finance/transactions/<account>/YYYY.jsonl` via
  the existing `upsert_finance_transactions` path; source="vanguard".
- Scaffold dedupe: `hash(account, trade_date, net_amount, transaction_type,
  symbol, occurrence)` — 18-month overlaps are idempotent.
- Multi-account export detected when `Account Number` column is present; rows
  self-route to separate vault accounts by account number.
- Multi-section exports (holdings block before transactions block, reported by
  community): NOT YET HANDLED. Must be addressed before re-wiring dispatch —
  scan for the transaction header row rather than assuming row 1.
- Known re-wiring checklist: (1) confirm all column names against a real
  Transaction History CSV; (2) confirm multi-section layout; (3) re-enable the
  commented dispatch block in `finance_import_csv`; (4) drop Needs-sample flag.

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Vanguard CSV
Export (L3763–L3769). Feasibility 🟢 high. The 18-month window is the most
generous of the big three no-API brokerages (Fidelity is 90 days). Plaid's
Vanguard reach is irrelevant here (developer-key hard block). Pairs with
Fidelity/Robinhood presets — the three share the brokerage-trade `extra`
conventions, so build them against the same ledger fields.
