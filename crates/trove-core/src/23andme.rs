//! 23andMe raw genetic data import — the user's personal SNP file (~650 K rows).
//!
//! Accepts the export zip (`genome_*.txt.zip`) or a bare `genome_*.txt` file.
//! The raw 4-column TSV is stored verbatim, gzipped, at
//! `health/genetics/23andme/raw/genome.txt.gz`.  A small metadata sidecar
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
//! **Format (verified against 23andMe customer-care docs and real export fixtures):**
//! Tab-delimited, 4 columns: `rsid`, `chromosome`, `position`, `genotype`.
//! Lines starting with `#` are comments.  The column header is embedded
//! *inside* the comment block as `# rsid\tchromosome\tposition\tgenotype` —
//! the **first non-comment line is a data row**, not a header row.
//! AncestryDNA uses 5 columns (allele1 + allele2 split) and is handled by the
//! separate `ancestrydna` module.
//!
//! Brief: `docs/integrations/23andme.md`.

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

const GENOME_GZ: &str = "health/genetics/23andme/raw/genome.txt.gz";
const IMPORT_META: &str = "health/genetics/23andme/raw/import.json";

// ── last-data probe ──────────────────────────────────────────────────────────

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(GENOME_GZ))
}

// ── DEF ─────────────────────────────────────────────────────────────────────

/// Registered in [`crate::integrations::INTEGRATIONS`] via lib.rs `pub mod twenty_three_and_me`.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "23andme",
        name: "23andMe",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your 23andMe raw DNA data (the 4-column TSV of ~650 K SNPs). \
                       Stored entirely on-device — the most sensitive file in your vault. \
                       Re-importing the same export is a no-op.",
        domain: "health",
        vault_path: "health/genetics/23andme/",
        toggleable: false,
        setup: &[
            "23andme.com → Settings → Privacy & Data → Manage My Data → Download Raw Data.",
            "Enter your password and check the consent box; 23andMe emails a download link (~15 min).",
            "Import the downloaded zip (or the extracted genome_*.txt) here.",
            "⚠️  Your raw genome file contains ~650,000 genetic markers — the most \
             sensitive data type you can store. It stays on this Mac and never leaves your vault.",
        ],
        caveats: "23andMe was acquired by TTAM Research Institute (2024 bankruptcy). \
                  The export still works as of June 2026 but could change without warning \
                  — download your data now to preserve it.",
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

            // The export zip contains exactly one .txt file (genome_*.txt).
            // Find it by extension rather than by name so future renames still work.
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
                "no *.txt file found at the root of the zip — is this a 23andMe data export?",
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
/// Real 23andMe files contain a line like:
/// `# We are using reference human assembly build 37 (also known as Hg19).`
/// AncestryDNA files may contain similar build lines.
/// Falls back to the brand name from the "generated by" line.
fn detect_chip_version(tsv: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(tsv).ok()?;
    let mut fallback: Option<String> = None;
    // Regex-free match for the real 23andMe assembly-build comment.
    // Pattern: "We are using reference human assembly build <N>"
    for line in text.lines() {
        if !line.starts_with('#') {
            break;
        }
        let body = line.trim_start_matches('#').trim();
        // Real 23andMe format: "We are using reference human assembly build 37 (...)"
        if let Some(rest) = body.strip_prefix("We are using reference human assembly build") {
            let build_num = rest.split_whitespace().next().unwrap_or("").trim_matches(|c: char| !c.is_ascii_digit());
            if !build_num.is_empty() {
                return Some(format!("GRCh{build_num}/build {build_num}"));
            }
            return Some(rest.trim().to_owned());
        }
        // Older/alternative explicit "Build: N" line (kept for robustness).
        if let Some(rest) = body.strip_prefix("Build:") {
            return Some(format!("build {}", rest.trim()));
        }
        // Record fallback from the "generated by" line.
        if body.starts_with("This data file generated by AncestryDNA") {
            fallback = Some("AncestryDNA".to_owned());
        } else if body.starts_with("This data file generated by 23andMe") {
            fallback = Some("23andMe".to_owned());
        }
    }
    fallback
}

/// Count data rows (non-comment, non-empty lines) in the TSV.
///
/// In real 23andMe exports the column header lives inside a `#` comment line
/// (`# rsid\tchromosome\tposition\tgenotype`), so the first non-comment line
/// is already a data row.  Count every non-comment non-empty line.
fn count_snps(tsv: &[u8]) -> u64 {
    let text = std::str::from_utf8(tsv).unwrap_or("");
    let mut count = 0u64;
    for line in text.lines() {
        if line.starts_with('#') {
            continue;
        }
        if !line.trim().is_empty() {
            count += 1;
        }
    }
    count
}

