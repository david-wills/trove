//! Medisafe — medication reminder and tracker app with in-app CSV export.
//!
//! Medisafe exports a CSV from its Reports screen (Reports → Export → choose
//! medication/timeframe). Since January 2026 the export requires a **Premium**
//! subscription; free users have no file to import.
//!
//! ## CSV schema status
//!
//! The CSV format is not publicly documented, and no sample is on disk.
//! The raw parser is scaffolded here to accept any CSV and archive each row
//! as a verbatim JSON object under `health/medical/medisafe/raw/`; the full
//! adherence-event parser is **parked pending a real export sample**.
//!
//! Once a real sample is available:
//!
//! 1. Confirm the exact column names (expected: medication name, dose, date,
//!    time, taken/missed status, notes — but the exact headers are unknown).
//! 2. Implement `entry_from_row` (currently a stub) to map CSV columns to the
//!    normalized `AdherenceEntry` shape.
//! 3. The normalized layer (`health/medical/medisafe/adherence/YYYY-MM.jsonl`)
//!    is the natural home for the `health-medical.medication` sibling draft
//!    when that contract is bound (it tracks when medications were taken/missed,
//!    which is adherence over prescribed meds).
//!
//! ## Vault layout
//!
//! - **Raw (unconditional):** `health/medical/medisafe/raw/YYYY-MM.jsonl` —
//!   the verbatim CSV rows as header→value JSON objects, month-partitioned by
//!   the date found in the row (or by import date if none is parseable).
//!
//! This data is medical detail (medication names, doses, adherence) and ships
//! `default_on: false` — the user opts in with explicit acknowledgement.
//!
//! Brief: docs/integrations/medisafe.md

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Raw CSV-row objects (full fidelity, unconditional).
const RAW_DIR: &str = "health/medical/medisafe/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "medisafe",
        name: "Medisafe",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Medisafe medication adherence CSV — doses taken, \
                      missed, and adherence rates — into the vault. Re-runnable: \
                      re-importing an overlapping export never duplicates entries.",
        domain: "health",
        vault_path: "health/medical/medisafe/",
        toggleable: false,
        setup: &[
            "Open the Medisafe app → Reports → Export → choose medication / time frame.",
            "The CSV will be emailed to you; save it and import it here.",
            "Note: export requires a Medisafe Premium subscription (paid since Jan 2026). \
             For prescription records without Premium, see Epic/SMART on FHIR.",
        ],
        caveats: "Export requires a Medisafe Premium subscription (paid since January 2026). \
                  Free users have no export access. \
                  This data is health-sensitive (medication names, doses, adherence) — opt-in only.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Raw row — the verbatim CSV row as a JSON object for the unconditional raw
// layer. `ts` is used only for partitioning and is skipped on disk; the raw
// map carries the verbatim date column instead.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;
    import_body(vault, &body, progress)
}

