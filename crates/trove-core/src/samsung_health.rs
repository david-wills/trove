//! Samsung Health personal-data export import.
//! Brief: docs/integrations/samsung-health.md
//!
//! Samsung Health exports a ZIP (phone: More → Settings → Download personal
//! data) containing one CSV per data category, using reverse-domain filenames:
//!
//!   com.samsung.health.exercise.*.csv
//!   com.samsung.health.heart_rate.*.csv
//!   com.samsung.shealth.sleep.*.csv
//!   tracker.pedometer_day_summary.*.csv
//!   … (locale-tagged, date-stamped suffix)
//!
//! The format uses a **two-row header**: the first row is a metadata comment
//! line (e.g. the package name and export timestamp), and the *second* row is
//! the actual column-name header.  All parsers here detect and skip the
//! metadata row before handing the remainder to the csv crate.
//!
//! **WARNING — Needs-sample.** Samsung's CSV column names carry a
//! reverse-domain prefix (`com.samsung.health.heart_rate.start_time`,
//! `com.samsung.health.heart_rate.heart_rate`, etc.).  Column matching here
//! uses suffix-based resolution so that both the namespaced real-export form
//! and the bare short form match the same logical field.  All timestamps are
//! treated as UTC wall-clock values (Samsung exports UTC; the companion
//! `*.time_offset` column carries the user's local offset, e.g. `UTC+0800`).
//! These assumptions MUST be verified against a real "Download personal data"
//! ZIP before the Needs-sample badge is removed.  Column matching is always by
//! header suffix (case-insensitive, trimmed), never positional, so an incorrect
//! guess falls into the raw layer without corrupting the contract rows.
//!
//! Two write layers (both unconditional):
//!
//! **Raw layer** — every CSV row, verbatim, under
//!   `health/samsung-health/raw/<category>/YYYY-MM.jsonl`
//! where `<category>` is the slug derived from the filename (e.g.
//! `heart_rate`, `sleep`, `exercise`).
//!
//! **Contract layer (vitals only)** — discrete vital-sign readings
//! (HR, BP, SpO2, glucose) are routed to
//!   `health/medical/samsung-health/observations/YYYY-MM.jsonl`
//! as [`crate::health_medical::Observation`] rows, using the Samsung Health
//! `health-medical` contract (same contract as Dexcom).  Observations are
//! only written when a numeric value is successfully parsed; rows with an
//! unresolvable value column are kept in the raw layer only.
//!
//! Activity (steps, sleep) and body composition are raw-only until a real
//! sample confirms the field names — those categories can then be wired to
//! their respective contract shapes in a follow-up.

use std::collections::HashSet;
use std::io::{BufReader, Read as _};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{FixedOffset, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::health_medical::Observation;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths.

/// Contract vital-sign observations (HR, BP, SpO2, glucose → Observation).
const VITALS_DIR: &str = "health/medical/samsung-health/observations";
/// Raw-only sink for all categories (full fidelity).
const RAW_BASE: &str = "health/samsung-health/raw";

const SOURCE: &str = "samsung-health";

// ---------------------------------------------------------------------------
// Column-name suffix tables.
//
// Samsung Health CSV columns carry a reverse-domain prefix in real exports:
//   e.g. "com.samsung.health.heart_rate.start_time"
//        "com.samsung.health.heart_rate.heart_rate"
//        "com.samsung.shealth.blood_pressure.systolic"
//
// We match by **suffix** (bare name, case-insensitive) so that both the
// namespaced form and any bare-header variant resolve correctly.  `resolve_col`
// strips any dot-separated prefix before comparing.
//
// Candidates are listed longest/most-specific first, shorter aliases after,
// so that ".start_time" is preferred over the generic "time".

// Heart rate — timestamp suffix candidates.
const HR_TS_SUFFIXES: &[&str] = &[
    "start_time",
    "time",
    "timestamp",
];
// Heart rate — value suffix candidates.
const HR_VALUE_SUFFIXES: &[&str] = &[
    "heart_rate",
    "bpm",
    "value",
];
// Heart rate — time-offset suffix candidates (sibling column carrying UTC offset string, e.g. "UTC+0800").
const TIME_OFFSET_SUFFIXES: &[&str] = &["time_offset"];

// Blood pressure — timestamp suffix candidates.
const BP_TS_SUFFIXES: &[&str] = &["start_time", "time", "timestamp"];
// Blood pressure — systolic value suffix candidates.
const BP_SYSTOLIC_SUFFIXES: &[&str] = &[
    "systolic",
    "systolic_pressure",
    "sys",
];
// Blood pressure — diastolic value suffix candidates.
const BP_DIASTOLIC_SUFFIXES: &[&str] = &[
    "diastolic",
    "diastolic_pressure",
    "dia",
];

// SpO2 / blood oxygen — timestamp suffix candidates.
const SPO2_TS_SUFFIXES: &[&str] = &["start_time", "time", "timestamp"];
// SpO2 — value suffix candidates.  The real column is likely "blood_oxygen"
// or "spo2" (community reports both).
const SPO2_VALUE_SUFFIXES: &[&str] = &[
    "blood_oxygen",
    "spo2",
    "oxygen_saturation",
    "value",
];

// Blood glucose — timestamp suffix candidates.
const GLUCOSE_TS_SUFFIXES: &[&str] = &[
    "start_time",
    "measured_time",
    "time",
    "timestamp",
];
// Blood glucose — value suffix candidates.
const GLUCOSE_VALUE_SUFFIXES: &[&str] = &[
    "glucose",
    "blood_glucose",
    "glucose_level",
    "value",
];
const GLUCOSE_UNIT_SUFFIXES: &[&str] = &["glucose_unit", "unit"];

// LOINC codes for the vitals we write.
const LOINC_HR: &str = "8867-4";          // Heart rate
const LOINC_BP_SYS: &str = "8480-6";     // Systolic BP
const LOINC_BP_DIA: &str = "8462-4";     // Diastolic BP
const LOINC_SPO2: &str = "59408-5";      // SpO2
const LOINC_GLUCOSE: &str = "2339-0";    // Glucose [Mass/vol] in Blood

// ---------------------------------------------------------------------------
// Category routing from CSV filename.

#[derive(Debug, Clone, Copy, PartialEq)]
enum Category {
    HeartRate,
    BloodPressure,
    BloodOxygen,
    Glucose,
    Steps,
    Sleep,
    Exercise,
    BodyComposition,
    Stress,
    Other,
}

/// Derive a [`Category`] and a short slug from the Samsung Health filename.
/// Samsung names follow the pattern:
///   `com.samsung.health.heart_rate.20240101000000.csv`
///   `com.samsung.shealth.sleep.20240101000000.csv`
///   `tracker.pedometer_day_summary.20240101000000.csv`
///
/// We strip the prefix and trailing date-stamp, then match the resulting slug.
fn classify(filename: &str) -> (Category, String) {
    // Remove any path component and the .csv extension.
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);

    // Strip known Samsung prefixes.
    let stripped = strip_prefix(stem, &[
        "com.samsung.health.",
        "com.samsung.shealth.",
        "tracker.",
    ]);

    // Strip trailing date-stamp (`.YYYYMMDDHHMMSS` or `_YYYYMMDDHHMMSS`).
    let core = strip_date_suffix(stripped);

    let category = if core.contains("heart_rate") || core.contains("heartrate") {
        Category::HeartRate
    } else if core.contains("blood_pressure") || core.contains("bp") {
        Category::BloodPressure
    } else if core.contains("oxygen") || core.contains("spo2") || core.contains("blood_oxygen") {
        Category::BloodOxygen
    } else if core.contains("glucose") || core.contains("blood_glucose") {
        Category::Glucose
    } else if core.contains("pedometer") || core.contains("step") {
        Category::Steps
    } else if core.contains("sleep") {
        Category::Sleep
    } else if core.contains("exercise") || core.contains("workout") || core.contains("activity") {
        Category::Exercise
    } else if core.contains("weight") || core.contains("body_composition")
        || core.contains("body_fat") || core.contains("muscle_mass")
        || core.contains("skeletal")
    {
        Category::BodyComposition
    } else if core.contains("stress") {
        Category::Stress
    } else {
        Category::Other
    };

    let slug = core.replace('.', "_").replace('-', "_");
    (category, slug)
}

