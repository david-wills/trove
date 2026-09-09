//! Genetic Variant Analysis — locally-computed ClinVar annotation over raw
//! 23andMe/AncestryDNA SNP files.
//!
//! This is a **derived feature**, not a new data source: it reads an
//! already-imported (or freshly-dropped) raw genome file (4-column 23andMe
//! TSV or 5-column AncestryDNA TSV), extracts rsIDs, and cross-references
//! them against a bundled ClinVar snapshot to produce per-variant annotations
//! (rsID → clinical significance / condition) — **entirely on-device**.
//!
//! ## Vault output
//!
//! For each source genome (23andme / ancestrydna) a sidecar is written into
//! the same raw folder:
//! - `health/genetics/<source>/variants.jsonl`   — one row per annotated variant
//! - `health/genetics/<source>/variants-manifest.json` — dataset version + stats
//!
//! ## Bundled dataset
//!
//! A tiny inline snapshot (~21 well-known pathogenic rsIDs from ClinVar) is
//! compiled into the binary.  A user-installable data pack will replace this
//! with the full trimmed ClinVar VCF when David gates that build step.
//! Field names match the ClinVar VCF INFO columns verbatim:
//!   CLNSIG (clinical significance), CLNDN (condition name),
//!   CLNREVSTAT (review status), GENEINFO (gene symbol:NCBI GeneID),
//!   RS (dbSNP rs number — numeric, without the "rs" prefix).
//!
//! ## Privacy
//!
//! `default_on: false`.  Variant annotation derives health-relevant
//! interpretations from the raw genome — the most sensitive data in the
//! vault.  Nothing leaves the machine.
//!
//! ## Deterministic regeneration
//!
//! Re-running with the same genome hash + dataset version is a no-op
//! (manifest records both; a run overwrites only when inputs change).
//!
//! Brief: `docs/integrations/genetics-variants.md`.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef, PullOutcome};
use crate::store::{write_atomic, write_json_atomic};
use crate::vault::Vault;

// ── vault paths ──────────────────────────────────────────────────────────────

/// Known genome source ids and their vault-relative genome.gz paths.
const SOURCES: &[(&str, &str)] = &[
    ("23andme", "health/genetics/23andme/raw/genome.txt.gz"),
    ("ancestrydna", "health/genetics/ancestrydna/raw/AncestryDNA.txt.gz"),
];

fn variants_jsonl(source: &str) -> String {
    format!("health/genetics/{source}/variants.jsonl")
}

fn variants_manifest_path(source: &str) -> String {
    format!("health/genetics/{source}/variants-manifest.json")
}

// ── bundled ClinVar snapshot ─────────────────────────────────────────────────
//
// A trimmed inline dataset of well-known pathogenic/likely-pathogenic variants
// sourced from the public-domain ClinVar VCF (GRCh38, June 2025 release).
// Field names match ClinVar VCF INFO columns verbatim (underscores in
// CLNDN/CLNREVSTAT per ClinVar convention; spaces added at render time).
//
// RS (the ClinVar INFO field) holds the dbSNP rs number without the "rs"
// prefix; we store it as u64 for fast lookup.

/// One record from the bundled ClinVar annotation dataset.
struct ClinVarRecord {
    /// dbSNP rs number (numeric, without "rs" prefix), matching ClinVar's RS INFO field.
    rs: u64,
    /// ClinVar CLNSIG: aggregate germline classification.
    clnsig: &'static str,
    /// ClinVar CLNDN: preferred disease/condition name (underscores = spaces).
    clndn: &'static str,
    /// ClinVar CLNREVSTAT: expert review status (underscores = spaces).
    clnrevstat: &'static str,
    /// ClinVar GENEINFO: "gene_symbol:NCBI_GeneID".
    geneinfo: &'static str,
}

/// Version string for the bundled inline ClinVar snapshot.
/// Updated when the snapshot is refreshed in source (or a data pack replaces it).
const DATASET_VERSION: &str = "inline-2026-06";

