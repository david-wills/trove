//! Google Photos — Google Takeout export import.
//!
//! Google permanently revoked library-wide API read access on 2025-03-31.
//! The only bulk path is Takeout: takeout.google.com → Google Photos → ZIP(s).
//!
//! ## Takeout archive structure
//!
//! Each Takeout ZIP contains:
//!
//! - **Original media files** (JPEG/HEIC/MP4/…) — skipped entirely; Trove is
//!   metadata-only and never copies image bytes into the vault.
//! - **Per-item JSON sidecars** — one per photo/video, containing `title`,
//!   `description`, `photoTakenTime`, `creationTime`, `modificationTime`,
//!   `geoData`, `geoDataExif`, `people`, `url`, `favorited`, and
//!   `imageViews`. The sidecar is the authoritative metadata source; EXIF in
//!   the media files is often stripped or wrong.
//! - **Album metadata JSON files** — separate files carrying the album title
//!   and the set of media items it contains. Written raw only; they do not
//!   produce Photo contract rows.
//!
//! ## Sidecar naming edge cases (community-documented)
//!
//! Google Photos Takeout naming is inconsistent (documented by google-photos-exif
//! and metadatafixer):
//!
//! - `IMG_1234.jpg.json` — full-filename sidecar (most common)
//! - `IMG_1234.json` — stem-only sidecar (older exports)
//! - `IMG_1234(1).json` — duplicate sidecar (files named `IMG_1234(1).jpg`)
//!
//! The routing heuristic here is simple: every `.json` file that contains a
//! `photoTakenTime` key is a photo sidecar. Album JSON files lack that key
//! and go raw-only.
//!
//! ## Field names (verified)
//!
//! Verified from the TypeScript interface in mattwilson1024/google-photos-exif
//! (src/models/google-metadata.ts) and from `docs/integrations-research.md` L3102:
//!
//! ```text
//! {
//!   "title": "IMG_1234.jpg",
//!   "description": "Grandma's birthday",
//!   "imageViews": "12",
//!   "creationTime":       { "timestamp": "1665341290", "formatted": "Oct 9, 2022, 6:28:10 PM UTC" },
//!   "modificationTime":   { "timestamp": "1665341290", "formatted": "…" },
//!   "photoTakenTime":     { "timestamp": "1558454400", "formatted": "May 21, 2019, 9:00:00 PM UTC" },
//!   "geoData":     { "latitude": 40.7128, "longitude": -74.0060, "altitude": 10.0,
//!                    "latitudeSpan": 0.01, "longitudeSpan": 0.01 },
//!   "geoDataExif": { "latitude": 40.7128, "longitude": -74.0060, "altitude": 10.0,
//!                    "latitudeSpan": 0.01, "longitudeSpan": 0.01 },
//!   "people": [ { "name": "Alice" } ],
//!   "url": "https://photos.google.com/photo/AF1QipMxyz123",
//!   "favorited": false,
//!   "googlePhotosOrigin": { "mobileUpload": { "deviceType": "IOS_PHONE" } }
//! }
//! ```
//!
//! ## Contract layer
//!
//! Each photo sidecar → one [`crate::photos::Photo`] row in
//! `photos/google-photos/YYYY-MM.jsonl` (month of `photoTakenTime`, UTC).
//! `guid` = the sidecar `url` field (stable per item); falls back to
//! `title + ":" + photoTakenTime.timestamp` when url is absent.
//!
//! ## Raw layer (unconditional — full fidelity)
//!
//! Photo sidecars → `photos/google-photos/raw/YYYY-MM.jsonl` (partitioned by
//! photoTakenTime month). Album JSON → `photos/google-photos/raw/albums.jsonl`.
//! Other JSON (non-sidecar, non-album) → `photos/google-photos/raw/other.jsonl`.
//!
//! ## Dedupe
//!
//! `guid` makes re-imports of overlapping Takeout batches (multi-ZIP libraries)
//! safe — the same sidecar in two ZIPs never duplicates a contract row.
//!
//! ## Privacy
//!
//! `geoData` is a dense location trail; `people` carries face-tag names. This
//! source ships `default_on: false`; setup copy explains the data sensitivity.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "google-photos";
const PHOTO_DIR: &str = "photos/google-photos";
const RAW_DIR: &str = "photos/google-photos/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(PHOTO_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-photos",
        name: "Google Photos",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports photo metadata from your Google Takeout export: \
                      timestamps, GPS coordinates, album names, face tags, and \
                      descriptions from the JSON sidecars. Image files are never \
                      stored. Supports multi-ZIP exports and re-import deduplication.",
        domain: "photos",
        vault_path: "photos/google-photos/",
        toggleable: false,
        setup: &[
            "Go to takeout.google.com, select Google Photos, and download your archive. \
             Large libraries are split into multiple ZIPs — import each one separately. \
             Note: Google permanently revoked API access in March 2025; Takeout is the \
             only supported import path.",
            "Drop a Takeout ZIP here. Only metadata (timestamps, GPS, captions, album \
             names) is stored — the photos and videos in the ZIP are never copied into \
             the vault.",
            "Heads-up: the Google Photos sidecar includes GPS location data forming a \
             detailed location trail, and face-tag names from your library. Only import \
             exports whose location and people data you are comfortable indexing.",
        ],
        caveats: "Metadata only — the vault never stores the image or video itself. \
                  Google permanently revoked library-wide API read access in March 2025; \
                  Takeout is the only supported import path. One-shot import; cannot be \
                  automated — there is no API to trigger a new Takeout.",
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
    // Load already-stored photo guids for re-runnable dedupe across ZIPs.
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
            "reading {} — is this a Google Photos Takeout ZIP?",
            path.display()
        )
    })?;

    // Collect all entry names first (avoids borrow issues while iterating).
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

    // Raw rows: photo sidecars → partitioned by photoTakenTime month;
    // album + other JSONs → flat files.
    let mut raw_by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut raw_albums: Vec<Value> = Vec::new();
    let mut raw_other: Vec<Value> = Vec::new();

    // Dedupe sets for raw rows (guid-based).
    let mut raw_seen_months: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    let mut raw_seen_albums: HashSet<String> = load_raw_flat_guids(vault, "albums");
    let mut raw_seen_other: HashSet<String> = load_raw_flat_guids(vault, "other");

    let total = names.len().max(1);
    for (i, name) in names.iter().enumerate() {
        let normalized = name.replace('\\', "/");
        let lower = normalized.to_ascii_lowercase();

        // Skip media files — metadata-only, never copy bytes.
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

        // Route: photo sidecar (has photoTakenTime) vs. album/other JSON.
        if is_photo_sidecar(&value) {
            // Determine the month partition from photoTakenTime.
            let month_key = photo_taken_month(&value).unwrap_or_else(|| "unknown".to_string());

            // Raw: write photo sidecar verbatim, partitioned by month.
            let raw_seen_entry = raw_seen_months
                .entry(month_key.clone())
                .or_insert_with(|| load_raw_month_guids(vault, &month_key));
            let raw_guid = url_from_sidecar(&value)
                .map(|u| format!("raw:{u}"))
                .or_else(|| title_ts_guid(&value).map(|g| format!("raw:{g}")))
                .unwrap_or_else(|| content_hash(&value));
            if raw_seen_entry.insert(raw_guid.clone()) {
                raw_by_month
                    .entry(month_key)
                    .or_default()
                    .push(json!({"guid": raw_guid, "raw": value.clone()}));
                stats.raw += 1;
            }

            // Contract: build a Photo row if we have a valid timestamp.
            if let Some(photo) = photo_from_sidecar(&value) {
                if seen_photos.insert(photo.guid.clone()) {
                    photos.push(photo);
                    stats.photos += 1;
                } else {
                    stats.duplicates += 1;
                }
            }
        } else if is_album_json(&value) {
            let g = content_hash(&value);
            if raw_seen_albums.insert(g.clone()) {
                raw_albums.push(json!({"guid": g, "raw": value}));
                stats.raw += 1;
            }
        } else {
            // Any other JSON in the export → other.
            let g = content_hash(&value);
            if raw_seen_other.insert(g.clone()) {
                raw_other.push(json!({"guid": g, "raw": value}));
                stats.raw += 1;
            }
        }

        if i % 50 == 0 {
            progress(ImportProgress {
                records: stats.photos + stats.raw,
                percent: (i as f32 / total as f32) * 100.0,
            });
        }
    }

    // Write contract rows (partitioned by month of photoTakenTime).
    photo_stream.append(&photos, |p| &p.ts)?;

    // Write raw photo sidecar rows (partitioned by month).
    for (month, rows) in &raw_by_month {
        if rows.is_empty() {
            continue;
        }
        append_raw_month(vault, month, rows)?;
    }

    // Write flat raw files.
    if !raw_albums.is_empty() {
        append_raw_flat(vault, "albums", &raw_albums)?;
    }
    if !raw_other.is_empty() {
        append_raw_flat(vault, "other", &raw_other)?;
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
// JSON routing predicates

/// Does this JSON value look like a Google Photos per-item sidecar?
/// The presence of `photoTakenTime` is the distinguishing field.
fn is_photo_sidecar(value: &Value) -> bool {
    value.get("photoTakenTime").is_some()
}

/// Does this JSON value look like an album metadata file?
///
/// Google Takeout album JSON files always have a `title` key and lack a
/// `photoTakenTime` key (which is the photo-sidecar distinguisher).  Modern
/// album files sometimes contain only `{title, description, date, access}`
/// with no `mediaItems` / `enrichments` arrays — the old predicate missed
/// those and routed them to raw/other.jsonl instead of raw/albums.jsonl.
///
/// Heuristic: top-level JSON object with `title` AND no `photoTakenTime`
/// (already excluded from photo-sidecar path, so redundant but safe).
fn is_album_json(value: &Value) -> bool {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return false,
    };
    // Must have a title but not be a photo sidecar (no photoTakenTime).
    obj.contains_key("title") && !obj.contains_key("photoTakenTime")
}

