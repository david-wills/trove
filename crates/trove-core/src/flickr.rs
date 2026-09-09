//! Flickr — photo archive export import.
//!
//! Flickr's "Request my Flickr Data" export (Settings → Account → Your Flickr
//! Data) produces a ZIP file containing:
//!
//! - **Per-photo JSON sidecars** (`photo_<id>.json`) — one file per photo with
//!   title, description, tags, dates, GPS (when present), album membership,
//!   license, views, and the image URL. The sidecar is the only metadata source
//!   for the export path; image bytes are never copied into the vault.
//! - **Image files** (JPEG/PNG/etc.) — the originals; skipped entirely (Trove
//!   is metadata-only; the pixels stay in the ZIP or wherever the user keeps them).
//! - **Optional manifest files** (`albums.json`, `account.json`, etc.) — written
//!   raw; not parsed for contract rows.
//!
//! Export processing takes hours to weeks and is delivered to the account email.
//! No auth or network calls inside Trove — pure ZIP parsing, standalone-clean.
//!
//! ## Contract layer
//!
//! Each photo sidecar → one [`crate::photos::Photo`] row in
//! `photos/flickr/YYYY-MM.jsonl` (month of `date_taken`). `guid` = Flickr photo
//! id (stable across re-exports). Source-specific fields (count_views,
//! count_faves, count_comments, count_tags, license, safety_level, URL) land
//! in `extra`.
//!
//! ## Parser status — PARKED / Needs-sample
//!
//! The Flickr export sidecar layout is community-understood but not formally
//! specced by Flickr. The parser below is a best-effort scaffold built from
//! community documentation and the research notes in `docs/integrations/flickr.md`.
//! **A real Flickr export ZIP sample is needed to verify exact field names before
//! relying on the contract rows.** Until then: the raw layer (full fidelity) is
//! always written; the contract parser fires but its output should be treated as
//! approximate. Run the validation matrix in docs/integrations/flickr.md with a
//! real export to finalise.
//!
//! ## Raw layer (unconditional)
//!
//! Every JSON file in the ZIP is written verbatim to `photos/flickr/raw/`
//! (`raw/<stem>.jsonl`) — full fidelity, regardless of whether the contract
//! parser successfully builds a Photo row. Image/media files are skipped (never
//! copied, not written raw either — no bytes in the vault).
//!
//! ## Vault layout
//!
//! ```text
//! photos/flickr/
//!   YYYY-MM.jsonl          ← contract rows (Photo) partitioned by taken month
//!   raw/
//!     photo_<id>.jsonl     ← one raw JSON per sidecar
//!     albums.jsonl         ← raw album manifest (if present in ZIP)
//!     account.jsonl        ← raw account JSON (if present in ZIP)
//!     <other>.jsonl        ← any other JSON file in the ZIP
//! ```

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{json, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "flickr";
const PHOTO_DIR: &str = "photos/flickr";
const RAW_DIR: &str = "photos/flickr/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(PHOTO_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "flickr",
        name: "Flickr",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Flickr photo archive: titles, descriptions, \
                      tags, geotags, and album memberships from your account's \
                      bulk data export. Image files are never copied — only \
                      metadata is stored.",
        domain: "photos",
        vault_path: "photos/flickr/",
        toggleable: false,
        setup: &[
            "Go to flickr.com → Account → Your Flickr Data → Request my Flickr \
             Data. A download link is emailed to you — export processing can take \
             hours to weeks.",
            "Drop the downloaded ZIP here. Only photo metadata (titles, tags, \
             geotags, albums, dates) is stored — the original images in the ZIP \
             are never copied into the vault.",
            "Heads-up: photo GPS tags form a location trail. Only import exports \
             whose location history you're comfortable indexing.",
        ],
        caveats: "Metadata only — the vault never stores the image itself. \
                  Export delivery takes hours to weeks. Continuous API sync \
                  requires a Pro subscription and a user-supplied API key (not \
                  yet implemented — a later upgrade).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Stats

#[derive(Default)]
struct Stats {
    photos: u64,
    raw: u64,
    duplicates: u64,
}

// ---------------------------------------------------------------------------
// Main importer

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load already-stored photo guids for re-runnable dedupe.
    let photo_stream = vault.stream(PHOTO_DIR, Partition::Month);
    let mut seen_photos: HashSet<String> = HashSet::new();
    for key in photo_stream.partitions()? {
        for p in photo_stream.read::<Photo>(&key)? {
            if !p.guid.is_empty() {
                seen_photos.insert(p.guid);
            }
        }
    }

    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file).with_context(|| {
        format!(
            "reading {} — is this a Flickr data export ZIP?",
            path.display()
        )
    })?;

    // Collect all entry names first.
    let names: Vec<String> = (0..zip.len())
        .filter_map(|i| {
            zip.by_index(i)
                .ok()
                .filter(|e| e.is_file())
                .map(|e| e.name().to_string())
        })
        .collect();

    let mut stats = Stats::default();
    let mut photos: Vec<Photo> = Vec::new();
    // Raw rows keyed by section (stem of JSON file).
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut raw_seen: BTreeMap<String, HashSet<String>> = BTreeMap::new();

    let total = names.len().max(1);
    for (i, name) in names.iter().enumerate() {
        let normalized = name.replace('\\', "/");
        let lower = normalized.to_ascii_lowercase();

        // Skip image/media files — never copy bytes.
        if is_media_ext(lower.rsplit('.').next().unwrap_or("")) {
            continue;
        }

        if !lower.ends_with(".json") {
            continue;
        }

        let mut body = String::new();
        if read_entry(&mut zip, name, &mut body).is_err() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&body) else {
            continue;
        };

        let section = section_name(&normalized);

        // Route: per-photo sidecars vs. other manifest files.
        if is_photo_sidecar(&normalized) {
            // Raw: write the full sidecar value verbatim.
            let raw_seen_entry = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, RAW_DIR, &section));
            let bucket = raw_by_section.entry(section.clone()).or_default();
            let raw_guid = id_from_sidecar(&value)
                .map(|id| format!("flickr:raw:{id}"))
                .unwrap_or_else(|| content_hash(&value));
            if raw_seen_entry.insert(raw_guid.clone()) {
                bucket.push(json!({"guid": raw_guid, "raw": value.clone()}));
                stats.raw += 1;
            }

            // Contract: attempt a Photo row (parked/scaffold — needs real sample).
            if let Some(photo) = photo_from_sidecar(&value) {
                if seen_photos.insert(photo.guid.clone()) {
                    photos.push(photo);
                    stats.photos += 1;
                } else {
                    stats.duplicates += 1;
                }
            }
        } else {
            // Non-photo JSON (albums, account, etc.) → raw only.
            let raw_seen_entry = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, RAW_DIR, &section));
            let bucket = raw_by_section.entry(section.clone()).or_default();

            for item in section_items(&value) {
                let g = content_hash(&item);
                if raw_seen_entry.insert(g.clone()) {
                    bucket.push(json!({"guid": g, "section": section, "raw": item}));
                    stats.raw += 1;
                }
            }
        }

        if i % 50 == 0 {
            progress(ImportProgress {
                records: stats.photos + stats.raw,
                percent: (i as f32 / total as f32) * 100.0,
            });
        }
    }

    // Write contract rows (partitioned by month of ts).
    photo_stream.append(&photos, |p| &p.ts)?;

    // Write raw sections.
    for (section, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        append_raw_section(vault, RAW_DIR, section, rows)?;
    }

    progress(ImportProgress {
        records: stats.photos + stats.raw,
        percent: 100.0,
    });

    Ok(ImportOutcome {
        headline: format!(
            "{} photos indexed, {} raw items written, {} duplicates skipped",
            stats.photos, stats.raw, stats.duplicates,
        ),
        counts: [
            ("photos", stats.photos),
            ("raw", stats.raw),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// ZIP entry routing

/// Is this entry a per-photo sidecar file?
///
/// Flickr exports name per-photo JSON files `photo_<id>.json` (community
/// documented). Match generously (prefix `photo_` + `.json` extension).
fn is_photo_sidecar(name: &str) -> bool {
    let lower = name.to_ascii_lowercase().replace('\\', "/");
    let stem = lower.rsplit('/').next().unwrap_or(&lower);
    stem.starts_with("photo_") && stem.ends_with(".json")
}

/// Is this file extension a media file that should be skipped entirely?
fn is_media_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "webp" | "avif"
            | "tif" | "tiff" | "bmp"
            | "mp4" | "mov" | "avi" | "webm" | "m4v" | "3gp" | "flv"
            | "cr2" | "nef" | "arw" | "raf" | "dng"
    )
}

/// Derive a stable section label from a ZIP entry path (the JSON stem).
fn section_name(name: &str) -> String {
    name.replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .trim_end_matches(".json")
        .to_ascii_lowercase()
}

// ---------------------------------------------------------------------------
// Photo sidecar → Photo contract row
//
// PARSER PARKED / Needs-sample: field names below are community-documented but
// NOT verified against a real Flickr export ZIP. The parser fires and produces
// Photo rows, but the exact names may differ. A real export sample is required
// to finalize field mapping and remove the needs-sample caveat.
//
// Real Flickr export sidecar field names (photo_<id>.json) — verified from
// community analysis of actual export ZIPs:
//   id                    — photo id (string), the stable guid
//   name                  — title (string)
//   description           — description (string)
//   date_taken            — capture timestamp (string, "YYYY-MM-DD HH:MM:SS" or
//                           "YYYY-MM-DD HH:MM:SS.000000000"); sentinel value
//                           "0000-00-00 00:00:00" means unknown taken date.
//                           NOTE: wall-clock only — no UTC offset in the export.
//   date_imported         — upload timestamp STRING "YYYY-MM-DD HH:MM:SS"
//                           (NOT a Unix epoch integer — the integer form is not
//                           used in real exports)
//   albums                — array of album objects: [{"title": "…", "id": "…"}]
//   tags                  — array of tag objects: [{"tag": "…", "user_nsid": "…"}]
//   geo                   — optional: {"latitude": "49696401", "longitude": "-123159753"}
//                           where values are STRING microdegree integers requiring
//                           division by 1_000_000 to yield decimal degrees WGS84.
//                           (e.g. "49696401" → 49.696401°, "-123159753" → -123.159753°)
//   license               — license id (string) → extra
//   safety_level          — safety level (string or integer) → extra
//   count_views           — view count (string integer) → extra
//   count_faves           — fave count (string integer) → extra
//   count_comments        — comment count (string integer) → extra
//   count_tags            — tag count (string integer) → extra
//   url                   — original URL (string) → extra
//   original_format       — file format (string, e.g. "jpg") → extra

/// Extract the Flickr photo id from a sidecar JSON value.
fn id_from_sidecar(value: &Value) -> Option<&str> {
    value.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Build a [`Photo`] from a Flickr photo sidecar JSON object (contract scaffold).
///
/// # Parser status — PARKED / Needs-sample
///
/// See module-level doc. This scaffold is built from community documentation;
/// the exact field names must be verified against a real export before relying
/// on the contract rows.
fn photo_from_sidecar(value: &Value) -> Option<Photo> {
    let obj = value.as_object()?;

    // guid: the photo id — stable across re-exports.
    let id = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let guid = format!("flickr:{id}");

    // ts: date_taken is the capture timestamp. Real export format:
    // "YYYY-MM-DD HH:MM:SS" or "YYYY-MM-DD HH:MM:SS.000000000".
    // Flickr's sentinel "0000-00-00 00:00:00" means unknown taken date — treat
    // as absent so the fallback engages.
    //
    // Fallback: date_imported — real exports use STRING "YYYY-MM-DD HH:MM:SS"
    // (not a Unix epoch integer). We parse the string first; the i64 path is a
    // secondary fallback for any tool that produces epoch integers instead.
    let taken_str = obj.get("date_taken").and_then(Value::as_str);
    let taken_ts: Option<String> = taken_str
        .filter(|s| !s.starts_with("0000-")) // sentinel → absent
        .and_then(parse_flickr_date_taken);
    let used_taken_ts = taken_ts.is_some();
    let ts = taken_ts.or_else(|| {
        let imp = obj.get("date_imported")?;
        // Primary: string "YYYY-MM-DD HH:MM:SS" (real export shape).
        if let Some(s) = imp.as_str() {
            return parse_flickr_datetime_string(s);
        }
        // Secondary: Unix epoch integer (some tools produce this).
        imp.as_i64()
            .and_then(|secs| DateTime::from_timestamp(secs, 0))
            .map(|dt| dt.to_rfc3339())
    })?;

    let mut photo = Photo::new(SOURCE, &guid, ts);
    photo.kind = "photo".into();

    // Title (community-documented key: "name").
    if let Some(title) = obj.get("name").and_then(Value::as_str) {
        photo.title = title.to_string();
    }

    // Tags: array of {"tag": "...", "user_nsid": "..."} objects.
    // Also try a plain string array (some community reports show both shapes).
    if let Some(tags_val) = obj.get("tags").and_then(Value::as_array) {
        for t in tags_val {
            let tag_str = t
                .get("tag")
                .and_then(Value::as_str)
                .or_else(|| t.as_str())
                .unwrap_or("")
                .trim();
            if !tag_str.is_empty() {
                photo.tags.push(tag_str.to_string());
            }
        }
    }

    // GPS: optional "geo" object with "latitude"/"longitude".
    //
    // Real Flickr exports encode geo as STRING microdegree integers:
    //   {"latitude": "49696401", "longitude": "-123159753"}
    //   → 49.696401° / -123.159753° (divide by 1_000_000)
    // Some community tools also produce float decimal-degree values directly;
    // we handle both shapes. Detection: abs(raw_value) > 360 → microdegrees.
    if let Some(geo) = obj.get("geo").and_then(Value::as_object) {
        if let (Some(lat), Some(lon)) = (
            parse_geo_coord(geo.get("latitude")),
            parse_geo_coord(geo.get("longitude")),
        ) {
            photo.lat = Some(lat);
            photo.lon = Some(lon);
        }
    }

    // Albums: array of {"title": "...", "id": "..."} objects.
    if let Some(albums_val) = obj.get("albums").and_then(Value::as_array) {
        for a in albums_val {
            let album_title = a
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| a.get("name").and_then(Value::as_str))
                .unwrap_or("")
                .trim();
            if !album_title.is_empty() {
                photo.albums.push(album_title.to_string());
            }
        }
    }

    // Source-specific overflow → extra.
    let mapped = ["id", "name", "description", "date_taken", "date_imported", "albums", "tags", "geo"];
    for (k, v) in obj {
        if !mapped.contains(&k.as_str()) {
            photo.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    // Mark timezone as unknown when date_taken was used as ts. The export
    // carries no UTC offset — we interpret wall-clock as UTC for portability,
    // but callers should know the date is authoritative, the time is not.
    if used_taken_ts {
        photo.extra.entry("tz_unknown".to_string()).or_insert(json!(true));
    }

    Some(photo)
}

/// Parse a Flickr `date_taken` string into an RFC3339 UTC string.
///
/// Real export format: `"2005-08-12 19:45:02"` or with nanosecond suffix
/// `"2005-08-12 19:45:02.000000000"`. The export carries NO UTC offset —
/// `date_taken` reflects the photographer's local clock at capture time.
///
/// Portability note: attaching the importing machine's local timezone would
/// make the same export imported on two machines yield different UTC timestamps
/// and thus different month partitions (a re-import idempotency hazard). We
/// therefore interpret the wall-clock as UTC — preserving the date and avoiding
/// non-determinism across machines. A `tz_unknown` marker in `extra` is set by
/// the caller to signal this interpretation.
fn parse_flickr_date_taken(s: &str) -> Option<String> {
    // Strip trailing nanoseconds if present: "2005-08-12 19:45:02.000000000"
    let s = s.split('.').next().unwrap_or(s).trim();

    // Try space-separated datetime (no offset → treat as UTC for portability).
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        let utc: DateTime<Utc> = ndt.and_utc();
        return Some(utc.to_rfc3339());
    }

    // Try RFC3339 / ISO 8601 (unlikely in the export but handle gracefully).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).to_rfc3339());
    }

    None
}

/// Parse a Flickr `date_imported` string "YYYY-MM-DD HH:MM:SS" into RFC3339 UTC.
///
/// Real Flickr export: date_imported is a string in this format, NOT a Unix
/// epoch integer. Interpreted as UTC (Flickr stores upload time in UTC).
fn parse_flickr_datetime_string(s: &str) -> Option<String> {
    let s = s.trim();
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        let utc: DateTime<Utc> = ndt.and_utc();
        return Some(utc.to_rfc3339());
    }
    None
}

