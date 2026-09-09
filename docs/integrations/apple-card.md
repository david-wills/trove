# Apple Card, Cash & Savings

- **id:** `apple-card`
- **domains:** `finance/` (contract: **document** — canonical ledger as
  built, the recorded write-time-dedupe exception)
- **status:** 🧪 scaffold
- **unavailable_reason:** none
- **behavior:** Import (per-month statement export dropped on Trove)
- **connection:** none (file import; no login)
- **evidence:** export path documented (Wallet → Apple Card → Statements →
  Export Transactions; web at card.apple.com) with CSV/OFX/QFX/QBO formats
  and known CSV columns — but no real sample file yet (**sample-required**
  before the preset locks)
- **effort / priority:** S / P2
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-sample (a real Apple Card CSV to pin the preset)

## What it is

Apple's consumer financial products: Apple Card (credit card), Apple Cash
(P2P balance), and Apple Savings. Uniquely, **no desktop aggregator can
reach them** — SimpleFIN cannot connect, and FinanceKit (the live-sync API
Copilot/Monarch/YNAB use) is iOS-only and entitlement-gated. The monthly
statement export is the only desktop route, which makes this import preset
the sole way an Apple Card user gets that spending into Trove.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Apple Card transactions | none | Transaction Date, Clearing Date, Description, Merchant, Category, Type, Amount (USD) — per-month CSV/OFX/QFX/QBO | documented export path + column list (research doc); sample-required |
| Apple Savings transactions | none | same Wallet export interface as Apple Card | research doc notes |
| Apple Cash statements | none | PDF only (last 12 months, emailed) — **not parsed in v1** | research doc notes |
| Live sync (FinanceKit) | blocked | n/a — iOS-only + Apple entitlement; needs an iOS companion app | research doc L3923–L3929; iceboxed |

All optional in the ledger; Apple Cash stays an honest gap ("PDF statements
only — not yet importable") until PDF→text extraction exists.

## Access & auth

- iPhone: Wallet → Apple Card → Card Balance → Statements → Export
  Transactions → CSV/OFX/QFX/QBO (one file per month). Web: card.apple.com
  → Statements → Export Transactions. Apple Savings uses the same Wallet
  interface.
- Apple Cash: Wallet → Request Statement → PDF to Apple ID email (12 mo).
- No API, no auth held by Trove, no TCC. Standalone-clean: pure file
  import. FinanceKit is explicitly out of scope for the macOS app (iOS
  17.4+, US-only, per-bundle-ID entitlement) — iceboxed until an iOS
  companion app is otherwise motivated.

## Vault mapping

- **Raw layer:** imported files preserved via the existing import pipeline
  (`finance/imports/`), as `csv-import` already does.
- **Contract layer:** canonical finance ledger rows; Apple-specific fields
  (Clearing Date, Category) ride in `extra`. Sign convention checked
  against the vault's (the Copilot lesson).
- **Dedupe:** the ledger's write-time cross-source matcher. Two real
  overlaps: users who seeded Apple Card history via the Copilot export, and
  month-boundary re-imports of the same statement.

## Build plan

1. ✅ Separate `apple_card.rs` module with `Behavior::Import` DEF — hub card,
   import box (CSV), last-data hook, setup copy, and caveats all wired.
2. ✅ **Parked scaffold:** `apple_card_columns` recognizer + `import_apple_card`
   in `finance/import.rs`; parked dispatch comment in `finance_import_csv`.
   Until a real CSV confirms column names and sign convention, imports fall
   through to the generic `detect_mapping` path (which correctly finds
   `Amount (USD)` and `Transaction Date`/`Merchant`).
3. ✅ 9 scaffold tests: column recognizer, sign flip, raw layer, occurrence
   counter, skip-bad-rows, hub card, reimport idempotence — all green.
4. **Needs-sample:** re-wiring checklist in both `apple_card.rs` and
   `finance/import.rs`. When a real CSV lands: confirm exact header spelling,
   sign convention for Amount (USD), and whether Description + Merchant are
   both present; then restore the dispatch block in `finance_import_csv`.
5. OFX/QFX variants ride the planned QFX parser follow-up (not v1 scope).
6. Apple Cash PDF extraction is a later capability, not v1.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Apple Card CSV import | — | export one real month from card.apple.com; drop on Trove; confirm ledger rows, correct signs, Merchant/Category in `extra` |
| Savings import | — | same export flow from the Savings card in Wallet |
| Copilot-overlap dedupe | — | import a month already covered by a Copilot seed; confirm no duplicate rows |
| FinanceKit live sync | n/a | blocked (iOS-only + entitlement); revisit with an iOS companion app |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Apple Card /
Apple Cash / Apple Savings (L3835–L3841) and §FinanceKit iOS Companion
(L3923–L3929, 🔴 blocked, M4/XL — folded into this brief per
combine-by-provider). Feasibility 🟢 high for the CSV path. Cross-cutting
note applies verbatim: file import is a permanent peer backend, not a
fallback — it is the *only* route to Apple's financial products on desktop.
