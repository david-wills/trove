# Cash App

- **id:** `cash-app`
- **domains:** `finance/` (canonical ledger — contract status: **document**;
  the shape already exists in code, Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (CSV export, delivered by email; re-runnable)
- **connection:** none — file import, no login. Cash App has no public API
  and, like Venmo, is unreachable by any aggregator.
- **evidence:** official-docs — web export path documented: cash.app →
  Activity → ⋯ → Export Transactions → All Time → CSV, emailed within
  minutes. Column layout not carried in the research evidence — fixture
  from a real export before shipping.
- **effort / priority:** S / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement on import)

## What it is

Cash App account history — peer payments, Cash App Card purchases, and
Bitcoin buys/sells — via the official all-time CSV export. Another
aggregator walled garden: bank feeds only see net settlements, so the
export is the only complete record. Cash App retains full history for
active accounts, making this a clean one-file backfill plus occasional
re-export.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Peer payments | all accounts | date, counterparty, amount, status, txn id | official export path |
| Cash App Card transactions | if user has the card | merchant, amount, date | research notes |
| Bitcoin buys/sells | if used | BTC amount, USD amount, txid | research notes |

All optional in the ledger row; a user without the card or BTC simply has
fewer row types. No special code paths.

## Access & auth

- Export: **web only** — cash.app → Activity → three-dot menu → Export
  Transactions → All Time → CSV. The mobile app has no export. The file is
  **emailed** to the registered address within minutes rather than
  downloaded directly — minor friction the onboarding copy must explain.
- No auth in Trove, no TCC, no network. Standalone-clean by construction.
- No public API exists — recorded so the card can say why there's no Sync
  button.

## Vault mapping

- **Raw layer:** `finance/` is the recorded write-time-dedupe exception —
  the import dedupes into shared per-account files: the Cash App balance
  registers in `finance/accounts.jsonl`, rows in
  `finance/transactions/<account>/<year>.jsonl`.
- **Contract layer:** canonical ledger rows; `guid` = Cash App transaction
  ID; signed amounts per vault convention; transaction type, BTC
  amount/txid, and card-merchant detail in `extra`. Bank settlements and
  on-chain BTC rows (Blockstream provider) coexist in their own accounts —
  reconciliation (txid join for BTC) at read time, never write time.
- **Dedupe:** transaction ID as `guid`; re-import appends nothing.

## Build plan

1. Cash App preset in the existing finance import (`finance/import.rs`
   header-sniff family) + thin `cash_app.rs` DEF (Import; id `cash-app`)
   for the hub card and import box. Registration line in `INTEGRATIONS`.
2. **Parser-last:** export-path evidence only, no schema evidence — obtain
   a real emailed export and build fixtures from it (peer in/out, card
   purchase, BTC buy/sell rows).
3. Tests: sign convention, guid dedupe on re-import, BTC txid preserved in
   `extra` for the read-time join; unique temp dirs.
4. Onboarding copy: web-only export + email delivery (the documented
   friction), "All Time" range tip.
5. Privacy gate: opt-in acknowledgement (financial detail).

## Build notes (2026-06-16)

- DEF built: `Behavior::Import`, `default_on: false`.
- Parser WIRED: column schema confirmed from `SolidX/FinanceExportTools` `CashAppExportMapper.cs`
  and cross-referenced real-export samples. The exact header row:
  `Transaction ID, Date, Transaction Type, Currency, Amount, Fee, Net Amount, Asset Type,
  Asset Price, Asset Amount, Status, Notes, Name of sender/receiver, Account`
- `cash_app_columns` recognizer + `import_cash_app` added to `finance/import.rs`.
- Dispatch wired in `finance_import_csv` (before Fidelity).
- Date format: `"YYYY-MM-DD HH:MM:SS TZ"` — `parse_date` extended to strip time+tz suffix.
- Guid: `"cash-app-<Transaction ID>"` — stable, unique per row. Hash fallback only for blank IDs.
- Sign convention confirmed: outflows already negative, no flip needed.
- Amount: gross `Amount` stored; `Fee` + `Net Amount` preserved in `extra`.
- BTC fields: `asset_type`, `asset_price`, `asset_amount` preserved in `extra` for
  read-time reconciliation against the Blockstream provider.
- Raw layer: `finance/cash-app/raw/<account>/YYYY.jsonl` — verbatim row, all columns,
  unconditional, written by `import_cash_app`.
- Generic description fallback extended: `"notes"`, `"note"`, `"name of sender/receiver"`
  added to `detect_mapping` description candidate list.
- `finance_import_csv` now bails with a clear error (not silent Ok) when all rows skip.
- 12 tests pass covering: hub card, params, last_data, import count, guid stability, amounts+signs,
  BTC extra fields, date format, idempotency, raw layer, no-account default, last_data stamp.
- `cargo check` and `cargo test -p trove-core cash_app::` green (12/12).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export import | 🧪 built | request a real export on cash.app web, fetch the emailed CSV; drag into the import box; confirm account + rows in `finance/`, hub last-data |
| Re-run safety | 🧪 built | import the same file twice; row count unchanged (idempotent on Transaction ID) |
| BTC overlap | — | a user with Cash App BTC + the `bitcoin` provider enabled: read-time view doesn't double-count (txid join) |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Cash App
Export (L3819–L3825). Feasibility 🟢 high — "CSV export covers full
account lifetime." Email delivery instead of direct download is the one
odd step; document it rather than fight it. Bitcoin rows overlap the
on-chain Blockstream source — per the vault's overlapping-sources rule,
both write their own accounts and the txid dedupe is a read-time concern.
