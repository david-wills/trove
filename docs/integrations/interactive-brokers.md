# Interactive Brokers

- **id:** `interactive-brokers`
- **domains:** `finance/` (trades/dividends/corp-actions → `finance-purchases` contract,
  `finance/purchases/interactive-brokers/YYYY-MM.jsonl`) · `finance/interactive-brokers/raw/`
  (full-fidelity raw) · `finance/interactive-brokers/positions/` (holdings snapshot, raw-only,
  finance-holdings draft)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily poll of a user-defined Flex Query via the two-step Flex Web Service)
- **connection:** `interactive-brokers` — TokenPaste (composite `<token>|<query_id>` pasted from
  Client Portal → Settings → Reports & Statements → Flex Queries). No Trove-held app credential.
  Not shared with other defs.
- **evidence:** official-docs — IBKR Flex Web Service v3 (two-step HTTP API; confirmed field
  names via ibflex/Types.py — the canonical open-source parser for the Flex format)
- **effort / priority:** M / P1
- **needs:** Needs-login (real IBKR account for live validation) · finance-holdings contract
  not yet ratified (positions parked raw-only, Needs-David)

## What it is

Interactive Brokers — the brokerage of choice for active traders and international users; covers
equities, options, futures, forex, and crypto. Its Flex Query export is the gold standard for
brokerage data: individual fills, commissions, lot-by-lot cost basis, wash sales, dividends,
corporate actions — detail exceeding what any aggregator returns.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Trades/fills | all accounts | fill-level date, symbol, qty, price, commission, net cash | official docs + ibflex/Types.py |
| Cash transactions (dividends, interest, fees) | all | amount, type, symbol, description | ibflex/Types.py confirmed |
| Corporate actions (splits, mergers) | all | actionID, quantity, amount, type | ibflex/Types.py confirmed |
| Positions (holdings) | all | symbol, position, markPrice, positionValue, costBasisPrice | ibflex/Types.py (raw-only, holdings pending) |

The Flex Query is user-defined — the user picks which sections it contains. All sections optional;
missing sections yield no rows (graceful). Two-step API: `SendRequest` (returns reference code) →
`GetStatement` (poll until ready; codes 1019/1018 = retry with backoff, up to 12 attempts × 5s).

## Access & auth

- TokenPaste: `<token>|<query_id>`. Both come from Client Portal.
- Token: Client Portal → Settings → Reports & Statements → Flex Queries → Generate Token.
- Query id: appears in the Flex Query list after creating a query.
- Preferred sections checklist: Trades, Cash Transactions, Corporate Actions, Open Positions.
  Format = XML (richer typing than CSV).
- No TCC, no local files. Standalone-clean (user-owned token, no Trove app registration).

## Vault mapping

- **Raw layer (unconditional):** `finance/interactive-brokers/raw/YYYY-MM.jsonl`
  All record types (Trade, CashTransaction, CorporateAction) serialized with full fidelity.
  Partitioned by the record's date field (tradeDate / dateTime / reportDate).

- **Contract layer (trades + dividends + corp actions → `finance-purchases.LineItem`):**
  `finance/purchases/interactive-brokers/YYYY-MM.jsonl`
  - `guid` = prefixed stable id: `trade:<transactionID>`, `cash:<transactionID>`,
    `corp:<actionID>`.
  - `ts` = trade/event date as local RFC3339.
  - `merchant` = "Interactive Brokers" (the brokerage).
  - `item` = e.g. "Buy AAPL", "Sell MSFT", "Dividend AAPL", "FS: AAPL" (corp action type + symbol).
  - `amount` = `netCash` (Trades), `amount` (CashTransactions / CorporateActions).
  - `currency` = from the record.
  - `extra` carries: `buy_sell`, `quantity`, `trade_price`, `commission`, `proceeds`,
    `transaction_id`, `account_id`, `exchange`, `description`, `isin`, `cusip`,
    `asset_category`, `action_type`, `transaction_type`, etc. — nothing dropped.
  - Deduped by `guid` on re-pull (overlapping Flex windows are idempotent).

- **Positions (raw-only, finance-holdings draft):**
  `finance/interactive-brokers/positions/YYYY-MM-DD.jsonl`
  One line per position per day (last-write-of-day wins if synced multiple times). Full fidelity:
  symbol, position, markPrice, positionValue, costBasisPrice, costBasisMoney, fifoPnlUnrealized,
  percentOfNAV, accountId, currency, assetCategory, side.
  **Parked behind the `finance-holdings` contract ratification (Needs-David).**

## Build notes

- Module: `crates/trove-core/src/interactive_brokers.rs` (replaces the Phase-2 NotWired stub).
- CONNECTION: new `pub static CONNECTION` added (TokenPaste, `id = "interactive-brokers"`);
  registered with one line in `CONNECTIONS` in `integrations.rs`.
- No new Cargo deps: `quick-xml` was already in Cargo.toml.
- Field names confirmed from `ibflex/Types.py` (the open-source canonical parser for the Flex
  format) before writing the parser. XML attributes match exactly: `transactionID`, `tradeDate`,
  `tradeTime`, `buySell`, `symbol`, `quantity`, `tradePrice`, `tradeMoney`, `proceeds`, `netCash`,
  `ibCommission`, `exchange`, `isin`, `cusip`, `dateTime`, `amount`, `actionID`, etc.
- `parse_ibkr_date_to_local` handles: `"YYYY-MM-DD"`, `"YYYYMMDD"`, `"YYYY-MM-DD;HH:MM:SS"`
  (dateTime with IBKR's semicolon separator), and falls back gracefully on unknowns.

## Validation matrix

| Capability | Status | How to validate |
|---|---|---|
| Trades | ✅ fixture-tested | XML fixture with Trade element; maps to LineItem with Buy/Sell label, netCash amount, commission in extra |
| Cash (dividends) | ✅ fixture-tested | CashTransaction type="Dividends" → LineItem "Dividend AAPL" |
| Corporate actions | ✅ fixture-tested | CorporateAction type="FS" → LineItem "FS: AAPL" |
| Positions (raw) | ✅ fixture-tested | OpenPosition → no LineItem (None); snapshot file written to positions/ |
| Dedupe on re-pull | ✅ tested | Second process_xml() with same XML returns 0 new rows |
| Retry on 1019 | ✅ tested | Mock returns 1019 first, then XML; fetch_flex_statement retries |
| Connection | ✅ tested | def_status shows Query ID; token-paste method registered |
| Live validation | ❌ Needs-login | Paste real token|query_id; Sync now; check vault files |
| Holdings positions | ❌ Needs-David | Blocked on finance-holdings contract ratification |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Interactive Brokers Flex Query API.
Feasibility 🟢 high. The two-step API is unusual but well-documented and used by many portfolio tools.
Pairs with Schwab as the two native-brokerage P1s. The `ibflex` open-source library (Python) served
as evidence of the exact field names — its `Types.py` documents every attribute for Trade,
CashTransaction, CorporateAction, and OpenPosition, confirmed against the IBKR Activity Flex Query
Reference guide.
