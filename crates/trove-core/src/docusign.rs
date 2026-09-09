//! Signed-document import — accepts completed PDFs from DocuSign, Dropbox Sign,
//! Adobe Sign, or any provider. The signed-document record lands in
//! `files/docusign/documents.jsonl`; the original PDF is stored alongside in
//! `files/docusign/documents/<sha256>.pdf`.
//!
//! **v1 scope:** PDF drop only (no DocuSign API / OAuth). Metadata extraction
//! is best-effort from PDF filename and file-system dates; a full certificate
//! parser requires a real completed-PDF sample (flag: Needs-sample).
//!
//! **Dedupe:** `guid = sha256:<hex>` over the whole file. Re-dropping the same
//! PDF is a no-op (even under a different filename). The catalog is a flat
//! `documents.jsonl` snapshot (atomically rewritten on every import).
//!
//! Brief: docs/integrations/docusign.md.

use std::collections::HashSet;
use std::fs;
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths

const CATALOG: &str = "files/docusign/documents.jsonl";
const DOCS_DIR: &str = "files/docusign/documents";

// ---------------------------------------------------------------------------
// Data types

/// One signed-document record in the catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedDoc {
    /// `sha256:<hex>` — content hash, stable across re-drops.
    pub guid: String,
    /// Human-readable title: from PDF XMP/metadata if readable, else filename stem.
    pub title: String,
    /// Signing parties as extracted (empty when not parseable from v1 PDFs).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parties: Vec<String>,
    /// RFC3339 date the document was signed/completed, if parseable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_ts: Option<String>,
    /// Original filename as the user dropped it.
    pub original_filename: String,
    /// Vault-relative path of the stored PDF copy.
    pub vault_pdf: String,
    /// Provider hint: "docusign" | "dropbox-sign" | "adobe-sign" | "unknown".
    pub source_service: String,
    /// Source tag for manifest indexing.
    pub source: String,
    /// RFC3339 time this row was created (import time).
    pub imported_ts: String,
    /// Source-specific overflow (future: envelope_id, signer details, …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// DEF

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(CATALOG))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "docusign",
        name: "Signed Documents (DocuSign, Dropbox Sign, Adobe Sign)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import completed signed-document PDFs — leases, contracts, \
                      employment agreements, closings — from any e-signature provider. \
                      The original PDF is stored; metadata (title, parties, date) is \
                      extracted on a best-effort basis. Re-runnable: re-dropping the \
                      same PDF never duplicates.",
        domain: "files",
        vault_path: "files/docusign/",
        toggleable: false,
        setup: &[
            "DocuSign: Manage → select an envelope → Download (Complete PDF).",
            "Dropbox Sign / Adobe Sign: use their web-UI download for the completed document.",
            "Drop any number of PDFs here — the same file dropped twice is a no-op.",
        ],
        caveats: "Metadata extraction (parties, signing dates) from PDF XMP fields is \
                  best-effort; a DocuSign completion-certificate page may be richer but \
                  requires a real sample to validate (flag: Needs-sample). A PDF that \
                  can't be parsed still lands with its filename and import timestamp.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["pdf"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Read the existing catalog for dedupe.
    let existing: Vec<SignedDoc> = vault.read_snapshot(CATALOG)?;
    let mut seen: HashSet<String> = existing.iter().map(|d| d.guid.clone()).collect();
    let mut catalog = existing;

    // Hash the dropped file.
    let guid = sha256_file(path)
        .with_context(|| format!("hashing {}", path.display()))?;
    let guid = format!("sha256:{guid}");

    let mut imported = 0u64;

    if !seen.insert(guid.clone()) {
        progress(ImportProgress { records: 0, percent: 100.0 });
        return Ok(ImportOutcome {
            headline: "0 documents imported, 1 duplicate skipped".into(),
            counts: [("imported", 0u64), ("duplicates", 1u64)].into(),
        });
    }

    // Store the PDF copy.
    let hex = guid.strip_prefix("sha256:").unwrap_or(&guid);
    let pdf_name = format!("{hex}.pdf");
    let vault_pdf = format!("{DOCS_DIR}/{pdf_name}");
    let dest_path = vault.root().join(&vault_pdf);
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating {DOCS_DIR}"))?;
    }
    fs::copy(path, &dest_path)
        .with_context(|| format!("copying PDF to vault"))?;

    // Build the record.
    let original_filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown.pdf".into());
    let title = title_from_filename(&original_filename);
    let source_service = provider_from_filename(&original_filename);
    let imported_ts = Local::now().to_rfc3339();

    // Best-effort: try to read PDF XMP metadata (parked — needs sample).
    // Currently returns None for all fields; the real extractor goes here.
    let (parties, signed_ts, extra) = extract_pdf_meta(path);

    let doc = SignedDoc {
        guid,
        title,
        parties,
        signed_ts,
        original_filename,
        vault_pdf,
        source_service,
        source: "docusign".into(),
        imported_ts,
        extra,
    };
    catalog.push(doc);
    imported += 1;

    // Atomically rewrite the catalog.
    vault
        .write_snapshot(CATALOG, &catalog)
        .context("writing catalog")?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} document{} imported, 0 duplicates skipped",
            if imported == 1 { "" } else { "s" },
        ),
        counts: [("imported", imported), ("duplicates", 0u64)].into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers

