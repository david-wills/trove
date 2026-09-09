//! Pharmacy Prescriptions — PDF/printout drop from pharmacy patient portals
//! (CVS, Walgreens, Rite Aid, and others).
//!
//! This provider is the **non-FHIR fallback** for prescription history. For
//! most users, prescriptions arrive structured through two other briefs:
//! Epic/Cerner FHIR `MedicationRequest` (via `epic-mychart` / `smart-on-fhir`)
//! and Medicare Part D drug claims (via `cms-blue-button`). This importer owns
//! the remaining path: a PDF or printout downloaded from the pharmacy's patient
//! portal and dropped into the import box.
//!
//! There is no consumer-facing pharmacy API (Walgreens and CVS developer
//! programmes are B2B refill-ordering only, not personal data retrieval).
//!
//! ## Vault layout
//!
//! - **Raw layer (unconditional):**
//!   `health/medical/pharmacy-prescriptions/raw/<stamp>-<filename>` —
//!   the verbatim uploaded file (PDF, CSV, or printout HTML), preserved at
//!   full fidelity on every import. Each import accumulates a new snapshot
//!   rather than overwriting.
//!
//! - **Contract layer (deferred — parked):**
//!   `health/medical/pharmacy-prescriptions/medications/YYYY-MM.jsonl` per the
//!   `health-medical.medication` sibling draft (medication, dosage, dates,
//!   prescriber). **Parser parked — Needs-sample**: pharmacy-portal PDF and CSV
//!   layouts are undocumented and differ per chain. Once a real sample arrives,
//!   implement `prescriptions_from_file` here; the raw layer and the
//!   `health-medical.medication` contract path are already established by the
//!   pioneer brief. Source-specific fields (NDC, days supply, quantity,
//!   refills remaining, pharmacy name) belong in `extra`.
//!
//! ## Privacy
//!
//! Prescription history is sensitive medical data. Ships `default_on: false`
//! with the opt-in caveat on the hub card.
//!
//! Brief: docs/integrations/pharmacy-prescriptions.md

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::write_atomic;
use crate::vault::Vault;

/// Verbatim-file raw layer: accumulating snapshots of every imported file.
const RAW_DIR: &str = "health/medical/pharmacy-prescriptions/raw";

/// Contract layer (parked pending a real sample confirming exact field names).
/// When unparked, this is the `medications/` sub-folder under the source root.
#[allow(dead_code)]
const MED_DIR: &str = "health/medical/pharmacy-prescriptions/medications";

/// Surfaced on the hub card when the import is invoked, so the user knows the
/// file was stored safely and why the structured-data layer is not yet active.
const PARKED_MSG: &str = "\
Pharmacy portal layouts (PDF, CSV, HTML printouts) are undocumented and \
differ per chain (CVS, Walgreens, Rite Aid, and others). Trove does not \
parse export files against an assumed field shape (the evidence rule). \
Your export has been stored verbatim in health/medical/pharmacy-prescriptions/raw/. \
When a real export sample is available, implement the per-prescription parser \
in pharmacy_prescriptions::prescriptions_from_file — the raw layer and the \
health-medical.medication contract path are already in place. \
For structured prescription data without a portal export, see \
epic-mychart/smart-on-fhir (FHIR MedicationRequest) and \
cms-blue-button (Medicare Part D fills).";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (line already present
/// from the Phase-2 stub — this body replaces `NotWired` with `Import`).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "pharmacy-prescriptions",
        name: "Pharmacy Prescriptions",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your prescription history from pharmacy patient portals \
                      (CVS, Walgreens, Rite Aid, and others) — medication names, \
                      fill/refill dates, dosage, and prescriber. For structured data, \
                      Epic MyChart (FHIR) and Medicare Blue Button cover most users \
                      automatically.",
        domain: "health",
        vault_path: "health/medical/pharmacy-prescriptions/",
        toggleable: false,
        setup: &[
            "Log in to your pharmacy's patient portal (CVS, Walgreens, Rite Aid, etc.).",
            "Locate your prescription history and download or print it as a PDF or CSV.",
            "Import the file here. Your file is preserved verbatim; per-prescription \
             parsing requires a sample from your chain to confirm exact field names.",
            "For structured data without a portal export, use Epic MyChart (FHIR) \
             or Medicare Blue Button instead.",
        ],
        caveats: "Prescription history is sensitive medical data — opt-in only. \
                  No consumer pharmacy API exists; structured data arrives via \
                  Epic/Cerner FHIR (MedicationRequest) or Medicare Blue Button (Part D). \
                  The portal PDF/CSV parser is parked until a real sample is available \
                  to confirm exact field names per chain.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Accept the most common pharmacy portal export formats. PDFs are the most
    // common; some portals offer CSV. HTML printouts saved as .html are also
    // accepted for verbatim preservation. Format is confirmed per-chain once a
    // real sample lands.
    accepts: &["pdf", "csv", "html", "zip"],
    params: &[],
    run: run_import,
};

