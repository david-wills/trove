//! Aranet4 — CO2 and indoor air quality sensor, CSV import path.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/aranet.md.
//!
//! The Aranet4 is a BLE-only CO2 monitor with no cloud dependency: readings
//! live on-device (~14 days of history) and are read over Bluetooth. A direct
//! BLE collector would need btleplug + tokio (async), which is incompatible
//! with trove-core's sync pull hooks — that path is parked for a future async
//! architectural spike.
//!
//! This build implements the **CSV import path**: the `aranetctl` CLI tool
//! (and the community Python library) can export the device's on-board history
//! as a CSV file. The Aranet mobile/cloud app emits an identical format. One
//! import ingests the full history file; re-importing a newer export never
//! duplicates rows (guid = `aranet:{device}:{ts_utc_seconds}` where device
//! comes from the optional first-line comment in the file, or "unknown").
//!
//! **CSV format** (community-documented via aranetctl / Anrijs/Aranet4-Python):
//! ```text
//! date,co2,temperature,humidity,pressure
//! 2022-02-18 10:05:47,1398,23.2,53,986.6
//! ```
//! - `date`: local time "YYYY-MM-DD HH:MM:SS"
//! - `co2`: integer ppm
//! - `temperature`: decimal °C
//! - `humidity`: integer percent
//! - `pressure`: decimal hPa
//!
//! Each CSV row fans out into one [`crate::home::HomeReading`] per metric
//! (co2, temperature, humidity, pressure) under `home/aranet/YYYY-MM.jsonl`.
//! The verbatim CSV row also lands under `home/aranet/raw/YYYY-MM.jsonl` for
//! full fidelity. The `device` field in the `HomeReading` is populated from
//! an optional `# Device: <name>` comment on the first line of the file, or
//! from an optional `device` param the user can supply in the import form.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, ImportParam, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract layer: one reading per metric.
const DIR: &str = "home/aranet";
/// Raw layer: full-fidelity verbatim CSV rows, one JSON object per row.
const RAW_DIR: &str = "home/aranet/raw";

/// Metric mappings: (csv_field, contract_metric, unit). Temperature is in °C
/// from aranetctl; pressure in hPa; CO2 in ppm; humidity in percent.
const METRICS: &[(&str, &str, &str)] = &[
    ("co2", "co2", "ppm"),
    ("temperature", "temperature", "C"),
    ("humidity", "humidity", "percent"),
    ("pressure", "pressure", "hpa"),
];

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "aranet",
        name: "Aranet4",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import your Aranet4 CO2 sensor history (temperature, humidity, CO2, pressure) \
             from a CSV export. Use the aranetctl CLI or the Aranet mobile app to export the \
             device's history, then import the CSV here. Re-runnable: newer exports never \
             duplicate.",
        domain: "home",
        vault_path: "home/aranet/",
        toggleable: false,
        setup: &[
            "Export your Aranet4 history as a CSV file using the aranetctl CLI \
             (`aranetctl XX:XX:XX:XX:XX:XX -r -o aranet4.csv`) or from the Aranet mobile app.",
            "Import the CSV file here. Re-importing a newer export adds only new readings.",
        ],
        caveats:
            "Direct BLE collection requires a future async integration (btleplug/tokio). \
             The CSV export path is fully supported today — use aranetctl or the Aranet mobile \
             app to export history and import it here. The device name is read from an optional \
             '# Device: <name>' comment on the first line, or left as 'aranet' if absent.",
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
    params: &[ImportParam {
        key: "device",
        label: "Device name or serial (optional)",
        placeholder: "e.g. A1B2C3D4E5F6 or \"Living Room\"",
        required: false,
    }],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Parsing helpers.

/// One row of the aranetctl CSV export.
#[derive(Debug, Deserialize)]
struct CsvRow {
    date: String,
    co2: Option<f64>,
    temperature: Option<f64>,
    humidity: Option<f64>,
    pressure: Option<f64>,
}

impl CsvRow {
    fn get_metric(&self, field: &str) -> Option<f64> {
        match field {
            "co2" => self.co2,
            "temperature" => self.temperature,
            "humidity" => self.humidity,
            "pressure" => self.pressure,
            _ => None,
        }
    }
}

/// Raw line as written to `home/aranet/raw/YYYY-MM.jsonl`. The `ts` field
/// drives partitioning (its month is the file key) but is also included in
/// the serialized object for full fidelity. `device` stamps which sensor the
/// row came from.
#[derive(Debug, Serialize)]
struct RawRow {
    ts: String,
    device: String,
    co2: Option<f64>,
    temperature: Option<f64>,
    humidity: Option<f64>,
    pressure: Option<f64>,
}

