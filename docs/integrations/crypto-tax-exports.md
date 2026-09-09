# Koinly / CoinTracker Exports

- **id:** `crypto-tax-exports`
- **domains:** `finance-purchases` (contract: `LineItem` — the same bound
  contract used by the Bitcoin on-chain collector, the established pioneer for
  crypto value-transfers in this store; **not** the canonical `finance/` ledger
  even though the brief originally said so — see Build notes below)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (one-time backfill CSV; no outbound API from either
  service, so no ongoing sync path exists)
- **connection:** none
- **evidence:** official-docs — in-app CSV export paths documented for
  both (Koinly: Settings → Tax Reports → Export transactions; CoinTracker:
  Portfolio → Export → CSV)
- **effort / priority:** S / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement)

## What it is

Koinly and CoinTracker are crypto tax aggregators: users wire up all their
exchanges and wallets once, and the service produces a normalized,
cost-basis-annotated transaction history. A user who has already done that
import pain holds a single clean CSV covering their *entire* crypto
history — strictly better as a Trove backfill than re-importing
exchange-by-exchange. Ongoing sync stays with direct exchange APIs and
on-chain reads; this is the seed, not the stream.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Koinly transaction CSV | free tier covers 10,000 txns | date, type (normalized), asset, amount, fee, cost basis, gains, wallet/exchange | official export path |
| CoinTracker portfolio CSV | all plans | date, type, asset, amount, exchange, cost basis | official export path |

All optional in the ledger shape; aggregator-specific columns (realized/
unrealized gains, normalized type) ride in `extra` — no special code paths.

## Access & auth

- Export mechanism only: user downloads the CSV from the service and drops
  it into the registry-driven import box. Trove never talks to either
  service (no outbound API from them anyway).
- No TCC, no credentials, no network. Standalone-clean.

## Vault mapping

- **Raw layer:** `finance/crypto-tax-exports/<source>/raw/YYYY-MM.jsonl`
  (per-source subfolders: `koinly/` and `cointracker/`). Full fidelity;
  rows are written unconditionally for all data with a parseable date.
- **Contract layer:** `finance/purchases/crypto-tax-exports/YYYY-MM.jsonl`
  (`finance-purchases` / `LineItem`). Same bound contract as the Bitcoin
  on-chain collector. `amount` = asset quantity (Received Amount for buys,
  negated Sent Amount for sells); `currency` = asset ticker (BTC, ETH, …).
  USD Net Value / cost basis / realized gains all go in `extra` at full
  fidelity. `guid` = TxHash → Transaction ID → stable SHA-256 fallback
  over `date|type|sent|received|wallet|fee|row_ordinal` (row ordinal
  prevents silent dedup of same-minute DCA fills).
- **Dedupe:** import-time dedup by guid; a later direct exchange integration
  covering the same wallet may overlap — read-time match on
  date+asset+amount+wallet resolves cross-source duplicates.

## Build plan

1. Module `crates/trove-core/src/crypto_tax_exports.rs`: `DEF` (Import);
   one registration line in `INTEGRATIONS`. (Alternative: ship as two
   format presets inside the existing csv-import header-sniff — decide at
   build time; one hub entry either way, per combine-by-provider.)
2. Two parsers (Koinly layout, CoinTracker layout) behind one def; the
   exact column sets aren't reproduced in the research doc, so finalize
   each parser against a real export file — fixtures from real exports,
   mild Needs-sample per format.
3. Ledger writes via existing `store` helpers — no contract wait.
4. Fuzzy-dedupe tests against seeded exchange rows.
5. Privacy gate: opt-in enable with explicit acknowledgement (financial
   detail).

## Build notes (2026-06-16, revised 2026-06-16)

- Implemented as `Import` behavior with auto-detection of Koinly vs CoinTracker format
  from the CSV header row (Koinly has `Received Amount`; CoinTracker has `Received Quantity`).
- Column shapes confirmed from BittyTax open-source parsers (the most reliable public
  source; koinly.io and cointracker.io support sites both block unauthenticated scraping).
- **Contract choice:** `finance-purchases` (`LineItem`), not the canonical `finance/` ledger
  the brief originally described. Reason: the Bitcoin on-chain collector already pioneers
  crypto value-transfers in `finance-purchases`, and reusing a bound contract is correct
  here. The `finance/` ledger is for bank/card transactions; it was not the right target
  despite the brief's description.
- **Amount convention (unified):** `amount` = asset quantity (Received Amount for buys,
  negated Sent Amount for sells); `currency` = asset ticker. This matches the Bitcoin
  sibling and CoinTracker. `Net Value (USD)` / cost basis / gain columns all go in `extra`
  so a reader summing `amount` across rows gets asset quantities, not mixed USD+asset totals.
- **Date parsing (comprehensive):** `parse_ts` now handles RFC 3339 / ISO 8601 with Z /
  offsets / millis, Koinly `YYYY-MM-DD HH:MM:SS UTC`, Koinly minute-precision `HH:MM UTC`,
  naive ISO datetimes (treated as UTC per BittyTax default), **CoinTracker `MM/DD/YYYY
  HH:MM:SS`** (the officially documented format — the original build used ISO dates in
  fixtures, masking a silent-zero-collection bug), and CoinTracker `MM/DD/YYYY` date-only.
- **Raw before gate:** raw rows are pushed before the contract mapping gate so full-fidelity
  data survives even when contract logic would skip a row. Rows with unparseable dates
  (cannot be partitioned by the vault store) are counted under `unparseable_dates` and the
  headline warns the user — no more silent zero-collection.
- **Guid widened:** fallback guid hash now includes `fee_amount` and a per-file row ordinal
  so same-minute identical rows (DCA fills, split fills) get distinct guids.
- Parser is `flexible(true)` so real exports with occasional short rows aren't dropped.
- 14/14 tests pass; cargo check clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Koinly CSV import | ✓ built | export from a real Koinly account; drop in import box; ledger rows + `finance/crypto-tax-exports/koinly/raw/` raw; re-import idempotent |
| CoinTracker CSV import | ✓ built | same with a CoinTracker export; raw lands in `finance/crypto-tax-exports/cointracker/raw/` |
| Dedupe vs exchange sync | — | after a direct exchange integration ships, a wallet covered by both shows each txn once (read-time match on date+asset+amount+wallet) |
| Needs-sample | flag | real exports from a live account needed to confirm exact Koinly date format and column order match production exports |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Crypto Tax
Aggregator Exports (L3915–L3921). Feasibility 🟡 medium — clean mechanism,
backfill-only value. Time-sensitivity note: as of 2026, 1099-DA means
exchanges must track and report cost basis themselves, so the population
needing third-party aggregators shrinks for straightforward cases — fine
for P2, don't promote. Direct exchange APIs + on-chain reads (Etherscan,
Blockstream) are the ongoing-sync siblings in this catalog; this entry
deliberately doesn't try to be them.