/// Is this file extension a media file that should be skipped?
fn is_media_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "png" | "gif" | "heic" | "heif" | "webp" | "avif"
            | "tif" | "tiff" | "bmp"
            | "mp4" | "mov" | "avi" | "webm" | "m4v" | "3gp" | "flv"
            | "cr2" | "nef" | "arw" | "raf" | "dng" | "mp" | "3gpp"
    )
}

// ---------------------------------------------------------------------------
// Photo sidecar → Photo contract row
//
// Field names verified from:
//   mattwilson1024/google-photos-exif: src/models/google-metadata.ts
//   docs/integrations-research.md L3102
//
// Sidecar structure:
//   title             — filename or user title (string)
//   description       — user caption (string) → extra
//   imageViews        — view count string → extra
//   creationTime      — upload time { timestamp: "epoch_secs", formatted: "..." }
//   modificationTime  — { timestamp: "epoch_secs", formatted: "..." } → extra
//   photoTakenTime    — capture time { timestamp: "epoch_secs", formatted: "..." }
//   geoData           — { latitude, longitude, altitude, latitudeSpan, longitudeSpan }
//   geoDataExif       — same structure, from original EXIF (less reliable, fallback)
//   people            — [{ name: "..." }]
//   url               — stable photo URL (guid)
//   favorited         — bool
//   googlePhotosOrigin — { mobileUpload: { deviceType: "..." } } → extra