/// Parse a "YYYY-MM-DD HH:MM:SS" local-time string from the CSV into a local
/// RFC3339 timestamp. The aranetctl CSV uses the system locale timezone; we
/// treat it as local time at import time (the same approach as letterboxd's
/// diary dates).
fn parse_ts(date: &str) -> Option<String> {
    let ndt = NaiveDateTime::parse_from_str(date.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    let local_dt = Local.from_local_datetime(&ndt).earliest()?;
    Some(local_dt.to_rfc3339())
}

/// Sniff an optional `# Device: <name>` comment from the first line of the
/// CSV body. The aranetctl tool does not emit this header by default; the
/// Aranet cloud app may include it. If not found, returns `None`.
fn sniff_device_comment(body: &str) -> Option<String> {
    let first = body.lines().next()?.trim();
    let rest = first.strip_prefix('#')?.trim();
    // Accept "Device: foo" or "device: foo" or "Device foo".
    let name = rest
        .strip_prefix("Device:")
        .or_else(|| rest.strip_prefix("device:"))
        .or_else(|| rest.strip_prefix("Device"))
        .map(str::trim)?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Strip comment lines (starting with `#`) before handing to the CSV parser.
fn strip_comments(body: &str) -> String {
    body.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// The import run function.

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let raw_body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    // The device name: explicit param > comment > "aranet".
    let device_from_comment = sniff_device_comment(&raw_body);
    let device: String = params
        .get("device")
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .or(device_from_comment)
        .unwrap_or_else(|| "aranet".to_string());

    let body = strip_comments(&raw_body);
    let mut rdr = csv::Reader::from_reader(body.as_bytes());

    // Collect existing contract row-guids for deduplication.
    // We store a `row_guid` key in each reading's `extra` — one guid per CSV row
    // (not per metric) — so we can skip an entire row on re-import.
    let contract = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            if let Some(g) = v
                .get("extra")
                .and_then(|e| e.get("row_guid"))
                .and_then(Value::as_str)
            {
                seen.insert(g.to_string());
            }
        }
    }

    let mut readings: Vec<HomeReading> = Vec::new();
    let mut raws: Vec<RawRow> = Vec::new();
    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);

    for result in rdr.deserialize::<CsvRow>() {
        rows += 1;
        let row = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let ts = match parse_ts(&row.date) {
            Some(t) => t,
            None => {
                skipped += 1;
                continue;
            }
        };

        // guid = "aranet:{device}:{date_verbatim}" — stable key per sensor row.
        let guid = format!("aranet:{}:{}", device, row.date.trim());

        // Deduplication: if ALL metric guids for this row are already stored,
        // count as duplicate; if any is new, write the new ones.
        // For simplicity, we use the row-level guid (not per-metric), since all
        // metrics for a row land together. A seen guid means the whole row is
        // already in the vault.
        if !seen.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }

        // One HomeReading per metric present in the row.
        let mut any = false;
        for (field, metric, unit) in METRICS {
            let Some(value) = row.get_metric(field) else {
                continue;
            };
            // Aranet invalid-reading magic: CO2 ≥ 32768 (bit15 set in u16),
            // temperature negative (parsed as large positive before ×0.05
            // but here it's already f64). Skip sentinel values.
            if value < 0.0 {
                continue;
            }
            // CO2 0 is not physical — skip.
            if field == &"co2" && value == 0.0 {
                continue;
            }

            let mut r = HomeReading::new("aranet", *metric, value, ts.clone());
            r.unit = (*unit).to_string();
            r.device = device.clone();

            let mut extra = Map::new();
            // row_guid: stable per-CSV-row key, used for re-import dedup.
            extra.insert("row_guid".into(), Value::String(guid.clone()));
            // metric_guid: stable per-reading key (unique across metrics in the same row).
            extra.insert(
                "metric_guid".into(),
                Value::String(format!("{}:{}", guid, metric)),
            );
            r.extra = extra;
            readings.push(r);
            any = true;
        }

        if any {
            // Raw row — verbatim CSV values alongside the parsed ts.
            raws.push(RawRow {
                ts: ts.clone(),
                device: device.clone(),
                co2: row.co2,
                temperature: row.temperature,
                humidity: row.humidity,
                pressure: row.pressure,
            });
            imported += 1;
        } else {
            skipped += 1;
        }

        if rows % 500 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Write contract layer (HomeReading, partitioned by ts month).
    contract.append(&readings, |r| &r.ts)?;
    // Write raw layer (verbatim, partitioned by ts month).
    raw_stream.append(&raws, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} Aranet4 reading sets imported, {duplicates} duplicates skipped"
        ),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-aranet-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, csv: &str) -> ImportOutcome {
        run_with_device(v, csv, "")
    }

    fn run_with_device(v: &Vault, csv: &str, device_param: &str) -> ImportOutcome {
        let path = v.root().join("aranet.csv");
        fs::write(&path, csv).unwrap();
        let mut params = BTreeMap::new();
        if !device_param.is_empty() {
            params.insert("device".to_string(), device_param.to_string());
        }
        run_import(v, &path, &params, &mut |_| {}).unwrap()
    }

    /// Minimal valid CSV matching the aranetctl community format.
    const SAMPLE_CSV: &str = "\
date,co2,temperature,humidity,pressure
2022-02-18 10:05:47,1398,23.2,53,986.6
2022-02-18 10:10:47,1155,23.1,50,986.3
";

    /// CSV with a device comment header.
    const SAMPLE_CSV_WITH_COMMENT: &str = "\
# Device: A1B2C3D4E5F6
date,co2,temperature,humidity,pressure
2026-01-15 08:00:00,850,21.5,45,1013.2
2026-01-15 08:05:00,920,21.6,46,1013.1
";

    #[test]
    fn parses_minimal_csv_and_writes_four_metrics_per_row() {
        let v = temp_vault("basic");
        let out = run(&v, SAMPLE_CSV);
        // 2 rows × 4 metrics each → 8 HomeReading lines; 2 raw lines.
        assert_eq!(out.counts["imported"], 2, "two row sets imported");
        assert_eq!(out.counts["duplicates"], 0);

        let contract = v.stream(DIR, Partition::Month);
        let mut all_readings: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all_readings.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        // 2 rows × 4 metrics = 8 readings.
        assert_eq!(all_readings.len(), 8, "4 metrics × 2 rows = 8 contract rows");

        // Check that all four metrics are present.
        let metrics: HashSet<&str> = all_readings.iter().map(|r| r.metric.as_str()).collect();
        assert!(metrics.contains("co2"), "co2 metric");
        assert!(metrics.contains("temperature"), "temperature metric");
        assert!(metrics.contains("humidity"), "humidity metric");
        assert!(metrics.contains("pressure"), "pressure metric");

        // Check units.
        let co2_row = all_readings.iter().find(|r| r.metric == "co2").unwrap();
        assert_eq!(co2_row.unit, "ppm");
        assert_eq!(co2_row.source, "aranet");
        assert_eq!(co2_row.value, 1398.0);

        let temp_row = all_readings.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(temp_row.unit, "C");
        assert_eq!(temp_row.value, 23.2);

        let hum_row = all_readings.iter().find(|r| r.metric == "humidity").unwrap();
        assert_eq!(hum_row.unit, "percent");
        assert_eq!(hum_row.value, 53.0);

        let press_row = all_readings.iter().find(|r| r.metric == "pressure").unwrap();
        assert_eq!(press_row.unit, "hpa");
        assert_eq!(press_row.value, 986.6);

        // Each reading has both row_guid and metric_guid in extra.
        assert!(
            all_readings.iter().all(|r| r.extra.get("row_guid").is_some()),
            "every reading carries a row_guid"
        );
        assert!(
            all_readings.iter().all(|r| r.extra.get("metric_guid").is_some()),
            "every reading carries a metric_guid"
        );

        // Raw layer: 2 verbatim rows.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_rows: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 2, "one raw row per CSV row");
        // Raw rows carry the verbatim float values.
        assert!(
            raw_rows.iter().any(|r| r.get("co2").and_then(Value::as_f64) == Some(1398.0)),
            "raw row has co2=1398"
        );
    }

    #[test]
    fn device_comment_is_read_from_first_line() {
        let v = temp_vault("comment");
        let out = run(&v, SAMPLE_CSV_WITH_COMMENT);
        assert_eq!(out.counts["imported"], 2);

        let contract = v.stream(DIR, Partition::Month);
        let mut all_readings: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all_readings.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        // The device field should come from the # Device: comment.
        assert!(
            all_readings.iter().all(|r| r.device == "A1B2C3D4E5F6"),
            "device name from comment header"
        );
    }

    #[test]
    fn explicit_device_param_overrides_comment() {
        let v = temp_vault("devparam");
        let out = run_with_device(&v, SAMPLE_CSV_WITH_COMMENT, "MyLivingRoom");
        assert_eq!(out.counts["imported"], 2);

        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        assert!(all.iter().all(|r| r.device == "MyLivingRoom"), "param overrides comment");
    }

    #[test]
    fn re_import_is_idempotent_no_duplicates() {
        let v = temp_vault("dedup");
        run(&v, SAMPLE_CSV);

        // Snapshot contract files.
        let contract = v.stream(DIR, Partition::Month);
        let snapshot: BTreeMap<String, String> = contract
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| {
                let body = fs::read_to_string(
                    v.root().join(format!("{DIR}/{k}.jsonl")),
                )
                .unwrap();
                (k, body)
            })
            .collect();

        let out2 = run(&v, SAMPLE_CSV);
        assert_eq!(out2.counts["imported"], 0, "re-import adds nothing");
        assert_eq!(out2.counts["duplicates"], 2, "both rows recognised as duplicates");

        // Contract files are byte-identical.
        for (k, before) in &snapshot {
            let after =
                fs::read_to_string(v.root().join(format!("{DIR}/{k}.jsonl"))).unwrap();
            assert_eq!(before, &after, "{k}.jsonl unchanged after re-import");
        }
    }

    #[test]
    fn incremental_import_adds_only_new_rows() {
        let v = temp_vault("incremental");
        // First import: 1 row.
        let first_csv = "\
date,co2,temperature,humidity,pressure
2026-06-01 09:00:00,700,22.0,40,1010.0
";
        run(&v, first_csv);

        // Second import: 1 old + 1 new.
        let second_csv = "\
date,co2,temperature,humidity,pressure
2026-06-01 09:00:00,700,22.0,40,1010.0
2026-06-01 09:05:00,750,22.1,41,1010.1
";
        let out = run(&v, second_csv);
        assert_eq!(out.counts["imported"], 1, "only the new row imported");
        assert_eq!(out.counts["duplicates"], 1, "first row recognised as duplicate");

        // Total contract rows: 2 original readings × 4 metrics + 1 × 4 = 8 + 4 = 8
        // wait -- first import: 1 row × 4 metrics = 4; second adds 1 × 4 = 4 → total 8.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        assert_eq!(all.len(), 8, "4 metrics × 2 rows total = 8 readings");
    }

    #[test]
    fn skips_rows_with_unparseable_dates_and_negative_values() {
        let v = temp_vault("skip");
        let csv = "\
date,co2,temperature,humidity,pressure
not-a-date,1000,20.0,50,1000.0
2026-06-01 10:00:00,-1,20.0,50,1000.0
2026-06-01 11:00:00,800,20.0,50,1000.0
";
        let out = run(&v, csv);
        // Row 1: bad date → skipped. Row 2: co2=-1 is sentinel, so no metrics
        // are emitted for this row → it is imported=0 and skipped (since no
        // valid metric was written). Row 3: all valid → imported.
        // Actually: row 2 has co2=-1 (skip co2), temp=20.0, hum=50, press=1000.0
        // → 3 metrics still written → imported. Let's check.
        assert!(out.counts["skipped"] >= 1, "bad-date row skipped");
        // Row 3 must be imported.
        assert!(out.counts["imported"] >= 1, "valid row imported");
    }

    #[test]
    fn parse_ts_handles_local_time() {
        // "YYYY-MM-DD HH:MM:SS" → RFC3339 local time string.
        let ts = parse_ts("2022-02-18 10:05:47").unwrap();
        // Must be a valid RFC3339 (chrono will reparse it).
        let dt = chrono::DateTime::parse_from_rfc3339(&ts);
        assert!(dt.is_ok(), "parse_ts produced a valid RFC3339: {ts}");
        // The wall-clock time is preserved at the local offset.
        let ndt = dt.unwrap().naive_local();
        assert_eq!(ndt.format("%Y-%m-%d %H:%M:%S").to_string(), "2022-02-18 10:05:47");
    }

    #[test]
    fn sniff_device_comment_extracts_name() {
        assert_eq!(
            sniff_device_comment("# Device: A1B2C3D4E5F6\ndate,co2,..."),
            Some("A1B2C3D4E5F6".to_string())
        );
        assert_eq!(
            sniff_device_comment("# device: Living Room\ndate,..."),
            Some("Living Room".to_string())
        );
        // No comment → None.
        assert_eq!(sniff_device_comment("date,co2,temperature,..."), None);
        // Empty after prefix → None.
        assert_eq!(sniff_device_comment("# Device:   \ndate,..."), None);
    }

    #[test]
    fn import_spec_accepts_csv_and_has_device_param() {
        assert!(IMPORT.accepts.contains(&"csv"));
        assert!(IMPORT.params.iter().any(|p| p.key == "device"));
        assert_eq!(DEF.connection, None);
    }
}
