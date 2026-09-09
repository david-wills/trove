//! Abbott FreeStyle Libre / LibreView — CGM glucose data via LibreView CSV
//! export. Brief: docs/integrations/freestyle-libre.md.
//!
//! **Import** (not a cloud sync): the user exports their glucose history from
//! libreview.com → Reports → Download glucose data, then drops the CSV on the
//! import box. No login required; the official LibreView API is Abbott-partner-
//! only. Re-importing a newer export is idempotent (deduplicated by a stable
//! guid derived from device serial + timestamp).
//!
//! ## LibreView CSV format (evidence: multiple open-source parsers)
//!
//! The export is a plain UTF-8 CSV with two leading rows before the actual
//! column-header row. Confirmed layout (English locale):
//!
//! ```text
//! Row 0: "Glucose Data","Created 01-01-2026 12:00 UTC","Created by","UserName"
//! Row 1: "Device","Serial Number","Device Timestamp","Record Type",
//!         "Historic Glucose mg/dL","Scan Glucose mg/dL",[insulin/food cols],
//!         "Glucose (Ketone) mmol/L",...
//! Row 2+: data rows
//! ```
//!
//! Column indices (0-based) that matter — we index by position, not header name,
//! to tolerate locale-translated headers (Tidepool's libreViewDriver.js precedent):
//!
//! - `[0]` Device model ("FreeStyle Libre 3")
//! - `[1]` Serial Number (stable device id)
//! - `[2]` Device Timestamp ("DD-MM-YYYY HH:MM" or "MM-DD-YYYY HH:MM AM/PM")
//! - `[3]` Record Type (0=historic, 1=scan, 2=strip/fingerstick, 3=ketone)
//! - `[4]` Historic Glucose (mg/dL or mmol/L per account settings)
//! - `[5]` Scan Glucose
//! - `[14]` Strip/fingerstick Glucose
//! - `[15]` Ketone (mmol/L)
//!
//! Glucose unit is inferred: if any reading value ≥ 40 it is mg/dL, otherwise
//! mmol/L. This is the Tidepool approach ("we cannot parse the units from the CSV
//! due to language differences").
//!
//! ## Two vault layers
//!
//! 1. **Raw** — full-fidelity CSV row (as a JSON object) under
//!    `health/freestyle-libre/raw/YYYY-MM.jsonl`, unconditional.
//! 2. **Contract** — [`crate::health_medical::Observation`] rows under
//!    `health/medical/freestyle-libre/observations/YYYY-MM.jsonl`, reusing the
//!    bound `health-medical` contract (same sink as Dexcom). LOINC `2339-0`
//!    (Glucose [Mass/volume] in Blood) for glucose readings; ketone readings use
//!    LOINC `2514-8` (Ketones [Mass/volume] in Blood).
//!
//! ## Guid / dedupe
//!
//! No stable source id exists in the CSV. We derive a guid from
//! `{serial_number}|{timestamp_raw}|{record_type}` — the serial number scopes
//! the guid to one sensor, the raw timestamp string is what the device actually
//! reported, and the record type distinguishes a historic read at the same
//! timestamp from a scan at the same timestamp. Re-importing an overlapping
//! export is idempotent by guid dedupe against what's already on disk.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::health_medical::Observation;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract observations (health-medical, same sink as Dexcom).
const CONTRACT_DIR: &str = "health/medical/freestyle-libre/observations";
/// Raw CSV rows, full fidelity.
const RAW_DIR: &str = "health/freestyle-libre/raw";

/// LOINC code for "Glucose [Mass/volume] in Blood" — shared with Dexcom so
/// cross-source glucose aligns at read time.
const GLUCOSE_LOINC: &str = "2339-0";
/// LOINC code for "Ketones [Mass/volume] in Blood".
const KETONE_LOINC: &str = "2514-8";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "freestyle-libre",
        name: "FreeStyle Libre (LibreView)",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import continuous glucose readings from your Abbott FreeStyle Libre sensor \
             via a LibreView CSV export. Full history, full resolution — backfills every \
             scan and automatic reading. Re-importing a newer export never duplicates.",
        domain: "health",
        vault_path: "health/freestyle-libre/",
        toggleable: false,
        setup: &[
            "Continuous glucose is sensitive medical data — enabling this opts you in to \
             collecting it.",
            "Sign in at libreview.com → go to your Glucose History (Reports → Download \
             glucose data), complete the CAPTCHA, and a CSV will download.",
            "Import the CSV here. Re-running with a newer export is safe: already-stored \
             readings are skipped.",
            "LibreLinkUp users already get basic glucose through the Apple Health import — \
             this adds full-resolution 15-minute sensor history.",
        ],
        caveats:
            "The official Abbott API is partner-only; the CSV export is the supported \
             path. Unit detection is automatic (mg/dL if any reading ≥ 40, otherwise \
             mmol/L). Locale-translated column headers are tolerated by parsing columns \
             by position.",
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
// Record type constants (from Tidepool's libreViewDriver.js).

const RECORD_HISTORIC: u8 = 0;
const RECORD_SCAN: u8 = 1;
const RECORD_STRIP: u8 = 2;
const RECORD_KETONE: u8 = 3;

// ---------------------------------------------------------------------------
// Timestamp parsing.

/// Date-field ordering inferred from the export file (whole-file decision).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DateOrder {
    /// Day is first: `DD-MM-YYYY` (European / default LibreView locale).
    DayFirst,
    /// Month is first: `MM-DD-YYYY` (US locale).
    MonthFirst,
    /// Could not determine — try both, day-first first (conservative default).
    Unknown,
}

