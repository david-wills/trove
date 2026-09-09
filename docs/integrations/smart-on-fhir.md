# Medical Records (SMART on FHIR)

- **id:** `smart-on-fhir`
- **domains:** `health/medical/` (contract: **health-medical.Observation**
  bound for Observation/lab/vital rows; Condition/Medication/Immunization/
  Allergy/Procedure raw-only pending sibling-draft ratification)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll FHIR R4 resources per configured endpoint;
  watermark cursor per resource type)
- **connection:** `smart-on-fhir` — TokenPaste (composite: FHIR base URL +
  bearer token, pipe-separated).  Full per-org PKCE via
  `.well-known/smart-configuration` is a Needs-David enhancement; the
  TokenPaste path covers any FHIR R4 / USCDI-v3 compliant provider not
  already handled by the dedicated Epic/Quest/Labcorp integrations.
- **evidence:** official — SMART on FHIR R4 standard; hl7.org/fhir/R4/;
  USCDI v3 federally mandated from Jan 2026.  Field mapping confirmed against
  the Epic sandbox fixtures in epic_mychart.rs (same FHIR R4 spec shape).
- **effort / priority:** L / P2 *(record P1 — drives the whole medical domain)*
- **needs:** privacy (diagnoses, medications, labs = most-sensitive medical
  detail — opt-in with explicit acknowledgement) · Needs-login (validation
  needs a real FHIR R4 / USCDI-v3 patient portal) · Needs-David (full
  per-org PKCE dynamic endpoint discovery deferred)

## What it is

The generic SMART-on-FHIR patient-access client — the canonical Mac-native path
for structured clinical records in the US. One implementation pulls FHIR R4
resources (conditions, medications, immunizations, lab/vital observations,
allergies, diagnostic reports, procedures, documents) from Epic, Cerner/Oracle
Health, Meditech, Allscripts, athenahealth, Quest, Labcorp, and any other
USCDI-v3-compliant EHR (federal floor from Jan 2026). The Epic, Quest, and
Labcorp provider briefs **ride this client** — they are configured endpoints,
not separate code. Clinical records are NOT in the Apple Health export.zip, so
this FHIR pull is the only structured path on the Mac.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Conditions / problems | per-org, `patient/Condition.read` | code (ICD/SNOMED), onset, status | official |
| Medications | `patient/MedicationRequest.read` | drug, dose, prescriber, dates | official |
| Lab & vital observations | `patient/Observation.read` | LOINC, value, unit, reference range, flag | official |
| Immunizations / allergies / procedures | per-org scopes | standard USCDI fields | official |
| Diagnostic reports / documents | `patient/DiagnosticReport.read`, `DocumentReference` | report bodies, attachments | official |

All optional in the contract — a given org/scope set yields a subset; missing
resource types simply produce no rows. No per-EHR code path: the difference is
the configured endpoint + which scopes the org grants.

## Access & auth

- SMART on FHIR standalone launch: app registers once (e.g. `open.epic.com`,
  free), then per provider the user authenticates with portal credentials via
  OAuth2 (Authorization Code + PKCE), authorizes `patient/*.read` scopes, and
  the app receives FHIR R4 bundles.
- **Per-organization auth:** each hospital system is a separate authorization
  endpoint; discovery via FHIR `.well-known/smart-configuration`. The connection
  must support a directory/URL input to pick the user's health system — the main
  UX challenge.
