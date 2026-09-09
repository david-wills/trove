# GoCardless Bank Account Data (EU/UK)

- **id:** `gocardless`
- **domains:** `finance/` (canonical ledger — document contract, the
  as-built shape with write-time cross-source dedupe)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Periodic (poll transactions/balances per linked account)
- **connection:** `gocardless` — TokenPaste (BYO free developer account:
  user registers at bankaccountdata.gocardless.com, ~10 min, pastes their
  own secret id/key). Not shared with other defs.
- **evidence:** official-docs — Account Information Services API at
  bankaccountdata.gocardless.com (`/api/v2/accounts/{id}/transactions/`);
  free tier 50 monthly connections; 2,500+ EU/UK banks via PSD2 mandate.
- **effort / priority:** L / P2
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-login (user-registered GoCardless account) ·
  **Needs-David: confirm GoCardless ToS permits embedded BYOK usage before
  building** (research verdict: "spike first")

## What it is

The EU/UK answer to SimpleFIN. SimpleFIN is US-centric, which leaves
European users of Trove without live bank sync — a real gap for the
"built for anyone" principle. GoCardless (ex-Nordigen) covers virtually
every European bank through PSD2 open banking, with a free production
tier.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transactions | free tier (50 monthly connections) | amount, date, description, counterparty, account | official docs |
| Balances | free tier | balance per account | official docs |
| Account metadata | free tier | IBAN, name, institution | official docs |

PSD2 consent windows mean bank links expire (~90–180 days per bank) and
need re-consent — surface staleness on the card, like SimpleFIN's broken-
connection handling.

## Access & auth

- REST: `https://bankaccountdata.gocardless.com/api/v2/...` — create
  requisition → user authorizes at their bank → poll accounts/
  transactions. Plain HTTPS, no TCC.
- **The credential is developer-held by design** — same structural problem
  as Plaid/Teller — but the free 50-connection tier makes
  bring-your-own-key realistic: each user registers their *own* free
  GoCardless developer account and pastes their own secret. Nothing
  Trove-owned ships in the binary; each user rides their own quota and
  ToS. This is the only aggregator where the research judged BYOK
  acceptable friction (EU users already know open banking).
- Onboarding copy must walk the registration explicitly ("register your
  own GoCardless account — free, ~10 minutes"), and the gated Connect
  button needs the disabled-affordance treatment.

## Vault mapping

- **Raw layer:** `finance/gocardless/` — native transaction objects,
  per-month partitions.
- **Contract layer:** canonical `finance/` ledger rows (write-time-dedupe
  exception); `guid` = GoCardless transaction id (fall back to internal
  hash where banks omit ids — PSD2 data quality varies); bank-specific
  extras in `extra`; fuzzy dedupe vs. any statement imports of the same
  accounts.

## Build plan

0. **Gate: David confirms ToS permits embedded BYOK** (the recorded
   Needs-David). If ToS forbids it, flip this brief to 🚫 with honest
   copy mirroring Plaid/Teller.
1. Module `crates/trove-core/src/gocardless.rs`: `DEF` (Periodic),
   `CONNECTION` (TokenPaste — secret id + key; setup copy carries the
   register-your-own-account walkthrough).
2. Requisition flow: bank selection + browser handoff for the consent
   redirect — the one part the registry doesn't give for free; model on
   SimpleFIN's claim-once-then-poll structure where possible.
3. Fixtures from official docs' example payloads, including a
   missing-transaction-id variant; cursor + dedupe tests.
4. Re-consent/staleness surfacing (PSD2 expiry) on the hub card.
5. Privacy gate: financial detail — opt-in with explicit acknowledgement.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Link + sync | — | needs an EU/UK bank account — David likely can't validate; sandbox institutions exist for the flow, a real EU user's run promotes the data slice |
| Re-consent expiry | — | observe a link past its consent window; card shows stale + re-consent path |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §GoCardless
(L3891–L3897) + cross-cutting notes (L3933, L3945). Feasibility 🟡 medium
(for EU/UK users specifically); recommendation **spike first**.
Effort L mostly from the requisition/consent flow and BYOK onboarding,
not the data mapping. Positioning: the EU/UK SimpleFIN equivalent —
"who holds the credential?" is answered "the user," via their own free
account rather than a purchased token.
