# Copilot Money

- **id:** `copilot-money` (shipped as a preset of def `csv-import`;
  parser in `crates/trove-core/src/finance/import.rs`)
- **domains:** `finance/` (contract: **document** — the canonical ledger
  as built; the recorded write-time-dedupe exception. Phase 3 writes the
  spec page without redesigning it.)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** CoveredBy(csv-import) — one export file, one drop
- **connection:** none
- **evidence:** built and validated on a 9,044-row real export
  (2020–2026, 13 accounts); export path confirmed in-app; Copilot has no
  API — export-only confirmed as of 2026
- **effort / priority:** S / P0
- **needs:** privacy (financial detail — user-initiated import is the
  explicit opt-in)

## What it is

Copilot Money is a popular iOS/Mac personal-finance app. Its CSV export
is Trove's **primary day-one history seeder**: a user who already tracks
in Copilot gets years of categorized, multi-account transaction history
in one drop — exactly what fills the gap behind SimpleFIN's ~90-day
window.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Full transaction history | Copilot subscription (export itself is free in-app) | date, name, amount, status, category, parent category, account, account mask, excluded, type, note, recurrings | built; 9,044-row validation |
| Account mapping | none | account mask → alias adoption onto SimpleFIN accounts | built |

Copilot categories ride along in the ledger record; nothing is dropped.

## Access & auth

Copilot app → Settings → Account → Export Transactions → one
`transactions.csv` covering every account and year (web app at
copilot.money also exports). No API exists. No auth, no TCC, no network —
local parse of a user-chosen file.

## Vault mapping

- **Raw/canonical layer:** the canonical ledger —
  `finance/accounts.jsonl`, `finance/transactions/<account-id>/<year>.jsonl`.
  Write-time dedupe into shared per-account files (the recorded
  exception); re-import never duplicates.
- **Contract layer:** same files. Two handled quirks: Copilot's signs are
  inverted vs. the vault convention (spending positive — normalized at
  parse); cross-source dedupe against bank rows uses relaxed
  Copilot-specific matching because Copilot merchant names differ from
  raw bank descriptions. Account-mask alias adoption lets SimpleFIN
  accounts absorb their Copilot history.

## Build plan

Already shipped as the Copilot preset in `csv-import` (the in-app import
copy names it explicitly). Remaining pipeline work:

1. None functional. Keep the preset's fixtures as the regression guard
   for the sign-inversion and relaxed-matching logic.
2. Phase 3 documentation pass covers it within
   `vault-spec/domains/finance.md` (note the dedupe-exception rationale).
3. If Copilot ever ships an API, that becomes a new Periodic def sharing
   this brief — combine-by-provider.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Full-history import | 🧪 (shipped pre-pipeline; validated on a 9,044-row real export, 13 accounts — David promotes to ✅) | export from Copilot, pick the Copilot option in the import dropdown, confirm per-account year files populate and re-import adds zero rows |
| Alias adoption | 🧪 (shipped pre-pipeline) | with SimpleFIN connected, confirm Copilot history lands under the same account ids the sync uses |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Copilot
Money Export (L3699–L3705). Feasibility 🟢 high. Not time-sensitive —
the export always covers full history. Monarch Money (queued, P2) gets
the same treatment: ship the safe CSV-export preset, skip fragile
unofficial APIs.
