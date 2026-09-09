//! Bearable symptom, mood, and medication tracker CSV import.
//!
//! Bearable is a subjective health-tracking app: users log mood ratings, pain
//! and fatigue scores, symptom severity, medications taken, and lifestyle
//! factors (sleep, steps, custom factors) with per-entry timestamps. The app
//! exposes its data only via a manual CSV export (Settings → Export Data).
//!
//! ## CSV format (confirmed against samstarling/bearable-csv types.ts)
//!
//! The export is a long-format log: one row per factor entry (so a single
//! check-in that covers mood, three symptoms, and a medication yields four or
//! more rows, all with the same date + time_of_day). Columns:
//!
//! ```text
//! date, weekday, time_of_day, category, rating_or_amount, detail, notes
//! ```
//!
//! - `date` — `YYYY-MM-DD`
//! - `weekday` — day name ("Monday")
//! - `time_of_day` — freeform slot ("Morning", "Afternoon", "Night", …)
//! - `category` — factor type ("Mood", "Symptom", "Medication", "Sleep",
//!   "Steps", or any user-defined custom category)
//! - `rating_or_amount` — a numeric rating or dosage (may be blank for
//!   presence-only factors like a medication tick)
//! - `detail` — the factor name (symptom name, medication name, etc.)
//! - `notes` — optional free-text note (may be blank)
//!
//! ## Vault layout
//!
//! Raw (full fidelity): `health/bearable/raw/YYYY-MM.jsonl` — the verbatim
//! CSV row as a JSON object, keyed by verbatim header name. The raw layer is
//! written unconditionally for every parseable row — it is a verbatim archive.
//!
//! Parsed entries: `health/bearable/YYYY-MM.jsonl` — normalised per-entry
//! objects, month-partitioned by `date`. Re-import of an overlapping export is
//! idempotent (guid dedupe: `date|time_of_day|category|detail|rating_or_amount|notes`,
//! plus an occurrence index so byte-identical rows within a single export all
//! land, and re-importing that same export is still idempotent).
//!
//! No contract layer: Bearable logs are subjective self-tracked factors, not
//! quantitative biometric readings. They live in `health/bearable/` in their
//! native shape; read-time views can join them with biometrics by timestamp.
//!
//! This is health-detail data (mood/symptom/medication specifics) so
//! `default_on: false` — the user opts in with explicit acknowledgement.
//!
//! Brief: docs/integrations/bearable.md

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
// `Deserialize` used by Entry (read-back in dedup scan) and test deserialization.
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Parsed entries directory (month-partitioned, per-source raw layout).
const DIR: &str = "health/bearable";
/// Raw CSV-row objects directory (full fidelity).
const RAW_DIR: &str = "health/bearable/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bearable",
        name: "Bearable",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your symptom logs, mood ratings, and medication records \
                      from a Bearable CSV export. Re-runnable: re-importing an overlapping \
                      export never duplicates entries.",
        domain: "health",
        vault_path: "health/bearable/",
        toggleable: false,
        setup: &[
            "Open the Bearable app → Settings → Export Data → CSV.",
            "Save the file to your device, then import it here.",
        ],
        caveats: "Full history export requires a Bearable Premium subscription ($34.99/yr). \
                  Free tier exports a recent window — the same parser handles both. \
                  This data is health-sensitive (mood, symptoms, medications) — opt-in only.",
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
// Parsed entry — the normalised per-entry object written to health/bearable/.

/// A normalised Bearable log entry; month-partitioned by `date`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    /// ISO date (`YYYY-MM-DD`) from the export. Used for partitioning.
    pub ts: String,
    /// Source identifier — always `"bearable"`.
    pub source: String,
    /// Stable dedupe key: `date|time_of_day|category|detail|rating_or_amount|notes`
    /// plus an occurrence index so byte-identical rows within a single export
    /// each get a distinct guid while re-import of the same export is idempotent.
    pub guid: String,
    /// Day-of-week name as the export provides ("Monday", etc.).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub weekday: String,
    /// Time-of-day slot ("Morning", "Afternoon", "Night", or custom).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub time_of_day: String,
    /// Factor category ("Mood", "Symptom", "Medication", "Sleep", "Steps",
    /// or any user-defined category). Carries through verbatim.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub category: String,
    /// Factor name — the specific symptom, medication, or custom factor.
    /// Blank for top-level category rows like a bare mood rating.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Numeric rating or amount as a string (preserves original precision).
    /// Blank for presence-only entries.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rating_or_amount: String,
    /// Optional free-text note from the user.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

