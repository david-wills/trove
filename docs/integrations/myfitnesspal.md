# MyFitnessPal

- **id:** `myfitnesspal`
- **domains:** `health/nutrition/` (contract: `health-nutrition` — bound by
  Cronometer pioneer; reused here per follower pattern)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (Premium "Download Your Data" ZIP — one-shot
  retrospective import)
- **connection:** none (export is downloaded by the user from their account;
  Trove never holds MFP credentials)
- **evidence:** official-docs — documented Premium export ZIP with 3 CSVs
  (Meal Level Nutrition Details, Progress History, Exercise History). The
  public API is closed/invite-only as of 2026 — no M5 path.
- **effort / priority:** S / P2
- **needs:** Needs-login (validation needs a Premium account — $10/mo gate on
  the export itself) · Needs-sample (exact Premium export column names
  unconfirmed — parser is provisional/alias-table-based)
- **parser_parked_needs_sample:** true — meal-nutrition column headers inferred
  from python-myfitnesspal/types.py + athlete_data_warehouse field list; no
  real Premium export sample on disk to confirm. Raw layer is unconditional.

## What it is

MyFitnessPal is the biggest consumer food-logging app. Its Premium data
export carries per-meal macro *and* micronutrient detail with timestamps —
the best micronutrient coverage of any nutrition-app export per the research
doc — plus weight/measurement progress history and exercise history. For
anyone with years of food logs, this is the only way to get that history
into the vault: the API is closed to new developers, so the export is the
path, full stop.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Meal-level nutrition | **Premium/Premium+ only** (free users cannot export at all) | macros + micros per meal, timestamps | official export docs |
| Progress history | Premium | weight, body measurements over time | official export docs |
| Exercise history | Premium | logged exercises | official export docs |

All optional in the contract. The Premium gate is on the *export mechanism*,
not on fields — there is no free-tier degraded mode; without Premium the
import simply has nothing to ingest (the import box copy must say so
honestly, per the disabled-controls-need-affordance rule).

## Access & auth

- Export: myfitnesspal.com → Settings → Account → Download Your Data
  (Premium/Premium+ only) → email link within ~1 hour → ZIP with the 3 CSVs.
  User drops the ZIP (or CSVs) on Trove.
- No API path: public API closed/invite-only as of 2026 — do not invest in
  it (mirrors the LinkedIn lesson).
- No credentials held, no TCC, fully offline once downloaded.
  Standalone-clean.
- iPhone users who let MFP write HealthKit already get daily nutrition
  *totals* via the shipped Apple Health import; this import adds the per-meal
  breakdown and pre-HealthKit history.

## Vault mapping

- **Raw layer:** `health/nutrition/myfitnesspal/raw/` — the 3 CSVs as
  received, one dated snapshot folder per import.
- **Contract layer:** rows per the nutrition contract — one row per logged
  entry (food item or meal-group rollup, depending on export variant), with
  `ts`, `source`, `guid`, meal slot, optional food name, energy + macro fields,
  micro fields omit-if-empty, overflow in `extra`. Progress-history weight rows
  route to the `health/` body-mass metric stream (data routes by shape, not
  provider); exercise history stays in the raw layer pending contract decisions.
- **Dedupe:** `guid` from full ts (with clock time) + meal + food label + amount
  hash (mirroring the Cronometer recipe so two rows for the same meal logged at
  different times within a day get distinct guids, matching the documented MFP
  export behaviour).

## Build status (2026-06-16 — fix rev)

- Module `crates/trove-core/src/myfitnesspal.rs`: DEF (Import/Behavior,
  no connection), reuses `health_nutrition::Entry` contract bound by Cronometer.
- Accepts ZIP (primary) or bare CSV. ZIP handler extracts all CSVs verbatim to
  `health/nutrition/myfitnesspal/snapshots/` (unconditional full fidelity) and
  routes the meal-nutrition CSV to the parser.
- Meal-nutrition parser: provisional alias-table-based (see below). Progress
  and exercise CSVs: raw-only (brief says those routes are read-time concerns).
- Raw JSONL layer (`health/nutrition/myfitnesspal/raw/YYYY-MM.jsonl`): every
  parsed CSV row stored verbatim, month-partitioned.
- Contract layer (`health/nutrition/myfitnesspal/YYYY-MM.jsonl`):
  `health_nutrition::Entry` rows, deduped by `guid` (content hash of
  ts + meal + food label + amount — food name optional, handles both per-food
  and meal-rollup export shapes).
- 16 tests pass; cargo check green.
- **Parser is provisional (Needs-sample):** column names inferred from
  python-myfitnesspal/types.py and the athlete_data_warehouse MFP integration.
  Export granularity (per-food vs. meal-rollup) unconfirmed — parser handles
  both. When a real export lands, tighten `TYPED_MACROS`/`FOOD_ALIASES`/
  `DATE_ALIASES`/`MEAL_SLOT_ALIASES` alias tables and update fixtures.
- **Fix rev changes:** (1) food name is no longer required — rollup rows
  without a food column still produce contract entries; (2) guid now includes
  full ts + amount so same-meal different-time rows are distinct; (3) fixed
  raw_files double-count in the no-meal-CSV ZIP branch.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Meal-level nutrition | — | export from a real Premium account; drop ZIP into the import box; confirm contract rows in `health/nutrition/myfitnesspal/` + raw snapshot + hub last-data (needs a Premium subscription — Needs-login) |
| Progress history | — | same import; confirm weight rows land in the `health/` body-mass metric CSV without duplicating Apple Health rows |
| Exercise history | — | same import; confirm raw-layer presence |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §MyFitnessPal (L1041–L1047). Feasibility 🟢 high. One-shot
retrospective, not continuous — fine: the ongoing-logging story for nutrition
is Apple Health passthrough or Cronometer. Premium paywall means free users
have *no* export path at all. Best micronutrient coverage of any nutrition
app export; Cronometer (free export, 100+ nutrient columns) is the sibling
source the nutrition contract is drafted against.
