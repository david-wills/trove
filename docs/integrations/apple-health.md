# Apple Health

- **id:** `apple-health` (shipped def id: `health` — grandfathered
  pre-pipeline identity; the brief is the canonical record)
- **domains:** `health/` (contract: **document** — per-metric CSV +
  per-source raw, as built; Phase 3 writes the spec page without
  redesigning it) · `health/nutrition/` (contract: **Phase 3 pending**) ·
  `health/medical/` (contract: **Phase 3 pending**, FHIR-shaped — the
  CDA/FHIR drop extension routes here)
- **status:** 🧪 built (core export.zip import shipped pre-pipeline;
  extensions queued)
- **unavailable_reason:** none
- **behavior:** Import (drop export.zip); extension: watch-folder
  (Periodic) for Health Auto Export continuous sync
- **connection:** none
- **evidence:** official export format — shipped `health.rs` parses it
  with streaming quick-xml; Health Auto Export app documented at
  github.com/Lybron/health-auto-export; HKClinicalRecord is iOS-only
  (officially NOT in export.zip)
- **effort / priority:** M / P0
- **needs:** privacy (health detail; workout routes are location trails —
  the route/clinical extensions ship opt-in with explicit
  acknowledgement) · extensions are the remaining work (below)

## What it is

The platform health hub: iPhone Health app → Export All Health Data →
export.zip, the single richest health artifact most users can produce.
Nearly every consumer wearable (Garmin, Fitbit, Polar, Withings, Omron,
…) syncs into HealthKit, so this one import captures a large fraction of
all wearable data for iPhone users. The shipped `health` integration
already imports the XML; this brief's scope is **deepen, don't rebuild**
— one provider, many export.zip extensions plus a watch-folder mechanism
that makes collection continuous instead of one-shot.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Quantity/category records | none | steps, HR, HRV, SpO2, sleep stages, glucose, BP, body mass, VO2max, respiratory, temp, cycle tracking, mood, … | official format; shipped |
| Workouts | none | duration, distance, energy per HKWorkout | official format; shipped |
| Workout routes (GPX) | none — **not yet parsed** | timestamped lat/lon/ele tracks in `workout-routes/` | official format |
| ECG CSVs | Apple Watch | per-recording voltage CSVs subfolder | official format |
| Nutrition records | only if a HealthKit calorie app logs | DietaryEnergy/Protein/Carbs/Fat/Fiber/Water/Sodium + ~30 micros | official format |
| Environmental audio exposure | Apple Watch only | dB SPL samples + loud-environment events | official format |
| Mindfulness / state-of-mind / medication dosages | iOS version dependent | additional record types | official format |
| export_cda.xml + clinical FHIR/CDA drops | clinical records are NOT in export.zip — per-document share from iPhone only | conditions, labs, meds, immunizations | official (HKClinicalRecord) |
| Health Auto Export watch-folder | 3rd-party app; free tier limits metrics, $4.99/yr unlocks 150+ | continuous JSON/CSV via iCloud Drive | app docs |

All optional; users without an Apple Watch simply lack the Watch-only
rows. No tier code paths.

## Access & auth

- Manual: user-initiated export.zip drop — the drop *is* the consent. No
  TCC, no network. export.xml can exceed 1 GB unzipped; the shipped
  streaming quick-xml parse is essential, keep it.
- Watch-folder extension: Health Auto Export (App Store, HealthyApps)
  writes JSON/CSV to its iCloud Drive container
  (`~/Library/Mobile Documents/iCloud~com~healthautoexport~…/`) on an iOS
  Shortcuts schedule — readable on Mac without Full Disk Access. The app
  is a *user-side* exporter, not a runtime dependency, so the standalone
  rule holds; the card copy must say the app is required on iPhone.
- Clinical records never reach export.zip (iOS HealthKit only) — accept
  per-document CDA/FHIR file drops; direct SMART-on-FHIR pulls (see the
  `smart-on-fhir` brief) are the strictly better path.

## Vault mapping

- **Raw + metric layer:** `health/` as built — per-metric CSV + per-source
  raw; the recorded **document** contract. Extensions append new metric
  slugs (environmental audio, mindfulness, state-of-mind, dosages) to the
  same shape.
- **Workout routes:** GPX stays **with the workout record under
  `health/`** — records route whole; the location view joins at read time
  (taxonomy rule). Never split to `location/`.
- **Nutrition:** `health/nutrition/` per the pending Phase 3 nutrition
  contract (shared with Cronometer/MyFitnessPal/MacroFactor).
- **Clinical drops:** `health/medical/` per the pending FHIR-shaped Phase
  3 contract; raw CDA/FHIR documents kept whole.
- Dedupe: record type + start ts + source-device composite (as built);
  re-imports stay idempotent.

## Build plan

1. Extend `import_health_export` in `crates/trove-core/src/health.rs` —
   no new module: workout-route GPX extraction, ECG CSV subfolder,
   `export_cda.xml`, remaining record types (mindfulness, noise/audio
   exposure, state-of-mind, medication dosages), nutrition
   HKQuantityTypes (→ `health/nutrition/` once that contract lands).
2. Watch-folder extension: a Periodic def slice (or second def sharing
   the module) watching the Health Auto Export iCloud path; ships
   **opt-in** with the explicit-acknowledgement gate (continuous health
   sync + the route data is a location trail).
3. Clinical CDA/FHIR drop acceptance, parked behind the Phase 3
   `health/medical` contract (Needs-David).
4. Fixtures: synthetic export.zip with routes + ECG + nutrition + audio
   exposure; large-file streaming test; unique temp dirs.
5. Apple's XML schema is undocumented and shifts across iOS versions —
   tolerate unknown identifiers, never fail the whole import.

## Validation matrix

Core import shipped **pre-pipeline** (fixture-tested 🧪); only David
promotes slices to ✅ on real-data confirmation.

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Core export.zip import | 🧪 shipped pre-pipeline | drop a real export.zip; confirm `health/` metric CSVs + hub last-data |
| Workout routes (GPX) | — | export from an Apple Watch user with outdoor workouts; confirm route data attached to workout records |
| Nutrition fields | — | log food in a HealthKit app first; re-export; confirm `health/nutrition/` rows |
| Environmental audio | — | Apple Watch wearer's export; confirm dB metric present |
| Watch-folder | — | install Health Auto Export, schedule an automation; confirm rows appear without a manual drop |

## Research notes

`integrations-research.md` → "Health: Wearables & Biometrics" §Apple
Health export.zip deep parse (L822-828, ✅ built) + §Health Auto Export
(L982-988); "Nutrition/Medical" §Nutrition fields (L1033-1039) +
§Clinical FHIR (L1097-1103); "Environment" §Environmental Audio
(L2152-2159); "Geolocation" §Workout Routes (L2364-2370). All 🟢 high
except clinical (🟡, iPhone-only). Lose It! (`loseit`) is CoveredBy this
import via HealthKit passthrough. Direct vendor APIs (Oura, WHOOP, …)
stay worthwhile only for fields HealthKit never sees. Not time-sensitive.