// ---------------------------------------------------------------------------
// Raw row — the verbatim CSV row as a JSON object, for the unconditional raw
// layer. The `ts` field is used only for partitioning and is skipped on disk;
// the raw map carries the verbatim `date` column instead.

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
    let entries_stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids for re-runnable deduplication of the parsed layer.
    let mut seen: HashSet<String> = HashSet::new();
    for key in entries_stream.partitions()? {
        for v in entries_stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                if !g.is_empty() {
                    seen.insert(g.to_string());
                }
            }
        }
    }

    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .context(
            "reading CSV header row — is this a Bearable CSV export? \
             Expected columns: date, weekday, time_of_day, category, rating_or_amount, detail, notes",
        )?
        .clone();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut entries: Vec<Entry> = Vec::new();
    // Raw rows are collected for ALL parseable rows (unconditional full fidelity).
    let mut raws: Vec<RawLine> = Vec::new();
    // Occurrence counter for the 6-field base key so byte-identical rows in
    // a single export each get a distinct guid, while re-importing the same
    // export at any later time is still idempotent.
    let mut occurrence: HashMap<String, u32> = HashMap::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };

        // Build verbatim header→value map for the raw layer (full fidelity).
        let fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();

        let Some(entry) = entry_from_row(&rec, &mut occurrence) else {
            skipped += 1;
            continue;
        };

        // Raw layer: written unconditionally for every parseable row, before
        // the parsed-layer dedup gate — the raw archive is verbatim regardless.
        raws.push(RawLine { ts: entry.ts.clone(), value: Value::Object(fields) });

        // Parsed layer: skip rows whose guid already appears in the vault
        // (idempotent re-import of overlapping exports).
        if !seen.insert(entry.guid.clone()) {
            duplicates += 1;
            continue;
        }

        entries.push(entry);
        imported += 1;

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Raw layer first (unconditional full fidelity), then the parsed entries.
    raw_stream.append(&raws, |r| &r.ts)?;
    entries_stream.append(&entries, |e| &e.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!("{imported} entries imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// One CSV string-record → a parsed [`Entry`].
/// Returns `None` when the row has no parseable `date` (can't partition).
///
/// `occurrence` tracks how many times each 6-field base key has appeared in
/// the current import pass; the count is embedded in the guid so byte-identical
/// rows within a single export each receive a distinct, stable id while
/// re-importing the same export at any later time produces the same guids.
fn entry_from_row(
    rec: &csv::StringRecord,
    occurrence: &mut HashMap<String, u32>,
) -> Option<Entry> {
    // Positional mapping: date[0], weekday[1], time_of_day[2], category[3],
    // rating_or_amount[4], detail[5], notes[6].
    let date = rec.get(0).map(str::trim).unwrap_or("").to_string();
    // Must yield a valid date for month-partitioning.
    NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?;

    let weekday = rec.get(1).map(str::trim).unwrap_or("").to_string();
    let time_of_day = rec.get(2).map(str::trim).unwrap_or("").to_string();
    let category = rec.get(3).map(str::trim).unwrap_or("").to_string();
    let rating_or_amount = rec.get(4).map(str::trim).unwrap_or("").to_string();
    let detail = rec.get(5).map(str::trim).unwrap_or("").to_string();
    let notes = rec.get(6).map(str::trim).unwrap_or("").to_string();

    // Build the stable dedupe guid including all value fields so rows that
    // share a (date, slot, category, detail) tuple but differ in
    // rating_or_amount or notes are treated as distinct entries.
    let idx = {
        let base = entry_guid_base(&date, &time_of_day, &category, &detail, &rating_or_amount, &notes);
        let count = occurrence.entry(base).or_insert(0);
        let idx = *count;
        *count += 1;
        idx
    };
    let guid = entry_guid(&date, &time_of_day, &category, &detail, &rating_or_amount, &notes, idx);

    Some(Entry {
        ts: date,
        source: "bearable".into(),
        guid,
        weekday,
        time_of_day,
        category,
        detail,
        rating_or_amount,
        notes,
    })
}

/// The raw base key (before occurrence-indexing): SHA-256 of
/// `date|time_of_day|category|detail|rating_or_amount|notes`.
/// Used only to track per-import occurrence counts.
fn entry_guid_base(
    date: &str,
    time_of_day: &str,
    category: &str,
    detail: &str,
    rating_or_amount: &str,
    notes: &str,
) -> String {
    let mut h = Sha256::new();
    h.update(
        [date, time_of_day, category, detail, rating_or_amount, notes]
            .join("\u{1f}")
            .as_bytes(),
    );
    let digest = h.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// Stable, occurrence-indexed dedupe key: SHA-256 of
/// `date|time_of_day|category|detail|rating_or_amount|notes|<idx>`
/// (unit-separator-joined). Including `rating_or_amount` and `notes` ensures
/// that two legitimately distinct entries in the same slot (e.g. the same
/// medication taken twice at different doses, or the same symptom at different
/// severities) yield different guids and are both preserved. The occurrence
/// index `idx` (0-based within a single import pass) makes truly byte-identical
/// rows distinct while still allowing re-import of the same export to be
/// idempotent (the same export always produces the same idx sequence).
/// Truncated to 10 hex chars — collision-safe at diary scale.
fn entry_guid(
    date: &str,
    time_of_day: &str,
    category: &str,
    detail: &str,
    rating_or_amount: &str,
    notes: &str,
    idx: u32,
) -> String {
    let mut h = Sha256::new();
    h.update(
        format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
            date, time_of_day, category, detail, rating_or_amount, notes, idx
        )
        .as_bytes(),
    );
    let digest = h.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-bearable-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A minimal realistic Bearable CSV export. Long-format: one row per
    /// factor entry. Columns confirmed against samstarling/bearable-csv types.ts.
    const EXPORT: &str = "\
date,weekday,time_of_day,category,rating_or_amount,detail,notes\n\
2026-06-10,Wednesday,Morning,Mood,7,,Feeling okay\n\
2026-06-10,Wednesday,Morning,Symptom,4,Headache,\n\
2026-06-10,Wednesday,Morning,Medication,,Ibuprofen,Taken with food\n\
2026-06-10,Wednesday,Evening,Mood,6,,\n\
2026-06-11,Thursday,Morning,Mood,8,,\n\
2026-06-11,Thursday,Morning,Sleep,7.5,,\n\
2026-06-11,Thursday,Morning,Steps,8432,,\n\
";

    fn import(v: &Vault, body: &str) -> ImportOutcome {
        import_body(v, body, &mut |_| {}).unwrap()
    }

    #[test]
    fn parses_all_rows_and_maps_fields_correctly() {
        let v = temp_vault("parse");
        let out = import(&v, EXPORT);
        assert_eq!(out.counts.get("imported"), Some(&7));
        assert_eq!(out.counts.get("skipped"), Some(&0));
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Both months land in the same 2026-06 partition.
        let jun = fs::read_to_string(v.root().join("health/bearable/2026-06.jsonl")).unwrap();
        assert_eq!(jun.lines().count(), 7, "all 7 entries written");

        // Spot-check a mood row.
        let first: Value = serde_json::from_str(jun.lines().next().unwrap()).unwrap();
        assert_eq!(first["source"], "bearable");
        assert_eq!(first["ts"], "2026-06-10");
        assert_eq!(first["category"], "Mood");
        assert_eq!(first["rating_or_amount"], "7");
        assert_eq!(first["notes"], "Feeling okay");
        // detail blank → omitted (skip_serializing_if)
        assert!(first.get("detail").is_none() || first["detail"] == "", "blank detail omitted or empty");

        // Spot-check a symptom row.
        let rows: Vec<Value> = jun
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let symptom = rows.iter().find(|r| r["category"] == "Symptom").unwrap();
        assert_eq!(symptom["detail"], "Headache");
        assert_eq!(symptom["rating_or_amount"], "4");
        assert_eq!(symptom["time_of_day"], "Morning");

        // Medication with blank rating_or_amount.
        let med = rows.iter().find(|r| r["category"] == "Medication").unwrap();
        assert_eq!(med["detail"], "Ibuprofen");
        assert_eq!(med["notes"], "Taken with food");

        // Sleep and steps (lifestyle factors).
        let sleep = rows.iter().find(|r| r["category"] == "Sleep").unwrap();
        assert_eq!(sleep["rating_or_amount"], "7.5");
        let steps = rows.iter().find(|r| r["category"] == "Steps").unwrap();
        assert_eq!(steps["rating_or_amount"], "8432");
    }

    #[test]
    fn raw_layer_written_with_verbatim_csv_cells() {
        let v = temp_vault("raw");
        import(&v, EXPORT);

        let raw = fs::read_to_string(v.root().join("health/bearable/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 7, "raw layer has same row count");

        // Raw preserves verbatim column names and values.
        let row: Value = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(row["date"], "2026-06-10");
        assert_eq!(row["weekday"], "Wednesday");
        assert_eq!(row["time_of_day"], "Morning");
        assert_eq!(row["category"], "Mood");
        assert_eq!(row["rating_or_amount"], "7");
        assert_eq!(row["notes"], "Feeling okay");
    }

    #[test]
    fn reimport_is_idempotent_no_duplicates() {
        let v = temp_vault("dedup");
        import(&v, EXPORT);

        let before = fs::read_to_string(v.root().join("health/bearable/2026-06.jsonl")).unwrap();
        let again = import(&v, EXPORT);
        let after = fs::read_to_string(v.root().join("health/bearable/2026-06.jsonl")).unwrap();

        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&7));
        assert_eq!(before, after, "file byte-identical after re-import");
    }

    #[test]
    fn row_with_invalid_date_is_skipped() {
        let bad = "\
date,weekday,time_of_day,category,rating_or_amount,detail,notes\n\
not-a-date,Monday,Morning,Mood,7,,\n\
2026-06-10,Wednesday,Morning,Mood,8,,\n\
";
        let v = temp_vault("baddate");
        let out = import(&v, bad);
        assert_eq!(out.counts.get("imported"), Some(&1), "valid row imported");
        assert_eq!(out.counts.get("skipped"), Some(&1), "invalid date skipped");
    }

    #[test]
    fn guid_stable_and_distinguishes_factor_slots() {
        // Helper: idx=0 for the common first-occurrence case.
        let g = |d: &str, t: &str, c: &str, det: &str, r: &str, n: &str| {
            entry_guid(d, t, c, det, r, n, 0)
        };
        let base = g("2026-06-10", "Morning", "Mood", "", "7", "");
        // Deterministic (same args → same guid).
        assert_eq!(base, g("2026-06-10", "Morning", "Mood", "", "7", ""));
        // Different time_of_day → different guid.
        assert_ne!(base, g("2026-06-10", "Evening", "Mood", "", "7", ""));
        // Different category → different guid.
        assert_ne!(base, g("2026-06-10", "Morning", "Symptom", "", "7", ""));
        // Different detail → different guid.
        assert_ne!(base, g("2026-06-10", "Morning", "Symptom", "Headache", "4", ""));
        // Different rating_or_amount → different guid (defect #1 fix).
        assert_ne!(base, g("2026-06-10", "Morning", "Mood", "", "8", ""));
        // Different notes → different guid (defect #1 fix).
        assert_ne!(base, g("2026-06-10", "Morning", "Mood", "", "7", "Felt great"));
        // Different occurrence index → different guid (byte-identical row support).
        assert_ne!(base, entry_guid("2026-06-10", "Morning", "Mood", "", "7", "", 1));
        // Expected length (10 hex chars).
        assert_eq!(base.len(), 10);
    }

    /// Fixture with two rows sharing the (date, slot, category, detail) tuple
    /// but differing in rating_or_amount — simulates the same symptom logged at
    /// two severities in the same time slot. Both must be imported and written
    /// to BOTH the parsed and raw layers (defects #1 and #2 regression test).
    const EXPORT_COLLISION: &str = "\
date,weekday,time_of_day,category,rating_or_amount,detail,notes\n\
2026-06-10,Wednesday,Morning,Symptom,3,Headache,\n\
2026-06-10,Wednesday,Morning,Symptom,5,Headache,\n\
2026-06-10,Wednesday,Morning,Medication,,Ibuprofen,first dose\n\
2026-06-10,Wednesday,Morning,Medication,,Ibuprofen,second dose\n\
";

    #[test]
    fn same_slot_different_value_rows_both_imported() {
        // Two Headache rows (severity 3 and 5) and two Ibuprofen rows (different
        // notes) must ALL be imported — the old 4-tuple guid would have collapsed
        // each pair into one row, silently losing data.
        let v = temp_vault("collision");
        let out = import(&v, EXPORT_COLLISION);
        assert_eq!(out.counts.get("imported"), Some(&4), "all 4 distinct entries imported");
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        let parsed = fs::read_to_string(v.root().join("health/bearable/2026-06.jsonl")).unwrap();
        assert_eq!(parsed.lines().count(), 4, "all 4 entries written to parsed layer");

        let rows: Vec<Value> = parsed
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        // Both Headache severities must appear.
        let headaches: Vec<_> = rows.iter().filter(|r| r["detail"] == "Headache").collect();
        assert_eq!(headaches.len(), 2, "both Headache entries present");
        let ratings: std::collections::HashSet<String> = headaches
            .iter()
            .filter_map(|r| r["rating_or_amount"].as_str().map(|s| s.to_string()))
            .collect();
        assert!(ratings.contains("3") && ratings.contains("5"), "severities 3 and 5 both present");

        // Both Ibuprofen doses must appear.
        let meds: Vec<_> = rows.iter().filter(|r| r["detail"] == "Ibuprofen").collect();
        assert_eq!(meds.len(), 2, "both Ibuprofen entries present");

        // All guids must be distinct.
        let guids: std::collections::HashSet<&str> = rows
            .iter()
            .filter_map(|r| r["guid"].as_str())
            .collect();
        assert_eq!(guids.len(), 4, "all 4 guids are distinct");
    }

    #[test]
    fn raw_layer_unconditional_includes_colliding_rows() {
        // The raw layer must contain ALL parseable rows — including rows whose
        // (slot, category, detail, value) would have collided under the old guid
        // scheme. Raw is a verbatim archive, written before any dedup gate.
        let v = temp_vault("raw-collision");
        import(&v, EXPORT_COLLISION);

        let raw = fs::read_to_string(v.root().join("health/bearable/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 4, "raw layer has all 4 verbatim rows");

        // Verify the raw verbatim values for both severity rows.
        let raw_rows: Vec<Value> =
            raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let raw_headaches: Vec<_> =
            raw_rows.iter().filter(|r| r["detail"] == "Headache").collect();
        assert_eq!(raw_headaches.len(), 2);
        let raw_ratings: std::collections::HashSet<&str> = raw_headaches
            .iter()
            .filter_map(|r| r["rating_or_amount"].as_str())
            .collect();
        assert!(raw_ratings.contains("3") && raw_ratings.contains("5"));
    }

    #[test]
    fn collision_reimport_is_idempotent() {
        // Re-importing EXPORT_COLLISION must produce 0 new entries (the same
        // occurrence indices are regenerated in the same order, yielding the
        // same guids as the first import, all of which are already seen).
        let v = temp_vault("collision-dedup");
        let first = import(&v, EXPORT_COLLISION);
        assert_eq!(first.counts.get("imported"), Some(&4));

        let before_parsed =
            fs::read_to_string(v.root().join("health/bearable/2026-06.jsonl")).unwrap();

        let second = import(&v, EXPORT_COLLISION);
        assert_eq!(second.counts.get("imported"), Some(&0), "re-import adds nothing to parsed layer");
        assert_eq!(second.counts.get("duplicates"), Some(&4));

        let after_parsed =
            fs::read_to_string(v.root().join("health/bearable/2026-06.jsonl")).unwrap();
        assert_eq!(before_parsed, after_parsed, "parsed file byte-identical after re-import");

        // Raw layer grows (verbatim archive is unconditional; re-import appends again).
        let raw = fs::read_to_string(v.root().join("health/bearable/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 8, "raw layer has 4 + 4 rows after two imports");
    }

    #[test]
    fn run_import_reads_from_a_csv_file() {
        let v = temp_vault("fileio");
        let path = v.root().join("bearable-export.csv");
        fs::write(&path, EXPORT).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&7));
    }

    #[test]
    fn hub_card_and_last_data_surface() {
        let v = temp_vault("hub");
        import(&v, EXPORT);

        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "bearable").unwrap();
        let import_info = card.import.as_ref().expect("import box info should be set");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2026-06"));
    }
}