/// Build a [`Photo`] from a Google Photos Takeout sidecar JSON object.
fn photo_from_sidecar(value: &Value) -> Option<Photo> {
    let obj = value.as_object()?;

    // ts: photoTakenTime.timestamp = epoch seconds (string, UTC).
    // Fall back to creationTime.timestamp if photoTakenTime is absent/zero.
    let ts = parse_google_timestamp(obj.get("photoTakenTime"))
        .or_else(|| parse_google_timestamp(obj.get("creationTime")))?;

    // guid: the stable `url` field. Fall back to title + photoTakenTime epoch.
    let guid = url_from_sidecar(value)
        .or_else(|| title_ts_guid(value))?;

    let mut photo = Photo::new(SOURCE, guid, &ts);

    // Google Takeout carries only UTC epoch; no timezone offset is available.
    // Flag this so read-time logic knows the wall-clock hour is approximate.
    photo.extra.insert("tz_unknown".to_string(), json!(true));

    // Kind: guess from title extension.
    let title_lower = obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    photo.kind = if is_video_ext(title_lower.rsplit('.').next().unwrap_or("")) {
        "video".into()
    } else {
        "photo".into()
    };

    // filename: the sidecar `title` field is always the media file's filename
    // (e.g. "IMG_1234.jpg") — not a user-authored caption.
    if let Some(t) = obj.get("title").and_then(Value::as_str) {
        photo.filename = t.to_string();
    }

    // title (= caption): the sidecar `description` field is the user-authored
    // caption.  Per the photos contract spec and the canonical worked example
    // in docs/vault-spec/domains/photos.md line 69, `title` carries the Google
    // `description` ("Grandma's birthday"), not the filename.
    // Only populate when non-empty to honour "omit empty fields".
    if let Some(desc) = obj.get("description").and_then(Value::as_str) {
        let desc = desc.trim();
        if !desc.is_empty() {
            photo.title = desc.to_string();
        }
    }

    // GPS: prefer geoData (authoritative) over geoDataExif.
    if let Some(geo_val) = obj.get("geoData") {
        if let Some((lat, lon)) = parse_geo(geo_val) {
            photo.lat = Some(lat);
            photo.lon = Some(lon);
            // altitude → extra (no top-level column in the contract).
            if let Some(alt) = geo_val.get("altitude").and_then(Value::as_f64) {
                if alt != 0.0 {
                    photo.extra.insert("altitude".to_string(), json!(alt));
                }
            }
        }
    }
    // Fall back to geoDataExif if geoData had no valid coords.
    if photo.lat.is_none() {
        if let Some(geo_val) = obj.get("geoDataExif") {
            if let Some((lat, lon)) = parse_geo(geo_val) {
                photo.lat = Some(lat);
                photo.lon = Some(lon);
                if let Some(alt) = geo_val.get("altitude").and_then(Value::as_f64) {
                    if alt != 0.0 {
                        photo.extra.insert("altitude".to_string(), json!(alt));
                    }
                }
                photo.extra.insert("geo_source".to_string(), json!("geoDataExif"));
            }
        }
    }

    // People (face tags): array of { name: "..." }.
    if let Some(people_arr) = obj.get("people").and_then(Value::as_array) {
        for p in people_arr {
            if let Some(name) = p.get("name").and_then(Value::as_str) {
                let name = name.trim();
                if !name.is_empty() {
                    photo.people_name.push(name.to_string());
                    // people mirrors people_name for this source (no separate cluster id).
                    photo.people.push(name.to_string());
                }
            }
        }
    }

    // favorited.
    if let Some(fav) = obj.get("favorited").and_then(Value::as_bool) {
        photo.favorite = Some(fav);
    }

    // Source-specific overflow → extra (lossless full fidelity).
    // The mapped top-level columns are excluded; everything else is preserved.
    let mapped = [
        "title",       // → photo.filename
        "description", // → photo.title (caption)
        "photoTakenTime",
        "creationTime",
        "geoData",
        "geoDataExif",
        "people",
        "url",
        "favorited",
    ];
    for (k, v) in obj {
        if !mapped.contains(&k.as_str()) {
            photo.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    Some(photo)
}

/// Extract the `url` field from a sidecar (the stable photo URL, used as guid).
fn url_from_sidecar(value: &Value) -> Option<String> {
    value
        .get("url")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Fallback guid: `title + ":" + best-available-timestamp`.
///
/// Mirrors the `ts` fallback: prefers `photoTakenTime.timestamp`; falls back
/// to `creationTime.timestamp` so that rows whose guid resolves via the
/// creationTime path still get a stable guid rather than being dropped.
fn title_ts_guid(value: &Value) -> Option<String> {
    let title = value.get("title").and_then(Value::as_str).unwrap_or("");
    // Mirror the ts fallback: photoTakenTime first, then creationTime.
    let ts = value
        .get("photoTakenTime")
        .and_then(|t| t.get("timestamp"))
        .and_then(Value::as_str)
        .filter(|s| *s != "0")
        .or_else(|| {
            value
                .get("creationTime")
                .and_then(|t| t.get("timestamp"))
                .and_then(Value::as_str)
                .filter(|s| *s != "0")
        })?;
    Some(format!("{title}:{ts}"))
}

/// The UTC month string `"YYYY-MM"` derived from `photoTakenTime.timestamp`.
fn photo_taken_month(value: &Value) -> Option<String> {
    let ts_str = value
        .get("photoTakenTime")
        .and_then(|t| t.get("timestamp"))
        .and_then(Value::as_str)
        .filter(|s| *s != "0")?;
    let epoch: i64 = ts_str.parse().ok()?;
    let dt = DateTime::from_timestamp(epoch, 0)?;
    Some(dt.format("%Y-%m").to_string())
}

/// Parse a Google timestamp object `{ timestamp: "epoch_secs", formatted: "..." }`
/// into an RFC3339 UTC string. Returns `None` when absent or epoch is zero.
///
/// Note: Google Takeout exposes only the UTC epoch; no per-photo timezone
/// offset is available.  The produced string therefore ends in `+00:00`.
/// Callers that care about the photographer's wall clock may check the
/// `tz_unknown` flag in `extra` (set by `photo_from_sidecar`).
fn parse_google_timestamp(val: Option<&Value>) -> Option<String> {
    let ts_str = val?
        .get("timestamp")
        .and_then(Value::as_str)
        .filter(|s| *s != "0")?;
    let epoch: i64 = ts_str.trim().parse().ok()?;
    if epoch == 0 {
        return None;
    }
    let dt: DateTime<Utc> = DateTime::from_timestamp(epoch, 0)?;
    Some(dt.to_rfc3339())
}

/// Parse a `geoData` / `geoDataExif` object into `(lat, lon)`.
/// Returns `None` when both latitude and longitude are zero (Google sentinel for no GPS).
fn parse_geo(val: &Value) -> Option<(f64, f64)> {
    let lat = val.get("latitude").and_then(Value::as_f64)?;
    let lon = val.get("longitude").and_then(Value::as_f64)?;
    // (0.0, 0.0) is the Google sentinel for "no GPS data present" — skip it.
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some((lat, lon))
}

/// Is this file extension a video?
fn is_video_ext(ext: &str) -> bool {
    matches!(ext, "mp4" | "mov" | "avi" | "webm" | "m4v" | "3gp" | "flv" | "mkv")
}

// ---------------------------------------------------------------------------
// Raw persistence helpers

/// Append raw rows to `photos/google-photos/raw/YYYY-MM.jsonl`.
fn append_raw_month(vault: &Vault, month: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{RAW_DIR}/{month}.jsonl");
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

/// Append raw rows to `photos/google-photos/raw/<name>.jsonl` (flat, non-partitioned).
fn append_raw_flat(vault: &Vault, name: &str, rows: &[Value]) -> Result<()> {
    use std::io::Write;
    let rel = format!("{RAW_DIR}/{name}.jsonl");
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

/// Load already-written guids from a raw month file (for re-import dedupe).
fn load_raw_month_guids(vault: &Vault, month: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(path) = vault.resolve(&format!("{RAW_DIR}/{month}.jsonl")) else {
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

/// Load already-written guids from a raw flat file (albums/other).
fn load_raw_flat_guids(vault: &Vault, name: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(path) = vault.resolve(&format!("{RAW_DIR}/{name}.jsonl")) else {
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

/// Content hash of a JSON value for raw dedupe (when no stable id is available).
fn content_hash(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(value.to_string().as_bytes());
    format!("{:x}", h.finalize())
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
            .join(format!("trove-gphotos-{}-{name}", std::process::id()));
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

    fn make_zip(v: &Vault, name: &str, entries: &[(&str, &str)]) -> std::path::PathBuf {
        let zip_path = v.root().join(name);
        let file = fs::File::create(&zip_path).unwrap();
        let mut w = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        for (entry_name, content) in entries {
            w.start_file(*entry_name, opts).unwrap();
            w.write_all(content.as_bytes()).unwrap();
        }
        w.finish().unwrap();
        zip_path
    }

    /// A minimal valid photo sidecar JSON string.
    fn sidecar_minimal() -> String {
        serde_json::to_string(&json!({
            "title": "IMG_1234.jpg",
            "description": "",
            "imageViews": "5",
            "creationTime": {"timestamp": "1665341290", "formatted": "Oct 9, 2022, 6:28:10 PM UTC"},
            "modificationTime": {"timestamp": "1665341290", "formatted": "Oct 9, 2022, 6:28:10 PM UTC"},
            "photoTakenTime": {"timestamp": "1558454400", "formatted": "May 21, 2019, 9:00:00 PM UTC"},
            "geoData": {"latitude": 0.0, "longitude": 0.0, "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0},
            "geoDataExif": {"latitude": 0.0, "longitude": 0.0, "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0},
            "people": [],
            "url": "https://photos.google.com/photo/AF1Qiptest1",
            "favorited": false,
            "googlePhotosOrigin": {"mobileUpload": {"deviceType": "IOS_PHONE"}}
        })).unwrap()
    }

    /// A rich photo sidecar with GPS, people, and a description.
    fn sidecar_rich() -> String {
        serde_json::to_string(&json!({
            "title": "vacation.jpg",
            "description": "Grandma's birthday",
            "imageViews": "42",
            "creationTime": {"timestamp": "1665341290", "formatted": "Oct 9, 2022, 6:28:10 PM UTC"},
            "modificationTime": {"timestamp": "1665341290", "formatted": "Oct 9, 2022, 6:28:10 PM UTC"},
            "photoTakenTime": {"timestamp": "1558454400", "formatted": "May 21, 2019, 9:00:00 PM UTC"},
            "geoData": {
                "latitude": 40.7128,
                "longitude": -74.0060,
                "altitude": 10.5,
                "latitudeSpan": 0.01,
                "longitudeSpan": 0.01
            },
            "geoDataExif": {
                "latitude": 40.7128,
                "longitude": -74.0060,
                "altitude": 10.5,
                "latitudeSpan": 0.0,
                "longitudeSpan": 0.0
            },
            "people": [{"name": "Alice"}, {"name": "Bob"}],
            "url": "https://photos.google.com/photo/AF1Qiprich1",
            "favorited": true,
            "googlePhotosOrigin": {}
        })).unwrap()
    }

    /// A video sidecar (extension-based kind detection).
    fn sidecar_video() -> String {
        serde_json::to_string(&json!({
            "title": "birthday_video.mp4",
            "description": "",
            "imageViews": "3",
            "creationTime": {"timestamp": "1665341290", "formatted": ""},
            "modificationTime": {"timestamp": "1665341290", "formatted": ""},
            "photoTakenTime": {"timestamp": "1558454400", "formatted": ""},
            "geoData": {"latitude": 0.0, "longitude": 0.0, "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0},
            "geoDataExif": {"latitude": 0.0, "longitude": 0.0, "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0},
            "people": [],
            "url": "https://photos.google.com/photo/AF1Qipvideo1",
            "favorited": false
        })).unwrap()
    }

    /// An album metadata JSON (should NOT produce a Photo row).
    fn album_json() -> String {
        serde_json::to_string(&json!({
            "title": "Summer 2019",
            "description": "Beach vacation",
            "mediaItems": [
                {"imageUrl": "https://photos.google.com/photo/AF1Qiptest1"}
            ]
        })).unwrap()
    }

    // -----------------------------------------------------------------------

    #[test]
    fn minimal_sidecar_yields_photo_row() {
        let v = temp_vault("minimal");
        let zip = make_zip(&v, "export.zip", &[("IMG_1234.jpg.json", &sidecar_minimal())]);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
        let rows = photo_rows(&v);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.source, "google-photos");
        assert_eq!(r.guid, "https://photos.google.com/photo/AF1Qiptest1");
        // epoch 1558454400 → 2019-05-21T21:00:00Z (UTC)
        assert!(r.ts.starts_with("2019-05-21"), "ts from photoTakenTime: {}", r.ts);
        assert_eq!(r.kind, "photo");
        assert_eq!(r.filename, "IMG_1234.jpg");
        // No GPS (sentinel 0,0) → lat/lon absent.
        assert!(r.lat.is_none() && r.lon.is_none(), "sentinel GPS → no lat/lon");
        // favorited = false → Some(false)
        assert_eq!(r.favorite, Some(false));
        // googlePhotosOrigin → extra (not a top-level column)
        assert!(r.extra.contains_key("googlePhotosOrigin"), "origin preserved in extra");
    }

    #[test]
    fn rich_sidecar_yields_gps_people_and_title() {
        let v = temp_vault("rich");
        let zip = make_zip(&v, "export.zip", &[("vacation.jpg.json", &sidecar_rich())]);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
        let r = &photo_rows(&v)[0];
        assert_eq!(r.guid, "https://photos.google.com/photo/AF1Qiprich1");
        // title = sidecar `description` (the user caption), per photos contract spec
        // (docs/vault-spec/domains/photos.md: "caption / title where the source has one").
        assert_eq!(r.title, "Grandma's birthday", "title must be the user caption (description)");
        // filename = sidecar `title` (the media filename).
        assert_eq!(r.filename, "vacation.jpg", "filename must be the media filename (title)");
        // description must NOT appear in extra — it was consumed into photo.title.
        assert!(
            !r.extra.contains_key("description"),
            "description consumed into title, not left in extra"
        );
        // GPS: geoData takes priority.
        let lat = r.lat.expect("lat present");
        let lon = r.lon.expect("lon present");
        assert!((lat - 40.7128).abs() < 1e-4, "lat: {lat}");
        assert!((lon + 74.0060).abs() < 1e-4, "lon: {lon}");
        // altitude → extra (non-zero)
        assert_eq!(r.extra.get("altitude"), Some(&json!(10.5)));
        // People.
        assert_eq!(r.people_name, vec!["Alice", "Bob"]);
        assert_eq!(r.people, vec!["Alice", "Bob"]);
        // favorited = true.
        assert_eq!(r.favorite, Some(true));
        // tz_unknown flag present (UTC-only timestamp).
        assert_eq!(r.extra.get("tz_unknown"), Some(&json!(true)));
    }

    #[test]
    fn video_sidecar_yields_kind_video() {
        let v = temp_vault("video");
        let zip = make_zip(&v, "export.zip", &[("birthday_video.mp4.json", &sidecar_video())]);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
        let r = &photo_rows(&v)[0];
        assert_eq!(r.kind, "video");
        assert_eq!(r.filename, "birthday_video.mp4");
    }

    #[test]
    fn album_json_does_not_produce_photo_row() {
        let v = temp_vault("album");
        let zip = make_zip(&v, "export.zip", &[
            ("Summer 2019.json", &album_json()),
            ("IMG_1234.jpg.json", &sidecar_minimal()),
        ]);
        let out = run(&v, &zip);
        // Only the photo sidecar → 1 photo row; album → 0 photo rows.
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
        let rows = photo_rows(&v);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn media_files_in_zip_are_skipped() {
        let v = temp_vault("media-skip");
        let zip = make_zip(&v, "export.zip", &[
            ("IMG_1234.jpg", "JFIF_BYTES"),
            ("IMG_1234.mp4", "MP4_BYTES"),
            ("IMG_1234.jpg.json", &sidecar_minimal()),
        ]);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
    }

    #[test]
    fn re_import_same_zip_adds_zero_rows() {
        let v = temp_vault("reimport");
        let zip = make_zip(&v, "export.zip", &[("IMG_1234.jpg.json", &sidecar_minimal())]);
        let out1 = run(&v, &zip);
        assert_eq!(out1.counts.get("photos"), Some(&1), "first import: {}", out1.headline);

        // Second import of the same ZIP: guid already seen → 0 new, 1 duplicate.
        let out2 = run(&v, &zip);
        assert_eq!(out2.counts.get("photos"), Some(&0), "second import adds nothing: {}", out2.headline);
        assert_eq!(out2.counts.get("duplicates"), Some(&1));
        assert_eq!(photo_rows(&v).len(), 1, "row count unchanged");
    }

    #[test]
    fn two_zips_same_photo_dedupes() {
        let v = temp_vault("twozips");
        // Same sidecar in two different ZIPs (overlapping Takeout batches).
        let zip1 = make_zip(&v, "export1.zip", &[("IMG_1234.jpg.json", &sidecar_minimal())]);
        let zip2 = make_zip(&v, "export2.zip", &[("IMG_1234.jpg.json", &sidecar_minimal())]);
        let out1 = run(&v, &zip1);
        let out2 = run(&v, &zip2);
        assert_eq!(out1.counts.get("photos"), Some(&1));
        assert_eq!(out2.counts.get("photos"), Some(&0), "second ZIP dedupes: {}", out2.headline);
        assert_eq!(photo_rows(&v).len(), 1);
    }

    #[test]
    fn gps_zero_sentinel_yields_no_lat_lon() {
        let v = temp_vault("zerogps");
        let zip = make_zip(&v, "export.zip", &[("IMG_1234.jpg.json", &sidecar_minimal())]);
        run(&v, &zip);
        let r = &photo_rows(&v)[0];
        assert!(
            r.lat.is_none() && r.lon.is_none(),
            "lat/lon absent for sentinel (0,0): {:?} {:?}",
            r.lat, r.lon
        );
    }

    #[test]
    fn geo_zero_lat_nonzero_lon_is_real_coord() {
        // A photo taken at exactly (0.0, non-zero) — the latitude is zero but
        // longitude is non-zero: (0,0) sentinel test fails → lat/lon is written.
        let v = temp_vault("zeroisland");
        let sidecar = serde_json::to_string(&json!({
            "title": "IMG_zero.jpg",
            "photoTakenTime": {"timestamp": "1558454400", "formatted": ""},
            "geoData": {
                "latitude": 0.0, "longitude": 10.5,
                "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0
            },
            "geoDataExif": {
                "latitude": 0.0, "longitude": 0.0,
                "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0
            },
            "people": [],
            "url": "https://photos.google.com/photo/AF1Qipzero1",
            "favorited": false
        })).unwrap();
        let zip = make_zip(&v, "export.zip", &[("IMG_zero.jpg.json", &sidecar)]);
        run(&v, &zip);
        let r = &photo_rows(&v)[0];
        assert_eq!(r.lat, Some(0.0), "lat=0 with non-zero lon is a real coord");
        assert_eq!(r.lon, Some(10.5));
    }

    #[test]
    fn fallback_guid_when_url_absent() {
        // When `url` is missing, fall back to title:photoTakenTime.timestamp.
        let v = temp_vault("noguid");
        let sidecar = serde_json::to_string(&json!({
            "title": "IMG_fallback.jpg",
            "photoTakenTime": {"timestamp": "1558454400", "formatted": ""},
            "geoData": {
                "latitude": 0.0, "longitude": 0.0,
                "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0
            },
            "geoDataExif": {
                "latitude": 0.0, "longitude": 0.0,
                "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0
            },
            "people": [],
            "favorited": false
        })).unwrap();
        let zip = make_zip(&v, "export.zip", &[("IMG_fallback.jpg.json", &sidecar)]);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
        let r = &photo_rows(&v)[0];
        assert_eq!(r.guid, "IMG_fallback.jpg:1558454400", "fallback guid");
    }

    #[test]
    fn raw_layer_always_written() {
        let v = temp_vault("rawlayer");
        let zip = make_zip(&v, "export.zip", &[
            ("IMG_1234.jpg.json", &sidecar_minimal()),
            ("album_metadata.json", &album_json()),
        ]);
        let out = run(&v, &zip);
        // Contract layer: 1 photo.
        assert_eq!(out.counts.get("photos"), Some(&1));
        // Raw layer: at least photo sidecar + album.
        assert!(
            out.counts.get("raw").copied().unwrap_or(0) >= 2,
            "raw items written: {}",
            out.headline
        );
        // Raw month file exists.
        let raw_month = v.resolve("photos/google-photos/raw/2019-05.jsonl").unwrap();
        assert!(raw_month.exists(), "raw month file written");
        // Raw album file exists.
        let raw_album = v.resolve("photos/google-photos/raw/albums.jsonl").unwrap();
        assert!(raw_album.exists(), "raw albums file written");
    }

    #[test]
    fn geodataexif_fallback_when_geodata_is_zero() {
        // geoData is sentinel (0,0) but geoDataExif has real coords → use ExifData.
        let v = temp_vault("geoexif");
        let sidecar = serde_json::to_string(&json!({
            "title": "IMG_exif.jpg",
            "photoTakenTime": {"timestamp": "1558454400", "formatted": ""},
            "geoData": {
                "latitude": 0.0, "longitude": 0.0,
                "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0
            },
            "geoDataExif": {
                "latitude": 51.5, "longitude": -0.12,
                "altitude": 30.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0
            },
            "people": [],
            "url": "https://photos.google.com/photo/AF1Qipexif1",
            "favorited": false
        })).unwrap();
        let zip = make_zip(&v, "export.zip", &[("IMG_exif.jpg.json", &sidecar)]);
        run(&v, &zip);
        let r = &photo_rows(&v)[0];
        assert!(
            (r.lat.unwrap() - 51.5).abs() < 1e-4,
            "geoDataExif fallback lat: {:?}",
            r.lat
        );
        assert!(
            (r.lon.unwrap() + 0.12).abs() < 1e-4,
            "geoDataExif fallback lon: {:?}",
            r.lon
        );
        assert_eq!(r.extra.get("geo_source"), Some(&json!("geoDataExif")));
    }

    #[test]
    fn def_is_default_off_import_no_connection() {
        assert!(!DEF.meta.default_on, "google-photos is default-off (GPS trail + face tags)");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no API connection — Takeout-only");
        let import = DEF.import_spec().unwrap();
        assert!(import.accepts.contains(&"zip"));
        // Setup copy mentions location.
        assert!(
            DEF.meta.setup.iter().any(|s| s.to_lowercase().contains("location")),
            "setup copy must acknowledge the location trail"
        );
    }

    #[test]
    fn serde_back_compat_old_sparse_line_still_deserializes() {
        // An older minimal line (only required fields) still parses.
        let old = r#"{"ts":"2019-05-21T21:00:00+00:00","source":"google-photos","guid":"https://photos.google.com/photo/AF1Qipold"}"#;
        let p: Photo = serde_json::from_str(old).unwrap();
        assert_eq!(p.source, "google-photos");
        assert_eq!(p.guid, "https://photos.google.com/photo/AF1Qipold");
        assert!(p.lat.is_none() && p.favorite.is_none());
    }

    #[test]
    fn corrupt_json_in_zip_is_skipped_not_fatal() {
        let v = temp_vault("corrupt");
        let zip = make_zip(&v, "export.zip", &[
            ("broken.json", "not valid json {{{"),
            ("IMG_1234.jpg.json", &sidecar_minimal()),
        ]);
        // Must not panic; the good sidecar is indexed.
        let out = run(&v, &zip);
        assert_eq!(
            out.counts.get("photos"),
            Some(&1),
            "good sidecar survives bad neighbor: {}",
            out.headline
        );
    }

    #[test]
    fn description_maps_to_title_not_extra() {
        // The major fix: sidecar `description` (user caption) → photo.title;
        // sidecar `title` (filename) → photo.filename.
        // The old code put the filename in photo.title and stashed description in extra.
        let v = temp_vault("desc-title");
        let zip = make_zip(&v, "export.zip", &[("vacation.jpg.json", &sidecar_rich())]);
        run(&v, &zip);
        let r = &photo_rows(&v)[0];
        assert_eq!(r.title, "Grandma's birthday", "title = caption (description field)");
        assert_eq!(r.filename, "vacation.jpg", "filename = media filename (title field)");
        assert!(
            !r.extra.contains_key("description"),
            "description consumed into title; must NOT appear in extra"
        );
    }

    #[test]
    fn enrichment_less_album_routes_to_albums_jsonl() {
        // Modern Google Takeout album files often have only {title, description,
        // date, access} — no mediaItems or enrichments arrays.  The relaxed
        // is_album_json predicate must route these to raw/albums.jsonl, not other.
        let v = temp_vault("slim-album");
        let slim_album = serde_json::to_string(&json!({
            "title": "Summer 2019",
            "description": "Beach vacation",
            "date": {"timestamp": "1558454400", "formatted": "May 21, 2019"},
            "access": "private"
        })).unwrap();
        let zip = make_zip(&v, "export.zip", &[
            ("Summer 2019.json", &slim_album),
            ("IMG_1234.jpg.json", &sidecar_minimal()),
        ]);
        let out = run(&v, &zip);
        assert_eq!(out.counts.get("photos"), Some(&1), "{}", out.headline);
        // The slim album must go to albums.jsonl, not other.jsonl.
        let raw_album = v.resolve("photos/google-photos/raw/albums.jsonl").unwrap();
        assert!(raw_album.exists(), "albums.jsonl written");
        let contents = std::fs::read_to_string(&raw_album).unwrap();
        assert!(
            contents.contains("Summer 2019"),
            "slim album routed to albums.jsonl, not other.jsonl"
        );
        // other.jsonl must NOT exist (or must not contain the album title).
        let raw_other = v.resolve("photos/google-photos/raw/other.jsonl").unwrap();
        if raw_other.exists() {
            let other_contents = std::fs::read_to_string(&raw_other).unwrap();
            assert!(
                !other_contents.contains("Summer 2019"),
                "album must not appear in other.jsonl"
            );
        }
    }

    #[test]
    fn guid_uses_creation_time_when_photo_taken_time_is_zero() {
        // When url is absent AND photoTakenTime is zero but creationTime is valid:
        // the ts fallback resolves via creationTime, and title_ts_guid must match
        // so the row is written rather than silently dropped.
        let v = temp_vault("no-taken-time");
        let sidecar = serde_json::to_string(&json!({
            "title": "IMG_notaken.jpg",
            "photoTakenTime": {"timestamp": "0", "formatted": ""},
            "creationTime": {"timestamp": "1665341290", "formatted": "Oct 9, 2022"},
            "geoData": {"latitude": 0.0, "longitude": 0.0, "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0},
            "geoDataExif": {"latitude": 0.0, "longitude": 0.0, "altitude": 0.0, "latitudeSpan": 0.0, "longitudeSpan": 0.0},
            "people": [],
            "favorited": false
        })).unwrap();
        let zip = make_zip(&v, "export.zip", &[("IMG_notaken.jpg.json", &sidecar)]);
        let out = run(&v, &zip);
        // Must write 1 photo row, not 0.
        assert_eq!(out.counts.get("photos"), Some(&1), "creationTime fallback produces a row: {}", out.headline);
        let r = &photo_rows(&v)[0];
        assert_eq!(r.guid, "IMG_notaken.jpg:1665341290", "guid uses creationTime fallback");
        assert!(r.ts.starts_with("2022-10-09"), "ts from creationTime: {}", r.ts);
    }

    #[test]
    fn tz_unknown_flag_present_in_extra() {
        // Every Google Photos contract row carries tz_unknown=true because Google
        // Takeout exposes only UTC epoch with no per-photo timezone offset.
        let v = temp_vault("tzflag");
        let zip = make_zip(&v, "export.zip", &[("IMG_1234.jpg.json", &sidecar_minimal())]);
        run(&v, &zip);
        let r = &photo_rows(&v)[0];
        assert_eq!(
            r.extra.get("tz_unknown"),
            Some(&json!(true)),
            "tz_unknown flag marks UTC-only timestamps"
        );
    }
}
