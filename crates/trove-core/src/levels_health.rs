//! Levels — metabolic-health CGM app with CSV export.
//! Brief: docs/integrations/levels-health.md
//!
//! Levels pairs a continuous glucose monitor with a food diary and computes
//! proprietary **Zones** scores (a glucose-response rating per meal). The CGM
//! stream overlaps Dexcom/Abbott (prefer those for the raw EGV feed); Levels is
//! imported for its **unique** food↔glucose correlation layer.
//!
//! **Export path.** The user exports four CSV files from
//! `support.levels.com/article/105-export` (Glucose, Food/Activity/Notes,
//! Zones, Nutrition). Trove never holds Levels credentials. No public API exists.
//!
//! **WARNING — PARSER PARKED.** The exact CSV column headers are not publicly
//! documented and no real export sample is available. The scaffolding below routes
//! by file name (Levels export filenames are well-known) and parses by
//! column-name matching (never positional), but the column-name tables inside are
//! **best-effort guesses based on the support article description**.
//!
//! Two contract bindings:
//! - Food log rows (with Zones scores) → [`crate::health_nutrition::Entry`]
//!   under `health/nutrition/levels-health/YYYY-MM.jsonl`.
//!   Zone score rides in `extra.zone_score`.
//! - Glucose/CGM rows → [`crate::health_medical::Observation`]
//!   under `health/medical/levels-health/observations/YYYY-MM.jsonl`.
//!   (Secondary to Dexcom; still imported for the food-glucose correlation.)
//! - Activity, biometrics, raw Zones → raw-only under `health/levels/raw/`.
//!
//! Two raw layers (unconditional):
//! - `health/nutrition/levels-health/raw/YYYY-MM.jsonl` — verbatim food-log rows.
//! - `health/medical/levels-health/raw/YYYY-MM.jsonl` — verbatim glucose rows.
//! - `health/levels-health/raw/<filename>.jsonl` — all other CSVs verbatim.
//!
//! **Flag: Needs-sample.** Once a real export is available, verify the column
//! names in GLUCOSE_TS_COLS, GLUCOSE_VALUE_COL, FOOD_TIMESTAMP_COLS,
//! FOOD_NAME_COL, ZONES_SCORE_COL, etc. and remove this warning.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::health_medical::Observation;
use crate::health_nutrition::Entry;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths.

/// Contract nutrition entries (food log + Zones).
const FOOD_DIR: &str = "health/nutrition/levels-health";
/// Raw food-log CSV rows.
const FOOD_RAW_DIR: &str = "health/nutrition/levels-health/raw";

/// Contract glucose observations.
const GLUCOSE_DIR: &str = "health/medical/levels-health/observations";
/// Raw glucose CSV rows.
const GLUCOSE_RAW_DIR: &str = "health/medical/levels-health/raw";

/// Raw-only for everything else (Zones, Activity, Biometrics).
const OTHER_RAW_DIR: &str = "health/levels-health/raw";

const SOURCE: &str = "levels-health";
const GLUCOSE_LOINC: &str = "2339-0"; // Glucose [Mass/volume] in Blood

// ---------------------------------------------------------------------------
// Column-name tables.
//
// **ALL OF THESE ARE BEST-EFFORT GUESSES** — they are based on the support
// article's description, the standard CGM export conventions (Dexcom/Libre
// style), and a canonical nutrition-log layout. A real export sample will
// correct any that are wrong. Column matching is always by header-name, never
// positional, so tolerated unknowns simply fall into raw.
//
// Glucose CSV — likely columns based on CGM conventions.
/// Candidate timestamp column names (in order of preference).
const GLUCOSE_TS_COLS: &[&str] = &["Timestamp (UTC)", "Timestamp", "Time", "Date Time"];
/// Candidate glucose-value column names.
const GLUCOSE_VALUE_COLS: &[&str] = &["Glucose Value (mg/dL)", "Glucose (mg/dL)", "Value", "mg/dL"];

