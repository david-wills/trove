# 23andMe

- **id:** `23andme`
- **domains:** `health/genetics/` (contract: **raw-only** — the path is the
  schema identifier; 4-column TSV format: rsid/chromosome/position/genotype)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (drop the downloaded `genome_*.txt`/zip into the vault)
- **connection:** none (web-initiated download; no OAuth, no API)
- **evidence:** official export verified June 2026 — `23andme.com` Settings →
  Privacy & Data → Download Raw Data; format well-documented (4-col TSV, ~650K
  SNPs, v5 chip). No live API; export-only.
- **effort / priority:** S / P2
- **needs:** privacy (raw genome is the single most sensitive data type in the
  vault — opt-in with explicit acknowledgement) · time-sensitive (TTAM
  acquisition means the export path could vanish; prompt users to download now)

## What it is

Consumer genetic-testing service. The "Download Raw Data" file is a
tab-delimited table of the user's measured SNP genotypes — the foundational
layer for any local ancestry/health/variant analysis. Otherwise never captured
locally; vault-local storage of a raw genome is a strong privacy proposition.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| Raw SNP genotypes | all accounts (one-shot download) | rsid, chromosome, position, genotype (~650K rows) | official export |

All optional in the contract; the file is a single retrospective snapshot, not
a stream. VCF conversion (bcftools) and downstream variant annotation are a
separate derived feature (`genetics-variants`), not this import.

## Access & auth

- Web download only: Settings → Privacy & Data → Manage My Data → Download Raw
  Data → password + consent checkbox → emailed link (~15 min). Delivers
  `genome_*.txt.zip`.
- No API, no OAuth, no TCC, no local app DB. Standalone-clean (user hands Trove
  a file). Only the account owner can download.

## Vault mapping

- **Raw layer:** `health/genetics/23andme/raw/genome.txt.gz` — the full
  4-column TSV stored verbatim (gzipped; ~650K rows). This is the canonical
  record; the path is the schema identifier (raw-only domain, no contract).
- **Contract layer:** none — `health/genetics/` is raw-only per the taxonomy.
  A minimal manifest row (`source`, `imported_at`, `chip_version`, `snp_count`,
  file path) may sit alongside for the hub's last-data display, but the genome
  itself is never normalized into a shared shape.
- **Dedupe:** content hash of the downloaded file; re-importing the same export
  is a no-op.

## Build plan

1. Module `crates/trove-core/src/genetics.rs` (shared with AncestryDNA): `DEF`
   (Import), parse 4-column TSV, detect chip/header variant, write gzipped raw +
   manifest via `store` helpers.
2. Registration line in `INTEGRATIONS`. No connection.
3. Privacy gate: ships opt-in — explicit acknowledgement on enable (raw genome).
   No secrets to strip (genotype data, not credentials), but the
   acknowledgement copy must name the sensitivity.
4. Fixtures: a small synthetic 4-column TSV (real-shape header + a handful of
   rows); parser + store + dedupe tests, unique temp dirs. Build is parser-first
   here because the format is documented and stable — no Needs-sample flag.
5. The shared parser must also accept the AncestryDNA header variant (see that
   brief) — one code path, two registered defs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| Raw SNP import | ✅ built | drop a real `genome_*.txt` or `genome_*.txt.zip` in the import box; confirm `health/genetics/23andme/raw/genome.txt.gz` + `import.json` land; hub last-data updates; re-drop is a no-op (same hash) |

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §23andMe Raw Genetic Data (L1105–L1111) + cross-cutting note 4
(L1249). Feasibility 🟢 high. Time-sensitive: 23andMe was acquired in
bankruptcy by TTAM Research Institute; download still works June 2026 but
could change without warning. Privacy is paramount — consider per-folder
encryption-at-rest for `health/genetics/` as a later opt-in (cross-cutting
note 6, L1253).

**Format note:** 23andMe is 4-column (rsid/chromosome/position/genotype).
AncestryDNA is **5-column** (rsid/chromosome/position/allele1/allele2 —
split alleles, not a combined genotype). The `ancestrydna` module handles
that file; the 23andMe parser correctly rejects 5-column files.
