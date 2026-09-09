//! MacroFactor nutrition-tracking CSV import — day-level energy-balance rows
//! (calories/macros + smoothed weight trend + estimated TDEE + program targets)
//! into the bound [`crate::health_nutrition`] contract.
//! Brief: docs/integrations/macrofactor.md.
//!
//! MacroFactor exports via **Settings → Export Your Data → Granular Export**
//! (per-type CSVs) or **Quick Export** (summary). The exact column headers are
//! not publicly documented and require a real subscriber export to confirm.
//! This module ships the import plumbing (raw + contract layers, deduplication,
//! progress) against the *documented shape* from the `health-nutrition` domain
//! spec, which names the expected fields from the MacroFactor example row:
//!
//! - `ts` = date-only `YYYY-MM-DD` (MacroFactor rows are day-precision; the
//!   export carries no sub-day timestamps).
//! - `energy_kcal`, `protein_g`, `carb_g`, `fat_g` — the four core macros,
//!   possibly labeled `"Calories"`, `"Protein"`, `"Carbs"`, `"Fat"` in the CSV
//!   (alias lists below; header-matched, never positional).
//! - `extra.expenditure_kcal` — MacroFactor's estimated TDEE for the day.
//! - `extra.trend_weight_kg` — the app's smoothed weight trend for that day.
//! - `extra.target_calories` / `extra.target_protein_g` etc — program targets
//!   where the export carries them.
//!
//! **PARSER PARKED — Needs-sample:** Column headers are inferred from the
//! spec example and community reports. The raw layer is unconditional (every
//! CSV cell is preserved), and the contract layer maps via a broad alias table;
//! but the real Granular Export header names must be confirmed against a real
//! subscriber export before the parser is finalized.
//!
//! Raw layer: `health/nutrition/macrofactor/raw/YYYY-MM.jsonl` (header→value
//! objects, one per CSV row, unconditional).
//! Contract layer: `health/nutrition/macrofactor/YYYY-MM.jsonl` (normalized
//! [`crate::health_nutrition::Entry`] rows, deduped by `guid`).
//! Weight-trend / expenditure / targets rows that carry no `food` name write
//! a contract row with a date-only `ts`, no `food`, and source-specific fields
//! in `extra` — the day-rollup shape the spec describes.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::health_nutrition::Entry;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer stream; raw rows nest under `raw/`.
const DIR: &str = "health/nutrition/macrofactor";
const RAW_DIR: &str = "health/nutrition/macrofactor/raw";
const SOURCE: &str = "macrofactor";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macrofactor",
        name: "MacroFactor",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your MacroFactor CSV export — daily calorie/macro logs, \
                      smoothed weight trend, estimated expenditure (TDEE), and program \
                      targets — into the unified nutrition store. Re-runnable: duplicate \
                      days are never double-counted.",
        domain: "health-nutrition",
        vault_path: "health/nutrition/macrofactor/",
        toggleable: false,
        setup: &[
            "MacroFactor → Settings → Export Your Data → Granular Export.",
            "Import any of the exported CSV files here (calories/macros, weight trend, \
             expenditure, or targets).",
        ],
        caveats: "Requires an active MacroFactor subscription to produce the export. \
                  MacroFactor rows are day-precision (no sub-day timestamps). \
                  Note: iPhone users — the Apple Health export already carries the \
                  MacroFactor-written nutrition totals via HealthKit; this import adds \
                  the expenditure, trend-weight, and targets detail that HealthKit does \
                  not receive.",
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
// Column-alias table.
//
// MacroFactor's exact export column headers are not publicly documented and
// require confirmation against a real export (Needs-sample). The alias lists
// below cover the names inferred from the domain-spec example row, community
// reports, and common nutrition-tracker conventions. Header-matching is
// case-insensitive. The first alias that matches a real column is used; the
// raw row always carries every original header verbatim regardless.
//
// These aliases are PROVISIONAL — confirm and tighten against a real export.

