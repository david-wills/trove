# Robinhood

- **id:** `robinhood`
- **domains:** `finance/` (contract: **document** — the canonical ledger as
  built; Phase 3 writes the spec page)
- **status:** 📋 queued
- **unavailable_reason:** none
- **behavior:** Import (CSV via the generic import pipeline; Robinhood preset)
- **connection:** none (user downloads their data from the Robinhood app/web)
- **evidence:** account "Download my data" path documented in the research
  doc; no official retail API; unofficial reverse-engineered libraries
  (robin-stocks) are fragile and explicitly rejected. Export column format is
  undocumented — sample-required.
- **effort / priority:** S / P1
- **needs:** privacy (financial detail — opt-in with explicit
  acknowledgement) · Needs-sample (export CSV columns unconfirmed)

## What it is

Hugely popular US retail brokerage, especially with younger investors —
stocks, options, and crypto trades. Robinhood intentionally limits data
portability: no official API, and the export covers trades and transfers but
not live positions. The CSV preset is honestly the v1 ceiling, and the brief
should not promise live sync.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Trade history | none | executed trades (columns unconfirmed) | research doc; Needs-sample |
| Transfers | none | deposits/withdrawals | research doc; Needs-sample |
| Live positions | — | **not in the export** — manual snapshot only, no programmatic path | research doc |

All optional in the contract (omit-if-empty); no tier code paths.

## Access & auth

- Main export: app/web → Account → Settings → Privacy & Security →
  "Download my data" → CSV (trade history + transfers). **No date-range
  picker on the main download**; older history comes statement-by-statement
  via Reports and Statements → Generate Report → CSV.
- No auth, no TCC, no network. Standalone-clean.
- Unofficial API (github.com/robin-stocks) exists but is unsupported,
  fragile, and a ToS risk — out of scope for a production integration.

## Vault mapping

- **Raw layer:** imported files preserved under `finance/imports/` per the
  existing import pipeline.
- **Contract layer:** trades and transfers normalize into the canonical
  finance ledger (the recorded write-time-dedupe exception). Brokerage
  fields (symbol, quantity, price) ride in `extra` until the ledger spec
  page formalizes a trade sub-shape. Transfers dedupe against the matching
  bank-side rows from SimpleFIN/statements via the existing cross-source
  matcher.
- **Dedupe:** guid from (date, symbol/side or transfer direction, amount);
  re-importing the cumulative "Download my data" file must be idempotent.

## Build plan

1. **Parser-last** — export format is undocumented folklore; the preset is
   written only once a real sample lands (flagged Needs-sample).
2. `crates/trove-core/src/robinhood.rs` `DEF` (Import) + one registration
   line; card copy documents the friction honestly (no date range on the
   main export; per-statement reports for older history; positions not
   included).
3. Generic header-sniff CSV import is the interim path — a Robinhood file
   may already parse loosely today; the preset adds correct signs/columns.
4. Fixtures from the first real sample; idempotent re-import test (the
   export is cumulative).
5. Privacy: financial detail — standard finance opt-in acknowledgement.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Trades + transfers import | — | request "Download my data" from a real account; drop CSV on the import box; confirm ledger rows + hub last-data; re-import → zero dupes |
| Older history via reports | — | generate one Reports-and-Statements CSV for a past range; confirm it merges without duplicating the main export's rows |

## Research notes

`integrations-research.md` → "Financial, Spending & Purchases" §Robinhood
Data Export (L3755–L3761). Feasibility 🟡 medium — the export works but
Robinhood limits portability by design. SnapTrade reaches Robinhood for live
sync but is catalogued unavailable (developer-held keys); revisit only if a
BYOK story ever ships. CSV preset + Import is the agreed v1.