/// SHA-256 hex of the whole file (for guid / dedupe).
fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// A readable title from the filename stem.
fn title_from_filename(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| filename.to_string());
    // Replace underscores and hyphens with spaces; trim trailing provider tags.
    stem.replace('_', " ").replace('-', " ").trim().to_string()
}

/// Heuristic: does the filename look like a specific provider's export?
fn provider_from_filename(filename: &str) -> String {
    let lower = filename.to_lowercase();
    if lower.contains("docusign") || lower.contains("envelope") {
        "docusign".into()
    } else if lower.contains("dropbox") || lower.contains("hellosign") {
        "dropbox-sign".into()
    } else if lower.contains("adobe") || lower.contains("echosign") {
        "adobe-sign".into()
    } else {
        "unknown".into()
    }
}

/// Best-effort metadata from the PDF.
///
/// V1: returns empty fields — the XMP/completion-certificate parser is parked
/// until a real completed-DocuSign PDF sample is available (Needs-sample).
/// This function is the placeholder; the real extractor slots in here.
fn extract_pdf_meta(
    _path: &Path,
) -> (Vec<String>, Option<String>, Map<String, Value>) {
    (Vec::new(), None, Map::new())
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-docusign-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn write_pdf(path: &std::path::PathBuf, content: &[u8]) {
        // Minimal valid PDF bytes (the real importer only needs a readable file).
        fs::write(path, content).unwrap();
    }

    fn run(vault: &Vault, pdf_path: &std::path::PathBuf) -> ImportOutcome {
        (IMPORT.run)(vault, pdf_path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_a_pdf_and_writes_catalog() {
        let v = temp_vault("basic");
        let pdf = v.root().join("lease-agreement.pdf");
        // Minimal PDF magic bytes so the file exists and is non-empty.
        write_pdf(&pdf, b"%PDF-1.4\n% fake signed document\n%%EOF\n");

        let out = run(&v, &pdf);
        assert_eq!(out.counts["imported"], 1);
        assert_eq!(out.counts["duplicates"], 0);
        assert!(out.headline.contains("1 document imported"));

        // Catalog written.
        let catalog_path = v.root().join(CATALOG);
        assert!(catalog_path.exists(), "documents.jsonl must exist after import");
        let raw = fs::read_to_string(&catalog_path).unwrap();
        assert!(raw.contains("sha256:"), "guid must be a content hash");
        assert!(raw.contains("lease agreement"), "title derived from filename");

        // PDF stored in the vault.
        let docs: Vec<SignedDoc> = v.read_snapshot(CATALOG).unwrap();
        assert_eq!(docs.len(), 1);
        let stored = v.root().join(&docs[0].vault_pdf);
        assert!(stored.exists(), "PDF copy must exist in vault");
        assert_eq!(docs[0].source, "docusign");
    }

    #[test]
    fn deduplicates_same_file_dropped_twice() {
        let v = temp_vault("dedup");
        let pdf = v.root().join("contract.pdf");
        write_pdf(&pdf, b"%PDF-1.4\n% contract body\n%%EOF\n");

        let first = run(&v, &pdf);
        assert_eq!(first.counts["imported"], 1, "first drop: one import");

        let second = run(&v, &pdf);
        assert_eq!(second.counts["imported"], 0, "second drop: no import");
        assert_eq!(second.counts["duplicates"], 1, "second drop: one duplicate");

        // Catalog still has exactly one row.
        let docs: Vec<SignedDoc> = v.read_snapshot(CATALOG).unwrap();
        assert_eq!(docs.len(), 1, "catalog must not duplicate");
    }

    #[test]
    fn different_files_both_imported() {
        let v = temp_vault("multi");
        let pdf_a = v.root().join("docusign-lease.pdf");
        let pdf_b = v.root().join("employment_agreement.pdf");
        write_pdf(&pdf_a, b"%PDF-1.4\n% lease\n%%EOF\n");
        write_pdf(&pdf_b, b"%PDF-1.4\n% employment\n%%EOF\n");

        run(&v, &pdf_a);
        run(&v, &pdf_b);

        let docs: Vec<SignedDoc> = v.read_snapshot(CATALOG).unwrap();
        assert_eq!(docs.len(), 2, "two distinct files -> two catalog rows");
        let guids: Vec<&str> = docs.iter().map(|d| d.guid.as_str()).collect();
        // GUIDs must differ.
        assert_ne!(guids[0], guids[1]);
    }

    #[test]
    fn provider_hint_detected_from_filename() {
        assert_eq!(provider_from_filename("docusign-envelope-abc.pdf"), "docusign");
        assert_eq!(provider_from_filename("dropbox-sign-contract.pdf"), "dropbox-sign");
        assert_eq!(provider_from_filename("adobe-sign-nda.pdf"), "adobe-sign");
        assert_eq!(provider_from_filename("lease-agreement.pdf"), "unknown");
    }

    #[test]
    fn title_cleaned_from_filename() {
        assert_eq!(title_from_filename("lease-agreement.pdf"), "lease agreement");
        assert_eq!(title_from_filename("employment_contract_2026.pdf"), "employment contract 2026");
        assert_eq!(title_from_filename("NDA.pdf"), "NDA");
    }

    #[test]
    fn missing_file_returns_error() {
        let v = temp_vault("err");
        let pdf = v.root().join("nonexistent.pdf");
        let result = (IMPORT.run)(&v, &pdf, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "missing file must return an error");
    }
}