/// Aliases for the date field.
const DATE_ALIASES: &[&str] = &["Date", "date", "Day", "day"];

/// Macros that earn a typed contract column. Each entry is `(aliases, setter)`.
const TYPED_MACROS: &[(&[&str], fn(&mut Entry, f64))] = &[
    (
        &["Calories", "Energy (kcal)", "Energy", "Calories (kcal)", "calories"],
        |e, v| e.energy_kcal = Some(v),
    ),
    (
        &["Protein (g)", "Protein", "protein"],
        |e, v| e.protein_g = Some(v),
    ),
    (
        &[
            "Carbohydrates (g)", "Carbs (g)", "Carbs", "Carbohydrates",
            "carbs", "carbohydrates",
        ],
        |e, v| e.carb_g = Some(v),
    ),
    (
        &["Fat (g)", "Fat", "fat"],
        |e, v| e.fat_g = Some(v),
    ),
    (
        &["Fiber (g)", "Fiber", "fiber"],
        |e, v| e.fiber_g = Some(v),
    ),
    (
        &["Sugar (g)", "Sugars (g)", "Sugars", "sugar"],
        |e, v| e.sugar_g = Some(v),
    ),
    (
        &["Saturated Fat (g)", "Saturated (g)", "Saturated Fat", "saturated_fat"],
        |e, v| e.saturated_fat_g = Some(v),
    ),
    (
        &["Sodium (mg)", "Sodium", "sodium"],
        |e, v| e.sodium_mg = Some(v),
    ),
];

/// Extra fields that belong in `entry.extra` (MacroFactor-specific metrics).
/// `(aliases, extra_key)`.
///
/// NOTE: Scale Weight (raw body-measurement) is intentionally excluded here per
/// health-nutrition.md L13-16: weight/body-measurement rows belong to `health/`,
/// not this contract. Scale weight columns remain in the raw layer verbatim.
const EXTRA_FIELDS: &[(&[&str], &str)] = &[
    (
        &[
            "Expenditure", "Expenditure (kcal)", "TDEE", "TDEE (kcal)",
            "Estimated Expenditure", "Estimated Expenditure (kcal)",
            "expenditure", "expenditure_kcal",
        ],
        "expenditure_kcal",
    ),
    (
        &[
            "Trend Weight (kg)", "Trend Weight", "Smoothed Weight (kg)",
            "Smoothed Weight", "trend_weight_kg", "trend_weight",
        ],
        "trend_weight_kg",
    ),
    (
        &[
            "Trend Weight (lbs)", "Trend Weight (lb)", "Smoothed Weight (lbs)",
            "trend_weight_lbs",
        ],
        "trend_weight_lbs",
    ),
    // scale_weight_kg / scale_weight_lbs are raw-only: routed to the verbatim
    // raw layer but never written into the nutrition contract (they are body
    // measurements, not food logs — spec excludes them from this domain).
    (
        &[
            "Target Calories", "Target Energy (kcal)", "Program Calories",
            "target_calories",
        ],
        "target_calories",
    ),
    (
        &[
            "Target Protein (g)", "Target Protein", "Program Protein (g)",
            "target_protein_g",
        ],
        "target_protein_g",
    ),
    (
        &[
            "Target Carbs (g)", "Target Carbohydrates (g)", "Target Carbs",
            "Program Carbs (g)", "target_carbs_g",
        ],
        "target_carbs_g",
    ),
    (
        &[
            "Target Fat (g)", "Program Fat (g)", "Target Fat",
            "target_fat_g",
        ],
        "target_fat_g",
    ),
];

// ---------------------------------------------------------------------------
// Raw row wrapper — the verbatim CSV row as a header→value JSON object.
// `ts` is skipped on serialisation; it exists only for the month-partition key.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Import entry point.

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