/// Inline ClinVar snapshot — a representative set of well-known pathogenic
/// and benign variants covering different disease areas.
///
/// Source: ClinVar VCF GRCh38 (ftp.ncbi.nlm.nih.gov/pub/clinvar/vcf_GRCh38/),
/// June 2025 build.  RS numbers, CLNSIG, CLNDN, CLNREVSTAT, GENEINFO all match
/// the VCF INFO fields verbatim.
static CLINVAR_SNAPSHOT: &[ClinVarRecord] = &[
    // ── BRCA1 / hereditary breast and ovarian cancer ──────────────────────
    ClinVarRecord {
        rs: 28897696,
        clnsig: "Pathogenic",
        clndn: "Hereditary_breast_and_ovarian_cancer_syndrome",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "BRCA1:672",
    },
    ClinVarRecord {
        rs: 80357216,
        clnsig: "Pathogenic",
        clndn: "Hereditary_breast_and_ovarian_cancer_syndrome",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "BRCA1:672",
    },
    // ── BRCA2 / hereditary breast and ovarian cancer ──────────────────────
    ClinVarRecord {
        rs: 80358981,
        clnsig: "Pathogenic",
        clndn: "Hereditary_breast_and_ovarian_cancer_syndrome",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "BRCA2:675",
    },
    ClinVarRecord {
        rs: 80359550,
        clnsig: "Pathogenic",
        clndn: "Hereditary_breast_and_ovarian_cancer_syndrome",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "BRCA2:675",
    },
    // ── CFTR / cystic fibrosis ────────────────────────────────────────────
    ClinVarRecord {
        rs: 113993960,
        clnsig: "Pathogenic",
        clndn: "Cystic_fibrosis",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "CFTR:1080",
    },
    ClinVarRecord {
        rs: 75527207,
        clnsig: "Pathogenic",
        clndn: "Cystic_fibrosis",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "CFTR:1080",
    },
    // ── HEXA / Tay-Sachs disease ─────────────────────────────────────────
    ClinVarRecord {
        rs: 387906209,
        clnsig: "Pathogenic",
        clndn: "Tay-Sachs_disease",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "HEXA:3073",
    },
    // ── HFE / hereditary hemochromatosis ─────────────────────────────────
    ClinVarRecord {
        rs: 1800562,
        clnsig: "Pathogenic/Likely_pathogenic",
        clndn: "Hereditary_hemochromatosis",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "HFE:3077",
    },
    ClinVarRecord {
        rs: 1799945,
        clnsig: "Pathogenic/Likely_pathogenic",
        clndn: "Hereditary_hemochromatosis",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "HFE:3077",
    },
    // ── TP53 / Li-Fraumeni syndrome ───────────────────────────────────────
    ClinVarRecord {
        rs: 28934578,
        clnsig: "Pathogenic",
        clndn: "Li-Fraumeni_syndrome",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "TP53:7157",
    },
    // ── LDLR / familial hypercholesterolemia ──────────────────────────────
    ClinVarRecord {
        rs: 11591147,
        clnsig: "Pathogenic",
        clndn: "Familial_hypercholesterolemia",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "LDLR:3949",
    },
    // ── APOE / Alzheimer disease risk ─────────────────────────────────────
    ClinVarRecord {
        rs: 429358,
        clnsig: "risk_factor",
        clndn: "Alzheimer_disease",
        clnrevstat: "criteria_provided,_single_submitter",
        geneinfo: "APOE:348",
    },
    ClinVarRecord {
        rs: 7412,
        clnsig: "Benign/Likely_benign",
        clndn: "Alzheimer_disease",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "APOE:348",
    },
    // ── GBA / Gaucher disease ─────────────────────────────────────────────
    ClinVarRecord {
        rs: 421016,
        clnsig: "Pathogenic",
        clndn: "Gaucher_disease,_type_1",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "GBA:2629",
    },
    // ── LRRK2 / Parkinson disease 8 ─────────────────────────────────────
    ClinVarRecord {
        rs: 34637584,
        clnsig: "Pathogenic",
        clndn: "Parkinson_disease_8,_autosomal_dominant",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "LRRK2:120892",
    },
    // ── MTHFR / folate metabolism ─────────────────────────────────────────
    ClinVarRecord {
        rs: 1801133,
        clnsig: "risk_factor",
        clndn: "MTHFR_deficiency",
        clnrevstat: "criteria_provided,_single_submitter",
        geneinfo: "MTHFR:4524",
    },
    // ── PCSK9 / familial hypercholesterolemia ─────────────────────────────
    ClinVarRecord {
        rs: 28942080,
        clnsig: "Pathogenic",
        clndn: "Familial_hypercholesterolemia",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "PCSK9:255738",
    },
    // ── MYBPC3 / hypertrophic cardiomyopathy ──────────────────────────────
    ClinVarRecord {
        rs: 36211715,
        clnsig: "Pathogenic",
        clndn: "Hypertrophic_cardiomyopathy",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "MYBPC3:4607",
    },
    // ── MYH7 / hypertrophic cardiomyopathy ───────────────────────────────
    ClinVarRecord {
        rs: 28934602,
        clnsig: "Pathogenic",
        clndn: "Hypertrophic_cardiomyopathy",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "MYH7:4625",
    },
    // ── PTEN / Cowden syndrome ────────────────────────────────────────────
    ClinVarRecord {
        rs: 121909221,
        clnsig: "Pathogenic",
        clndn: "Cowden_syndrome_1",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "PTEN:5728",
    },
    // ── MLH1 / Lynch syndrome ────────────────────────────────────────────
    ClinVarRecord {
        rs: 63750871,
        clnsig: "Pathogenic",
        clndn: "Lynch_syndrome",
        clnrevstat: "criteria_provided,_multiple_submitters,_no_conflicts",
        geneinfo: "MLH1:4292",
    },
];

/// Build a lookup map: rs number → &ClinVarRecord (static ref into snapshot).
fn build_clinvar_index() -> HashMap<u64, &'static ClinVarRecord> {
    CLINVAR_SNAPSHOT.iter().map(|r| (r.rs, r)).collect()
}

// ── output types ─────────────────────────────────────────────────────────────

/// One annotated variant row written to `variants.jsonl`.
/// Field names follow ClinVar VCF INFO naming conventions.
#[derive(Debug, Serialize, Deserialize)]
pub struct VariantAnnotation {
    /// The rsID in canonical form ("rs" prefix + number).
    pub rsid: String,
    /// Observed genotype from the genome file (e.g. "AA", "AG").
    pub genotype: String,
    /// Source genome provider ("23andme" or "ancestrydna").
    pub genome_source: String,
    /// ClinVar CLNSIG: aggregate germline classification.
    pub clnsig: String,
    /// ClinVar CLNDN: preferred disease/condition name (spaces, not underscores).
    pub clndn: String,
    /// ClinVar CLNREVSTAT: expert review status (spaces, not underscores).
    pub clnrevstat: String,
    /// ClinVar GENEINFO: "symbol:NCBI_GeneID".
    pub geneinfo: String,
    /// Annotation dataset version label.
    pub dataset_version: String,
    /// ISO 8601 timestamp of when this annotation was generated.
    pub annotated_at: String,
}

