# Samsung Health

- **id:** `samsung-health`
- **domains:** `health/` (contract: **document** — per-metric CSV + per-source
  raw, as built; Phase 3 writes the spec page without redesigning it)
- **status:** 🧪 built (Needs-sample — vitals parser parked pending a real export ZIP)
- **unavailable_reason:** none
- **behavior:** Import (user-initiated "Download personal data" ZIP from the
  Samsung Health app — one-shot historical import)
- **connection:** none
- **evidence:** official self-service export — Samsung Health app → More →
  Settings → Download personal data → ZIP of per-category CSVs. The export
  exists officially but the CSV schemas are not publicly documented:
  sample-required for the parser.
- **effort / priority:** M / P2
- **needs:** privacy (the ZIP can include ECG, blood pressure, glucose,
  stress, and cycle-adjacent health categories — opt-in with explicit
  acknowledgement) · Needs-sample (CSV schemas undocumented — parser-last)

## What it is

Samsung Health is the Android-world counterpart to Apple Health: the
aggregation hub for Galaxy Watch wearables, Samsung scales, and
HealthKit-style app writes. Trove is a Mac app, so there is no live path —
the value here is **historical import** for users who switched from Android
(or still carry a Galaxy Watch) and would otherwise lose years of steps,
sleep, heart-rate, and body-composition history. Per the research doc's
cross-cutting notes, the Samsung CSV export is also one of only two reliable
paths to body-composition detail (the systematic Apple Health blind spot).

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Activity | none | steps, floors, workouts | official export (CSV categories); sample-required |
| Vitals | none | heart rate, blood oxygen, blood pressure, blood glucose, ECG, temperature | official export; sample-required |
| Sleep | none | sleep sessions with stages | official export; sample-required |
| Stress | none | stress-level samples | official export; sample-required |
| Body composition | none | weight + fat/muscle/water (Samsung scales) | official export; sample-required |

All optional in the contract — a phone-only user's ZIP simply lacks the
watch/scale categories. No paid tiers.

## Access & auth

- Export is produced **on the user's phone**: Samsung Health app → More →
  Settings → Download personal data → ZIP with one CSV per category; the user
  moves the ZIP to the Mac and drops it on Trove. No credentials, no API, no
  TCC — pure M1 import.
- The Samsung Health Data SDK and Health Connect are Android-only — no
  macOS/Rust path exists, and none should be attempted (standalone rule:
  nothing to absorb here anyway).
- Fully offline once the ZIP is on disk. Standalone-clean.

## Vault mapping

- **Raw layer:** `health/samsung-health/raw/` — the export's CSVs preserved
  as received (full fidelity first), one import = one dated snapshot folder.
- **Contract layer:** the documented health shape (per-metric CSV, as built):
  steps, heart rate, sleep, SpO2, BP, glucose, body-mass rows join the same
  per-metric streams the Apple Health import writes; Samsung-only categories
  (stress score, segmental body comp) get new per-metric CSVs following the
  same conventions. Workouts route whole to `health/` per the taxonomy.
- **Dedupe:** `guid` from category + start-timestamp (+ device id if the CSVs
  carry one — confirm against the sample). Watch for overlap with Apple
  Health rows in dual-ecosystem vaults; same merge rule as other importers
  (timestamp+metric exact match).

## Build plan

1. Module `crates/trove-core/src/samsung_health.rs` (def id
   `samsung-health`): `DEF` (Import, no connection), one registry line; the
   generic import box handles the ZIP drop.
2. **Parser-last (Needs-sample):** acquire a real "Download personal data"
   ZIP before writing the parser — the per-category CSV schemas are not
   publicly documented. Build the ZIP walker + category router first; add
   per-category parsers from the sample, most-common categories first
   (steps, HR, sleep), long tail incrementally.
3. Fixtures cut from the sample (anonymized); parser + store + dedupe tests,
   unique temp dirs.
4. Privacy gate: ships opt-in (ECG/BP/glucose/stress are detailed medical
   data) — explicit acknowledgement on enable.
