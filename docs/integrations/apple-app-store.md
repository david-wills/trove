# App Store & iTunes Purchases

- **id:** `apple-app-store`
- **domains:** `finance/purchases/` (contract: **Phase 3 pending** —
  purchase line-item shape, per-source subfolders)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Apple Privacy data-export ZIP dropped on Trove)
- **connection:** none (file import; no login held by Trove)
- **evidence:** official export path documented (privacy.apple.com → Data
  and Privacy → Get a copy of your data → App Store Activity CSV); exact
  CSV columns not enumerated anywhere in the research doc —
  **sample-required**
- **effort / priority:** M / P2
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-sample (App Store Activity CSV format
  undocumented; parser built last)

## What it is

Lifetime purchase history across Apple's storefronts: App Store, iTunes,
Apple TV+, Apple Music, Apple Arcade — apps, media, subscriptions, in-app
purchases. Particularly good for subscription tracking and digital-spending
analysis: these charges show up on card statements as undifferentiated
"APPLE.COM/BILL" rows, and this export is what itemizes them.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Purchase history (all storefronts) | none | app/item name, purchase date, amount, category (per research doc; full column set unverified) | official privacy-export path; sample-required |
| Subscription detection | none | recurring rows in the same data | derived, read-time |
| Live API / direct download | n/a | none exists | research doc |

All optional in the (pending) contract. reportaproblem.apple.com shows the
same history in-browser but offers no structured download — view-only, not
a collector path.

## Access & auth

- privacy.apple.com → Data and Privacy → Get a copy of your data → select
  **App Store Activity** → Apple emails a download link (within hours for
  most users) → ZIP with the activity CSVs.
- No API, no credentials held by Trove, no TCC. Standalone-clean.
- The same Apple Privacy ZIP carries other Trove-relevant exports (Maps
  search history, Siri usage, …). The research doc recommends one **Apple
  Privacy ZIP importer** that routes each inner dataset by shape; this def
  owns the App Store Activity slice and should be built so the ZIP-walking
  scaffolding is reusable by future Apple-export defs.

## Vault mapping

- **Raw layer:** `finance/purchases/apple-app-store/` — export rows at
  full fidelity, partitioned by purchase-date month (taxonomy path).
- **Contract layer:** purchase line-item rows per the **pending Phase 3
  contract** (merchant = Apple storefront; item name, amount, category;
  storefront/subscription details in `extra`).
- **Dedupe:** `guid` from the export's order/transaction identifier if the
  sample provides one; otherwise a content hash of (date, item, amount) —
  decided when a real sample lands. Re-imports idempotent.

## Build plan

1. Module `crates/trove-core/src/apple_app_store.rs` (def id
   `apple-app-store`): `DEF` (Import), registry-driven import box accepting
   the Apple Privacy ZIP or the bare App Store Activity CSV.
2. **Parser-last:** the CSV format is undocumented — no public column
   spec. Ship the def + ZIP plumbing; lock the parser and dedupe key only
   against a real export (Needs-sample). David can generate one from his
   own Apple ID during validation, but any user's export serves.
3. ZIP-walker: locate the App Store Activity files inside Apple's folder
   structure; ignore (don't fail on) the other datasets; structure the
   walker for reuse by later Apple-Privacy-export defs.
4. Onboarding copy: request → wait for Apple's email → download → drop the
   ZIP on Trove; note the moderate friction honestly.
5. Fixtures from the obtained sample (redacted); parser + store tests with
   unique temp dirs. Contract rows follow purchases-contract ratification
   (raw import can ship first).

## Build notes (2026-06-17)

- Module `crates/trove-core/src/apple_app_store.rs` — `Behavior::Import` wired; accepts ZIP and bare CSV.
- ZIP walker: `is_app_store_activity()` matches any `.csv` whose path contains `App Store Activity` or `App_Store_Activity` (case-insensitive); reusable for future Apple Privacy export defs.
- Raw layer: every CSV row written unconditionally to `finance/purchases/apple-app-store/raw/YYYY-MM.jsonl` as a flat JSON object with `_raw_ts` + `_raw_guid` (content hash for idempotent re-imports).
- Contract parser: **PARKED** (`PARSER_ACTIVE = false`). No real export sample exists to confirm column names. The `try_map_to_line_item` stub is in place with a re-wiring checklist in the module doc. Set `PARSER_ACTIVE = true` after confirming headers against a real export.
- Date-guessing heuristic: tries YYYY-MM-DD, MM/DD/YYYY, DD-Mon-YYYY, YYYY/MM/DD from "date"/"purchase"-keyed columns; falls back to import time.
- 13 tests pass; `cargo check` clean.
- `contract_mode`: reuse-bound (`finance-purchases.LineItem`) — raw written now; contract rows follow when sample confirms columns.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Privacy-ZIP import | ✅ built (raw layer) | request App Store Activity at privacy.apple.com; drop the ZIP; confirm rows in `finance/purchases/apple-app-store/raw/` + hub last-data |
| Bare-CSV import | ✅ built (raw layer) | extract the CSV and drop it directly; same raw rows |
| Contract rows (LineItem) | ⏸ parked Needs-sample | confirm column headers from a real export; implement `try_map_to_line_item`; set `PARSER_ACTIVE = true` |
| Subscription rows | ⏸ deferred | derived from contract rows; unblock after contract parser lands |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Apple App
Store / iTunes Purchase History (L3851–L3857). Feasibility 🟡 medium — the
path works but means waiting on Apple and parsing a format with no public
spec (hence Needs-sample, parser-last). Research recommendation was "build
later," consistent with P2. iOS App Store *receipts* (developer-facing
in-app-purchase receipts) are a different concept and out of scope. Vault
path follows the taxonomy (`finance/purchases/`), which supersedes the
research entry.