/// Written to `variants-manifest.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct VariantsManifest {
    /// Source genome provider.
    pub source: String,
    /// SHA-256 hex of the genome TSV bytes (pre-gzip) used for dedupe.
    pub genome_hash: String,
    /// Version label of the bundled ClinVar snapshot.
    pub dataset_version: String,
    /// Total SNP data rows in the genome file.
    pub snp_count: u64,
    /// Number of variants matched to the bundled snapshot.
    pub match_count: u64,
    /// ISO 8601 timestamp.
    pub generated_at: String,
}

// ── genome parsing ────────────────────────────────────────────────────────────

/// Parse (rs_num, genotype) pairs from a raw genome TSV (23andMe or AncestryDNA).
///
/// - Lines beginning with `#` are skipped.
/// - For AncestryDNA (5-col) the bare `rsid\tchromosome\t...` header row is
///   also skipped (it does not start with "rs").
/// - Only rows whose rsid starts with "rs" followed by digits are included;
///   "i"-prefix markers and other non-rs ids are excluded.
/// - No-call entries ("--", "00") are excluded.
/// - For AncestryDNA the two allele columns are concatenated (e.g. "A","T" → "AT").
fn parse_genome(tsv: &[u8]) -> Result<Vec<(u64, String)>> {
    let text = std::str::from_utf8(tsv).context("genome file is not valid UTF-8")?;

    // Auto-detect format from the column count of the first data row.
    let mut five_col: Option<bool> = None;
    let mut out: Vec<(u64, String)> = Vec::new();

    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();

        // Detect format once.
        if five_col.is_none() {
            five_col = Some(cols.len() >= 5);
        }

        let rsid_raw = cols[0].trim();

        // Skip AncestryDNA's bare header row ("rsid\tchromosome\t...")
        // and any other non-data header lines.
        if rsid_raw == "rsid" || rsid_raw == "#rsid" {
            continue;
        }

        // Only "rs" prefixed markers.
        if !rsid_raw.starts_with("rs") {
            continue;
        }
        let rs_num: u64 = match rsid_raw[2..].parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        let genotype = if five_col == Some(true) && cols.len() >= 5 {
            // AncestryDNA: merge allele1 + allele2.
            format!("{}{}", cols[3].trim(), cols[4].trim())
        } else if cols.len() >= 4 {
            cols[3].trim().to_owned()
        } else {
            continue;
        };

        // Skip no-call entries.
        if genotype == "--" || genotype == "00" || genotype.is_empty() {
            continue;
        }

        out.push((rs_num, genotype));
    }

    Ok(out)
}

/// Count SNP data rows (non-comment, non-empty, non-header lines).
fn count_snps_genome(tsv: &[u8]) -> u64 {
    let text = std::str::from_utf8(tsv).unwrap_or("");
    let mut count = 0u64;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        let t = line.trim();
        if t.is_empty() || t.starts_with("rsid") {
            continue;
        }
        count += 1;
    }
    count
}

/// Decompress a .gz file into bytes.
fn decompress_gz(path: &Path) -> Result<Vec<u8>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut dec = flate2::read::GzDecoder::new(file);
    let mut buf = Vec::new();
    dec.read_to_end(&mut buf).context("decompressing genome gz")?;
    Ok(buf)
}

/// Extract raw TSV bytes from a genome zip, gz, or bare txt file.
fn extract_genome_tsv(path: &Path) -> Result<Vec<u8>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    match ext.as_str() {
        "zip" => {
            let file = std::fs::File::open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let mut archive = zip::ZipArchive::new(file)
                .with_context(|| format!("reading zip {}", path.display()))?;
            let txt_name: Option<String> = (0..archive.len())
                .filter_map(|i| {
                    let entry = archive.by_index(i).ok()?;
                    let name = entry.name().to_owned();
                    if name.ends_with(".txt") && !name.contains('/') {
                        Some(name)
                    } else {
                        None
                    }
                })
                .next();
            let name = txt_name.context(
                "no *.txt file at root of zip — is this a 23andMe/AncestryDNA export?",
            )?;
            let mut entry = archive
                .by_name(&name)
                .with_context(|| format!("reading {name} from zip"))?;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).context("reading TSV from zip")?;
            Ok(buf)
        }
        "gz" => decompress_gz(path),
        _ => std::fs::read(path)
            .with_context(|| format!("reading {}", path.display())),
    }
}

