# Teller.io

- **id:** `teller`
- **domains:** `finance/` (canonical ledger, document contract — would apply
  if this were buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** Teller authenticates with a certificate issued to
  the developer, not to you — sharing it across all Trove users would
  violate Teller's terms, and a relay server would break local-first.
  SimpleFIN covers bank sync with credentials you own.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** official-docs — api.teller.io; per-developer mTLS
  certificate model confirmed (research doc, hard-block analysis at L137
  and finance cross-cutting notes)
- **effort / priority:** XL / P2
- **needs:** privacy (financial detail — moot while unavailable)

## What it is

A bank-aggregation backend: direct API connections to banks (no screen
scraping), near-real-time transactions and balances. Technically one of the
cleanest aggregators — but its auth model makes it structurally unshippable
in a standalone distributed app.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Bank/card transactions | free tier: 100 live connections | amount, date, description, account | official docs |
| Account balances | same | balance, account metadata | official docs |

None reachable: every capability sits behind the developer-held mTLS
certificate.

## Access & auth

- Teller Connect widget (OAuth-style flow) + REST at
  `https://api.teller.io/accounts/{id}/transactions`.
- Auth is mutual TLS with a certificate issued **per developer**. Shipping
  that cert in the Trove binary exposes it to extraction and puts every
  Trove user on one developer's quota and ToS — the free 100-connection
  tier means 100 total users, then a ToS violation.
- A Trove-run relay would fix the cert problem and break the local-first
  promise instead. Both paths fail the standalone rule.

## Vault mapping

- **Raw layer:** would be `finance/teller/` — n/a.
- **Contract layer:** would feed the canonical `finance/` ledger (the
  documented as-built shape, write-time dedupe vs. SimpleFIN/imports) —
  n/a.

## Build plan

None. Catalogued so the app can show the greyed card with the honest
reason. Revisit only if Teller ever offers user-held credentials, or as a
BYO-certificate power-user mode (same friction problem as Plaid BYOK —
research verdict: not worth it while SimpleFIN exists).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| none | 🚫 | n/a — unavailable card renders via `Behavior::Unavailable` |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Teller.io
(L3867–L3873). Feasibility 🟠 low for standalone distribution; effort XL;
recommendation: icebox. Same wall as Plaid/MX/Finicity/Akoya/Yodlee/
TrueLayer/SaltEdge (cross-cutting note L3933: every aggregator evaluation
starts with "who holds the credential?"). SimpleFIN (`bank-sync`, built ✅)
is the chosen user-held-credential answer for US banks; GoCardless is the
EU/UK path. Teller's cleaner direct-API approach is noted as a possible
fallback for institutional deployments where a relay is acceptable — out
of scope for the app.