// Food/nutrition CSV — columns based on the support article + standard
// nutrition-log conventions (similar to Cronometer/MFP).
/// Candidate timestamp column names for food-log rows.
const FOOD_TS_COLS: &[&str] = &["Logged At", "Timestamp", "Time", "Date Time", "Eaten At"];
/// Candidate food-name column names.
const FOOD_NAME_COLS: &[&str] = &["Food", "Food Name", "Item", "Name"];
/// Candidate meal-slot column names.
const FOOD_MEAL_COLS: &[&str] = &["Meal", "Meal Name", "Meal Type", "Group"];
/// Candidate Zones score column name.
const ZONES_SCORE_COLS: &[&str] = &["Zone Score", "Zones Score", "Score", "Metabolic Score"];

// Macronutrient typed columns (label → setter), guessed from standard export
// conventions. All amounts in grams/mg as indicated.
const TYPED_MACROS: &[(&str, fn(&mut Entry, f64))] = &[
    ("Energy (kcal)", |e, v| e.energy_kcal = Some(v)),
    ("Calories", |e, v| e.energy_kcal = Some(v)),
    ("Protein (g)", |e, v| e.protein_g = Some(v)),
    ("Carbs (g)", |e, v| e.carb_g = Some(v)),
    ("Carbohydrates (g)", |e, v| e.carb_g = Some(v)),
    ("Fat (g)", |e, v| e.fat_g = Some(v)),
    ("Total Fat (g)", |e, v| e.fat_g = Some(v)),
    ("Fiber (g)", |e, v| e.fiber_g = Some(v)),
    ("Dietary Fiber (g)", |e, v| e.fiber_g = Some(v)),
    ("Sugars (g)", |e, v| e.sugar_g = Some(v)),
    ("Total Sugars (g)", |e, v| e.sugar_g = Some(v)),
    ("Saturated Fat (g)", |e, v| e.saturated_fat_g = Some(v)),
    ("Sodium (mg)", |e, v| e.sodium_mg = Some(v)),
    ("Cholesterol (mg)", |e, v| e.cholesterol_mg = Some(v)),
];

// Core (non-nutrient) columns that are consumed individually and should NOT
// land in the nutrients map.
const FOOD_CORE_COLS: &[&str] = &[
    "Logged At", "Timestamp", "Time", "Date Time", "Eaten At",
    "Food", "Food Name", "Item", "Name",
    "Meal", "Meal Name", "Meal Type", "Group",
    "Zone Score", "Zones Score", "Score", "Metabolic Score",
    "Amount", "Quantity", "Serving Size",
    "Unit", "Serving Unit",
];

