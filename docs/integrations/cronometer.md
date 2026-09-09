# Cronometer

- **id:** `cronometer`
- **domains:** `health/nutrition/` — **first-in-domain collector; this build
  binds the `health-nutrition` contract** (the new `NutritionEntry` Rust type in
  `crate::health_nutrition` + the `health-nutrition` DOMAINS entry in
  `contracts.rs`; the `health-nutrition.entry` fixture is promoted from a Phase-3
  draft to the ratified set in `spec_validation`). myfitnesspal / macrofactor /
  lifesum / levels (food-log) follow this same shape.
- **status:** 🧪 built (fixture-tested, not validated) — **Needs-sample** (a real
  Cronometer Servings CSV export to validate against)
- **unavailable_reason:** none
- **behavior:** `Behavior::Import` — a repeatable retrospective import of the free
  web CSV export; no schedule, no credentials (Trove never touches cronometer.com).
  Registry projection in `docs/integration-schedule.md`: `cronometer` Not-wired →
  `Import` ("manual; runs when you import a file").
- **connection:** none (the export is downloaded by the user from cronometer.com;
  Trove never holds Cronometer credentials).
- **default:** off (`default_on: false`, `toggleable: false`) — an import source,
  nothing runs until the user drops a CSV.
- **evidence:** official-docs — free, documented web export producing clean CSVs
  (Diary / Servings / Biometrics / Exercises / Notes); community confirmation of
  the verbatim **Servings** column headers via the `gocronometer` library.
- **effort / priority:** S / P2
- **needs:** none — the `health-nutrition` contract is now **ratified by this
  build**; live validation needs a real Cronometer **Servings** export (a
  Needs-sample item — no login, no app registration).

## What it is

Cronometer is the nutrition tracker for people who care about *micronutrients* —
widely regarded as having the most complete micronutrient database of any
consumer app. Its free CSV export is correspondingly rich: a single **Servings**
(food-diary) row carries 80+ nutrient columns (full amino-acid profile, all
vitamins, minerals, omega-3/6, individual sugars) — unmatched breadth among
consumer apps. Free export, documented format, clean CSVs: the easiest
high-value source in the nutrition domain, and the **first collector** to write
the `health-nutrition` contract.

## What this build imports

The importer reads **only the Servings export — the per-entry food log** — and
maps each row to one `NutritionEntry`. Cronometer's *other* exports route by
shape and are out of scope here:

| Export | This build | Why |
|---|---|---|
| **Servings** (food diary) | ✅ imported → `health-nutrition` | the per-food entries with the full nutrient tail |
| Daily Nutrition (day totals) | — | day rollups; the per-entry Servings export supersedes it for this contract |
| Biometrics (weight/BP/glucose) | — | route by shape to the `health/` per-metric streams (already built) |
| Exercises | — | not a food log; stays per-source raw if/when added |
| Notes | — | diary notes; per-source raw if/when added |

The Gold tier ($) gates only per-entry **timestamps**: a free-tier Servings row
carries the diary date (`Day`) but no clock time (`Time` is blank), so it lands
at **day precision** (`ts = "YYYY-MM-DD"`, no fabricated clock); a Gold row's
`Day`+`Time` becomes a full local RFC3339 `ts`. Same code path, sparser `ts`
precision, never a special case.

## Access & auth

- Export: **cronometer.com → Profile → Account → Export Data → Export Servings →
  CSV download.** Free; repeatable whenever the user wants to refresh. The user
  drops the CSV on Trove's import box.
- **No official API.** The unofficial session-scraping path (gocronometer's
  approach — driving the same session-based export endpoint the web app uses) is
  fragile and ToS-risky — by decision, stay with the export; do not build the
  scrape.
- No credentials held, no TCC, fully offline once downloaded. Standalone-clean.
- iPhone users syncing Cronometer → HealthKit already land daily totals in the
  shipped Apple Health import; this import adds the full micronutrient breadth and
  any pre-HealthKit history. Overlap is a **read-time** reconciliation — each
  source keeps its own folder and stable guids; nothing is merged at write time.

## Vault mapping

- **Raw layer (unconditional, full fidelity):**
  `health/nutrition/cronometer/raw/YYYY-MM.jsonl` — each Servings row as a
  verbatim header→value JSON object, partitioned by the contract month.
- **Contract layer:** `health/nutrition/cronometer/YYYY-MM.jsonl` — one
  `NutritionEntry` per food row:
  - **Typed core macros** from their named columns → typed fields: `Energy
    (kcal)`→`energy_kcal`, `Protein (g)`→`protein_g`, `Carbs (g)`→`carb_g`, `Fat
    (g)`→`fat_g`, plus fiber / sugars / added-sugars / saturated /
    mono- & poly-unsaturated / sodium / cholesterol.
  - **The long micronutrient tail** (every other numeric column — `Iron (mg)`,
    `Omega-3 (g)`, `Tryptophan (g)`, …) → the nested `nutrients` map, keyed by the
    **verbatim Cronometer label-with-unit**. Name normalization is a read-time
    job. Blank cells are omitted (not zero); explicit zeros are kept.
  - `Group` → the closed `meal` enum (`breakfast`/`lunch`/`dinner`/`snack`) where
    it matches; the verbatim group always also rides in `extra.group`.
  - `Amount` (`"80.00 g"`) → `amount` 80.0 + `unit` `"g"`; the raw string is kept
    in `extra.amount_raw`. `Category` → `extra.category`.
