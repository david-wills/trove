# Medicare Blue Button

- **id:** `cms-blue-button`
- **domains:** `health/medical/` (contract: **raw-only** — EOB claims do not
  fit the bound Observation contract; health-medical.condition /
  health-medical.medication are unbound sibling drafts → deferred)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Periodic (poll ExplanationOfBenefit since last cursor;
  claims arrive with lag, low frequency is fine)
- **connection:** `cms-blue-button` — NEW OAuth (Medicare.gov login; CMS app
  registration is free and reviewed). Not the `smart-on-fhir` connection —
  Blue Button is one fixed national endpoint, not per-org discovery. OAuth2
  endpoints confirmed live 2026-06-17 from production `.well-known/smart-configuration`.
  Redirect port: 38816 (38580 + 236).
- **evidence:** official-docs — bluebutton.cms.gov (OAuth2, FHIR R4 +
  CARIN IG, free registration, sandbox at sandbox.bluebutton.cms.gov,
  actively developed)
- **effort / priority:** M / P2
- **needs:** privacy (claims expose diagnoses, procedures, and
  prescriptions — ships opt-in with explicit acknowledgement) · medical
  contract not yet ratified (Needs-David)

## What it is

CMS Blue Button 2.0 is Medicare's official claims API: a beneficiary
authorizes an app with their Medicare.gov login and it returns Part A
(inpatient), Part B (outpatient/physician), and Part D (prescription)
claims — what was billed, diagnosed, and filled, with ICD-10/CPT/NDC
codes. Claims-level data is not accessible any other way. Audience is
narrow (64M+ Medicare beneficiaries: 65+ or disability) but the leverage
for that group is very high, and Part D is the most complete prescription
record available to non-Epic users.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Part A/B claims | Medicare beneficiaries only | ExplanationOfBenefit: ICD-10 diagnoses, CPT/HCPCS procedures, dates, providers, amounts | official docs |
| Part D drug claims | Medicare beneficiaries only | drug NDC, fill dates, days supply, prescriber | official docs |
| Coverage & patient | Medicare beneficiaries only | Coverage, Patient resources | official docs |

All optional in the contract. This is billing data, not clinical notes —
it complements an Epic FHIR pull rather than replacing it.

## Access & auth

- Register the app at bluebutton.cms.gov (free, reviewed); user
  authorizes via Medicare.gov credentials (OAuth2); pull FHIR R4 + CARIN
  IG resources from the single national endpoint over plain HTTPS.
- No TCC, no local files, no per-org discovery. Standalone-clean.
- Sandbox with synthetic beneficiaries at sandbox.bluebutton.cms.gov.

## Vault mapping

- **Raw layer:** `health/medical/cms-blue-button/raw/` — EOB/Coverage/
  Patient resources as NDJSON, full fidelity, partitioned by resource
  type.
- **Contract layer:** `health/medical/cms-blue-button/` per the pending
  FHIR-shaped Phase 3 contract — claims rows (`ts` = service/fill date,
  `source`, `guid` = EOB id, coded fields, amounts) with EOB overflow in
  `extra`. Prescription fills are a natural join against Epic
  MedicationRequest rows at read time.
- **Dedupe:** EOB resource id as `guid`; cursor in `.trove/`, rebuildable
  from output files.

## Build plan

1. Module `crates/trove-core/src/cms_blue_button.rs`: `DEF` (Periodic,
   daily-ish — claims lag weeks anyway), `CONNECTION` (OAuth against the
   fixed Medicare.gov endpoint), `pull` hook for Sync-now.
2. Registration lines in `INTEGRATIONS` + `CONNECTIONS`.
3. Reuse the FHIR R4 parsing core from the `smart-on-fhir` work; add the
   CARIN EOB profile mapping.
4. Fixtures from the CMS sandbox's synthetic beneficiaries; parser +
   store + cursor tests, unique temp dirs.
5. Privacy gate: opt-in with explicit acknowledgement on enable.
6. Hub copy should say plainly this is for Medicare beneficiaries —
   anyone else has nothing to pull (clean empty state, not an error).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Sandbox pull | 🧪 built | connect a sandbox synthetic beneficiary (bluebutton.cms.gov); Sync now; confirm EOB/Coverage/Patient NDJSON in `health/medical/cms-blue-button/raw/`; hub last-data shows month |
| Real beneficiary | Needs-login | requires a Medicare account (David is not a beneficiary — any real user's run validates this slice) |
| Module tests | ✅ 11/11 | `cargo test -p trove-core cms_blue_button::` → 11 passed |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §CMS Blue Button 2.0 (L1073–L1079). Feasibility 🟢 high.
Cross-cutting note 7: pharmacy consumer APIs don't exist — Part D via
Blue Button plus Epic MedicationRequest is the recommended prescription
path (see the `pharmacy-prescriptions` entry, which rides this build).
Sequence after the `smart-on-fhir` client so the FHIR core is already in
place.