fn strip_prefix<'a>(s: &'a str, prefixes: &[&str]) -> &'a str {
    for prefix in prefixes {
        if let Some(rest) = s.strip_prefix(prefix) {
            return rest;
        }
    }
    s
}

fn strip_date_suffix(s: &str) -> &str {
    // Suffix is either `.20240101000000` or `_20240101000000` (14 digits).
    // We find the last '.' or '_' followed by exactly 14 digits.
    let bytes = s.as_bytes();
    for sep in [b'.', b'_'] {
        if let Some(pos) = bytes.iter().rposition(|&b| b == sep) {
            let tail = &bytes[pos + 1..];
            if tail.len() == 14 && tail.iter().all(|b| b.is_ascii_digit()) {
                return &s[..pos];
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------
// Two-row header detection and stripping.
//
// Samsung Health CSVs have a metadata row before the actual column-name row:
//
//   Row 0 (metadata): "com.samsung.health.heart_rate","2.17.0.300","device_uuid"
//   Row 1 (headers):  "com.samsung.health.heart_rate.start_time","com.samsung.health.heart_rate.heart_rate",…
//   Row 2+ (data)
//
// OR (older / locale-specific exports):
//   Row 0 (metadata): "com.samsung.health.heart_rate","package_version","…"
//   Row 1 (headers):  "start_time","heart_rate","device_uuid"
//   Row 2+ (data)
//
// In both cases the METADATA row's first CSV field is the bare package name
// (`com.samsung.health.heart_rate`) with NO trailing `.field_name`.
// The HEADER row's first field either also starts with `com.samsung` but has
// a trailing `.field_name` suffix (namespaced headers), or starts with a plain
// short column name.
//
// Detection rule:
//   1. The first row must contain `com.samsung` (or start with '#').
//   2. Additionally, the first comma-delimited field of that row must NOT look
//      like a column name — i.e., after stripping quotes and the known prefix,
//      the remaining fragment must NOT itself contain a dot (bare package name
//      `com.samsung.health.heart_rate` has no dot after the last segment,
//      while a column name like `com.samsung.health.heart_rate.start_time` does).
//
// This correctly preserves the column-header row when it uses namespaced names.

fn strip_metadata_row(body: &str) -> &str {
    let first_line = body.lines().next().unwrap_or("").trim();
    if first_line.starts_with('#') {
        return body.find('\n').map(|i| &body[i + 1..]).unwrap_or(body);
    }
    let lower = first_line.to_ascii_lowercase();
    if !lower.contains("com.samsung") {
        return body;
    }
    // Extract the first CSV field (handle optional surrounding quotes).
    let first_field = first_line
        .split(',')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    // Strip known Samsung package prefixes to get the "local" part.
    let local = strip_prefix(first_field, &[
        "com.samsung.health.",
        "com.samsung.shealth.",
        "tracker.",
    ]);
    // A metadata-row package name has NO remaining dots after the prefix is
    // stripped (e.g. `heart_rate`).  A column-header field has a dot because
    // it includes the field suffix (e.g. `heart_rate.start_time`).
    let is_metadata_field = !local.contains('.');
    if is_metadata_field {
        body.find('\n').map(|i| &body[i + 1..]).unwrap_or(body)
    } else {
        body
    }
}

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
    crate::registry::newest_stem(&vault.root().join(VITALS_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(RAW_BASE)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "samsung-health",
        name: "Samsung Health",
        kind: IntegrationKind::Import,
        // ECG, blood pressure, glucose, stress, and body composition are
        // detailed medical data — opt-in with explicit acknowledgement.
        default_on: false,
        description: "Import your Samsung Health history — steps, heart rate, \
                      sleep, blood pressure, SpO2, glucose, body composition, \
                      and more — from the official personal-data export ZIP. \
                      Valuable for Android and Galaxy Watch users migrating to \
                      iPhone, or anyone wanting their full health history in one place.",
        domain: "health",
        vault_path: "health/samsung-health/",
        toggleable: false,
        setup: &[
            "Samsung Health stores blood pressure, glucose, ECG, and other sensitive \
             health data. Enabling this import opts you in to collecting them locally.",
            "On your Galaxy device: Samsung Health → More (⋮) → Settings → \
             Download personal data → confirm → ZIP delivered to your phone.",
            "Move the ZIP to this Mac (AirDrop or cable), then drop it here.",
        ],
        caveats: "CSV column names are undocumented and verified only by community \
                  reports — the vitals parser carries a Needs-sample flag until \
                  a real export is confirmed. All rows are always preserved verbatim \
                  in the raw layer regardless.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "zip" {
        run_import_zip(vault, path, progress)
    } else {
        let raw_body = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let body = strip_bom(&raw_body);
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let mut totals = ImportTotals::default();
        import_csv(vault, body, &filename, &mut totals, progress)?;
        Ok(totals.into_outcome())
    }
}

fn run_import_zip(
    vault: &Vault,
    path: &Path,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(BufReader::new(file)).context("reading ZIP")?;

    let mut totals = ImportTotals::default();

    // Collect CSV filenames first (borrows conflict when iterating + reading).
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| {
            archive.by_index(i).ok().and_then(|f| {
                let n = f.name().to_string();
                if n.to_ascii_lowercase().ends_with(".csv") {
                    Some(n)
                } else {
                    None
                }
            })
        })
        .collect();

    for name in names {
        let mut entry = archive.by_name(&name)?;
        let mut body = String::new();
        entry.read_to_string(&mut body)?;
        let body = strip_bom(&body);
        let filename = Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&name)
            .to_string();
        import_csv(vault, body, &filename, &mut totals, progress)?;
    }

    Ok(totals.into_outcome())
}

// ---------------------------------------------------------------------------
// Per-CSV dispatch.

#[derive(Debug, Default)]
struct ImportTotals {
    vitals_imported: u64,
    vitals_duplicates: u64,
    raw_rows: u64,
    raw_skipped: u64,
}

impl ImportTotals {
    fn into_outcome(self) -> ImportOutcome {
        ImportOutcome {
            headline: format!(
                "{} vitals imported, {} raw rows stored",
                self.vitals_imported, self.raw_rows,
            ),
            counts: [
                ("vitals_imported", self.vitals_imported),
                ("vitals_duplicates", self.vitals_duplicates),
                ("raw_rows", self.raw_rows),
                ("raw_skipped", self.raw_skipped),
            ]
            .into(),
        }
    }
}

fn import_csv(
    vault: &Vault,
    body: &str,
    filename: &str,
    totals: &mut ImportTotals,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<()> {
    let body = strip_metadata_row(body);
    let (category, slug) = classify(filename);

    match category {
        Category::HeartRate => import_vitals_hr(vault, body, &slug, totals, progress),
        Category::BloodPressure => import_vitals_bp(vault, body, &slug, totals, progress),
        Category::BloodOxygen => import_vitals_spo2(vault, body, &slug, totals, progress),
        Category::Glucose => import_vitals_glucose(vault, body, &slug, totals, progress),
        _ => import_raw_only(vault, body, &slug, totals, progress),
    }
}

// ---------------------------------------------------------------------------
// Heart-rate vitals importer.

fn import_vitals_hr(
    vault: &Vault,
    body: &str,
    slug: &str,
    totals: &mut ImportTotals,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<()> {
    let contract = vault.stream(VITALS_DIR, Partition::Month);
    let raw = vault.stream(&format!("{RAW_BASE}/{slug}"), Partition::Month);

    let mut seen = existing_guids(&contract)?;

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr.headers().context("HR CSV headers")?.clone();
    let ts_col = resolve_col_by_suffix(&headers, HR_TS_SUFFIXES);
    let val_col = resolve_col_by_suffix(&headers, HR_VALUE_SUFFIXES);
    let offset_col = resolve_col_by_suffix(&headers, TIME_OFFSET_SUFFIXES);

    let (mut rows, mut obs_batch, mut raw_batch) = (0u64, vec![], vec![]);

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else { continue };
        let fields = row_map(&headers, &rec);
        let Some(ts_raw) = first_str_by_col(&fields, ts_col) else { continue };
        let offset_str = first_str_by_col(&fields, offset_col);
        let Some(ts) = parse_ts_utc(&ts_raw, offset_str.as_deref()) else { continue };
        let val_str = first_str_by_col(&fields, val_col);
        let val: Option<f64> = val_str.as_deref().and_then(|s| s.parse().ok());

        // Guid includes the value string so that distinct readings at the
        // same start_time (e.g. bucketed min/max vs. avg) do not collide.
        let guid_val = val_str.as_deref().unwrap_or("");
        let guid = content_guid(&[SOURCE, "hr", &ts_raw, guid_val]);

        raw_batch.push(RawLine { ts: ts.clone(), value: Value::Object(fields.clone()) });
        if seen.insert(guid.clone()) {
            // Only emit a contract Observation when we have an actual value.
            if let Some(v) = val {
                let mut obs = Observation::new(SOURCE, guid, &ts, "Heart Rate");
                obs.code = LOINC_HR.into();
                obs.code_system = "loinc".into();
                obs.value = Some(v);
                obs.unit = "count/min".into();
                extra_fields(&mut obs, &fields, ts_col, val_col);
                obs_batch.push(obs);
                totals.vitals_imported += 1;
            }
        } else {
            totals.vitals_duplicates += 1;
        }
        if rows % 500 == 0 {
            progress(ImportProgress { records: totals.vitals_imported, percent: 0.0 });
        }
    }

    totals.raw_rows += raw_batch.len() as u64;
    raw.append(&raw_batch, |r| &r.ts)?;
    contract.append(&obs_batch, |o| &o.ts)?;
    progress(ImportProgress { records: totals.vitals_imported, percent: 100.0 });
    Ok(())
}

// ---------------------------------------------------------------------------
// Blood-pressure vitals importer.
// BP produces two Observation rows per reading (systolic + diastolic).

fn import_vitals_bp(
    vault: &Vault,
    body: &str,
    slug: &str,
    totals: &mut ImportTotals,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<()> {
    let contract = vault.stream(VITALS_DIR, Partition::Month);
    let raw = vault.stream(&format!("{RAW_BASE}/{slug}"), Partition::Month);

    let mut seen = existing_guids(&contract)?;

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr.headers().context("BP CSV headers")?.clone();
    let ts_col = resolve_col_by_suffix(&headers, BP_TS_SUFFIXES);
    let sys_col = resolve_col_by_suffix(&headers, BP_SYSTOLIC_SUFFIXES);
    let dia_col = resolve_col_by_suffix(&headers, BP_DIASTOLIC_SUFFIXES);
    let offset_col = resolve_col_by_suffix(&headers, TIME_OFFSET_SUFFIXES);

    let (mut rows, mut obs_batch, mut raw_batch) = (0u64, vec![], vec![]);

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else { continue };
        let fields = row_map(&headers, &rec);
        let Some(ts_raw) = first_str_by_col(&fields, ts_col) else { continue };
        let offset_str = first_str_by_col(&fields, offset_col);
        let Some(ts) = parse_ts_utc(&ts_raw, offset_str.as_deref()) else { continue };

        raw_batch.push(RawLine { ts: ts.clone(), value: Value::Object(fields.clone()) });

        if let Some(sys_raw) = first_str_by_col(&fields, sys_col) {
            if let Some(sys_val) = sys_raw.parse::<f64>().ok() {
                let guid = content_guid(&[SOURCE, "bp-sys", &ts_raw, &sys_raw]);
                if seen.insert(guid.clone()) {
                    let mut obs = Observation::new(SOURCE, guid, &ts, "Blood Pressure (Systolic)");
                    obs.code = LOINC_BP_SYS.into();
                    obs.code_system = "loinc".into();
                    obs.value = Some(sys_val);
                    obs.unit = "mm[Hg]".into();
                    extra_fields(&mut obs, &fields, ts_col, sys_col);
                    obs_batch.push(obs);
                    totals.vitals_imported += 1;
                } else {
                    totals.vitals_duplicates += 1;
                }
            }
        }
        if let Some(dia_raw) = first_str_by_col(&fields, dia_col) {
            if let Some(dia_val) = dia_raw.parse::<f64>().ok() {
                let guid = content_guid(&[SOURCE, "bp-dia", &ts_raw, &dia_raw]);
                if seen.insert(guid.clone()) {
                    let mut obs = Observation::new(SOURCE, guid, &ts, "Blood Pressure (Diastolic)");
                    obs.code = LOINC_BP_DIA.into();
                    obs.code_system = "loinc".into();
                    obs.value = Some(dia_val);
                    obs.unit = "mm[Hg]".into();
                    extra_fields(&mut obs, &fields, ts_col, dia_col);
                    obs_batch.push(obs);
                    totals.vitals_imported += 1;
                } else {
                    totals.vitals_duplicates += 1;
                }
            }
        }

        if rows % 500 == 0 {
            progress(ImportProgress { records: totals.vitals_imported, percent: 0.0 });
        }
    }

    totals.raw_rows += raw_batch.len() as u64;
    raw.append(&raw_batch, |r| &r.ts)?;
    contract.append(&obs_batch, |o| &o.ts)?;
    progress(ImportProgress { records: totals.vitals_imported, percent: 100.0 });
    Ok(())
}

// ---------------------------------------------------------------------------
// SpO2 vitals importer.

fn import_vitals_spo2(
    vault: &Vault,
    body: &str,
    slug: &str,
    totals: &mut ImportTotals,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<()> {
    let contract = vault.stream(VITALS_DIR, Partition::Month);
    let raw = vault.stream(&format!("{RAW_BASE}/{slug}"), Partition::Month);

    let mut seen = existing_guids(&contract)?;

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr.headers().context("SpO2 CSV headers")?.clone();
    let ts_col = resolve_col_by_suffix(&headers, SPO2_TS_SUFFIXES);
    let val_col = resolve_col_by_suffix(&headers, SPO2_VALUE_SUFFIXES);
    let offset_col = resolve_col_by_suffix(&headers, TIME_OFFSET_SUFFIXES);

    let (mut rows, mut obs_batch, mut raw_batch) = (0u64, vec![], vec![]);

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else { continue };
        let fields = row_map(&headers, &rec);
        let Some(ts_raw) = first_str_by_col(&fields, ts_col) else { continue };
        let offset_str = first_str_by_col(&fields, offset_col);
        let Some(ts) = parse_ts_utc(&ts_raw, offset_str.as_deref()) else { continue };
        let val_str = first_str_by_col(&fields, val_col);
        let val: Option<f64> = val_str.as_deref().and_then(|s| s.parse().ok());

        let guid_val = val_str.as_deref().unwrap_or("");
        let guid = content_guid(&[SOURCE, "spo2", &ts_raw, guid_val]);

        raw_batch.push(RawLine { ts: ts.clone(), value: Value::Object(fields.clone()) });
        if seen.insert(guid.clone()) {
            if let Some(v) = val {
                let mut obs = Observation::new(SOURCE, guid, &ts, "Oxygen Saturation (SpO2)");
                obs.code = LOINC_SPO2.into();
                obs.code_system = "loinc".into();
                obs.value = Some(v);
                obs.unit = "%".into();
                extra_fields(&mut obs, &fields, ts_col, val_col);
                obs_batch.push(obs);
                totals.vitals_imported += 1;
            }
        } else {
            totals.vitals_duplicates += 1;
        }
        if rows % 500 == 0 {
            progress(ImportProgress { records: totals.vitals_imported, percent: 0.0 });
        }
    }

    totals.raw_rows += raw_batch.len() as u64;
    raw.append(&raw_batch, |r| &r.ts)?;
    contract.append(&obs_batch, |o| &o.ts)?;
    progress(ImportProgress { records: totals.vitals_imported, percent: 100.0 });
    Ok(())
}

// ---------------------------------------------------------------------------
// Glucose vitals importer.

fn import_vitals_glucose(
    vault: &Vault,
    body: &str,
    slug: &str,
    totals: &mut ImportTotals,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<()> {
    let contract = vault.stream(VITALS_DIR, Partition::Month);
    let raw = vault.stream(&format!("{RAW_BASE}/{slug}"), Partition::Month);

    let mut seen = existing_guids(&contract)?;

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr.headers().context("glucose CSV headers")?.clone();
    let ts_col = resolve_col_by_suffix(&headers, GLUCOSE_TS_SUFFIXES);
    let val_col = resolve_col_by_suffix(&headers, GLUCOSE_VALUE_SUFFIXES);
    let unit_col = resolve_col_by_suffix(&headers, GLUCOSE_UNIT_SUFFIXES);
    let offset_col = resolve_col_by_suffix(&headers, TIME_OFFSET_SUFFIXES);

    let (mut rows, mut obs_batch, mut raw_batch) = (0u64, vec![], vec![]);

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else { continue };
        let fields = row_map(&headers, &rec);
        let Some(ts_raw) = first_str_by_col(&fields, ts_col) else { continue };
        let offset_str = first_str_by_col(&fields, offset_col);
        let Some(ts) = parse_ts_utc(&ts_raw, offset_str.as_deref()) else { continue };
        let val_str = first_str_by_col(&fields, val_col);
        let val: Option<f64> = val_str.as_deref().and_then(|s| s.parse().ok());
        let unit = first_str_by_col(&fields, unit_col)
            .unwrap_or_else(|| "mg/dL".into());

        let guid_val = val_str.as_deref().unwrap_or("");
        let guid = content_guid(&[SOURCE, "glucose", &ts_raw, guid_val]);

        raw_batch.push(RawLine { ts: ts.clone(), value: Value::Object(fields.clone()) });
        if seen.insert(guid.clone()) {
            if let Some(v) = val {
                let mut obs = Observation::new(SOURCE, guid, &ts, "Blood Glucose");
                obs.code = LOINC_GLUCOSE.into();
                obs.code_system = "loinc".into();
                obs.value = Some(v);
                obs.unit = unit;
                extra_fields(&mut obs, &fields, ts_col, val_col);
                obs_batch.push(obs);
                totals.vitals_imported += 1;
            }
        } else {
            totals.vitals_duplicates += 1;
        }
        if rows % 500 == 0 {
            progress(ImportProgress { records: totals.vitals_imported, percent: 0.0 });
        }
    }

    totals.raw_rows += raw_batch.len() as u64;
    raw.append(&raw_batch, |r| &r.ts)?;
    contract.append(&obs_batch, |o| &o.ts)?;
    progress(ImportProgress { records: totals.vitals_imported, percent: 100.0 });
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw-only: steps, sleep, exercise, body composition, stress, unknown.

fn import_raw_only(
    vault: &Vault,
    body: &str,
    slug: &str,
    totals: &mut ImportTotals,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<()> {
    let raw = vault.stream(&format!("{RAW_BASE}/{slug}"), Partition::Month);

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr.headers().context("raw CSV headers")?.clone();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();

    let mut raw_batch: Vec<RawLine> = Vec::new();
    let mut skipped = 0u64;

    for rec in rdr.records() {
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        let fields = row_map(&headers, &rec);
        let ts = fields
            .values()
            .filter_map(|v| v.as_str())
            .find_map(parse_ts)
            .unwrap_or_else(|| today.clone());
        raw_batch.push(RawLine { ts, value: Value::Object(fields) });
        if raw_batch.len() % 500 == 0 {
            progress(ImportProgress {
                records: totals.raw_rows + raw_batch.len() as u64,
                percent: 0.0,
            });
        }
    }

    totals.raw_rows += raw_batch.len() as u64;
    totals.raw_skipped += skipped;
    raw.append(&raw_batch, |r| &r.ts)?;
    progress(ImportProgress { records: totals.raw_rows, percent: 100.0 });
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers.

/// Strip a leading UTF-8 BOM (U+FEFF, encoded as EF BB BF) if present.
/// Samsung Health CSVs sometimes carry this marker; the csv crate doesn't
/// strip it, leaving it as a prefix on the first header cell.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Collect guids already stored in a contract stream for deduplication.
fn existing_guids(stream: &crate::store::JsonlStream) -> Result<HashSet<String>> {
    let mut seen = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                if !g.is_empty() {
                    seen.insert(g.to_string());
                }
            }
        }
    }
    Ok(seen)
}

/// Return the actual header string whose **suffix** (after the last '.') matches
/// one of the `suffixes`, case-insensitively, trimmed.  This handles both the
/// namespaced real-export form (`com.samsung.health.heart_rate.start_time`) and
/// any bare short form (`start_time`) in one pass.
///
/// Candidates are tried in the order given; the first match wins.
fn resolve_col_by_suffix<'h>(
    headers: &'h csv::StringRecord,
    suffixes: &[&str],
) -> Option<&'h str> {
    for suffix in suffixes {
        for h in headers.iter() {
            let trimmed = h.trim();
            // The suffix to compare is the part after the last '.', or the
            // whole header if there's no '.'.
            let bare = trimmed.rsplit('.').next().unwrap_or(trimmed);
            if bare.eq_ignore_ascii_case(suffix) {
                return Some(h);
            }
        }
    }
    None
}

/// CSV record → header→value JSON object (verbatim strings).
fn row_map(headers: &csv::StringRecord, rec: &csv::StringRecord) -> Map<String, Value> {
    headers
        .iter()
        .zip(rec.iter())
        .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
        .collect()
}

/// Extract a trimmed, non-empty string from `fields` using an already-resolved
/// column name (`explicit_col` is the exact header string from the CSV).
fn first_str_by_col(
    fields: &Map<String, Value>,
    explicit_col: Option<&str>,
) -> Option<String> {
    explicit_col
        .and_then(|c| fields.get(c))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Copy every field not matching the timestamp or value column into
/// `obs.extra` (device uuid, update time, etc.).
fn extra_fields(
    obs: &mut Observation,
    fields: &Map<String, Value>,
    ts_col: Option<&str>,
    val_col: Option<&str>,
) {
    let skip: HashSet<&str> = ts_col.iter().chain(val_col.iter()).copied().collect();
    for (k, v) in fields {
        if skip.contains(k.as_str()) {
            continue;
        }
        let s = v.as_str().unwrap_or("").trim();
        if !s.is_empty() {
            obs.extra.insert(k.clone(), Value::String(s.to_string()));
        }
    }
}

/// Parse a Samsung Health `time_offset` string such as `"UTC+0800"` or
/// `"UTC-0530"` into a [`FixedOffset`].  Returns `None` for unknown forms.
fn parse_samsung_offset(offset_str: &str) -> Option<FixedOffset> {
    // Strip leading "UTC" prefix if present (case-insensitive).
    let s = offset_str.trim();
    let s = if s.to_ascii_uppercase().starts_with("UTC") { &s[3..] } else { s };
    if s.is_empty() {
        return None;
    }
    // Expect ±HHMM or ±HH:MM.
    let (sign, digits) = if let Some(rest) = s.strip_prefix('-') {
        (-1i32, rest)
    } else {
        let rest = s.strip_prefix('+').unwrap_or(s);
        (1i32, rest)
    };
    let digits = digits.replace(':', "");
    if digits.len() < 4 {
        return None;
    }
    let hh: i32 = digits[..2].parse().ok()?;
    let mm: i32 = digits[2..4].parse().ok()?;
    let total_secs = sign * (hh * 3600 + mm * 60);
    FixedOffset::east_opt(total_secs)
}

/// Parse a Samsung Health timestamp string and return an RFC3339 string
/// in the machine's local timezone.
///
/// Samsung exports UTC wall-clock values.  The companion `time_offset` column
/// (e.g. `"UTC+0800"`) carries the user's local offset at the time of the
/// reading; when present it is used to convert from UTC to that local offset.
/// When absent, the naive datetime is treated as UTC and converted to the
/// machine's local timezone.
///
/// Formats handled:
/// - Epoch milliseconds (`1705315800000`) — always UTC
/// - RFC 3339 / ISO 8601 with explicit offset — used as-is
/// - Naive datetime with space or T separator, optional fractional seconds
///   — treated as UTC, then converted using `offset_str` if provided
/// - Date-only (`YYYY-MM-DD`) — returned as-is (no offset conversion)
fn parse_ts_utc(s: &str, offset_str: Option<&str>) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Epoch milliseconds (Samsung SDK: System.currentTimeMillis() → UTC).
    if let Ok(ms) = s.parse::<i64>() {
        if ms > 1_000_000_000_000 {
            let secs = ms / 1000;
            let nsecs = ((ms % 1000) * 1_000_000) as u32;
            return Utc
                .timestamp_opt(secs, nsecs)
                .single()
                .map(|dt| dt.with_timezone(&Local).to_rfc3339());
        }
        return None;
    }

    // RFC3339 / ISO 8601 with offset — trust the embedded offset.
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // ISO 8601 with numeric-only offset (e.g. `+0800` without colon), which
    // chrono's rfc3339 parser rejects.  Try explicit format strings.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M:%S%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%z",
    ] {
        if let Ok(dt) = chrono::DateTime::parse_from_str(s, fmt) {
            return Some(dt.with_timezone(&Local).to_rfc3339());
        }
    }

    // Naive datetime — treat as UTC, then apply offset if available.
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            // The naive value is a UTC wall-clock time.
            let utc_dt = Utc.from_utc_datetime(&naive);
            if let Some(offset) = offset_str.and_then(parse_samsung_offset) {
                // Convert from UTC to the user's local offset at reading time.
                return Some(utc_dt.with_timezone(&offset).to_rfc3339());
            }
            // No offset available — convert to machine local.
            return Some(utc_dt.with_timezone(&Local).to_rfc3339());
        }
    }

    // Date-only.
    for fmt in ["%Y-%m-%d", "%m/%d/%Y"] {
        if let Ok(d) = NaiveDate::parse_from_str(s, fmt) {
            return Some(d.format("%Y-%m-%d").to_string());
        }
    }

    None
}

