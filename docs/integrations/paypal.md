# PayPal

- **id:** `paypal`
- **domains:** `finance/` (canonical ledger — contract status: **document**;
  the shape already exists in code, Phase 3 writes the spec page without
  redesigning it)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (CSV activity download; re-runnable)
- **connection:** none — file import, no login. PayPal's REST API requires
  developer-held app credentials (not user-generated keys), so the API path
  is not standalone-distributable; it also covers only 3 years vs the
  download's 7. The file is the full solution, not a fallback.
- **evidence:** official-docs — PayPal activity download path documented
  (paypal.com/reports/dlog; CSV/TAB, 7-year range, 50k records/file).
  Column layout not carried in the research evidence — fixture from a real
  export before shipping.
- **effort / priority:** S / P2
- **needs:** privacy (financial detail — ships opt-in with explicit
  acknowledgement on import)

## What it is

PayPal account activity — purchases, peer payments, refunds, balance
transfers — via the official CSV activity download. PayPal sits outside
most bank feeds (SimpleFIN sees only the settlements, not the PayPal-side
detail), so the export is the only complete record. Covers up to 7 years
of history in one pull.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activity history (7 yr) | all accounts | date, type, counterparty, gross/fee/net, currency, status, txn id | official download path |
| Venmo settlements | when linked | transfers to/from a linked PayPal balance | research notes |

All optional in the ledger row; fee/currency detail goes to `extra` if it
doesn't fit the canonical fields.

## Access & auth

- Export: PayPal account → Activity → Download → CSV or TAB; date range up
  to 7 years; max 50k records per file (split/ZIP if larger). URL shortcut:
  `paypal.com/reports/dlog`.
- No auth in Trove, no TCC, no network. Standalone-clean by construction.
- The REST Transaction Search API (`/v1/reporting/transactions`) is
  recorded for honesty: OAuth app with developer-held credentials — the
  hard-block credential model — and a 3-year window. Not built.

## Vault mapping

- **Raw layer:** `finance/` is the recorded write-time-dedupe exception —
  the import dedupes into shared per-account files: PayPal balance
  registers in `finance/accounts.jsonl`, rows in
  `finance/transactions/<account>/<year>.jsonl`.
- **Contract layer:** canonical ledger rows; `guid` = PayPal transaction
  ID column; signed amounts per the vault convention (watch sign
  conventions — Copilot needed inversion); type/fee/counterparty email in
  `extra`. Transactions that also settle to a linked bank account appear
  in that bank's rows too — reconciliation at read time, never write time.
- **Dedupe:** transaction ID as `guid`; re-import of overlapping ranges
  appends nothing.

## Build plan

1. PayPal preset in the existing finance import (`finance/import.rs`
   header-sniff family, like the Chase/Copilot presets) + a thin
   `paypal.rs` DEF (Import behavior) so the hub shows the card and import
   box. Registration line in `INTEGRATIONS`.
2. **Parser-last:** the export path is documented but the column layout
   evidence is path-level only — get a real export file first and build
   fixtures from it (multi-currency, refund, fee-bearing rows).
3. Tests: sign convention, guid dedupe on re-import, 50k-split files
   imported in sequence; unique temp dirs.
4. Onboarding copy: the dlog URL, the 7-year range tip, and the note that
   bank-side settlements are expected to coexist.
5. Privacy gate: opt-in acknowledgement (financial detail).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Activity import | ✅ built | download a real CSV via paypal.com/reports/dlog; drag into the import box; confirm account + rows in `finance/`, hub last-data |
| Re-run safety | ✅ tested | import the same file twice + an overlapping range; row count unchanged (Transaction ID dedupe) |
| Large history | — | a 7-year export (split files); all parts import, totals plausible vs PayPal's UI |

## Build notes (2026-06-16)

- **Needs-sample:** no real export on disk. Schema derived from PayPal's official
  Activity Download Report docs (developer.paypal.com, 87 columns) and cross-referenced
  parser projects. "Confirmed" language in the original build notes was overstated;
  all claims are secondary-source until a real paypal.com/reports/dlog CSV is inspected.
  The adversarial review (2026-06-16) surfaced this and the fixture was updated accordingly.
- Column schema: `"Date","Time","TimeZone","Name","Type","Status","Currency","Gross","Fee",
  "Net","From Email Address","To Email Address","Transaction ID","CounterParty Status",
  "Shipping Address","Address Status","Item Title",... (87 total)`. "CounterParty Status"
  at position 14 (after Transaction ID) was absent from the original fixture — fixed.
  It is marked "Unselected" by default so most real exports will not include it; the
  name-based parser handles both shapes correctly.
- Sign convention (derived from secondary sources, unverified): `"Net"` appears to be
  already outflow-negative — no flip applied. This is NOT the Copilot-inversion trap.
  Verify against a real file before marking confirmed.
- Date format `"MM/DD/YYYY"` handled by existing `parse_date`.
- Guid: `"Transaction ID"` (stable PayPal primary key); re-import of overlapping ranges
  is a clean no-op via exact id match.
- Parser lives in `finance/import.rs` (`paypal_columns` + `import_paypal`), dispatched
  from `finance_import_csv` before Cash App (PayPal's "Time" + "TimeZone" + "Gross"
  + "Net" + "Transaction ID" signature is unique).
- Raw layer: `finance/paypal/raw/<account>/YYYY.jsonl` — verbatim all columns.
- Contract layer: `finance/transactions/<account>/YYYY.jsonl` via existing upsert.
- Multi-currency: row-level `"Currency"` field preserved in both `Transaction.currency`
  and `extra["currency"]`; account currency set from first row.
- Pending detection: `Status == "Pending"` (case-insensitive) sets `pending: true`.
- 4 unit tests: layout recognition, reimport no-op, account auto-registration, sign
  conventions. All green.

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §PayPal
Activity Download (L3803–L3809). Feasibility 🟢 high. M1 file-import is
the deliberate, complete path — the API is both credential-blocked for
standalone distribution and strictly worse (3-year window). The download
also captures Venmo transfers settling through a linked PayPal balance,
which partially overlaps the `venmo` provider's export (read-time concern).