/// Infer the date ordering once per file before parsing any rows.
///
/// Strategy (most-to-least-reliable):
/// 1. Row 0 preamble: LibreView writes `"Created DD-MM-YYYY HH:MM UTC"` (EU) or
///    `"Created MM-DD-YYYY HH:MM UTC"` (US) in column 1. If the day-position
///    value > 12 it cannot be a month, so we know the order.
/// 2. Data cells: scan all `Device Timestamp` cells (column 2 of data rows).
///    - If first numeric field > 12 → must be month-first (MM-DD).
///    - If second numeric field > 12 → must be day-first (DD-MM).
///    First unambiguous cell wins.
/// 3. Fall back to `Unknown` (caller tries DD-first then MM-first).
fn infer_date_order(preamble_row: Option<&Vec<String>>, data_rows: &[Vec<String>]) -> DateOrder {
    // --- Step 1: preamble "Created XX-YY-YYYY ..." in col 1 ---
    if let Some(row) = preamble_row {
        if let Some(created) = row.get(1).map(|s| s.trim()) {
            // Strip the "Created " prefix and grab the date part.
            let date_part = created.strip_prefix("Created ").unwrap_or(created);
            if let Some(order) = order_from_date_str(date_part) {
                return order;
            }
        }
    }

    // --- Step 2: scan Device Timestamp column (col 2) of data rows ---
    for cols in data_rows {
        if let Some(ts) = cols.get(2).map(|s| s.trim()) {
            if let Some(order) = order_from_date_str(ts) {
                return order;
            }
        }
    }

    DateOrder::Unknown
}

/// Try to extract date ordering from a string that starts with a date like
/// `DD-MM-YYYY` or `MM-DD-YYYY` (dash or slash separated, with optional
/// trailing time/text).  Returns `None` when both leading fields are ≤ 12
/// (ambiguous).
fn order_from_date_str(s: &str) -> Option<DateOrder> {
    // Split on the first separator (dash or slash) and grab first two fields.
    let sep = if s.contains('-') { '-' } else if s.contains('/') { '/' } else { return None };
    let mut parts = s.splitn(3, sep);
    let first: u32 = parts.next()?.trim().parse().ok()?;
    let second: u32 = parts.next()?.trim().parse().ok()?;
    // If first field can't be a valid day (>31) or month (>12) we can still
    // infer from the comparison.
    if first > 12 {
        // first field > 12 → can only be a day → DD-MM ordering.
        // But wait: if it's also > 31 it's invalid; we still know it's not a month.
        return Some(DateOrder::DayFirst);
    }
    if second > 12 {
        // second field > 12 → can only be a day → MM-DD ordering.
        return Some(DateOrder::MonthFirst);
    }
    // Both ≤ 12: ambiguous.
    None
}

/// Parse a LibreView device timestamp using a pre-determined date ordering.
///
/// The export format varies by locale and account settings:
/// - `DD-MM-YYYY HH:MM` (24-hour, Day-Month-Year)
/// - `MM-DD-YYYY HH:MM` (24-hour, Month-Day-Year)
/// - `DD-MM-YYYY HH:MM AM/PM` (12-hour)
/// - `MM-DD-YYYY HH:MM AM/PM` (12-hour)
///
/// `order` is inferred once per file by `infer_date_order` so that ambiguous
/// dates (day and month both ≤ 12) are resolved consistently rather than
/// silently defaulting to the wrong ordering.
fn parse_device_timestamp(s: &str, order: DateOrder) -> Option<NaiveDateTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Choose the preferred date fragment based on the inferred ordering.
    // For dash-separated and slash-separated variants, preferred comes first.
    let (dash_p, dash_s) = match order {
        DateOrder::DayFirst | DateOrder::Unknown => ("%d-%m-%Y", "%m-%d-%Y"),
        DateOrder::MonthFirst => ("%m-%d-%Y", "%d-%m-%Y"),
    };
    let (slash_p, slash_s) = match order {
        DateOrder::DayFirst | DateOrder::Unknown => ("%d/%m/%Y", "%m/%d/%Y"),
        DateOrder::MonthFirst => ("%m/%d/%Y", "%d/%m/%Y"),
    };

    // Build owned format strings (needed to hold the concatenated slices).
    let fmts = [
        format!("{dash_p} %H:%M"),
        format!("{dash_s} %H:%M"),
        format!("{slash_p} %H:%M"),
        format!("{slash_s} %H:%M"),
        format!("{dash_p} %I:%M %p"),
        format!("{dash_s} %I:%M %p"),
        format!("{slash_p} %I:%M %p"),
        format!("{slash_s} %I:%M %p"),
    ];

    for fmt in &fmts {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    None
}

