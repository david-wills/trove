# Coinbase

- **id:** `coinbase`
- **domains:** `finance/` (contract: **document** — the canonical ledger as
  built; Phase 3 writes the spec page) + `finance/holdings/` (contract:
  **Phase 3 pending** — holdings-snapshot shape)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll for new transactions; watermark cursor) +
  Import fallback (Taxes-page CSV for backfill)
- **connection:** `coinbase` — TokenPaste (personal read-only API key +
  secret, HMAC-signed requests; user generates at coinbase.com → Settings →
  API — no Trove app credential). Not shared with other defs.
- **evidence:** official API docs — api.coinbase.com/v2 (`/transactions`,
  `/accounts`, `/orders`), personal-key model confirmed; Taxes-page CSV
  export documented as fallback
- **effort / priority:** M / P1
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-login (validation only — build proceeds from
  documented shapes)

## What it is

The largest US crypto exchange. Buys, sells, sends, receives, staking, and
Advanced Trade (ex-Pro) orders. As of 2026 US exchanges must issue Form
1099-DA, so Coinbase necessarily keeps full transaction history — and both
the personal-key REST API and the tax CSV expose all of it. The cleanest
crypto-exchange integration: ongoing API sync plus a one-shot CSV backfill.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transactions | all accounts | buys/sells/sends/receives/conversions per wallet | official docs (v2 `/transactions`) |
| Accounts/balances | all accounts | per-asset wallets + balances (holdings snapshot) | official docs (v2 `/accounts`) |
| Advanced Trade orders | users who trade on Advanced | order fills | official docs (`/orders`) |
| Full-history CSV | all accounts | any date range, all transactions | documented Taxes → Generate Report path |

All optional in the contract (omit-if-empty); no tier code paths.

## Access & auth

- REST: `https://api.coinbase.com/v2/transactions`, `/accounts`, `/orders`.
  Personal API key + secret; requests HMAC-signed. **Read-only scope is
  sufficient and is what the connect card instructs** — Trove never asks for
  trade/transfer permissions.
- CSV fallback: coinbase.com → Taxes → Generate Report (CSV, any range) —
  the M1 backfill for deep history or key-averse users.
- No TCC, no local files. Standalone-clean (plain HTTPS with user-owned
  key). Key + secret stored in macOS Keychain like SimpleFIN.

## Vault mapping

- **Raw layer:** `finance/coinbase/raw/YYYY-MM.jsonl` — API transaction
  objects, full fidelity; imported tax CSVs preserved via the import
  pipeline.
- **Contract layer:** transactions normalize into the canonical finance
  ledger (the recorded write-time-dedupe exception) — asset, quantity, spot
  price, fees in `extra` until the ledger spec page formalizes a trade
  sub-shape. Balance/position snapshots from `/accounts` land in
  `finance/holdings/` once the Phase 3 holdings-snapshot contract is
  drafted.
- **Dedupe:** Coinbase transaction id as `guid` — API rows and tax-CSV
  backfill rows carry the same ids, so the two routes merge cleanly; cursor
  in `.trove/coinbase-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/coinbase.rs`: `DEF` (Periodic, daily-ish),
   `CONNECTION` (TokenPaste: key + secret fields, help copy walking the
   read-only key creation; SimpleFIN affordance rule), `pull` hook.
2. HMAC request signing in the sync client (the one non-boilerplate piece —
   timestamp + method + path + body signature per docs).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. Tax-CSV preset for backfill, sharing the dedupe guid with API rows.
5. Fixtures from documented v2 example responses (transaction, account,
   Advanced Trade order variants); parser + store + cursor + signing tests.
6. Privacy: financial detail — standard finance opt-in acknowledgement.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| API sync | 🧪 built | create a read-only key on a real account; paste KEY:SECRET in connect card; Sync now; confirm `finance/purchases/coinbase/YYYY-MM.jsonl` rows + raw layer + hub last-data |
| CSV backfill | — | generate a Taxes-page CSV; import separately (transaction ids match API rows; guid dedupe merges cleanly) |
| Advanced Trade | — | requires an account with Advanced Trade fills (any real user's run can validate this slice) |

## Build notes (2026-06-16)

- Behavior: `Periodic` (daily, `every_on_run`); manual Sync-now via `pull` hook.
- Auth: HMAC-SHA256 over `timestamp + METHOD + path + body`; headers `CB-ACCESS-KEY`, `CB-ACCESS-SIGN`, `CB-ACCESS-TIMESTAMP`, `CB-VERSION`. Signing algorithm sourced from `coinbase/wallet/auth.py` in the official Python SDK.
- Credentials stored as `KEY:SECRET` (single-paste, colon-separated) in `.trove/sync/coinbase.json` (0600).
- Contract: `finance-purchases` → `LineItem` rows at `finance/purchases/coinbase/YYYY-MM.jsonl`; raw transaction layer at `finance/coinbase/raw/YYYY-MM.jsonl`; raw account-snapshot layer at `finance/coinbase/raw-accounts/YYYY-MM.jsonl`. `ts` is Coinbase's `created_at` UTC instant re-emitted as RFC3339 local (matching the spec and bitcoin.rs); original UTC is preserved in `extra.created_at_utc`.
- Cursor: per-account `starting_after` id stored in `.trove/coinbase-sync.json` (rebuildable, non-secret). Drain-before-advance pattern; guid dedupe makes re-pulls idempotent.
- Holdings snapshot (`/v2/accounts` balances) raw-only at `finance/coinbase/raw-accounts/YYYY-MM.jsonl`; written every sync run before the transaction drain (unconditional, deduplicated by account id within each monthly partition). `finance-holdings` contract is a deferred sibling draft — raw backfill already on disk for when it lands.
- CSV backfill (Taxes-page export) shares the same transaction `id`s, so a future Import behavior can merge cleanly; deferred to a follow-up (not blocking 🧪).
- Dep added: `hmac = "0.12"` (RustCrypto, pure Rust — standalone rule maintained).

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Coinbase
Exchange API (L3771–L3777). Feasibility 🟢 high. Personal-key model means no
Trove-held app credential — the same property that made SimpleFIN the bank
choice. Also covers Advanced Trade (formerly Pro). Crypto rows may overlap
on-chain wallet data (the queued `ethereum`/`bitcoin` providers) and Cash
App bitcoin rows — dedupe on txid where present. Koinly/CoinTracker exports
(queued separately) are an alternative backfill for multi-exchange users.
