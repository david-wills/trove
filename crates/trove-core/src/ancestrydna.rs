//! AncestryDNA raw genetic data import — the user's personal SNP file (~700 K rows).
//!
//! Accepts the export zip (`AncestryDNA.zip`) or a bare `AncestryDNA.txt` file.
//! The raw 5-column TSV is stored verbatim, gzipped, at
//! `health/genetics/ancestrydna/raw/AncestryDNA.txt.gz`.  A small metadata sidecar
//! (`import.json`) records source, import timestamp, chip version (parsed from
//! the `#` header block), and SNP count.
//!
//! **Dedup:** the full-file SHA-256 is persisted in the sidecar; re-importing
//! the identical export is a no-op.
//!
//! **Privacy:** `default_on: false` — the user must explicitly opt in.  Raw
//! genome data is the single most sensitive file type in the vault; the setup
//! copy names that directly.
//!
//! **Format (verified against AncestryDNA community docs + real export fixtures):**
//! Tab-delimited, **5 columns**: `rsid`, `chromosome`, `position`, `allele1`, `allele2`.
//! Lines starting with `#` are comments.  The column header `rsid\tchromosome\t...`
//! appears as a **bare (non-comment) header row** — the first non-comment line
//! is the column header, not a data row.  (This differs from 23andMe, which
//! embeds the header inside a `#` comment line and starts data on the first
//! non-comment line.)
//!
//! **This is NOT the same format as 23andMe** (4-col genotype string) —
//! AncestryDNA splits the allele pair into two columns (`allele1` + `allele2`).
//! The 23andMe parser correctly rejects this format.
//!
//! Brief: `docs/integrations/ancestrydna.md`.

use std::io::Read as _;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Local;
use flate2::{write::GzEncoder, Compression};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write as _;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::{write_atomic, write_json_atomic};
use crate::vault::Vault;

// ── vault paths ─────────────────────────────────────────────────────────────

const GENOME_GZ: &str = "health/genetics/ancestrydna/raw/AncestryDNA.txt.gz";
const IMPORT_META: &str = "health/genetics/ancestrydna/raw/import.json";

// ── last-data probe ──────────────────────────────────────────────────────────

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(GENOME_GZ))
}

// ── DEF ─────────────────────────────────────────────────────────────────────

/// Registered in [`crate::integrations::INTEGRATIONS`] via lib.rs `pub mod ancestrydna`.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ancestrydna",
        name: "AncestryDNA",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your AncestryDNA raw DNA data (the 5-column TSV of ~700 K SNPs). \
                       Stored entirely on-device — the most sensitive file in your vault. \
                       Re-importing the same export is a no-op.",
        domain: "health",
        vault_path: "health/genetics/ancestrydna/",
        toggleable: false,
        setup: &[
            "ancestry.com → DNA → Settings → Actions → Download Raw DNA Data.",
            "Confirm your password and accept the consent prompt; Ancestry emails a download link (~15 min).",
            "Import the downloaded zip (or the extracted AncestryDNA.txt) here.",
            "\u{26a0}\u{fe0f}  Your raw genome file contains ~700,000 genetic markers — the most \
             sensitive data type you can store. It stays on this Mac and never leaves your vault.",
        ],
        caveats: "AncestryDNA uses 5-column format (allele1 + allele2 split) which differs from \
                  23andMe's 4-column format — each importer handles only its own format. \
                  FamilyTreeDNA exports yet another variant and is handled separately.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "txt", "gz"],
    params: &[],
    run: run_import,
};

// ── import metadata sidecar ──────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct ImportMeta {
    source: String,
    imported_at: String,
    chip_version: Option<String>,
    snp_count: u64,
    /// SHA-256 hex of the **uncompressed** TSV bytes (pre-gzip).
    content_hash: String,
}

// ── parser ───────────────────────────────────────────────────────────────────

/// Extract the raw TSV bytes from a genome zip or bare .txt / .txt.gz.
/// Returns the raw bytes of the plaintext TSV.
fn extract_tsv(path: &Path) -> Result<Vec<u8>> {
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

            // The export zip contains exactly one .txt file (AncestryDNA.txt).
            // Find it by extension rather than by name so renamed exports still work.
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
                "no *.txt file found at the root of the zip — is this an AncestryDNA data export?",
            )?;
            let mut entry = archive
                .by_name(&name)
                .with_context(|| format!("reading {name} from zip"))?;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).context("reading TSV from zip")?;
            Ok(buf)
        }

        "gz" => {
            // Already gzip-compressed — decompress first so we can parse.
            let file = std::fs::File::open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let mut dec = flate2::read::GzDecoder::new(file);
            let mut buf = Vec::new();
            dec.read_to_end(&mut buf).context("decompressing gz")?;
            Ok(buf)
        }

        _ => {
            // Bare .txt file.
            std::fs::read(path).with_context(|| format!("reading {}", path.display()))
        }
    }
}

