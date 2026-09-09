# MacroFactor

- **id:** `macrofactor`
- **domains:** `health/nutrition/` (contract: **Phase 3 pending** — nutrition
  shape drafted from Cronometer + MyFitnessPal + MacroFactor together)
- **status:** 🧪 built (parser parked — Needs-sample to confirm export column headers)
- **unavailable_reason:** none
- **behavior:** Import (user exports CSVs from the app; drop into Trove)
- **connection:** none (file import — no login flow in Trove; the *user's*
  MacroFactor subscription is what gates the export)
- **evidence:** official in-app export (Settings → Export Your Data →
  Granular/Quick Export, documented CSV set); unofficial Rust crate
  `macro-factor-api` (Firestore REST) exists as a future-spike note only
- **effort / priority:** S / P2
- **needs:** Needs-login (an active MacroFactor subscription is required to
  produce the export — validation needs a subscriber; David may not be one)
  · nutrition contract not yet ratified (Needs-David)

## What it is

MacroFactor is a popular adherence-neutral macro/calorie tracker with an
adaptive-expenditure algorithm (TDEE estimated from intake + weight trend).
Its export captures the energy-balance story — logged calories/macros,
smoothed weight trend, estimated expenditure, and program targets — which
is data no wearable produces.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Calories & macros log | active subscription (app is sub-only) | per-day calories, protein, carbs, fat | official export |
| Weight trend | active subscription | scale weights + smoothed trend weight | official export |
| Expenditure estimate | active subscription | daily estimated TDEE | official export |
| Targets | active subscription | program calorie/macro targets over time | official export |

All optional in the (pending) contract. Export is macro-focused — thinner
micronutrient detail than Cronometer; rows simply omit what isn't there.

## Access & auth

- In-app: Settings → Export Your Data → **Granular Export** (per data type)
  or **Quick Export** (summary). CSVs for weight trend, expenditure,
  calories/macros, targets. User moves the files to the Mac and drops them
  on the import box.
- No API: official API doesn't exist; the unofficial `macro-factor-api`
  crate reads Firebase/Firestore with the user's credentials — unsupported,
  fragile, a future M5 spike at most. Not part of this build.
- No TCC, no network. Standalone-clean.

## Vault mapping

- **Raw layer:** `health/nutrition/macrofactor/raw/` — imported CSVs
  preserved as received (per export-type files).
- **Contract layer:** `health/nutrition/macrofactor/` rows per the pending
  Phase 3 nutrition contract (expected: per-day intake rows — `ts`,
  `source`, calories/macro fields; weight-trend and expenditure rows carry
  MacroFactor-specific fields in `extra`). Until ratification, raw import
  lands and normalization follows the contract pass.
- **Dedupe:** `guid` from export type + date (per-day granularity);
  re-imports overwrite-idempotent.

## Build plan

1. Module `crates/trove-core/src/macrofactor.rs`: `DEF` (Import), generic
   import box accepts the export CSVs (single files or the set).
2. One registration line in `INTEGRATIONS`. No `CONNECTION`.
3. Fixtures: research doc describes the export set but column layouts are
   not reproduced — **parser-last, Needs-sample**: get a real Granular
   Export from a subscriber before finalizing the parser; build the import
   plumbing and tests around fixture CSVs once obtained.
4. Note in the hub copy that Apple Health export already carries
   MacroFactor's HealthKit-written nutrition totals for iPhone users; this
   import adds trend/expenditure/targets detail.
5. Vault writes via `store` helpers; contract rows after the nutrition
   contract ratifies.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| CSV import (all export types) | Needs-sample | active subscriber runs Granular Export, drops files on the import box; confirm raw files + rows + hub last-data; confirm exact column header names match alias table in macrofactor.rs |
| Quick Export variant | Needs-sample | same flow with Quick Export; confirm parser handles the summary layout |

## Build notes (2026-06-16)

Module `crates/trove-core/src/macrofactor.rs` built as `Behavior::Import` writing to the `health-nutrition` contract (`crate::health_nutrition::Entry`). Two layers unconditionally: raw (`health/nutrition/macrofactor/raw/YYYY-MM.jsonl`) + contract (`health/nutrition/macrofactor/YYYY-MM.jsonl`). Day-level rows per the domain spec: date-only `ts`, day-rollup macros in typed columns, MacroFactor-specific fields (expenditure, trend weight, targets) in `extra`.

**Parser parked (Needs-sample):** The exact column headers are not publicly documented. The alias table in the module covers the most probable headers (inferred from the domain-spec example row + common nutrition-tracker conventions), but must be confirmed/corrected against a real Granular Export before the parser is considered final. The raw layer preserves every original header verbatim regardless.

The guid scheme is per-day-per-column-fingerprint: `hash("mf|{date}|{sorted-present-column-names}")`. Different Granular Export types (calories, expenditure, weight-trend, targets) carry different columns → different fingerprints → distinct guids → all coexist without false-dedup. Re-importing the identical file produces the same fingerprint → still idempotent. Scale weight columns (`Scale Weight (kg/lbs)`) are raw-only per the health-nutrition spec (body measurements excluded from this domain contract); trend weight is contract-blessed in `extra`. Integer-valued extras (expenditure_kcal, target_calories, etc.) serialize as integers to match the spec example.

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §MacroFactor (L1057–L1063). Feasibility 🟢 high. Cross-cutting
note 2: any HealthKit-connected nutrition app already reaches the shipped
Apple Health export — this brief is the *additional* detail layer.
Sequence with Cronometer/MyFitnessPal so the nutrition contract is drafted
against all three shapes at once.
