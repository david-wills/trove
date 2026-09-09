//! Garmin Connect — a file-import source ([`Behavior::Import`]): GPS
//! activities, biometrics, and Body Battery from the official bulk-export ZIP.
//!
//! Garmin has **no viable consumer API** — the official Activity API is a
//! partnership-gated, push-based OAuth 1.0a program (Garmin pushes to *your*
//! server, which a local-first app doesn't have), and the unofficial web
//! scrapers are ToS-gray and intermittently TLS-fingerprint-blocked. The one
//! sanctioned, self-service path is the user-requested data export: Garmin
//! Connect → Account → Export Your Data → a full archive ZIP emailed within
//! ~24–48 h. So this is a drop-the-ZIP importer, no auth, no network.
//!
//! Export ZIP layout (a missing folder just yields fewer streams):
//!   - `…/Activities/*.fit`    — per-activity binary FIT files (GPS trace, HR,
//!     power, cadence, laps, session summary). Decoded via the pure-Rust
//!     `fitparser` crate; we never hand-parse the binary.
//!   - `…/DI_CONNECT/**/*.csv` — summary + health CSVs (activity summary,
//!     sleep, steps, stress, Body Battery, HRV, …).
//!   - `…/WorkoutFiles/`, `…/Courses/` — planned workouts and routes (deferred).
//!
//! ## Vault mapping — the raw-only `health/garmin/` document-domain
//!
//! `health/` is a per-source **raw shape** (like `gaming/`, `developer/`), not
//! a normalized contract: no binding, no `DOMAINS` entry, no schema validation.
//! Full fidelity is the contract — every decoded FIT message and every CSV row
//! is preserved, including message/field types the FIT profile doesn't name
//! (they ride through generically, never dropped).
//!
//!   - **FIT activities → `health/garmin/activities/YYYY-MM/<activity-id>.jsonl`**:
//!     one JSONL line per FIT data message (`file_id`, `session`, `lap`,
//!     `record` GPS/HR/power samples, `event`, `device_info`, …). The GPS route
//!     stays **embedded** in the activity stream — it is never split out to
//!     `location/` (taxonomy rule: the location view joins at read time).
//!     Partitioned by the activity's start month; the `guid` is the activity id
//!     (filename stem, falling back to the `file_id` time+serial).
//!   - **DI_CONNECT CSVs → `health/garmin/<name>.jsonl`**: each CSV becomes a
//!     per-type stream, one JSONL object per data row (header keys → values),
//!     deduped by a stable per-row key. Unknown CSVs are tolerated generically.
//!
//! ## Dedupe / re-import
//!
//! The activity id is the `guid`; every line of one activity carries it.
//! Re-importing the same ZIP is a no-op (the activity's guid is already
//! present); a newer ZIP adds only the new activities — never duplicates.
//! CSV rows dedupe on a stable composite key the same way.
//!
//! ## Privacy
//!
//! 🔒 default-off / opt-in: full GPS traces are location trails. The import
//! copy carries the acknowledgement, the ~24–48 h export-turnaround hint, and
//! the pointer to Strava as the better ongoing-sync path for auto-sync users.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const ACTIVITIES_DIR: &str = "health/garmin/activities";
const HEALTH_DIR: &str = "health/garmin";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Newest write under health/garmin/ — both the per-month activity subdirs
    // and the flat DI_CONNECT CSV streams live below here.
    crate::registry::newest_mtime_recursive(&vault.root().join(HEALTH_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "garmin",
        name: "Garmin Connect",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import your full Garmin activity history — GPS workouts (FIT), Body Battery, \
             sleep, stress, HRV, and health summaries — from a Garmin Connect data-export \
             ZIP. Re-runnable: a newer export never duplicates activities.",
        domain: "health",
        vault_path: "health/garmin/",
        toggleable: false,
        setup: &[
            "Garmin Connect → Account → Export Your Data (connect.garmin.com → Account Settings \
             → Export Your Data). Garmin emails a full-archive ZIP within ~24–48 hours.",
            "Import the ZIP here as-is (or a single activity's .fit file). Imports the FIT \
             activities and the DI_CONNECT summary/health CSVs.",
            "Heads-up: activities include full GPS traces (location trails) — this source is \
             off by default; enabling it imports those trails into your vault.",
            "For ongoing sync, re-export periodically — or, if you auto-sync to Strava, the \
             Strava integration is the better continuous path.",
        ],
        caveats:
            "The official Garmin API is partnership-gated push-OAuth (no standalone path); the \
             self-service data export is the only viable route, so this is a manual ZIP import, \
             not background sync. The export arrives ~24–48 h after you request it. FIT GPS, \
             power, and Body Battery data never reach Apple Health. GPS routes stay embedded in \
             the activity record (the location view joins them at read time).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // The canonical full-export ZIP, or a bare single-activity .fit.
    accepts: &["zip", "fit"],
    params: &[],
    run: run_import,
};

#[derive(Default)]
struct Stats {
    activities: u64,
    activity_records: u64,
    csv_rows: u64,
    csv_streams: u64,
    duplicates: u64,
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let mut stats = Stats::default();
    // Activity guids already stored (across every month partition) — the
    // dedupe set so re-importing the same/newer ZIP never re-writes an
    // activity. Loaded once up front.
    let mut seen_activities = stored_activity_guids(vault)?;
    // CSV row-keys already stored, keyed per CSV stream name.
    let mut seen_csv: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    let is_zip = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip"));
    if is_zip {
        import_zip(vault, path, &mut seen_activities, &mut seen_csv, &mut stats)?;
    } else {
        // A bare .fit — a single exported activity.
        let bytes = std::fs::read(path).with_context(|| format!("opening {}", path.display()))?;
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
        import_fit(vault, &bytes, stem.as_deref(), &mut seen_activities, &mut stats)?;
    }

    progress(ImportProgress { records: stats.activity_records + stats.csv_rows, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{} activities ({} FIT records), {} CSV rows across {} streams imported, {} duplicates skipped",
            stats.activities,
            stats.activity_records,
            stats.csv_rows,
            stats.csv_streams,
            stats.duplicates
        ),
        counts: [
            ("activities", stats.activities),
            ("activity_records", stats.activity_records),
            ("csv_rows", stats.csv_rows),
            ("csv_streams", stats.csv_streams),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

/// Walk the export ZIP: FIT files under `Activities/` → activity streams,
/// CSVs under `DI_CONNECT/` → per-type health/summary streams. Folder
/// matching is suffix-based so a ZIP that nests everything under a top-level
/// folder (Garmin's exports do) still matches.
fn import_zip(
    vault: &Vault,
    path: &Path,
    seen_activities: &mut HashSet<String>,
    seen_csv: &mut BTreeMap<String, HashSet<String>>,
    stats: &mut Stats,
) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string()))
        .collect();

    for name in &names {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".fit") && path_contains_segment(&lower, "activities") {
            let mut bytes = Vec::new();
            if read_entry_bytes(&mut zip, name, &mut bytes).is_err() {
                continue;
            }
            let stem = file_stem(name);
            import_fit(vault, &bytes, stem.as_deref(), seen_activities, stats)?;
        } else if lower.ends_with(".csv") && path_contains_segment(&lower, "di_connect") {
            let mut body = String::new();
            if read_entry_string(&mut zip, name, &mut body).is_err() {
                continue;
            }
            import_csv(vault, name, &body, seen_csv, stats)?;
        }
        // WorkoutFiles/ and Courses/ are deferred — a future additive pass.
    }
    Ok(())
}