// ---------------------------------------------------------------------------
// Raw row shape (verbatim CSV row tagged with a `ts` for the partition writer).

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(FOOD_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(GLUCOSE_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "levels-health",
        name: "Levels",
        kind: IntegrationKind::Import,
        // CGM + food logs are medical/dietary data — opt-in with explicit
        // acknowledgement.
        default_on: false,
        description: "Import your Levels metabolic-health export — food logs with \
                      proprietary Zones glucose-response scores, continuous glucose \
                      readings, and activity data — into the unified nutrition and \
                      medical stores.",
        domain: "health-nutrition",
        vault_path: "health/nutrition/levels-health/",
        toggleable: false,
        setup: &[
            "Continuous glucose and food logs are sensitive health data — enabling this opts \
             you in to collecting them.",
            "From the Levels member portal: Health Data → Glucose Dashboard → Export Data.",
            "Download the CSV set (Glucose, Nutrition, Zones, Activity) and import each here.",
        ],
        caveats: "Requires an active Levels subscription ($200+/yr). Raw glucose is better \
                  sourced from Dexcom or Abbott directly; Levels is imported for the unique \
                  food-glucose Zones correlation layer. CSV column names are unverified — a \
                  real export sample is needed to confirm the parser (Needs-sample flag).",
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
// Routing: the file name tells us which CSV type this is.

#[derive(Debug, Clone, Copy, PartialEq)]
enum CsvKind {
    Glucose,
    Food,
    Zones,
    Activity,
    Biometrics,
    Unknown,
}

/// Infer the CSV kind from the file name (Levels export filenames are
/// consistent with the support article's four categories). Falls back to
/// column-header sniffing when the name is ambiguous.
fn csv_kind_from_filename(name: &str) -> CsvKind {
    let lower = name.to_ascii_lowercase();
    if lower.contains("glucose") || lower.contains("cgm") || lower.contains("egv") {
        CsvKind::Glucose
    } else if lower.contains("nutrition") || lower.contains("food_log") || lower.contains("food-log") {
        CsvKind::Food
    } else if lower.contains("zone") {
        CsvKind::Zones
    } else if lower.contains("activity") {
        CsvKind::Activity
    } else if lower.contains("biometric") || lower.contains("weight") {
        CsvKind::Biometrics
    } else {
        CsvKind::Unknown
    }
}

/// Refine the kind against the header row when the filename didn't resolve it.
fn csv_kind_from_headers(headers: &csv::StringRecord) -> CsvKind {
    let hset: Vec<&str> = headers.iter().collect();
    let has = |col: &str| hset.iter().any(|h| h.eq_ignore_ascii_case(col));
    if GLUCOSE_VALUE_COLS.iter().any(|c| has(c)) {
        return CsvKind::Glucose;
    }
    if FOOD_NAME_COLS.iter().any(|c| has(c)) {
        return CsvKind::Food;
    }
    if ZONES_SCORE_COLS.iter().any(|c| has(c)) {
        return CsvKind::Zones;
    }
    CsvKind::Unknown
}

// ---------------------------------------------------------------------------
// The import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    import_body(vault, &body, &filename, progress)
}

/// The testable seam: drives the full import from an in-memory CSV string.
pub(crate) fn import_body(
    vault: &Vault,
    body: &str,
    filename: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Sniff the header row to resolve the kind.
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .context("reading CSV header row — is this a Levels export CSV?")?
        .clone();

    let kind = {
        let from_name = csv_kind_from_filename(filename);
        if from_name == CsvKind::Unknown {
            csv_kind_from_headers(&headers)
        } else {
            from_name
        }
    };

    match kind {
        CsvKind::Glucose => import_glucose(vault, body, progress),
        CsvKind::Food => import_food(vault, body, progress),
        // Zones, Activity, Biometrics, Unknown → raw-only (full fidelity)
        _ => import_raw_only(vault, body, filename, progress),
    }
}

// ---------------------------------------------------------------------------
// Glucose import → health_medical::Observation + raw.

fn import_glucose(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract = vault.stream(GLUCOSE_DIR, Partition::Month);
    let raw = vault.stream(GLUCOSE_RAW_DIR, Partition::Month);

    // Existing guids for idempotency (re-import dedupes by guid).
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
    let headers = rdr.headers().context("glucose CSV header")?.clone();

    // Resolve the actual timestamp and value column names (whichever of the
    // candidates appear in this file's header).
    let ts_col = resolve_col_name(&headers, GLUCOSE_TS_COLS);
    let val_col = resolve_col_name(&headers, GLUCOSE_VALUE_COLS);

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut obs_batch: Vec<Observation> = Vec::new();
    let mut raw_batch: Vec<RawLine> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        let fields = row_to_map(&headers, &rec);
        let Some(obs) = glucose_observation(&fields, ts_col, val_col) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(obs.guid.clone()) {
            duplicates += 1;
            continue;
        }
        raw_batch.push(RawLine { ts: obs.ts.clone(), value: Value::Object(fields) });
        obs_batch.push(obs);
        imported += 1;
        if rows % 500 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    raw.append(&raw_batch, |r| &r.ts)?;
    contract.append(&obs_batch, |o| &o.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} glucose readings imported, {duplicates} duplicates skipped"),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)].into(),
    })
}