/// Scan the comment header block for a build/assembly version and return a
/// best-effort chip/version string.
///
/// AncestryDNA files typically contain a line like:
/// `# AncestryDNA complete data download made at: December 07, 2020`
/// and may mention the array chip (e.g. "Affymetrix Genome-Wide Human SNP Array").
/// Falls back to "AncestryDNA" from the "generated by" line.
fn detect_chip_version(tsv: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(tsv).ok()?;
    let mut fallback: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with('#') {
            // Stop at the first non-comment line (column header or data).
            break;
        }
        let body = line.trim_start_matches('#').trim();
        // AncestryDNA "generated by" line.
        if body.starts_with("This data file generated by AncestryDNA") {
            fallback = Some("AncestryDNA".to_owned());
        }
        // Generic assembly-build reference (may appear in AncestryDNA too).
        if let Some(rest) = body.strip_prefix("We are using reference human assembly build") {
            let build_num = rest.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_ascii_digit());
            if !build_num.is_empty() {
                return Some(format!("GRCh{build_num}/build {build_num}"));
            }
            return Some(rest.trim().to_owned());
        }
        // Chip/array mention.
        if body.contains("Affymetrix") {
            return Some("Affymetrix Genome-Wide Human SNP Array".to_owned());
        }
    }
    fallback
}

/// Count data rows (non-comment, non-header, non-empty lines) in the TSV.
///
/// AncestryDNA format: the column header appears as a BARE (non-comment) row
/// starting with "rsid". All subsequent non-empty lines are data rows.
fn count_snps(tsv: &[u8]) -> u64 {
    let text = std::str::from_utf8(tsv).unwrap_or("");
    let mut count = 0u64;
    let mut past_header = false;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        // The column header row starts with "rsid" (literal, no `#`).
        if !past_header && line.starts_with("rsid") {
            past_header = true;
            continue; // skip the header row itself
        }
        count += 1;
    }
    count
}

/// Validate that the TSV looks like a real AncestryDNA raw data export.
///
/// AncestryDNA 5-column format:
/// - Comment lines begin with `#`
/// - A bare (non-comment) header row: `rsid\tchromosome\tposition\tallele1\tallele2`
/// - Data rows: 5 tab-separated columns, first column matches `rs...` or `i...`
///
/// Validation strategy:
/// 1. The first non-comment, non-empty line must be a column header starting with "rsid"
///    and containing "allele1" and "allele2" (distinguishes from 23andMe's comment-embedded header).
/// 2. The first data row after the header must have 5 tab-separated columns.
/// 3. The rsid column must match the SNP id pattern `^(rs|i)\d+`.
fn validate_tsv(tsv: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(tsv).context("raw genome file is not valid UTF-8")?;
    let mut found_header = false;
    let mut found_data_row = false;

    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if !found_header {
            // First non-comment line must be the column header.
            let lower = line.to_lowercase();
            if !lower.starts_with("rsid") || !lower.contains("allele1") || !lower.contains("allele2") {
                bail!(
                    "expected AncestryDNA column header (rsid/chromosome/position/allele1/allele2) \
                     as first non-comment line, got: {line}"
                );
            }
            found_header = true;
            continue;
        }
        // First data row after the column header must have 5 columns.
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != 5 {
            bail!(
                "expected first data row to have 5 tab-separated columns (got {}). \
                 This file does not look like an AncestryDNA raw data export: {line}",
                cols.len()
            );
        }
        // First column must be an rsid (rs... or i...) or similar SNP identifier.
        let id = cols[0].trim();
        let looks_like_snp = id.starts_with("rs") || id.starts_with('i');
        if !looks_like_snp {
            bail!(
                "first data row does not start with an SNP identifier (rs.../i...): {line}"
            );
        }
        found_data_row = true;
        break;
    }

    if !found_header && !found_data_row {
        bail!("file is empty or contains only comment lines — not an AncestryDNA export");
    }
    if !found_header {
        bail!("file has no column header row — not an AncestryDNA export");
    }
    if !found_data_row {
        bail!("file has no data rows after the column header — not an AncestryDNA export");
    }
    Ok(())
}