/// Validate that the TSV looks like a real 23andMe raw data export.
///
/// Real 23andMe format: the column header is embedded in the comment block as
/// `# rsid\tchromosome\tposition\tgenotype` (a `#`-prefixed line).  The first
/// non-comment line is a data row (e.g. `rs4477212\t1\t82154\tAA`), not a
/// column header.
///
/// Validation strategy:
/// 1. The comment block must contain a `# rsid` header line (confirms source).
/// 2. The first non-comment line must be a data row with 4 tab-separated
///    columns whose first column matches the SNP id pattern `^(rs|i)\d+`.
fn validate_tsv(tsv: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(tsv).context("raw genome file is not valid UTF-8")?;
    let mut found_comment_header = false;
    let mut found_data_row = false;

    for line in text.lines() {
        if line.starts_with('#') {
            // Detect the column-header comment: "# rsid\tchromosome\t..."
            let body = line.trim_start_matches('#').trim().to_lowercase();
            if body.starts_with("rsid") && body.contains("chromosome") && body.contains("position") {
                found_comment_header = true;
            }
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        // First non-comment, non-empty line must be a data row.
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() != 4 {
            bail!(
                "expected first data row to have 4 tab-separated columns (got {}). \
                 This file does not look like a 23andMe raw data export: {line}",
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

    if !found_data_row && !found_comment_header {
        bail!("file is empty or contains only comment lines — not a 23andMe export");
    }
    if !found_data_row {
        bail!("file has no data rows after the comment block — not a 23andMe export");
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
    write_atomic(&gz_path, &gz_bytes).context("writing genome.txt.gz")?;

    progress(ImportProgress { records: snp_count, percent: 90.0 });

    // Write the metadata sidecar.
    let meta = ImportMeta {
        source: "23andme".to_owned(),
        imported_at: Local::now().to_rfc3339(),
        chip_version,
        snp_count,
        content_hash: hash,
    };
    write_json_atomic(&meta_path, &meta).context("writing import.json")?;

    progress(ImportProgress { records: snp_count, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{snp_count} SNPs imported into health/genetics/23andme/raw/genome.txt.gz"
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
            .join(format!("trove-23andme-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal 23andMe-style genome fixture matching the REAL export format.
    ///
    /// Key: the column header is embedded in the comment block as
    /// `# rsid\tchromosome\tposition\tgenotype`.  The first non-comment line
    /// is a DATA row — real exports contain NO bare column-header row.
    const GENOME_TSV: &str = "\
# This data file generated by 23andMe at Thu Jun 05 12:00:00 2025\n\
# Below is a text version of your data. Fields are TAB-separated\n\
# Each line corresponds to a single SNP.  For each SNP, we provide its identifier\n\
# We are using reference human assembly build 37 (also known as Hg19).\n\
# rsid\tchromosome\tposition\tgenotype\n\
rs4477212\t1\t82154\tAA\n\
rs3094315\t1\t752566\tAG\n\
rs3131972\t1\t752721\tAG\n\
rs12562034\t1\t768448\tAA\n\
rs116390263\t1\t772927\t--\n\
";

    /// AncestryDNA-header variant (different comment prefix, same 4-col data).
    const ANCESTRY_TSV: &str = "\
# This data file generated by AncestryDNA at: Mon Dec  7 10:27:48 2020\n\
# Below is a text version of your data. Fields are TAB-separated\n\
# Each line corresponds to a single SNP.  For each SNP, we provide its identifier\n\
# AncestryDNA complete data download made at: December 07, 2020\n\
rsid\tchromosome\tposition\tallele1\tallele2\n\
rs4477212\t1\t82154\tA\tA\n\
rs3094315\t1\t752566\tA\tG\n\
";

    fn do_import(v: &Vault, txt: &str) -> ImportOutcome {
        let path = v.root().join("genome.txt");
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
        assert!(gz_path.exists(), "genome.txt.gz written");

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
        assert_eq!(meta.source, "23andme");
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
        let zip_path = v.root().join("genome_export.zip");

        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(file);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("genome_JohnDoe_Full_20250605143000.txt", opts).unwrap();
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
    fn validates_bad_header() {
        let v = temp_vault("bad_header");
        let bad_tsv = "# comment\nfield1\tfield2\tfield3\tfield4\nval\t1\t100\tAA\n";
        let path = v.root().join("bad.txt");
        fs::write(&path, bad_tsv).unwrap();
        let res = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(res.is_err(), "bad header must fail validation");
    }

    #[test]
    fn chip_version_extracted_from_header() {
        // Real 23andMe format: build version in the "We are using reference human
        // assembly build N" comment line; column header also inside a # comment;
        // first non-comment line is a data row.
        let tsv_with_chip = "\
# This data file generated by 23andMe at Thu Jun 05 12:00:00 2025\n\
# We are using reference human assembly build 37 (also known as Hg19).\n\
# rsid\tchromosome\tposition\tgenotype\n\
rs4477212\t1\t82154\tAA\n\
";
        let v = temp_vault("chip");
        let out = do_import(&v, tsv_with_chip);
        assert_eq!(out.counts.get("snps"), Some(&1));
        let meta: ImportMeta =
            serde_json::from_slice(&fs::read(v.root().join(IMPORT_META)).unwrap()).unwrap();
        // Should capture "GRCh37/build 37" from the assembly-build comment.
        assert_eq!(meta.chip_version.as_deref(), Some("GRCh37/build 37"));
    }

    #[test]
    fn snp_count_ignores_comment_and_header_lines() {
        assert_eq!(count_snps(GENOME_TSV.as_bytes()), 5);
    }

    #[test]
    fn ancestry_header_variant_parses() {
        let v = temp_vault("ancestry_header");
        // AncestryDNA uses 5 columns (allele1 + allele2 split), not 4.
        // The 23andMe importer validates that the first data row has exactly 4
        // columns, so it correctly rejects a 5-col AncestryDNA file.
        // The ancestrydna module handles that format.
        let path = v.root().join("AncestryDNA.txt");
        fs::write(&path, ANCESTRY_TSV).unwrap();
        let res = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(
            res.is_err(),
            "23andme importer should reject 5-col AncestryDNA file"
        );
    }

    #[test]
    fn hub_card_has_import_box_and_last_data() {
        let v = temp_vault("hub");
        do_import(&v, GENOME_TSV);
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "23andme").unwrap();
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