/// Convert a device-local `NaiveDateTime` to RFC3339 in the local timezone.
/// LibreView timestamps are already in the user's local clock — the sensor
/// reports device time, and LibreView stores it without an offset — so we
/// treat them as local wall-clock time.
fn ts_rfc3339(dt: NaiveDateTime) -> Option<String> {
    Local.from_local_datetime(&dt).earliest().map(|t| t.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Unit detection (Tidepool's approach).

/// LibreView exports either mg/dL or mmol/L depending on account settings; the
/// unit is not reliably in the column header (locale translation) so we infer
/// from values: any reading ≥ 40 means mg/dL (mmol/L readings are typically
/// 3–20; a 40 mg/dL ≈ 2.2 mmol/L so there is no ambiguity).
fn detect_unit(glucose_values: &[f64]) -> &'static str {
    if glucose_values.iter().any(|&v| v >= 40.0) { "mg/dL" } else { "mmol/L" }
}

// ---------------------------------------------------------------------------
// CSV row representation.

/// One parsed data row: everything we'll need to write both layers.
#[derive(Debug)]
struct Row {
    /// Original raw columns for the raw layer.
    raw_cols: Vec<String>,
    /// Stable dedupe key.
    guid: String,
    /// RFC3339 local timestamp (or None if unparseable — row is skipped).
    ts: Option<String>,
    /// Record type.
    record_type: u8,
    /// Glucose value (column 4 or 5 or 14 depending on record type).
    glucose: Option<f64>,
    /// Ketone value (column 15 for record type 3).
    ketone: Option<f64>,
    /// Device model (column 0).
    device: String,
    /// Serial number (column 1) — part of the guid.
    serial: String,
    /// Raw timestamp string (column 2) — part of the guid (stored for
    /// documentation; the guid is computed during row construction).
    #[allow(dead_code)]
    ts_raw: String,
}

/// Parse a non-empty, non-whitespace CSV string to f64. Tolerates a comma
/// decimal separator (common in continental European locales).
fn parse_f64(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Replace comma decimal separator.
    let normalized = s.replace(',', ".");
    normalized.parse::<f64>().ok()
}

fn col(cols: &[String], idx: usize) -> &str {
    cols.get(idx).map(|s| s.as_str()).unwrap_or("")
}

/// Parse a single CSV data row (already split into columns).
/// `order` must be pre-determined for the whole file via `infer_date_order`
/// so that ambiguous dates (day and month both ≤ 12) parse consistently.
fn parse_row(raw_cols: Vec<String>, order: DateOrder) -> Option<Row> {
    let device = col(&raw_cols, 0).trim().to_string();
    let serial = col(&raw_cols, 1).trim().to_string();
    let ts_raw = col(&raw_cols, 2).trim().to_string();
    let record_type_str = col(&raw_cols, 3).trim();

    // A fully-blank row (trailing newline) should be ignored.
    if device.is_empty() && serial.is_empty() && ts_raw.is_empty() {
        return None;
    }

    let record_type: u8 = record_type_str.parse().ok()?;

    let glucose = match record_type {
        RECORD_HISTORIC => parse_f64(col(&raw_cols, 4)),
        RECORD_SCAN => parse_f64(col(&raw_cols, 5)),
        RECORD_STRIP => parse_f64(col(&raw_cols, 14)),
        _ => None,
    };
    let ketone = if record_type == RECORD_KETONE {
        parse_f64(col(&raw_cols, 15))
    } else {
        None
    };

    // Skip rows that carry neither glucose nor ketone (e.g. insulin/notes-only
    // events which have record type ≥ 4 — not CGM readings).
    if glucose.is_none() && ketone.is_none() {
        return None;
    }

    let ts = parse_device_timestamp(&ts_raw, order).and_then(ts_rfc3339);

    // Stable guid: serial + raw timestamp + record type. No globally-unique
    // id is present in the CSV; this combination is collision-free for a single
    // device (a sensor can't report two different readings at the exact same
    // timestamp) and scoped by serial so two sensors don't collide.
    let guid = format!("{serial}|{ts_raw}|{record_type}");

    Some(Row { raw_cols, guid, ts, record_type, glucose, ketone, device, serial, ts_raw })
}

// ---------------------------------------------------------------------------
// Raw layer: one JSON object per row.

#[derive(Serialize)]
struct RawLine {
    /// The partition key — not written to disk (skip), used only to file the
    /// row in the right monthly shard.
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

fn raw_line(row: &Row, headers: &[String]) -> Option<RawLine> {
    let ts = row.ts.clone()?;
    let mut fields = Map::new();
    for (i, val) in row.raw_cols.iter().enumerate() {
        let key = headers
            .get(i)
            .map(|h| h.trim())
            .filter(|h| !h.is_empty())
            .map(|h| h.to_string())
            .unwrap_or_else(|| format!("col_{i}"));
        if !val.trim().is_empty() {
            fields.insert(key, Value::String(val.trim().to_string()));
        }
    }
    Some(RawLine { ts, fields })
}

// ---------------------------------------------------------------------------
// Contract layer: Observation rows.

/// Map one parsed row to an [`Observation`]. `unit` is pre-determined (mg/dL
/// or mmol/L) from the whole-file unit detection pass.
fn observation_from(row: &Row, unit: &str) -> Option<Observation> {
    let ts = row.ts.clone()?;

    let (value, loinc, test, obs_unit) = if row.record_type == RECORD_KETONE {
        // Ketone is always mmol/L regardless of glucose unit.
        (row.ketone, KETONE_LOINC, "Ketone", "mmol/L")
    } else {
        (row.glucose, GLUCOSE_LOINC, "Glucose", unit)
    };

    let mut extra = Map::new();
    // Device provenance — not a contract column, but useful for the raw layer
    // reader (extra is full fidelity).
    if !row.device.is_empty() {
        extra.insert("device".into(), Value::String(row.device.clone()));
    }
    if !row.serial.is_empty() {
        extra.insert("serialNumber".into(), Value::String(row.serial.clone()));
    }
    let record_label = match row.record_type {
        RECORD_HISTORIC => "historic",
        RECORD_SCAN => "scan",
        RECORD_STRIP => "strip",
        RECORD_KETONE => "ketone",
        n => {
            extra.insert("recordType".into(), Value::from(n));
            "unknown"
        }
    };
    extra.insert("recordType".into(), Value::String(record_label.into()));

    Some(Observation {
        ts,
        source: "freestyle-libre".into(),
        guid: row.guid.clone(),
        test: test.into(),
        code: loinc.into(),
        code_system: "loinc".into(),
        value,
        value_text: String::new(),
        unit: obs_unit.into(),
        reference_range: String::new(),
        flag: String::new(),
        panel: String::new(),
        provider: String::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Import entrypoint.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // ---- Parse CSV ----------------------------------------------------------
    // Skip row 0 (preamble/metadata). Row 1 is the column header. Rows 2+ data.
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(false)     // we manage headers manually (two-row preamble)
        .flexible(true)         // rows may have different lengths
        .trim(csv::Trim::Fields)
        .from_reader(body.as_bytes());

    let mut all_rows: Vec<Vec<String>> = Vec::new();
    for result in rdr.records() {
        let rec = result.context("reading LibreView CSV")?;
        all_rows.push(rec.iter().map(|s| s.to_string()).collect());
    }

    if all_rows.len() < 2 {
        anyhow::bail!(
            "LibreView CSV has fewer than 2 rows — expected a metadata row + header row + data"
        );
    }

    // Row 0 = preamble/metadata (skip), row 1 = column headers, rows 2+ = data.
    let headers: Vec<String> = all_rows[1].iter().map(|s| s.trim().to_string()).collect();
    let data_rows = &all_rows[2..];

    // ---- Infer date ordering once for the whole file -----------------------
    // Must happen before any timestamp parse so that ambiguous dates like
    // "06-07-2026" (June 7 US vs July 6 EU) resolve correctly for every row.
    let date_order = infer_date_order(all_rows.first(), data_rows);

    // ---- Parse rows + collect glucose values for unit detection -------------
    let mut parsed: Vec<Row> = Vec::new();
    let mut glucose_values: Vec<f64> = Vec::new();
    let mut skipped = 0u64;

    for cols in data_rows {
        match parse_row(cols.clone(), date_order) {
            Some(row) => {
                if let Some(g) = row.glucose {
                    glucose_values.push(g);
                }
                parsed.push(row);
            }
            None => skipped += 1,
        }
    }

    // ---- Determine unit (mg/dL vs mmol/L) -----------------------------------
    let unit = detect_unit(&glucose_values);

    // ---- Dedupe against existing contract rows ------------------------------
    let contract = vault.stream(CONTRACT_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

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

    // ---- Write both layers --------------------------------------------------
    let mut obs_rows: Vec<Observation> = Vec::new();
    let mut raw_rows: Vec<RawLine> = Vec::new();
    let mut imported = 0u64;
    let mut duplicates = 0u64;
    let total = parsed.len();

    for (i, row) in parsed.into_iter().enumerate() {
        if seen.contains(&row.guid) {
            duplicates += 1;
            continue;
        }

        // Skip rows where the timestamp couldn't be parsed — they have no ts
        // to partition on and we cannot file them.
        if row.ts.is_none() {
            skipped += 1;
            continue;
        }

        seen.insert(row.guid.clone());

        if let Some(obs) = observation_from(&row, unit) {
            if let Some(raw) = raw_line(&row, &headers) {
                raw_rows.push(raw);
                obs_rows.push(obs);
                imported += 1;
            }
        }

        if (i + 1) % 500 == 0 {
            let pct = ((i + 1) as f64 / total as f64 * 90.0) as f32;
            progress(ImportProgress { records: imported, percent: pct });
        }
    }

    // Write contract rows, partitioned by month.
    contract.append(&obs_rows, |o| &o.ts)?;
    // Write raw rows, same monthly partition.
    raw_stream.append(&raw_rows, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} glucose readings imported, {duplicates} duplicates skipped"
        ),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)]
            .into(),
    })
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
            .join(format!("trove-freestyle-libre-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn do_import(vault: &Vault, csv: &str) -> ImportOutcome {
        let path = vault.root().join("libre.csv");
        fs::write(&path, csv).unwrap();
        (IMPORT.run)(vault, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // ---- fixtures drawn from the real LibreView CSV format ------------------
    // Confirmed against: philipp-1337/glucose-data-processor test file (German
    // locale, column positions), shrugalic/LibreView_to_AppleHealth_converter
    // (skips 2 rows, row[2]=timestamp, row[4]=historic, row[5]=scan),
    // RaunakMandal/Freestyle-Libre-Viewer (REQUIRED_COLUMNS + col positions),
    // Tidepool libreViewDriver.js (col index mapping, unit detection heuristic).

    /// A minimal English-locale LibreView CSV (mg/dL, 24h, DD-MM-YYYY).
    /// Row 0: preamble; Row 1: headers; Rows 2+: data.
    const CSV_MGDL_24H: &str = "\
Glucose Data,Created 16-06-2026 10:00 UTC,Created by,TestUser\r\n\
Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,Scan Glucose mg/dL,Non-numeric Rapid-Acting Insulin,Rapid-Acting Insulin (units),Non-numeric Food,Carbohydrates (grams),Carbohydrates (servings),Non-numeric Long-Acting Insulin,Long-Acting Insulin (units),Notes,Strip Glucose mg/dL,Ketone mmol/L,Meal Insulin (units),Correction Insulin (units),User Change Insulin (units)\r\n\
FreeStyle Libre 3,SN-ABCDEF123,10-06-2026 00:00,0,95,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-ABCDEF123,10-06-2026 00:15,0,98,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-ABCDEF123,10-06-2026 00:30,0,102,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-ABCDEF123,10-06-2026 01:00,1,,115,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-ABCDEF123,10-06-2026 02:00,2,,,,,,,,,,,88,,,,\r\n\
FreeStyle Libre 3,SN-ABCDEF123,10-06-2026 03:00,3,,,,,,,,,,,,0.3,,,\r\n\
";

    /// A mmol/L export (all glucose values < 40).
    const CSV_MMOL: &str = "\
Glucose Data,Created 16-06-2026 10:00 UTC,Created by,TestUser\r\n\
Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mmol/L,Scan Glucose mmol/L,Non-numeric Rapid-Acting Insulin,Rapid-Acting Insulin (units),Non-numeric Food,Carbohydrates (grams),Carbohydrates (servings),Non-numeric Long-Acting Insulin,Long-Acting Insulin (units),Notes,Strip Glucose mmol/L,Ketone mmol/L,Meal Insulin (units),Correction Insulin (units),User Change Insulin (units)\r\n\
FreeStyle Libre 3,SN-ZZZZZ999,10-06-2026 06:00,0,5.4,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-ZZZZZ999,10-06-2026 06:15,0,5.8,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-ZZZZZ999,10-06-2026 06:30,1,,6.2,,,,,,,,,,,,,\r\n\
";

    /// Export with 12-hour timestamps (MM-DD-YYYY HH:MM AM/PM format — US locale).
    const CSV_12H_AMPM: &str = "\
Glucose Data,Created 06-16-2026 10:00 UTC,Created by,UserABC\r\n\
Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,Scan Glucose mg/dL,Non-numeric Rapid-Acting Insulin,Rapid-Acting Insulin (units),Non-numeric Food,Carbohydrates (grams),Carbohydrates (servings),Non-numeric Long-Acting Insulin,Long-Acting Insulin (units),Notes,Strip Glucose mg/dL,Ketone mmol/L,Meal Insulin (units),Correction Insulin (units),User Change Insulin (units)\r\n\
FreeStyle Libre 2,SN-111AAA,06-15-2026 11:30 AM,0,110,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 2,SN-111AAA,06-15-2026 11:45 AM,1,,105,,,,,,,,,,,,,\r\n\
";

    // ---- unit detection -----------------------------------------------------

    #[test]
    fn detects_mgdl_when_any_value_at_or_above_40() {
        assert_eq!(detect_unit(&[5.5, 39.9, 40.0, 7.2]), "mg/dL");
        assert_eq!(detect_unit(&[95.0, 120.0, 80.0]), "mg/dL");
    }

    #[test]
    fn detects_mmol_when_all_values_below_40() {
        assert_eq!(detect_unit(&[4.2, 5.8, 7.1, 10.0]), "mmol/L");
        assert_eq!(detect_unit(&[]), "mmol/L"); // default (no values)
    }

    // ---- timestamp parsing --------------------------------------------------

    #[test]
    fn parses_dd_mm_yyyy_24h_format() {
        let dt = parse_device_timestamp("10-06-2026 00:15", DateOrder::DayFirst).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-06-10 00:15");
    }

    #[test]
    fn parses_mm_dd_yyyy_ampm_format() {
        let dt = parse_device_timestamp("06-15-2026 11:30 AM", DateOrder::MonthFirst).unwrap();
        assert_eq!(dt.format("%Y-%m-%d %H:%M").to_string(), "2026-06-15 11:30");

        let dt_pm = parse_device_timestamp("06-15-2026 01:45 PM", DateOrder::MonthFirst).unwrap();
        assert_eq!(dt_pm.format("%H:%M").to_string(), "13:45");
    }

    #[test]
    fn empty_or_garbage_timestamp_returns_none() {
        assert!(parse_device_timestamp("", DateOrder::Unknown).is_none());
        assert!(parse_device_timestamp("not-a-time", DateOrder::Unknown).is_none());
    }

    // ---- date-order inference -----------------------------------------------

    #[test]
    fn infer_order_from_preamble_day_gt_12() {
        // "Created 16-06-2026 ..." → first field 16 > 12 → DayFirst
        let preamble = vec![
            "Glucose Data".to_string(),
            "Created 16-06-2026 10:00 UTC".to_string(),
        ];
        let order = infer_date_order(Some(&preamble), &[]);
        assert_eq!(order, DateOrder::DayFirst, "16 in day position => DayFirst");
    }

    #[test]
    fn infer_order_from_preamble_month_first_us() {
        // "Created 06-16-2026 ..." → second field 16 > 12 → MonthFirst
        let preamble = vec![
            "Glucose Data".to_string(),
            "Created 06-16-2026 10:00 UTC".to_string(),
        ];
        let order = infer_date_order(Some(&preamble), &[]);
        assert_eq!(order, DateOrder::MonthFirst, "16 in month-day position => MonthFirst");
    }

    #[test]
    fn infer_order_from_data_row_when_preamble_ambiguous() {
        // Preamble date "01-01-2026" is all-ones — ambiguous.
        // Data row has day=20 → DayFirst.
        let preamble = vec![
            "Glucose Data".to_string(),
            "Created 01-01-2026 00:00 UTC".to_string(),
        ];
        let data: Vec<Vec<String>> = vec![vec![
            "FreeStyle Libre 3".to_string(),
            "SN-X".to_string(),
            "20-06-2026 08:00".to_string(), // first field 20 > 12 → DayFirst
            "0".to_string(),
            "100".to_string(),
        ]];
        let order = infer_date_order(Some(&preamble), &data);
        assert_eq!(order, DateOrder::DayFirst);
    }

    #[test]
    fn ambiguous_us_date_parses_correctly_with_month_first_order() {
        // The reported defect: "06-07-2026 09:00" with DayFirst order (wrong)
        // silently gives 2026-07-06 (July 6).  With MonthFirst it correctly
        // gives 2026-06-07 (June 7).
        let wrong = parse_device_timestamp("06-07-2026 09:00", DateOrder::DayFirst).unwrap();
        assert_eq!(
            wrong.format("%Y-%m-%d").to_string(),
            "2026-07-06",
            "DayFirst makes the ambiguous US date wrong (July 6)"
        );

        let correct = parse_device_timestamp("06-07-2026 09:00", DateOrder::MonthFirst).unwrap();
        assert_eq!(
            correct.format("%Y-%m-%d").to_string(),
            "2026-06-07",
            "MonthFirst makes the ambiguous US date correct (June 7)"
        );
    }

    // ---- guid construction --------------------------------------------------

    #[test]
    fn guid_is_serial_timestamp_recordtype() {
        let cols: Vec<String> = vec![
            "FreeStyle Libre 3".into(),
            "SN-ABCDEF123".into(),
            "10-06-2026 00:00".into(),
            "0".into(),
            "95".into(),
            "".into(),
            "".into(), "".into(), "".into(), "".into(), "".into(), "".into(), "".into(),
            "".into(), "".into(), "".into(), "".into(), "".into(), "".into(),
        ];
        let row = parse_row(cols, DateOrder::DayFirst).unwrap();
        assert_eq!(row.guid, "SN-ABCDEF123|10-06-2026 00:00|0");
    }

    // ---- parse_row ----------------------------------------------------------

    #[test]
    fn parse_row_extracts_historic_glucose() {
        let cols: Vec<String> = "FreeStyle Libre 3,SN-ABC,10-06-2026 00:00,0,95,,,,,,,,,,,,,,,"
            .split(',')
            .map(Into::into)
            .collect();
        let row = parse_row(cols, DateOrder::DayFirst).unwrap();
        assert_eq!(row.record_type, RECORD_HISTORIC);
        assert_eq!(row.glucose, Some(95.0));
        assert!(row.ketone.is_none());
        assert_eq!(row.device, "FreeStyle Libre 3");
        assert_eq!(row.serial, "SN-ABC");
    }

    #[test]
    fn parse_row_extracts_scan_glucose() {
        let mut cols: Vec<String> = vec!["FreeStyle Libre 3".into(), "SN-XYZ".into(),
            "10-06-2026 01:00".into(), "1".into(), "".into(), "115".into()];
        cols.extend(std::iter::repeat_n("".to_string(), 13));
        let row = parse_row(cols, DateOrder::DayFirst).unwrap();
        assert_eq!(row.record_type, RECORD_SCAN);
        assert_eq!(row.glucose, Some(115.0));
    }

    #[test]
    fn parse_row_extracts_strip_glucose() {
        let mut cols: Vec<String> = vec!["FreeStyle Libre 3".into(), "SN-AAA".into(),
            "10-06-2026 02:00".into(), "2".into()];
        cols.extend(std::iter::repeat_n("".to_string(), 10)); // cols 4-13 empty
        cols.push("88".into()); // col 14 = strip
        cols.push("".into());   // col 15 = ketone
        cols.extend(std::iter::repeat_n("".to_string(), 3));
        let row = parse_row(cols, DateOrder::DayFirst).unwrap();
        assert_eq!(row.record_type, RECORD_STRIP);
        assert_eq!(row.glucose, Some(88.0));
    }

    #[test]
    fn parse_row_extracts_ketone() {
        let mut cols: Vec<String> = vec!["FreeStyle Libre 3".into(), "SN-BBB".into(),
            "10-06-2026 03:00".into(), "3".into()];
        cols.extend(std::iter::repeat_n("".to_string(), 11)); // cols 4-14 empty
        cols.push("0.3".into()); // col 15 = ketone
        cols.extend(std::iter::repeat_n("".to_string(), 3));
        let row = parse_row(cols, DateOrder::DayFirst).unwrap();
        assert_eq!(row.record_type, RECORD_KETONE);
        assert!(row.glucose.is_none());
        assert_eq!(row.ketone, Some(0.3));
    }

    #[test]
    fn parse_row_skips_fully_blank_row() {
        let cols = vec!["".to_string(), "".to_string(), "".to_string(), "".to_string()];
        assert!(parse_row(cols, DateOrder::Unknown).is_none());
    }

    #[test]
    fn parse_row_skips_non_cgm_record_types() {
        // Record type 4+ (insulin/notes only) has neither glucose nor ketone.
        let cols: Vec<String> = vec!["FreeStyle Libre 3".into(), "SN-CCC".into(),
            "10-06-2026 04:00".into(), "4".into(),
            "".into(), "".into()];
        assert!(parse_row(cols, DateOrder::DayFirst).is_none());
    }

    #[test]
    fn comma_decimal_separator_is_tolerated() {
        // Continental European locales export 5,4 instead of 5.4.
        assert_eq!(parse_f64("5,4"), Some(5.4));
        assert_eq!(parse_f64("5.4"), Some(5.4));
        assert_eq!(parse_f64(""), None);
    }

    // ---- observation mapping -----------------------------------------------

    #[test]
    fn observation_from_maps_glucose_to_loinc() {
        let row = Row {
            raw_cols: vec![],
            guid: "SN-X|ts|0".into(),
            ts: Some("2026-06-10T00:00:00+01:00".into()),
            record_type: RECORD_HISTORIC,
            glucose: Some(95.0),
            ketone: None,
            device: "FreeStyle Libre 3".into(),
            serial: "SN-X".into(),
            ts_raw: "10-06-2026 00:00".into(),
        };
        let obs = observation_from(&row, "mg/dL").unwrap();
        assert_eq!(obs.source, "freestyle-libre");
        assert_eq!(obs.guid, "SN-X|ts|0");
        assert_eq!(obs.test, "Glucose");
        assert_eq!(obs.code, "2339-0");
        assert_eq!(obs.code_system, "loinc");
        assert_eq!(obs.value, Some(95.0));
        assert_eq!(obs.unit, "mg/dL");
        assert_eq!(obs.extra.get("recordType"), Some(&serde_json::json!("historic")));
        assert_eq!(obs.extra.get("device"), Some(&serde_json::json!("FreeStyle Libre 3")));
        assert_eq!(obs.extra.get("serialNumber"), Some(&serde_json::json!("SN-X")));
    }

    #[test]
    fn observation_from_maps_ketone_to_ketone_loinc() {
        let row = Row {
            raw_cols: vec![],
            guid: "SN-K|ts|3".into(),
            ts: Some("2026-06-10T03:00:00+00:00".into()),
            record_type: RECORD_KETONE,
            glucose: None,
            ketone: Some(0.3),
            device: "FreeStyle Libre 3".into(),
            serial: "SN-K".into(),
            ts_raw: "10-06-2026 03:00".into(),
        };
        let obs = observation_from(&row, "mg/dL").unwrap();
        assert_eq!(obs.test, "Ketone");
        assert_eq!(obs.code, "2514-8");
        assert_eq!(obs.unit, "mmol/L", "ketone is always mmol/L");
        assert_eq!(obs.value, Some(0.3));
    }

    #[test]
    fn observation_from_returns_none_when_ts_is_none() {
        let row = Row {
            raw_cols: vec![],
            guid: "x".into(),
            ts: None, // unparseable timestamp
            record_type: RECORD_HISTORIC,
            glucose: Some(100.0),
            ketone: None,
            device: "".into(),
            serial: "SN-X".into(),
            ts_raw: "bad-ts".into(),
        };
        assert!(observation_from(&row, "mg/dL").is_none());
    }

    // ---- full import integration tests -------------------------------------

    #[test]
    fn imports_mgdl_csv_to_both_layers_and_reports_counts() {
        let v = temp_vault("mgdl");
        let out = do_import(&v, CSV_MGDL_24H);

        // 3 historic + 1 scan + 1 strip + 1 ketone = 6 rows imported.
        assert_eq!(out.counts.get("imported"), Some(&6), "all rows: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Contract layer: observations partitioned by month.
        let obs_path = v.root().join("health/medical/freestyle-libre/observations/2026-06.jsonl");
        let obs = fs::read_to_string(&obs_path).unwrap();
        assert_eq!(obs.lines().count(), 6, "6 contract rows in 2026-06.jsonl");

        // Glucose readings carry the glucose LOINC.
        assert!(obs.contains("\"code\":\"2339-0\""), "glucose LOINC in contract");
        // Ketone readings carry the ketone LOINC.
        assert!(obs.contains("\"code\":\"2514-8\""), "ketone LOINC in contract");
        // Units correct: mg/dL for glucose, mmol/L for ketone.
        assert!(obs.contains("\"unit\":\"mg/dL\""), "mg/dL unit on glucose rows");
        assert!(obs.contains("\"unit\":\"mmol/L\""), "mmol/L unit on ketone row");
        // Source is always "freestyle-libre".
        assert!(obs.contains("\"source\":\"freestyle-libre\""));

        // Raw layer: full-fidelity CSV columns as JSON.
        let raw_path = v.root().join("health/freestyle-libre/raw/2026-06.jsonl");
        let raw = fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw.lines().count(), 6, "6 raw rows");
        assert!(raw.contains("FreeStyle Libre 3"), "device in raw");
        assert!(raw.contains("SN-ABCDEF123"), "serial in raw");
    }

    #[test]
    fn detects_mmol_unit_and_uses_it() {
        let v = temp_vault("mmol");
        let out = do_import(&v, CSV_MMOL);
        assert_eq!(out.counts.get("imported"), Some(&3));

        let obs = fs::read_to_string(
            v.root().join("health/medical/freestyle-libre/observations/2026-06.jsonl"),
        )
        .unwrap();
        // Unit should be mmol/L (all values < 40).
        assert!(obs.contains("\"unit\":\"mmol/L\""), "mmol/L detected: {obs}");
        assert!(!obs.contains("\"unit\":\"mg/dL\""), "no mg/dL when mmol detected");
    }

    #[test]
    fn imports_12h_ampm_timestamps() {
        let v = temp_vault("ampm");
        let out = do_import(&v, CSV_12H_AMPM);
        assert_eq!(out.counts.get("imported"), Some(&2), "2 rows: {out:?}");

        let obs = fs::read_to_string(
            v.root().join("health/medical/freestyle-libre/observations/2026-06.jsonl"),
        )
        .unwrap();
        // The 11:30 AM timestamp should parse correctly.
        assert!(obs.contains("2026-06-15"), "date in contract ts: {obs}");
        assert!(obs.contains("11:30"), "time preserved: {obs}");
    }

    #[test]
    fn reimport_is_idempotent_no_duplicates() {
        let v = temp_vault("rerun");
        let first = do_import(&v, CSV_MGDL_24H);
        let before = fs::read_to_string(
            v.root().join("health/medical/freestyle-libre/observations/2026-06.jsonl"),
        )
        .unwrap();

        // Re-import same CSV — all duplicates, nothing new.
        let second = do_import(&v, CSV_MGDL_24H);
        assert_eq!(second.counts.get("imported"), Some(&0), "no new rows on re-import");
        assert_eq!(second.counts.get("duplicates"), Some(&6));

        let after = fs::read_to_string(
            v.root().join("health/medical/freestyle-libre/observations/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(before, after, "observation file byte-identical after re-import");
        let _ = first;
    }

    #[test]
    fn two_different_serials_dont_collide_guids() {
        // Two sensors reporting the same timestamp must produce distinct guids.
        let csv = format!(
            "{}{}{}",
            "Metadata\r\n",
            "Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,Scan Glucose mg/dL,,,,,,,,,,,,,,\r\n",
            "FreeStyle Libre 3,SN-AAA,10-06-2026 00:00,0,95,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 3,SN-BBB,10-06-2026 00:00,0,97,,,,,,,,,,,,,,\r\n"
        );
        let v = temp_vault("twosensors");
        let out = do_import(&v, &csv);
        assert_eq!(out.counts.get("imported"), Some(&2), "both sensors land: {out:?}");
    }

    #[test]
    fn observation_contract_fields_round_trip() {
        let v = temp_vault("roundtrip");
        do_import(&v, CSV_MGDL_24H);

        let contract = vault_stream_obs(&v);
        let first = contract.first().unwrap();
        let back: Observation = serde_json::from_value(serde_json::to_value(first).unwrap()).unwrap();
        assert_eq!(back.source, "freestyle-libre");
        assert!(!back.guid.is_empty());
        assert_eq!(back.code_system, "loinc");
    }

    fn vault_stream_obs(v: &Vault) -> Vec<serde_json::Value> {
        let stream = v.stream(CONTRACT_DIR, Partition::Month);
        let mut all = Vec::new();
        for key in stream.partitions().unwrap() {
            all.extend(stream.read::<serde_json::Value>(&key).unwrap());
        }
        all
    }

    #[test]
    fn hub_card_shows_import_box_and_last_data() {
        let v = temp_vault("hub");
        do_import(&v, CSV_MGDL_24H);

        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "freestyle-libre").unwrap();
        let imp = card.import.as_ref().expect("import box on freestyle-libre card");
        assert_eq!(imp.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2026-06"), "last_data set after import");
    }

    #[test]
    fn short_file_is_an_error() {
        let v = temp_vault("short");
        let path = v.root().join("short.csv");
        fs::write(&path, "Only one row\n").unwrap();
        let err = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("fewer than 2 rows"), "clear error on short file: {err}");
    }

    /// Regression test for the ambiguous US-locale date defect.
    ///
    /// An export where ALL Device Timestamps have day ≤ 12 and month ≤ 12
    /// (e.g. "06-07-2026" = June 7 in MM-DD-YYYY) must be filed as June 7, not
    /// July 6.  Without whole-file date-order inference the day-first default
    /// silently produces the wrong month AND day for ~40% of a dense export.
    /// The fix: the preamble "Created 06-17-2026 ..." carries second field 17
    /// > 12 → MonthFirst, which routes "06-07-2026" to 2026-06-07 (correct).
    #[test]
    fn us_locale_ambiguous_dates_use_preamble_to_pick_month_first() {
        // All timestamps have day ≤ 12 AND month ≤ 12 → impossible to resolve
        // per-row.  The preamble carries "Created 06-17-2026" → second=17 > 12
        // → MonthFirst.
        let csv = "\
Glucose Data,Created 06-17-2026 10:00 UTC,Created by,TestUser\r\n\
Device,Serial Number,Device Timestamp,Record Type,Historic Glucose mg/dL,Scan Glucose mg/dL,Non-numeric Rapid-Acting Insulin,Rapid-Acting Insulin (units),Non-numeric Food,Carbohydrates (grams),Carbohydrates (servings),Non-numeric Long-Acting Insulin,Long-Acting Insulin (units),Notes,Strip Glucose mg/dL,Ketone mmol/L,Meal Insulin (units),Correction Insulin (units),User Change Insulin (units)\r\n\
FreeStyle Libre 2,SN-US001,06-07-2026 09:00,0,110,,,,,,,,,,,,,,\r\n\
FreeStyle Libre 2,SN-US001,06-07-2026 09:15,0,112,,,,,,,,,,,,,,\r\n\
";
        let v = temp_vault("ambiguous-us");
        let out = do_import(&v, csv);
        assert_eq!(out.counts.get("imported"), Some(&2), "both rows imported: {out:?}");

        // The observations must be filed in 2026-06, NOT 2026-07.
        let june_path =
            v.root().join("health/medical/freestyle-libre/observations/2026-06.jsonl");
        let july_path =
            v.root().join("health/medical/freestyle-libre/observations/2026-07.jsonl");

        assert!(june_path.exists(), "readings landed in 2026-06 (June) as expected");
        assert!(!july_path.exists(), "no 2026-07 file — month was NOT misread as day");

        let obs = fs::read_to_string(&june_path).unwrap();
        // ts must show 2026-06-07, not 2026-07-06.
        assert!(
            obs.contains("2026-06-07"),
            "observation ts shows June 7 (MM-DD correct): {obs}"
        );
        assert!(
            !obs.contains("2026-07-06"),
            "observation ts must NOT show July 6 (the day-first bug): {obs}"
        );
    }
}