5. Mirror the shipped Apple Health import's streaming/partitioning patterns
   (`health.rs` is the in-repo reference for a health-hub importer).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Core metrics (steps/HR/sleep) | ⚠️ Needs-sample | export ZIP from a real Samsung Health account; drop into the import box; confirm raw rows in `health/samsung-health/raw/` + hub last-data |
| Vitals (HR/BP/SpO2/glucose) | ⚠️ Needs-sample | same ZIP — if column names match the candidate lists in `samsung_health.rs` they route to `health/medical/samsung-health/observations/` as contract Observations; otherwise fall through to raw (no data lost) |
| Body composition | ⚠️ Needs-sample | ZIP from an account with a Samsung scale; raw-only until field names are confirmed |

## Build notes (2026-06-21, fixed 2026-06-21)

Module `crates/trove-core/src/samsung_health.rs` built as `Behavior::Import` (ZIP + CSV).  42 unit tests green.

- Two-layer write: raw unconditionally (`health/samsung-health/raw/<category>/YYYY-MM.jsonl`) + contract vitals (`health/medical/samsung-health/observations/YYYY-MM.jsonl` via `health_medical::Observation`).
- Two-row header detection: Samsung CSVs prepend a metadata row (`com.samsung.health.heart_rate,<version>,...`); `strip_metadata_row` distinguishes it from the column-header row (which has `com.samsung.health.heart_rate.start_time,...`) by checking whether the first CSV field carries a trailing `.field_name` suffix — metadata rows don't, column-header rows do.
- Filename router: `com.samsung.health.*` / `com.samsung.shealth.*` / `tracker.*` → HeartRate / BloodPressure / BloodOxygen / Glucose → contract; Steps / Sleep / Exercise / BodyComposition / Stress → raw-only.
- Column matching: **suffix-based** (`resolve_col_by_suffix`) — matches headers by their last dot-segment (case-insensitive), so both the real namespaced form (`com.samsung.health.heart_rate.start_time`) and any bare short form (`start_time`) resolve the same logical field.  Candidate suffixes: timestamp=`start_time`/`time`/`timestamp`; HR value=`heart_rate`/`bpm`/`value`; SpO2=`blood_oxygen`/`spo2`; BP=`systolic`/`diastolic`; glucose=`glucose`/`blood_glucose`; offset=`time_offset`.
- Timestamp handling: Samsung exports UTC wall-clock values.  Naive datetimes (the common export form) are now parsed as UTC via `Utc.from_utc_datetime`, then the sibling `*.time_offset` column (e.g. `UTC+0800`) is read and applied as a `FixedOffset` to produce the correct local timestamp.  Epoch-ms and RFC3339 forms also supported.
- None-value guard: `Observation` rows are only written when a numeric value is successfully parsed from the value column; rows where the value column is absent or unparseable remain in the raw layer only.
- GUID uniqueness: `content_guid` now includes the value string so distinct readings at the same `start_time` (e.g. hourly-summary min vs. avg) produce distinct guids.
- BOM/flexible CSV: leading UTF-8 BOM stripped before parsing; all `csv::Reader` instances use `flexible(true)` to tolerate trailing-comma / ragged rows.
- All LOINC-coded: HR 8867-4, BP-sys 8480-6, BP-dia 8462-4, SpO2 59408-5, glucose 2339-0.
- Needs-sample: column-suffix assumptions must be verified against a real "Download personal data" ZIP; the module gracefully falls to raw-only when guesses don't match.
- No new deps, no new ConnectionDef, no CONNECTIONS line needed.

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Samsung Health
(L974–L980). Feasibility 🟡 medium — the export itself is reliable and
self-service; the M rating is schema breadth, not access. Primarily a
historical-import use case (ex-Android users, Galaxy Watch owners without
Apple Health). Cross-cutting note 8 (L992): Samsung Health CSV is one of two
reliable body-composition paths. Samsung Health also syncs to Google
Fit/Health Connect — both Android-side, irrelevant to the Mac app.