/// One glucose CSV row → a contract [`Observation`], or `None` when the row
/// can't be parsed (no timestamp or no usable value column).
/// `ts_col` / `val_col` are the *exact* header names (already resolved from
/// the candidate lists), so the lookup into the row Map is a simple key get.
fn glucose_observation(
    fields: &Map<String, Value>,
    ts_col: Option<&str>,
    val_col: Option<&str>,
) -> Option<Observation> {
    // Timestamp (required).
    let ts_raw = ts_col
        .and_then(|c| fields.get(c))
        .or_else(|| GLUCOSE_TS_COLS.iter().find_map(|c| fields.get(*c)))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)?;
    let ts = parse_any_ts(&ts_raw)?;

    // Glucose value (optional — keep the row even when absent; the raw layer
    // always has full fidelity).
    let val_raw: Option<String> = val_col
        .and_then(|c| fields.get(c))
        .or_else(|| GLUCOSE_VALUE_COLS.iter().find_map(|c| fields.get(*c)))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let value: Option<f64> = val_raw.as_deref().and_then(|s| s.parse().ok());

    // guid: stable content hash of (ts_raw + val_raw) since Levels CGM
    // export has no row id. Using ts_raw so two rows at the same instant but
    // different values stay distinct.
    let guid = content_guid(&[&ts_raw, val_raw.as_deref().unwrap_or("")]);

    let mut obs = Observation::new(SOURCE, guid, ts, "Glucose");
    obs.code = GLUCOSE_LOINC.into();
    obs.code_system = "loinc".into();
    obs.value = value;
    obs.unit = "mg/dL".into();

    // Remaining columns → extra (source-specific signal: device id, trend, etc.).
    let skip_ts: HashSet<&str> = GLUCOSE_TS_COLS.iter().copied().collect();
    let skip_val: HashSet<&str> = GLUCOSE_VALUE_COLS.iter().copied().collect();
    for (k, v) in fields {
        if skip_ts.contains(k.as_str()) || skip_val.contains(k.as_str()) {
            continue;
        }
        let s = v.as_str().unwrap_or("").trim();
        if !s.is_empty() {
            obs.extra.insert(k.clone(), Value::String(s.to_string()));
        }
    }

    Some(obs)
}

// ---------------------------------------------------------------------------
// Food log import → health_nutrition::Entry + raw.

fn import_food(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract = vault.stream(FOOD_DIR, Partition::Month);
    let raw = vault.stream(FOOD_RAW_DIR, Partition::Month);

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
    let headers = rdr.headers().context("food CSV header")?.clone();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut entry_batch: Vec<Entry> = Vec::new();
    let mut raw_batch: Vec<RawLine> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        let fields = row_to_map(&headers, &rec);
        let Some(entry) = food_entry(&fields) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(entry.guid.clone()) {
            duplicates += 1;
            continue;
        }
        raw_batch.push(RawLine { ts: entry.ts.clone(), value: Value::Object(fields) });
        entry_batch.push(entry);
        imported += 1;
        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    raw.append(&raw_batch, |r| &r.ts)?;
    contract.append(&entry_batch, |e| &e.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} food entries imported, {duplicates} duplicates skipped"),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)].into(),
    })
}

/// One food-log CSV row → a contract [`Entry`], or `None` when the row can't
/// be parsed (no timestamp or no food name).
fn food_entry(fields: &Map<String, Value>) -> Option<Entry> {
    // Timestamp (required).
    let ts_raw = FOOD_TS_COLS.iter()
        .find_map(|c| fields.get(*c).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))?;
    let ts = parse_any_ts(&ts_raw)?;

    // Food name (required — the primary key of a food-log row).
    let food = FOOD_NAME_COLS.iter()
        .find_map(|c| fields.get(*c).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))?;

    // guid: stable hash of (ts_raw + food). No row id in CSV exports.
    let guid = content_guid(&[&ts_raw, &food]);

    let mut entry = Entry::new(SOURCE, guid, ts);
    entry.food = food.clone();

    // Meal slot.
    if let Some(meal_raw) = FOOD_MEAL_COLS.iter()
        .find_map(|c| fields.get(*c).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
    {
        if let Some(slot) = meal_slot(&meal_raw) {
            entry.meal = slot.to_string();
        }
        entry.extra.insert("meal_raw".into(), Value::String(meal_raw));
    }

    // Zones score → extra.zone_score.
    if let Some(score_str) = ZONES_SCORE_COLS.iter()
        .find_map(|c| fields.get(*c).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string))
    {
        if let Ok(f) = score_str.parse::<f64>() {
            entry.extra.insert("zone_score".into(), Value::from(f));
        } else if !score_str.is_empty() {
            entry.extra.insert("zone_score".into(), Value::String(score_str));
        }
    }

    // Amount / serving size.
    for amount_col in &["Amount", "Quantity", "Serving Size"] {
        if let Some(raw) = fields.get(*amount_col).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            let (qty, unit) = split_amount(raw);
            entry.amount = qty;
            entry.unit = unit;
            entry.extra.insert("amount_raw".into(), Value::String(raw.to_string()));
            break;
        }
    }

    // Typed macros.
    for (header, set) in TYPED_MACROS {
        if let Some(v) = fields.get(*header).and_then(Value::as_str).and_then(|s| s.trim().parse::<f64>().ok()) {
            set(&mut entry, v);
        }
    }

    // Everything not already consumed → nutrients (non-core non-macro numeric
    // columns carry micronutrients).
    let core: HashSet<&str> = FOOD_CORE_COLS.iter().copied()
        .chain(TYPED_MACROS.iter().map(|(h, _)| *h))
        .collect();
    for (header, raw_val) in fields {
        if core.contains(header.as_str()) {
            continue;
        }
        if let Some(n) = raw_val.as_str().and_then(|s| num_value(s.trim())) {
            entry.nutrients.insert(header.clone(), n);
        }
    }

    Some(entry)
}

