# Ethereum Wallet (Etherscan)

- **id:** `ethereum`
- **domains:** `finance/` (canonical ledger — contract status: **document**;
  the shape already exists in code, Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll per-address transaction lists; watermark
  cursor on block number)
- **connection:** `ethereum` — TokenPaste (free Etherscan API key from
  etherscan.io/myapikey; no payment, no OAuth). Not shared with other defs.
- **evidence:** official-docs — Etherscan API v2 (api.etherscan.io/v2/api),
  documented endpoints, rate limits, and multi-chain support
- **effort / priority:** M / P1
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement; additionally, querying discloses wallet addresses to
  Etherscan — say so on enable) · Needs-login (validation only — an
  Etherscan key + a real address; build proceeds from documented shapes)

## What it is

On-chain transaction history for Ethereum and EVM-compatible chains
(Polygon, Arbitrum, Optimism, Base, …) via the Etherscan API. The
blockchain is public: the user supplies wallet **addresses**, never private
keys — nothing secret ever touches Trove. Paired with the Bitcoin/Esplora
integration this covers ~95% of crypto users' wallet activity.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Normal transactions | free key | hash, from/to, value, gas, timestamp, block | official docs (`action=txlist`) |
| ERC-20 token transfers | free key | token contract/symbol/decimals, from/to, value | official docs (`action=tokentx`) |
| Internal transactions | free key | contract-initiated value moves | official docs (`action=txlistinternal`) |
| Other EVM chains | same free key | same shapes via `chainid` param | official docs (API v2) |
| ENS resolution | free key | name ↔ address | official docs |

All optional in the ledger row; token transfers and internals simply add
rows. No tier code paths — free tier is adequate for personal history.

## Access & auth

- REST: `https://api.etherscan.io/v2/api?chainid=1&module=account&action=txlist&address={addr}`;
  API key as query param. Free key, no payment.
- Rate limits: 5 calls/sec free tier; **as of July 2026 free tier returns
  max 1,000 records/request** (down from 10,000) — paginate for large
  wallets.
- No TCC, no local files. Standalone-clean (plain HTTPS, user-owned key).
  Privacy: every query leaks the queried addresses to Etherscan — this is
  the integration's only networked path and the enable copy must disclose
  it. Self-hosted Erigon/Geth is beyond scope.

## Vault mapping

- **Raw layer:** `finance/` is the recorded write-time-dedupe exception —
  no per-source raw folder; each wallet address registers as an account in
  `finance/accounts.jsonl` and rows land in
  `finance/transactions/<account>/<year>.jsonl`.
- **Contract layer:** canonical ledger rows: `guid` = tx hash (+ log index
  for token transfers so one hash can carry several rows), ts from block
  timestamp, amount signed by direction relative to the user's address,
  counterparty = other address (ENS name when resolvable); chain id, gas,
  token contract in `extra`.
- **Dedupe:** tx hash(+index) as `guid`; cursor (last block per
  address/chain) in `.trove/ethereum-sync.json`, rebuildable by scanning
  the output files.

## Build plan

1. Module `crates/trove-core/src/ethereum.rs`: `DEF` (Periodic), `CONNECTION`
   (TokenPaste: key label/help per the SimpleFIN affordance rule), `pull`
   hook for Sync-now. Address list is per-user config (addresses, chains).
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from the documented example responses (txlist, tokentx,
   txlistinternal; empty-wallet and paginated variants); parser + ledger +
   cursor tests, unique temp dirs.
4. Pagination against the 1,000-record cap; value decoding (wei → decimal
   string, token decimals) — never floats for money.
5. Privacy gate: opt-in with explicit acknowledgement (financial detail +
   address disclosure to Etherscan).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Normal + token txs | ✅ built (unit tests) | paste a free Etherscan key, add a real address; Sync now; confirm rows in `finance/purchases/ethereum/` and hub last-data |
| Multi-chain | ✅ built (cursor key includes chainid) | paste same address with @137 appended; confirm separate cursor keys, no cross-chain guid collisions |
| Large-wallet pagination | ✅ built (drain loop, 1000/page) | an address with >1,000 records; confirm complete history, no duplicates on re-sync |
| Dedup on re-sync | ✅ tested | re-running with same rows writes 0 new rows; files are byte-identical |

## Build notes (2026-06-16)

- Module: `crates/trove-core/src/ethereum.rs` — Periodic pull, TokenPaste connection.
- Connection format: `api_key|addr1,addr2,...@chainid` (chainid optional, defaults to 1).
- Three endpoint types: `txlist`, `tokentx`, `txlistinternal` — all drained per address.
- Cursor: per `address:chainid` key → last block number drained; stored in `.trove/ethereum-sync.json`.
- Contract: `finance/purchases/ethereum/YYYY-MM.jsonl` (LineItem); raw: same path under `raw/`.
- guid disambiguators: normal txs use `hash`; token transfers use `hash:transactionIndex`; internals use `hash:int:traceId`.
- 18 unit tests pass. No new crate deps. `&crate::ethereum::CONNECTION` added to CONNECTIONS.

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Ethereum /
EVM Blockchain (Etherscan API) (L3787–L3793). Feasibility 🟢 high — "the
canonical way to get Ethereum wallet history." Addresses only, never keys.
Free-tier record cap drops to 1,000/request July 2026 (time-relevant to
pagination design, not to viability). Koinly/CoinTracker CSV imports
(`crypto-tax-exports`) cover one-time normalized backfill with cost basis;
ongoing sync stays here. Solana (Helius) deferred — paid plan required.