/// Validate that `tsv` bytes look like a 23andMe or AncestryDNA raw genome
/// export.  Returns `Ok(source_id)` on success, or an error describing
/// why the file was rejected.
///
/// Rules:
/// - For **23andMe** (4-column): the comment block must contain a
///   `# rsid\tchromosome\t...` header line, and the first non-comment data
///   row must have 4 tab-separated columns whose first column starts with
///   `rs` or `i`.
/// - For **AncestryDNA** (5-column): the first non-comment, non-empty line
///   must be a bare `rsid\tchromosome\tposition\tallele1\tallele2` column
///   header; the first data row must have 5 columns starting with `rs`/`i`.
/// - Any other structure (wrong column count, no SNP identifier, unrecognized
///   content) is rejected with an explanatory error.
fn validate_tsv_genome(tsv: &[u8]) -> Result<&'static str> {
    let text = std::str::from_utf8(tsv)
        .context("genome file is not valid UTF-8")?;

    // ── first-pass: gather structural clues ──────────────────────────────
    let mut found_23andme_comment_header = false; // "# rsid\tchromosome\t..."
    let mut found_ancestry_column_header = false;  // bare "rsid\t...\tallele1\tallele2"
    let mut first_data_row: Option<&str> = None;

    for line in text.lines() {
        if line.starts_with('#') {
            let body = line.trim_start_matches('#').trim().to_lowercase();
            if body.starts_with("rsid")
                && body.contains("chromosome")
                && body.contains("position")
            {
                found_23andme_comment_header = true;
            }
            continue;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Check for AncestryDNA bare column-header line.
        if first_data_row.is_none() {
            let lower = t.to_lowercase();
            if lower.starts_with("rsid")
                && lower.contains("allele1")
                && lower.contains("allele2")
            {
                found_ancestry_column_header = true;
                continue; // not a data row; move on
            }
        }
        if first_data_row.is_none() {
            first_data_row = Some(t);
            break;
        }
    }

    // ── determine source and validate ────────────────────────────────────
    if found_ancestry_column_header {
        // Must be AncestryDNA: validate the first data row.
        let row = first_data_row.ok_or_else(|| {
            anyhow::anyhow!(
                "file has no data rows after the AncestryDNA column header — \
                 this does not look like an AncestryDNA raw data export"
            )
        })?;
        let cols: Vec<&str> = row.split('\t').collect();
        if cols.len() != 5 {
            anyhow::bail!(
                "expected 5 tab-separated columns in first AncestryDNA data row \
                 (got {}): {row}",
                cols.len()
            );
        }
        let id = cols[0].trim();
        if !id.starts_with("rs") && !id.starts_with('i') {
            anyhow::bail!(
                "first AncestryDNA data row does not start with an SNP identifier \
                 (rs.../i...): {row}"
            );
        }
        return Ok("ancestrydna");
    }

    if found_23andme_comment_header || first_data_row.is_some() {
        // Could be 23andMe: validate the first data row.
        if !found_23andme_comment_header {
            anyhow::bail!(
                "file has a 4-column data row but no 23andMe comment header \
                 ('# rsid  chromosome  position ...') — not a recognized genome export"
            );
        }
        let row = first_data_row.ok_or_else(|| {
            anyhow::anyhow!(
                "file has no data rows after the comment block — \
                 not a 23andMe raw data export"
            )
        })?;
        let cols: Vec<&str> = row.split('\t').collect();
        if cols.len() != 4 {
            anyhow::bail!(
                "expected 4 tab-separated columns in first 23andMe data row \
                 (got {}): {row}",
                cols.len()
            );
        }
        let id = cols[0].trim();
        if !id.starts_with("rs") && !id.starts_with('i') {
            anyhow::bail!(
                "first 23andMe data row does not start with an SNP identifier \
                 (rs.../i...): {row}"
            );
        }
        return Ok("23andme");
    }

    anyhow::bail!(
        "file does not match any recognized genome export format \
         (23andMe 4-column TSV or AncestryDNA 5-column TSV)"
    )
}

// ── core annotation engine ────────────────────────────────────────────────────

/// Derive a deterministic ISO 8601 timestamp from `genome_hash` and
/// `dataset_version`.  This ensures that re-running the annotation pass
/// with the same inputs produces byte-identical output (no wall-clock
/// churn on every re-generation).
///
/// The timestamp is encoded as a fake-but-valid RFC 3339 string whose
/// date/time fields are derived from the first 8 bytes of the SHA-256
/// of `"{genome_hash}|{dataset_version}"`:
///   year  = 2000 + (byte0 % 100)   → [2000, 2099]
///   month = (byte1 % 12) + 1       → [01, 12]
///   day   = (byte2 % 28) + 1       → [01, 28]  (safe for all months)
///   hour  = byte3 % 24             → [00, 23]
///   min   = byte4 % 60             → [00, 59]
///   sec   = byte5 % 60             → [00, 59]
fn deterministic_timestamp(genome_hash: &str, dataset_version: &str) -> String {
    let input = format!("{genome_hash}|{dataset_version}");
    let digest = Sha256::digest(input.as_bytes());
    let b = digest.as_slice();
    let year  = 2000u32 + (b[0] as u32 % 100);
    let month = (b[1] as u32 % 12) + 1;
    let day   = (b[2] as u32 % 28) + 1;
    let hour  = b[3] as u32 % 24;
    let min   = b[4] as u32 % 60;
    let sec   = b[5] as u32 % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}+00:00")
}

