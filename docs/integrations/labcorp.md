# Labcorp

- **id:** `labcorp`
- **domains:** `health/medical/` (contract: **Phase 3 pending** —
  FHIR-shaped; one client, many providers)
- **status:** 🧪 built (parser + tests pass; endpoint validation needs Needs-login)
- **unavailable_reason:** none
- **behavior:** Periodic (poll new DiagnosticReports/Observations;
  watermark cursor)
- **connection:** `smart-on-fhir` — OAuth (Labcorp patient account against
  Labcorp's patient FHIR endpoint; registration at
  fhir.labcorp.com/register/patient/). Shared with `epic-mychart`,
  `quest-diagnostics`, and the generic `smart-on-fhir` def.
- **evidence:** official — patient FHIR registration portal at
  fhir.labcorp.com/register/patient/ (OAuth2, Observation/
  DiagnosticReport); endpoint details less publicly documented than
  Quest's; PDF download from patient.labcorp.com as M1 fallback
- **effort / priority:** M / P1
- **needs:** privacy (lab results are clinical data — ships opt-in with
  explicit acknowledgement) · medical contract not yet ratified
  (Needs-David) · Needs-login (validation needs a real Labcorp patient
  account; endpoint thinly documented, so live verification matters more
  here)

## What it is

Labcorp is the other half of the US lab duopoly with Quest. Its patient
FHIR API lets a user connect third-party apps and pull lab results as
structured FHIR Observation/DiagnosticReport resources. Same value story
as Quest: LOINC-coded longitudinal lab history as queryable rows. Labcorp
also integrates with Apple Health Records — but that store is iOS-only,
so this direct pull is the Mac-native path.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Lab results (structured) | free Labcorp patient account | Observation: test code, value, units, reference range, flags; DiagnosticReport grouping, dates | official (registration portal; details thin) |
| Lab results (PDF) | free patient account | per-test PDF from patient.labcorp.com | official portal (fallback) |

All optional in the contract. The PDF path routes to the generic
`lab-pdf-import` provider, not this build.

## Access & auth

- FHIR patient access: patient registers/authorizes at
  fhir.labcorp.com/register/patient/, OAuth2; pull Observation +
  DiagnosticReport over plain HTTPS. Rides the shared SMART on FHIR
  client as a preset endpoint.
- Endpoint documentation is thinner than Quest's — expect discovery work
  in the build loop's live-docs verification (Phase 4), and keep the
  parser tolerant of profile variance.
- No TCC, no local files. Standalone-clean.
- Like Quest, results ordered through an Epic-connected system often also
  appear in Epic FHIR bundles — overlap handled at read time, raw layers
  stay complete.

## Vault mapping

- **Raw layer:** `health/medical/labcorp/raw/` — FHIR DiagnosticReport/
  Observation NDJSON, full fidelity.
- **Contract layer:** `health/medical/labcorp/` per the pending
  FHIR-shaped Phase 3 contract — one row per result (`ts` = collection
  date, `source`, `guid` = Observation id, LOINC code, value, units,
  range, flags; report context in `extra`).
- **Dedupe:** Observation resource id as `guid` within this source;
  cross-source overlap with Epic-carried results is a read-time join,
  per the Quest brief.

## Build plan

1. Depends on the shared `smart-on-fhir` client; build Quest's generic
   FHIR lab importer first — Labcorp should then be an endpoint preset +
   def, **not** a second implementation (the research doc says exactly
   this).
2. Module `crates/trove-core/src/labcorp.rs`: `DEF` (Periodic),
   `connection: Some("smart-on-fhir")` with the Labcorp endpoint preset.
3. Fixtures: FHIR R4 Observation/DiagnosticReport bundles (spec-shaped;
   no Labcorp sandbox cited) — tolerate field variance vs the Quest
   fixtures; parser + store + cursor tests.
4. Privacy gate: opt-in with explicit acknowledgement on enable.
5. Because the endpoint is thinly documented, treat the first real-login
   run as the schema check: log-and-store unknown fields raw rather than
   dropping them.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| FHIR pull | 🧪 (unit tests pass; endpoint unverified) | real Labcorp patient account registers at fhir.labcorp.com/register/patient/, OAuths with redirect http://localhost:38660/callback; Sync now; confirm rows match patient.labcorp.com results |
| FHIR parser coverage | ✅ 18 tests green | numeric, qualitative, date-only, flag, referenceRange, pagination, dedup, cursor back-compat; ts-precedence (effectivePeriod.start over issued), watermark-field (meta.lastUpdated), watermark-UTC-normalization, provider-from-performer |
| Endpoint profile | Needs-sample | Labcorp's FHIR base URL and exact OAuth URLs require live verification — parser is tolerant of field variance |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Labcorp Patient FHIR (L1089–L1095). Feasibility 🟢 high —
"same approach as Quest"; the explicit recommendation is one generic
FHIR lab importer, not per-lab integrations. Cross-cutting note 1 (single
SMART on FHIR client) and note 3 (clinical data never reaches Apple's
export.zip — direct FHIR is the Mac-native path) both apply. PDF fallback
lives in `lab-pdf-import`.
