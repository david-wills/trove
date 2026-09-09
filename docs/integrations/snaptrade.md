# SnapTrade (Brokerage Aggregator)

- **id:** `snaptrade`
- **domains:** `finance/` · `finance/holdings/` (would-be; no folder is
  created while unavailable)
- **status:** 🚫 unavailable
- **unavailable_reason:** SnapTrade issues developer-held API keys that
  can't safely ship in a standalone app, and a relay server would break
  Trove's local-first promise. May return as a bring-your-own-key option
  for power users.
- **behavior:** Unavailable
- **connection:** none (the blocked model: per-developer API keys from the
  SnapTrade dashboard)
- **evidence:** official-docs — docs.snaptrade.com + official Rust SDK
  (github.com/passiv/snaptrade-sdks); per-developer key model verified
- **effort / priority:** L / P2
- **needs:** privacy (financial detail — would ship opt-in if ever built)

## What it is

A brokerage-data aggregator: 30+ brokerages (Robinhood, Schwab, Fidelity,
TD, IBKR, …) behind one REST API, with an official Rust SDK and a generous
free tier (100 live connections). Technically excellent — and structurally
incompatible with a standalone distributed binary, the same wall as Plaid
and Teller.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Accounts/positions/transactions across 30+ brokerages | free tier: 100 connections | holdings, balances, trade history | official docs (not buildable) |

## Access & auth

`GET /accounts`, `/accounts/{id}/positions`, `/accounts/{id}/transactions`
per docs.snaptrade.com — but keys are issued per *developer*, not per user.
Shipping Trove's key in the binary exposes it (and burns the shared quota);
a Trove relay server would route everyone's brokerage data through one
host, violating the local-first hard rule. The only standalone-clean shape
is BYOK: each user registers their own SnapTrade developer account —
unusual friction the research doc flags as a spike, not a build.

## Vault mapping

none while unavailable. If a BYOK mode ever ships: trades → canonical
`finance/` ledger, positions → `finance/holdings/snaptrade/` per the
Phase 3 holdings-snapshot contract, raw under `finance/snaptrade/raw/`.

## Build plan

none. Catalog entry ships as a dim, sorted-last hub card showing the
unavailable_reason. Revisit trigger (from the catalog record): after native
Schwab and Interactive Brokers ship, if real users need brokerages those
two don't cover and the GoCardless-style BYOK UX proves acceptable, spike a
BYOK connect flow.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows the entry dimmed with the honest reason; hidden by the "hide unavailable" filter |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §SnapTrade
(L3739–L3745). Feasibility 🟡 medium — blocked on distribution model, not
technology. SOC 2 Type 2; official Rust SDK exists. Research verdict:
"Spike first; defer Schwab/IBKR native first, then revisit SnapTrade for
broader coverage." Same constraint family as Plaid and Teller.io (see
their briefs); SimpleFIN remains the user-owned-credential answer for
banks, native APIs for brokerages.
