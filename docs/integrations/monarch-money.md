# Monarch Money

- **id:** `monarch-money`
- **domains:** `finance/` (canonical ledger — document contract, the
  as-built shape with write-time cross-source dedupe)
- **status:** 🧪 built (parser confirmed correct; dispatch parked pending real-export end-to-end validation)
- **unavailable_reason:** none
- **behavior:** Import (CSV export preset on the existing file importer;
  the unofficial API is explicitly skipped)
- **connection:** none (file import, no login)
- **evidence:** primary sources — help.monarch.com "Downloading Transaction or
  Account History" (403-walled to automated fetch, confirmed via help.403fin.io
  + QuickBankConvert, Feb 2026 currency); column set confirmed:
  `Date, Merchant, Category, Account, Original Statement, Notes, Amount, Tags`;
  sign convention confirmed: debits negative. No official API as of June 2026;
  unofficial GraphQL libs rejected.
- **effort / priority:** M / P2
- **needs:** privacy (financial detail — opt-in with explicit acknowledgement)

## What it is

A popular subscription personal-finance app (the main Mint successor).
Users who track there have years of categorized transactions across all
their accounts — a one-stop history seeder for Trove, exactly like the
Copilot Money import that's already shipped and validated.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Transaction history (CSV) | any paid plan (Monarch is subscription-only) | date, amount, merchant, category, account, notes/tags (exact columns Needs-sample) | community-confirmed export path |
| Unofficial GraphQL API | n/a — rejected | live accounts/transactions | community libs; fragile + ToS risk |

All fields optional in the ledger shape; whatever the CSV carries beyond
the core row lands in `extra`.

## Access & auth

- User-driven export: Monarch → Settings → Export Data → CSV; drop the file
  on Trove's import box. No credentials, no network, no TCC.
- The unofficial API requires the user's username/password driven through
  reverse-engineered GraphQL — breaks with app updates, violates ToS for
  automated credential use, and is a maintenance trap. **Not built.**
  Upgrade to a Periodic CloudSync def only if Monarch ships an official
  API.

## Vault mapping

- **Raw layer:** original file preserved under `finance/imports/` like
  every statement import.
- **Contract layer:** rows join the canonical `finance/` ledger (the
  recorded write-time-dedupe exception) — same path as Copilot. Monarch
  categories/tags go in `extra`; dedupe via the existing cross-source
  fuzzy matcher against SimpleFIN/bank-CSV rows, plus account-mask alias
  adoption as done for Copilot.

## Build plan

1. New preset in the existing import engine (`finance/import.rs` family) —
   header-sniff signature for Monarch's export, like the Copilot preset.
   No new module/def needed if it rides `csv-import`'s registry entry;
   confirm whether the hub should surface "Monarch Money" as its own
   import card (Copilot precedent says yes, via the importer's preset
   list).
2. **Parser-last:** column set is undocumented — acquire a real export
   (any Monarch user; David may not have an account) before writing the
   mapper. Check sign convention against the vault rule (Copilot's was
   inverted; expect surprises).
3. Fixtures from the sample (anonymized); dedupe tests against overlapping
   SimpleFIN fixtures.
4. Privacy gate: financial detail — import is inherently explicit user
   action, which satisfies opt-in; no background collection exists.

## Build notes (2026-06-17)

- `Behavior::Import` wired; `run_import` calls `vault.finance_import_csv` which
  falls through to the generic `detect_mapping` path (date/amount/merchant columns).
- `MonarchMoneyCols` struct + `monarch_money_columns` recognizer wired with
  **confirmed** real-export column names in `finance/import.rs`.
- `import_monarch_money` function confirmed correct in `finance/import.rs`:
  uses `negate=false` (debits already negative — opposite of Copilot Money).
  Dispatch remains commented out pending real-export end-to-end validation.
- 9 tests pass: hub card, params, opt-in, idempotent re-import, last_data,
  recognizer fires for confirmed shape, rejects old-scaffold shape, rejects Copilot.
- Column shape confirmed from primary sources (Feb 2026 currency):
  `Date, Merchant, Category, Account, Original Statement, Notes, Amount, Tags`
- Sign convention confirmed: debits negative, credits positive.

### What changed in the fix pass (2026-06-17)

Original build used wrong column names derived from the GraphQL API schema
(`Name`, `Original Date`, `Institution`) rather than the CSV export columns.
Fix verified against help.monarch.com (403-walled) via corroborating primary
sources (help.403fin.io, QuickBankConvert, Feb 2026):
- `MonarchMoneyCols`: renamed `name` → `merchant`, dropped `original_date` +
  `institution`, added `original_statement` as the unique fingerprint column.
- `monarch_money_columns` recognizer: now requires `"original statement"` as the
  primary differentiator (replaces old `"name"` + `"account"` combo).
- `import_monarch_money`: removed `negate(&raw_amount)` call — amounts already
  negative for outflows (vault convention). Field refs updated to match new struct.
- Fixture: updated to confirmed column order `Date,Merchant,Category,Account,
  Original Statement,Notes,Amount,Tags` with correct negative-outflow sign.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import (generic path) | built — parked parser | generic `detect_mapping` path works for any CSV with date+amount+merchant; confirmed via 9 module tests |
| CSV import (Monarch-specific parser) | confirmed-correct, dispatch parked | column names + sign convention confirmed from primary sources; un-park dispatch in `finance_import_csv` after real-export end-to-end validation; verify ledger rows, category passthrough in `extra["category"]`, `original-statement` in `extra`, account splitting, and zero dupes vs. SimpleFIN |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Monarch
Money (L3875–L3881). Feasibility 🟠 low *for the API*, but the research
verdict is explicit: ship the CSV preset (safe, ToS-clean, the Copilot
pattern), defer all API work unless an official API launches. Research
also notes users migrating Monarch → Lunch Money (which has a first-class
API and its own queued brief) — the import is a seeder, not an ongoing
sync.
