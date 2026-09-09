//! Lifesum — nutrition and diet tracking app, file [`Behavior::Import`].
//! Catalogued in the Phase 2 pass; brief: docs/integrations/lifesum.md.
//!
//! ## Vault layout
//!
//! - **Raw layer (unconditional):**
//!   `health/nutrition/lifesum/raw/<datestamp>-<filename>` — the verbatim
//!   export file, preserved at full fidelity on every import (accumulating,
//!   not overwriting).
//!
//! - **Contract layer (parked):**
//!   `health/nutrition/lifesum/YYYY-MM.jsonl` per the bound
//!   [`crate::health_nutrition::Entry`] shape. Each food diary row becomes one
//!   `Entry` (ts, source, guid, food, meal, macros, nutrients, extra). **This
//!   layer is parked** — the Lifesum export format is undocumented and no real
//!   export sample exists on disk. Once a sample confirms the exact field names
//!   and file format (CSV? JSON? ZIP?), only [`entries_from_export`] needs
//!   filling; the raw store and the contract shape are already in place.
//!
//! ## Access model
//!
//! No public API, no OAuth, no token. The user downloads their export from
//! `lifesum.com/account/export-data` (7-day quick export) or requests a full
//! GDPR dump from Lifesum support, then hands the file to this importer.
//! Trove never contacts lifesum.com.
//!
//! ## Apple Health overlap
//!
//! For iOS Lifesum users the shipped Apple Health export already captures daily
//! nutrition totals via the HealthKit passthrough. This importer adds the
//! per-food-entry originals (individual foods and meals, not aggregated totals).
//! Cross-source dedupe is a read-time concern — both sources keep their own
//! folders and stable guids at write time.
//!
//! ## Parser parked — Needs-sample (evidence rule)
//!
//! Lifesum's export format is **not officially documented**. Per the project
//! evidence rule we do **not** parse blind against an assumed field shape
//! (a green test over a fabricated fixture is false confidence — cf. the
//! raindrop `_id` bug). The raw layer stores the export verbatim; the contract
//! layer is parked until a real sample pins the exact format and column names.
//! See [`PARKED_MSG`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::write_atomic;
use crate::vault::Vault;

/// Raw-layer directory for verbatim export snapshots.
const RAW_DIR: &str = "health/nutrition/lifesum/raw";

/// Contract-layer directory: per-entry rows per the `health-nutrition` contract.
/// Written once [`entries_from_export`] is unparked.
#[allow(dead_code)]
const DIR: &str = "health/nutrition/lifesum";

/// Shown when the import is invoked before a real export sample exists to pin
/// the exact field names and file format. The raw layer stores the export
/// verbatim; the health-nutrition Entry rows are the only piece waiting.
const PARKED_MSG: &str = "Lifesum per-entry import is parked pending a real data-export sample. \
Lifesum's export format (lifesum.com/account/export-data) is not officially documented, \
and Trove does not parse export files against a guessed field shape (the evidence rule). \
The export file has been stored verbatim in health/nutrition/lifesum/raw/. \
Once a real sample is provided, the field mapping is wired in \
lifesum::entries_from_export — the raw layer and health-nutrition contract are already in place.";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The line is already present (Phase 2 stub); this build upgrades from
/// `NotWired` to `Import`.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "lifesum",
        name: "Lifesum",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Lifesum food diary from the Lifesum data export — \
                      individual food entries with macros and meal slots, adding per-entry \
                      detail beyond the daily totals Apple Health captures for iOS users. \
                      Re-importable: newer exports accumulate safely.",
        domain: "health-nutrition",
        vault_path: "health/nutrition/lifesum/",
        toggleable: false,
        setup: &[
            "lifesum.com/account/export-data — download the 7-day quick export after web login.",
            "For full history, contact Lifesum support to request a GDPR data export.",
            "Import the downloaded file here.",
        ],
        caveats: "Lifesum's export format is not officially documented — the per-entry parser \
                  is parked until a real export sample confirms the exact fields. The export \
                  file is stored verbatim in full fidelity in health/nutrition/lifesum/raw/ \
                  on every import. iOS users: the Apple Health passthrough already captures \
                  daily totals; this import adds per-food-entry originals.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Unknown format until a real sample exists — accept common container/export
    // types and store verbatim. Adjust once the format is confirmed.
    accepts: &["csv", "json", "zip"],
    params: &[],
    run: run_import,
};

