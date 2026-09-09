# Amazon Orders

- **id:** `amazon`
- **domains:** `finance/purchases/` (contract: **Phase 3 pending** —
  purchase line-item shape, per-source subfolders; drafted from Amazon +
  email-receipt parses + loyalty exports together)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Privacy Central data-request ZIP dropped on Trove)
- **connection:** none (file import; no login held by Trove)
- **evidence:** official data-request path documented (amazon.com → Privacy
  Central → Request Your Data → Order History → ZIP with
  `Retail.OrderHistory.1/` CSV) with key fields enumerated in the research
  doc; native CSV export removed March 2023; community Chrome-extension
  scrapers exist as the alternative
- **effort / priority:** M / P2
- **needs:** privacy (financial detail / itemized purchases — opt-in with
  explicit acknowledgement)

## What it is

Amazon order history — the most common retail purchase source for most
users. Itemized orders (what was bought, not just "AMZN $63.07") turn
opaque bank rows into real spending knowledge: line items, categories,
prices, tracking. Bank/card feeds already capture the totals; this is the
enrichment layer.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Order history (full lifetime) | none | Order ID, Order Date, Title, Category, ASIN, Quantity, Payment Instrument Type, Unit Price, Unit Price Tax, Shipping Charge, Total Charged, Tracking Number | official data-request path; fields per research doc |
| Bank-row enrichment | needs `bank-sync`/`csv-import` data present | order total ± date window matched to AMZN/Amazon descriptions | research doc pairing strategy |
| Live/ongoing scrape | v2 (M6) | orders page via browser-session scraping (Copilot's technique; Trove's `browser.rs` snapshot machinery) | research doc, planned |

All optional in the (pending) contract; the v1 import works with zero other
integrations enabled — enrichment is read-time joining, not a dependency.

## Access & auth

- Official path: amazon.com → Account → Privacy Central → Request Your
  Data → Order History → email notification → download ZIP (takes hours to
  days). The ZIP contains `Retail.OrderHistory.1/` with the detailed CSV.
- No API. No credentials held by Trove; no TCC. Standalone-clean.
- The browser-session scraper (v2) reuses the logged-in browser like the
  existing browser collectors — still no stored Amazon credential.

## Vault mapping

- **Raw layer:** `finance/purchases/amazon/` — the order CSV rows at full
  fidelity (taxonomy path; the research doc predates the taxonomy),
  partitioned by order-date month.
- **Contract layer:** purchase line-item rows per the **pending Phase 3
  contract** (expected: `ts`, `source`, `guid`, merchant, item title,
  category, qty, unit/total amounts; Amazon extras — ASIN, tracking,
  payment instrument — in `extra`). Ledger rows in `finance/` are **not**
  duplicated here; the AMZN bank row and the order stay separate records
  joined at read time.
- **Dedupe:** `guid` = Order ID + line index (one order has many lines);
  re-imported ZIPs are idempotent.

## Build plan

1. Module `crates/trove-core/src/amazon.rs`: `DEF` (Import), generic import
   box from the registry; parser for `Retail.OrderHistory.1/` CSV inside
   the request-ZIP (accept the ZIP or the bare CSV).
2. Fixtures: synthetic CSV from the documented field list; validate against
   a real data-request export during validation (field list is
   research-doc-sourced, not a verified sample — treat mapping as
   provisional until one lands).
3. Onboarding copy must set expectations: the data request takes hours to
   days and arrives by email — link the Privacy Central path.
4. Read-time enrichment (matching AMZN bank rows to order totals) is a
   later read-feature, not part of the collector.
5. v2: browser-session order scrape for ongoing pulls (M6, reuses
   `browser.rs` machinery) — out of scope for the first iteration.
6. **Parked behind Needs-David (contract)** for the contract layer: raw
   import can ship first (per-source raw is always allowed), contract rows
   follow ratification of the purchases shape.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Order ZIP import | ✅ built | request data at Privacy Central; drop the ZIP on Trove; confirm rows in `finance/purchases/amazon/` + hub last-data |
| Idempotent re-import | ✅ built | drop the same ZIP twice; row count unchanged (tested in `reimport_is_idempotent`) |
| Bank-row pairing | — | (read-time, later) spot-check an order total against its AMZN ledger row |
| Real-export smoke test | ✅ confirmed | CSV shape verified against two real Privacy Central exports (2023: 27 cols, 2025: 28 cols with Item Serial Number) |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Amazon Order
History (L3843–L3849). Feasibility 🟢 high for data quality, medium UX
(request latency). Native CSV export was removed in 2023 — the data request
beats screen scraping for completeness. Third-party Chrome extensions exist
but the official path is preferred for v1. The vault path here follows the
taxonomy (`finance/purchases/`), which supersedes any path in the research
entry.
