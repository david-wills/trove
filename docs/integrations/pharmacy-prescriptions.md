# Pharmacy Prescriptions

- **id:** `pharmacy-prescriptions`
- **domains:** `health/medical/` (contract: **Phase 3 pending** — FHIR-shaped;
  one client, many providers)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (pharmacy-portal PDF/printout drop is the only
  pharmacy-specific path; structured prescription data otherwise arrives via
  the FHIR and Blue Button briefs)
- **connection:** none for the PDF-drop path. The structured paths reuse the
  `smart-on-fhir` connection (Epic/Cerner MedicationRequest) and the
  `cms-blue-button` connection (Medicare Part D) — both owned by their own
  briefs, not re-declared here.
- **evidence:** official-docs — no consumer pharmacy API exists (Walgreens
  `developer.walgreens.com` Refill API is B2B order-only; CVS has none). FHIR
  R4 MedicationRequest (Epic/Cerner) and CARIN Blue Button Part D drug claims
  are the documented structured paths.
- **effort / priority:** S / P2
- **needs:** privacy (prescription history is sensitive medical data — opt-in
  with explicit acknowledgement) · Needs-sample (pharmacy-portal PDF layouts
  are undocumented and vary per chain — parser-last) · medical contract not yet
  ratified (Needs-David)

## What it is

Your prescription/medication history. There is no consumer-facing pharmacy API
in 2026 — the chains' developer programs are B2B refill-ordering, not personal
data retrieval. So this brief is mostly a **coverage map**: for most users,
prescriptions already arrive structured through two other briefs (Epic/Cerner
FHIR `MedicationRequest`, and Medicare Part D drug claims via Blue Button). This
provider's own contribution is the fallback for everyone else — a PDF/printout
drop from the pharmacy patient portal.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| FHIR MedicationRequest | via Epic/Cerner FHIR (covered by `epic-mychart`/`smart-on-fhir`) | medication, dosage, prescriber, authored date, status | FHIR R4 docs |
| Part D drug claims | Medicare users (covered by `cms-blue-button`) | NDC, fill date, days supply, prescriber | CARIN Blue Button |
| Pharmacy portal PDF | any user (this provider) | drug name, fill/refill dates, prescriber, pharmacy — as parsed | portal print/download |
| Pharmacy chain API | — | none usable (B2B refill-ordering only) | Walgreens/CVS dev docs |

All optional; what a given user gets depends entirely on which upstream path
applies. No tier-specific code paths.

## Access & auth

- **Structured (preferred):** no new auth — rides the `smart-on-fhir` FHIR
  client (MedicationRequest resources land in the Epic/Cerner pull) and the
  `cms-blue-button` OAuth flow (Part D). Those connections are declared in their
  own briefs.
- **PDF drop (this provider):** user downloads/prints prescription history from
  the CVS/Walgreens/etc. patient portal and drops the file on the import box. No
  auth, no API, no TCC. Standalone-clean.
- Pharmacy developer APIs (Walgreens Refill API, etc.) are explicitly out —
  B2B order placement, no personal data retrieval.

## Vault mapping

- **Raw layer:** `health/medical/pharmacy/raw/` — the dropped PDF/printout
  stored verbatim, plus the parsed extraction (`prescriptions.jsonl`) when the
  parser can read the layout. FHIR- and Blue-Button-sourced prescriptions stay
  in their own source folders (`health/medical/<fhir-source>/`,
  `health/medical/cms-blue-button/`) per the per-source-raw rule.
- **Contract layer:** the pending `health/medical/` FHIR-shaped contract; a
  parsed pharmacy prescription maps to the MedicationRequest-equivalent shape
  (medication, dosage, dates, prescriber) with chain-specific extras in `extra`.
  Until that contract is ratified this provider is **parked behind Needs-David
  (contract)**.
- **Dedupe:** PDF content hash for the file; per-prescription `guid` from
  (NDC/medication + fill date + prescriber) where extractable.

## Build plan

1. Module `crates/trove-core/src/pharmacy.rs`: `DEF` (Import) for the PDF-drop
   path. No connection (structured paths live in their own modules).
2. Registration line in `INTEGRATIONS`.
3. PDF parsing is **parser-last** and **Needs-sample**: pharmacy-portal layouts
   are undocumented and differ per chain. Land the import + raw-store path
   first (the PDF is always preserved verbatim); add per-chain extractors as
   real samples arrive, via the shared Vision/OCR bridge for image-only PDFs.
4. Privacy gate: ships opt-in (prescription history) with explicit
   acknowledgement.
5. Fixtures: synthetic prescription-history PDFs once samples exist; assert raw
   preservation always, extraction best-effort. Coordinate the MedicationRequest
   mapping with the `epic-mychart`/`smart-on-fhir` and `cms-blue-button` briefs
   so the three paths converge on one contract row shape.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| PDF raw import | ✅ built | drop any pharmacy-portal PDF/CSV; confirm verbatim file in `health/medical/pharmacy-prescriptions/raw/` + hub last-data; 8 unit tests green |
| PDF extraction | ⏸ parked | Needs-sample: implement `prescriptions_from_file` when a real chain export is available |
| FHIR coverage | — | validated through `epic-mychart`/`smart-on-fhir` — MedicationRequest resources present in that pull |
| Part D coverage | — | validated through `cms-blue-button` — drug claims present |

## Build notes (2026-06-16)

- Behavior: `Import` (PDF/CSV/HTML/ZIP drop, no API/auth).
- Raw layer: `health/medical/pharmacy-prescriptions/raw/<stamp>-<filename>` — verbatim accumulating snapshots.
- Contract layer: deferred — `health-medical.medication` sibling draft; parser parked (Needs-sample, pharmacy-portal layouts differ per chain and are undocumented).
- No new connection or dependency needed; all existing deps (`anyhow`, `chrono`, `write_atomic`) are already in scope.
- 8 tests: raw verbatim store, binary (PDF) integrity, accumulating re-imports, hub card, last_data, DEF shape, parked message.
- `vault_path` corrected from the brief's `health/medical/` to `health/medical/pharmacy-prescriptions/` (per-source-raw rule).

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Pharmacy Prescriptions (L1225–L1231) + cross-cutting note 7 (L1255).
Feasibility 🟡 medium — no direct path, but the FHIR + Blue Button combo covers
most users without a dedicated pharmacy connector, so this brief deliberately
leans on those two and only owns the PDF fallback. Walgreens/CVS developer APIs
are B2B refill-ordering, not personal retrieval — catalogued as unusable for our
purpose. Dental prescriptions and non-Epic/non-Medicare users without a portal
PDF have no structured path in 2026.
