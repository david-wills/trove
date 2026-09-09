# Genetic Variant Analysis (derived)

- **id:** `genetics-variants`
- **domains:** `health/genetics/` (contract: **raw-only** — annotation output
  is a derived sidecar in the same raw folder; the path is the schema identifier)
- **status:** 🧪 built
- **unavailable_reason:** none
- **behavior:** Import (a local derivation pass over an already-imported raw
  genome — no network, no new file drop)
- **connection:** none (pure local computation; no API, no auth)
- **evidence:** official-docs — ClinVar bulk downloads (public domain, FTP at
  `ftp.ncbi.nlm.nih.gov/pub/clinvar`); SNPedia bulk dump (registered download);
  OSS reference `github.com/heiner/snpedia-23andme`; bcftools for VCF
  conversion. Pure local compute — no external API.
- **effort / priority:** M / P2
- **needs:** privacy (variant annotation derives health-relevant interpretations
  from the raw genome — opt-in with explicit acknowledgement) · depends on the
  raw `23andme`/`ancestrydna` import existing first

## What it is

A derived feature, not a new data source: it reads an already-imported raw
genome (23andMe / AncestryDNA 4-column TSV), extracts rsIDs, and cross-references
them against a bundled ClinVar/SNPedia snapshot to produce variant annotations
(rsID → phenotype/clinical-significance) — **entirely on-device**. This is the
local-bundled annotation story flagged as a key infrastructure primitive
(research L112) and a genuine privacy differentiator versus Promethease /
MyHeritage, where the genome is uploaded to a third party.

## Capabilities (what data it can yield)

| Capability | Tier/plan gating | Fields it yields | Evidence |
|---|---|---|---|
| rsID extraction | n/a (local) | rsid, chromosome, position, genotype | from imported raw TSV |
| ClinVar annotation | n/a (bundled dataset) | clinical significance, condition, review status per matched variant | ClinVar VCF |
| SNPedia annotation | n/a (bundled dataset) | curated SNP–phenotype magnitude/summary | SNPedia dump |
| VCF conversion (optional) | n/a | standard VCF for downstream tools | bcftools |

All optional; coverage is bounded by which rsIDs the bundled snapshot knows.
No tiering — the dataset is shipped (or user-installed as a data pack), so every
user gets identical annotation depth.

## Access & auth

- No network at run time. Input: the raw genome already at
  `health/genetics/<source>/raw/`. Reference data: a trimmed ClinVar/SNPedia
  snapshot compiled into the binary or shipped as a user-installable data pack.
- Reference data is refreshed by Trove releases (or an explicit pack update),
  never fetched silently — keeps the standalone/local-only guarantee intact.
- No TCC, no OAuth. Standalone-clean by construction.

## Vault mapping

- **Raw layer:** reads `health/genetics/<source>/raw/genome.txt.gz`; writes the
  derived annotation as a sidecar in the same raw-only domain, e.g.
  `health/genetics/<source>/variants.jsonl` (one row per annotated variant:
  rsid, genotype, source dataset, significance, condition) plus a
  `variants-manifest.json` (dataset version, match count, generated_at).
- **Contract layer:** none — `health/genetics/` is raw-only. The annotation is
  a derived view kept alongside the raw genome, never normalized into a shared
  shape. Records route whole: this stays in the genetics folder.
- **Dedupe:** regenerated deterministically from (genome hash + dataset
  version); re-running with the same inputs overwrites identically. The manifest
  records the dataset version so a snapshot update produces a fresh pass.

## Build plan

1. Module `crates/trove-core/src/genetics_variants.rs` (or a derivation pass in
   the shared `genetics.rs`): `DEF` (Import-shaped derivation triggered after a
   raw genome import or on demand). No connection.
2. Bundled-dataset plumbing: a trimmed ClinVar VCF (+ optional SNPedia subset)
   either compiled in or resolved as a data pack; version it in the manifest.
3. Parser/joiner: read the 4-column TSV, index by rsID, join against the
   reference VCF, emit `variants.jsonl`. Optional bcftools-style VCF export.
4. Privacy gate: ships opt-in (derived health interpretation) with explicit
   acknowledgement; same sensitivity tier as the raw genome itself.
5. Fixtures: a tiny synthetic genome TSV + a handful of known ClinVar rows;
   tests assert correct rsID matching, annotation fields, and deterministic
   regeneration, unique temp dirs.
6. Gated on the raw `23andme`/`ancestrydna` import shipping first — sequence
   after those briefs.

## Validation matrix

| Capability | Status | How to validate (exact steps) |
|---|---|---|
| rsID extraction | ✅ | `cargo test -p trove-core genetics_variants::` — `parses_23andme_4col_format` + `parses_ancestrydna_5col_format` |
| ClinVar annotation | ✅ | `annotates_23andme_pathogenic_variants` asserts rs1800562/rs429358/rs7412 annotated with correct significance |
| Deterministic regen | ✅ | `rerun_with_same_inputs_is_noop` checks manifest mtime does not change on re-run |

## Build notes (2026-06-17)

- Implemented as `Behavior::Import` (file-drop path) + `pull: Some(def_pull)` (derives from vault's existing genome).
- Bundled inline ClinVar snapshot: 21 well-known pathogenic/benign variants across BRCA1/2, CFTR, HFE, APOE, TP53, LDLR, LRRK2, HEXA, GBA, MLH1, PTEN, MYBPC3, MYH7, PCSK9, MTHFR (dataset_version = `inline-2026-06`).
- ClinVar INFO field names verified from `ftp.ncbi.nlm.nih.gov/pub/clinvar/README_VCF.txt`: RS (numeric rs number), CLNSIG, CLNDN, CLNREVSTAT, GENEINFO.
- Supports 23andMe 4-column (genotype string) and AncestryDNA 5-column (allele1+allele2 merged) formats.
- Raw-only domain; no contract bind. Writes `variants.jsonl` + `variants-manifest.json` sidecar per source.
- Deterministic: skips re-annotation when genome SHA-256 + dataset_version match the manifest.
- Full ClinVar data pack (trimmed VCF, ~650K variants) is a Needs-David gate for a future release.
- No new crate deps; no new ConnectionDef; raw-only (no contract). 13/13 tests pass.

## Research notes

`integrations-research.md` → "Health: Nutrition, Medical Records, Labs &
Genetics" §23andMe/AncestryDNA — Variant Analysis (L1217–L1223) + cross-cutting
notes 4 (L1249) and the primitives list (L112). Feasibility 🟢 high — pure local
computation, no API. Both consumer genome formats are the same 4-column TSV, so
one upstream parser feeds this. ClinVar is public-domain bulk-downloadable;
SNPedia needs a registered bulk download. The whole value proposition is that
nothing leaves the machine — preserve that strictly (no fallback to an online
annotator). Consider per-folder encryption-at-rest for `health/genetics/` as a
later opt-in (cross-cutting note 6).