// ---------------------------------------------------------------------------
// Raw-only: Zones, Activity, Biometrics, and anything unrecognized.

fn import_raw_only(
    vault: &Vault,
    body: &str,
    filename: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Derive a stable partition-safe stem from the filename.
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    // Raw rows share the generic OTHER_RAW_DIR, partitioned by stem-prefixed key.
    // Since these have no guaranteed date column we use the full file as one blob
    // under a YYYY-MM-style key derived from today (or the first row's date).
    let raw = vault.stream(OTHER_RAW_DIR, Partition::Month);

    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr.headers().context("raw CSV header")?.clone();

    let (mut rows, mut skipped) = (0u64, 0u64);
    let mut raw_batch: Vec<RawLine> = Vec::new();
    let today_key = chrono::Local::now().format("%Y-%m-%d").to_string();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        let fields = row_to_map(&headers, &rec);
        // Use the first column that looks like a date/timestamp, else today.
        let ts = fields.values()
            .filter_map(|v| v.as_str())
            .find_map(|s| parse_any_ts(s))
            .unwrap_or_else(|| today_key.clone());
        raw_batch.push(RawLine { ts, value: Value::Object(fields) });
        if rows % 500 == 0 {
            progress(ImportProgress { records: rows, percent: 0.0 });
        }
    }

    raw.append(&raw_batch, |r| &r.ts)?;
    progress(ImportProgress { records: rows, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{} rows ({stem}) stored verbatim (raw-only)",
            rows - skipped
        ),
        counts: [("rows", rows - skipped), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers.

/// Return the first header name (from `candidates`) that actually exists in
/// the CSV header row (case-insensitive match). Returns the *canonical* name
/// from `headers` (not the candidate string) so `Map::get` finds it exactly.
fn resolve_col_name<'h>(
    headers: &'h csv::StringRecord,
    candidates: &[&str],
) -> Option<&'h str> {
    candidates.iter().find_map(|cand| {
        headers.iter().find(|h| h.trim().eq_ignore_ascii_case(cand))
    })
}

/// A CSV record → a header→value JSON object. Values are verbatim strings.
fn row_to_map(headers: &csv::StringRecord, rec: &csv::StringRecord) -> Map<String, Value> {
    headers
        .iter()
        .zip(rec.iter())
        .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
        .collect()
}

/// Parse a timestamp string in any of the formats Levels might use:
/// - RFC3339 / ISO 8601 with offset (`2026-06-10T17:00:00Z`, `+07:00`)
/// - Naive datetime (`2026-06-10T17:00:00`, `2026-06-10 17:00:00`)
/// - Date-only (`2026-06-10`, `06/10/2026`, `06/10/2026 10:00:00 AM`)
///
/// Returns an RFC3339 local-offset timestamp, or a date-only `YYYY-MM-DD`
/// when the source has no time.
fn parse_any_ts(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // RFC3339 / ISO 8601 with offset.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // Naive ISO datetime, space or T separator, optional seconds.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
                return Some(dt.to_rfc3339());
            }
        }
    }
    // US-locale datetime with 12-hour clock (common in iOS/Mac app exports).
    for fmt in [
        "%m/%d/%Y %I:%M:%S %p",
        "%m/%d/%Y %I:%M %p",
        "%m/%d/%y %I:%M:%S %p",
        "%m/%d/%y %I:%M %p",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            if let Some(dt) = Local.from_local_datetime(&naive).earliest() {
                return Some(dt.to_rfc3339());
            }
        }
    }
    // Date-only forms — return as YYYY-MM-DD.
    for fmt in ["%Y-%m-%d", "%m/%d/%Y", "%m/%d/%y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }
    None
}

