# Grocery & Retail Loyalty Programs

- **id:** `grocery-loyalty`
- **domains:** `finance/purchases/` (purchase line-item shape — **Phase 3
  pending**; would apply if this were buildable)
- **status:** 🚫 unavailable
- **unavailable_reason:** Kroger, Safeway and other loyalty programs offer
  no export or API for your itemized purchase history. Privacy-law data
  requests take weeks and return inconsistent, often non-machine-readable
  files. Revisit if regulation (CFPB 1033) forces a path.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** research-verified absence — no API or structured export
  exists; GDPR/CCPA request paths documented (kroger.com → Account → Data
  Privacy → Request My Data; Albertsons/Safeway similar) and found
  inconsistent/often PDF
- **effort / priority:** XL / P2
- **needs:** privacy (itemized purchase detail — moot while unavailable)

## What it is

The itemized record of what you actually bought at the grocery store —
arguably the most detailed spending data that exists about a person
(Kroger earned $527M selling shopper data). Bank rows say "$84.12 KROGER";
loyalty data says which 31 items. High user value, zero consumer access.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Itemized purchase history | none reachable | (would be: items, prices, store, date) | no API/export exists |
| Privacy-law data request | CA/TX/VA/OR etc. residents only | inconsistent, often PDF, weeks of latency | research-documented |

The data-request path can't be productized: no schedule, no stable format,
no machine-readable guarantee — a parser per retailer per request batch.

## Access & auth

None viable. Retailers provide no consumer API and no structured export;
the only lever is a CCPA/GDPR-style request, which is jurisdiction-
dependent, takes weeks, and returns whatever format the retailer feels
like. Building on that would be a support burden masquerading as an
integration.

## Vault mapping

- **Raw layer:** would be `finance/purchases/<retailer>/` per the taxonomy
  (per-source subfolders under the Phase 3 purchase line-item shape) — n/a.
- **Contract layer:** purchase line-item contract, Phase 3 pending — n/a.

## Build plan

None. Catalogued for the honest greyed card. Two realistic future paths,
both outside this entry:

1. **Email-receipt parsing** (the planned email-corpus enrichment):
   Instacart, Amazon Fresh, and grocery pickup/delivery orders email
   itemized receipts — the indirect route to a slice of this data. Noted
   on the email integration's roadmap, not here.
2. **Regulatory change:** CFPB Section 1033 implementation could force a
   programmatic path — re-evaluate then.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| none | 🚫 | n/a — unavailable card renders via `Behavior::Unavailable` |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases"
§Grocery/Retail Loyalty Programs (L3899–L3905). Feasibility 🟠 low;
recommendation: icebox until a programmatic path exists. The research
flags the asymmetry pointedly: retailers monetize this data at scale
while giving the consumer effectively nothing structured back. If it ever
opens up, this is privacy-sensitive itemized purchasing — opt-in with
explicit acknowledgement, per the financial-detail rule.