/// Testable seam — parse the in-memory CSV string and write both vault layers.
fn import_body(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load already-stored guids for idempotent re-imports.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
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
        .context("reading CSV header row — is this a MacroFactor CSV export?")?
        .clone();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut entries: Vec<Entry> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };

        // Verbatim header→value map for the raw layer (full fidelity).
        let fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();

        // Raw layer is unconditional: every well-formed CSV row lands in raw
        // regardless of whether entry_from can produce a contract row.
        // We need the ts for partitioning; fall back to "0000-00" if missing.
        let raw_ts = col_val(&fields, DATE_ALIASES)
            .and_then(|d| {
                // Accept YYYY-MM-DD dates; strip any trailing time component.
                let d = if d.len() > 10 { &d[..10] } else { d };
                NaiveDate::parse_from_str(d, "%Y-%m-%d").ok().map(|_| d.to_string())
            })
            .unwrap_or_else(|| "0000-00-00".to_string());
        raws.push(RawLine { ts: raw_ts, value: Value::Object(fields.clone()) });

        let Some(entry) = entry_from(&fields) else {
            skipped += 1;
            continue;
        };
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

    // Raw layer unconditionally first, then contract rows.
    raw.append(&raws, |r| &r.ts)?;
    contract.append(&entries, |e| &e.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!("{imported} rows imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers.

/// Find the value of the first matching alias column in `fields`.
fn col_val<'a>(fields: &'a Map<String, Value>, aliases: &[&str]) -> Option<&'a str> {
    for alias in aliases {
        if let Some(v) = fields.get(*alias).and_then(Value::as_str) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

/// Parse a numeric string; `None` for blank or non-numeric.
fn parse_num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<f64>().ok()
}

/// Build one [`Entry`] from a row's header→value map. Returns `None` when the
/// row has no parseable date (can't partition) or no numeric content at all.
fn entry_from(fields: &Map<String, Value>) -> Option<Entry> {
    // Resolve the date via alias lookup (header-matched, never positional).
    let date_str = col_val(fields, DATE_ALIASES)?;

    // Accept YYYY-MM-DD; also tolerate a trailing time component (strip it).
    let date_str = if date_str.len() > 10 { &date_str[..10] } else { date_str };
    NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
    let ts = date_str.to_string();

    // guid: stable per-day-per-export-type key.
    //
    // Using date alone caused guid collisions when multiple MacroFactor Granular
    // Export files (e.g. calories CSV + expenditure CSV) were imported
    // separately: rows from different files for the same date shared a guid and
    // the second file's rows were silently dropped as "duplicates".
    //
    // Fix: build a column-fingerprint from the sorted non-date column names that
    // are present and non-empty in this row, then hash date + fingerprint.
    // Different export types carry different columns → different fingerprints →
    // different guids → coexist correctly. Re-importing the identical file
    // produces the identical fingerprint → deduplicated correctly.
    let guid = row_guid(&ts, fields);

    let mut entry = Entry::new(SOURCE, guid, ts.clone());

    // Map typed macro columns.
    for (aliases, set) in TYPED_MACROS {
        if let Some(s) = col_val(fields, aliases) {
            if let Some(v) = parse_num(s) {
                set(&mut entry, v);
            }
        }
    }

    // Map MacroFactor-specific extras (excluding scale-weight; body-measurement
    // columns stay raw-only per the health-nutrition domain spec).
    for (aliases, key) in EXTRA_FIELDS {
        if let Some(s) = col_val(fields, aliases) {
            if let Some(v) = parse_num(s) {
                // Preserve integer fidelity: write i64 when the value has no
                // fractional part (spec example: expenditure_kcal:2640, not 2640.0).
                let json_val = if v.fract() == 0.0 && v.abs() < i64::MAX as f64 {
                    Value::Number(serde_json::Number::from(v as i64))
                } else {
                    serde_json::Number::from_f64(v)
                        .map(Value::Number)
                        .unwrap_or(Value::String(s.to_string()))
                };
                entry.extra.insert((*key).to_string(), json_val);
            }
        }
    }

    // Require at least one numeric field to have landed (skip entirely-blank rows).
    let has_any = entry.energy_kcal.is_some()
        || entry.protein_g.is_some()
        || entry.carb_g.is_some()
        || entry.fat_g.is_some()
        || !entry.extra.is_empty();
    if !has_any {
        return None;
    }

    Some(entry)
}

/// A stable per-day-per-export-type guid.
///
/// Incorporates the sorted set of non-date column names present in the row as
/// a "column fingerprint" so that different MacroFactor Granular Export files
/// (calories, weight-trend, expenditure, targets) carrying the same date but
/// different columns get distinct guids and coexist without false-dedup.
fn row_guid(date: &str, fields: &Map<String, Value>) -> String {
    // Collect non-date, non-empty column names → sort → join as fingerprint.
    let mut col_names: Vec<&str> = fields
        .iter()
        .filter(|(k, v)| {
            let is_date_col = DATE_ALIASES.iter().any(|a| a.eq_ignore_ascii_case(k));
            let is_nonempty = v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false);
            !is_date_col && is_nonempty
        })
        .map(|(k, _)| k.as_str())
        .collect();
    col_names.sort_unstable();
    let fingerprint = col_names.join(",");

    let mut h = Sha256::new();
    h.update(format!("mf|{date}|{fingerprint}").as_bytes());
    let d = h.finalize();
    format!("mf-{}", d.iter().take(4).map(|b| format!("{b:02x}")).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-macrofactor-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import(v: &Vault, body: &str) -> ImportOutcome {
        import_body(v, body, &mut |_| {}).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixture CSV rows are built from the domain-spec example for MacroFactor:
    //   {"ts":"2026-06-11","source":"macrofactor","guid":"mf-2026-06-11-total",
    //    "energy_kcal":2180,"protein_g":158,"carb_g":201,"fat_g":74,
    //    "extra":{"expenditure_kcal":2640,"trend_weight_kg":81.4}}
    //
    // Column names use the provisional aliases (real export headers are
    // Needs-sample and will be confirmed against a subscriber export).

    const CALORIES_CSV: &str = "\
Date,Calories,Protein,Carbs,Fat,Fiber,Expenditure,Trend Weight (kg)\n\
2026-06-11,2180,158,201,74,32,2640,81.4\n\
2026-06-12,2250,165,210,78,28,2670,81.2\n\
";

    const WEIGHT_ONLY_CSV: &str = "\
Date,Trend Weight (kg),Scale Weight (kg)\n\
2026-06-10,81.6,82.1\n\
2026-06-11,81.4,81.8\n\
";

    #[test]
    fn maps_calories_row_to_contract_entry() {
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "2026-06-11",
            "Calories": "2180",
            "Protein": "158",
            "Carbs": "201",
            "Fat": "74",
            "Fiber": "32",
            "Expenditure": "2640",
            "Trend Weight (kg)": "81.4"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.source, "macrofactor");
        assert_eq!(e.ts, "2026-06-11");
        assert!(e.food.is_empty(), "day-rollup has no per-food name");
        assert_eq!(e.energy_kcal, Some(2180.0));
        assert_eq!(e.protein_g, Some(158.0));
        assert_eq!(e.carb_g, Some(201.0));
        assert_eq!(e.fat_g, Some(74.0));
        assert_eq!(e.fiber_g, Some(32.0));
        // MacroFactor-specific extras land in extra with integer fidelity.
        // expenditure_kcal is a whole number → should serialize as 2640, not 2640.0
        // (matches the health-nutrition spec example row).
        assert_eq!(e.extra.get("expenditure_kcal"), Some(&serde_json::json!(2640)));
        assert_eq!(e.extra.get("trend_weight_kg"), Some(&serde_json::json!(81.4)));
    }

    #[test]
    fn weight_only_row_maps_trend_weight_not_scale_weight() {
        // Scale weight is a body-measurement column excluded from the nutrition
        // contract per health-nutrition.md: stays raw-only, never in extra.
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "2026-06-10",
            "Trend Weight (kg)": "81.6",
            "Scale Weight (kg)": "82.1"
        }))
        .unwrap();
        let e = entry_from(&fields).unwrap();
        assert_eq!(e.ts, "2026-06-10");
        assert!(e.energy_kcal.is_none(), "weight-only row carries no macros");
        // Trend weight (smoothed/modeled) IS spec-blessed in nutrition extra.
        assert_eq!(e.extra.get("trend_weight_kg"), Some(&serde_json::json!(81.6)));
        // Scale weight (raw body measurement) must NOT appear in the contract extra.
        assert!(
            e.extra.get("scale_weight_kg").is_none(),
            "scale_weight_kg must be raw-only, not in nutrition contract extra"
        );
    }

    #[test]
    fn row_without_date_is_skipped() {
        let no_date: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Calories": "2000",
            "Protein": "150"
        }))
        .unwrap();
        assert!(entry_from(&no_date).is_none(), "no date => can't partition");
    }

    #[test]
    fn row_with_no_numeric_content_is_skipped() {
        let empty: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "2026-06-11",
            "Calories": "",
            "Protein": ""
        }))
        .unwrap();
        assert!(entry_from(&empty).is_none(), "all blank numerics => skip");
    }

    #[test]
    fn guid_is_stable_per_date_and_columns() {
        let calories_fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "2026-06-11",
            "Calories": "2180",
            "Protein": "158",
        }))
        .unwrap();
        let weight_fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "2026-06-11",
            "Trend Weight (kg)": "81.4",
        }))
        .unwrap();
        let other_day: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Date": "2026-06-12",
            "Calories": "2250",
        }))
        .unwrap();

        let g_cal1 = row_guid("2026-06-11", &calories_fields);
        let g_cal2 = row_guid("2026-06-11", &calories_fields);
        let g_wt = row_guid("2026-06-11", &weight_fields);
        let g_other = row_guid("2026-06-12", &other_day);

        assert_eq!(g_cal1, g_cal2, "identical fields => same guid (re-import stable)");
        assert_ne!(g_cal1, g_wt, "same date, different columns => different guids");
        assert_ne!(g_cal1, g_other, "different dates => different guids");
        assert!(g_cal1.starts_with("mf-"), "guid has mf- prefix");
    }

    #[test]
    fn full_import_writes_both_layers_and_dedupes() {
        let v = temp_vault("fullimport");
        let out = import(&v, CALORIES_CSV);
        assert_eq!(out.counts.get("imported"), Some(&2));
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Contract rows land in the June partition.
        let jun = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(jun.lines().count(), 2, "two June rows in contract file");
        assert!(jun.contains("\"energy_kcal\":2180.0"), "energy_kcal on disk: {jun}");
        // expenditure_kcal must serialize as integer 2640 (not 2640.0) per spec.
        assert!(jun.contains("\"expenditure_kcal\":2640"), "expenditure_kcal integer: {jun}");
        assert!(jun.contains("\"trend_weight_kg\""), "trend_weight in extra: {jun}");

        // Raw layer (verbatim CSV cells).
        let raw = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/raw/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(raw.lines().count(), 2, "two raw rows");
        assert!(raw.contains("\"Calories\":\"2180\""), "raw keeps verbatim value: {raw}");
        assert!(
            raw.contains("\"Trend Weight (kg)\":\"81.4\""),
            "raw header verbatim: {raw}"
        );

        // Re-import the same export => pure duplicates.
        let again = import(&v, CALORIES_CSV);
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&2));
        let jun2 = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(jun, jun2, "contract file unchanged after re-import");
    }

    #[test]
    fn weight_only_csv_writes_trend_weight_not_scale_weight() {
        // Trend weight lands in contract extra; scale weight is raw-only per spec.
        let v = temp_vault("weightonly");
        let out = import(&v, WEIGHT_ONLY_CSV);
        assert_eq!(out.counts.get("imported"), Some(&2));
        let jun = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/2026-06.jsonl"),
        )
        .unwrap();
        assert!(jun.contains("\"trend_weight_kg\""), "trend weight in extra: {jun}");
        assert!(
            !jun.contains("\"scale_weight_kg\""),
            "scale_weight must not appear in nutrition contract: {jun}"
        );
        assert!(!jun.contains("\"energy_kcal\""), "no macros in weight-only rows: {jun}");

        // Raw layer still preserves the Scale Weight column verbatim.
        let raw = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/raw/2026-06.jsonl"),
        )
        .unwrap();
        assert!(
            raw.contains("\"Scale Weight (kg)\""),
            "scale weight must be in raw layer: {raw}"
        );
    }

    #[test]
    fn multi_file_import_coexists_no_guid_collision() {
        // Importing a calories CSV then a weight-trend CSV for the same date
        // must NOT drop the second file's rows as "duplicates". Each export type
        // has a distinct column fingerprint → distinct guid → both land in vault.
        const EXPENDITURE_CSV: &str = "\
Date,Expenditure\n\
2026-06-11,2640\n\
2026-06-12,2670\n\
";
        let v = temp_vault("multifile");

        // First import: calories
        let out1 = import(&v, CALORIES_CSV);
        assert_eq!(out1.counts.get("imported"), Some(&2), "calories import");

        // Second import: expenditure-only CSV for the same dates
        let out2 = import(&v, EXPENDITURE_CSV);
        assert_eq!(
            out2.counts.get("imported"), Some(&2),
            "expenditure rows must not be dropped as duplicates of calories rows"
        );
        assert_eq!(out2.counts.get("duplicates"), Some(&0));

        // Four total rows in the contract file.
        let jun = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(jun.lines().count(), 4, "all four rows coexist: {jun}");

        // Re-importing the expenditure CSV => pure duplicates (same fingerprint).
        let out3 = import(&v, EXPENDITURE_CSV);
        assert_eq!(out3.counts.get("imported"), Some(&0));
        assert_eq!(out3.counts.get("duplicates"), Some(&2));
    }

    #[test]
    fn raw_layer_is_unconditional() {
        // Rows that fail entry_from (e.g. no numeric content) must still land in
        // the raw layer — raw fidelity is unconditional per module spec.
        const MIXED_CSV: &str = "\
Date,Calories,Note\n\
2026-06-11,2180,good day\n\
2026-06-12,,rest day\n\
";
        let v = temp_vault("rawunconditional");
        let out = import(&v, MIXED_CSV);
        // Only the row with numeric content produces a contract entry.
        assert_eq!(out.counts.get("imported"), Some(&1));
        assert_eq!(out.counts.get("skipped"), Some(&1));

        // But BOTH rows must appear in the raw layer.
        let raw = fs::read_to_string(
            v.root().join("health/nutrition/macrofactor/raw/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(raw.lines().count(), 2, "raw layer must have both rows: {raw}");
        assert!(raw.contains("\"Note\":\"rest day\""), "blank-calorie row in raw: {raw}");
    }

    #[test]
    fn import_registers_source_on_manifest() {
        let v = temp_vault("manifest");
        import(&v, CALORIES_CSV);
        let m = v.rebuild_manifest().unwrap();
        let dom = m.domains.iter().find(|d| d.domain == "health-nutrition").unwrap();
        assert!(dom.sources.contains(&"macrofactor".to_string()));
        assert_eq!(dom.first.as_deref(), Some("2026-06"));
    }

    #[test]
    fn import_surfaces_on_hub_status() {
        let v = temp_vault("hubstatus");
        import(&v, CALORIES_CSV);
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "macrofactor").unwrap();
        let import_info = card.import.as_ref().expect("import box should be present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2026-06"));
    }

    #[test]
    fn run_import_reads_from_csv_file() {
        let v = temp_vault("fileio");
        let path = v.root().join("macrofactor.csv");
        fs::write(&path, CALORIES_CSV).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&2));
    }
}