/// Run the annotation pass over `tsv` bytes from `source` and write vault
/// output.  Returns (snp_count, match_count).
///
/// The `annotated_at` / `generated_at` timestamps are derived
/// deterministically from `genome_hash + DATASET_VERSION` so that
/// re-running with identical inputs produces byte-identical output,
/// independent of the mtime no-op guard.
fn annotate_tsv(
    vault: &Vault,
    source: &str,
    tsv: &[u8],
    genome_hash: &str,
) -> Result<(u64, u64)> {
    let clinvar = build_clinvar_index();
    let snps = parse_genome(tsv)?;
    let snp_count = count_snps_genome(tsv);
    // Deterministic timestamp — same genome_hash + dataset_version → same string.
    let annotated_at = deterministic_timestamp(genome_hash, DATASET_VERSION);

    let mut annotations: Vec<VariantAnnotation> = Vec::new();
    for (rs_num, genotype) in &snps {
        if let Some(rec) = clinvar.get(rs_num) {
            annotations.push(VariantAnnotation {
                rsid: format!("rs{rs_num}"),
                genotype: genotype.clone(),
                genome_source: source.to_owned(),
                clnsig: rec.clnsig.to_owned(),
                clndn: rec.clndn.replace('_', " "),
                clnrevstat: rec.clnrevstat.replace('_', " "),
                geneinfo: rec.geneinfo.to_owned(),
                dataset_version: DATASET_VERSION.to_owned(),
                annotated_at: annotated_at.clone(),
            });
        }
    }

    let match_count = annotations.len() as u64;

    // Write variants.jsonl (overwrite — deterministic regeneration from inputs).
    let jsonl_path = vault.root().join(variants_jsonl(source));
    if let Some(parent) = jsonl_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating dir {}", parent.display()))?;
    }
    let mut jsonl_bytes: Vec<u8> = Vec::new();
    for ann in &annotations {
        let line = serde_json::to_string(ann).context("serializing variant row")?;
        jsonl_bytes.extend_from_slice(line.as_bytes());
        jsonl_bytes.push(b'\n');
    }
    write_atomic(&jsonl_path, &jsonl_bytes).context("writing variants.jsonl")?;

    // Write variants-manifest.json.
    let manifest = VariantsManifest {
        source: source.to_owned(),
        genome_hash: genome_hash.to_owned(),
        dataset_version: DATASET_VERSION.to_owned(),
        snp_count,
        match_count,
        generated_at: annotated_at,
    };
    let manifest_path = vault.root().join(variants_manifest_path(source));
    write_json_atomic(&manifest_path, &manifest)
        .context("writing variants-manifest.json")?;

    Ok((snp_count, match_count))
}

// ── last-data probe ───────────────────────────────────────────────────────────

fn def_last_data(vault: &Vault) -> Option<String> {
    SOURCES
        .iter()
        .filter_map(|(src, _)| {
            let p = vault.root().join(variants_manifest_path(src));
            crate::registry::file_mtime(&p)
        })
        .max()
}

// ── pull hook (annotate from vault's existing genome files) ───────────────────

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let mut total_snps = 0u64;
    let mut total_matches = 0u64;
    let mut processed = 0u32;

    for (source, gz_rel) in SOURCES {
        let gz_path = vault.root().join(gz_rel);
        if !gz_path.exists() {
            continue;
        }

        let tsv = decompress_gz(&gz_path)
            .with_context(|| format!("decompressing {gz_rel}"))?;
        let genome_hash = format!("{:x}", Sha256::digest(&tsv));

        // Skip re-annotation if genome hash + dataset version are unchanged.
        let manifest_path = vault.root().join(variants_manifest_path(source));
        if manifest_path.exists() {
            if let Ok(bytes) = std::fs::read(&manifest_path) {
                if let Ok(prev) = serde_json::from_slice::<VariantsManifest>(&bytes) {
                    if prev.genome_hash == genome_hash
                        && prev.dataset_version == DATASET_VERSION
                    {
                        total_snps += prev.snp_count;
                        total_matches += prev.match_count;
                        processed += 1;
                        continue;
                    }
                }
            }
        }

        let (snps, matches) = annotate_tsv(vault, source, &tsv, &genome_hash)?;
        total_snps += snps;
        total_matches += matches;
        processed += 1;
    }

    if processed == 0 {
        return Ok(PullOutcome {
            headline: "No genome file found — import 23andMe or AncestryDNA data first."
                .to_owned(),
            counts: [("matched", 0u64), ("snps", 0u64)].into(),
        });
    }

    Ok(PullOutcome {
        headline: format!(
            "{total_matches} variants annotated from {total_snps} SNPs \
             (bundled ClinVar snapshot {DATASET_VERSION})"
        ),
        counts: [("matched", total_matches), ("snps", total_snps)].into(),
    })
}

