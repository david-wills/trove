# DocuSign / e-signed documents

- **id:** `docusign`
- **domains:** `files/` (raw-only per the taxonomy — no shared contract)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (PDF file drop; API sync deliberately deferred)
- **connection:** none for the v1 PDF drop. An OAuth connection (DocuSign
  developer app) exists as a documented path but is only built if automated
  sync ever earns demand.
- **evidence:** official-docs — DocuSign eSignature REST API (>400
  endpoints) is comprehensive; third-party bulk-export prior art
  (github.com/SignRequest/docusign-exporter). Manual download from the web
  UI (Manage > Download) is the documented user path.
- **effort / priority:** M / P2
- **needs:** none

## What it is

DocuSign (and peers — Dropbox Sign, Adobe Sign) is where life's signed
paperwork lives: leases, contracts, employment agreements, closings. Most
personal users have a handful of high-importance documents rather than a
stream. The research doc's recommendation, carried here: a **generic
signed-document PDF drop** (parties, dates, title metadata) serves more
users than a DocuSign-specific API sync — this brief covers the provider,
but v1 accepts any e-signed PDF.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Signed-PDF drop (v1) | none (any provider's downloaded PDF) | title, signing parties, signed/completed dates, file hash, path | research doc recommendation |
| Envelope API sync (deferred) | DocuSign dev-app OAuth | envelope status (completed/voided), signer details, dates, document PDFs | official API docs |

All optional; a PDF whose metadata can't be parsed still lands with
filename + dates and a "needs manual tagging" marker.

## Access & auth

- v1: user downloads completed PDFs from DocuSign web UI (Manage >
  Download) and drops them in the generic import box. No auth, no TCC, no
  network. Standalone-clean.
- Deferred API path: `GET /accounts/{id}/envelopes`,
  `GET /envelopes/{id}/documents/{doc_id}`; OAuth 2.0 via a registered
  DocuSign developer app. Recorded for completeness, not in the v1 plan.

## Vault mapping

- **Raw layer:** `files/docusign/documents.jsonl` — one row per signed
  document: `guid` (file content hash; envelope id when parseable from the
  PDF/filename), `title`, `parties[]`, `signed_ts`, `source_service`
  (docusign / dropbox-sign / adobe-sign / unknown), original filename,
  vault-relative path of the stored PDF copy under
  `files/docusign/documents/`. Unlike photos, the document file itself is
  the record — store the PDF.
- **Contract layer:** none — `files/` is raw-only per the taxonomy.
- **Dedupe:** content hash as `guid`; re-dropping the same PDF is a no-op.

## Build plan

1. Module `crates/trove-core/src/docusign.rs`: `DEF` (Behavior::Import)
   accepting PDFs via the generic import box.
2. Metadata extraction is **parser-last / best-effort**: try DocuSign's
   PDF metadata and completion-certificate page for parties/dates; fall
   back to filename + file dates. The exact embedded-metadata shape is not
   documented in the research doc — flag **Needs-sample** (a real completed
   DocuSign PDF) before investing in the certificate parser; ship the
   hash+filename row first.
3. Registration line in `INTEGRATIONS`.
4. Fixtures: a minimal PDF with decoy metadata; tests for hash dedupe and
   graceful no-metadata fallback (unique temp dirs).
5. Keep copy provider-agnostic in the hub card ("Signed documents
   (DocuSign, Dropbox Sign, Adobe Sign)") so non-DocuSign users find it.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| PDF drop + dedupe | ✅ built + tested | drop a real completed DocuSign PDF; confirm row in `files/docusign/documents.jsonl` + stored copy under `files/docusign/documents/<sha256>.pdf`; re-drop, confirm `0 documents imported, 1 duplicate skipped` |
| Metadata extraction | 🅿 parked (Needs-sample) | real completed-DocuSign PDF needed; v1 records filename+hash row; parties/dates empty until sample validates the XMP/certificate parser |
| API sync | — | not built (deferred); validate only if demand promotes it |

## Research notes

`integrations-research.md` → "Artifacts: Notes, Documents, Drafts & Files"
§DocuSign / e-signed documents (L2987–L2993). Feasibility 🟡 medium — the
API is comprehensive but needs a developer app + OAuth; for the typical
few-documents personal user, M1 manual download suffices, so the API is
explicitly deferred. Alternatives considered: per-provider integrations
for Dropbox Sign (ex-HelloSign) and Adobe Sign — rejected in favor of the
generic drop. Not time-sensitive: completed documents remain downloadable.