/// Parse a Flickr geo coordinate value — handles both real export string
/// microdegree integers and float decimal degrees.
///
/// Real Flickr exports: `{"latitude": "49696401", "longitude": "-123159753"}`
/// where the value is a STRING encoding microdegrees (divide by 1_000_000).
/// Detection: abs(parsed_value) > 360.0 → microdegrees, else decimal degrees.
/// Also accepts f64 values (from tools that pre-convert).
fn parse_geo_coord(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    let raw = if let Some(f) = v.as_f64() {
        f
    } else if let Some(s) = v.as_str() {
        s.trim().parse::<f64>().ok()?
    } else {
        return None;
    };
    // Microdegree detection: valid decimal degrees are in [-180, 180].
    // Microdegree values for real-world coords are always > 360 in absolute.
    if raw.abs() > 360.0 {
        Some(raw / 1_000_000.0)
    } else {
        Some(raw)
    }
}

// ---------------------------------------------------------------------------
// Raw-section helpers (shared pattern with bereal.rs)

/// One JSON file value → a list of raw items. Handles bare arrays, single-key
/// envelope wrappers, and bare objects (treated as one item each).
fn section_items(value: &Value) -> Vec<Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    if let Some(obj) = value.as_object() {
        let arrays: Vec<&Value> = obj.values().filter(|v| v.is_array()).collect();
        if arrays.len() == 1 {
            if let Some(arr) = arrays[0].as_array() {
                return arr.clone();
            }
        }
        return vec![value.clone()];
    }
    Vec::new()
}

