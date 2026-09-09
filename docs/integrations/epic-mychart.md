# Epic MyChart

- **id:** `epic-mychart`
- **domains:** `health/medical/` (contract: **health-medical.Observation** bound
  for Observation/lab/vital rows; Condition/Medication/Immunization/Allergy/Procedure raw-only
  pending health-medical.condition / health-medical.medication sibling-draft ratification)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll FHIR resources per connected organization;
  watermark cursor per resource type)
- **connection:** `smart-on-fhir` — OAuth (Authorization Code + PKCE,
  per-hospital authorize: each health system is a separate authorization
  endpoint discovered via `.well-known/smart-configuration`). Shared with
  `quest-diagnostics`, `labcorp`, and the generic `smart-on-fhir` def.
- **evidence:** official-docs — open.epic.com (free registration, 750+
  APIs, FHIR R4, USCDI v3 coverage, free sandbox; production review in
  days)
- **effort / priority:** L / P1
- **needs:** privacy (clinical records — diagnoses, medications, labs —
  among the most sensitive data in the vault; ships opt-in with explicit
  acknowledgement) · medical contract not yet ratified (Needs-David)

## What it is

Epic is the dominant US hospital EHR; MyChart is its patient portal. The
SMART on FHIR patient-standalone launch lets a user authorize Trove with
their MyChart login and pull structured clinical records — conditions,
medications, immunizations, lab observations, allergies, procedures. This
is the only structured Mac-native path to clinical records: Apple's Health
Records live in iOS-only HealthKit and never reach export.zip.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Conditions / problems | per-org availability | ICD-10 coded conditions, onset | official docs (USCDI v3) |
| Medications | per-org | MedicationRequest: drug, dose, prescriber, dates | official docs |
| Lab results | per-org | Observation/DiagnosticReport: LOINC, value, units, ranges | official docs |
| Immunizations, allergies, procedures, care plans, documents | per-org | Immunization, AllergyIntolerance, Procedure, CarePlan, DocumentReference | official docs |

All optional in the contract — what an org exposes varies; rows simply
carry what the bundle returned. USCDI v3 is the federal floor from
Jan 2026.

## Access & auth

- Register the app at open.epic.com (free); patient authenticates with
  MyChart credentials via OAuth2 (Auth Code + PKCE), grants
  `patient/*.read` scopes; pull FHIR R4 resources over plain HTTPS.
- Auth is **per organization** — the user picks their health system; the
  connect flow needs an org search/URL input plus
  `.well-known/smart-configuration` discovery. One user may connect
  several orgs.
- No TCC, no local files. Standalone-clean. The same client works against
  Cerner/Oracle Health, Meditech, Allscripts (all USCDI-mandated) — that
  generality lives in the shared `smart-on-fhir` client this def rides.

## Vault mapping

- **Raw layer:** `health/medical/epic-mychart/raw/` — FHIR resources as
  NDJSON, partitioned by resource type (and org where multiple are
  connected). Full fidelity first.
- **Contract layer:** `health/medical/epic-mychart/` per the pending
  FHIR-shaped Phase 3 contract (drafted across Epic + Quest + Labcorp +
  Blue Button: expected one row per clinical event, `ts`, `source`,
  `guid` = FHIR resource id + org, coded fields — LOINC/ICD-10/NDC — with
  resource overflow in `extra`).
- **Dedupe:** FHIR `Resource.id` scoped by org as `guid`; per-org cursor
  in `.trove/`, rebuildable from output files.

## Build plan

1. Build the shared SMART on FHIR client first (see the `smart-on-fhir`
   brief) — discovery, PKCE, token refresh, R4 resource paging. Epic is
   its first concrete endpoint.
2. Module `crates/trove-core/src/epic_mychart.rs` (def id `epic-mychart`):
   `DEF` (Periodic), `connection: Some("smart-on-fhir")`.
3. Fixtures from the open.epic.com sandbox (free) — Conditions,
   MedicationRequest, Observation bundles; parser + store + cursor tests.
4. Privacy gate: opt-in with explicit acknowledgement on enable (clinical
   records).
5. Effort is L because FHIR schema breadth is the work, not auth; scope
   the first pass to the resource list above.
6. Parked behind Needs-David (medical contract) for the normalized layer;
   raw NDJSON capture can land first.

## Build notes (2026-06-16)

- OAuth endpoints confirmed from Epic sandbox `.well-known/smart-configuration`:
  - authorize: `https://fhir.epic.com/interconnect-fhir-oauth/oauth2/authorize`
  - token: `https://fhir.epic.com/interconnect-fhir-oauth/oauth2/token`
  - FHIR R4 base: `https://fhir.epic.com/interconnect-fhir-oauth/api/FHIR/R4`
- Production redirect port: 38673 (38580 + 93).
- Behavior: Periodic (every 6 hours); 6 resource types polled: Observation, Condition,
  MedicationRequest, Immunization, AllergyIntolerance, Procedure.
- Contract layer: Observation → health-medical.Observation (bound). All others → raw only.
- Per-resource-type watermarks stored in `.trove/epic-mychart-sync.json` (hash-map keyed by
  resourceType). Patient ID cached under `__patient_id` key in that map.
- guids scoped as `epic-mychart/<FHIR-id>` to avoid collisions with Quest/Labcorp IDs.
- Narrowing: multi-org endpoint discovery (per-hospital `.well-known/smart-configuration`)
  deferred; first pass targets the Epic sandbox fixed endpoint. Connection ID is new
  (`epic-mychart`); connection added to CONNECTIONS registry.
- Needs-login: validation requires a real MyChart account or the Epic sandbox login.
- No new Cargo deps added.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Sandbox pull | built | connect the Epic sandbox patient (open.epic.com); Sync now; confirm NDJSON per resource type in `health/medical/epic-mychart/raw/<ResourceType>/` + observations contract rows in `observations/` |
| Real-org pull | Needs-login | a user with a real MyChart account authorizes their hospital; confirm conditions/meds/labs land; per-resource-type cursor advances on re-sync |
| Multi-org | deferred | multi-org per-hospital endpoint discovery deferred; each hospital uses the same sandbox endpoint for now |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Epic MyChart / SMART on FHIR Patient Portals (L1065–L1071).
Feasibility 🟢 high — 42% of US hospitals certified mid-2024 and growing.
Cross-cutting note 1: build the single generic FHIR client before any
EHR-specific work — it covers Epic, Cerner, Quest, Labcorp and ~40 EHRs.
Note 3: this pull bypasses Apple entirely (clinical records never reach
export.zip). Quest results often already appear in Epic bundles when
ordered through a connected system. USCDI v5 support is in development at
Epic — future field growth, additive only.
