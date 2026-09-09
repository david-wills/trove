# Lose It!

- **id:** `loseit`
- **domains:** `health/` via the Apple Health export (health domain contract:
  **document** — per-metric CSV + per-source raw, as built). No
  `health/nutrition/loseit/` folder exists or is planned — there is no direct
  Lose It! data path.
- **status:** 🧪 built (covered by the shipped `health` def — no dedicated
  module)
- **unavailable_reason:** none
- **behavior:** CoveredBy(health)
- **evidence:** research doc confirms no personal export and no public API;
  the Validic connector is enterprise/clinical-only. The HealthKit
  passthrough → export.zip path is the documented, only practical route.
- **effort / priority:** S / P2
- **needs:** none

## What it is

Calorie/nutrition tracker with a crowdsourced food database. It offers **no
self-serve export and no consumer API** — but on iOS it writes food data to
HealthKit, so a Lose It! user's nutrition lands inside the Apple Health
`export.zip` that Trove's shipped `health` integration already imports. The
right build is therefore *nothing*: a CoveredBy entry so the hub can answer
"does Trove support Lose It!?" with "yes — through your Apple Health export."

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Nutrition via HealthKit passthrough | iOS users who granted Lose It! HealthKit write | Dietary* HKQuantityType records (energy, macros, water, …) inside export.zip | research doc L1197, cross-cutting note 2 |
| Direct export / API | — | none exists (Validic connector is enterprise B2B only) | research doc L1197 |

Full nutrition-field parsing of export.zip is tracked as an extension on the
**apple-health** brief ("nutrition HKQuantityType records") — Lose It!
coverage deepens automatically when that lands; no Lose It!-specific code is
ever needed.

## Access & auth

None of its own. The data path is: Lose It! (iOS) → HealthKit → user-initiated
Health export.zip → Trove's existing Apple Health import. No connection, no
TCC beyond what the health import already handles, standalone-clean.

## Vault mapping

- **Raw layer:** whatever the `health` def stores from export.zip — Lose It!
  records appear as Dietary* record types attributed to the Lose It! source
  app in the XML; no separate `loseit/` folder.
- **Contract layer:** the health domain's as-built per-metric shape
  (document-tier contract). Lose It! rows are indistinguishable from any
  other HealthKit nutrition writer except by the source-app field — which the
  parser should preserve so read-time views can attribute entries.

## Build plan

No dedicated build. Registry work only:

1. Catalog stub: `loseit` entry with `Behavior::CoveredBy("health")` so the
   hub card exists, groups under Health, and explains the export.zip path.
2. Coverage depth rides the **apple-health** extension for nutrition
   HKQuantityType parsing (see that brief) — when it lands, verify Lose It!
   source attribution survives into the stored rows.
3. If Lose It! ever ships a personal export, re-open as a normal Import
   provider with its own folder.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Apple Health import (carrier) | 🧪 shipped pre-pipeline (the `health` def) — only David promotes to ✅ | existing health-import validation |
| Lose It! nutrition rows | — | from an iPhone with Lose It! logging to HealthKit: export.zip → import; confirm Dietary* rows attributed to Lose It! appear under `health/` (requires the apple-health nutrition extension) |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Lose It! (L1193–L1199); at-a-glance L1024 (feasibility 🟠 Low for
a direct path); cross-cutting notes 2 and 7. Research recommendation: icebox
for direct integration — Apple Health passthrough is the only practical
route, Lose It!'s market share is declining, and portability-minded users
prefer Cronometer/MFP (both queued with real exports).
