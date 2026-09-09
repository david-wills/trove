# Bank Statement Import (CSV/OFX/QFX)

- **id:** `bank-statements` (shipped def: `csv-import`; module
  `crates/trove-core/src/finance/import.rs`)
- **domains:** `finance/` (contract: **document** — the canonical ledger
  as built; the recorded write-time-dedupe exception. Phase 3 writes the
  spec page without redesigning it.)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (drop a downloaded statement file on Trove)
- **connection:** none
- **evidence:** built and validated on real exports (Chase card, Chase
  checking, generic header-sniff); bank portal download paths verified in
  the research doc
- **effort / priority:** S / P0
- **needs:** privacy (financial detail — user-initiated import is the
  explicit opt-in) · extension: OFX/QFX parsing is the next format after
  CSV

## What it is

File import for bank and card statements downloaded from any bank portal.
A **permanent peer backend, not a fallback**: it is the only route to
Apple Card / Apple Cash, Venmo, Cash App, and deep history past
aggregator windows (SimpleFIN's upstream cap is ~90 days). Anyone with a
bank gets value from this on day one, no subscription or login required.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Per-bank CSV presets | none | date, amount, description, category (bank-dependent) | built; Chase card + checking validated |
| Generic CSV (header-sniff) | none | best-effort column mapping | built |
| OFX/QFX | none — planned next format | unambiguous parse, no column guessing | research doc; still offered by smaller banks/CUs |
| Balance backfill | none — noted follow-up | running-balance column → daily snapshots | research doc note |

All optional in the ledger shape; per-bank presets are added on demand
when a user submits their header row.

## Access & auth

User downloads from their bank portal (typically 90-day or custom-range
exports): Chase CSV via Activity & Orders → Download; Fidelity CSV (OFX
dropped Jan 17 2026); Amex CSV; BofA CSV (OFX/QFX dropped Sep 30 2025).
OFX/QFX remains valid for smaller banks and credit unions. No auth, no
TCC, no network — fully local parse of a user-chosen file routed through
`Vault::resolve`.

## Vault mapping

- **Raw/canonical layer:** the canonical ledger —
  `finance/accounts.jsonl`, `finance/transactions/<account-id>/<year>.jsonl`,
  `finance/balances/<account-id>.jsonl`. Imports dedupe into the shared
  per-account files at write time (the recorded exception in
  `vault-spec/conventions.md`); re-running an overlapping export never
  duplicates.
- **Contract layer:** same files — `finance/` is its own documented
  contract. Account matching/alias adoption ties statement files to
  SimpleFIN-synced accounts.

## Build plan

Already shipped (def `csv-import`, the registry's generic import box).
Remaining pipeline work:

1. OFX/QFX parser (next format; unambiguous spec, no header guessing) —
   small, fixture-driven addition to `import.rs`.
2. Checking-CSV running-balance backfill for net-worth-over-time.
3. New per-bank presets on demand from user-submitted header rows.
4. Phase 3 documentation pass covers this in the same
   `vault-spec/domains/finance.md` page as SimpleFIN.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Chase card/checking + generic CSV | 🧪 (shipped pre-pipeline; real-export validated — David promotes to ✅) | drop a fresh bank CSV on the import box; confirm new rows in `finance/transactions/<account>/`, re-drop and confirm zero duplicates |
| OFX/QFX | — | not built yet; validate with a real OFX from a credit union once the parser lands |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Bank/Card
Statement CSV, OFX, QFX Import (L3691–L3697). Feasibility 🟢 high. Major
banks (BofA, Chase, Fidelity) have been dropping OFX/QFX in favor of CSV
— format support priorities may shift over time. This importer is also
the landing path for the queued P2P briefs (PayPal, Venmo, Cash App) and
the `apple-card` monthly statement export.
