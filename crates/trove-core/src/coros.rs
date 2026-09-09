//! COROS GPS sports watch data — Import of per-activity `.fit` files exported
//! from the COROS app.
//!
//! COROS has a Cloud API but it is application-gated (business/partnership
//! model, not self-service). The dependable, no-auth path is the per-activity
//! FIT export from the COROS app: open a completed workout → share/export as
//! `.fit`. The same `fitparser` crate used by Garmin decodes it; COROS FIT
//! files follow the Flexible and Interoperable Data Transfer standard with
//! sport-specific developer fields for training load, running power, and
//! aerobic/anaerobic training effect.
//!
//! ## Vault mapping — the raw-only `health/coros/` document-domain
//!
//! `health/` is a per-source **raw shape** (like `gaming/`, `developer/`), not
//! a normalised contract: no binding, no `DOMAINS` entry, no schema validation.
//! Full fidelity is the contract — every decoded FIT message is preserved.
//!
//!   - **FIT activities → `health/coros/activities/YYYY-MM/<guid>.jsonl`**:
//!     one JSONL line per FIT data message (`file_id`, `session`, `lap`,
//!     `record` GPS/HR/power samples, `event`, `device_info`, developer fields
//!     carrying COROS-specific training metrics, …). The GPS route stays
//!     **embedded** in the activity stream — it is never split out to
//!     `location/` (taxonomy rule: the location view joins at read time).
//!     Partitioned by the activity's start month (UTC); the `guid` is derived
//!     from the FIT `file_id` `time_created` normalised to UTC, plus the
//!     `serial_number` (device).  Both the partition and the guid use UTC so
//!     they are machine-timezone-independent.
//!
//! ## Dedupe / re-import
//!
//! The guid is derived from the `file_id.time_created` UTC instant (plus
//! device serial); re-importing the same `.fit` file is a no-op regardless of
//! the machine timezone or DST state at import time.  A user can export an
//! activity multiple times without accreting duplicates.
//!
//! ## Privacy
//!
//! default-off / opt-in: full GPS traces are location trails. The import
//! copy carries the acknowledgement and notes that for iPhone users the
//! Apple Health integration already captures the basics; this adds full
//! workout telemetry and supports non-iPhone users.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::DateTime;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const ACTIVITIES_DIR: &str = "health/coros/activities";
const HEALTH_DIR: &str = "health/coros";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime_recursive(&vault.root().join(HEALTH_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "coros",
        name: "COROS",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import GPS workout data from COROS sports watches — full FIT telemetry \
             including training load, running power, HR, laps, and GPS routes. \
             Re-runnable: re-importing the same activity is a no-op.",
        domain: "health",
        vault_path: "health/coros/",
        toggleable: false,
        setup: &[
            "COROS app → open a completed workout → tap the share icon → Export as FIT file. \
             Import that .fit file here. Repeat for each activity you want to capture.",
            "Heads-up: activities include full GPS traces (location trails) — this source is \
             off by default; enabling it imports those trails into your vault.",
            "For iPhone users: Apple Health already receives the basic activity summary from \
             COROS. This import adds full GPS telemetry, running power, training load, and \
             other fields that never reach Apple Health, and covers non-iPhone users.",
            "The COROS Cloud API requires a partnership application and is not self-service \
             — the FIT export is the reliable path for individual users.",
        ],
        caveats:
            "Per-activity FIT export requires opening each workout individually in the COROS \
             app — there is no bulk export. GPS routes stay embedded in the activity record \
             (the location view joins them at read time). The COROS API is \
             application-gated and not available for self-service integrations.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["fit"],
    params: &[],
    run: run_import,
};

#[derive(Default)]
struct Stats {
    activities: u64,
    activity_records: u64,
    duplicates: u64,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let mut stats = Stats::default();
    let mut seen_activities = stored_activity_guids(vault)?;

    let bytes = std::fs::read(path).with_context(|| format!("opening {}", path.display()))?;
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
    import_fit(vault, &bytes, stem.as_deref(), &mut seen_activities, &mut stats)?;

    progress(ImportProgress { records: stats.activity_records, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{} activities ({} FIT records) imported, {} duplicates skipped",
            stats.activities, stats.activity_records, stats.duplicates,
        ),
        counts: [
            ("activities", stats.activities),
            ("activity_records", stats.activity_records),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

/// Decode one FIT file's bytes and write its messages as an activity stream.
/// `stem` is the source filename stem (used as a secondary id hint if the
/// file_id fields are absent — unlikely for a real COROS export, but lenient).
fn import_fit(
    vault: &Vault,
    bytes: &[u8],
    stem: Option<&str>,
    seen_activities: &mut HashSet<String>,
    stats: &mut Stats,
) -> Result<()> {
    // Propagate FIT parse errors so the user sees a real failure rather than a
    // misleading "0 activities imported" success.  Unlike garmin.rs (which loops
    // over many files inside a ZIP and can afford to skip a single corrupt entry),
    // COROS imports exactly one .fit file — swallowing its parse error would
    // return a green outcome with nothing written and no explanation.
    let records = fitparser::from_bytes(bytes)
        .with_context(|| format!("failed to parse FIT file: {}", stem.unwrap_or("<unknown>")))?;
    if records.is_empty() {
        return Ok(());
    }

    // Decode every message into (kind, fields-map) once; reused for guid
    // derivation, start-time partitioning, and JSONL serialisation.
    let decoded: Vec<(String, Map<String, Value>)> =
        records.iter().map(|r| (mesg_kind(r), fields_map(r))).collect();

    let guid = activity_guid(stem, &decoded);
    if !seen_activities.insert(guid.clone()) {
        stats.duplicates += 1;
        return Ok(());
    }

    // Partition by the activity's start month (local). Without a timestamp the
    // activity can't be placed on the timeline — skip it (and un-see the guid).
    let Some(start_ts) = activity_start_ts(&decoded) else {
        seen_activities.remove(&guid);
        return Ok(());
    };

    let mut lines: Vec<Value> = Vec::with_capacity(decoded.len());
    for (kind, fields) in &decoded {
        let mut row = Map::new();
        row.insert("guid".into(), Value::String(guid.clone()));
        row.insert("activity_start".into(), Value::String(start_ts.clone()));
        row.insert("source".into(), Value::String("coros".into()));
        row.insert("message".into(), Value::String(kind.clone()));
        row.insert("fields".into(), Value::Object(fields.clone()));
        lines.push(Value::Object(row));
    }

    let Some(month) = Partition::Month.key(&start_ts) else {
        seen_activities.remove(&guid);
        return Ok(());
    };
    let rel = format!("{ACTIVITIES_DIR}/{month}/{}.jsonl", activity_filename(&guid));
    write_jsonl(vault, &rel, &lines)?;

    stats.activities += 1;
    stats.activity_records += lines.len() as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// FIT decoding helpers (mirrors garmin.rs helpers for the same crate).

/// One FIT message's kind as the profile name string, or its numeric value for
/// messages the profile doesn't name — preserved, never dropped.
fn mesg_kind(rec: &fitparser::FitDataRecord) -> String {
    match serde_json::to_value(rec.kind()) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "unknown".into(),
    }
}

/// One FIT message's fields as a name→value map. Unknown fields keep their
/// generic name. COROS developer fields (training load, running power, aerobic/
/// anaerobic TE) ride through generically — nothing is dropped.
fn fields_map(rec: &fitparser::FitDataRecord) -> Map<String, Value> {
    let mut m = Map::new();
    for f in rec.fields() {
        let v = serde_json::to_value(f.value()).unwrap_or(Value::Null);
        m.insert(f.name().to_string(), v);
    }
    m
}

/// A stable activity guid for COROS FIT files.
///
/// COROS does not encode a numeric activity id in the filename (unlike Garmin).
/// The most stable key is the `file_id` message's `time_created` +
/// `serial_number` (device serial) — this pair is unique per recording session.
/// Falls back to the filename stem, then to the start timestamp.
///
/// The `time_created` component is **normalised to UTC** before composing the
/// guid, so the key is identical regardless of the machine timezone at import
/// time.  fitparser decodes FIT timestamps as `DateTime<Local>`, which serialises
/// to an RFC3339 string carrying the local offset; without normalisation the same
/// physical instant produces different guid strings across timezones (and even
/// across DST transitions), breaking the re-import no-op guarantee.
///
/// Note: `serial_number` may be the FIT invalid sentinel (`2147483647` /
/// `4294967295`) on devices that did not set it; the value is still included so
/// the component is always present, but uniqueness rests on `time_created` when
/// the serial is a sentinel.
fn activity_guid(stem: Option<&str>, decoded: &[(String, Map<String, Value>)]) -> String {
    if let Some((_, f)) = decoded.iter().find(|(k, _)| k == "file_id") {
        let raw_time = f.get("time_created").and_then(Value::as_str).unwrap_or("");
        let serial = f.get("serial_number").map(value_scalar_str).unwrap_or_default();
        // Normalise the timestamp to UTC so the guid is timezone-independent.
        // fitparser emits RFC3339 with the local offset (e.g. "2012-04-09T17:22:26-04:00");
        // we parse and re-format as the UTC instant ("2012-04-09T21:22:26Z").
        let time = normalise_ts_to_utc(raw_time).unwrap_or_else(|| raw_time.to_string());
        if !time.is_empty() || !serial.is_empty() {
            return format!("coros|{time}|{serial}");
        }
    }
    // Fallback to filename stem (not a stable id for COROS, but better than
    // nothing if file_id is absent).
    if let Some(s) = stem {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    activity_start_ts(decoded).unwrap_or_else(|| "coros-activity".into())
}

/// The activity's start timestamp normalised to UTC (RFC3339 with `Z` suffix),
/// for month-partitioning.
///
/// Timestamps are normalised to UTC so that the month partition key is
/// machine-timezone-independent — an activity near a month boundary must land
/// in the same partition regardless of where the vault lives.
fn activity_start_ts(decoded: &[(String, Map<String, Value>)]) -> Option<String> {
    let field = |kind: &str, key: &str| {
        decoded
            .iter()
            .find(|(k, _)| k == kind)
            .and_then(|(_, f)| f.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    let raw = field("session", "start_time")
        .or_else(|| field("file_id", "time_created"))
        .or_else(|| {
            decoded.iter().find_map(|(_, f)| {
                f.get("timestamp").and_then(Value::as_str).map(str::to_string)
            })
        })?;
    // Normalise to UTC; fall back to the raw string if parsing fails.
    Some(normalise_ts_to_utc(&raw).unwrap_or(raw))
}

// ---------------------------------------------------------------------------
// Dedupe set + write helpers.

/// Every activity guid already stored under health/coros/activities/. Reading
/// the stored `guid` field from the first line of each activity file.
fn stored_activity_guids(vault: &Vault) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let root = vault.resolve(ACTIVITIES_DIR)?;
    let Ok(months) = std::fs::read_dir(&root) else {
        return Ok(out);
    };
    for month in months.flatten() {
        if !month.path().is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(month.path()) else { continue };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().is_some_and(|x| x == "jsonl") {
                if let Ok(body) = std::fs::read_to_string(&p) {
                    if let Some(first) = body.lines().find(|l| !l.trim().is_empty()) {
                        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(first) {
                            if let Some(g) = obj.get("guid").and_then(Value::as_str) {
                                out.insert(g.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Write a JSONL file for one activity's messages (one activity = one file).
/// Atomic so a re-import upsert can't tear the file.
fn write_jsonl(vault: &Vault, rel: &str, rows: &[Value]) -> Result<()> {
    let path = vault.resolve(rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    for r in rows {
        body.push_str(&serde_json::to_string(r)?);
        body.push('\n');
    }
    crate::store::write_atomic(&path, body.as_bytes())
}

// ---------------------------------------------------------------------------
// Small path/value utilities.

/// A filesystem-safe form of a guid for use as a filename.
fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "activity".into() } else { s }
}

/// The on-disk filename stem for an activity. Appends a short hash when
/// sanitization changes the guid (preventing collisions between guids that
/// happen to sanitize identically).
fn activity_filename(guid: &str) -> String {
    let clean = sanitize(guid);
    if clean == guid { clean } else { format!("{clean}-{}", short_hash(guid)) }
}

/// First 8 hex chars of a DefaultHasher of the input.
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// Parse an RFC3339 timestamp and return it normalised to UTC (`Z` suffix).
/// Returns `None` if parsing fails (caller falls back to the raw string).
fn normalise_ts_to_utc(ts: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(ts).ok()?;
    Some(dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

/// A scalar JSON value as a plain string, for composing ids.
fn value_scalar_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-coros-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Reuse the canonical Garmin FIT-SDK `Activity.fit` fixture (public test
    /// data, MIT). It is a standard FIT file decodable by `fitparser` — the
    /// same binary format COROS uses. We test COROS-specific path logic
    /// (vault path, source tag, guid scheme) against it; a real COROS-exported
    /// `.fit` would differ only in developer fields the test doesn't rely on.
    const ACTIVITY_FIT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/garmin-activity.fit"
    ));

    #[test]
    fn fit_decode_writes_coros_activity_jsonl() {
        let v = temp_vault("fit");
        let fit = v.root().join("activity_run.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();

        let out = run(&v, &fit);
        assert_eq!(out.counts.get("activities"), Some(&1));
        // The fixture has 22 FIT messages — all preserved.
        assert_eq!(out.counts.get("activity_records"), Some(&22));
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // COROS vault path is health/coros/activities/, not health/garmin/.
        // The fixture's start month is 2012-04.
        let month_dir = v.root().join("health/coros/activities/2012-04");
        assert!(month_dir.exists(), "month partition created under health/coros/");

        let files: Vec<_> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1, "one activity file");

        let body = fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<Value> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 22);

        // Every line carries source=coros, a stable guid, and a message kind.
        for line in &lines {
            assert_eq!(line["source"], Value::String("coros".into()), "source tagged coros");
            assert!(line["guid"].is_string());
            assert!(line["message"].is_string());
        }

        // Core FIT message kinds present.
        let kinds: HashSet<String> = lines
            .iter()
            .filter_map(|l| l["message"].as_str().map(str::to_string))
            .collect();
        assert!(kinds.contains("file_id"), "kinds: {kinds:?}");
        assert!(kinds.contains("session"));
        assert!(kinds.contains("record"), "GPS/HR sample messages");

        // GPS stays in the activity stream — not split to location/.
        assert!(!v.root().join("location").exists(), "GPS never split to location/");
    }

    #[test]
    fn reimport_same_fit_is_a_noop() {
        let v = temp_vault("reimport");
        let fit = v.root().join("run.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();

        let first = run(&v, &fit);
        assert_eq!(first.counts.get("activities"), Some(&1));

        // Locate the written file before re-import.
        let month_dir = v.root().join("health/coros/activities/2012-04");
        let before: Vec<(std::path::PathBuf, String)> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .map(|e| (e.path(), fs::read_to_string(e.path()).unwrap()))
            .collect();

        let again = run(&v, &fit);
        assert_eq!(again.counts.get("activities"), Some(&0), "activity deduped on guid");
        assert_eq!(again.counts.get("duplicates"), Some(&1), "counted as duplicate");

        // File on disk is unchanged.
        let after: Vec<(std::path::PathBuf, String)> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .map(|e| (e.path(), fs::read_to_string(e.path()).unwrap()))
            .collect();
        assert_eq!(before, after, "file unchanged on re-import");
    }

    #[test]
    fn def_is_default_off_fit_only_no_connection() {
        assert!(!DEF.meta.default_on, "GPS trails => opt-in");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no login — pure file import");
        let import = DEF.import_spec().unwrap();
        assert_eq!(import.accepts, &["fit"], "accepts only .fit, not .zip");

        // Setup copy carries the GPS acknowledgement.
        assert!(
            DEF.meta.setup.iter().any(|s| s.to_lowercase().contains("gps")
                || s.to_lowercase().contains("trail")
                || s.to_lowercase().contains("location")),
            "GPS-trails acknowledgement in setup copy"
        );
        // Setup copy mentions Apple Health (incremental value framing).
        assert!(
            DEF.meta.setup.iter().any(|s| s.to_lowercase().contains("apple health")),
            "Apple Health mention in setup copy"
        );
    }

    #[test]
    fn all_messages_preserved_not_filtered() {
        let v = temp_vault("all-msgs");
        let fit = v.root().join("a.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();
        run(&v, &fit);

        let month_dir = v.root().join("health/coros/activities/2012-04");
        let files: Vec<_> = fs::read_dir(&month_dir).unwrap().flatten().collect();
        let body = fs::read_to_string(files[0].path()).unwrap();
        let kinds: HashSet<String> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v["message"].as_str().map(str::to_string))
            .collect();
        // At least 5 distinct message kinds (file_id, session, record, lap,
        // file_creator/event/device_info/activity...).
        assert!(kinds.len() >= 5, "non-core messages preserved generically: {kinds:?}");
    }

    #[test]
    fn guid_is_timezone_independent_utc_normalised() {
        // normalise_ts_to_utc must produce the same UTC instant for all timezone
        // representations of the same moment — the fixture's time_created is
        // "2012-04-09T21:22:26Z".
        let utc = normalise_ts_to_utc("2012-04-09T21:22:26Z");
        let nyc = normalise_ts_to_utc("2012-04-09T17:22:26-04:00");
        let tok = normalise_ts_to_utc("2012-04-10T06:22:26+09:00");
        assert!(utc.is_some());
        assert_eq!(utc, nyc, "UTC and America/New_York representations should normalise equally");
        assert_eq!(utc, tok, "UTC and Asia/Tokyo representations should normalise equally");
        // The canonical form has a Z suffix and no offset.
        assert!(utc.as_deref().unwrap().ends_with('Z'), "normalised form ends with Z");

        // Verify the guid itself uses the Z-normalised form: decode the fixture
        // and check that the stored guid contains the UTC representation.
        let v = temp_vault("guid-utc");
        let fit = v.root().join("tz.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();
        run(&v, &fit);

        let month_dir = v.root().join("health/coros/activities/2012-04");
        let files: Vec<_> = fs::read_dir(&month_dir).unwrap().flatten().collect();
        let body = fs::read_to_string(files[0].path()).unwrap();
        let first: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        let guid = first["guid"].as_str().unwrap();
        // The guid must embed the Z-normalised timestamp, not a local-offset form.
        assert!(
            guid.contains('Z') || !guid.contains('+') && !guid.ends_with("00"),
            "guid should use UTC (Z) form, got: {guid}"
        );
        assert!(
            !guid.contains("-04:00") && !guid.contains("+09:00"),
            "guid must not contain local TZ offset, got: {guid}"
        );
    }

    #[test]
    fn corrupt_fit_returns_error_not_silent_zero() {
        let v = temp_vault("corrupt");
        let bad_fit = v.root().join("bad.fit");
        fs::write(&bad_fit, b"this is not a FIT file at all").unwrap();

        let result = (IMPORT.run)(&v, &bad_fit, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "corrupt FIT should return Err, not Ok with 0 activities");
    }
}
