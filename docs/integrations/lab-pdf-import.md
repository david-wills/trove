# Lab Results (PDF)

- **id:** `lab-pdf-import`
- **domains:** `health/medical/` (contract: **Phase 3 pending** — FHIR-shaped
  medical-records contract; one client/shape, many providers)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (drop a lab PDF downloaded from any patient portal)
- **connection:** none (user supplies the file; no auth)
- **evidence:** PDFs are universally available from all labs/portals; parsing is
  a local rules+LLM layer over extracted text. No external API. Format varies per
  lab — **Needs-sample** to harden the extractor across layouts.
- **effort / priority:** M / P2
- **needs:** privacy (lab values = financial/medical detail-class — opt-in with
  explicit acknowledgement) · Needs-sample (real PDFs from multiple labs to
  validate the extractor)

## What it is

The universal non-FHIR fallback for lab results: any lab or hospital portal
(MyQuest, patient.labcorp.com, hospital portals, international/specialty labs)
lets the user download a PDF. Trove extracts text and parses test name, value,
unit, reference range, date, and ordering provider into a structured row,
keeping the original PDF. Complements the FHIR pull (`smart-on-fhir`) for every
provider that doesn't expose FHIR.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Structured lab observations | none (any PDF) | test name, result value, unit, reference range, flag, collection date, provider | official (PDFs universal) |
| Raw document | none | the original PDF, kept verbatim | — |

Extraction accuracy depends on layout; fields are all optional in the contract,
so a partially-parsed report still stores what it got plus the raw PDF. LOINC
mapping is approximate for non-FHIR sources.

## Access & auth

- User downloads a PDF from a patient portal and drops it in the import box. No
  endpoint, no OAuth, no TCC.
- Text extraction via an open-source PDF text layer (e.g. `pdfium`/`pdf-extract`
  crate); scanned/image PDFs need an OCR bridge (macOS Vision). Standalone-clean
  — OCR is a compiled-in/system framework, not an external service.

## Vault mapping

- **Raw layer:** `health/medical/lab-pdf/raw/<hash>.pdf` — the original PDF kept
  verbatim, plus `health/medical/lab-pdf/raw/<hash>.json` with the
  extractor's full output (every field it found, confidence).
- **Contract layer:** `health/medical/…` per the (pending) FHIR-shaped
  medical-records contract — expected shape: one row per observation
  (`ts` = collection date, `source` = `lab-pdf`, `guid`, `test`/LOINC, `value`,
  `unit`, `reference_range`, `flag`), provider/order metadata and low-confidence
  fields in `extra`. Parked behind **Needs-David (contract)** until the
  medical-records contract is ratified; the same shape serves the FHIR labs.
- **Dedupe:** content hash of the PDF as the import key; per-observation `guid`
  from (hash + test + collection date).

## Build plan

1. Module `crates/trove-core/src/lab_pdf.rs`: `DEF` (Import), PDF text
   extraction + OCR fallback, a rules layer that normalizes the common
   Quest/Labcorp layouts, storing raw PDF + extracted JSONL via `store` helpers.
2. Registration line in `INTEGRATIONS`. No connection.
3. **Parser-last / Needs-sample:** the rules layer is built against real sample
   PDFs from several labs — flag Needs-sample and harden incrementally; never
   fail an import, always store the raw PDF + whatever parsed.
4. Privacy gate: ships opt-in — explicit acknowledgement (medical detail).
5. Contract writes wait on the Phase 3 medical-records contract; until then this
   provider is **parked behind Needs-David (contract)** and writes raw-only.
6. An LLM-assisted extraction path (local model) can sharpen parsing later — keep
   it optional and standalone (no cloud calls).

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Raw PDF capture | ✅ built | drop a lab PDF; confirm the original + extraction JSON land in `health/medical/lab-pdf/raw/` |
| Structured parse | ⚠️ Needs-sample | patterns tuned to Quest/Labcorp/Epic scaffold; needs real PDFs from multiple labs to confirm field names and layout (see build notes below) |

## Build notes (2026-06-17)

- **Behavior:** `Import` (user drops a PDF from any patient portal — no auth, no network).
- **Raw layer:** verbatim PDF stored as `health/medical/lab-pdf/raw/<sha256>.pdf`; full-fidelity extraction JSON at `health/medical/lab-pdf/raw/<sha256>.json` with every field the rules layer found plus the full raw text.
- **Contract layer:** `health_medical::Observation` rows under `health/medical/lab-pdf/observations/YYYY-MM.jsonl`; deduped by guid `<hash8>-<test_slug>-<datekey>`.
- **Dep added:** `pdf-extract = "0.10.0"` (pure Rust; no openssl/native-tls/cmake).
- **Parser status:** scaffold rules layer tuned to common Quest/Labcorp/Epic text layouts; regex patterns extract collection date, provider, panel, test name, numeric/qualitative value, unit, reference range, and flag. Precision improves with real samples (Needs-sample). The raw PDF is ALWAYS stored — no information is lost while the parser matures.
- **Image PDFs:** `pdf-extract` returns empty text for scanned/image PDFs; the raw PDF is stored + the extraction JSON records `text_extracted: false` with a note. OCR (macOS Vision) is deferred.
- **Tests:** 18 tests green (`cargo test -p trove-core lab_pdf_import::`); `cargo check` clean.

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §Lab PDF Import (L1137–L1143) + cross-cutting note 1 (FHIR-first,
L1243) and 7 (blocked/low paths, L1255). Feasibility 🟢 high for capture, parse
quality is the variable. This is the catch-all under the FHIR labs (Quest,
Labcorp) — build the FHIR client first; PDF import covers every provider without
FHIR, international users, and specialty/dental labs. Store both raw PDF and
extracted JSONL so re-parsing with a better extractor is always possible.
