# Domain: health-medical

Clinical records — lab results, medications, and problems — normalized out of
FHIR. The generic SMART-on-FHIR client is the canonical writer (one client, many
providers: Epic MyChart, Quest, Labcorp, Cerner, and ~40 other USCDI-v3 EHRs all
ride it as configured endpoints); CMS Blue Button adds Medicare Part D fills and
claim diagnoses on its own national connection; the `lab-pdf` and `pharmacy`
importers are the non-FHIR fallbacks (a portal PDF in, best-effort parse out).
FHIR resources map to flat rows — `Observation`/`DiagnosticReport` → observation,
`MedicationRequest`/`MedicationStatement` + Part D → medication, `Condition` +
claim diagnoses → condition — and merge at read time across every source. This
domain is **privacy-sensitive (medical)**: every collector ships opt-in with
explicit acknowledgement, and full-fidelity raw resources live under each
source's own `raw/` folder regardless of the contract.

- **Layout:** `health/medical/<source>/observations/YYYY-MM.jsonl` +
  `health/medical/<source>/medications/YYYY-MM.jsonl` +
  `health/medical/<source>/conditions/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event streams (three shapes)
- **Schemas:**
  [`schemas/health-medical.observation.schema.json`](../schemas/health-medical.observation.schema.json),
  [`schemas/health-medical.medication.schema.json`](../schemas/health-medical.medication.schema.json),
  [`schemas/health-medical.condition.schema.json`](../schemas/health-medical.condition.schema.json)
- **Dedupe key:** `guid` (source-unique: FHIR `Resource.id` scoped by org, EOB id,
  or `hash+test+date` for a parsed PDF). Imports skip already-stored guids.

Source folders are discovered by scanning — no registration, no code change.
`ts` is RFC3339 local when a clinical instant is known; clinical data is
routinely date-granular, so a **date-only `YYYY-MM-DD`** is allowed when that is
all the source gives (it still prefix-sorts by day/month). Coded terminologies
ride as an optional `code` + `code_system` pair (LOINC for labs, RxNorm/NDC for
meds, SNOMED/ICD-10 for conditions) — one idiom across all three shapes, additive
as new code systems appear; a source without a code (a parsed PDF) omits both.

## Observation

A lab result or vital sign: FHIR `Observation` (grouped by `DiagnosticReport`),
or a row parsed from a lab PDF. Numeric results carry `value` (+`unit`);
qualitative results ("Non-Reactive", "Positive") carry `value_text`; a row uses
whichever applies. Only `ts`/`source`/`guid`/`test` are required — a sparse PDF
parse writes little more.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | effective/collection time (RFC3339 local, or date-only `YYYY-MM-DD`) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id (FHIR `Observation.id`, or `hash+test+date`), the dedupe key |
| `test` | string | ✔ | test/measurement name (`code.text` or `coding.display`) |
| `code` | string | | terminology code (`coding.code`) — LOINC for labs/vitals |
| `code_system` | string | | which system the code is from: `"loinc"` \| `"snomed"` \| … |
| `value` | number | | numeric result (FHIR `valueQuantity.value`) |
| `value_text` | string | | qualitative result (FHIR `valueString`: `"Positive"`, `"Non-Reactive"`, …) |
| `unit` | string | | unit of `value` (UCUM where FHIR-sourced: `"mg/dL"`, `"mm[Hg]"`) |
| `reference_range` | string | | normal range as text (`"70-99"`, `"<5.7"`) |
| `flag` | string | | abnormal interpretation (`"H"`, `"L"`, `"A"`, …) |
| `panel` | string | | parent `DiagnosticReport`/order name (`"Comprehensive Metabolic Panel"`) |
| `provider` | string | | ordering provider or lab, as a display name |
| `extra` | object | | everything source-specific (specimen, status, full resource, confidence) |

Omit empty fields. Unknown fields are tolerated.

## Medication

A prescribed, reported, or filled medication: FHIR `MedicationRequest` /
`MedicationStatement`, a Medicare Part D drug claim, or a parsed pharmacy PDF.
`kind` separates a prescription order (`request`) from a reported med
(`statement`) from a dispensed fill (`fill`). Only `ts`/`source`/`guid`/`name`
are required.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | authored / fill date (RFC3339 local, or date-only `YYYY-MM-DD`) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id (FHIR resource id, EOB id, or `ndc+date+prescriber`) |
| `name` | string | ✔ | medication name (`medicationCodeableConcept.text` / `coding.display`) |
| `code` | string | | terminology code (RxNorm `coding.code`, or the dispensed NDC) |
| `code_system` | string | | `"rxnorm"` \| `"ndc"` \| … |
| `dose` | string | | dosage instruction as text (`dosageInstruction.text`) |
| `status` | string | | source-native status (`"active"`, `"completed"`, `"stopped"`, …) |
| `kind` | string | | `"request"` (order) \| `"statement"` (reported) \| `"fill"` (dispensed claim) |
| `prescriber` | string | | prescriber, as a display name (`requester.display`) |
| `start`, `end` | string | | validity/effective period (RFC3339 local or date-only) |
| `extra` | object | | everything source-specific (days supply, quantity, route, full resource) |

Omit empty fields. Unknown fields are tolerated.

## Condition

A problem, diagnosis, or condition: FHIR `Condition`, or an ICD-10 diagnosis
carried on a Blue Button claim. Only `ts`/`source`/`guid`/`name` are required —
a claim-carried diagnosis is often just a code + name.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | recorded date (RFC3339 local, or date-only `YYYY-MM-DD`) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id (FHIR `Condition.id`, or `eob-id+dx-seq`) |
| `name` | string | ✔ | condition name (`code.text` / `coding.display`) |
| `code` | string | | terminology code (`coding.code`) |
| `code_system` | string | | `"snomed"` \| `"icd10"` \| … |
| `onset` | string | | onset (RFC3339 local, date-only, or a coarse year `"2019"` when that's all that's recorded) |
| `status` | string | | clinical status (`"active"`, `"resolved"`, `"remission"`, …) |
| `category` | string | | FHIR category (`"problem-list-item"`, `"encounter-diagnosis"`, …) |
| `extra` | object | | everything source-specific (verification status, severity, full resource) |

Omit empty fields. Unknown fields are tolerated.

## Examples

```jsonl
{"ts":"2026-04-02T09:30:10-07:00","source":"quest-diagnostics","guid":"obs-15074-8-7a3f","test":"Glucose [Mass/volume] in Blood","code":"15074-8","code_system":"loinc","value":113,"unit":"mg/dL","reference_range":"70-99","flag":"H","panel":"Comprehensive Metabolic Panel","provider":"Quest Diagnostics"}
{"ts":"2026-05-21T08:14:00-07:00","source":"epic-mychart","guid":"Observation/eXJ2-bp-9921","test":"Systolic blood pressure","code":"8480-6","code_system":"loinc","value":128,"unit":"mm[Hg]"}
{"ts":"2025-11-03","source":"lab-pdf","guid":"f3a9c1-hiv-ab","test":"HIV 1/2 Antibody Screen","value_text":"Non-Reactive","extra":{"confidence":"0.82"}}
```

```jsonl-medication
{"ts":"2026-03-14T11:02:00-07:00","source":"epic-mychart","guid":"MedicationRequest/eM7-atorva","name":"Atorvastatin 20 MG Oral Tablet","code":"617312","code_system":"rxnorm","dose":"1 tablet by mouth nightly","status":"active","kind":"request","prescriber":"Dr. Priya Nair","start":"2026-03-14"}
{"ts":"2026-05-01","source":"cms-blue-button","guid":"eob-pde-88412","name":"LISINOPRIL 10MG TABLET","code":"00603-3741-21","code_system":"ndc","status":"completed","kind":"fill","prescriber":"NAIR, PRIYA","extra":{"daysSupply":90,"quantity":90}}
```

```jsonl-condition
{"ts":"2026-02-10T15:40:00-08:00","source":"epic-mychart","guid":"Condition/eC4-htn","name":"Essential hypertension","code":"59621000","code_system":"snomed","onset":"2019","status":"active","category":"problem-list-item"}
{"ts":"2026-05-01","source":"cms-blue-button","guid":"eob-88412-dx1","name":"Type 2 diabetes mellitus without complications","code":"E11.9","code_system":"icd10"}
```

## Read-time semantics (FYI for writers)

The medical reader scans `health/medical/*/observations|medications|conditions/`
across every source; creating those folders is the registration. Lab trends
group by `code` (LOINC) — or by `test` when no code is present — so the same
analyte from Quest, Labcorp, a PDF, and an Epic bundle lines up on one axis;
cross-source duplicates (a Quest result that also arrives inside an Epic bundle,
a Part D fill that matches an Epic `MedicationRequest`) are reconciled at read
time by code+date+value, never deduped across sources at write time — each
source keeps its own complete rows. Blue Button **claims** are only partly
contracted: Part D fills become medications and claim diagnoses become
conditions, but billing-specific detail (amounts, CPT/HCPCS procedure lines,
payer adjudication) has no contract column and stays in `extra` + the source's
raw NDJSON. Write codes when the source gives them and omit them when it
doesn't — a coded row and a PDF-parsed row are the same shape with different
fill. Never persist a derived view back into the vault.