- **`ts`:** `Day`+`Time` as a local RFC3339 where the export carries a time
  (Gold), else the date-only `Day` (free tier) — never a fabricated clock time.
- **Dedupe:** the CSV carries no row ids, so `guid` is a stable SHA-256 content
  hash (10 hex chars) of `Day|Time|Group|Food Name|Amount`; re-importing an
  overlapping range is idempotent (the letterboxd/readwise pattern) — files stay
  byte-identical on a re-run.

## Build plan — DONE (2026-06-15, INDEX #133)

1. ✅ Module `crates/trove-core/src/cronometer.rs`: `DEF` (`Behavior::Import`, no
   connection), header-name-matched Servings parser, raw + contract writes via
   `store` helpers, content-hash guids, idempotent re-import. One registry line
   (the `&crate::cronometer::DEF` already present from the Phase-2 stub — the
   generic import box now accepts `csv`).
2. ✅ **First-in-domain contract bind:** the `NutritionEntry` Rust type
   (`crate::health_nutrition`) + the `health-nutrition` DOMAINS entry in
   `contracts.rs` + the `health-nutrition.entry` fixture promoted to the ratified
   set in `spec_validation` (round-trip + doc-sync + required-list rows).
3. ✅ Fixtures from the documented Servings column set (Gold rows with a `Time`,
   a free-tier row with blank `Time`, a representative micronutrient tail);
   parser + raw/contract store + partition + idempotent-reimport tests, unique
   temp dirs.
4. No privacy gate per the catalog (food logging is not on the mandatory flag
   list).

## Validation matrix

Built + **fixture-green** (9 unit tests in `cronometer.rs` + 4 round-trip/serde
tests in `health_nutrition.rs` + `spec_validation` green + workspace `cargo
check` + regenerated `schedule_doc` + bindings clean). Promotion to ✅ needs a
real Cronometer **Servings** CSV (Needs-sample).

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Servings import (food diary) | 🧪 fixture | `full_import_writes_both_layers_partitions_and_dedupes`, `import_joins_every_generic_surface`, `run_import_reads_from_a_csv_file`. **David:** cronometer.com → Profile → Account → Export Data → **Export Servings** → drop the CSV into the **Cronometer** import box → confirm contract rows in `health/nutrition/cronometer/YYYY-MM.jsonl` (typed `energy_kcal`/`protein_g` + the `nutrients` map under verbatim labels) + the verbatim raw under `health/nutrition/cronometer/raw/` + the hub "last data" month. |
| Typed macros + nutrient tail | 🧪 fixture | `maps_a_gold_row_with_typed_macros_meal_amount_and_nutrient_tail`. **David:** in a contract row, confirm the named macros land in their typed fields and that vitamins/minerals (e.g. `Iron (mg)`) sit inside `nutrients` keyed by Cronometer's own label — and are **not** duplicated into the typed columns. |
| Free-tier day precision | 🧪 fixture | `free_tier_blank_time_lands_at_day_precision`. **David (free account):** confirm a free-tier row's `ts` is the date-only `YYYY-MM-DD` (no invented clock time) and still files into the correct `YYYY-MM.jsonl`. |
| Gold per-entry timestamps | 🧪 fixture | (same path; covered by the Gold-row mapping test). **David (Gold subscriber only):** export from a Gold account → confirm a row's `ts` carries the logged clock time (`...T08:14:00…`). Any Gold user's run validates this leg. |
| Meal slot + custom groups | 🧪 fixture | `custom_group_rides_extra_and_yields_no_meal_enum`. **David:** a default group (Breakfast/Lunch/Dinner/Snacks) sets `meal`; a renamed/custom group leaves `meal` empty but survives in `extra.group`. |
| Idempotent re-import | 🧪 fixture | `full_import_writes_both_layers_partitions_and_dedupes` (re-run leg), `guid_is_stable_and_distinguishes_time_and_food`. **David:** drop the **same** export a second time → 0 imported / all duplicates → the `YYYY-MM.jsonl` files are byte-identical. |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Cronometer (L1049–L1055) + the emphasis duplicate §Cronometer CSV
export detailed (L1145–L1151). Feasibility 🟢 high. Gold tier needed for
per-entry timestamps (meal-timing analysis); unofficial session API is
fragile/ToS-risky — export-only, by decision. Scheduled exports don't exist;
this stays a repeatable manual import. **Build note:** the importer is
Servings-only by design (Biometrics route to `health/`; Exercises/Notes are not
yet imported) — the Servings export is the per-entry food log the
`health-nutrition` contract is shaped around.