/// Convenience wrapper — parse without a time_offset hint (for raw-only path
/// and tests where the offset isn't available).
fn parse_ts(s: &str) -> Option<String> {
    parse_ts_utc(s, None)
}

/// Stable 10-hex-char SHA-256 digest over the joined parts — dedupe key for
/// CSV rows with no native row id.
fn content_guid(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    h.update(parts.join("\u{1f}").as_bytes());
    let digest = h.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Tests.
//
// Fixtures use the NAMESPACED header form found in real Samsung Health exports
// (community-reported: "com.samsung.health.heart_rate.start_time", etc.) as
// the primary fixture, plus a BARE-header fixture to prove backward compat.
// Needs-sample flag remains until a real ZIP is verified against these shapes.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-samsung-health-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn no_progress(_: ImportProgress) {}

    // -------------------------------------------------------------------------
    // Routing.
    // -------------------------------------------------------------------------

    #[test]
    fn classify_heart_rate() {
        let (cat, slug) = classify("com.samsung.health.heart_rate.20240101000000.csv");
        assert_eq!(cat, Category::HeartRate);
        assert!(slug.contains("heart_rate"), "slug: {slug}");
    }

    #[test]
    fn classify_sleep() {
        let (cat, _) = classify("com.samsung.shealth.sleep.20240101120000.csv");
        assert_eq!(cat, Category::Sleep);
    }

    #[test]
    fn classify_pedometer() {
        let (cat, _) = classify("tracker.pedometer_day_summary.20240101000000.csv");
        assert_eq!(cat, Category::Steps);
    }

    #[test]
    fn classify_blood_pressure() {
        let (cat, _) = classify("com.samsung.health.blood_pressure.20240101000000.csv");
        assert_eq!(cat, Category::BloodPressure);
    }

    #[test]
    fn classify_blood_oxygen() {
        let (cat, _) = classify("com.samsung.health.blood_oxygen.20240101000000.csv");
        assert_eq!(cat, Category::BloodOxygen);
    }

    #[test]
    fn classify_glucose() {
        let (cat, _) = classify("com.samsung.health.blood_glucose.20240101000000.csv");
        assert_eq!(cat, Category::Glucose);
    }

    #[test]
    fn classify_exercise() {
        let (cat, _) = classify("com.samsung.health.exercise.20240101000000.csv");
        assert_eq!(cat, Category::Exercise);
    }

    #[test]
    fn classify_body_composition() {
        let (cat, _) = classify("com.samsung.health.body_composition.20240101000000.csv");
        assert_eq!(cat, Category::BodyComposition);
    }

    #[test]
    fn classify_stress() {
        let (cat, _) = classify("com.samsung.health.stress.20240101000000.csv");
        assert_eq!(cat, Category::Stress);
    }

    // -------------------------------------------------------------------------
    // Two-row header / metadata strip.
    // -------------------------------------------------------------------------

    #[test]
    fn strips_samsung_metadata_row() {
        // Real export: metadata row first, then the namespaced column-header row.
        let body = concat!(
            "com.samsung.health.heart_rate,package_version,some_device_id\n",
            "com.samsung.health.heart_rate.start_time,com.samsung.health.heart_rate.heart_rate,com.samsung.health.heart_rate.device_uuid\n",
            "2024-01-15 08:30:00,72,abc123\n",
        );
        let stripped = strip_metadata_row(body);
        assert!(
            stripped.starts_with("com.samsung.health.heart_rate.start_time"),
            "metadata row not stripped; got: {stripped}"
        );
        assert!(
            stripped.lines().count() == 2,
            "expected header+data (2 lines after strip), got: {stripped}"
        );
    }

    #[test]
    fn strip_metadata_row_preserves_namespaced_header() {
        // A CSV that starts directly with a namespaced column-header row (no
        // leading metadata row) must NOT have its header stripped.
        let body = concat!(
            "com.samsung.health.heart_rate.start_time,com.samsung.health.heart_rate.heart_rate\n",
            "2024-01-15 08:30:00,72\n",
        );
        let stripped = strip_metadata_row(body);
        assert!(
            stripped.starts_with("com.samsung.health.heart_rate.start_time"),
            "namespaced header row must not be stripped; got: {stripped}"
        );
        assert_eq!(stripped.lines().count(), 2, "no row should be removed");
    }

    #[test]
    fn no_strip_for_normal_csv() {
        // A CSV without a Samsung metadata row passes through unchanged.
        let body = "start_time,heart_rate,device_uuid\n1705315800000,72,abc123\n";
        assert_eq!(strip_metadata_row(body), body);
    }

    // -------------------------------------------------------------------------
    // BOM stripping.
    // -------------------------------------------------------------------------

    #[test]
    fn strip_bom_removes_utf8_bom() {
        let with_bom = "\u{FEFF}start_time,heart_rate\n";
        assert_eq!(strip_bom(with_bom), "start_time,heart_rate\n");
    }

    #[test]
    fn strip_bom_noop_without_bom() {
        let no_bom = "start_time,heart_rate\n";
        assert_eq!(strip_bom(no_bom), no_bom);
    }

    // -------------------------------------------------------------------------
    // Suffix-based column resolution.
    // -------------------------------------------------------------------------

    #[test]
    fn resolve_col_by_suffix_namespaced() {
        // Simulate a real Samsung Health header row with fully-namespaced columns.
        let headers: csv::StringRecord = vec![
            "com.samsung.health.heart_rate.start_time",
            "com.samsung.health.heart_rate.heart_rate",
            "com.samsung.health.heart_rate.device_uuid",
            "com.samsung.health.heart_rate.time_offset",
        ]
        .into_iter()
        .collect();

        let ts = resolve_col_by_suffix(&headers, HR_TS_SUFFIXES);
        let val = resolve_col_by_suffix(&headers, HR_VALUE_SUFFIXES);
        let off = resolve_col_by_suffix(&headers, TIME_OFFSET_SUFFIXES);

        assert_eq!(ts, Some("com.samsung.health.heart_rate.start_time"), "timestamp column");
        assert_eq!(val, Some("com.samsung.health.heart_rate.heart_rate"), "value column");
        assert_eq!(off, Some("com.samsung.health.heart_rate.time_offset"), "offset column");
    }

    #[test]
    fn resolve_col_by_suffix_bare() {
        // Bare headers (community fixture / fallback) also resolve.
        let headers: csv::StringRecord =
            vec!["start_time", "heart_rate", "device_uuid"].into_iter().collect();
        let ts = resolve_col_by_suffix(&headers, HR_TS_SUFFIXES);
        let val = resolve_col_by_suffix(&headers, HR_VALUE_SUFFIXES);
        assert_eq!(ts, Some("start_time"));
        assert_eq!(val, Some("heart_rate"));
    }

    // -------------------------------------------------------------------------
    // Timestamp parsing — UTC treatment.
    // -------------------------------------------------------------------------

    #[test]
    fn parse_epoch_ms() {
        assert!(parse_ts("1705315800000").is_some(), "epoch-ms should parse");
    }

    #[test]
    fn parse_iso_with_offset() {
        assert!(parse_ts("2024-01-15T08:30:00.000+0800").is_some());
    }

    #[test]
    fn parse_naive_space_treated_as_utc() {
        // A naive datetime "2024-01-15 00:00:00" should be treated as UTC.
        // We can't assert the exact local rendering (machine-tz-dependent) but
        // it must parse and the result must not equal a local-treated version
        // on machines where local != UTC (we just verify it parses).
        assert!(parse_ts_utc("2024-01-15 00:00:00", None).is_some());
    }

    #[test]
    fn parse_ts_utc_applies_time_offset() {
        // "2024-01-15 00:00:00" UTC + "UTC+0800" → "2024-01-15T08:00:00+08:00"
        let result = parse_ts_utc("2024-01-15 00:00:00", Some("UTC+0800")).unwrap();
        assert!(
            result.contains("+08:00"),
            "expected +08:00 in result, got: {result}"
        );
        assert!(result.starts_with("2024-01-15T08:00:00"), "got: {result}");
    }

    #[test]
    fn parse_date_only() {
        assert_eq!(parse_ts("2024-01-15"), Some("2024-01-15".into()));
    }

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_ts("").is_none());
        assert!(parse_ts("   ").is_none());
    }

    #[test]
    fn parse_samsung_offset_positive() {
        let off = parse_samsung_offset("UTC+0800").unwrap();
        assert_eq!(off.local_minus_utc(), 8 * 3600);
    }

    #[test]
    fn parse_samsung_offset_negative() {
        let off = parse_samsung_offset("UTC-0530").unwrap();
        assert_eq!(off.local_minus_utc(), -(5 * 3600 + 30 * 60));
    }

    #[test]
    fn parse_samsung_offset_unknown_returns_none() {
        assert!(parse_samsung_offset("").is_none());
        assert!(parse_samsung_offset("garbage").is_none());
    }

    // -------------------------------------------------------------------------
    // Heart-rate import — namespaced headers (real export shape).
    // -------------------------------------------------------------------------

    /// Namespaced headers matching the real Samsung Health export shape
    /// (community-reported).  The time_offset column ("UTC+0800") is included
    /// so we can verify it is applied to produce a +08:00 RFC3339 timestamp.
    const HR_CSV_NAMESPACED: &str = "\
com.samsung.health.heart_rate.start_time,com.samsung.health.heart_rate.heart_rate,com.samsung.health.heart_rate.device_uuid,com.samsung.health.heart_rate.time_offset
2024-01-15 08:30:00,72,abc123,UTC+0800
2024-01-15 09:00:00,68,abc123,UTC+0800
";

    #[test]
    fn imports_hr_namespaced_to_contract_and_raw() {
        let v = temp_vault("hr_ns");
        let mut totals = ImportTotals::default();
        import_vitals_hr(&v, HR_CSV_NAMESPACED, "heart_rate", &mut totals, &mut no_progress)
            .unwrap();

        assert_eq!(totals.vitals_imported, 2, "both rows should map to contract observations");
        assert_eq!(totals.vitals_duplicates, 0);
        assert_eq!(totals.raw_rows, 2);

        // Verify that the stored `ts` carries the time_offset-adjusted value.
        let contract = v.stream(VITALS_DIR, Partition::Month);
        assert!(!contract.partitions().unwrap().is_empty(), "contract partition created");
        let obs: Vec<Observation> = contract
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| contract.read::<Observation>(p).unwrap())
            .collect();
        // All observations should have +08:00 in the timestamp.
        for o in &obs {
            assert!(
                o.ts.contains("+08:00"),
                "timestamp should reflect UTC+0800 offset, got: {}",
                o.ts
            );
            assert!(o.value.is_some(), "value must be present: {}", o.source);
        }
    }

    #[test]
    fn hr_namespaced_import_is_idempotent() {
        let v = temp_vault("hr_ns_idem");
        let mut t1 = ImportTotals::default();
        import_vitals_hr(&v, HR_CSV_NAMESPACED, "heart_rate", &mut t1, &mut no_progress).unwrap();
        let mut t2 = ImportTotals::default();
        import_vitals_hr(&v, HR_CSV_NAMESPACED, "heart_rate", &mut t2, &mut no_progress).unwrap();
        assert_eq!(t2.vitals_imported, 0, "re-import should yield all duplicates");
        assert_eq!(t2.vitals_duplicates, 2);
    }

    // -------------------------------------------------------------------------
    // Heart-rate import — bare headers (backward compat / bare-format exports).
    // -------------------------------------------------------------------------

    /// Bare-name headers; epoch-ms timestamps (both formats must work).
    const HR_CSV_BARE: &str = "\
start_time,heart_rate,device_uuid,update_time
1705315800000,72,abc123,1705315860000
1705316400000,68,abc123,1705316460000
";

    #[test]
    fn imports_hr_bare_headers_to_contract_and_raw() {
        let v = temp_vault("hr_bare");
        let mut totals = ImportTotals::default();
        import_vitals_hr(&v, HR_CSV_BARE, "heart_rate", &mut totals, &mut no_progress).unwrap();

        assert_eq!(totals.vitals_imported, 2);
        assert_eq!(totals.vitals_duplicates, 0);
        assert_eq!(totals.raw_rows, 2);

        let contract = v.stream(VITALS_DIR, Partition::Month);
        assert!(!contract.partitions().unwrap().is_empty(), "contract partition created");

        let raw = v.stream(&format!("{RAW_BASE}/heart_rate"), Partition::Month);
        assert!(!raw.partitions().unwrap().is_empty(), "raw partition created");
    }

    #[test]
    fn hr_bare_import_is_idempotent() {
        let v = temp_vault("hr_bare_idem");
        let mut t1 = ImportTotals::default();
        import_vitals_hr(&v, HR_CSV_BARE, "heart_rate", &mut t1, &mut no_progress).unwrap();
        let mut t2 = ImportTotals::default();
        import_vitals_hr(&v, HR_CSV_BARE, "heart_rate", &mut t2, &mut no_progress).unwrap();
        assert_eq!(t2.vitals_imported, 0, "re-import should yield all duplicates");
        assert_eq!(t2.vitals_duplicates, 2);
    }

    // -------------------------------------------------------------------------
    // None-value guard: rows with no parseable value must NOT produce an
    // Observation — they stay in the raw layer only.
    // -------------------------------------------------------------------------

    #[test]
    fn hr_row_with_no_value_stays_raw_only() {
        // The value column is absent; the timestamp column is present.
        // No Observation should be emitted; the raw row is still written.
        let body = "\
com.samsung.health.heart_rate.start_time,com.samsung.health.heart_rate.device_uuid
2024-01-15 08:30:00,abc123
";
        let v = temp_vault("hr_no_val");
        let mut totals = ImportTotals::default();
        import_vitals_hr(&v, body, "heart_rate", &mut totals, &mut no_progress).unwrap();
        assert_eq!(totals.vitals_imported, 0, "no value → no contract obs");
        assert_eq!(totals.raw_rows, 1, "raw row still written");
    }

    // -------------------------------------------------------------------------
    // Blood-pressure import — namespaced + bare headers.
    // -------------------------------------------------------------------------

    const BP_CSV_NAMESPACED: &str = "\
com.samsung.shealth.blood_pressure.start_time,com.samsung.shealth.blood_pressure.systolic,com.samsung.shealth.blood_pressure.diastolic,com.samsung.shealth.blood_pressure.device_uuid,com.samsung.shealth.blood_pressure.time_offset
2024-01-15 08:30:00,122,78,dev001,UTC+0000
2024-01-16 09:00:00,118,76,dev001,UTC+0000
";

    #[test]
    fn imports_bp_namespaced_two_obs_per_row() {
        let v = temp_vault("bp_ns");
        let mut totals = ImportTotals::default();
        import_vitals_bp(&v, BP_CSV_NAMESPACED, "blood_pressure", &mut totals, &mut no_progress)
            .unwrap();
        // 2 rows × 2 observations (sys + dia) = 4.
        assert_eq!(totals.vitals_imported, 4);
        assert_eq!(totals.raw_rows, 2);
    }

    const BP_CSV_BARE: &str = "\
start_time,systolic,diastolic,device_uuid
2024-01-15 08:30:00,122,78,dev001
2024-01-16 09:00:00,118,76,dev001
";

    #[test]
    fn imports_bp_bare_two_obs_per_row() {
        let v = temp_vault("bp_bare");
        let mut totals = ImportTotals::default();
        import_vitals_bp(&v, BP_CSV_BARE, "blood_pressure", &mut totals, &mut no_progress)
            .unwrap();
        // 2 rows × 2 observations (sys + dia) = 4.
        assert_eq!(totals.vitals_imported, 4);
        assert_eq!(totals.raw_rows, 2);
    }

    // -------------------------------------------------------------------------
    // SpO2 import — namespaced + bare headers.
    // -------------------------------------------------------------------------

    const SPO2_CSV_NAMESPACED: &str = "\
com.samsung.health.blood_oxygen.start_time,com.samsung.health.blood_oxygen.blood_oxygen,com.samsung.health.blood_oxygen.device_uuid,com.samsung.health.blood_oxygen.time_offset
2024-01-15 08:30:00,97,watch001,UTC+0000
2024-01-15 09:00:00,98,watch001,UTC+0000
";

    #[test]
    fn imports_spo2_namespaced_to_contract_and_raw() {
        let v = temp_vault("spo2_ns");
        let mut totals = ImportTotals::default();
        import_vitals_spo2(
            &v,
            SPO2_CSV_NAMESPACED,
            "blood_oxygen",
            &mut totals,
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(totals.vitals_imported, 2);
        assert_eq!(totals.raw_rows, 2);
    }

    const SPO2_CSV_BARE: &str = "\
start_time,spo2,device_uuid
2024-01-15 08:30:00,97,watch001
2024-01-15 09:00:00,98,watch001
";

    #[test]
    fn imports_spo2_bare_to_contract_and_raw() {
        let v = temp_vault("spo2_bare");
        let mut totals = ImportTotals::default();
        import_vitals_spo2(&v, SPO2_CSV_BARE, "blood_oxygen", &mut totals, &mut no_progress)
            .unwrap();
        assert_eq!(totals.vitals_imported, 2);
        assert_eq!(totals.raw_rows, 2);
    }

    // -------------------------------------------------------------------------
    // Glucose import — namespaced + bare headers.
    // -------------------------------------------------------------------------

    const GLUCOSE_CSV_NAMESPACED: &str = "\
com.samsung.health.blood_glucose.start_time,com.samsung.health.blood_glucose.glucose,com.samsung.health.blood_glucose.glucose_unit,com.samsung.health.blood_glucose.device_uuid,com.samsung.health.blood_glucose.time_offset
2024-01-15 07:00:00,95,mg/dL,meter001,UTC+0000
2024-01-15 12:00:00,110,mg/dL,meter001,UTC+0000
";

    #[test]
    fn imports_glucose_namespaced_with_loinc_and_unit() {
        let v = temp_vault("glucose_ns");
        let mut totals = ImportTotals::default();
        import_vitals_glucose(
            &v,
            GLUCOSE_CSV_NAMESPACED,
            "blood_glucose",
            &mut totals,
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(totals.vitals_imported, 2);
        assert_eq!(totals.raw_rows, 2);

        let contract = v.stream(VITALS_DIR, Partition::Month);
        let obs: Vec<Observation> = contract
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| contract.read::<Observation>(p).unwrap())
            .collect();
        assert!(obs.iter().any(|o| o.code == LOINC_GLUCOSE));
        assert!(obs.iter().any(|o| o.unit == "mg/dL"));
    }

    const GLUCOSE_CSV_BARE: &str = "\
start_time,glucose,unit,device_uuid
2024-01-15 07:00:00,95,mg/dL,meter001
2024-01-15 12:00:00,110,mg/dL,meter001
";

    #[test]
    fn imports_glucose_bare_with_loinc_and_unit() {
        let v = temp_vault("glucose_bare");
        let mut totals = ImportTotals::default();
        import_vitals_glucose(
            &v,
            GLUCOSE_CSV_BARE,
            "blood_glucose",
            &mut totals,
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(totals.vitals_imported, 2);
        assert_eq!(totals.raw_rows, 2);

        let contract = v.stream(VITALS_DIR, Partition::Month);
        let obs: Vec<Observation> = contract
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| contract.read::<Observation>(p).unwrap())
            .collect();
        assert!(obs.iter().any(|o| o.code == LOINC_GLUCOSE));
        assert!(obs.iter().any(|o| o.unit == "mg/dL"));
    }

    // -------------------------------------------------------------------------
    // GUID uniqueness: two HR rows at the same start_time but different values
    // must NOT be treated as duplicates.
    // -------------------------------------------------------------------------

    #[test]
    fn hr_guid_unique_when_same_ts_different_value() {
        // Hourly-summary row for min=58, then avg=65 could share a start_time.
        let body = "\
com.samsung.health.heart_rate.start_time,com.samsung.health.heart_rate.heart_rate,com.samsung.health.heart_rate.device_uuid
2024-01-15 08:00:00,58,watch001
2024-01-15 08:00:00,65,watch001
";
        let v = temp_vault("hr_guid_uniq");
        let mut totals = ImportTotals::default();
        import_vitals_hr(&v, body, "heart_rate", &mut totals, &mut no_progress).unwrap();
        assert_eq!(totals.vitals_imported, 2, "distinct values at same ts must not collide");
        assert_eq!(totals.vitals_duplicates, 0);
    }

    // -------------------------------------------------------------------------
    // Raw-only categories.
    // -------------------------------------------------------------------------

    const SLEEP_CSV: &str = "\
start_time,end_time,sleep_type,duration
2024-01-15 23:00:00,2024-01-16 07:00:00,SLEEP_SESSION,28800
2024-01-16 23:30:00,2024-01-17 06:30:00,SLEEP_SESSION,25200
";

    #[test]
    fn sleep_is_raw_only() {
        let v = temp_vault("sleep");
        let mut totals = ImportTotals::default();
        import_raw_only(&v, SLEEP_CSV, "sleep", &mut totals, &mut no_progress).unwrap();
        assert_eq!(totals.raw_rows, 2);
        // No contract rows.
        let contract = v.stream(VITALS_DIR, Partition::Month);
        assert!(contract.partitions().unwrap().is_empty());
    }

    // -------------------------------------------------------------------------
    // ZIP import.
    // -------------------------------------------------------------------------

    #[test]
    fn zip_import_dispatches_multiple_csv_types() {
        use std::io::Write;

        let v = temp_vault("zip");
        let zip_path = v.root().join("samsung_health_export.zip");
        // HR CSV with Samsung metadata row, then namespaced column headers.
        let hr_with_meta = format!(
            "com.samsung.health.heart_rate,package_version_meta,device_id_meta\n{HR_CSV_NAMESPACED}"
        );

        {
            let mut zw = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file(
                "com.samsung.health.heart_rate/com.samsung.health.heart_rate.20240115000000.csv",
                opts,
            )
            .unwrap();
            zw.write_all(hr_with_meta.as_bytes()).unwrap();
            zw.start_file(
                "com.samsung.shealth.sleep/com.samsung.shealth.sleep.20240115000000.csv",
                opts,
            )
            .unwrap();
            zw.write_all(SLEEP_CSV.as_bytes()).unwrap();
            zw.finish().unwrap();
        }

        let outcome = run_import_zip(&v, &zip_path, &mut no_progress).unwrap();
        assert_eq!(outcome.counts.get("vitals_imported"), Some(&2));
        assert_eq!(outcome.counts.get("raw_rows"), Some(&4));
    }

    // -------------------------------------------------------------------------
    // Unknown columns — graceful degradation.
    // -------------------------------------------------------------------------

    #[test]
    fn unknown_columns_skip_gracefully() {
        // Column names that don't match any of our guesses (e.g., German locale).
        let body = "Zeit,Puls,Geraet\n2024-01-15 08:30:00,72,Uhr\n";
        let v = temp_vault("unknown_cols");
        let mut totals = ImportTotals::default();
        // timestamp column unknown → row skipped without panic.
        import_vitals_hr(&v, body, "heart_rate", &mut totals, &mut no_progress).unwrap();
        assert_eq!(totals.vitals_imported, 0);
    }

    // -------------------------------------------------------------------------
    // Filename-based dispatch via import_csv.
    // -------------------------------------------------------------------------

    #[test]
    fn dispatch_routes_by_filename() {
        let v = temp_vault("dispatch");
        let mut totals = ImportTotals::default();
        import_csv(
            &v,
            HR_CSV_NAMESPACED,
            "com.samsung.health.heart_rate.20240115000000.csv",
            &mut totals,
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(totals.vitals_imported, 2);

        import_csv(
            &v,
            SLEEP_CSV,
            "com.samsung.shealth.sleep.20240115000000.csv",
            &mut totals,
            &mut no_progress,
        )
        .unwrap();
        assert_eq!(totals.vitals_imported, 2, "sleep should not add vitals");
        assert!(totals.raw_rows >= 4);
    }

    // -------------------------------------------------------------------------
    // DEF smoke test.
    // -------------------------------------------------------------------------

    #[test]
    fn def_is_correct() {
        assert_eq!(DEF.meta.id, "samsung-health");
        assert!(!DEF.meta.default_on);
        assert!(DEF.meta.setup.len() >= 2);
        assert!(DEF.connection.is_none());
        let Behavior::Import(spec) = DEF.behavior else {
            panic!("expected Import behavior");
        };
        assert!(spec.accepts.contains(&"zip"));
        assert!(spec.accepts.contains(&"csv"));
    }
}
