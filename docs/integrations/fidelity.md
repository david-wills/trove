# Fidelity

- **id:** `fidelity`
- **domains:** `finance/` (contract: **document** — the canonical ledger as
  built; Phase 3 writes the spec page) + `finance/holdings/` (contract:
  **Phase 3 pending** — holdings-snapshot shape)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (drag-and-drop CSV via the generic import pipeline; a
  Fidelity preset, like Chase/Copilot)
- **connection:** none (no login — user downloads files from fidelity.com)
- **evidence:** portal download path documented in the research doc, including
  the activity CSV's column list; no consumer API exists (Fidelity dropped OFX
  January 17, 2026). Holdings export format is publicly documented (usefidelity.com)
  but the finance-holdings contract is an unbound Phase-3 draft; holdings parser
  is parked until the contract binds.
- **effort / priority:** S / P1
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-sample (holdings/Portfolio export format)

## What it is

Largest US retail brokerage by assets. Trade history, settlements, and
position snapshots for brokerage/retirement accounts. Fidelity offers no
consumer-facing API and ended OFX export in January 2026 — the portal CSV
download is the only programmatic path, which makes a clean import preset
the complete solution rather than a stopgap.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Trade/activity history | none (any account) | Run Date, Action, Symbol, Description, Type, Quantity, Price ($), Commission ($), Fees ($), Amount ($), Settlement Date (optional) | confirmed by TradingDiaryPro, TradeLog, Moneydance samples, and community sources |
| Holdings snapshot | none | positions per account (format unconfirmed) | research doc; Needs-sample |

All optional in the contract (omit-if-empty); no tier code paths.

## Access & auth

- Activity CSV: fidelity.com → Accounts & Trade → Activity & Orders →
  History → Download. **90-day window per download** — ~4 files/year for
  full history; onboarding copy must say "download each 90-day window and
  drop them all here."
- Holdings CSV: Accounts → Portfolio → Download (different format — second
  preset).
- No auth, no TCC, no network. Standalone-clean by construction.

## Vault mapping

- **Raw layer:** verbatim rows written to `finance/fidelity/raw/<vault-account-id>/<year>.jsonl`
  (one JSONL file per account per calendar year, appended on each import).
  Holdings raw will land under `finance/holdings/fidelity/` when the holdings
  contract binds.
- **Contract layer:** activity rows normalize into the canonical finance
  ledger (the recorded write-time-dedupe exception — cross-source dedupe
  against SimpleFIN/statement rows applies). Trades stay in the ledger;
  position snapshots land in `finance/holdings/` once the holdings-snapshot
  contract is drafted (Phase 3). Brokerage-specific fields (symbol,
  quantity, price, commission) ride in `extra` until the ledger spec page
  formalizes a trade sub-shape.
- **Dedupe:** ledger's existing fuzzy/cross-source matcher; within-file
  guid from (account, run_date, symbol, action_normalized, amount, occurrence).

## Build plan

1. Fidelity activity preset in the CSV importer (header-sniff signature from
   the documented column list); signs checked against vault convention like
   the Copilot preset.
2. `crates/trove-core/src/fidelity.rs` `DEF` (Import) so the hub shows a
   Fidelity card with the 90-day-window onboarding copy; register one line
   in `INTEGRATIONS`.
3. Holdings preset **parser-last** — flagged Needs-sample; until a real
   Portfolio export lands, the card documents the path and accepts the file
   into raw only.
4. Fixtures: synthetic activity CSV from the documented columns; tests for
   parse, sign convention, dedupe vs an overlapping bank row.
5. Privacy: financial detail — ships behind the standard finance opt-in
   acknowledgement.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Activity import | 🧪 built | download a real 90-day History CSV; drop on the import box; confirm `finance/transactions/<account>/YYYY.jsonl` rows + `finance/fidelity/raw/<account>/YYYY.jsonl`; re-import same file → zero dupes |
| Holdings import | 🚧 Needs-sample | needs a real Portfolio export sample (any Fidelity user) before the parser is written |

## Build notes (2026-06-15)

- **Status:** 🧪 (built, tests pass, cargo check green)
- **Module:** `crates/trove-core/src/fidelity.rs`
- **CSV preset added to:** `crates/trove-core/src/finance/import.rs` (`fidelity_columns`, `import_fidelity` method)
- **Contract mode:** The activity CSV writes `finance.Transaction` rows via the existing `upsert_finance_transactions` path (the finance ledger, NOT `finance-purchases.LineItem` — trades are ledger events, not itemized retail purchases). Raw layer: `finance/fidelity/raw/<vault-account-id>/YYYY.jsonl`.
- **Real export shape:** per-account header is `Run Date,Action,Symbol,Description,Type,Quantity,Price ($),Commission ($),Fees ($),Accrued Interest ($),Amount ($),Cash Balance ($),Settlement Date`. `Run Date` is always populated (primary date). `Settlement Date` is blank for dividends/reinvestments/transfers and stored in `extra` only when present. The All-Accounts export prepends `Account Number,Account Name`. Action strings are full phrases (`YOU BOUGHT`, `DIVIDEND RECEIVED`, etc.) — normalized to short verbs for description, raw value kept in `extra["action"]`.
- **Sign convention confirmed:** BUYS and fees negative, SELLS and DIVIDENDS positive — matches vault convention, no flip needed.
- **Holdings parser parked:** finance-holdings contract is an unbound Phase-3 draft; the parser will land once the contract binds. The Portfolio CSV format is in fact publicly documented (usefidelity.com), but parsing it before the contract is ready would be raw-only.
- **No new connection/deps:** keyless CSV import, no OAuth, no new crates.

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Fidelity
Brokerage CSV Export (L3747–L3753). Feasibility 🟢 high. The research doc's
at-a-glance row confirms no individual-consumer API and the OFX cutoff
(2026-01-17), so don't burn time hunting for a live-sync path. Complements
Schwab (API) for users with both. SnapTrade would cover Fidelity live but is
catalogued unavailable (developer-held keys).