/// Parse a Lifesum export file into normalized food diary entries.
///
/// **Parked — Needs-sample.** The export format and exact field names are
/// undocumented and unconfirmed. This is the *only* piece waiting on a real
/// sample: the raw store, the health-nutrition contract, and the import
/// scaffold are already in place. When a sample lands, implement the parse
/// here (detect format, walk rows, fill `Entry`) — nothing downstream changes.
///
/// Reference collector for the Entry type and persist path: `cronometer.rs`.
/// The same guid strategy applies: a stable per-entry id from the export
/// where one exists, else a content hash of (date | meal | food | amount).
#[allow(dead_code)]
fn entries_from_export(_path: &Path) -> Result<Vec<crate::health_nutrition::Entry>> {
    anyhow::bail!("{PARKED_MSG}")
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Raw layer: store the verbatim export file regardless of the parser state.
    // Full fidelity first — nothing from the export is dropped while the
    // per-entry parser is parked.
    let raw_bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // Name the raw snapshot by import timestamp so re-imports accumulate
    // (each import is a distinct snapshot, not an overwrite).
    let stamp = Local::now().format("%Y%m%dT%H%M%S").to_string();
    let orig_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("lifesum_export");
    let raw_rel = format!("{RAW_DIR}/{stamp}-{orig_name}");
    let raw_path = vault.resolve(&raw_rel)?;
    write_atomic(&raw_path, &raw_bytes)?;
    progress(ImportProgress { records: 1, percent: 50.0 });

    // Contract layer (health/nutrition/lifesum/YYYY-MM.jsonl): parked until a
    // real export sample pins the exact fields. The outcome headline surfaces
    // this clearly so the user knows the file was stored safely.
    progress(ImportProgress { records: 1, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "Export stored verbatim in health/nutrition/lifesum/raw/ — \
             per-entry import parked pending a real sample (Needs-sample). \
             Raw file: {stamp}-{orig_name}"
        ),
        counts: BTreeMap::from([("raw_files", 1u64), ("entries", 0u64)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-lifesum-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A plausible Lifesum export shape based on the brief description and common
    // nutrition tracker conventions (date, meal, food, macros). The exact field
    // names are UNCONFIRMED (Needs-sample) — this fixture is deliberately NOT
    // used to drive a parser. It represents the kind of file the export is
    // expected to contain, stored verbatim for full fidelity.
    const SCAFFOLD_BYTES: &str = r#"Date,Meal,Title,Calories,Carbohydrates,Fat,Protein
2026-06-10,Breakfast,Oatmeal,311,54.8,5.3,10.7
2026-06-10,Lunch,Chicken Salad,420,18.0,14.0,52.0
2026-06-11,Breakfast,Banana,105,27.0,0.4,1.3
"#;

    #[test]
    fn import_stores_raw_verbatim_and_returns_parked_outcome() {
        let v = temp_vault("raw-store");
        let export_path = v.root().join("lifesum_export.csv");
        fs::write(&export_path, SCAFFOLD_BYTES).unwrap();

        let outcome =
            (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        // Outcome reports raw storage and zero entries (parser parked).
        assert_eq!(outcome.counts.get("raw_files"), Some(&1));
        assert_eq!(outcome.counts.get("entries"), Some(&0));
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
        let raw_body = fs::read_to_string(entries[0].path()).unwrap();
        assert_eq!(raw_body, SCAFFOLD_BYTES, "raw snapshot is verbatim");
    }

    #[test]
    fn import_twice_accumulates_two_snapshots() {
        // Re-importing (e.g. a newer 7-day window or a GDPR dump) accumulates
        // raw snapshots rather than overwriting — each import gets its own
        // timestamped filename.
        let v = temp_vault("accumulate");
        let path1 = v.root().join("lifesum_export_v1.csv");
        fs::write(&path1, SCAFFOLD_BYTES).unwrap();
        (IMPORT.run)(&v, &path1, &BTreeMap::new(), &mut |_| {}).unwrap();

        let updated = "Date,Meal,Title,Calories\n2026-06-12,Dinner,Pasta,620\n";
        let path2 = v.root().join("lifesum_export_v2.csv");
        fs::write(&path2, updated).unwrap();
        (IMPORT.run)(&v, &path2, &BTreeMap::new(), &mut |_| {}).unwrap();

        let raw_dir = v.root().join(RAW_DIR);
        let entries: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 2, "each import leaves its own raw snapshot");
    }

    #[test]
    fn def_is_import_behavior_and_has_no_connection() {
        // DEF upgraded from NotWired to Import.
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        // No connection: the user downloads the export manually.
        assert!(DEF.connection.is_none());
        // Import box accepts the likely export formats.
        let spec = DEF.import_spec().expect("Import behavior has a spec");
        assert!(spec.accepts.contains(&"csv"), "csv in accepts");
        assert!(spec.accepts.contains(&"json"), "json in accepts");
        assert!(spec.accepts.contains(&"zip"), "zip in accepts");
        // Ships opt-in (food diary is personal health data).
        assert!(!DEF.meta.default_on);
        // Last-data hook exists for the hub card.
        assert!(DEF.last_data.is_some());
        // Domain is health-nutrition.
        assert_eq!(DEF.meta.domain, "health-nutrition");
    }

    #[test]
    fn last_data_returns_none_when_vault_is_empty() {
        let v = temp_vault("empty");
        let result = (DEF.last_data.unwrap())(&v);
        assert!(result.is_none(), "empty vault → no last_data");
    }

    #[test]
    fn last_data_returns_a_stamp_after_import() {
        let v = temp_vault("has-data");
        let export_path = v.root().join("lifesum_export.csv");
        fs::write(&export_path, SCAFFOLD_BYTES).unwrap();
        (IMPORT.run)(&v, &export_path, &BTreeMap::new(), &mut |_| {}).unwrap();

        let last = (DEF.last_data.unwrap())(&v);
        assert!(last.is_some(), "after import, last_data is set");
    }

    #[test]
    fn parked_message_names_source_function_and_raw_path() {
        // The parked message must name the source, explain the reason
        // (Needs-sample), point to the function to fill in when the sample
        // lands, and confirm the raw storage path.
        assert!(PARKED_MSG.contains("entries_from_export"), "points to the function");
        assert!(PARKED_MSG.contains("raw"), "confirms raw storage");
        assert!(
            PARKED_MSG.contains("health-nutrition") || PARKED_MSG.contains("health/nutrition"),
            "names the contract/path"
        );
    }
}
