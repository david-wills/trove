# Zelle

- **id:** `zelle`
- **domains:** `finance/` (contract: **document** — canonical ledger as built,
  the recorded write-time-dedupe exception)
- **status:** 🧪 built (covered by shipped defs `bank-sync` + `csv-import`;
  pre-pipeline — David promotes to ✅)
- **unavailable_reason:** none
- **behavior:** CoveredBy(bank-sync) — no separate pull exists or is needed
- **connection:** none of its own (rides the existing `simplefin` connection
  via `bank-sync`, or no connection at all via `csv-import`)
- **evidence:** research doc verified: no standalone Zelle export or API
  exists; Zelle activity surfaces as bank transactions (feasibility 🟢 high)
- **effort / priority:** S / P2
- **needs:** privacy (financial detail — finance domain ships opt-in with
  explicit acknowledgement, as the existing finance defs already do)

## What it is

Zelle is the bank-embedded US peer-to-peer payment network. Unlike Venmo or
Cash App it has no wallet of its own — every Zelle payment settles directly
in the sender's/receiver's bank account. That makes it the rare provider
whose complete data already flows into Trove through existing integrations.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Zelle payments (in/out) | none | bank transaction rows (date, amount, counterparty in description) | surfaces in SimpleFIN feeds + bank CSV exports |

All fields arrive as ordinary ledger rows; the Zelle counterparty name is
embedded in the bank's transaction description. No capability exists beyond
what the bank reports.

## Access & auth

No Zelle endpoint, file, or export. Access is transitively whatever the
user's bank grants: SimpleFIN sync (`bank-sync`) or bank-portal CSV/OFX
(`csv-import`). No new auth, no TCC, standalone-clean by construction.

## Vault mapping

- **Raw layer:** none of its own — rows land via `finance/` exactly as
  written by `bank-sync` / `csv-import` (e.g. `finance/transactions/`,
  `finance/imports/`).
- **Contract layer:** the canonical finance ledger as built; Zelle rows are
  ordinary transactions, deduped by the existing cross-source fuzzy matcher.
- **Dedupe:** inherited from the ledger's write-time dedupe.

## Build plan

1. Catalog entry only: a `CoveredBy(bank-sync)` stub def so the hub answers
   "where's my Zelle data?" — card copy explains that Zelle settles in the
   bank account and is already captured by Bank Sync / statement import.
2. No module logic, no connection, no parser. The registry's CoveredBy
   rendering does the rest.
3. Known gap to state honestly in the card copy: users of the standalone
   Zelle app with no connected bank account have no export path (extremely
   rare configuration per the research doc).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Zelle rows via bank | 🧪 (shipped pre-pipeline via bank-sync/csv-import; David promotes to ✅) | send/receive a Zelle payment; Sync now on Bank Sync; confirm the row appears in `finance/` with the Zelle counterparty in its description |
| Hub coverage copy | — | open the hub; confirm the Zelle card renders as covered-by with the explainer, not as an actionable integration |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Zelle
Transactions (L3827–L3833). Feasibility 🟢 high precisely because there is
nothing to build: no standalone export exists and all activity surfaces in
bank transactions. The research doc's explicit recommendation is that
onboarding copy explain the coverage rather than list Zelle as an
integration demanding action. Cross-cutting note: file import remains a
permanent peer backend for banks the aggregator can't reach.