/// The import body over an in-memory CSV string — the testable seam.
fn import_body(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // Track raw guids to avoid re-writing verbatim rows already in the archive
    // on re-import (idempotent for the raw layer too).
    let mut seen_raw: HashSet<String> = HashSet::new();
    for key in raw_stream.partitions()? {
        for v in raw_stream.read::<Value>(&key)? {
            if let Some(g) = v.get("_guid").and_then(Value::as_str) {
                if !g.is_empty() {
                    seen_raw.insert(g.to_string());
                }
            }
        }
    }

    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .context(
            "reading CSV header row — is this a Medisafe CSV export? \
             Expected columns include medication name, dose, date, taken/missed status.",
        )?
        .clone();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut raws: Vec<RawLine> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };

        // Build verbatim header→value map for the raw layer.
        let mut fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();

        // Derive a stable guid from the row content, for raw-layer dedup.
        let guid = row_content_guid(&rec);
        // Partition ts: try to find a date-like column in the row; fall back to
        // a sentinel "0000-00" which sorts to the beginning and signals "unknown
        // date format" — the real parser (post-sample) will map the correct col.
        let ts = date_ts_from_row(&headers, &rec)
            .unwrap_or_else(|| "0000-00-01".to_string());

        if !seen_raw.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }

        // Embed guid into the raw object so the scanner can load it for dedup.
        fields.insert("_guid".into(), Value::String(guid));
        raws.push(RawLine { ts, value: Value::Object(fields) });
        imported += 1;

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    raw_stream.append(&raws, |r| &r.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!("{imported} rows archived (raw), {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Try to find a YYYY-MM-DD date in the CSV row by scanning column values for
/// anything that looks like an ISO date. Falls back to `None` when nothing
/// matches (the real parser, post-sample, will use the correct column by name).
///
/// The returned string is always normalized to use `'-'` separators so it is
/// a valid [`Partition::key`] prefix (`Partition::Month::key` requires `'-'`
/// at index 4; a raw `'/'` from YYYY/MM/DD exports would cause `append` to
/// error and abort the whole import with zero rows written).
fn date_ts_from_row(headers: &csv::StringRecord, rec: &csv::StringRecord) -> Option<String> {
    // First, look for a column whose header contains "date" (case-insensitive).
    for (h, v) in headers.iter().zip(rec.iter()) {
        if h.to_ascii_lowercase().contains("date") {
            let v = v.trim();
            if looks_like_date(v) {
                return Some(normalize_date_sep(v));
            }
        }
    }
    // Second pass: any column whose value looks like YYYY-MM-DD.
    for v in rec.iter() {
        let v = v.trim();
        if looks_like_date(v) {
            return Some(normalize_date_sep(v));
        }
    }
    None
}

/// Replace any `'/'` separators in a detected date string with `'-'` so the
/// result is always a valid `Partition::key` prefix (e.g. `"2026/06/10"` →
/// `"2026-06-10"`). Only the separator positions (4, 7) are replaced; digit
/// characters are unchanged.
fn normalize_date_sep(s: &str) -> String {
    s.chars()
        .enumerate()
        .map(|(i, c)| if (i == 4 || i == 7) && c == '/' { '-' } else { c })
        .collect()
}

/// Rough check: a string that starts with a 4-digit year and uses dashes or
/// slashes as separators (e.g. "2026-01-15", "2026/01/15").
fn looks_like_date(s: &str) -> bool {
    if s.len() < 8 {
        return false;
    }
    let b = s.as_bytes();
    // First four chars must be ASCII digits.
    b[..4].iter().all(|c| c.is_ascii_digit())
        && (b[4] == b'-' || b[4] == b'/')
}

/// A stable short hex digest over the row's field values — used as a raw-layer
/// dedup key (SHA-256, truncated to 10 hex chars). Includes the index of each
/// field to prevent accidental collisions when two different CSV layouts happen
/// to yield the same concatenated string.
fn row_content_guid(rec: &csv::StringRecord) -> String {
    let mut h = Sha256::new();
    for (i, v) in rec.iter().enumerate() {
        h.update(format!("{i}:{v}\u{1f}").as_bytes());
    }
    let digest = h.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-medisafe-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    /// Synthetic Medisafe-like export. The real column names are unknown (schema
    /// is undocumented, no sample on disk); this fixture uses the field names
    /// implied by the brief ("adherence rates, doses taken/missed, timestamps,
    /// notes") as placeholders. The real parser will confirm exact names against
    /// a genuine export.
    ///
    /// PARSER PARKED — once a real export sample is available, update
    /// `entry_from_row` to map the confirmed column names to the normalized
    /// `AdherenceEntry` shape.
    const SAMPLE_CSV: &str = "\
Medication,Dose,Date,Time,Status,Notes\n\
Lisinopril 10mg,1 tablet,2026-06-10,08:00,Taken,\n\
Atorvastatin 20mg,1 tablet,2026-06-10,21:00,Taken,With food\n\
Lisinopril 10mg,1 tablet,2026-06-11,08:00,Missed,Forgot\n\
";

    fn import(v: &crate::vault::Vault, body: &str) -> ImportOutcome {
        import_body(v, body, &mut |_| {}).unwrap()
    }

    #[test]
    fn archives_all_rows_to_raw_layer() {
        let v = temp_vault("raw");
        let out = import(&v, SAMPLE_CSV);
        assert_eq!(out.counts.get("imported"), Some(&3));
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert_eq!(out.counts.get("skipped"), Some(&0));

        // Raw layer exists.
        let raw = fs::read_to_string(v.root().join("health/medical/medisafe/raw/2026-06.jsonl"))
            .unwrap();
        assert_eq!(raw.lines().count(), 3, "three rows archived");

        // Verbatim column names and values preserved.
        let first: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(first["Medication"], "Lisinopril 10mg");
        assert_eq!(first["Status"], "Taken");
        assert_eq!(first["Date"], "2026-06-10");
        // A stable guid is embedded for dedup.
        assert!(first.get("_guid").is_some(), "raw row carries _guid for dedup");
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("dedup");
        import(&v, SAMPLE_CSV);
        let before = fs::read_to_string(v.root().join("health/medical/medisafe/raw/2026-06.jsonl"))
            .unwrap();

        let again = import(&v, SAMPLE_CSV);
        let after = fs::read_to_string(v.root().join("health/medical/medisafe/raw/2026-06.jsonl"))
            .unwrap();

        assert_eq!(again.counts.get("duplicates"), Some(&3));
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(before, after, "raw file byte-identical after re-import");
    }

    #[test]
    fn date_detection_finds_iso_date_from_date_column() {
        let mut hdr = csv::StringRecord::new();
        hdr.push_field("Medication");
        hdr.push_field("Date");
        hdr.push_field("Status");

        let mut rec = csv::StringRecord::new();
        rec.push_field("Aspirin");
        rec.push_field("2026-06-15");
        rec.push_field("Taken");

        let ts = date_ts_from_row(&hdr, &rec);
        assert_eq!(ts, Some("2026-06-15".to_string()));
    }

    #[test]
    fn date_detection_falls_back_to_value_scan() {
        // If no column is named "date", scan values.
        let mut hdr = csv::StringRecord::new();
        hdr.push_field("Med");
        hdr.push_field("When");

        let mut rec = csv::StringRecord::new();
        rec.push_field("Aspirin");
        rec.push_field("2026-06-15");

        let ts = date_ts_from_row(&hdr, &rec);
        assert_eq!(ts, Some("2026-06-15".to_string()));
    }

    #[test]
    fn slash_date_export_archives_all_rows_without_aborting() {
        // Medisafe exports using YYYY/MM/DD separators must not abort the whole
        // import. Before the fix, `looks_like_date` accepted '/' but the
        // verbatim string ("2026/06/10") failed `Partition::Month::key` (requires
        // '-' at index 4), causing `append` to error and write ZERO rows.
        let csv_body = "\
Medication,Dose,Date,Time,Status,Notes\n\
Lisinopril 10mg,1 tablet,2026/06/10,08:00,Taken,\n\
Atorvastatin 20mg,1 tablet,2026/06/10,21:00,Taken,With food\n\
Lisinopril 10mg,1 tablet,2026/06/11,08:00,Missed,Forgot\n\
";
        let v = temp_vault("slashdate");
        let out = import(&v, csv_body);
        assert_eq!(out.counts.get("imported"), Some(&3), "slash dates must not abort import");
        // Normalized to YYYY-MM partition.
        let raw = fs::read_to_string(v.root().join("health/medical/medisafe/raw/2026-06.jsonl"))
            .unwrap();
        assert_eq!(raw.lines().count(), 3, "all three rows written to normalized partition");
        // Verbatim value preserved in the raw object (original slash date stored as-is).
        let first: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(first["Date"], "2026/06/10", "verbatim slash date preserved in raw object");
    }

    #[test]
    fn normalize_date_sep_replaces_slashes_only_at_sep_positions() {
        assert_eq!(normalize_date_sep("2026/06/10"), "2026-06-10");
        assert_eq!(normalize_date_sep("2026-06-10"), "2026-06-10");
        // Only positions 4 and 7; other chars untouched.
        assert_eq!(normalize_date_sep("2026/06"), "2026-06");
    }

    #[test]
    fn row_with_unparseable_date_still_archived_under_sentinel() {
        // A row with no date-like value falls back to "0000-00-01" partition —
        // it still lands in the raw layer, not silently dropped.
        let csv_body = "Medication,Dose,Status\nLibrium 5mg,2 caps,Taken\n";
        let v = temp_vault("nodate");
        let out = import(&v, csv_body);
        assert_eq!(out.counts.get("imported"), Some(&1));
        // Lands in the sentinel partition file.
        let raw = fs::read_to_string(v.root().join("health/medical/medisafe/raw/0000-00.jsonl"))
            .unwrap();
        assert_eq!(raw.lines().count(), 1);
    }

    #[test]
    fn guid_stable_per_row_content() {
        let mut a = csv::StringRecord::new();
        a.push_field("Aspirin");
        a.push_field("81mg");
        a.push_field("2026-06-10");

        let mut b = csv::StringRecord::new();
        b.push_field("Aspirin");
        b.push_field("81mg");
        b.push_field("2026-06-11");

        let g_a1 = row_content_guid(&a);
        let g_a2 = row_content_guid(&a);
        let g_b = row_content_guid(&b);

        assert_eq!(g_a1, g_a2, "same content → same guid");
        assert_ne!(g_a1, g_b, "different date → different guid");
        assert_eq!(g_a1.len(), 10, "10 hex chars");
    }

    #[test]
    fn run_import_reads_from_file() {
        let v = temp_vault("fileio");
        let path = v.root().join("medisafe-export.csv");
        fs::write(&path, SAMPLE_CSV).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3));
    }

    #[test]
    fn hub_card_and_last_data_surface() {
        let v = temp_vault("hub");
        import(&v, SAMPLE_CSV);

        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "medisafe").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2026-06"));
    }
}
