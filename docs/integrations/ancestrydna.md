# AncestryDNA

- **id:** `ancestrydna`
- **domains:** `health/genetics/` (contract: **raw-only** — the path is the
  schema identifier; **5-column** TSV format: rsid/chromosome/position/allele1/allele2)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (drop the downloaded `AncestryDNA.txt`/zip into the vault)
- **connection:** none (web-initiated download; no OAuth, no API)
- **evidence:** documented stable export — `ancestry.com` → DNA → Settings →
  Download Raw DNA Data; **5-column** TSV (rsid/chromosome/position/allele1/allele2,
  bare header row) — distinct from 23andMe's 4-column genotype layout; ~700K SNPs
  (Affymetrix Genome-Wide Human SNP Array). No API; export-only.
- **effort / priority:** S / P2
- **needs:** privacy (raw genome — opt-in with explicit acknowledgement)

## What it is

Consumer genetic-testing service focused on ethnicity estimates and family
trees. Its "Download Raw DNA Data" file is the same concept as 23andMe's: a
tab-delimited table of measured SNP genotypes — the foundational layer for any
local ancestry/health/variant analysis. The company is healthy (not in
bankruptcy), so the export path is stable.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Raw SNP genotypes | all accounts (one-shot download) | rsid, chromosome, position, allele1, allele2 (~700K rows) | documented export |

All optional; a single retrospective snapshot, not a stream. File is larger
than 23andMe's (~150MB unzipped). Variant annotation is the separate derived
`genetics-variants` feature, not this import.

## Access & auth

- Web download only: ancestry.com → DNA → Settings → Actions → Download Raw DNA
  Data → password confirmation + consent → emailed link (~15 min). Delivers
  `AncestryDNA.txt` (zip).
- No API, no OAuth, no TCC, no local app DB. Standalone-clean. Only the test
  owner or account manager can download.

## Vault mapping

- **Raw layer:** `health/genetics/ancestrydna/raw/AncestryDNA.txt.gz` — the full
  4-column TSV stored verbatim (gzipped; ~700K rows). Canonical record; the path
  is the schema identifier (raw-only domain, no contract).
- **Contract layer:** none — `health/genetics/` is raw-only per the taxonomy. A
  minimal manifest row (`source`, `imported_at`, `chip_version`, `snp_count`,
  file path) sits alongside for hub last-data; the genome itself is never
  normalized.
- **Dedupe:** content hash of the downloaded file; re-importing the same export
  is a no-op.

## Build plan

1. New module `crates/trove-core/src/ancestrydna.rs`: `DEF` (Import) with its
   own parser. **AncestryDNA is 5-column** (rsid/chromosome/position/allele1/
   allele2) — the alleles are split, not combined into a single genotype field.
   The 23andMe parser (4-col) cannot handle this format and correctly rejects it.
   Write a separate parser that accepts 5-col rows and stores the raw file
   gzipped at `health/genetics/ancestrydna/raw/AncestryDNA.txt.gz`.
2. Registration line in `INTEGRATIONS`. No connection.
3. Privacy gate: ships opt-in — explicit acknowledgement on enable (raw genome);
   no secrets to strip.
4. Fixtures: synthetic AncestryDNA 5-column TSV; parser + store + dedupe tests,
   unique temp dirs. Parser-first (format documented and stable) — no Needs-sample flag.
5. FamilyTreeDNA uses yet another variant — evaluate separately when that
   module is built.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Raw SNP import | 🧪 built | drop a real `AncestryDNA.txt` in the import box; confirm gzipped raw + manifest land in `health/genetics/ancestrydna/` and hub last-data updates; re-drop is a no-op |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §AncestryDNA Raw Genetic Data (L1113–L1119) + cross-cutting note 4
(L1249). Feasibility 🟢 high. Privacy is paramount — per-folder
encryption-at-rest for `health/genetics/` is a candidate later opt-in
(cross-cutting note 6, L1253).

**Format note:** AncestryDNA is **5-column** (rsid/chromosome/position/
allele1/allele2), NOT 4-column like 23andMe. The allele pair is split into two
separate fields rather than combined into a genotype string. The ancestrydna
builder must write a dedicated 5-column parser — do not attempt to reuse the
23andMe 4-column parser.
