# Dental Records

- **id:** `dental-records`
- **domains:** `health/medical/` (contract: **Phase 3 pending** —
  FHIR-shaped; nothing dental joins it today)
- **status:** 🚫 unavailable
- **unavailable_reason:** No practical patient-accessible structured path in
  2026 — the HL7 dental FHIR standard has near-zero vendor adoption. Dental
  PDFs from practice portals can be dropped into the generic medical-document
  import instead.
- **behavior:** Unavailable
- **connection:** none
- **evidence:** Low — HL7 Dental Data Exchange IG (v2.0 ballot, 2025) exists
  but vendor adoption is voluntary and near-zero; Open Dental's REST API is
  dentist-side, not patient-side. Research doc feasibility 🟠 Low.
- **effort / priority:** S / P2
- **needs:** privacy (medical records — any future path ships opt-in with
  explicit acknowledgement)

## What it is

The dental slice of personal medical history — treatment notes, perio charts,
X-rays. Catalogued so the hub can honestly explain the gap: dental care is
fragmented across thousands of private practices with no Epic-equivalent, and
although an HL7 FHIR profile for dental data exists on paper, essentially no
vendor exposes it to patients in 2026. Practical access is PDFs from a
practice portal or a records request.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Patient FHIR (dental IG) | — | none in practice — adoption near-zero | research doc L1213 |
| Portal/records-request PDFs | per practice | unstructured documents — route to the generic lab/medical PDF import (`lab-pdf-import` brief), not a dental connector | research doc L1213–L1214 |

## Access & auth

No buildable mechanism. Open Dental (open-source practice-management) has a
REST API, but it serves the dentist's installation, not the patient — using
it would require the user's dentist to run Open Dental *and* hand out access,
which is not a product path.

## Vault mapping

None — no integration is built. Dental PDFs a user collects belong in the
generic medical-document import (see the `lab-pdf-import` brief), landing
under `health/medical/` with the rest of the document drops; they get no
dedicated folder.

## Build plan

None. The app card renders greyed with the `unavailable_reason` above, per
`Behavior::Unavailable`, and should point users at the PDF-drop path.
Revisit when HL7 Dental IG adoption produces real patient-facing endpoints —
at that point this re-opens as a rider on the generic `smart-on-fhir` client
rather than a bespoke build. Any future path is privacy-gated (medical
records: opt-in with explicit acknowledgement).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Unavailable card | — | hub shows Dental Records dimmed in the Health group with the honest reason + PDF-drop pointer |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Dental Records (L1209–L1215); at-a-glance L1026; cross-cutting
note 7 (blocked/low paths). Dental PDF layouts are less structured than
medical lab PDFs, so even the PDF-parse quality will lag the lab importer.
Alternatives considered and rejected: Open Dental API (dentist-side),
Dentrix Enterprise integrations (practice-side, not patient-facing).
