# Kraken

- **id:** `kraken`
- **domains:** `finance/` (contract: **document** — the canonical ledger as
  built; Phase 3 writes the spec page)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily; `/0/private/Ledgers` offset pagination, cursor = newest id)
- **connection:** `kraken` — new `pub static CONNECTION` in `kraken.rs` (TokenPaste KEY:SECRET;
  Query Ledger Entries permission only). Registered in CONNECTIONS.
- **evidence:** official REST docs — docs.kraken.com/api/docs/rest-api/get-ledgers-info
  (Ledgers endpoint, 50/page, offset-based) + docs.kraken.com/api/docs/guides/spot-rest-auth
  (two-level HMAC-SHA512 signing scheme). TradesHistory not separately drained — Ledgers
  covers all entry types including trade fills via `type="trade"`.
- **effort / priority:** M / P1
- **needs:** Needs-login (validation only — key + secret from a real account)

## What it is

Major US crypto exchange — largest by volume for many asset pairs. Trades,
deposits/withdrawals, fees, plus staking rewards and earn positions (all
surfaced through the Ledgers endpoint). Like Coinbase, both an official API
with user-generated keys and a documented CSV export exist; the API runs
ongoing sync, the CSV seeds deep history.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Trade history | all accounts | fills (pair, price, volume, fee, time) | official docs (TradesHistory) |
| Ledger entries | all accounts | deposits, withdrawals, fees, staking rewards, earn | official docs (Ledgers) |
| Full-history CSV | all accounts | Ledgers or Trades export, custom range | documented export path |

All optional in the contract (omit-if-empty); no tier code paths.

## Access & auth

- REST: `https://api.kraken.com/0/private/TradesHistory` (paginated, 50
  trades/request, offset-based) and `/0/private/Ledgers`. Signed private
  requests (API key + secret, nonce + HMAC per Kraken's scheme).
- Key creation: kraken.com → Security → API → Add Key — connect-card copy
  instructs the minimal permission set (**Query Ledger Entries** — and
  Export Data only if the user wants in-app export triggering; never trade
  permissions).
- CSV export: profile icon → Documents → Create Export (Ledgers or Trades,
  custom range). **Generation is async — minutes to a week** — so it's an
  offline backfill step, not something Trove waits on.
- No TCC, no local files. Standalone-clean; key + secret in macOS Keychain.

## Vault mapping

- **Raw layer:** `finance/kraken/raw/YYYY-MM.jsonl` — API trade/ledger
  objects, full fidelity; imported export CSVs preserved via the import
  pipeline.
- **Contract layer:** trades and ledger rows normalize into the canonical
  finance ledger (the recorded write-time-dedupe exception) — pair, volume,
  fee, ledger type (staking/earn) in `extra` until the ledger spec page
  formalizes a trade sub-shape. No holdings substream catalogued for v1
  (balance snapshots can join a later holdings pass if wanted).
- **Dedupe:** Kraken trade id / ledger id as `guid` — present in both API
  responses and CSV exports, so the two routes merge cleanly; cursor in
  `.trove/kraken-sync.json`, rebuildable from output files.

## Build plan

1. Module `crates/trove-core/src/kraken.rs`: `DEF` (Periodic, daily-ish),
   `CONNECTION` (TokenPaste: key + secret, help copy for the minimal
   permission set; SimpleFIN affordance rule), `pull` hook.
2. Kraken request signing (nonce + HMAC-SHA512 path signature per docs) and
   offset pagination (50/request — fine for a personal account; cap pages
   per sync pass and continue from the cursor).
3. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
4. CSV presets (Trades + Ledgers export formats) for backfill, sharing the
   id-based dedupe guid with API rows.
5. Fixtures from documented example responses (trade, ledger incl. a
   staking-reward row); parser + store + cursor + signing tests.
6. Privacy: financial detail — standard finance opt-in acknowledgement.

## Implementation notes (built 2026-06-16)

- Uses only `/0/private/Ledgers` (covers trades, deposits, withdrawals, staking, earn, transfers).
  TradesHistory not separately drained — redundant given Ledgers covers all types.
- Signing: two-level HMAC-SHA512 (SHA256(nonce+body) → path_bytes+digest → HMAC-SHA512 with
  base64-decoded secret → base64 encode). Implemented in `sign()` using sha2 + hmac + base64
  crates already present in Cargo.toml (no new deps added).
- Cursor: `newest_id` in `.trove/kraken-sync.json` (rebuildable, non-secret). Drain stops when
  the saved id is encountered in the page (overlap idempotent via guid dedupe).
- Raw layer: `finance/kraken/raw/YYYY-MM.jsonl` — full fidelity, id tagged.
- Contract layer: `finance/purchases/kraken/YYYY-MM.jsonl` — `LineItem` (reuse-bound,
  finance-purchases). `currency` = Kraken asset symbol (XXBT, ZUSD, ETH2.S, …); `item` = ledger
  type (trade/deposit/withdrawal/staking/…); `extra` carries refid, subtype, fee, balance, time_raw.
- CSV backfill not wired (Ledgers export generation is async; offline import step left for later).
- 22 tests, all green; `cargo check` clean.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| API sync | — | create a Query-Ledger-Entries key on a real account; paste in connect card; Sync now; confirm ledger rows + hub last-data |
| CSV backfill | — | request a Ledgers export, import the delivered CSV; confirm rows merge with API rows, zero dupes |
| Staking/earn rows | — | requires an account with staking history (any real user's run can validate this slice) |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Kraken
Exchange API + Export (L3779–L3785). Feasibility 🟢 high. The research doc's
explicit guidance: API for ongoing sync, CSV for initial backfill (paginating
thousands of historical trades at 50/request is the slow path). Export
generation latency (up to a week) must be set as an expectation in the card
copy. Pairs with the Coinbase brief — build against the same ledger `extra`
conventions; crypto rows dedupe on txid against on-chain providers where
applicable.