/// Map a meal-group string to the contract closed enum, or `None` for custom.
fn meal_slot(group: &str) -> Option<&'static str> {
    match group.trim().to_ascii_lowercase().as_str() {
        "breakfast" => Some("breakfast"),
        "lunch" => Some("lunch"),
        "dinner" => Some("dinner"),
        "snack" | "snacks" => Some("snack"),
        _ => None,
    }
}

/// Split a combined `Amount` cell (`"80.00 g"`, `"1 serving"`) into a numeric
/// quantity and a unit string.
fn split_amount(s: &str) -> (Option<f64>, String) {
    let mut parts = s.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let unit = parts.next().unwrap_or("").trim().to_string();
    (head.parse::<f64>().ok(), unit)
}

/// Numeric cell string → a JSON number, or `None` for blank/non-numeric.
fn num_value(s: &str) -> Option<Value> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(i) = s.parse::<i64>() {
        return Some(Value::from(i));
    }
    s.parse::<f64>().ok().and_then(|f| {
        serde_json::Number::from_f64(f).map(Value::Number)
    })
}

/// A stable 10-hex-char SHA-256 digest over the join of the given parts —
/// the dedupe key for CSV rows that carry no native row id.
fn content_guid(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(parts.join("\u{1f}").as_bytes());
    let digest = h.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-levels-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn no_progress(_: ImportProgress) {}

    // -------------------------------------------------------------------------
    // Fixtures.
    //
    // IMPORTANT: These use GUESSED column names. They will need verification
    // against a real Levels export. The tests prove that the routing, parsing,
    // deduplication, and vault-write logic all work; they do NOT validate the
    // actual field names against a real Levels export (Needs-sample flag).
    // -------------------------------------------------------------------------

    /// A plausible glucose CSV using Dexcom-style CGM column names (best-guess).
    const GLUCOSE_CSV: &str = "\
Timestamp (UTC),Glucose Value (mg/dL),Status
2026-06-10T17:00:00Z,112,
2026-06-10T17:05:00Z,108,
2026-06-10T17:10:00Z,95,
";

    /// A plausible food-log CSV with Zones score (best-guess column names).
    const FOOD_CSV: &str = "\
Logged At,Food,Meal,Amount,Energy (kcal),Protein (g),Carbs (g),Fat (g),Zone Score
2026-06-10T08:14:00-07:00,Oatmeal,Breakfast,80.00 g,311,10.7,54.8,5.3,7.2
2026-06-10T13:02:00-07:00,Chicken Bowl,Lunch,1.00 serving,625,42,58,22,6.8
2026-06-11T08:00:00-07:00,Banana,Breakfast,1.00 medium,105,1.3,27,0.4,5.0
";

    /// A Zones CSV (raw-only — we don't know the shape; route to raw).
    const ZONES_CSV: &str = "\
Date,Meal Name,Zone Score,Glucose Response
2026-06-10,Oatmeal,7.2,Stable
2026-06-10,Chicken Bowl,6.8,Mild rise
";

    // -------------------------------------------------------------------------
    // Routing tests.
    // -------------------------------------------------------------------------

    #[test]
    fn filename_routing_works_for_well_known_names() {
        assert_eq!(csv_kind_from_filename("glucose_2026.csv"), CsvKind::Glucose);
        assert_eq!(csv_kind_from_filename("Nutrition_Log.csv"), CsvKind::Food);
        assert_eq!(csv_kind_from_filename("zones_export.csv"), CsvKind::Zones);
        assert_eq!(csv_kind_from_filename("activity.csv"), CsvKind::Activity);
        assert_eq!(csv_kind_from_filename("biometrics_2026.csv"), CsvKind::Biometrics);
        assert_eq!(csv_kind_from_filename("something_else.csv"), CsvKind::Unknown);
    }

    #[test]
    fn header_routing_resolves_ambiguous_filenames() {
        let mut rdr = csv::Reader::from_reader(GLUCOSE_CSV.as_bytes());
        let h = rdr.headers().unwrap().clone();
        assert_eq!(csv_kind_from_headers(&h), CsvKind::Glucose);

        let mut rdr = csv::Reader::from_reader(FOOD_CSV.as_bytes());
        let h = rdr.headers().unwrap().clone();
        assert_eq!(csv_kind_from_headers(&h), CsvKind::Food);
    }

    // -------------------------------------------------------------------------
    // Timestamp parsing tests.
    // -------------------------------------------------------------------------

    #[test]
    fn parse_ts_handles_iso_with_offset_naive_and_date_only() {
        // RFC3339 with offset.
        assert!(parse_any_ts("2026-06-10T17:00:00Z").is_some());
        assert!(parse_any_ts("2026-06-10T08:14:00-07:00").is_some());
        // Naive ISO.
        assert!(parse_any_ts("2026-06-10T17:05:00").is_some());
        assert!(parse_any_ts("2026-06-10 17:05:00").is_some());
        // Date-only → YYYY-MM-DD.
        let d = parse_any_ts("2026-06-11").unwrap();
        assert_eq!(d, "2026-06-11", "date-only returns YYYY-MM-DD: {d}");
        // US locale.
        assert!(parse_any_ts("06/10/2026 08:14 AM").is_some());
        // Invalid.
        assert!(parse_any_ts("").is_none());
        assert!(parse_any_ts("not-a-date").is_none());
    }

    // -------------------------------------------------------------------------
    // Glucose import tests.
    // -------------------------------------------------------------------------

    #[test]
    fn glucose_import_writes_contract_and_raw_layers() {
        let v = temp_vault("glucose");
        let out = import_body(&v, GLUCOSE_CSV, "glucose_export.csv", &mut no_progress).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3));
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Contract observations partitioned by month.
        let obs_path = v.root().join("health/medical/levels-health/observations/2026-06.jsonl");
        let obs = std::fs::read_to_string(&obs_path).unwrap();
        assert_eq!(obs.lines().count(), 3, "three observations on disk");
        assert!(obs.contains("\"test\":\"Glucose\""));
        assert!(obs.contains("\"code\":\"2339-0\""), "LOINC on disk");
        assert!(obs.contains("\"unit\":\"mg/dL\""));
        assert!(obs.contains("\"value\":112"));

        // Raw layer.
        let raw_path = v.root().join("health/medical/levels-health/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw layer written");
        let raw = std::fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw.lines().count(), 3);
    }

    #[test]
    fn glucose_import_is_idempotent() {
        let v = temp_vault("glucose_idem");
        import_body(&v, GLUCOSE_CSV, "glucose.csv", &mut no_progress).unwrap();
        let again = import_body(&v, GLUCOSE_CSV, "glucose.csv", &mut no_progress).unwrap();
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&3));
    }

    // -------------------------------------------------------------------------
    // Food log import tests.
    // -------------------------------------------------------------------------

    #[test]
    fn food_import_writes_contract_and_raw_layers_with_zone_score() {
        let v = temp_vault("food");
        let out = import_body(&v, FOOD_CSV, "nutrition_export.csv", &mut no_progress).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&3));

        let food_path = v.root().join("health/nutrition/levels-health/2026-06.jsonl");
        let food = std::fs::read_to_string(&food_path).unwrap();
        assert_eq!(food.lines().count(), 3);
        assert!(food.contains("\"source\":\"levels-health\""));
        // Typed macros.
        assert!(food.contains("\"energy_kcal\":311"));
        assert!(food.contains("\"protein_g\":10.7"));
        // Zones score rides in extra.
        assert!(food.contains("\"zone_score\":7.2"), "zone score in extra: {food}");
        assert!(food.contains("\"zone_score\":6.8"));

        // Raw layer.
        let raw_path = v.root().join("health/nutrition/levels-health/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw layer written");
    }

    #[test]
    fn food_import_is_idempotent() {
        let v = temp_vault("food_idem");
        import_body(&v, FOOD_CSV, "nutrition.csv", &mut no_progress).unwrap();
        let again = import_body(&v, FOOD_CSV, "nutrition.csv", &mut no_progress).unwrap();
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&3));
    }

    #[test]
    fn food_entry_maps_meal_and_zone_score() {
        let fields: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Logged At": "2026-06-10T08:14:00-07:00",
            "Food": "Oatmeal",
            "Meal": "Breakfast",
            "Amount": "80.00 g",
            "Energy (kcal)": "311",
            "Protein (g)": "10.7",
            "Zone Score": "7.2"
        }))
        .unwrap();
        let e = food_entry(&fields).unwrap();
        assert_eq!(e.source, "levels-health");
        assert_eq!(e.food, "Oatmeal");
        assert_eq!(e.meal, "breakfast", "meal slot normalized");
        assert_eq!(e.energy_kcal, Some(311.0));
        assert_eq!(e.protein_g, Some(10.7));
        assert_eq!(e.amount, Some(80.0));
        assert_eq!(e.unit, "g");
        assert_eq!(e.extra.get("zone_score"), Some(&serde_json::json!(7.2)));
    }

    #[test]
    fn rows_without_food_or_ts_are_skipped() {
        let no_food: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Logged At": "2026-06-10T08:14:00Z",
            "Food": ""
        }))
        .unwrap();
        assert!(food_entry(&no_food).is_none(), "empty food name → skipped");

        let no_ts: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "Food": "Oatmeal"
        }))
        .unwrap();
        assert!(food_entry(&no_ts).is_none(), "missing ts → skipped");
    }

    // -------------------------------------------------------------------------
    // Raw-only routing tests (Zones / Activity / Biometrics).
    // -------------------------------------------------------------------------

    #[test]
    fn zones_csv_routes_to_raw_only() {
        let v = temp_vault("zones");
        let out = import_body(&v, ZONES_CSV, "zones_export.csv", &mut no_progress).unwrap();
        assert_eq!(out.counts.get("rows"), Some(&2));
        // Nothing written to the contract paths.
        assert!(!v.root().join("health/nutrition/levels-health").exists(), "no nutrition contract");
        assert!(!v.root().join("health/medical/levels-health").exists(), "no medical contract");
        // Raw layer written.
        let raw_dir = v.root().join("health/levels-health/raw");
        assert!(raw_dir.exists(), "raw-only dir created");
    }

    // -------------------------------------------------------------------------
    // Helper unit tests.
    // -------------------------------------------------------------------------

    #[test]
    fn content_guid_is_stable_and_distinct() {
        let g = content_guid(&["2026-06-10T17:00:00Z", "112"]);
        assert_eq!(g, content_guid(&["2026-06-10T17:00:00Z", "112"]));
        assert_ne!(g, content_guid(&["2026-06-10T17:00:00Z", "113"]));
        assert_eq!(g.len(), 10);
    }

    #[test]
    fn split_amount_handles_common_formats() {
        assert_eq!(split_amount("80.00 g"), (Some(80.0), "g".into()));
        assert_eq!(split_amount("1.00 serving"), (Some(1.0), "serving".into()));
        assert_eq!(split_amount("100"), (Some(100.0), "".into()));
    }

    #[test]
    fn meal_slot_maps_standard_names_and_returns_none_for_custom() {
        assert_eq!(meal_slot("Breakfast"), Some("breakfast"));
        assert_eq!(meal_slot("LUNCH"), Some("lunch"));
        assert_eq!(meal_slot("Snacks"), Some("snack"));
        assert_eq!(meal_slot("Pre-Workout"), None);
        assert_eq!(meal_slot(""), None);
    }

    #[test]
    fn def_is_import_with_csv_type() {
        match DEF.behavior {
            Behavior::Import(spec) => assert!(spec.accepts.contains(&"csv")),
            _ => panic!("expected Import behavior"),
        }
        assert_eq!(DEF.meta.id, "levels-health");
        assert!(DEF.meta.default_on == false, "opt-in due to medical sensitivity");
        assert!(DEF.connection.is_none(), "no auth needed");
    }
}