/// Parse a pharmacy portal export file into normalized prescription rows.
///
/// **Parked — Needs-sample.** Pharmacy-portal layouts (PDF, CSV, HTML
/// printouts) are undocumented and differ per chain. Per the project evidence
/// rule we do not parse blind against an assumed field shape. This is the
/// *only* piece waiting on a real sample: the raw store and the
/// `health-medical.medication` sibling contract path are already established.
///
/// When a real sample lands, implement here:
/// 1. Detect file format (PDF text extraction / CSV columns / HTML parse).
/// 2. Map chain-specific columns to the normalized shape:
///    `ts` (fill date), `source` ("pharmacy-prescriptions"),
///    `guid` (NDC+date+prescriber or content hash), `name` (medication),
///    `dose`, `kind` ("fill"), `prescriber`, `start`/`end`, `code`/`code_system`
///    (NDC → `"ndc"`), and chain-specific fields in `extra`
///    (days supply, quantity, refills remaining, pharmacy name).
/// 3. Persist via `vault.stream(MED_DIR, Partition::Month).append(...)`.
/// 4. Flip `parser_parked_needs_sample` in the integration brief.
///
/// Reference for the medication shape: `smart_on_fhir.rs` (FHIR
/// MedicationRequest → medication rows) and `cms_blue_button.rs` (Part D
/// fills → medication rows). The schema field spec is in
/// `docs/vault-spec/domains/health-medical.md`.
#[allow(dead_code)]
fn prescriptions_from_file(_path: &Path) -> Result<Vec<serde_json::Value>> {
    anyhow::bail!("{PARKED_MSG}")
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Raw layer (unconditional): store the verbatim export file.
    // Full fidelity first — no pharmacy-specific data is discarded while the
    // per-prescription parser is parked. Each import accumulates a timestamped
    // snapshot so re-imports of newer portal exports do not overwrite history.
    let raw_bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
    let orig_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pharmacy_export");
    let raw_rel = format!("{RAW_DIR}/{stamp}-{orig_name}");
    let raw_path = vault.resolve(&raw_rel)?;
    write_atomic(&raw_path, &raw_bytes)?;
    progress(ImportProgress { records: 1, percent: 50.0 });

    // Contract layer (health/medical/pharmacy-prescriptions/medications/):
    // deferred — health-medical.medication sibling draft, parked pending a real
    // sample that confirms the exact field names and file format per chain.
    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Export stored verbatim in health/medical/pharmacy-prescriptions/raw/ — \
             per-prescription parsing parked (Needs-sample: pharmacy portal layouts \
             differ per chain). Raw file: {stamp}-{orig_name}"
        ),
        counts: BTreeMap::from([("raw_files", 1u64), ("prescriptions", 0u64)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-pharmacy-prescriptions-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    /// A plausible pharmacy portal CSV printout (field names are illustrative —
    /// the real format per chain is UNCONFIRMED, Needs-sample). This fixture is
    /// stored verbatim; it is NOT parsed by the parked prescription extractor.
    const SCAFFOLD_CSV: &str = "\
Drug Name,Strength,Fill Date,Days Supply,Qty Dispensed,Prescriber,Pharmacy\r\n\
Atorvastatin,20 MG,2026-06-01,90,90,Dr. Priya Nair,CVS #1234\r\n\
Lisinopril,10 MG,2026-05-15,30,30,Dr. Priya Nair,CVS #1234\r\n\
";

    #[test]
    fn import_stores_raw_verbatim_and_returns_parked_outcome() {
        let v = temp_vault("raw-store");
        let path = v.root().join("prescriptions.csv");
        fs::write(&path, SCAFFOLD_CSV).unwrap();

        let outcome =
            (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Outcome reports raw storage and zero parsed rows (parser parked).
        assert_eq!(outcome.counts.get("raw_files"), Some(&1));
        assert_eq!(outcome.counts.get("prescriptions"), Some(&0));
        assert!(
            outcome.headline.contains("raw"),
            "headline should mention raw storage: {}",
            outcome.headline
        );
        assert!(
            outcome.headline.contains("parked") || outcome.headline.contains("Needs-sample"),
            "headline should note the parked parser: {}",
            outcome.headline
        );

        // The raw directory contains exactly one snapshot.
        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "one raw snapshot written");

        // Raw snapshot is byte-identical to what was imported (full fidelity).
        let raw_body = fs::read(&entries[0].path()).unwrap();
        assert_eq!(raw_body, SCAFFOLD_CSV.as_bytes(), "raw snapshot is verbatim");
    }

    #[test]
    fn import_twice_accumulates_two_snapshots() {
        // Re-importing (e.g. a newer portal export) accumulates raw snapshots
        // rather than overwriting — each import gets its own timestamped filename.
        let v = temp_vault("accumulate");
        let path1 = v.root().join("prescriptions_v1.csv");
        fs::write(&path1, SCAFFOLD_CSV).unwrap();
        (IMPORT.run)(&v, &path1, &BTreeMap::new(), &mut |_| {}).unwrap();

        let updated = "Drug Name,Fill Date\r\nMetformin 500mg,2026-06-10\r\n";
        let path2 = v.root().join("prescriptions_v2.csv");
        fs::write(&path2, updated).unwrap();
        (IMPORT.run)(&v, &path2, &BTreeMap::new(), &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 2, "each import leaves its own raw snapshot");
    }

    #[test]
    fn pdf_bytes_stored_verbatim() {
        // A PDF starts with the magic bytes %PDF — confirm binary files are
        // stored intact (no text-mode corruption).
        let v = temp_vault("pdf");
        let fake_pdf = b"%PDF-1.4\n%fake pharmacy prescription PDF content\n%%EOF\n";
        let path = v.root().join("prescriptions.pdf");
        fs::write(&path, fake_pdf).unwrap();

        (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        let stored = fs::read(&entries[0].path()).unwrap();
        assert_eq!(stored, fake_pdf as &[u8], "PDF bytes stored verbatim");
    }

    #[test]
    fn def_is_import_behavior_and_has_no_connection() {
        // DEF upgraded from NotWired to Import.
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        // No connection — the user downloads the export manually; no API.
        assert!(DEF.connection.is_none());
        // Import box accepts the documented pharmacy export formats.
        let spec = DEF.import_spec().expect("Import behavior has a spec");
        assert!(spec.accepts.contains(&"pdf"), "pdf in accepts");
        assert!(spec.accepts.contains(&"csv"), "csv in accepts");
        assert!(spec.accepts.contains(&"html"), "html in accepts");
        // Ships opt-in (sensitive medical data).
        assert!(!DEF.meta.default_on, "opt-in for sensitive medical data");
        // Last-data hook exists for the hub card.
        assert!(DEF.last_data.is_some());
        // Domain is health.
        assert_eq!(DEF.meta.domain, "health");
    }

    #[test]
    fn last_data_returns_none_when_vault_is_empty() {
        let v = temp_vault("empty");
        let result = (DEF.last_data.unwrap())(&v);
        assert!(result.is_none(), "empty vault → no last_data");
    }

    #[test]
    fn last_data_returns_stamp_after_import() {
        let v = temp_vault("has-data");
        let path = v.root().join("prescriptions.csv");
        fs::write(&path, SCAFFOLD_CSV).unwrap();
        (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let last = (DEF.last_data.unwrap())(&v);
        assert!(last.is_some(), "after import, last_data is set");
    }

    #[test]
    fn parked_message_names_source_function_and_raw_path() {
        assert!(PARKED_MSG.contains("prescriptions_from_file"), "names the function to implement");
        assert!(PARKED_MSG.contains("raw"), "confirms raw storage path");
        assert!(
            PARKED_MSG.contains("health-medical.medication")
                || PARKED_MSG.contains("health/medical"),
            "names the contract or path"
        );
    }

    #[test]
    fn hub_card_surfaces_correctly() {
        let v = temp_vault("hub");
        let path = v.root().join("prescriptions.csv");
        fs::write(&path, SCAFFOLD_CSV).unwrap();
        (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let status = v.integrations_status();
        let card = status
            .iter()
            .find(|s| s.id == "pharmacy-prescriptions")
            .unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert!(import_info.accepts.contains(&"pdf"));
        assert!(import_info.accepts.contains(&"csv"));
        assert!(card.last_data.is_some());
    }
}