/// Decode one FIT file's bytes and write its messages as an activity stream.
/// `stem` is the source filename stem (the preferred stable activity id).
fn import_fit(
    vault: &Vault,
    bytes: &[u8],
    stem: Option<&str>,
    seen_activities: &mut HashSet<String>,
    stats: &mut Stats,
) -> Result<()> {
    // Lenient: a corrupt/partial FIT shouldn't abort a whole-ZIP import.
    let records = match fitparser::from_bytes(bytes) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    if records.is_empty() {
        return Ok(());
    }

    // Decode every message into (kind, fields-map) once; we reuse it to derive
    // the activity id, start timestamp, and the JSONL lines.
    let decoded: Vec<(String, Map<String, Value>)> = records
        .iter()
        .map(|r| (mesg_kind(r), fields_map(r)))
        .collect();

    let guid = activity_guid(stem, &decoded);
    if !seen_activities.insert(guid.clone()) {
        // Already imported (same or older ZIP) — upsert is a no-op.
        stats.duplicates += 1;
        return Ok(());
    }

    // Partition by the activity's start month (local), derived from the
    // session start / file_id creation / first record timestamp. Without one,
    // the activity can't be placed on the timeline — skip it (and un-see it).
    let Some(start_ts) = activity_start_ts(&decoded) else {
        seen_activities.remove(&guid);
        return Ok(());
    };

    let mut lines: Vec<Value> = Vec::with_capacity(decoded.len());
    for (kind, fields) in &decoded {
        let mut row = Map::new();
        // Handles a reader needs on every line: the owning activity + which
        // message this is. The full decoded message rides in `fields`.
        row.insert("guid".into(), Value::String(guid.clone()));
        row.insert("activity_start".into(), Value::String(start_ts.clone()));
        row.insert("source".into(), Value::String("garmin".into()));
        row.insert("message".into(), Value::String(kind.clone()));
        row.insert("fields".into(), Value::Object(fields.clone()));
        lines.push(Value::Object(row));
    }

    // One subdirectory per month so a decade of activities stays navigable:
    // health/garmin/activities/YYYY-MM/<guid>.jsonl. All of an activity's
    // messages live in one file, named by the activity id.
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

/// Parse one DI_CONNECT CSV into a per-type JSONL stream. The stream name is
/// derived from the CSV filename (so sleep, steps, stress, … each get their
/// own file). Header row → keys; each data row → one JSON object. Rows dedupe
/// on a stable composite of all their values, so a re-import upserts.
fn import_csv(
    vault: &Vault,
    name: &str,
    body: &str,
    seen_csv: &mut BTreeMap<String, HashSet<String>>,
    stats: &mut Stats,
) -> Result<()> {
    let stream = csv_stream_name(name);
    let rel = format!("{HEALTH_DIR}/{stream}.jsonl");

    // Lazily load this stream's already-stored row keys the first time we see
    // it (re-runnable dedupe).
    if !seen_csv.contains_key(&stream) {
        seen_csv.insert(stream.clone(), stored_csv_keys(vault, &rel)?);
    }
    let seen = seen_csv.get_mut(&stream).expect("just inserted");

    let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(body.as_bytes());
    let headers: Vec<String> = match rdr.headers() {
        Ok(h) => h.iter().map(|s| s.trim().to_string()).collect(),
        Err(_) => return Ok(()),
    };
    if headers.is_empty() {
        return Ok(());
    }

    let mut rows: Vec<Value> = Vec::new();
    let mut wrote_any = false;
    for rec in rdr.records() {
        let Ok(rec) = rec else { continue };
        let mut obj = Map::new();
        for (i, field) in rec.iter().enumerate() {
            let key = headers.get(i).cloned().unwrap_or_else(|| format!("col{i}"));
            obj.insert(key, Value::String(field.to_string()));
        }
        if obj.is_empty() {
            continue;
        }
        // Stable dedupe key: the stream + every value, in column order. A
        // duplicate row (same export re-imported) is skipped; a genuinely
        // repeated reading is rare and harmless to keep distinct via the key.
        // Stored verbatim as `_dedupe` so re-imports rebuild the exact set
        // without depending on serde_json's map key ordering.
        let key = row_key(&stream, &rec);
        if !seen.insert(key.clone()) {
            stats.duplicates += 1;
            continue;
        }
        obj.insert("source".into(), Value::String("garmin".into()));
        obj.insert("stream".into(), Value::String(stream.clone()));
        obj.insert("_dedupe".into(), Value::String(key));
        rows.push(Value::Object(obj));
        wrote_any = true;
    }

    if !rows.is_empty() {
        let added = rows.len() as u64;
        append_jsonl(vault, &rel, &rows)?;
        stats.csv_rows += added;
    }
    if wrote_any {
        stats.csv_streams += 1;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FIT decoding helpers.

/// One FIT message's kind as the FIT profile names it ("record", "session",
/// "file_id", …). `MesgNum` serializes to the profile name string, or to its
/// numeric value for messages the profile doesn't name — either way it is
/// preserved, never dropped.
fn mesg_kind(rec: &fitparser::FitDataRecord) -> String {
    match serde_json::to_value(rec.kind()) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "unknown".into(),
    }
}

/// One FIT message's fields as a name→value map. Field names come from the FIT
/// profile (or the developer-field definition); unknown fields keep their
/// generic name, so nothing is lost. Units are dropped from the value map for
/// compactness — the value itself (already scaled by the decoder) is the data.
fn fields_map(rec: &fitparser::FitDataRecord) -> Map<String, Value> {
    let mut m = Map::new();
    for f in rec.fields() {
        // serde of `fitparser::Value` is untagged → the bare JSON value
        // (number, string, array, or a timestamp string). Full fidelity.
        let v = serde_json::to_value(f.value()).unwrap_or(Value::Null);
        m.insert(f.name().to_string(), v);
    }
    m
}

/// A stable activity id. Prefer the source filename stem (Garmin names FIT
/// files by the numeric activity id); else derive one from the `file_id`
/// message's `time_created` + `serial_number`; else fall back to the start
/// timestamp. Never empty.
fn activity_guid(stem: Option<&str>, decoded: &[(String, Map<String, Value>)]) -> String {
    if let Some(s) = stem {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    if let Some((_, f)) = decoded.iter().find(|(k, _)| k == "file_id") {
        let time = f.get("time_created").and_then(Value::as_str).unwrap_or("");
        let serial = f.get("serial_number").map(value_scalar_str).unwrap_or_default();
        if !time.is_empty() || !serial.is_empty() {
            return format!("{time}|{serial}");
        }
    }
    activity_start_ts(decoded).unwrap_or_else(|| "garmin-activity".into())
}

/// The activity's start timestamp (RFC3339 local), for partitioning. Looks at
/// the session `start_time`, then `file_id.time_created`, then the first
/// timestamped message — whichever is present.
fn activity_start_ts(decoded: &[(String, Map<String, Value>)]) -> Option<String> {
    let field = |kind: &str, key: &str| {
        decoded
            .iter()
            .find(|(k, _)| k == kind)
            .and_then(|(_, f)| f.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    field("session", "start_time")
        .or_else(|| field("file_id", "time_created"))
        .or_else(|| {
            // First message carrying a timestamp string (fitparser emits FIT
            // timestamps as RFC3339 strings).
            decoded.iter().find_map(|(_, f)| {
                f.get("timestamp").and_then(Value::as_str).map(str::to_string)
            })
        })
}

// ---------------------------------------------------------------------------
// CSV helpers.

/// Stream name for a DI_CONNECT CSV from its path: the filename stem,
/// lowercased and slugified, with the date suffix Garmin appends stripped so
/// monthly chunks of the same metric land in one stream
/// (`…_sleepData_2024-01-01_2024-02-01.csv` → `sleepdata`).
fn csv_stream_name(name: &str) -> String {
    let stem = file_stem(name).unwrap_or_else(|| "csv".into());
    let mut out = String::new();
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '_' || ch == '-' || ch == ' ' {
            out.push('_');
        }
        // drop other punctuation
    }
    // Strip trailing date-range tokens (yyyy_mm_dd…) so re-exports of the same
    // metric accrete into one stream instead of one file per export window.
    let cleaned: Vec<&str> = out
        .split('_')
        .filter(|tok| !tok.is_empty())
        .take_while(|tok| !looks_like_date_token(tok))
        .collect();
    let s = if cleaned.is_empty() { out } else { cleaned.join("_") };
    if s.is_empty() { "csv".into() } else { s }
}

/// A `_`-split token that is all digits and 4 or 8 long (a year or yyyymmdd) —
/// the date noise Garmin appends to export filenames.
fn looks_like_date_token(tok: &str) -> bool {
    (tok.len() == 4 || tok.len() == 8) && tok.bytes().all(|b| b.is_ascii_digit())
}

/// A stable dedupe key for one CSV row: stream name + every field value joined
/// with a unit separator (so two rows differ iff any cell differs).
fn row_key(stream: &str, rec: &csv::StringRecord) -> String {
    let mut k = String::from(stream);
    for field in rec.iter() {
        k.push('\u{1f}');
        k.push_str(field);
    }
    k
}

/// Row dedupe keys already stored in a CSV stream file, for re-runnable
/// dedupe. Each stored row carries its key verbatim in `_dedupe`, so this is
/// independent of how serde orders JSON object keys.
fn stored_csv_keys(vault: &Vault, rel: &str) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let path = vault.resolve(rel)?;
    if !path.exists() {
        return Ok(out);
    }
    let body = std::fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(line) {
            if let Some(k) = obj.get("_dedupe").and_then(Value::as_str) {
                out.insert(k.to_string());
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Activity-stream dedupe set + write helpers.

/// Every activity guid already stored under health/garmin/activities/. One
/// guid per activity file (each line repeats it), so reading the first line of
/// each file is enough — but reading all lines is cheap and robust.
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
                // The filename is the (sanitized) guid, but read the stored
                // guid field to be exact.
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

/// Overwrite a JSONL file with `rows` (one activity's messages → its own
/// file). Atomic so a re-import upsert can't tear a file.
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

/// Append `rows` to a JSONL file (CSV streams accrete across exports).
fn append_jsonl(vault: &Vault, rel: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let path = vault.resolve(rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {rel}"))?;
    for r in rows {
        writeln!(f, "{}", serde_json::to_string(r)?)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Small path/value utilities.

/// Read one zip entry's bytes.
fn read_entry_bytes(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    out: &mut Vec<u8>,
) -> Result<()> {
    out.clear();
    zip.by_name(name)
        .with_context(|| format!("entry {name}"))?
        .read_to_end(out)
        .with_context(|| format!("reading {name}"))?;
    Ok(())
}

/// Read one zip entry as a UTF-8 string (lossy-tolerant via read_to_end).
fn read_entry_string(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    out: &mut String,
) -> Result<()> {
    let mut bytes = Vec::new();
    read_entry_bytes(zip, name, &mut bytes)?;
    *out = String::from_utf8_lossy(&bytes).into_owned();
    Ok(())
}

/// Does a `/`-delimited (already-lowercased) path contain `seg` as a whole
/// path segment? Suffix/prefix tolerant — matches whether or not the export
/// nests everything under a top-level folder.
fn path_contains_segment(lower_path: &str, seg: &str) -> bool {
    lower_path.split('/').any(|s| s == seg)
}

/// The file stem of a `/`-delimited path entry (no directory, no extension).
fn file_stem(name: &str) -> Option<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    Path::new(base).file_stem().map(|s| s.to_string_lossy().into_owned())
}

/// A filesystem-safe form of an activity id for use as a filename.
fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "activity".into() } else { s }
}

/// The on-disk filename stem for an activity, from its raw guid. The canonical
/// case (a clean numeric/alphanumeric guid like `12345`) keeps its plain stem.
/// When sanitization actually changes the guid — punctuation replaced by `_`,
/// or an empty stem — two distinct "dirty" guids could sanitize to the same
/// string and silently overwrite each other (`act 1` and `act_1` both → `act_1`),
/// losing one activity even on re-import (both guids are in `seen_activities`).
/// To keep them distinct, append a short hash of the RAW guid. The `guid` field
/// stored inside the JSONL is always the real raw guid; only the filename is
/// disambiguated, so re-importing the same activity still maps to one file.
fn activity_filename(guid: &str) -> String {
    let clean = sanitize(guid);
    if clean == guid {
        clean
    } else {
        format!("{clean}-{}", short_hash(guid))
    }
}

/// First 8 hex chars of a `DefaultHasher` of the input — enough to keep
/// distinct dirty guids from colliding on disk, without escaping the path.
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

/// A scalar JSON value as a plain string (numbers without quotes), for
/// composing ids. Non-scalars stringify via serde.
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
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-garmin-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// The canonical Garmin FIT-SDK `Activity.fit` example (public test data,
    /// MIT), committed as `tests/fixtures/garmin-activity.fit`.
    const ACTIVITY_FIT: &[u8] =
        include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/garmin-activity.fit"));

    /// A small synthetic DI_CONNECT summary CSV (header + two data rows).
    const SUMMARY_CSV: &str = "\
activityId,activityName,startTimeLocal,activityType,distance,duration,calories,averageHR
1001,Morning Run,2024-01-15 06:30:00,running,5230.0,1800.0,360,148
1002,Evening Ride,2024-01-15 18:00:00,cycling,21000.0,3600.0,640,132
";

    #[test]
    fn fit_decode_writes_activity_jsonl_with_core_fields_and_preserves_all_messages() {
        let v = temp_vault("fit-decode");
        let fit = v.root().join("activity_777.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();

        let out = run(&v, &fit);
        assert_eq!(out.counts.get("activities"), Some(&1));
        // The canonical Activity.fit has 22 FIT messages — every one preserved.
        assert_eq!(out.counts.get("activity_records"), Some(&22), "all messages kept");

        // It partitions by the activity start month (2012-04 in this fixture).
        let path = v.root().join("health/garmin/activities/2012-04/activity_777.jsonl");
        let body = fs::read_to_string(&path).expect("activity jsonl written at start month");
        let lines: Vec<Value> =
            body.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines.len(), 22);

        // Every line carries the activity guid (filename stem) + the message kind.
        for l in &lines {
            assert_eq!(l["guid"], Value::String("activity_777".into()));
            assert_eq!(l["source"], Value::String("garmin".into()));
            assert!(l["message"].is_string());
        }

        // The kinds present include the core activity messages.
        let kinds: HashSet<String> =
            lines.iter().filter_map(|l| l["message"].as_str().map(str::to_string)).collect();
        assert!(kinds.contains("file_id"), "kinds: {kinds:?}");
        assert!(kinds.contains("session"));
        assert!(kinds.contains("record"), "GPS/HR sample messages");
        assert!(kinds.contains("lap"));

        // The session message yields the sport/activity type + a timestamp.
        let session = lines.iter().find(|l| l["message"] == "session").unwrap();
        assert_eq!(session["fields"]["sport"], Value::String("running".into()), "activity type");
        assert!(session["fields"]["start_time"].is_string(), "session timestamp");

        // A record (GPS sample) carries lat/lon + distance + a timestamp.
        let record = lines.iter().find(|l| l["message"] == "record").unwrap();
        assert!(record["fields"]["position_lat"].is_number(), "GPS lat embedded");
        assert!(record["fields"]["position_long"].is_number(), "GPS lon embedded");
        assert!(record["fields"]["distance"].is_number());
        assert!(record["fields"]["timestamp"].is_string(), "sample timestamp");
        // GPS stays in the activity stream — nothing written to location/.
        assert!(!v.root().join("location").exists(), "GPS never split to location/");
    }

    #[test]
    fn unknown_message_types_are_preserved_not_dropped() {
        // The fixture contains messages beyond the headline set (file_creator,
        // event, device_info, activity, …). None are filtered: the count of
        // distinct kinds is well above the four core ones we assert by name.
        let v = temp_vault("preserve");
        let fit = v.root().join("a.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();
        run(&v, &fit);
        let body =
            fs::read_to_string(v.root().join("health/garmin/activities/2012-04/a.jsonl")).unwrap();
        let kinds: HashSet<String> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| v["message"].as_str().map(str::to_string))
            .collect();
        // file_id, file_creator, event, device_info, record, lap, session,
        // activity → strictly more than the 4 we name explicitly.
        assert!(kinds.len() >= 5, "non-core messages preserved generically: {kinds:?}");
        assert!(kinds.contains("file_creator") || kinds.contains("event"), "{kinds:?}");
    }

    #[test]
    fn di_connect_csv_becomes_jsonl_header_keys_to_values() {
        let v = temp_vault("csv");
        let csv_path = v.root().join("garmin_summary.csv");
        // A bare .csv isn't an accepted top-level import (zip/fit only); drive
        // the CSV path directly, as a ZIP import would.
        let mut seen = BTreeMap::new();
        let mut stats = Stats::default();
        import_csv(
            &v,
            "DI_CONNECT/DI-Connect-Fitness/summarizedActivities.csv",
            SUMMARY_CSV,
            &mut seen,
            &mut stats,
        )
        .unwrap();
        let _ = csv_path; // (only to keep the temp dir intent explicit)

        assert_eq!(stats.csv_rows, 2);
        assert_eq!(stats.csv_streams, 1);
        let rel = format!("{HEALTH_DIR}/summarizedactivities.jsonl");
        let body = fs::read_to_string(v.root().join(&rel)).expect("csv stream written");
        let rows: Vec<Value> =
            body.lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["activityName"], Value::String("Morning Run".into()));
        assert_eq!(rows[0]["activityType"], Value::String("running".into()));
        assert_eq!(rows[0]["averageHR"], Value::String("148".into()), "header keys → values");
        assert_eq!(rows[0]["source"], Value::String("garmin".into()));
        assert_eq!(rows[1]["activityName"], Value::String("Evening Ride".into()));
    }

    #[test]
    fn mini_export_zip_writes_both_activity_and_summary_streams() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("garmin-export.zip");
        let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        // Nest under a top-level folder like a real export.
        z.start_file("DI_CONNECT-export/Activities/12345.fit", opts).unwrap();
        z.write_all(ACTIVITY_FIT).unwrap();
        z.start_file("DI_CONNECT-export/DI_CONNECT/DI-Connect-Fitness/summary.csv", opts).unwrap();
        z.write_all(SUMMARY_CSV.as_bytes()).unwrap();
        z.finish().unwrap();

        let out = run(&v, &zip_path);
        assert_eq!(out.counts.get("activities"), Some(&1), "FIT under Activities/");
        assert_eq!(out.counts.get("csv_rows"), Some(&2), "CSV under DI_CONNECT/");

        // Activity JSONL named by the FIT stem (the numeric activity id).
        assert!(
            v.root().join("health/garmin/activities/2012-04/12345.jsonl").exists(),
            "activity stream written"
        );
        // Summary JSONL stream written.
        assert!(
            v.root().join("health/garmin/summary.jsonl").exists(),
            "summary stream written"
        );
    }

    #[test]
    fn reimport_same_zip_is_a_noop_newer_zip_adds_only_new() {
        let v = temp_vault("dedupe");
        // First ZIP: one activity (12345) + the summary CSV.
        let zip1 = v.root().join("export1.zip");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&zip1).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("Activities/12345.fit", opts).unwrap();
            z.write_all(ACTIVITY_FIT).unwrap();
            z.start_file("DI_CONNECT/summary.csv", opts).unwrap();
            z.write_all(SUMMARY_CSV.as_bytes()).unwrap();
            z.finish().unwrap();
        }
        let first = run(&v, &zip1);
        assert_eq!(first.counts.get("activities"), Some(&1));
        assert_eq!(first.counts.get("csv_rows"), Some(&2));

        let activity_before =
            fs::read_to_string(v.root().join("health/garmin/activities/2012-04/12345.jsonl")).unwrap();
        let summary_before = fs::read_to_string(v.root().join("health/garmin/summary.jsonl")).unwrap();

        // Re-import the SAME ZIP: pure no-op — activity already seen, CSV rows
        // already seen.
        let again = run(&v, &zip1);
        assert_eq!(again.counts.get("activities"), Some(&0), "activity deduped on guid");
        assert_eq!(again.counts.get("csv_rows"), Some(&0), "csv rows deduped");
        assert!(again.counts.get("duplicates").unwrap() >= &1);
        assert_eq!(
            fs::read_to_string(v.root().join("health/garmin/activities/2012-04/12345.jsonl")).unwrap(),
            activity_before,
            "activity file unchanged on re-import"
        );
        assert_eq!(
            fs::read_to_string(v.root().join("health/garmin/summary.jsonl")).unwrap(),
            summary_before,
            "summary file unchanged on re-import"
        );

        // A NEWER ZIP with the same old activity PLUS a new one (67890) and one
        // new CSV row: only the new activity + the new row are added.
        let zip2 = v.root().join("export2.zip");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&zip2).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("Activities/12345.fit", opts).unwrap(); // old, dedupes
            z.write_all(ACTIVITY_FIT).unwrap();
            z.start_file("Activities/67890.fit", opts).unwrap(); // new
            z.write_all(ACTIVITY_FIT).unwrap();
            z.start_file("DI_CONNECT/summary.csv", opts).unwrap();
            // Old two rows + one new row.
            let mut newer = String::from(SUMMARY_CSV);
            newer.push_str("1003,Night Walk,2024-01-16 21:00:00,walking,3000.0,2400.0,180,99\n");
            z.write_all(newer.as_bytes()).unwrap();
            z.finish().unwrap();
        }
        let newer = run(&v, &zip2);
        assert_eq!(newer.counts.get("activities"), Some(&1), "only the new activity added");
        assert_eq!(newer.counts.get("csv_rows"), Some(&1), "only the new CSV row added");

        // The new activity landed in its own file; the old one is untouched.
        assert!(v.root().join("health/garmin/activities/2012-04/67890.jsonl").exists());
        let summary_rows = fs::read_to_string(v.root().join("health/garmin/summary.jsonl"))
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(summary_rows, 3, "two old rows + one new row, no duplicates");
    }

    #[test]
    fn bare_fit_file_imports_as_a_single_activity() {
        let v = temp_vault("bare-fit");
        let fit = v.root().join("morning_5k.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();
        let out = run(&v, &fit);
        assert_eq!(out.counts.get("activities"), Some(&1));
        assert!(v.root().join("health/garmin/activities/2012-04/morning_5k.jsonl").exists());
    }

    #[test]
    fn distinct_guids_that_sanitize_alike_do_not_collide() {
        // Two DISTINCT activity guids that sanitize to the SAME string
        // ("act 1" and "act_1" both → "act_1") must NOT overwrite each other.
        let v = temp_vault("collide");
        let mut seen = HashSet::new();
        let mut stats = Stats::default();
        import_fit(&v, ACTIVITY_FIT, Some("act 1"), &mut seen, &mut stats).unwrap();
        import_fit(&v, ACTIVITY_FIT, Some("act_1"), &mut seen, &mut stats).unwrap();

        // Both activities counted, none silently lost as a "duplicate".
        assert_eq!(stats.activities, 2, "both distinct activities imported");
        assert_eq!(stats.duplicates, 0, "distinct guids are not deduped");

        // Two distinct files on disk (no data loss from a filename clash).
        let month_dir = v.root().join("health/garmin/activities/2012-04");
        let files: Vec<_> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 2, "two distinct files, not one overwritten: {files:?}");

        // Each file stores its OWN real raw guid inside the JSONL.
        let stored: HashSet<String> = files
            .iter()
            .filter_map(|p| fs::read_to_string(p).ok())
            .filter_map(|b| {
                b.lines()
                    .find(|l| !l.trim().is_empty())
                    .and_then(|l| serde_json::from_str::<Value>(l).ok())
            })
            .filter_map(|v| v["guid"].as_str().map(str::to_string))
            .collect();
        assert!(stored.contains("act 1"), "raw guid preserved inside JSONL: {stored:?}");
        assert!(stored.contains("act_1"), "raw guid preserved inside JSONL: {stored:?}");

        // The canonical case: a clean numeric guid keeps its plain <id>.jsonl
        // (no hash suffix), unchanged from prior behavior.
        let v2 = temp_vault("collide-clean");
        let mut seen2 = HashSet::new();
        let mut stats2 = Stats::default();
        import_fit(&v2, ACTIVITY_FIT, Some("12345"), &mut seen2, &mut stats2).unwrap();
        assert!(
            v2.root().join("health/garmin/activities/2012-04/12345.jsonl").exists(),
            "clean numeric guid keeps its plain <id>.jsonl filename"
        );
        assert_eq!(activity_filename("12345"), "12345", "no suffix on a clean guid");
        assert_ne!(
            activity_filename("act 1"),
            activity_filename("act_1"),
            "dirty guids that sanitize alike get distinct filenames"
        );
    }

    #[test]
    fn def_is_default_off_opt_in_with_import_box() {
        // Privacy: GPS trails ⇒ opt-in.
        assert!(!DEF.meta.default_on, "Garmin is default-off (GPS trails)");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no login — pure file import");
        let import = DEF.import_spec().unwrap();
        assert_eq!(import.accepts, &["zip", "fit"]);
        // The hub surfaces it from the registry, with the export-turnaround +
        // GPS acknowledgement in the setup copy.
        let status = v_status();
        let card = status.iter().find(|s| s.id == "garmin").unwrap();
        assert!(card.import.is_some(), "import box info present");
        assert!(
            card.setup.iter().any(|s| s.contains("24") || s.to_lowercase().contains("hour")),
            "export-turnaround hint in setup copy"
        );
        assert!(
            card.setup.iter().any(|s| s.to_lowercase().contains("gps") || s.to_lowercase().contains("trail")),
            "GPS-trails acknowledgement in setup copy"
        );
        assert!(
            card.setup.iter().any(|s| s.to_lowercase().contains("strava")),
            "Strava ongoing-sync pointer in setup copy"
        );
    }

    fn v_status() -> Vec<crate::integrations::IntegrationStatus> {
        let v = temp_vault("status");
        v.integrations_status()
    }
}
