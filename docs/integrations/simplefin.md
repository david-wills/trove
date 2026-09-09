# SimpleFIN (Bank Sync)

- **id:** `simplefin` (shipped def: `bank-sync`; module
  `crates/trove-core/src/finance/simplefin.rs`)
- **domains:** `finance/` (contract: **document** — the canonical ledger
  as built; the recorded write-time-dedupe exception. Phase 3 writes the
  spec page without redesigning it.)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (daily poll of the access URL; Sync-now wired)
- **connection:** `simplefin` — TokenPaste (one-time setup token →
  claimed long-lived access URL, stored in macOS Keychain). Shared with
  any future def that reads the same Bridge credential.
- **evidence:** official protocol — beta-bridge.simplefin.org docs;
  validated on real data (7 accounts synced)
- **effort / priority:** S / P0
- **needs:** privacy (financial detail — explicit user connect required;
  data leaves the machine only to the user's own Bridge credential)

## What it is

SimpleFIN Bridge is a bank/card aggregator (~16k institutions via the MX
network) where **each user owns their credential** (~$1.50/mo) — the only
aggregator model compatible with Trove's standalone rule, and why it was
chosen over Plaid/Teller (both hard-blocked on developer-held keys).
This is the live spine of the finance domain: balances and transactions
across checking, savings, and cards.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Accounts + balances | Bridge subscription | account id/name/org, balance, currency | official protocol; validated |
| Transactions | Bridge subscription | date, amount, description, pending | official protocol; validated |
| History depth | upstream cap | ~90-day rolling window per sync | official; not a Trove limit |

Deep history past the 90-day window is seeded via `bank-statements` /
`copilot-money` file imports — by design, not a gap.

## Access & auth

- POST `https://beta-bridge.simplefin.org/simplefin/claim` with the
  one-time setup token → long-lived access URL; then
  `GET {access_url}/accounts?start-date={epoch}`.
- No per-developer key; credential lives in the macOS Keychain
  (`finance/keychain.rs`). No TCC. Standalone-clean.
- Connection breaks (bank MFA resets) surface in the Bridge dashboard;
  Trove shows staleness and deep-links there.

## Vault mapping

- **Raw/canonical layer:** the canonical ledger —
  `finance/accounts.jsonl` (account registry),
  `finance/transactions/<account-id>/<year>.jsonl`,
  `finance/balances/<account-id>.jsonl` (daily snapshots).
- **Contract layer:** `finance/` is itself the documented contract:
  bank syncs and statement imports dedupe into shared per-account files
  at write time because a transaction's identity belongs to the account,
  not the observing source (`vault-spec/conventions.md`). Dedupe guid =
  the canonical transaction id; sync cursor in `.trove`, rebuildable.

## Build plan

Already shipped (def `bank-sync`, connection `simplefin`). Remaining
pipeline work:

1. Phase 3 documentation pass: write `vault-spec/domains/finance.md`
   describing the ledger shape as built (no redesign).
2. Keep as the reference TokenPaste + Keychain pattern for new finance
   connections (Coinbase, IBKR, YNAB briefs all point here).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Claim + daily sync | 🧪 (shipped pre-pipeline; real-data validated on 7 accounts — David promotes to ✅) | paste a setup token in the connect card; Sync now; confirm rows in `finance/transactions/` + balances + hub last-data |
| Staleness surfacing | 🧪 (shipped pre-pipeline) | break a connection in the Bridge dashboard; confirm Trove shows staleness + deep-link |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §SimpleFIN
Bridge (L3683–L3689). Feasibility 🟢 high. Time-sensitive in operation:
the 90-day upstream window means lapsed syncing loses history — the
staleness UI exists for this. Apple Card/Cash/Savings are unreachable by
*any* aggregator (FinanceKit-gated) — the `apple-card` and
`bank-statements` briefs carry that route. Plaid/Teller catalogued as
unavailable alternatives with honest reasons.
