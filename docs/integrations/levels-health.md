# Levels

- **id:** `levels-health`
- **domains:** `health/` (glucose/CGM time series + biometrics) and
  `health/nutrition/` (food logs with glucose-response Zones scores — contract:
  **Phase 3 pending**, nutrition shape drafted from Cronometer + MyFitnessPal +
  MacroFactor)
- **status:** 🧪 built (parser-parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (Levels web CSV export — repeatable retrospective import)
- **connection:** none (export downloaded by the user from levels.com; Trove
  never holds Levels credentials; there is no public API)
- **evidence:** official-docs — `support.levels.com/article/105-export` lists
  the CSV set (Glucose/CGM, Activity Logs, Food Logs with nutritional metadata,
  Zones scores). No public API documented.
- **effort / priority:** S / P2
- **needs:** privacy (continuous glucose + food logs are medical data — opt-in
  with explicit acknowledgement) · Needs-login (validation only — to produce a
  real export; build proceeds from the documented CSV set) · nutrition contract
  not yet ratified (Needs-David)

## What it is

Levels is a metabolic-health subscription app ($200+/yr) that pairs a
continuous glucose monitor (CGM) with food logging and computes proprietary
**Zones** scores — a glucose-response rating per meal. The CGM stream itself is
better sourced directly from Dexcom/Abbott (Levels just resells their device
data), so the **unique** value here is the food↔glucose correlation: Zones
scores and the annotated food log. A niche but growing audience; ingest it when
a user has Levels rather than treating it as a primary glucose source.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Glucose / CGM CSV | active subscription | timestamped glucose readings | official export |
| Food Logs | active subscription | food entries with nutritional metadata + linked glucose response | official export |
| Zones scores | active subscription (Levels-proprietary) | per-meal glucose-response score | official export |
| Activity Logs | active subscription | logged activity entries | official export |
| Biometrics | active subscription | weight/other biometric entries | official export |

All optional in the contract; everything requires an active subscription (no
free tier yields data). No tier-specific code paths.

## Access & auth

- Web CSV export only: in the Levels app/web, follow
  `support.levels.com/article/105-export` to download the CSV set. No public,
  documented API; Levels pulls CGM data from Dexcom/Abbott under the hood.
- User drops the export on the import box. No OAuth, no TCC, no network from
  Trove. Standalone-clean.
- Glucose overlaps the `dexcom`/`freestyle-libre` briefs — prefer those for the
  raw CGM stream; Levels is ingested for the Zones/food-correlation layer.

## Vault mapping

- **Raw layer:** `health/levels/raw/` — each export CSV stored verbatim
  (glucose, food, zones, activity, biometrics), full fidelity.
- **Contract layer:**
  - Glucose/CGM and biometrics map into `health/` as built (the document
    per-metric CSV + per-source raw shape — same target as Dexcom). Note the
    double-count hazard: a user running Dexcom *and* Levels gets the same
    readings twice; dedupe at read time by timestamp proximity, raw stays
    complete on both sides.
  - Food logs + Zones map into the pending `health/nutrition/` contract
    (food entries with nutrient metadata); the Levels-specific Zones score
    lands in `extra` since no other nutrition source has it. Parked behind
    Needs-David (nutrition contract) until ratified.
- **Dedupe:** export content hash per file; per-reading `guid` from
  (metric + timestamp); per-meal `guid` from (timestamp + food).

## Build plan

1. Module `crates/trove-core/src/levels.rs`: `DEF` (Import). No connection.
2. Registration line in `INTEGRATIONS`.
3. CSV parsers for the documented export set (glucose, food, zones, activity,
   biometrics); route glucose/biometrics to `health/`, food/zones to
   `health/nutrition/`. Records route by shape.
4. Privacy gate: ships opt-in (CGM + food logs = medical data) with explicit
   acknowledgement.
5. Fixtures from the documented column sets (Needs-sample only if real headers
   differ from the support-article description — flag if so). Parser + dual-route
   store + dedupe tests, unique temp dirs.
6. Coordinate the nutrition mapping with the Cronometer/MyFitnessPal/MacroFactor
   briefs so Zones rides the shared nutrition contract.

## Build status (2026-06-16)

- **Status:** 🧪 built (scaffold + parser-parked)
- **Behavior:** `Import` (CSV files dropped by user; no credentials held)
- **Contract mode:** `reuse-bound`
  - Food log rows (+ Zones score) → `health_nutrition::Entry` under `health/nutrition/levels-health/YYYY-MM.jsonl`
  - Glucose/CGM rows → `health_medical::Observation` under `health/medical/levels-health/observations/YYYY-MM.jsonl`
  - Zones, Activity, Biometrics → raw-only under `health/levels-health/raw/`
- **Parser status:** PARKED — exact Levels CSV column headers are not publicly documented (no sample on disk). Column-name tables are best-effort guesses based on the support article description and standard CGM/nutrition export conventions. Column matching is always by header-name (never positional), so the parser tolerates header variations and routes unknown columns to `extra` / `nutrients`. A real export sample is needed to verify and correct the column names.
- **Flag:** Needs-sample

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Glucose import | scaffold/parked | drop a real Levels glucose CSV; verify `Timestamp (UTC)` and `Glucose Value (mg/dL)` headers match — update GLUCOSE_TS_COLS / GLUCOSE_VALUE_COLS if different; confirm rows in `health/medical/levels-health/observations/` |
| Food + Zones | scaffold/parked | drop a real Levels nutrition CSV; verify food-log column headers; confirm `zone_score` in `extra` of each entry |
| Zones CSV raw-only | scaffold/parked | drop the Zones CSV; confirm rows in `health/levels-health/raw/` only (no contract rows) |
| Dexcom de-dupe | read-time note | with both Dexcom and Levels imported, confirm read-time dedupe doesn't double-count overlapping readings (separate source folders; dedupe at read time by timestamp proximity) |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Levels Health (L1233–L1239). Feasibility 🟡 medium — export works,
but the glucose data duplicates Dexcom/Abbott, so the build is justified by the
proprietary Zones/food-correlation layer, not the CGM stream. Premium
subscription ($200+/yr), niche but growing audience — low priority. No public
API; the support-article CSV export is the only path.