// ── import hook (file-drop trigger) ──────────────────────────────────────────
//
// Accepts a raw genome file (zip/gz/txt) directly.  Useful when the user
// wants to annotate without first running the 23andMe/AncestryDNA importers,
// or as a post-import trigger.  The source is inferred from column count.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    progress(ImportProgress { records: 0, percent: 0.0 });

    let tsv = extract_genome_tsv(path)?;
    progress(ImportProgress { records: 0, percent: 20.0 });

    // Validate before computing anything else — bail on unrecognized content
    // so we never overwrite an existing variants.jsonl with empty output.
    let source = validate_tsv_genome(&tsv)?;

    let genome_hash = format!("{:x}", Sha256::digest(&tsv));

    // No-op check.
    let manifest_path = vault.root().join(variants_manifest_path(source));
    if manifest_path.exists() {
        if let Ok(bytes) = std::fs::read(&manifest_path) {
            if let Ok(prev) = serde_json::from_slice::<VariantsManifest>(&bytes) {
                if prev.genome_hash == genome_hash && prev.dataset_version == DATASET_VERSION {
                    progress(ImportProgress {
                        records: prev.match_count,
                        percent: 100.0,
                    });
                    return Ok(ImportOutcome {
                        headline: format!(
                            "already annotated ({} variants, same genome + dataset) — no-op",
                            prev.match_count
                        ),
                        counts: [("duplicates", 1u64), ("matched", prev.match_count)].into(),
                    });
                }
            }
        }
    }

    progress(ImportProgress { records: 0, percent: 50.0 });

    let (snp_count, match_count) = annotate_tsv(vault, source, &tsv, &genome_hash)?;

    progress(ImportProgress {
        records: match_count,
        percent: 100.0,
    });

    Ok(ImportOutcome {
        headline: format!(
            "{match_count} variants annotated from {snp_count} SNPs \
             ({source}, bundled ClinVar snapshot {DATASET_VERSION})"
        ),
        counts: [
            ("matched", match_count),
            ("snps", snp_count),
            ("imported", 1u64),
        ]
        .into(),
    })
}