- No TCC. Standalone-clean (plain HTTPS to the provider's FHIR server). Token
  refresh per org; store per-org cursors.

## Vault mapping

- **Raw layer:** `health/medical/fhir/<org>/raw/YYYY-MM.ndjson` — the FHIR
  resource bundles as returned, full fidelity, partitioned by org and month.
- **Contract layer:** `health/medical/…` per the (pending) FHIR-shaped
  medical-records contract — expected shape: one row per clinical observation/
  event (`ts`, `source` = `fhir`/org, `guid` = FHIR resource id, resource
  `type`, coded value, normalized fields), the full resource JSON in `extra`.
  The lab-PDF importer targets the **same** observation shape so portal PDFs and
  FHIR labs merge. Parked behind **Needs-David (contract)** until ratified.
- **Dedupe:** FHIR resource id (+ org) as `guid`; per-org watermark cursor in
  `.trove/smart-on-fhir-<org>.json`, rebuildable by scanning output.

## Build plan

1. Module `crates/trove-core/src/smart_on_fhir.rs`: the generic FHIR R4 client
   (reqwest + serde_json, or `fhir-rs` for base types), `DEF`s per shipped
   provider (Epic/Quest/Labcorp first) all pointing at this client, `CONNECTION`
   (OAuth + PKCE + per-org endpoint discovery via
   `.well-known/smart-configuration`), `pull` hook.
2. Registration lines in `INTEGRATIONS` (one per provider def) + one in
   `CONNECTIONS`. Many defs share the `smart-on-fhir` connection.
3. Connection UX: provider directory/URL input + the per-org authorize flow;
   apply the affordance rule (clear label/help on the connect card).
4. Develop against the Epic sandbox (free, no real PHI) — the complexity is FHIR
   schema breadth, not auth. Fixtures from sandbox/spec example bundles covering
   each target resource type; parser + store + cursor tests, unique temp dirs.
5. Privacy gate: ships opt-in — explicit acknowledgement (clinical detail);
   consider per-folder encryption-at-rest for `health/medical/` as a later
   opt-in.
6. Vault writes wait on the Phase 3 medical-records contract; until then this
   provider is **parked behind Needs-David (contract)** and writes raw-only
   NDJSON.

## Build notes (2026-06-16)

- The brief assumed a Google-style many-defs-one-connection model (Epic/Quest/
  Labcorp riding this connection). In practice Epic (#93), Quest (#83), and
  Labcorp (#80) each shipped with their own dedicated connection. This module is
  therefore the catch-all for any FHIR R4 / USCDI-v3 compliant provider NOT
  already covered by those three.
- Connection: TokenPaste (composite `fhir-base|bearer-token`). The FHIR base URL
  is stored non-secretly in `.trove/smart-on-fhir-config.json`; the bearer token
  is stored 0600 in `.trove/sync/smart-on-fhir.json`. Full per-org PKCE discovery
  via `.well-known/smart-configuration` requires a runtime-configurable OAuth
  flow that the static `Provider` architecture does not support yet — Needs-David.
- Behavior: Periodic (every 6 hours).
- Resource types polled: Observation, Condition, MedicationRequest, Immunization,
  AllergyIntolerance, Procedure.
- Contract layer: FHIR Observations → health-medical.Observation (bound, same
  field mapping as epic_mychart.rs, built against hl7.org/fhir/R4/observation.html).
  Non-Observation types → raw-only (sibling drafts pending).
- guid scoped as `smart-on-fhir/<FHIR-id>` to avoid collision with Epic/Quest/Labcorp.
- Watermarks per resource type in `.trove/smart-on-fhir-sync.json`; patient id
  cached in same file.
- 27 tests green; cargo check green.
- No new Cargo deps added.
- Needs-login: validation requires a real FHIR R4 / USCDI-v3 patient portal
  (any provider not already covered by Epic/Quest/Labcorp).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Resource pull | built | paste a FHIR R4 base URL + bearer token (composite); Sync now; confirm NDJSON per resource type in `health/medical/smart-on-fhir/raw/<ResourceType>/` + Observation contract rows in `observations/` |
| Real patient portal | Needs-login | a user with a Cerner/athenahealth/Meditech/etc. portal pastes FHIR base URL + token; confirm conditions/meds/labs land; per-resource-type cursor advances on re-sync |
| Full PKCE auto-login | Needs-David | dynamic `.well-known/smart-configuration` discovery requires extending the OAuth architecture; deferred |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Generic FHIR Patient Access (L1161–L1167), §Epic MyChart (L1065–L1071),
§Quest (L1081–L1087), §Labcorp (L1089–L1095) + cross-cutting note 1 (build the
one FHIR client first, L1243) and note 3 (clinical records are not in export.zip
— FHIR is the Mac-native path, L1247). Feasibility 🟢 high. CMS Blue Button is
a sibling FHIR pull (claims, not clinical) on its own Medicare connection. The
main UX challenge remains per-org OAuth discovery (Needs-David).