// ── run_import ───────────────────────────────────────────────────────────────

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    progress(ImportProgress { records: 0, percent: 0.0 });

    // Extract and validate the raw TSV bytes.
    let tsv = extract_tsv(path)?;
    validate_tsv(&tsv)?;

    // Content hash for dedup (over the uncompressed TSV bytes).
    let hash = format!("{:x}", Sha256::digest(&tsv));

    // Check for an existing import with the same hash — no-op if identical.
    let meta_path = vault.root().join(IMPORT_META);
    if meta_path.exists() {
        if let Ok(bytes) = std::fs::read(&meta_path) {
            if let Ok(prev) = serde_json::from_slice::<ImportMeta>(&bytes) {
                if prev.content_hash == hash {
                    return Ok(ImportOutcome {
                        headline: format!(
                            "already imported (same file, {} SNPs) — no-op",
                            prev.snp_count
                        ),
                        counts: [("duplicates", 1), ("imported", 0)].into(),
                    });
                }
            }
        }
    }

    progress(ImportProgress { records: 0, percent: 20.0 });

    // Parse metadata from the header.
    let chip_version = detect_chip_version(&tsv);
    let snp_count = count_snps(&tsv);

    progress(ImportProgress { records: snp_count, percent: 50.0 });

    // Gzip the raw TSV and write atomically.
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tsv).context("gzip-encoding genome")?;
    let gz_bytes = encoder.finish().context("finalizing gzip")?;

    let gz_path = vault.root().join(GENOME_GZ);
    write_atomic(&gz_path, &gz_bytes).context("writing AncestryDNA.txt.gz")?;

    progress(ImportProgress { records: snp_count, percent: 90.0 });

    // Write the metadata sidecar.
    let meta = ImportMeta {
        source: "ancestrydna".to_owned(),
        imported_at: Local::now().to_rfc3339(),
        chip_version,
        snp_count,
        content_hash: hash,
    };
    write_json_atomic(&meta_path, &meta).context("writing import.json")?;

    progress(ImportProgress { records: snp_count, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{snp_count} SNPs imported into health/genetics/ancestrydna/raw/AncestryDNA.txt.gz"
        ),
        counts: [("snps", snp_count), ("imported", 1)].into(),
    })
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ancestrydna-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal AncestryDNA-style genome fixture matching the REAL export format.
    ///
    /// Key differences from 23andMe:
    /// - 5 columns: rsid, chromosome, position, allele1, allele2 (alleles split)
    /// - The column header appears as a BARE (non-comment) row (no `#` prefix)
    /// - Data rows start immediately after the bare header row
    const GENOME_TSV: &str = "\
# This data file generated by AncestryDNA at: Mon Dec  7 10:27:48 2020\n\
# Below is a text version of your data. Fields are TAB-separated.\n\
# Each line corresponds to a single SNP. For each SNP, we provide its identifier\n\
# (rsid), its location on the human reference sequence (chromosome and position),\n\
# and the genotype call oriented with respect to the plus strand on the human reference\n\
# sequence.\n\
# AncestryDNA complete data download made at: December 07, 2020\n\
rsid\tchromosome\tposition\tallele1\tallele2\n\
rs4477212\t1\t82154\tA\tA\n\
rs3094315\t1\t752566\tA\tG\n\
rs3131972\t1\t752721\tA\tG\n\
rs12562034\t1\t768448\tA\tA\n\
rs116390263\t1\t772927\t0\t0\n\
";

    fn do_import(v: &Vault, txt: &str) -> ImportOutcome {
        let path = v.root().join("AncestryDNA.txt");
        fs::write(&path, txt).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_snps_and_writes_gz_plus_sidecar() {
        let v = temp_vault("basic");
        let out = do_import(&v, GENOME_TSV);
        assert_eq!(out.counts.get("snps"), Some(&5), "5 data rows");
        assert_eq!(out.counts.get("imported"), Some(&1));

        // Raw gz must exist.
        let gz_path = v.root().join(GENOME_GZ);
        assert!(gz_path.exists(), "AncestryDNA.txt.gz written");

        // Decompress and verify the bytes round-trip.
        let gz_bytes = fs::read(&gz_path).unwrap();
        let mut dec = flate2::read::GzDecoder::new(gz_bytes.as_slice());
        let mut recovered = String::new();
        dec.read_to_string(&mut recovered).unwrap();
        assert_eq!(recovered, GENOME_TSV, "round-trip matches");

        // Sidecar must exist with correct snp_count.
        let meta_path = v.root().join(IMPORT_META);
        assert!(meta_path.exists(), "import.json written");
        let meta: ImportMeta = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
        assert_eq!(meta.snp_count, 5);
        assert_eq!(meta.source, "ancestrydna");
        assert!(!meta.content_hash.is_empty());
    }

    #[test]
    fn reimport_of_same_file_is_noop() {
        let v = temp_vault("dedup");
        let first = do_import(&v, GENOME_TSV);
        assert_eq!(first.counts.get("imported"), Some(&1));

        let second = do_import(&v, GENOME_TSV);
        assert_eq!(second.counts.get("imported"), Some(&0));
        assert_eq!(second.counts.get("duplicates"), Some(&1));
        assert!(second.headline.contains("no-op"), "headline: {}", second.headline);
    }

    #[test]
    fn imports_from_zip() {
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("AncestryDNA_export.zip");

        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("AncestryDNA.txt", opts).unwrap();
            w.write_all(GENOME_TSV.as_bytes()).unwrap();
            w.finish().unwrap();
        }

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("snps"), Some(&5));
        assert!(v.root().join(GENOME_GZ).exists());
    }

    #[test]
    fn last_data_probe_returns_none_before_import_and_some_after() {
        let v = temp_vault("last_data");
        assert!(def_last_data(&v).is_none(), "no file yet");
        do_import(&v, GENOME_TSV);
        assert!(def_last_data(&v).is_some(), "file written");
    }

    #[test]
    fn validates_bad_header_no_allele_columns() {
        let v = temp_vault("bad_header");
        // 23andMe-style 4-col format: header inside comment, no bare header row,
        // data starts immediately after comment block.
        let bad_tsv = "# This data file generated by 23andMe\n\
                        # rsid\tchromosome\tposition\tgenotype\n\
                        rs4477212\t1\t82154\tAA\n";
        let path = v.root().join("bad.txt");
        fs::write(&path, bad_tsv).unwrap();
        let res = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(res.is_err(), "23andMe 4-col format must fail AncestryDNA validation");
    }

    #[test]
    fn validates_4_col_data_row_rejected() {
        let v = temp_vault("bad_4col");
        // Has the right header, but data rows have only 4 columns.
        let bad_tsv = "# comment\n\
                        rsid\tchromosome\tposition\tallele1\tallele2\n\
                        rs4477212\t1\t82154\tAA\n";
        let path = v.root().join("bad4.txt");
        fs::write(&path, bad_tsv).unwrap();
        let res = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(res.is_err(), "4-col data row must fail 5-col validation");
    }

    #[test]
    fn snp_count_ignores_comment_and_header_lines() {
        assert_eq!(count_snps(GENOME_TSV.as_bytes()), 5);
    }

    #[test]
    fn chip_version_extracted_from_affymetrix_mention() {
        let tsv = "\
# This data file generated by AncestryDNA at: Mon Dec  7 10:27:48 2020\n\
# Below is a text version of your data based on Affymetrix Genome-Wide Human SNP Array.\n\
rsid\tchromosome\tposition\tallele1\tallele2\n\
rs4477212\t1\t82154\tA\tA\n\
";
        let v = temp_vault("chip");
        let out = do_import(&v, tsv);
        assert_eq!(out.counts.get("snps"), Some(&1));
        let meta: ImportMeta =
            serde_json::from_slice(&fs::read(v.root().join(IMPORT_META)).unwrap()).unwrap();
        assert_eq!(
            meta.chip_version.as_deref(),
            Some("Affymetrix Genome-Wide Human SNP Array")
        );
    }

    #[test]
    fn hub_card_has_import_box_and_last_data() {
        let v = temp_vault("hub");
        do_import(&v, GENOME_TSV);
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "ancestrydna").unwrap();
        let import_info = card.import.as_ref().expect("import box info present");
        assert!(
            import_info.accepts.contains(&"zip"),
            "accepts zip: {:?}",
            import_info.accepts
        );
        assert!(
            import_info.accepts.contains(&"txt"),
            "accepts txt: {:?}",
            import_info.accepts
        );
        assert!(card.last_data.is_some(), "last_data populated after import");
    }
}