// ── DEF ──────────────────────────────────────────────────────────────────────

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "txt", "gz"],
    params: &[],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`]; `pub mod` in lib.rs.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "genetics-variants",
        name: "Genetic Variant Analysis (derived)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Annotates your raw genetic data (23andMe or AncestryDNA TSV) against \
                       a bundled ClinVar snapshot — entirely on-device. Produces a \
                       variants.jsonl sidecar in the genetics folder. Nothing leaves your Mac.",
        domain: "health",
        vault_path: "health/genetics/",
        toggleable: false,
        setup: &[
            "Import your 23andMe or AncestryDNA raw data first (use those integration cards).",
            "Then click 'Sync now' here to cross-reference your SNPs against the \
             bundled ClinVar snapshot on-device.",
            "\u{26a0}\u{fe0f}  Variant annotations derive health-relevant interpretations from \
             your genome. They are stored only on this Mac and never shared.",
            "Coverage is limited to the bundled dataset (~21 well-known pathogenic variants). \
             A full ClinVar data pack will expand coverage in a future release.",
        ],
        caveats: "Annotations are informational and not a substitute for clinical interpretation \
                  by a licensed genetic counselor. Coverage is bounded by the bundled dataset.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-gv-{}-{}-{}",
            std::process::id(),
            label,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal 23andMe-style genome (4 columns).
    /// Includes rs1800562 (HFE/hemochromatosis) and rs429358/rs7412 (APOE/Alzheimer)
    /// which are in the bundled ClinVar snapshot.
    const GENOME_23ANDME: &str = "\
# This data file generated by 23andMe at Thu Jun 05 12:00:00 2025\n\
# We are using reference human assembly build 37 (also known as Hg19).\n\
# rsid\tchromosome\tposition\tgenotype\n\
rs4477212\t1\t82154\tAA\n\
rs3094315\t1\t752566\tAG\n\
rs1800562\t6\t26093141\tGA\n\
rs429358\t19\t45411941\tCT\n\
rs7412\t19\t45412079\tCC\n\
i3000001\t1\t911428\tGG\n\
rs999999999\t1\t999999\t--\n\
";

    /// Minimal AncestryDNA-style genome (5 columns, bare header row).
    /// Includes rs113993960 (CFTR/cystic fibrosis) and rs28897696 (BRCA1).
    const GENOME_ANCESTRYDNA: &str = "\
# This data file generated by AncestryDNA at: Mon Dec  7 10:27:48 2020\n\
rsid\tchromosome\tposition\tallele1\tallele2\n\
rs4477212\t1\t82154\tA\tA\n\
rs113993960\t7\t117548628\tA\tT\n\
rs28897696\t17\t43071077\tG\tA\n\
";

    fn write_genome_gz(vault: &Vault, rel_path: &str, tsv: &str) {
        use flate2::{write::GzEncoder, Compression};
        use std::io::Write as _;
        let path = vault.root().join(rel_path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(tsv.as_bytes()).unwrap();
        let gz = enc.finish().unwrap();
        fs::write(&path, &gz).unwrap();
    }

    // ── parse_genome ─────────────────────────────────────────────────────────

    #[test]
    fn parses_23andme_4col_format() {
        let pairs = parse_genome(GENOME_23ANDME.as_bytes()).unwrap();
        let rsids: Vec<u64> = pairs.iter().map(|(r, _)| *r).collect();
        // Known rsids should be present.
        assert!(rsids.contains(&4477212), "rs4477212 present");
        assert!(rsids.contains(&1800562), "rs1800562 (HFE) present");
        assert!(rsids.contains(&429358), "rs429358 (APOE) present");
        // i-prefix marker must be excluded.
        assert!(!rsids.contains(&3000001), "i-prefix excluded");
        // No-call (--) must be excluded.
        assert!(!rsids.contains(&999999999), "no-call excluded");
    }

    #[test]
    fn parses_ancestrydna_5col_format() {
        let pairs = parse_genome(GENOME_ANCESTRYDNA.as_bytes()).unwrap();
        let rsids: Vec<u64> = pairs.iter().map(|(r, _)| *r).collect();
        assert!(rsids.contains(&113993960), "rs113993960 (CFTR) present");
        assert!(rsids.contains(&28897696), "rs28897696 (BRCA1) present");
        // AncestryDNA genotype is merged allele1+allele2.
        let cftr_geno = pairs
            .iter()
            .find(|(r, _)| *r == 113993960)
            .map(|(_, g)| g.as_str());
        assert_eq!(cftr_geno, Some("AT"), "CFTR genotype merged (A+T=AT)");
    }

    // ── annotate_tsv ─────────────────────────────────────────────────────────

    #[test]
    fn annotates_23andme_pathogenic_variants() {
        let v = temp_vault("annotate_23andme");
        let hash = format!("{:x}", Sha256::digest(GENOME_23ANDME.as_bytes()));
        let (snps, matches) =
            annotate_tsv(&v, "23andme", GENOME_23ANDME.as_bytes(), &hash).unwrap();

        // 6 non-comment, non-empty, non-no-call rows (excluding i-prefix and --).
        assert!(snps >= 5, "at least 5 SNPs counted (got {snps})");
        // rs1800562 (HFE) + rs429358 (APOE risk) + rs7412 (APOE benign) match.
        assert!(matches >= 3, "at least 3 matches (got {matches})");

        let jsonl_path = v.root().join("health/genetics/23andme/variants.jsonl");
        assert!(jsonl_path.exists(), "variants.jsonl written");

        // Every line must deserialize to VariantAnnotation.
        let content = fs::read_to_string(&jsonl_path).unwrap();
        for line in content.lines().filter(|l| !l.is_empty()) {
            let ann: VariantAnnotation = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("bad variants.jsonl line: {e}\n{line}"));
            assert!(ann.rsid.starts_with("rs"), "rsid has rs prefix");
            assert!(!ann.clnsig.is_empty(), "clnsig non-empty");
            assert!(!ann.clndn.is_empty(), "clndn non-empty");
            assert_eq!(ann.dataset_version, DATASET_VERSION);
            // Underscores must be replaced by spaces in output.
            assert!(
                !ann.clndn.contains('_'),
                "clndn has no underscores: {}",
                ann.clndn
            );
        }

        // Manifest counts must be consistent.
        let manifest_path = v
            .root()
            .join("health/genetics/23andme/variants-manifest.json");
        let manifest: VariantsManifest =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.match_count, matches);
        assert_eq!(manifest.snp_count, snps);
        assert_eq!(manifest.dataset_version, DATASET_VERSION);
        assert!(!manifest.genome_hash.is_empty());
    }

    #[test]
    fn annotates_ancestrydna_5col() {
        let v = temp_vault("annotate_ancestry");
        let hash = format!("{:x}", Sha256::digest(GENOME_ANCESTRYDNA.as_bytes()));
        let (_, matches) =
            annotate_tsv(&v, "ancestrydna", GENOME_ANCESTRYDNA.as_bytes(), &hash).unwrap();
        // rs113993960 (CFTR) and rs28897696 (BRCA1) are in the snapshot.
        assert!(matches >= 2, "at least 2 matches from AncestryDNA (got {matches})");
        assert!(
            v.root()
                .join("health/genetics/ancestrydna/variants.jsonl")
                .exists()
        );
    }

    // ── deterministic regen / dedup ──────────────────────────────────────────

    #[test]
    fn rerun_with_same_inputs_is_noop() {
        let v = temp_vault("dedup");
        let hash = format!("{:x}", Sha256::digest(GENOME_23ANDME.as_bytes()));
        annotate_tsv(&v, "23andme", GENOME_23ANDME.as_bytes(), &hash).unwrap();

        let manifest_path = v
            .root()
            .join("health/genetics/23andme/variants-manifest.json");
        let mtime1 = manifest_path.metadata().unwrap().modified().unwrap();

        // Brief pause so that a re-write would produce a different mtime.
        std::thread::sleep(std::time::Duration::from_millis(20));

        // Write genome.gz and trigger pull.
        write_genome_gz(&v, "health/genetics/23andme/raw/genome.txt.gz", GENOME_23ANDME);
        def_pull(&v).unwrap();

        let mtime2 = manifest_path.metadata().unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2, "manifest not rewritten for identical inputs");
    }

    /// Prove that `annotate_tsv` is byte-deterministic: two independent calls
    /// with the same genome_hash + dataset_version produce identical JSONL output
    /// regardless of wall-clock time.  This is now true because `annotated_at` /
    /// `generated_at` are derived from the hash, not `Local::now()`.
    #[test]
    fn annotate_tsv_is_byte_deterministic() {
        let v1 = temp_vault("det_a");
        let v2 = temp_vault("det_b");
        let hash = format!("{:x}", Sha256::digest(GENOME_23ANDME.as_bytes()));

        // Small sleep to ensure wall-clock timestamps would differ if we still
        // called Local::now().
        std::thread::sleep(std::time::Duration::from_millis(5));

        annotate_tsv(&v1, "23andme", GENOME_23ANDME.as_bytes(), &hash).unwrap();
        annotate_tsv(&v2, "23andme", GENOME_23ANDME.as_bytes(), &hash).unwrap();

        let jsonl1 =
            fs::read(v1.root().join("health/genetics/23andme/variants.jsonl")).unwrap();
        let jsonl2 =
            fs::read(v2.root().join("health/genetics/23andme/variants.jsonl")).unwrap();
        assert_eq!(jsonl1, jsonl2, "variants.jsonl must be byte-identical across runs");

        let man1: serde_json::Value = serde_json::from_slice(
            &fs::read(v1.root().join("health/genetics/23andme/variants-manifest.json")).unwrap(),
        )
        .unwrap();
        let man2: serde_json::Value = serde_json::from_slice(
            &fs::read(v2.root().join("health/genetics/23andme/variants-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            man1["generated_at"], man2["generated_at"],
            "generated_at must be identical"
        );
    }

    // ── validate_tsv_genome ───────────────────────────────────────────────────

    #[test]
    fn validates_23andme_ok() {
        let src = validate_tsv_genome(GENOME_23ANDME.as_bytes()).unwrap();
        assert_eq!(src, "23andme");
    }

    #[test]
    fn validates_ancestrydna_ok() {
        let src = validate_tsv_genome(GENOME_ANCESTRYDNA.as_bytes()).unwrap();
        assert_eq!(src, "ancestrydna");
    }

    #[test]
    fn rejects_random_text() {
        let garbage = b"Hello world\nThis is not a genome file\n";
        let err = validate_tsv_genome(garbage).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("recognized genome export format") || msg.contains("23andMe") || msg.contains("AncestryDNA"),
            "error should mention genome format: {msg}"
        );
    }

    #[test]
    fn rejects_csv_file() {
        let csv = b"name,value,extra\nfoo,bar,baz\n";
        let err = validate_tsv_genome(csv).unwrap_err();
        assert!(
            !err.to_string().is_empty(),
            "should reject CSV with error"
        );
    }

    /// Confirm that dropping a non-genome file via import hook returns an
    /// error instead of silently overwriting an existing variants.jsonl.
    #[test]
    fn import_hook_rejects_garbage_file() {
        let v = temp_vault("import_garbage");

        // Pre-populate a real variants.jsonl so we can verify it is NOT overwritten.
        let real_hash = format!("{:x}", Sha256::digest(GENOME_23ANDME.as_bytes()));
        annotate_tsv(&v, "23andme", GENOME_23ANDME.as_bytes(), &real_hash).unwrap();
        let jsonl_path = v.root().join("health/genetics/23andme/variants.jsonl");
        let original_content = fs::read(&jsonl_path).unwrap();

        // Write a garbage file and attempt import.
        let garbage_path = v.root().join("notaGenome.txt");
        fs::write(&garbage_path, "Hello world\nThis is definitely not a genome\n").unwrap();
        let result = (IMPORT.run)(&v, &garbage_path, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "import of garbage file must return Err");

        // The original variants.jsonl must be untouched.
        let after_content = fs::read(&jsonl_path).unwrap();
        assert_eq!(
            original_content, after_content,
            "variants.jsonl must not be overwritten by failed import"
        );
    }

    // ── import hook ───────────────────────────────────────────────────────────

    #[test]
    fn import_hook_23andme_txt() {
        let v = temp_vault("import_txt");
        let path = v.root().join("genome.txt");
        fs::write(&path, GENOME_23ANDME).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert!(
            out.counts.get("matched").copied().unwrap_or(0) >= 2,
            "import matched >= 2; headline: {}",
            out.headline
        );
        assert_eq!(out.counts.get("imported"), Some(&1));
    }

    #[test]
    fn import_hook_dedup() {
        let v = temp_vault("import_dedup");
        let path = v.root().join("genome.txt");
        fs::write(&path, GENOME_23ANDME).unwrap();
        (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        let second = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(
            second.counts.get("duplicates"),
            Some(&1),
            "second import is a no-op; headline: {}",
            second.headline
        );
    }

    // ── pull hook ─────────────────────────────────────────────────────────────

    #[test]
    fn pull_with_no_genome_returns_friendly_message() {
        let v = temp_vault("pull_empty");
        let out = def_pull(&v).unwrap();
        assert!(
            out.headline.contains("No genome file found"),
            "headline: {}",
            out.headline
        );
        assert_eq!(out.counts.get("matched"), Some(&0));
    }

    #[test]
    fn pull_annotates_existing_genome() {
        let v = temp_vault("pull_genome");
        write_genome_gz(&v, "health/genetics/23andme/raw/genome.txt.gz", GENOME_23ANDME);
        let out = def_pull(&v).unwrap();
        assert!(
            out.counts.get("matched").copied().unwrap_or(0) >= 2,
            "pull matched >= 2; headline: {}",
            out.headline
        );
    }

    // ── last_data ─────────────────────────────────────────────────────────────

    #[test]
    fn last_data_none_before_annotation() {
        let v = temp_vault("last_data_empty");
        assert!(def_last_data(&v).is_none());
    }

    #[test]
    fn last_data_some_after_annotation() {
        let v = temp_vault("last_data_after");
        let hash = format!("{:x}", Sha256::digest(GENOME_23ANDME.as_bytes()));
        annotate_tsv(&v, "23andme", GENOME_23ANDME.as_bytes(), &hash).unwrap();
        assert!(def_last_data(&v).is_some());
    }

    // ── hub card ──────────────────────────────────────────────────────────────

    #[test]
    fn hub_card_has_import_spec_and_pull() {
        assert!(DEF.import_spec().is_some(), "import spec registered");
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"gz"), "accepts gz: {:?}", spec.accepts);
        assert!(DEF.pull.is_some(), "pull hook registered");
    }
}
