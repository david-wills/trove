# Charles Schwab

- **id:** `schwab`
- **domains:** `finance/` (trades → canonical ledger — contract:
  **document**, as built) · `finance/holdings/` (position snapshots —
  contract: **Phase 3 pending**, holdings-snapshot shape)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll transactions + position snapshots)
- **connection:** `schwab` — OAuth (user registers a free Individual
  Developer account at developer.schwab.com and OAuth-authorizes their own
  brokerage account; same user-owned flow as TickTick/Oura). Not shared
  with other defs.
- **evidence:** official-docs — developer.schwab.com Individual Developer
  tier (free with any Schwab brokerage account; positions + transactions
  endpoints documented)
- **effort / priority:** M / P1
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement) · Needs-login (real Schwab brokerage + developer account
  for validation) · holdings contract not yet ratified (Needs-David)

## What it is

One of the largest US retail brokerages (equities, options, mutual funds,
ETFs, retirement accounts), with an official, free, individual-developer
API — rare among brokerages. Yields near-real-time positions and a year-
per-request trade history, putting investment activity in the vault next to
the spending ledger.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Positions (holdings) | free w/ brokerage acct | symbol, quantity, market value, cost basis | official docs |
| Transactions (trades) | free; max 1 yr per request | trade date, symbol, action, quantity, price, fees | official docs |
| Balances | free | account cash/total balances | official docs |

All optional in the contract shapes; the 60-day/request transaction window is
a pagination concern (walk back in 60-day windows), not a tiering one.

## Access & auth

- OAuth 2.0 against developer.schwab.com; endpoints
  `GET /accounts/{accountNumber}/positions` and
  `GET /accounts/{accountNumber}/transactions`.
- Setup friction is real and must be in the connect-card copy: the
  Individual Developer account is a separate login from the brokerage, and
  the user registers their own app there — Trove ships no Schwab credential.
- Rust: reqwest + oauth2 crates per the research entry.
- No TCC, no local files. Standalone-clean (user-owned OAuth app).
- M1 fallback: Schwab's portal OFX export still works — routes through the
  statement importer (csv-import's OFX extension) if the API path stalls.

## Vault mapping

- **Raw layer:** `finance/purchases/schwab/raw/YYYY-MM.jsonl` — API transaction
  objects, full fidelity (under the contract tree per spec convention).
- **Contract layer:** trades → canonical `finance/` ledger rows (`ts`,
  `amount`, `account`, `guid` = transaction id; symbol/action/quantity in
  `extra`); positions → `finance/holdings/schwab/` snapshots per the
  (pending) holdings-snapshot contract — dated point-in-time rows, not
  events. Trades stay in the ledger; positions never do (taxonomy rule).
- **Dedupe:** transaction id as `guid`; holdings snapshots keyed by
  (snapshot date, account, symbol).

## Build plan

1. Module `crates/trove-core/src/schwab.rs`: `DEF` (Periodic, daily),
   `CONNECTION` (OAuth; setup copy walks the Individual Developer
   registration — disabled-control affordance until connected), `pull` hook.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Fixtures from official docs' response examples; ledger-mapping +
   yearly-window pagination tests.
4. Privacy gate: opt-in enable with explicit acknowledgement.
5. Holdings rows are **parked behind Needs-David (holdings contract)**;
   ledger trades can ship first — the holdings sub-schema should be
   designed before/with this build (research note: "design
   finance/investments sub-schema first" — now the `finance/holdings/`
   Phase 3 contract).

## Build notes (2026-06-16)

- Behavior: Periodic (4-hour cadence), OAuth via `api.schwabapi.com/v1/oauth/*`
- Contract: trades → `finance-purchases.LineItem` (reuse-bound; bitcoin.rs precedent);
  positions → raw-only (`finance/schwab/positions/`) pending `finance-holdings` contract
- Connection: NEW `pub static CONNECTION` with `redirect_port=38647` (assigned port)
- Field names confirmed from SchwabApiCS (C# open-source client):
  `activityId`, `time`, `tradeDate`, `netAmount`, `transferItems[].instrument.symbol`,
  `.amount`, `.price`, `.cost`, `.feeType`, `.positionEffect`
- Position field names confirmed from Accounts.cs:
  `longQuantity`, `averagePrice`, `marketValue`, `instrument.symbol`
- Parser parked for positions (holdings contract deferred); raw snapshot written daily
- Needs-login: real Schwab brokerage + Individual Developer account for validation
- All 18 unit tests pass (cargo test -p trove-core schwab::)

## Fix notes (2026-06-16) — adversarial review

- **Walk-back pagination implemented:** First sync walks back from `now` in 60-day
  windows (confirmed via schwab-py: "startDate must be within 60 days") until a short
  page. The prior single-call 365-day attempt would have been rejected (400) or clamped.
- **UTC dates:** All API query dates are now formatted from `Utc::now()` — never from
  `Local::now()`. Using `Local` with the Z suffix would mislabel the offset, silently
  skipping transactions for east-of-UTC users on incremental syncs.
- **Cursor uses `time` field:** The cursor now keys on `time` (the API activity
  timestamp the endpoint orders by), not `tradeDate` (midnight, a different axis).
  Using `tradeDate` as a cursor risks stagnation and mixing filter axes.
- **Raw dir moved:** Transaction raw is now at `finance/purchases/schwab/raw/`
  (matching bitcoin.rs precedent and spec convention) rather than `finance/schwab/raw/`.
- **Cursor advances on parsed rows:** `max_time_utc` uses the `time` field directly
  from raw JSON; `drain_window` returns the count of successfully-written rows, so the
  contract count stays accurate even if some rows drop at parse time.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Transactions | 🧪 built | connect a real Schwab account via OAuth; Sync now; confirm LineItem rows + raw layer; walk-back fills history |
| Positions | 🧪 raw-only | confirm dated snapshot rows in `finance/schwab/positions/` match the portal; full contract awaits finance-holdings |
| Balances | — | hub last-data + account balance fields match Client Portal |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Charles
Schwab Brokerage API (L3723–L3729). Feasibility 🟢 high. The Individual
Developer role explicitly supports personal-account apps — no ToS risk for
the user-owned-app model. Complements Fidelity (CSV-only) for users with
both. Sequence with Interactive Brokers (same domains, same holdings
contract) to exercise the contract with two sources.
