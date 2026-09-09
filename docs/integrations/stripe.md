# Stripe Billing

- **id:** `stripe`
- **domains:** `finance/purchases/` (contract: **Phase 3 pending** —
  purchase line-item shape; per-source subfolders)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (dashboard CSV drop; no ongoing sync)
- **connection:** none (CSV export needs no credential; the API path would
  need a Stripe API key, but it's not the recommended path — see notes)
- **evidence:** official-docs — Stripe Dashboard → Billing → Invoices →
  Export → CSV is documented; `GET /v1/invoices` exists but requires a
  Stripe account (small population)
- **effort / priority:** S / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement) · Needs-David (icebox call: research verdict is that
  email-receipt parsing covers Stripe-billed charges for *everyone*, while
  this import only serves direct Stripe account holders)

## What it is

Stripe is the billing backend behind a huge share of SaaS subscriptions —
but most consumers only ever see it as an email receipt. The subset who
hold a Stripe account directly (freelancers, developers, platform sellers,
some SaaS subscribers) can export their invoice history as CSV from the
dashboard. For them it's a clean record of recurring SaaS spend with
line-item detail the bank ledger lacks.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Invoice history (CSV) | any Stripe account | date, amount, currency, description/line items, status, customer | official docs (dashboard export) |
| Invoice API (`/v1/invoices`) | any Stripe account + API key | same, JSON, paginated | official docs — not the chosen path |

All optional in the contract (omit-if-empty); no tier-specific code paths.

## Access & auth

- Export mechanism: Stripe Dashboard → Billing → Invoices → Export → CSV;
  user drags the file into the registry-driven import box. No credential
  held by Trove.
- The REST API (`GET /v1/invoices`, Bearer secret key) is feasible but
  serves the same small population at higher auth cost — Import wins.
- No TCC, no local files, no network. Standalone-clean.

## Vault mapping

- **Raw layer:** `finance/purchases/stripe/` — the imported CSV preserved
  as received, plus parsed rows partitioned `YYYY-MM.jsonl`.
- **Contract layer:** purchase line-item rows per the (pending) Phase 3
  purchases contract — expected shape: `ts`, `source`, `guid` = Stripe
  invoice id (falls back to a date+amount+description hash if the export
  omits ids), merchant/payer, amount, currency, line items; Stripe-specific
  columns ride in `extra`. Note these are *itemized orders/receipts*, not
  ledger transactions — the bank charge stays in `finance/`, joined at
  read time.
- **Dedupe:** invoice id as `guid`; re-importing an overlapping export is
  idempotent.

## Build plan

1. **Gate first:** Needs-David — confirm this leaves the icebox. The
   research recommendation is to fold Stripe receipts into the planned
   email-receipt enrichment instead and skip the standalone import.
2. If green-lit: module `crates/trove-core/src/stripe.rs` with `DEF`
   (Import); one registration line in `INTEGRATIONS`.
3. CSV parser against a real dashboard export — the export's exact column
   set isn't reproduced in the research doc, so parser-last with a
   fixture from a real export (mild Needs-sample).
4. Privacy gate: opt-in enable with explicit acknowledgement (financial
   detail).
5. Contract rows wait on the Phase 3 purchases contract; raw import can
   land first (full fidelity first, normalization second).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Invoice CSV import | — | export invoices from a real Stripe dashboard; drop into the import box; confirm rows in `finance/purchases/stripe/` + hub last-data; re-import dedupes |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Stripe
Billing / Invoice Data (L3907–L3913). Feasibility 🟡 medium — mechanism is
trivial, population is small. **Research recommendation: Icebox** — most
users meet Stripe only through other services' billing; email-receipt
parsing (planned email-corpus enrichment) captures Stripe-billed SaaS
charges for everyone, including non-account-holders. Catalogued queued so
the call is recorded; sequencing is bottom-of-queue pending the
Needs-David decision.
