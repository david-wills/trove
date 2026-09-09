//! Suunto GPS sports watch data — FIT/GPX file import or bulk-JSON export.
//! Brief: docs/integrations/suunto.md.
//!
//! Suunto makes GPS sports watches with strong DNA in trail running, triathlon,
//! diving, and multisport. The Suunto app syncs to Apple Health (so Apple Health
//! covers heart-rate and step counts for iPhone users), but per-activity FIT/GPX
//! files carry the full GPS route, laps, HR samples, and altitude that never
//! reach Apple Health.
//!
//! ## Build targets (self-service)
//!
//! 1. **Per-activity FIT**: export one activity from the Suunto app → share →
//!    `.fit` file. Decoded via the same `fitparser` crate used by
//!    Garmin/COROS/Wahoo.
//! 2. **Per-activity GPX**: export one activity from the Suunto app → share →
//!    `.gpx` file. Parsed via `quick-xml`; track-points, extensions, metadata,
//!    and waypoints all preserved.
//! 3. **Bulk ZIP export**: suunto.com → Account → Export Data → a ZIP containing
//!    FIT and/or GPX files, plus (optionally) a bulk JSON. FIT/GPX entries are
//!    routed to their own importers. The bulk JSON shape is **undocumented** in
//!    the research brief — the raw JSON is preserved in `health/suunto/raw/` and
//!    the structured parser is **parked until a real export is in hand**.
//!
//! ## Vault mapping (raw-only `health/suunto/`)
//!
//! `health/` is a per-source **raw shape** (no bound contract, no `DOMAINS`
//! entry, no spec-validation row). Full fidelity is the contract.
//!
//! - **FIT/GPX activities →
//!   `health/suunto/workouts/YYYY-MM/<guid>.jsonl`**: one JSONL line per FIT
//!   message or GPX element, with a shared `guid` and `activity_start` on every
//!   line. GPS routes stay embedded — the location view joins at read time.
//! - **Bulk JSON / unknown ZIP content →
//!   `health/suunto/raw/<filename>`**: verbatim copy, full fidelity, parked for
//!   a future structured parser.
//!
//! ## GUID scheme
//!
//! FIT files: `suunto|<utc-time_created>|<serial>` — UTC-normalised so the guid
//! is timezone-independent (same activity imported from two machines in different
//! TZ → same guid → one file, no duplicate). GPX files: `suunto-gpx|<date>|<name>`
//! derived from the GPX metadata or the first track-point timestamp.
//!
//! ## Dedupe / re-import
//!
//! Re-importing the same file is a no-op — the guid is already in
//! `seen_activities`. A newer ZIP with the same activities adds nothing; new
//! activities accrete.
//!
//! ## Privacy
//!
//! default-off / opt-in: GPS workout routes are location trails. The import copy
//! carries the acknowledgement and notes that Apple Health already covers basics
//! for iPhone users; this adds full telemetry and covers non-iPhone users.
//!
//! ## Cloud API (deferred)
//!
//! `apizone.suunto.com` (Azure API Management) — business-approval bias, personal
//! access unclear. Webhook FIT-URL notifications exist but require a server relay
//! (incompatible with local-first). Poll approach would work if access is ever
//! granted; recorded in the brief, not implemented here.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::DateTime;
use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use quick_xml::XmlVersion;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const WORKOUTS_DIR: &str = "health/suunto/workouts";
const RAW_DIR: &str = "health/suunto/raw";
const HEALTH_DIR: &str = "health/suunto";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime_recursive(&vault.root().join(HEALTH_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub is already there).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "suunto",
        name: "Suunto",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Import GPS workouts from Suunto watches — FIT or GPX per-activity exports \
             from the Suunto app, or the bulk ZIP export from suunto.com. Re-runnable: \
             re-importing the same activity is a no-op.",
        domain: "health",
        vault_path: "health/suunto/",
        toggleable: false,
        setup: &[
            "Suunto app → open a completed workout → tap the share icon → Export as FIT \
             or GPX file. Import that file here. Repeat for each activity you want to \
             capture. For a full history, suunto.com → Account → Export Data produces a \
             bulk ZIP (FIT + GPX inside).",
            "Heads-up: activities include full GPS traces (location trails) — this source \
             is off by default; enabling it imports those routes into your vault.",
            "For iPhone users: Apple Health already receives the basic activity summary \
             from Suunto via the Suunto app. This import adds full GPS telemetry, \
             lap data, and HR samples that never reach Apple Health, and covers \
             non-iPhone users.",
            "The Suunto Cloud API requires a business-approval application and is not \
             self-service — the FIT/GPX file export is the reliable path for individual \
             users.",
        ],
        caveats:
            "Per-activity FIT/GPX export requires opening each workout in the Suunto app. \
             The bulk suunto.com export provides a ZIP with all activities at once. \
             GPS routes stay embedded in the activity record (the location view joins \
             them at read time). The Cloud API is business-gated and not available for \
             self-service integrations.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Per-activity FIT and GPX exports from the Suunto app, plus the bulk ZIP
    // from suunto.com which may contain either or both.
    accepts: &["fit", "gpx", "zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Stats

#[derive(Default)]
struct Stats {
    activities: u64,
    activity_records: u64,
    raw_files: u64,
    duplicates: u64,
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let mut stats = Stats::default();
    let mut seen_activities = stored_activity_guids(vault)?;

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("fit") => {
            let bytes =
                std::fs::read(path).with_context(|| format!("opening {}", path.display()))?;
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            import_fit(vault, &bytes, stem.as_deref(), &mut seen_activities, &mut stats)?;
        }
        Some("gpx") => {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            import_gpx(vault, &body, stem.as_deref(), &mut seen_activities, &mut stats)?;
        }
        Some("zip") => {
            import_zip(vault, path, &mut seen_activities, &mut stats)?;
        }
        _ => {
            anyhow::bail!(
                "Suunto importer only accepts .fit, .gpx, or .zip files \
                 (got: {})",
                path.display()
            );
        }
    }

    progress(ImportProgress {
        records: stats.activity_records + stats.raw_files,
        percent: 100.0,
    });
    Ok(ImportOutcome {
        headline: format!(
            "{} activities ({} records) imported, {} raw files preserved, {} duplicates skipped",
            stats.activities, stats.activity_records, stats.raw_files, stats.duplicates
        ),
        counts: [
            ("activities", stats.activities),
            ("activity_records", stats.activity_records),
            ("raw_files", stats.raw_files),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// ZIP importer — routes entries to FIT / GPX / raw preservers

fn import_zip(
    vault: &Vault,
    path: &Path,
    seen_activities: &mut HashSet<String>,
    stats: &mut Stats,
) -> Result<()> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip =
        zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;

    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            zip.by_index(i).ok().filter(|e| e.is_file()).map(|e| e.name().to_string())
        })
        .collect();

    for name in &names {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".fit") {
            let mut bytes = Vec::new();
            if read_entry_bytes(&mut zip, name, &mut bytes).is_err() {
                continue;
            }
            let stem = file_stem(name);
            import_fit(vault, &bytes, stem.as_deref(), seen_activities, stats).ok();
        } else if lower.ends_with(".gpx") {
            let mut body = String::new();
            if read_entry_string(&mut zip, name, &mut body).is_err() {
                continue;
            }
            let stem = file_stem(name);
            import_gpx(vault, &body, stem.as_deref(), seen_activities, stats).ok();
        } else {
            // Bulk JSON or other content: preserve verbatim in health/suunto/raw/.
            // The bulk JSON format is undocumented; a structured parser is parked
            // until a real export sample is available.
            let mut bytes = Vec::new();
            if read_entry_bytes(&mut zip, name, &mut bytes).is_err() {
                continue;
            }
            let basename = name.rsplit('/').next().unwrap_or(name);
            if basename.is_empty() || bytes.is_empty() {
                continue;
            }
            let rel = format!("{RAW_DIR}/{basename}");
            if let Ok(dest) = vault.resolve(&rel) {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                crate::store::write_atomic(&dest, &bytes).ok();
                stats.raw_files += 1;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// FIT importer (mirrors coros.rs with suunto source tag + UTC-normalised guid)

fn import_fit(
    vault: &Vault,
    bytes: &[u8],
    stem: Option<&str>,
    seen_activities: &mut HashSet<String>,
    stats: &mut Stats,
) -> Result<()> {
    // Lenient for ZIP batch imports (a single corrupt entry shouldn't abort the
    // whole batch), but callers of bare-file imports still get the parse error.
    let records = match fitparser::from_bytes(bytes) {
        Ok(r) => r,
        Err(e) => {
            if stem.is_some() {
                return Err(anyhow::anyhow!(
                    "failed to parse FIT file {}: {e}",
                    stem.unwrap_or("<unknown>")
                ));
            }
            return Ok(());
        }
    };
    if records.is_empty() {
        return Ok(());
    }

    let decoded: Vec<(String, Map<String, Value>)> =
        records.iter().map(|r| (mesg_kind(r), fields_map(r))).collect();

    let guid = fit_guid(stem, &decoded);
    if !seen_activities.insert(guid.clone()) {
        stats.duplicates += 1;
        return Ok(());
    }

    let Some(start_ts) = fit_start_ts(&decoded) else {
        seen_activities.remove(&guid);
        return Ok(());
    };

    let mut lines: Vec<Value> = Vec::with_capacity(decoded.len());
    for (kind, fields) in &decoded {
        let mut row = Map::new();
        row.insert("guid".into(), Value::String(guid.clone()));
        row.insert("activity_start".into(), Value::String(start_ts.clone()));
        row.insert("source".into(), Value::String("suunto".into()));
        row.insert("message".into(), Value::String(kind.clone()));
        row.insert("fields".into(), Value::Object(fields.clone()));
        lines.push(Value::Object(row));
    }

    let Some(month) = Partition::Month.key(&start_ts) else {
        seen_activities.remove(&guid);
        return Ok(());
    };
    let rel = format!("{WORKOUTS_DIR}/{month}/{}.jsonl", activity_filename(&guid));
    write_jsonl(vault, &rel, &lines)?;

    stats.activities += 1;
    stats.activity_records += lines.len() as u64;
    Ok(())
}

// ---------------------------------------------------------------------------
// GPX importer — parse with quick-xml, preserve full fidelity

/// Parse a GPX document and write one JSONL file per track (or the whole
/// document if it has no tracks). GPX structure:
/// - `<metadata>`: title, desc, author, link, time, bounds
/// - `<wpt>`: waypoints (standalone)
/// - `<trk>/<trkseg>/<trkpt>`: track-points (lat, lon, ele, time + extensions)
/// - `<rte>/<rtept>`: route waypoints
///
/// Each track becomes one activity (one JSONL file). A GPX with only waypoints
/// or routes (no `<trk>`) is treated as a single activity from the file.
fn import_gpx(
    vault: &Vault,
    body: &str,
    stem: Option<&str>,
    seen_activities: &mut HashSet<String>,
    stats: &mut Stats,
) -> Result<()> {
    let parsed = parse_gpx(body)?;
    if parsed.is_empty() {
        return Ok(());
    }

    for activity in parsed {
        let guid = activity.guid.clone();
        if !seen_activities.insert(guid.clone()) {
            stats.duplicates += 1;
            continue;
        }

        let start_ts = activity.start_ts.clone();
        // Route-only / no-time activities use "undated" as the partition bucket
        // rather than defaulting to the 1970 epoch.
        let month = if start_ts == "undated" {
            "undated".to_string()
        } else {
            match Partition::Month.key(&start_ts) {
                Some(m) => m.to_string(),
                None => {
                    seen_activities.remove(&guid);
                    continue;
                }
            }
        };
        let rel = format!("{WORKOUTS_DIR}/{month}/{}.jsonl", activity_filename(&guid));
        write_jsonl(vault, &rel, &activity.rows)?;

        stats.activities += 1;
        stats.activity_records += activity.rows.len() as u64;
    }

    let _ = stem; // used as fallback inside parse_gpx already
    Ok(())
}

/// One parsed GPX activity: a stable guid, the partition timestamp, and the
/// JSONL rows (one per trkpt / metadata element / waypoint / route-point).
struct GpxActivity {
    guid: String,
    start_ts: String,
    rows: Vec<Value>,
}

/// Parse a GPX document into one or more activities.
/// A GPX may contain multiple `<trk>` elements; each becomes a separate
/// activity. All trkpts within one trk share the same guid.
fn parse_gpx(body: &str) -> Result<Vec<GpxActivity>> {
    // We do a two-pass strategy:
    // Pass 1: collect metadata, waypoints, and track/route points into a flat
    //         attribute-bag per element using quick-xml's streaming reader.
    // Each track segment produces a list of point objects. Metadata / waypoints
    // are included as a "header" block on the first/only track.

    let mut reader = XmlReader::from_str(body);
    reader.config_mut().trim_text(true);

    // Accumulator for the current track (or the file if no trk elements).
    // When we finish a </trk>, we flush it as a GpxActivity.
    let mut activities: Vec<GpxActivity> = Vec::new();
    let mut current_rows: Vec<Value> = Vec::new();
    let mut current_start_ts: Option<String> = None;
    // Per-track <name> element (distinct from file-level <metadata><name>).
    let mut cur_trk_name: Option<String> = None;
    // Count of flushed tracks; used as a disambiguator when no per-track name.
    let mut trk_index: usize = 0;

    // Metadata extracted from <metadata>.
    let mut metadata: Map<String, Value> = Map::new();

    // State machine tracking our position in the XML tree.
    #[derive(Debug, PartialEq, Clone)]
    enum Ctx {
        Root,
        Metadata,
        Wpt,
        Trk,
        TrkSeg,
        TrkPt,
        Rte,
        RtePt,
        Extension,
    }
    let mut ctx_stack: Vec<Ctx> = vec![Ctx::Root];
    // Current element being built (attrs + child text).
    let mut cur_elem: Map<String, Value> = Map::new();
    // Extensions under the current trkpt/wpt (as a JSON object).
    let mut cur_ext: Map<String, Value> = Map::new();
    // Text buffer for the current element's text content.
    let mut cur_text = String::new();
    // Attribute bag for the current trkpt/wpt (lat/lon/ele from XML attrs).
    let mut cur_attrs: Map<String, Value> = Map::new();
    // Whether we are inside any <extensions> block.
    let mut in_extensions = false;
    // Extension element currently being read.
    let mut ext_tag = String::new();

    // A flag to detect whether any <trk> appeared in the doc at all; if not,
    // we emit one activity from waypoints / route-points.
    let mut saw_trk = false;

    // For "source" on every row:
    let source_val = Value::String("suunto".into());

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                let tag = std::str::from_utf8(e.name().0).unwrap_or("?").to_string();
                let ctx = ctx_stack.last().cloned().unwrap_or(Ctx::Root);

                match tag.as_str() {
                    "metadata" => ctx_stack.push(Ctx::Metadata),
                    "wpt" => {
                        ctx_stack.push(Ctx::Wpt);
                        cur_attrs = extract_xml_attrs(e);
                        cur_elem.clear();
                        cur_text.clear();
                    }
                    "trk" => {
                        saw_trk = true;
                        // Reset per-track state for the new track.
                        ctx_stack.push(Ctx::Trk);
                        current_rows.clear();
                        cur_trk_name = None;
                        current_start_ts = None;
                        cur_text.clear();
                    }
                    "trkseg" => {
                        ctx_stack.push(Ctx::TrkSeg);
                    }
                    "trkpt" => {
                        ctx_stack.push(Ctx::TrkPt);
                        cur_attrs = extract_xml_attrs(e);
                        cur_elem.clear();
                        cur_text.clear();
                        cur_ext.clear();
                        in_extensions = false;
                    }
                    "rte" => ctx_stack.push(Ctx::Rte),
                    "rtept" => {
                        ctx_stack.push(Ctx::RtePt);
                        cur_attrs = extract_xml_attrs(e);
                        cur_elem.clear();
                        cur_text.clear();
                        cur_ext.clear();
                        in_extensions = false;
                    }
                    "extensions" => {
                        in_extensions = true;
                        ctx_stack.push(Ctx::Extension);
                    }
                    _ => {
                        if in_extensions {
                            ext_tag = tag.clone();
                        }
                        if matches!(
                            ctx,
                            Ctx::Metadata | Ctx::Wpt | Ctx::TrkPt | Ctx::RtePt | Ctx::Trk
                        ) {
                            cur_text.clear();
                        }
                    }
                }
            }

            Ok(Event::End(ref e)) => {
                let tag = std::str::from_utf8(e.name().0).unwrap_or("?").to_string();

                match tag.as_str() {
                    "extensions" => {
                        in_extensions = false;
                        ctx_stack.pop();
                    }
                    "metadata" => {
                        ctx_stack.pop();
                    }
                    "wpt" => {
                        let mut obj = cur_attrs.clone();
                        obj.extend(cur_elem.clone());
                        obj.insert("_kind".into(), Value::String("wpt".into()));
                        obj.insert("source".into(), source_val.clone());
                        // Track time for start_ts derivation.
                        if let Some(t) = obj.get("time").and_then(Value::as_str) {
                            if current_start_ts.is_none() {
                                current_start_ts =
                                    normalise_ts(t).or_else(|| Some(t.to_string()));
                            }
                        }
                        current_rows.push(Value::Object(obj));
                        ctx_stack.pop();
                        cur_attrs.clear();
                        cur_elem.clear();
                    }
                    "trkpt" => {
                        let mut obj = cur_attrs.clone();
                        obj.extend(cur_elem.clone());
                        if !cur_ext.is_empty() {
                            obj.insert("extensions".into(), Value::Object(cur_ext.clone()));
                        }
                        obj.insert("_kind".into(), Value::String("trkpt".into()));
                        obj.insert("source".into(), source_val.clone());
                        if let Some(t) = obj.get("time").and_then(Value::as_str) {
                            if current_start_ts.is_none() {
                                current_start_ts =
                                    normalise_ts(t).or_else(|| Some(t.to_string()));
                            }
                        }
                        current_rows.push(Value::Object(obj));
                        ctx_stack.pop();
                        cur_attrs.clear();
                        cur_elem.clear();
                        cur_ext.clear();
                        in_extensions = false;
                    }
                    "rtept" => {
                        let mut obj = cur_attrs.clone();
                        obj.extend(cur_elem.clone());
                        if !cur_ext.is_empty() {
                            obj.insert("extensions".into(), Value::Object(cur_ext.clone()));
                        }
                        obj.insert("_kind".into(), Value::String("rtept".into()));
                        obj.insert("source".into(), source_val.clone());
                        if let Some(t) = obj.get("time").and_then(Value::as_str) {
                            if current_start_ts.is_none() {
                                current_start_ts =
                                    normalise_ts(t).or_else(|| Some(t.to_string()));
                            }
                        }
                        current_rows.push(Value::Object(obj));
                        ctx_stack.pop();
                        cur_attrs.clear();
                        cur_elem.clear();
                        cur_ext.clear();
                        in_extensions = false;
                    }
                    "trkseg" => {
                        ctx_stack.pop();
                    }
                    "trk" => {
                        // Flush this track as one GpxActivity.
                        if !current_rows.is_empty() {
                            let (guid, start_ts) = match current_start_ts.take() {
                                Some(ts) => {
                                    // Timed track: guid uses UTC start + per-track name/index
                                    // to disambiguate multiple tracks with the same timestamp.
                                    let g = gpx_guid(
                                        &ts,
                                        cur_trk_name.as_deref(),
                                        trk_index,
                                    );
                                    (g, ts)
                                }
                                None => {
                                    // Route-only / no <time>: derive guid from content hash
                                    // and partition under "undated" to avoid 1970 junk bucket.
                                    let g = gpx_route_guid(&current_rows, trk_index);
                                    (g, "undated".into())
                                }
                            };
                            // Attach the guid + activity_start to every row.
                            let rows: Vec<Value> = current_rows
                                .drain(..)
                                .map(|mut r| {
                                    if let Value::Object(ref mut m) = r {
                                        m.insert("guid".into(), Value::String(guid.clone()));
                                        m.insert(
                                            "activity_start".into(),
                                            Value::String(start_ts.clone()),
                                        );
                                    }
                                    r
                                })
                                .collect();
                            trk_index += 1;
                            activities.push(GpxActivity { guid, start_ts, rows });
                        }
                        ctx_stack.pop();
                    }
                    _ => {
                        // Text child of metadata → store in metadata map.
                        let ctx = ctx_stack.last().cloned().unwrap_or(Ctx::Root);
                        if matches!(ctx, Ctx::Metadata) && !cur_text.trim().is_empty() {
                            metadata.insert(
                                tag.clone(),
                                Value::String(cur_text.trim().to_string()),
                            );
                            cur_text.clear();
                        }
                        // Text child of <trk> (direct child, not inside trkseg) → capture
                        // the per-track <name> so it can disambiguate multi-track guids.
                        if matches!(ctx, Ctx::Trk) && tag == "name" && !cur_text.trim().is_empty()
                        {
                            cur_trk_name = Some(cur_text.trim().to_string());
                            cur_text.clear();
                        }
                        // Text child of wpt/trkpt/rtept → store in cur_elem.
                        if matches!(ctx, Ctx::Wpt | Ctx::TrkPt | Ctx::RtePt) {
                            if !cur_text.trim().is_empty() {
                                cur_elem.insert(
                                    tag.clone(),
                                    Value::String(cur_text.trim().to_string()),
                                );
                            }
                            cur_text.clear();
                        }
                        // Text child of an extension element → store in cur_ext.
                        // Normalize namespace prefixes: gpxtpx:hr → hr.
                        if in_extensions && !ext_tag.is_empty() && tag == ext_tag {
                            if !cur_text.trim().is_empty() {
                                let local_key = strip_ns_prefix(&ext_tag).to_string();
                                cur_ext.insert(
                                    local_key,
                                    Value::String(cur_text.trim().to_string()),
                                );
                            }
                            cur_text.clear();
                            ext_tag.clear();
                        }
                    }
                }
            }

            Ok(Event::Text(ref e)) => {
                let t = e.decode().unwrap_or_default();
                if !t.trim().is_empty() {
                    cur_text.push_str(t.trim());
                }
            }

            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // If no <trk> elements appeared, treat all accumulated rows (waypoints,
    // route-points) as a single activity.
    if !saw_trk && !current_rows.is_empty() {
        let (guid, start_ts) = match current_start_ts.take() {
            Some(ts) => {
                let g = gpx_guid(
                    &ts,
                    metadata.get("name").and_then(Value::as_str),
                    0,
                );
                (g, ts)
            }
            None => {
                // No time anywhere in this no-trk file: content-hash guid, undated bucket.
                let g = gpx_route_guid(&current_rows, 0);
                (g, "undated".into())
            }
        };
        let rows: Vec<Value> = current_rows
            .into_iter()
            .map(|mut r| {
                if let Value::Object(ref mut m) = r {
                    m.insert("guid".into(), Value::String(guid.clone()));
                    m.insert("activity_start".into(), Value::String(start_ts.clone()));
                }
                r
            })
            .collect();
        activities.push(GpxActivity { guid, start_ts, rows });
    }

    Ok(activities)
}

/// Extract XML element attributes into a JSON map (`{"lat": "...", "lon": "..."}`).
fn extract_xml_attrs(e: &quick_xml::events::BytesStart<'_>) -> Map<String, Value> {
    let mut m = Map::new();
    for attr in e.attributes().flatten() {
        let key = std::str::from_utf8(attr.key.0).unwrap_or("?").to_string();
        let val = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .unwrap_or_default()
            .to_string();
        m.insert(key, Value::String(val));
    }
    m
}

// ---------------------------------------------------------------------------
// FIT decoding helpers (mirrors coros.rs)

fn mesg_kind(rec: &fitparser::FitDataRecord) -> String {
    match serde_json::to_value(rec.kind()) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => "unknown".into(),
    }
}

fn fields_map(rec: &fitparser::FitDataRecord) -> Map<String, Value> {
    let mut m = Map::new();
    for f in rec.fields() {
        let v = serde_json::to_value(f.value()).unwrap_or(Value::Null);
        m.insert(f.name().to_string(), v);
    }
    m
}

/// Stable, timezone-independent guid for a FIT activity. Derives from
/// `file_id.time_created` (UTC-normalised) + `serial_number`, falling back
/// to the filename stem or start timestamp.
fn fit_guid(stem: Option<&str>, decoded: &[(String, Map<String, Value>)]) -> String {
    if let Some((_, f)) = decoded.iter().find(|(k, _)| k == "file_id") {
        let raw_time = f.get("time_created").and_then(Value::as_str).unwrap_or("");
        let serial = f.get("serial_number").map(value_scalar_str).unwrap_or_default();
        let time = normalise_ts(raw_time).unwrap_or_else(|| raw_time.to_string());
        if !time.is_empty() || !serial.is_empty() {
            return format!("suunto|{time}|{serial}");
        }
    }
    if let Some(s) = stem {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    fit_start_ts(decoded).unwrap_or_else(|| "suunto-activity".into())
}

/// Activity start timestamp (UTC RFC3339) from a decoded FIT file.
fn fit_start_ts(decoded: &[(String, Map<String, Value>)]) -> Option<String> {
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
    Some(normalise_ts(&raw).unwrap_or(raw))
}

// ---------------------------------------------------------------------------
// GPX GUID

/// A stable, timezone-independent guid for a GPX activity.
///
/// For timed tracks: `suunto-gpx|<utc-start>|<trk-name-slug-or-idx>`.
/// The per-track name (or sequence index) disambiguates multiple `<trk>`
/// elements in one file that share the same first-trkpt timestamp.
///
/// For route-only / no-time tracks: `suunto-gpx|undated|<content-hash>`
/// derived from the lat/lon sequence so two different route exports with
/// the same metadata name do not collide.
fn gpx_guid(start_ts: &str, trk_name: Option<&str>, trk_idx: usize) -> String {
    let disambig = trk_name
        .map(|n| {
            let slug: String = n
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
                .collect();
            slug
        })
        .unwrap_or_else(|| format!("track{trk_idx}"));
    format!("suunto-gpx|{start_ts}|{disambig}")
}

/// Build a guid for a route-only (no `<time>`) GPX track using a content hash
/// of its lat/lon sequence. This avoids the 1970-epoch collision.
fn gpx_route_guid(rows: &[Value], trk_idx: usize) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    trk_idx.hash(&mut h);
    for row in rows {
        if let Some(obj) = row.as_object() {
            if let Some(lat) = obj.get("lat").and_then(Value::as_str) {
                lat.hash(&mut h);
            }
            if let Some(lon) = obj.get("lon").and_then(Value::as_str) {
                lon.hash(&mut h);
            }
        }
    }
    format!("suunto-gpx|undated|{:016x}", h.finish())
}

/// Normalize an extension tag name by stripping any XML namespace prefix.
/// `gpxtpx:hr` → `hr`, `hr` → `hr`.
fn strip_ns_prefix(tag: &str) -> &str {
    tag.rfind(':').map(|i| &tag[i + 1..]).unwrap_or(tag)
}

// ---------------------------------------------------------------------------
// Timestamp normalisation to UTC

/// Parse an RFC3339 timestamp and re-emit it as UTC with a `Z` suffix, so
/// guids and partition keys are machine-timezone-independent.
fn normalise_ts(ts: &str) -> Option<String> {
    let dt = DateTime::parse_from_rfc3339(ts).ok()?;
    Some(dt.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

// ---------------------------------------------------------------------------
// Dedupe helpers

fn stored_activity_guids(vault: &Vault) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    let root = vault.resolve(WORKOUTS_DIR)?;
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

// ---------------------------------------------------------------------------
// Write helpers

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
// Small utilities

fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "activity".into() } else { s }
}

fn activity_filename(guid: &str) -> String {
    let clean = sanitize(guid);
    if clean == guid { clean } else { format!("{clean}-{}", short_hash(guid)) }
}

fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

fn value_scalar_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

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

fn file_stem(name: &str) -> Option<String> {
    let base = name.rsplit('/').next().unwrap_or(name);
    Path::new(base).file_stem().map(|s| s.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-suunto-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Reuse the canonical Garmin FIT-SDK `Activity.fit` fixture. Standard
    /// FIT binary; tests Suunto-specific vault paths and source tagging.
    const ACTIVITY_FIT: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/garmin-activity.fit"
    ));

    /// Minimal but structurally correct GPX document with one track and
    /// two track-points. Uses the real Garmin/Suunto TrackPointExtension
    /// namespace shape (gpxtpx:TrackPointExtension wrapping gpxtpx:hr etc.)
    /// as actually produced by the Suunto app and Garmin devices.
    const ACTIVITY_GPX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="Suunto app"
     xmlns="http://www.topografix.com/GPX/1/1"
     xmlns:gpxtpx="http://www.garmin.com/xmlschemas/TrackPointExtension/v1">
  <metadata>
    <name>Morning Trail Run</name>
    <time>2026-06-10T07:30:00Z</time>
    <desc>Easy run in the park</desc>
  </metadata>
  <trk>
    <name>Trail Run</name>
    <trkseg>
      <trkpt lat="60.169857" lon="24.938379">
        <ele>15.0</ele>
        <time>2026-06-10T07:30:00Z</time>
        <extensions>
          <gpxtpx:TrackPointExtension>
            <gpxtpx:hr>142</gpxtpx:hr>
            <gpxtpx:cad>85</gpxtpx:cad>
            <gpxtpx:atemp>12.5</gpxtpx:atemp>
          </gpxtpx:TrackPointExtension>
        </extensions>
      </trkpt>
      <trkpt lat="60.170012" lon="24.939001">
        <ele>16.2</ele>
        <time>2026-06-10T07:30:05Z</time>
        <extensions>
          <gpxtpx:TrackPointExtension>
            <gpxtpx:hr>145</gpxtpx:hr>
            <gpxtpx:cad>87</gpxtpx:cad>
            <gpxtpx:atemp>12.5</gpxtpx:atemp>
          </gpxtpx:TrackPointExtension>
        </extensions>
      </trkpt>
    </trkseg>
  </trk>
</gpx>"#;

    // -----------------------------------------------------------------------
    // FIT import

    #[test]
    fn fit_decode_writes_suunto_vault_path_and_source_tag() {
        let v = temp_vault("fit");
        let fit = v.root().join("morning_run.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();

        let out = run(&v, &fit);
        assert_eq!(out.counts.get("activities"), Some(&1));
        assert_eq!(out.counts.get("activity_records"), Some(&22), "all 22 messages preserved");
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Vault path is health/suunto/workouts/, NOT health/garmin/ or health/coros/.
        let month_dir = v.root().join("health/suunto/workouts/2012-04");
        assert!(month_dir.exists(), "suunto vault path used");

        let files: Vec<_> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1);

        let body = fs::read_to_string(files[0].path()).unwrap();
        let lines: Vec<Value> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // Source tag must be "suunto", not "garmin" or "coros".
        for line in &lines {
            assert_eq!(line["source"], Value::String("suunto".into()), "source=suunto");
            assert!(line["guid"].is_string());
            assert!(line["message"].is_string());
        }

        // GPS stays in the activity stream — never split to location/.
        assert!(!v.root().join("location").exists(), "GPS never split to location/");
    }

    #[test]
    fn fit_reimport_is_noop() {
        let v = temp_vault("fit-dedup");
        let fit = v.root().join("run.fit");
        fs::write(&fit, ACTIVITY_FIT).unwrap();

        let first = run(&v, &fit);
        assert_eq!(first.counts.get("activities"), Some(&1));

        let again = run(&v, &fit);
        assert_eq!(again.counts.get("activities"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&1));
    }

    #[test]
    fn fit_guid_is_utc_normalised() {
        // Same FIT moment in different timezone representations must produce
        // the same guid and land in the same partition.
        assert_eq!(
            normalise_ts("2012-04-09T17:22:26-04:00"),
            normalise_ts("2012-04-09T21:22:26+00:00"),
            "UTC normalisation makes tz-different representations equal"
        );
        assert_eq!(
            normalise_ts("2012-04-09T17:22:26-04:00").as_deref(),
            Some("2012-04-09T21:22:26Z")
        );
    }

    #[test]
    fn corrupt_fit_returns_error() {
        let v = temp_vault("corrupt");
        let bad = v.root().join("bad.fit");
        fs::write(&bad, b"this is not a fit file").unwrap();
        let result = (IMPORT.run)(&v, &bad, &BTreeMap::new(), &mut |_| {});
        assert!(result.is_err(), "corrupt FIT must return Err");
    }

    // -----------------------------------------------------------------------
    // GPX import

    #[test]
    fn gpx_import_writes_trkpts_with_extensions() {
        let v = temp_vault("gpx");
        let gpx = v.root().join("morning_trail.gpx");
        fs::write(&gpx, ACTIVITY_GPX).unwrap();

        let out = run(&v, &gpx);
        assert_eq!(out.counts.get("activities"), Some(&1));
        assert_eq!(
            out.counts.get("activity_records"),
            Some(&2),
            "two trkpts → two rows"
        );
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Vault path: health/suunto/workouts/2026-06/<guid>.jsonl
        let month_dir = v.root().join("health/suunto/workouts/2026-06");
        assert!(month_dir.exists(), "GPX partitioned by track start month");

        let files: Vec<_> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1);

        let body = fs::read_to_string(files[0].path()).unwrap();
        let rows: Vec<Value> = body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);

        // Both rows carry the same guid, source, and activity_start.
        assert_eq!(rows[0]["guid"], rows[1]["guid"], "same guid on all trkpts");
        assert_eq!(rows[0]["source"], Value::String("suunto".into()));
        assert_eq!(
            rows[0]["activity_start"],
            Value::String("2026-06-10T07:30:00Z".into())
        );

        // GPS coordinates and time are present.
        assert_eq!(rows[0]["lat"], Value::String("60.169857".into()), "lat preserved");
        assert_eq!(rows[0]["lon"], Value::String("24.938379".into()), "lon preserved");
        assert_eq!(rows[0]["ele"], Value::String("15.0".into()), "elevation preserved");
        assert_eq!(rows[0]["time"], Value::String("2026-06-10T07:30:00Z".into()));

        // Extensions (HR, cadence, temperature) are stored.
        let ext = rows[0]["extensions"].as_object().expect("extensions object");
        assert_eq!(ext.get("hr"), Some(&Value::String("142".into())), "HR in extensions");
        assert_eq!(ext.get("cad"), Some(&Value::String("85".into())), "cadence in extensions");
        assert_eq!(
            ext.get("atemp"),
            Some(&Value::String("12.5".into())),
            "air temp in extensions"
        );

        // _kind marks the element type.
        assert_eq!(rows[0]["_kind"], Value::String("trkpt".into()));
    }

    #[test]
    fn gpx_reimport_is_noop() {
        let v = temp_vault("gpx-dedup");
        let gpx = v.root().join("run.gpx");
        fs::write(&gpx, ACTIVITY_GPX).unwrap();

        let first = run(&v, &gpx);
        assert_eq!(first.counts.get("activities"), Some(&1));

        let again = run(&v, &gpx);
        assert_eq!(again.counts.get("activities"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&1));
    }

    // -----------------------------------------------------------------------
    // ZIP import (FIT + GPX inside)

    #[test]
    fn zip_import_routes_fit_and_gpx_entries() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("export.zip");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            // A FIT activity.
            z.start_file("activities/12345.fit", opts).unwrap();
            z.write_all(ACTIVITY_FIT).unwrap();
            // A GPX activity.
            z.start_file("activities/morning.gpx", opts).unwrap();
            z.write_all(ACTIVITY_GPX.as_bytes()).unwrap();
            // A bulk JSON (unknown format → raw/).
            z.start_file("export.json", opts).unwrap();
            z.write_all(br#"{"workouts":[]}"#).unwrap();
            z.finish().unwrap();
        }

        let out = run(&v, &zip_path);
        // 1 FIT activity + 1 GPX activity.
        assert_eq!(out.counts.get("activities"), Some(&2));
        // 1 raw file (the JSON).
        assert_eq!(out.counts.get("raw_files"), Some(&1));

        // FIT went to health/suunto/workouts/2012-04/.
        let fit_dir = v.root().join("health/suunto/workouts/2012-04");
        assert!(fit_dir.exists(), "FIT month partition written");

        // GPX went to health/suunto/workouts/2026-06/.
        let gpx_dir = v.root().join("health/suunto/workouts/2026-06");
        assert!(gpx_dir.exists(), "GPX month partition written");

        // Bulk JSON preserved in health/suunto/raw/.
        let raw_file = v.root().join("health/suunto/raw/export.json");
        assert!(raw_file.exists(), "bulk JSON preserved verbatim");
        let raw_body = fs::read_to_string(&raw_file).unwrap();
        assert!(raw_body.contains("workouts"), "raw content intact");
    }

    #[test]
    fn zip_reimport_is_noop_for_activities() {
        let v = temp_vault("zip-dedup");
        let zip_path = v.root().join("suunto.zip");
        {
            let mut z = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("run.fit", opts).unwrap();
            z.write_all(ACTIVITY_FIT).unwrap();
            z.finish().unwrap();
        }

        let first = run(&v, &zip_path);
        assert_eq!(first.counts.get("activities"), Some(&1));

        let again = run(&v, &zip_path);
        assert_eq!(again.counts.get("activities"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&1));
    }

    // -----------------------------------------------------------------------
    // DEF / registry

    #[test]
    fn def_is_default_off_import_only_no_connection() {
        assert!(!DEF.meta.default_on, "GPS trails => opt-in");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no login — pure file import");

        let import = DEF.import_spec().unwrap();
        assert!(import.accepts.contains(&"fit"), "accepts .fit");
        assert!(import.accepts.contains(&"gpx"), "accepts .gpx");
        assert!(import.accepts.contains(&"zip"), "accepts .zip");

        // Setup copy carries GPS-trails acknowledgement.
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
    fn gpx_guid_includes_utc_start_and_name_slug() {
        // The gpx_guid derives from the UTC-normalised start + per-track name slug.
        let guid = gpx_guid("2026-06-10T07:30:00Z", Some("Morning Run"), 0);
        assert!(guid.starts_with("suunto-gpx|"), "guid prefix: {guid}");
        assert!(guid.contains("morning_run"), "name slug: {guid}");
        assert!(guid.contains("2026-06-10"), "date in guid: {guid}");
    }

    // -----------------------------------------------------------------------
    // Multi-track GPX: guid collision fix (defect 1)

    #[test]
    fn gpx_multi_track_produces_distinct_guids_and_files() {
        // Two <trk> elements whose first trkpt shares the same UTC second must
        // produce distinct guids and write two separate JSONL files.
        const MULTI_TRACK_GPX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="Suunto app"
     xmlns="http://www.topografix.com/GPX/1/1">
  <metadata>
    <name>Shared File Name</name>
  </metadata>
  <trk>
    <name>Leg One</name>
    <trkseg>
      <trkpt lat="60.1" lon="24.9">
        <time>2026-06-15T08:00:00Z</time>
      </trkpt>
    </trkseg>
  </trk>
  <trk>
    <name>Leg Two</name>
    <trkseg>
      <trkpt lat="60.2" lon="24.8">
        <time>2026-06-15T08:00:00Z</time>
      </trkpt>
    </trkseg>
  </trk>
</gpx>"#;

        let v = temp_vault("gpx-multitrack");
        let gpx = v.root().join("combined.gpx");
        fs::write(&gpx, MULTI_TRACK_GPX).unwrap();

        let out = run(&v, &gpx);
        assert_eq!(out.counts.get("activities"), Some(&2), "two distinct activities");
        assert_eq!(out.counts.get("duplicates"), Some(&0), "no collision/dup");

        // Both activities should land in the same month partition.
        let month_dir = v.root().join("health/suunto/workouts/2026-06");
        assert!(month_dir.exists(), "month partition written");

        let files: Vec<_> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 2, "two separate JSONL files for two tracks");

        // The guids on the two files must differ.
        let guids: Vec<String> = files
            .iter()
            .map(|e| {
                let body = fs::read_to_string(e.path()).unwrap();
                let first_line = body.lines().find(|l| !l.trim().is_empty()).unwrap();
                let obj: Value = serde_json::from_str(first_line).unwrap();
                obj["guid"].as_str().unwrap().to_string()
            })
            .collect();
        assert_ne!(guids[0], guids[1], "guids must be distinct: {:?}", guids);
    }

    // -----------------------------------------------------------------------
    // Route-only / no-time GPX: 1970 partition fix (defect 2)

    #[test]
    fn gpx_route_only_no_time_uses_undated_bucket_not_1970() {
        // A "GPX Files (Route Only)" export has no <time> elements. It must NOT
        // land in health/suunto/workouts/1970-01/ and two distinct route exports
        // must NOT collide to the same guid.
        // Route-only GPX: trkpt elements have lat/lon attrs but no <time> child.
        // Uses explicit open/close tags (self-closing would be Event::Empty which
        // the streaming parser does not need to handle for full exports).
        const ROUTE_GPX_A: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="Suunto app"
     xmlns="http://www.topografix.com/GPX/1/1">
  <metadata>
    <name>Summit Route</name>
  </metadata>
  <trk>
    <name>Summit Route</name>
    <trkseg>
      <trkpt lat="60.100" lon="24.900"><ele>120.0</ele></trkpt>
      <trkpt lat="60.101" lon="24.901"><ele>121.0</ele></trkpt>
    </trkseg>
  </trk>
</gpx>"#;

        const ROUTE_GPX_B: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<gpx version="1.1" creator="Suunto app"
     xmlns="http://www.topografix.com/GPX/1/1">
  <metadata>
    <name>Summit Route</name>
  </metadata>
  <trk>
    <name>Summit Route</name>
    <trkseg>
      <trkpt lat="61.200" lon="25.800"><ele>200.0</ele></trkpt>
      <trkpt lat="61.201" lon="25.801"><ele>201.0</ele></trkpt>
    </trkseg>
  </trk>
</gpx>"#;

        let v = temp_vault("gpx-route");
        let gpx_a = v.root().join("route_a.gpx");
        let gpx_b = v.root().join("route_b.gpx");
        fs::write(&gpx_a, ROUTE_GPX_A).unwrap();
        fs::write(&gpx_b, ROUTE_GPX_B).unwrap();

        let out_a = run(&v, &gpx_a);
        assert_eq!(out_a.counts.get("activities"), Some(&1), "route A imported");

        let out_b = run(&v, &gpx_b);
        assert_eq!(out_b.counts.get("activities"), Some(&1), "route B imported");
        assert_eq!(out_b.counts.get("duplicates"), Some(&0), "routes must not collide");

        // Must NOT write to the 1970 junk partition.
        assert!(
            !v.root().join("health/suunto/workouts/1970-01").exists(),
            "route-only GPX must not land in 1970-01/"
        );

        // Must write to the "undated" bucket.
        let undated_dir = v.root().join("health/suunto/workouts/undated");
        assert!(undated_dir.exists(), "route-only GPX uses undated/ bucket");

        let files: Vec<_> = fs::read_dir(&undated_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 2, "two distinct route files in undated/");
    }

    // -----------------------------------------------------------------------
    // Extension namespace normalisation: gpxtpx:hr → hr (defect 3)

    #[test]
    fn gpx_extension_namespace_prefix_stripped_to_local_name() {
        // The ACTIVITY_GPX fixture uses real gpxtpx:TrackPointExtension nesting.
        // After namespace-prefix stripping, the extension keys must be "hr",
        // "cad", "atemp" (not "gpxtpx:hr" etc.).
        let v = temp_vault("gpx-ext-ns");
        let gpx = v.root().join("run.gpx");
        fs::write(&gpx, ACTIVITY_GPX).unwrap();
        run(&v, &gpx);

        let month_dir = v.root().join("health/suunto/workouts/2026-06");
        let files: Vec<_> = fs::read_dir(&month_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(files.len(), 1);

        let body = fs::read_to_string(files[0].path()).unwrap();
        let first_row: Value =
            serde_json::from_str(body.lines().find(|l| !l.trim().is_empty()).unwrap()).unwrap();
        let ext = first_row["extensions"].as_object().expect("extensions object");

        // Keys must use the local name (no namespace prefix).
        assert_eq!(ext.get("hr"), Some(&Value::String("142".into())), "hr normalised");
        assert_eq!(ext.get("cad"), Some(&Value::String("85".into())), "cad normalised");
        assert_eq!(ext.get("atemp"), Some(&Value::String("12.5".into())), "atemp normalised");

        // Must NOT contain prefixed keys.
        assert!(ext.get("gpxtpx:hr").is_none(), "no prefixed key gpxtpx:hr");
    }
}
