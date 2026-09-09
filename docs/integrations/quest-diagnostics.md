# Quest Diagnostics

- **id:** `quest-diagnostics`
- **domains:** `health/medical/` (contract: **Phase 3 pending** —
  FHIR-shaped; one client, many providers)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll new DiagnosticReports/Observations;
  watermark cursor)
- **connection:** `quest-diagnostics` — NEW OAuth ConnectionDef (SMART on FHIR
  PKCE public client; own connection since `smart-on-fhir` is still a NotWired
  stub). Auth endpoint: `api.questdiagnostics.com/oauth2/authorize`; token:
  `api.questdiagnostics.com/oauth2/token`. Redirect port: 38663. Scopes:
  `patient/Observation.read patient/DiagnosticReport.read offline_access`.
- **evidence:** official — live patient FHIR endpoint at
  api.questdiagnostics.com (LOINC-coded Observations via
  DiagnosticReport); MyQuest portal PDF download as M1 fallback
- **effort / priority:** M / P1
- **needs:** privacy (lab results are clinical data — ships opt-in with
  explicit acknowledgement) · medical contract not yet ratified
  (Needs-David) · Needs-login (validation needs a real MyQuest account;
  Quest's FHIR access now requires third-party identity verification)

## What it is

Quest is the largest US lab network; most Americans who've had bloodwork
have results in MyQuest. Its patient FHIR endpoint returns structured,
LOINC-coded lab results — test, value, units, reference range, flags —
which is the foundation of longitudinal health tracking (lipids, A1c,
hormones over years). Structured beats the PDF: trends become queryable
rows instead of documents.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Lab results (structured) | free MyQuest account; identity verification for 3rd-party apps | Observation: LOINC code, value, units, reference range, flags; DiagnosticReport grouping, dates, ordering provider | official (live endpoint) |
| Lab results (PDF) | free MyQuest account | per-test PDF | official portal (fallback) |

All optional in the contract. The PDF path is not built here — it routes
to the generic `lab-pdf-import` provider.

## Access & auth

- FHIR patient access: OAuth2 with MyQuest credentials against
  `api.questdiagnostics.com`; pull DiagnosticReport + Observation
  resources over plain HTTPS. Rides the shared SMART on FHIR client —
  Quest is a fixed endpoint rather than a per-hospital search.
- **Identity-verification gotcha:** MyQuest's updated FHIR policy
  requires third-party identity verification for app access — the
  connect flow must surface this step honestly rather than failing
  opaquely.
- No TCC, no local files. Standalone-clean.
- Many users' Quest results also arrive inside Epic bundles (when ordered
  through a connected health system) — overlap, not a conflict; dedupe
  notes below.

## Vault mapping

- **Raw layer:** `health/medical/quest-diagnostics/raw/` — FHIR
  DiagnosticReport/Observation NDJSON, full fidelity.
- **Contract layer:** `health/medical/quest-diagnostics/` per the pending
  FHIR-shaped Phase 3 contract — one row per result (`ts` = collection
  date, `source`, `guid` = Observation id, LOINC code, value, units,
  range, flags; report context in `extra`).
- **Dedupe:** Observation resource id as `guid` within this source.
  Cross-source duplication with Epic-carried Quest results is a
  read-time concern for the contract pass (LOINC + date + value join),
  not a write-time merge — raw layers stay complete per source.

## Build plan

1. Depends on the shared `smart-on-fhir` client (discovery, PKCE,
   refresh, R4 paging) — build that first via the Epic brief.
2. Module `crates/trove-core/src/quest_diagnostics.rs`: `DEF` (Periodic),
   `connection: Some("smart-on-fhir")` with the Quest endpoint preset.
3. Build it as the generic FHIR **lab** importer (DiagnosticReport →
   Observation mapping) so Labcorp is a preset away, not a second
   implementation.
4. Fixtures: synthesize DiagnosticReport/Observation bundles from the
   FHIR R4 spec shapes (no public Quest sandbox is cited in the research
   doc); parser + store + cursor tests.
5. Privacy gate: opt-in with explicit acknowledgement on enable.
6. Connect-card copy must explain the identity-verification step
   (disabled-affordance rule: never a dead button).

## Build notes (2026-06-16)

- Built as a standalone SMART-on-FHIR PKCE OAuth collector (own `ConnectionDef`,
  port 38663). The `smart-on-fhir` shared client is still NotWired — this module
  does NOT depend on it and is self-contained.
- Parser validated against FHIR R4 spec-shape fixtures (hl7.org/fhir/R4/observation.html);
  18 unit tests green. No public Quest sandbox exists; `parser_parked_needs_sample=false`
  because the parser is against the published FHIR R4 standard, not folklore.
- The OAuth endpoint URLs (`api.questdiagnostics.com/oauth2/…`) are unconfirmed
  without a live dev portal account — these follow the SMART on FHIR convention;
  `Needs-login` flag covers real validation.
- Contract: `health-medical.Observation` (bound, `reuse-bound`). Raw layer
  unconditional at `health/medical/quest-diagnostics/raw/YYYY-MM.jsonl`.
- Integrator must add `&crate::quest_diagnostics::CONNECTION,` to CONNECTIONS.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| FHIR pull | Needs-login | real MyQuest account completes identity verification + OAuth; Sync now; confirm LOINC rows match the portal's results |
| Epic-overlap sanity | Needs-login | a user with both connections pulls both; confirm both raw layers complete and read-time join dedupes |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Quest Diagnostics (MyQuest / FHIR) (L1081–L1087). Feasibility
🟢 high. Cross-cutting note 1: one SMART on FHIR client covers Quest,
Labcorp, Epic, and ~40 EHRs — never build per-lab clients. PDF fallback
is universal and lives in `lab-pdf-import`. Results ordered through
Epic-connected systems already surface in Epic FHIR bundles — Quest
direct matters most for direct-to-consumer and standalone-lab orders.
