# Venmo

- **id:** `venmo`
- **domains:** `finance/` (canonical ledger — contract status: **document**;
  the shape already exists in code, Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (CSV/JSON data export; re-runnable)
- **connection:** none — file import, no login. Venmo exposes no public API
  for individuals (its Plaid integration is inbound-only, for linking
  external accounts) and no aggregator can reach it.
- **evidence:** official-docs — export path documented: account.venmo.com →
  Privacy → Request Your Data → Transaction History (CSV or JSON); also a
  direct statement-download URL while logged in. Column layout not carried
  in the research evidence — fixture from a real export before shipping.
- **effort / priority:** S / P2
- **needs:** privacy (financial detail, and payment **notes** are
  message-like content — ships opt-in with explicit acknowledgement on
  import)

## What it is

Venmo peer-payment history — who paid whom, when, how much, and the note
attached to each payment — via the official data export. Venmo is a walled
garden for aggregators: SimpleFIN and bank CSVs see only the settlements
to/from the linked bank, never the peer-level detail or notes. The export
is the only complete source, and the notes make it unusually rich social
context for a finance stream.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Peer payments | all accounts | date, from/to, amount, **note**, status, txn id | official export path |
| Merchant payments | all accounts | merchant, amount, date | research notes |
| Balance transfers | all accounts | bank transfer in/out rows | research notes |

All optional in the ledger row; notes and counterparty handles ride in
`extra` (or the ledger's memo field if one fits).

## Access & auth

- Export: account.venmo.com → Privacy tab → Request Your Data →
  Transaction History → CSV or JSON. Direct URL while logged in:
  `https://account.venmo.com/api/statement/download?startDate=…&endDate=…&csv=true`.
- No auth in Trove, no TCC, no network. Standalone-clean by construction.
- No public API exists for individuals — recorded so the card can say why
  there's no Sync button.

## Vault mapping

- **Raw layer:** `finance/` is the recorded write-time-dedupe exception —
  the import dedupes into shared per-account files: the Venmo balance
  registers in `finance/accounts.jsonl`, rows in
  `finance/transactions/<account>/<year>.jsonl`.
- **Contract layer:** canonical ledger rows; `guid` = Venmo transaction ID;
  signed amounts per vault convention; note, counterparty username, and
  payment type in `extra`. Settlements to the linked bank also appear as
  that bank's rows (and possibly in a linked PayPal download) —
  reconciliation at read time, never write time.
- **Dedupe:** transaction ID as `guid`; re-import appends nothing.

## Build plan

1. Venmo preset in the existing finance import (`finance/import.rs`
   header-sniff family) + thin `venmo.rs` DEF (Import) for the hub card
   and import box; prefer the CSV shape, accept the JSON variant if cheap.
   Registration line in `INTEGRATIONS`.
2. **Parser-last:** export-path evidence only, no schema evidence — obtain
   a real export and build fixtures from it (payment in, payment out,
   merchant, bank transfer, declined/cancelled rows).
3. Tests: sign convention, guid dedupe on re-import, note preservation
   byte-for-byte in `extra`; unique temp dirs.
4. Onboarding copy: the Privacy-tab path (non-obvious), and that bank-side
   settlement rows coexisting is expected.
5. Privacy gate: opt-in acknowledgement (financial detail + payment notes
   as message-like content).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Export import | Needs-sample | request a real export from account.venmo.com; drag into the import box; confirm account + rows (with notes) in `finance/`, hub last-data |
| Re-run safety | ✅ tested | import the same file twice; row count unchanged (scaffold test) |
| Settlement overlap | — | confirm a Venmo cash-out shows once in the read-time merged view despite a matching bank row |
| Column recognizer | Needs-sample | once a real CSV is on-disk: run `venmo_columns` recognizer; update scaffold if column names differ; re-wire dispatch in `finance_import_csv` |

## Build notes (2026-06-17)

- Replaced the NotWired stub with a full `Import` DEF following the `apple_card.rs` parked-parser pattern.
- Added `VenmoCols` struct + `venmo_columns()` recognizer + `import_venmo()` parser to `finance/import.rs` as parked scaffolds — column names are community-reported, NOT confirmed against a real export.
- Scaffold assumes `"+ $50.00"` / `"- $25.00"` amount encoding (handled by `clean_amount` after stripping spaces/`$`).
- Scaffold assumes ISO-8601 datetime column (time stripped by `parse_date`).
- 15 tests pass: hub card, column recognizer (accept/reject), parser internals (guid, sign, note, from/to, idempotent, raw layer, date parse, skip bad rows, description fallback).
- `parser_parked_needs_sample=true` — the specific column dispatch is NOT wired in `finance_import_csv`. Imports fall through to the generic `detect_mapping` path (which may handle basic cases but won't parse the `+ $50.00` amount encoding or social fields correctly). Re-wire once a real export confirms the shape.
- No new CONNECTION, no Cargo deps, no shared contract files touched.

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Venmo
Transaction Export (L3811–L3817). Feasibility 🟢 high — export "covers
full account history." Not reachable by any aggregator, so this is not a
nice-to-have duplicate: peer-level detail and notes exist nowhere else.
PayPal's download captures Venmo transfers that settle via a linked PayPal
balance — partial overlap handled at read time.
