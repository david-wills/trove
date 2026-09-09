# Domain: health-nutrition

Food logging — what the user ate, one record per logged entry, from every
nutrition tracker. Cronometer, MyFitnessPal, MacroFactor, Lifesum, and the
food-log portion of Levels all write this shape; readers see one diet timeline
regardless of which app produced it. Core macros (energy_kcal, protein, carb, fat)
are typed columns so the common query never digs into a sub-object, while the
long micronutrient tail — Cronometer alone exposes 80+ nutrients per food — rides
a nested `nutrients` map so that fidelity survives without 80 columns. The same
meal reaching the vault through two apps (a MyFitnessPal export and an Apple
Health passthrough of the same day) writes its own rows with its own guids and
reconciles at read time. **Scope:** only food logs live here. A nutrition app's
*other* exports route by shape — weight/body-measurement rows go to `health/`,
Levels' raw CGM glucose readings go to `health/`, exercise logs stay per-source
raw — never into this contract. Lose It! has no export path of its own and is
`CoveredBy(health)` via Apple Health, so it writes no folder here.

- **Layout:** `health/nutrition/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/health-nutrition.entry.schema.json`](../schemas/health-nutrition.entry.schema.json)
- **Dedupe key:** `guid` (source-unique: a per-entry id where the source has one
  — Levels/MyFitnessPal log ids — else a content hash of date + meal + food +
  amount, since most nutrition CSV exports carry no row ids). Re-imports of
  overlapping export ranges must skip already-stored guids before appending.

## Entry

One logged food entry per line. Only `ts`, `source`, `guid` are required — they
place and identify the record; everything else is omit-if-empty. A rich
Cronometer row carries macros plus a `nutrients` map of micros; a MyFitnessPal
row carries per-meal macros with a timestamp; a MacroFactor day-rollup is just an
entry with day-precision `ts` and no per-food `food`. `ts`
is the most identity-bearing time the source exposes: the eaten/logged time where
the export has one (MyFitnessPal Premium, Cronometer Gold, Levels), or the
date-only `YYYY-MM-DD` of the diary day where it does not (Cronometer free tier,
MacroFactor per-day rows).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local time the food was eaten/logged, **or** a date-only `YYYY-MM-DD` for a day-precision diary row the source never timestamped |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id, the dedupe key |
| `meal` | string | | `"breakfast"` \| `"lunch"` \| `"dinner"` \| `"snack"` — the meal slot, where the source records one |
| `food` | string | | food / item name, verbatim |
| `brand` | string | | brand or manufacturer, where the source carries one |
| `amount` | number | | quantity consumed, paired with `unit` |
| `unit` | string | | unit for `amount` (`"g"`, `"oz"`, `"cup"`, `"serving"`, …), verbatim |
| `energy_kcal` | number | | energy in kilocalories (consumer "Calories") |
| `protein_g` | number | | protein, grams |
| `carb_g` | number | | total carbohydrate, grams |
| `fat_g` | number | | total fat, grams |
| `fiber_g` | number | | dietary fiber, grams |
| `sugar_g` | number | | total sugars, grams |
| `added_sugar_g` | number | | added sugars, grams |
| `saturated_fat_g` | number | | saturated fat, grams |
| `polyunsaturated_fat_g` | number | | polyunsaturated fat, grams |
| `monounsaturated_fat_g` | number | | monounsaturated fat, grams |
| `sodium_mg` | number | | sodium, milligrams |
| `cholesterol_mg` | number | | cholesterol, milligrams |
| `nutrients` | object | | the long micronutrient/extended-macro tail: nutrient label → number in the source's native unit (the unit rides in the key, e.g. `"Vitamin C (mg)"`, `"Sodium (mg)"`, `"Omega-3 (g)"`). Keys are the source's own labels — normalizing nutrient names across apps is a read-time job |
| `extra` | object | | everything source-specific (Levels Zones score, MacroFactor estimated expenditure / trend weight, food notes, source-native food/recipe ids, …) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-06-10T08:14:00-07:00","source":"cronometer","guid":"33f1c0a7e2","meal":"breakfast","food":"Oats, Rolled, Dry","brand":"Bob's Red Mill","amount":80,"unit":"g","energy_kcal":311,"protein_g":10.7,"carb_g":54.8,"fat_g":5.3,"fiber_g":8.0,"sugar_g":0.8,"saturated_fat_g":0.9,"sodium_mg":4,"cholesterol_mg":0,"nutrients":{"Vitamin C (mg)":0.0,"Iron (mg)":3.5,"Magnesium (mg)":138.0,"Omega-3 (g)":0.08,"Tryptophan (g)":0.18}}
{"ts":"2026-06-10T13:02:00-07:00","source":"myfitnesspal","guid":"mfp-log-7781201","meal":"lunch","food":"Chicken Burrito Bowl","brand":"Chipotle","amount":1,"unit":"serving","energy_kcal":625,"protein_g":42,"carb_g":58,"fat_g":22}
{"ts":"2026-06-11","source":"macrofactor","guid":"mf-2026-06-11-total","energy_kcal":2180,"protein_g":158,"carb_g":201,"fat_g":74,"extra":{"expenditure_kcal":2640,"trend_weight_kg":81.4}}
```

## Read-time semantics (FYI for writers)

The nutrition reader scans `health/nutrition/*/`; creating your source folder is
the registration. Daily totals are summed from the entries within a date range,
grouped by `meal` where present; a day-precision row contributes to its day
without a fabricated clock time. The typed macro columns answer the common
energy_kcal/protein/carb/fat queries directly; the `nutrients` map is the
full-fidelity tail a micronutrient view reads when it needs it. The same diet day
arriving from two apps (a direct export and an Apple Health passthrough) stays as
separate rows with separate guids — cross-source dedupe and precedence are a
read-time opinion, never a write-time merge. Write the true eaten/logged time
where the export gives one and a date-only `ts` where it does not; never invent a
clock time. Per-source raw fidelity (the original CSVs) lives under
`health/nutrition/<source>/raw/`; this contract is the normalized convergence,
not a superset of every tracker's columns.