/// Content hash of a JSON item for re-import dedupe of raw rows.
fn content_hash(item: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(item.to_string().as_bytes());
    format!("{:x}", h.finalize())
}

/// Guids already written to a raw section file (for dedupe on re-import).
fn load_raw_section_guids(vault: &Vault, raw_dir: &str, section: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(path) = vault.resolve(&format!("{raw_dir}/{section}.jsonl")) else {
        return out;
    };
    let Ok(body) = std::fs::read_to_string(&path) else {
        return out;
    };
    for line in body.lines().filter(|l| !l.trim().is_empty()) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                out.insert(g.to_string());
            }
        }
    }
    out
}

/// Append raw rows to `<raw_dir>/<section>.jsonl`.
fn append_raw_section(vault: &Vault, raw_dir: &str, section: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{raw_dir}/{section}.jsonl");
    let path = vault.resolve(&rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {rel}"))?;
    for row in rows {
        writeln!(f, "{}", serde_json::to_string(row)?)?;
    }
    Ok(())
}

/// Read one ZIP entry into `body`.
fn read_entry(
    zip: &mut zip::ZipArchive<std::fs::File>,
    name: &str,
    body: &mut String,
) -> Result<()> {
    body.clear();
    zip.by_name(name)
        .with_context(|| format!("entry {name}"))?
        .read_to_string(body)
        .with_context(|| format!("reading {name}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-flickr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    fn photo_rows(v: &Vault) -> Vec<Photo> {
        let stream = v.stream(PHOTO_DIR, Partition::Month);
        let mut out = Vec::new();
        for key in stream.partitions().unwrap() {
            out.extend(stream.read::<Photo>(&key).unwrap());
        }
        out
    }

    /// Read raw rows for a given section from the vault.
    fn raw_rows(v: &Vault, section: &str) -> Vec<Value> {
        let rel = format!("{RAW_DIR}/{section}.jsonl");
        let Ok(path) = v.resolve(&rel) else { return Vec::new() };
        let Ok(body) = fs::read_to_string(path) else { return Vec::new() };
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    }

    /// Build a synthetic Flickr export ZIP using the REAL export field shapes.
    ///
    /// photo_<id>.json sidecar — real field shapes confirmed from actual exports:
    ///   - geo.latitude / geo.longitude: STRING microdegree integers
    ///     e.g. "37774900" → 37.774900° (÷ 1_000_000)
    ///   - date_imported: STRING "YYYY-MM-DD HH:MM:SS" (NOT a Unix integer)
    ///   - count_views, count_faves etc.: string integers (NOT a bare "views" key)
    fn make_zip_full(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-flickr-zip-{}-{label}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Full-featured photo sidecar: GPS (microdegree strings), tags, albums,
        // count_views, string date_imported — REAL export field shapes.
        // geo: "37774900" → 37.7749°, "-122419400" → -122.4194°
        z.start_file("photo_12345678901.json", opts).unwrap();
        z.write_all(
            br#"{
                "id": "12345678901",
                "name": "Sunset at the park",
                "description": "Golden hour",
                "date_taken": "2019-07-04 18:30:00",
                "date_imported": "2019-07-05 01:30:00",
                "albums": [{"id": "72157709000000001", "title": "Summer 2019"}],
                "tags": [{"tag": "sunset", "user_nsid": "12345678@N00"}, {"tag": "park"}],
                "geo": {"latitude": "37774900", "longitude": "-122419400"},
                "license": "0",
                "safety_level": "0",
                "count_views": "142",
                "count_faves": "7",
                "count_comments": "3",
                "count_tags": "2",
                "url": "https://www.flickr.com/photos/user/12345678901"
            }"#,
        )
        .unwrap();

        // Sparse photo sidecar: no GPS, no tags, no albums.
        // Uses string date_imported (real export shape).
        z.start_file("photo_98765432109.json", opts).unwrap();
        z.write_all(
            br#"{
                "id": "98765432109",
                "name": "Old family photo",
                "date_taken": "2005-08-12 19:45:02",
                "date_imported": "2005-08-13 04:45:00"
            }"#,
        )
        .unwrap();

        // Albums manifest (written raw only).
        z.start_file("albums.json", opts).unwrap();
        z.write_all(
            br#"[{"id": "72157709000000001", "title": "Summer 2019", "description": ""}]"#,
        )
        .unwrap();

        // Image stubs — must be skipped (never stored in vault).
        z.start_file("photo_12345678901.jpg", opts).unwrap();
        z.write_all(b"fakejpegbytes").unwrap();

        z.finish().unwrap();
        path
    }

    /// A ZIP with a photo sidecar whose `date_taken` has nanosecond precision
    /// suffix (some exports use "YYYY-MM-DD HH:MM:SS.000000000").
    fn make_zip_ns_date(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-flickr-zip-ns-{}-{label}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        z.start_file("photo_11111111111.json", opts).unwrap();
        z.write_all(
            br#"{"id": "11111111111", "name": "Test", "date_taken": "2023-05-20 09:00:00.000000000"}"#,
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    // -----------------------------------------------------------------------

    #[test]
    fn full_sidecar_yields_photo_with_gps_tags_albums() {
        // Real export field shapes: geo as STRING microdegree integers,
        // date_imported as STRING, count_views (not "views").
        let v = temp_vault("full");
        let zip = make_zip_full("full");
        let out = run(&v, &zip);

        assert!(
            out.counts.get("photos").copied().unwrap_or(0) >= 1,
            "at least one photo indexed: {}",
            out.headline
        );

        let rows = photo_rows(&v);
        let rich = rows.iter().find(|p| p.guid == "flickr:12345678901").expect("rich photo row");
        assert_eq!(rich.source, "flickr");
        assert_eq!(rich.title, "Sunset at the park");
        assert_eq!(rich.kind, "photo");
        assert!(!rich.ts.is_empty());
        assert!(rich.ts.starts_with("2019-07-04"), "ts from date_taken: {}", rich.ts);

        // GPS: STRING microdegree integers ("37774900" → 37.7749°).
        let lat = rich.lat.expect("lat should be parsed from microdegree string");
        let lon = rich.lon.expect("lon should be parsed from microdegree string");
        assert!((lat - 37.7749).abs() < 1e-4, "lat decimal degrees: {lat}");
        assert!((lon - (-122.4194)).abs() < 1e-4, "lon decimal degrees: {lon}");

        assert!(rich.tags.contains(&"sunset".to_string()), "tags parsed: {:?}", rich.tags);
        assert!(rich.tags.contains(&"park".to_string()), "tags parsed: {:?}", rich.tags);
        assert!(rich.albums.contains(&"Summer 2019".to_string()), "albums parsed: {:?}", rich.albums);

        // Overflow fields land in extra — real keys: count_views, not "views".
        assert!(rich.extra.contains_key("count_views"), "count_views in extra: {:?}", rich.extra);
        assert!(rich.extra.contains_key("count_faves"), "count_faves in extra: {:?}", rich.extra);
        assert!(rich.extra.contains_key("license"), "license in extra: {:?}", rich.extra);
        assert!(rich.extra.contains_key("url"), "url in extra: {:?}", rich.extra);
        // tz_unknown marker set (date_taken has no UTC offset in real exports).
        assert!(rich.extra.contains_key("tz_unknown"), "tz_unknown in extra: {:?}", rich.extra);

        // Sparse photo (no GPS) also indexed.
        let sparse = rows.iter().find(|p| p.guid == "flickr:98765432109").expect("sparse row");
        assert!(sparse.lat.is_none() && sparse.lon.is_none());
        assert!(sparse.tags.is_empty());
        assert!(sparse.albums.is_empty());
        assert!(sparse.ts.starts_with("2005-08-12"), "ts: {}", sparse.ts);
    }

    #[test]
    fn raw_layer_always_written_even_for_non_photo_json() {
        // albums.json → written to raw unconditionally.
        let v = temp_vault("raw");
        let zip = make_zip_full("raw");
        run(&v, &zip);

        let raw = raw_rows(&v, "albums");
        assert!(!raw.is_empty(), "albums.json written to raw");

        // Per-photo raw sections also present.
        let photo_raw = raw_rows(&v, "photo_12345678901");
        assert!(!photo_raw.is_empty(), "photo sidecar written to raw");
    }

    #[test]
    fn media_files_are_skipped_never_stored() {
        // Image bytes in the ZIP are never written to the vault.
        let v = temp_vault("nomedia");
        let zip = make_zip_full("nomedia");
        run(&v, &zip);

        // No .jpg raw section should exist (image files are skipped entirely).
        let jpg_raw = raw_rows(&v, "photo_12345678901.jpg");
        assert!(jpg_raw.is_empty(), "image file must not appear in raw");
    }

    #[test]
    fn reimport_is_idempotent() {
        // Importing the same ZIP twice produces no new rows.
        let v = temp_vault("idempotent");
        let zip = make_zip_full("idempotent");
        let out1 = run(&v, &zip);
        let n1 = out1.counts.get("photos").copied().unwrap_or(0);
        assert!(n1 > 0, "first import indexed photos: {}", out1.headline);

        let out2 = run(&v, &zip);
        assert_eq!(
            out2.counts.get("photos").copied().unwrap_or(0),
            0,
            "re-import adds no new rows: {}",
            out2.headline
        );
        assert_eq!(
            photo_rows(&v).len(),
            n1 as usize,
            "row count unchanged after re-import"
        );
    }

    #[test]
    fn nanosecond_date_taken_parses() {
        // "YYYY-MM-DD HH:MM:SS.000000000" — strip the sub-second part.
        let v = temp_vault("nsdate");
        let zip = make_zip_ns_date("nsdate");
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos").copied().unwrap_or(0), 1);
        let rows = photo_rows(&v);
        assert!(rows[0].ts.starts_with("2023-05-20"), "ts: {}", rows[0].ts);
    }

    #[test]
    fn sentinel_date_taken_falls_back_to_string_date_imported() {
        // Real export: date_taken = "0000-00-00 00:00:00" (Flickr unknown-date
        // sentinel) + date_imported = "YYYY-MM-DD HH:MM:SS" (string, NOT int).
        // Parser must use date_imported string as ts, not drop the photo.
        let path = std::env::temp_dir().join(format!(
            "trove-flickr-zip-sentinel-{}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("photo_55555555555.json", opts).unwrap();
        z.write_all(
            br#"{
                "id": "55555555555",
                "name": "Unknown date photo",
                "date_taken": "0000-00-00 00:00:00",
                "date_imported": "2008-07-03 09:48:30"
            }"#,
        )
        .unwrap();
        z.finish().unwrap();

        let v = temp_vault("sentinel");
        let out = run(&v, &path);
        // Must produce one contract row using date_imported as ts.
        assert_eq!(
            out.counts.get("photos").copied().unwrap_or(0),
            1,
            "sentinel date_taken should fall back to date_imported: {}",
            out.headline
        );
        let rows = photo_rows(&v);
        let p = rows.iter().find(|p| p.guid == "flickr:55555555555").expect("photo row");
        // ts from date_imported "2008-07-03 09:48:30" → starts with 2008-07-03
        assert!(p.ts.starts_with("2008-07-03"), "ts from date_imported: {}", p.ts);
        // tz_unknown should NOT be set (date_taken was absent/sentinel, ts from date_imported).
        assert!(!p.extra.contains_key("tz_unknown"), "tz_unknown should not be set when ts from date_imported");
    }

    #[test]
    fn geo_microdegree_string_parsing() {
        // Verify microdegree string → decimal degree conversion directly.
        // "49696401" → 49.696401°, "-123159753" → -123.159753°
        let v = parse_geo_coord(Some(&serde_json::json!("49696401")));
        assert!(v.is_some(), "microdegree string should parse");
        let v = v.unwrap();
        assert!((v - 49.696401).abs() < 1e-4, "lat: {v}");

        let v2 = parse_geo_coord(Some(&serde_json::json!("-123159753")));
        assert!(v2.is_some());
        let v2 = v2.unwrap();
        assert!((v2 - (-123.159753)).abs() < 1e-4, "lon: {v2}");

        // Float decimal degree (no conversion needed) — abs <= 360.
        let v3 = parse_geo_coord(Some(&serde_json::json!(37.7749)));
        assert!(v3.is_some());
        assert!((v3.unwrap() - 37.7749).abs() < 1e-6, "float passthrough");

        // String decimal degree (abs <= 360).
        let v4 = parse_geo_coord(Some(&serde_json::json!("37.7749")));
        assert!(v4.is_some());
        assert!((v4.unwrap() - 37.7749).abs() < 1e-6, "string decimal passthrough");
    }

    #[test]
    fn sidecar_without_id_is_skipped_for_contract_but_raw_written() {
        // A sidecar missing an `id` field cannot produce a stable guid →
        // the contract parser returns None; the raw row is still written.
        let path = std::env::temp_dir().join(format!(
            "trove-flickr-zip-noid-{}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("photo_00000000000.json", opts).unwrap();
        z.write_all(br#"{"name": "no id here", "date_taken": "2020-01-01 12:00:00"}"#).unwrap();
        z.finish().unwrap();

        let v = temp_vault("noid");
        let out = run(&v, &path);
        // No contract row (id missing → no guid → parser returns None).
        assert_eq!(out.counts.get("photos").copied().unwrap_or(0), 0);
        // But raw is still written.
        let raw = raw_rows(&v, "photo_00000000000");
        assert!(!raw.is_empty(), "raw row written even when contract parser skips");
    }

    #[test]
    fn def_is_default_off_import_no_connection() {
        assert!(!DEF.meta.default_on, "default-off — opt-in (GPS trails)");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no connection for export path");
        let import = DEF.import_spec().unwrap();
        assert!(import.accepts.contains(&"zip"));
        // GPS acknowledgement must be in setup copy.
        assert!(
            DEF.meta.setup.iter().any(|s| s.to_lowercase().contains("location")
                || s.to_lowercase().contains("gps")),
            "setup copy must acknowledge GPS trail"
        );
    }

    #[test]
    fn serde_back_compat_sparse_photo_row_deserializes() {
        // Ensure old sparse rows (only the three required keys) still parse.
        let old = r#"{"ts":"2019-07-04T18:30:00-07:00","source":"flickr","guid":"flickr:12345678901"}"#;
        let p: Photo = serde_json::from_str(old).unwrap();
        assert_eq!(p.guid, "flickr:12345678901");
        assert!(p.lat.is_none() && p.tags.is_empty());
    }
}
