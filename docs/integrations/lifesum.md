# Lifesum

- **id:** `lifesum`
- **domains:** `health/nutrition/` (contract: **Phase 3 pending** — nutrition
  shape drafted from Cronometer + MyFitnessPal + MacroFactor; Lifesum joins it)
- **status:** 🧪 built (parser parked — Needs-sample)
- **unavailable_reason:** none
- **behavior:** Import (user downloads export from the Lifesum website, drops
  it into Trove)
- **connection:** none
- **evidence:** official export mechanism — lifesum.com/account/export-data
  (7-day quick export); full history via GDPR support request. Export *format*
  is undocumented — **sample-required**. No public API.
- **effort / priority:** S / P2
- **needs:** Needs-sample (export format undocumented — parser built last,
  against a real file)

## What it is

Popular nutrition/meal-logging app (strong in Europe). On iOS it writes
nutrition data to HealthKit, so the shipped Apple Health export already
captures a Lifesum user's daily totals indirectly. A direct Lifesum import
adds the original per-food-entry detail (individual foods and meals vs.
aggregated HealthKit totals) — incremental value, hence P2 / build-later in
the research doc.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| 7-day quick export | free (web login) | recent food/meal entries (exact fields unknown — Needs-sample) | official export page, research doc L1181 |
| Full-history GDPR dump | free, manual support request | complete logging history (format unknown) | research doc L1181 |
| Apple Health passthrough | iOS users | daily nutrition totals — already captured by the shipped `health` def | research doc L1183, cross-cutting note 2 |

All capability fields are optional in the contract (omit-if-empty); tiering
never needs special code paths.

## Access & auth

- Export: `lifesum.com/account/export-data` — direct 7-day export after web
  login; full history requires contacting support (GDPR dump). No public API,
  no OAuth, no token.
- No TCC, no local files, no connection def. Pure Import behavior via the
  registry-driven import box.
- Standalone-clean: nothing runs, nothing polls; the user brings the file.

## Vault mapping

- **Raw layer:** `health/nutrition/lifesum/raw/` — the export files as
  delivered, full fidelity (exact partitioning decided once a sample shows
  the shape; likely per-export-file).
- **Contract layer:** `health/nutrition/lifesum/` rows per the pending Phase 3
  nutrition contract (expected shape from the Cronometer/MFP/MacroFactor
  draft: per-entry `ts`, `source`, food name, energy/macros, micros optional,
  overflow in `extra`). `guid` from a stable per-entry id if the export has
  one, else a content hash of (ts, food, amount).
- **Overlap note:** daily totals may duplicate Apple-Health-imported numbers
  for iOS users — raw stays complete in both folders; dedupe/preference is a
  read-time concern, never a write-time one.

## Build plan

1. **Parser-last.** Export format is undocumented — do not write the parser
   from guesses. Acquire a real 7-day export (any Lifesum account works;
   flag stays **Needs-sample** until one exists in fixtures).
2. Module `crates/trove-core/src/lifesum.rs`: `DEF` with Import behavior; no
   `CONNECTION`. One registration line in `INTEGRATIONS`.
3. Fixtures from the sample (redacted); parser + store tests with unique temp
   dirs; handle both the 7-day quick export and the GDPR full dump if their
   shapes differ.
4. Vault writes via `store` helpers once the nutrition contract is ratified;
   raw-layer import can land first (full fidelity first, normalization
   second).
5. UI copy on the import card should note the Apple Health overlap so iOS
   users understand what this adds (per-entry detail).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| 7-day export import | parked (Needs-sample) | export from lifesum.com/account/export-data, drop into the import box; confirm raw file under `health/nutrition/lifesum/raw/` + hub last-data; per-entry parsing waits for a real sample |
| Full-history GDPR dump | parked (Needs-sample) | request via Lifesum support; import; confirm raw storage; per-entry parsing waits for field confirmation |

## Build notes (2026-06-16)

- Module upgraded from `NotWired` stub to `Behavior::Import` with raw-layer
  verbatim storage (`health/nutrition/lifesum/raw/`).
- Contract layer (`health/nutrition/lifesum/YYYY-MM.jsonl`,
  `health_nutrition::Entry`) is designed and referenced but **parked** — the
  Lifesum export format is undocumented and no real sample exists. The
  `entries_from_export` function is the only piece waiting on a sample.
- All attempts to find official export documentation failed (the export page
  requires auth; no open-source parsers found; no help-center article
  accessible). The `Needs-sample` flag stays until a real export file is
  provided.
- 6 tests green; cargo check clean. No new deps or connections added.

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Lifesum (L1177–L1183); at-a-glance L1022 (feasibility 🟡 Medium).
Build-later in the research doc because Apple Health passthrough already
covers totals for iOS users; the direct export earns its place by carrying
the per-food-entry originals. Sequence after Cronometer/MyFitnessPal/
MacroFactor, which define the nutrition contract this joins.
