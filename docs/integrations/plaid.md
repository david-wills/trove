# Plaid

- **id:** `plaid`
- **domains:** `finance/` (would write the canonical ledger, like
  `bank-sync`)
- **status:** 🚫 unavailable
- **unavailable_reason:** Plaid issues developer-held keys that can't
  safely ship inside a standalone app, and routing your bank data through a
  relay server would break Trove's local-first promise. SimpleFIN covers
  bank sync with credentials you own.
- **behavior:** Unavailable (greyed catalog card with the reason above)
- **connection:** none (the blocker *is* the credential model)
- **evidence:** official docs; per-developer `client_id` + `secret` model
  confirmed (research doc hard-block list L137 and entry L3859–L3865)
- **effort / priority:** XL / P2
- **needs:** privacy (financial detail — would be opt-in if it ever
  shipped)

## What it is

The dominant US bank-data aggregator — broadest institution coverage,
near-real-time transactions, and an Investments product covering brokerage
accounts. Technically the gold standard for bank data; structurally
incompatible with a distributed standalone binary. Catalogued so the app
can answer "why isn't Plaid available?" honestly.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Bank/card transactions | per-Item pricing (~$0.10–$2.00/Item/mo; 10-Item free trial) | /transactions/get | official docs |
| Account balances | same | /accounts/get | official docs |
| Investment holdings + trades | same | /investments/holdings/get, /investments/transactions/get | official docs |

None reachable under Trove's constraints — see Access & auth.

## Access & auth

Plaid Link widget + REST calls to production.plaid.com, authenticated with
a **developer-held** `client_id` + `secret` issued per developer/company.
That model fails Trove three ways: (a) shipping the secret in the binary
exposes it to extraction; (b) all users would ride one developer's quota
and bind to Plaid's ToS through Trove; (c) the standard fix — a relay
server — violates local-first. The cross-cutting question for any
aggregator is "who holds the credential?", and Plaid's answer is the wrong
one. The same wall blocks MX, Finicity, Akoya, Yodlee, TrueLayer, and
SaltEdge (and Teller/SnapTrade, catalogued separately).

## Vault mapping

None (unavailable). If a BYOK mode ever ships it would write the canonical
`finance/` ledger exactly as `bank-sync` does, deduped by the existing
cross-source matcher.

## Build plan

None. Catalog stub only:

1. `Behavior::Unavailable` def with the `unavailable_reason` above —
   renders dim, sorts last in the Finance section, visible by default.
2. Card copy points users to Bank Sync (SimpleFIN) as the shipped
   alternative with user-owned credentials.
3. Revisit triggers: Plaid offering user-held credentials, or a deliberate
   BYOK-for-power-users decision (research verdict: "possible via BYOK,
   ever" — user registers their own Plaid developer account; too much
   friction to be a first-class integration). Plaid Investments is the one
   capability SimpleFIN doesn't replace; native Schwab/IBKR briefs cover
   that gap instead.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | open the hub Finance section; confirm the Plaid card renders greyed with the reason copy and a pointer to Bank Sync |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Plaid
(L3859–L3865); hard-block list L137; domain cross-cutting notes
(L3931–L3945): the developer-key distribution problem is the single biggest
structural constraint in finance, and SimpleFIN was chosen precisely
because each user holds their own credential. Feasibility 🟠 low for
standalone distribution despite technical excellence; recommendation:
icebox.
