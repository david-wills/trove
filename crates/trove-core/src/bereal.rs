//! BeReal — GDPR export import (ZIP) for dual-camera photo archive.
//!
//! BeReal has no API; the only data path is a GDPR export ZIP requested via
//! in-app support chat (delivered within ~48 hours). The export carries:
//!
//! - **Daily posts** — every "BeReal" taken: a rear (`primary`) + selfie
//!   (`secondary`) photo pair per post. Image files are random-named flat
//!   `.webp` files under a `Photos/` directory (e.g. `Photos/_OaBX9T….webp`);
//!   there are NO date-named folders in real exports. The `posts.json` manifest
//!   carries metadata (timestamps, paths, location, caption). Trove never copies
//!   image bytes — only metadata (timestamp, `primary`/`secondary` filenames,
//!   optional GPS) is indexed. Field names confirmed from
//!   hatobi/bereal-gdpr-photo-toolkit (process-photos.py).
//! - **Account metadata** — username, phone number, registration date,
//!   privacy-settings history. Written to `social/bereal/raw/account.jsonl`.
//! - **Login history** — one entry per login event. Written to
//!   `social/bereal/raw/logins.jsonl`.
//! - **App analytics** (optional `.json.gz`) — aggregate analytics; written
//!   raw and otherwise ignored.
//! - **Everything else** — any unrecognized JSON section is written verbatim
//!   to `social/bereal/raw/<stem>.jsonl` (full fidelity, no drop).
//!
//! ## Contract layer
//!
//! Each dual-camera post → one [`crate::photos::Photo`] row in
//! `photos/bereal/YYYY-MM.jsonl`. `guid` = `bereal:moment:<takenAt>:<primaryFilename>`
//! (stable across GDPR re-exports; mutable fields like reactions are excluded).
//! `ts` is parsed from `takenAt` (ISO 8601 UTC). `extra.back_file` = `primary`
//! filename (rear camera); `extra.front_file` = `secondary` filename (selfie).
//! Re-importing a newer export that added reactions/RealMojis never duplicates
//! (guid anchored to immutable capture identity).
//!
//! ## Parser status
//!
//! **PARSER PARKED — Needs-sample.** The BeReal GDPR export format is not
//! officially documented; the layout above is based on community toolkits and
//! user reports. The parser here is a best-effort scaffold that correctly
//! handles the known structure. A real GDPR ZIP sample is needed to verify
//! exact field names, JSON file names, and confirm the photo-folder layout
//! before relying on the contract rows. Acquire a real export and run the
//! validation matrix in docs/integrations/bereal.md to finalize the parser.
//!
//! ## Raw layer (unconditional)
//!
//! Every JSON file in the ZIP is written verbatim to `photos/bereal/raw/`
//! or `social/bereal/raw/` regardless of whether the contract parser
//! successfully extracts a Photo row — full fidelity is never conditional.
//!
//! ## Media note
//!
//! BeReal's proprietary raw photo format requires external decoding tools to
//! produce standard JPEGs. Trove never copies or decodes image bytes; only the
//! metadata objects from the export's JSON files are stored. The in-ZIP image
//! files are enumerated for their names (→ `extra.front_file` /
//! `extra.back_file`) and then skipped.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime};
use sha2::{Digest, Sha256};
use serde_json::{json, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "bereal";
const PHOTO_DIR: &str = "photos/bereal";
const PHOTO_RAW_DIR: &str = "photos/bereal/raw";
const SOCIAL_RAW_DIR: &str = "social/bereal/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(PHOTO_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bereal",
        name: "BeReal",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your BeReal archive: every dual-camera photo with \
                      timestamps, account metadata, and login history from your \
                      GDPR data export. Image files are never copied — only metadata \
                      (timestamps, filenames, location when present) is stored.",
        domain: "photos",
        vault_path: "photos/bereal/",
        toggleable: false,
        setup: &[
            "Request your BeReal data export via in-app chat support (Settings → \
             Contact Support → request a copy of your personal data). A ZIP file \
             is delivered within ~48 hours.",
            "Drop the ZIP here. Only metadata is stored — the dual-camera images \
             themselves are never copied into the vault.",
        ],
        caveats: "Export requires a support chat request rather than a self-serve \
                  button — delivery typically takes up to 48 hours. BeReal's proprietary \
                  dual-camera image format is not decoded; only metadata (post timestamps, \
                  filenames, location when present) is indexed. The exact export layout \
                  is community-documented only — results may vary across export versions.",
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
    sections: HashSet<String>,
}

// ---------------------------------------------------------------------------
// Main importer

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load already-stored photo guids for re-runnable imports (dedupe by guid).
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
            "reading {} — is this a BeReal GDPR export ZIP?",
            path.display()
        )
    })?;

    // Collect all entry names first (ZipArchive doesn't allow iterating by
    // reference while also calling by_name).
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
    // Raw rows keyed by their section file (account, logins, etc.).
    let mut raw_by_section: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut raw_seen: BTreeMap<String, HashSet<String>> = BTreeMap::new();
    // Track which post folders we've seen so we build one Photo per post even
    // if the ZIP contains multiple entries per folder (front + back).
    let mut post_folders: BTreeMap<String, PostEntry> = BTreeMap::new();
    // Whether we found and processed a posts JSON manifest (even if all rows
    // were duplicates). The folder-fallback only fires when NO manifest is found.
    let mut found_posts_manifest = false;

    // First pass: enumerate image entries to populate per-post folder data.
    for name in &names {
        let normalized = name.replace('\\', "/");
        if let Some(folder) = post_folder_of(&normalized) {
            let ext_lower = normalized
                .rsplit('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if is_image_ext(&ext_lower) {
                let entry = post_folders.entry(folder.to_string()).or_default();
                let filename = normalized.rsplit('/').next().unwrap_or(&normalized).to_string();
                // Heuristic: files with "front"/"selfie" in the name → front camera;
                // files with "back"/"photo" or appearing second → back camera.
                // BeReal community docs consistently name them `front.jpg` / `back.jpg`
                // (or with varying extensions). We check the stem, not the parent path.
                let stem_lower = filename
                    .rsplit('.')
                    .nth(1)
                    .unwrap_or(&filename)
                    .to_ascii_lowercase();
                if stem_lower.contains("front") || stem_lower.contains("selfie") {
                    entry.front_file = Some(filename);
                } else if stem_lower.contains("back")
                    || stem_lower.contains("photo")
                    || entry.front_file.is_some()
                {
                    entry.back_file = Some(filename);
                } else {
                    // First image seen for this folder → tentatively front.
                    entry.front_file = Some(filename);
                }
            }
        }
    }

    // Second pass: parse JSON sections and build Photo rows.
    for name in &names {
        let normalized = name.replace('\\', "/");
        let lower = normalized.to_ascii_lowercase();

        // Skip image/media files — we enumerated them in the first pass.
        if lower
            .rsplit('.')
            .next()
            .map(|e| is_image_ext(e) || is_video_ext(e))
            .unwrap_or(false)
        {
            continue;
        }

        // Skip the app-analytics .json.gz (optional, low-value) — but note it.
        if lower.ends_with(".json.gz") || lower.ends_with(".gz") {
            let section = "analytics";
            let section_seen = raw_seen
                .entry(section.to_string())
                .or_insert_with(|| load_raw_section_guids(vault, SOCIAL_RAW_DIR, section));
            let bucket = raw_by_section.entry(section.to_string()).or_default();
            let stub = json!({"source": "bereal", "section": section, "file": normalized});
            let g = content_hash(&stub);
            if section_seen.insert(g.clone()) {
                bucket.push(json!({"guid": g, "raw": stub}));
                stats.raw += 1;
                stats.sections.insert(section.to_string());
            }
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

        // Route JSON files to raw sections + attempt contract rows for posts.
        let section = section_name(&normalized);

        if is_posts_file(&normalized) {
            found_posts_manifest = true;
            // Posts JSON: try to build Photo rows (best-effort scaffold).
            let seen = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, PHOTO_RAW_DIR, &section));
            let bucket = raw_by_section.entry(section.clone()).or_default();

            for post_val in posts_array(&value) {
                // Raw: always.
                let raw_guid = content_hash(post_val);
                if seen.insert(raw_guid.clone()) {
                    bucket.push(json!({
                        "guid": raw_guid,
                        "section": section,
                        "raw": post_val.clone()
                    }));
                    stats.raw += 1;
                    stats.sections.insert(section.clone());
                }

                // Contract: best-effort Photo row.
                if let Some(photo) = photo_from_post(post_val, &post_folders) {
                    if !seen_photos.insert(photo.guid.clone()) {
                        stats.duplicates += 1;
                    } else {
                        photos.push(photo);
                        stats.photos += 1;
                    }
                }
            }
        } else {
            // Non-post JSON section → raw (account, logins, settings, etc.).
            // Route to SOCIAL_RAW_DIR for non-photo metadata.
            let raw_dir = if is_social_section(&section) {
                SOCIAL_RAW_DIR
            } else {
                PHOTO_RAW_DIR
            };
            let seen = raw_seen
                .entry(section.clone())
                .or_insert_with(|| load_raw_section_guids(vault, raw_dir, &section));
            let bucket = raw_by_section.entry(format!("{raw_dir}/{section}")).or_default();

            for item in section_items(&value) {
                let g = content_hash(&item);
                if !seen.insert(g.clone()) {
                    stats.duplicates += 1;
                    continue;
                }
                bucket.push(json!({"guid": g, "section": section, "raw": item}));
                stats.raw += 1;
                stats.sections.insert(section.clone());
            }
        }

        progress(ImportProgress {
            records: stats.photos + stats.raw,
            percent: 50.0,
        });
    }

    // If no posts JSON manifest was found (the export may use folder-only
    // structure with no manifest), build Photo rows from the post folder
    // entries. When a manifest WAS found (even if all rows were duplicates),
    // skip the folder fallback — we already processed the authoritative source.
    if !found_posts_manifest && !post_folders.is_empty() {
        for (folder_name, entry) in &post_folders {
            if let Some(photo) = photo_from_folder(folder_name, entry) {
                if !seen_photos.insert(photo.guid.clone()) {
                    stats.duplicates += 1;
                } else {
                    photos.push(photo);
                    stats.photos += 1;
                }
            }
        }
    }

    // Write contract rows (photo stream, partitioned by month of ts).
    photo_stream.append(&photos, |p| &p.ts)?;

    // Write raw rows, split into their respective raw directories.
    for (section_key, rows) in &raw_by_section {
        if rows.is_empty() {
            continue;
        }
        // section_key is either "section_name" (→ PHOTO_RAW_DIR) or
        // "social/bereal/raw/section_name" (→ that dir directly).
        if let Some(rest) = section_key.strip_prefix(&format!("{SOCIAL_RAW_DIR}/")) {
            append_raw_section(vault, SOCIAL_RAW_DIR, rest, rows)?;
        } else {
            append_raw_section(vault, PHOTO_RAW_DIR, section_key, rows)?;
        }
    }

    progress(ImportProgress {
        records: stats.photos + stats.raw,
        percent: 100.0,
    });

    Ok(ImportOutcome {
        headline: format!(
            "{} posts indexed, {} raw items across {} sections, {} duplicates skipped",
            stats.photos,
            stats.raw,
            stats.sections.len(),
            stats.duplicates,
        ),
        counts: [
            ("photos", stats.photos),
            ("raw", stats.raw),
            ("sections", stats.sections.len() as u64),
            ("duplicates", stats.duplicates),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Post folder tracking

/// Per-post folder: which image filenames have we seen for this folder?
/// (Built in the first enumeration pass before JSON parsing.)
#[derive(Default)]
struct PostEntry {
    front_file: Option<String>,
    back_file: Option<String>,
}

// ---------------------------------------------------------------------------
// ZIP entry routing

/// Does this ZIP entry name belong to a date-named post folder?
/// BeReal exports use directory names like `2023-04-15-14-32-07` (community
/// documented format, a local timestamp as `YYYY-MM-DD-HH-MM-SS`).
fn post_folder_of(name: &str) -> Option<&str> {
    // The entry is `<folder>/<file>` — the folder is the first path component.
    let folder = name.split('/').next()?;
    // A post folder name matches `DDDD-DD-DD-DD-DD-DD` (6 dash-separated groups).
    // Be lenient: just require ≥14 chars and the right character set.
    if looks_like_post_folder(folder) {
        Some(folder)
    } else {
        // May also be nested: `posts/2023-04-15-14-32-07/front.jpg`
        let mut parts = name.split('/');
        parts.next()?; // skip outer dir
        let folder = parts.next()?;
        if looks_like_post_folder(folder) {
            Some(folder)
        } else {
            None
        }
    }
}

fn looks_like_post_folder(s: &str) -> bool {
    // `YYYY-MM-DD-HH-MM-SS` = 6 groups of 2-4 digits separated by dashes,
    // total length 19. Allow minor variation (some exports may differ).
    s.len() >= 14
        && s.chars().all(|c| c.is_ascii_digit() || c == '-')
        && s.chars().filter(|&c| c == '-').count() >= 4
}

/// Is this entry the posts/memories JSON file?
fn is_posts_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase().replace('\\', "/");
    // Community toolkits reference a top-level `posts.json` or
    // `memories.json`, or a `data/posts.json`. Match generously.
    let stem = lower.rsplit('/').next().unwrap_or(&lower);
    matches!(stem, "posts.json" | "memories.json" | "bereal.json" | "data.json")
}

/// Is this section name for social metadata (not photos)?
fn is_social_section(section: &str) -> bool {
    matches!(section, "account" | "logins" | "login" | "user" | "profile" | "settings" | "privacy")
}

fn is_image_ext(ext: &str) -> bool {
    matches!(
        ext,
        "jpg" | "jpeg" | "heic" | "heif" | "png" | "webp" | "avif" | "gif"
    )
}

fn is_video_ext(ext: &str) -> bool {
    matches!(ext, "mp4" | "mov" | "avi" | "webm" | "m4v")
}

// ---------------------------------------------------------------------------
// Posts → Photo contract

/// Extract all post objects from a JSON value. Handles both a bare array
/// `[ {…}, … ]` and the common single-key wrapper `{"posts": [ {…}, … ]}`.
fn posts_array(value: &Value) -> Vec<&Value> {
    if let Some(arr) = value.as_array() {
        return arr.iter().collect();
    }
    if let Some(obj) = value.as_object() {
        // Look for any key whose value is an array — common envelope shape.
        for v in obj.values() {
            if let Some(arr) = v.as_array() {
                return arr.iter().collect();
            }
        }
        // Fall back: the whole object as a single item (e.g. one post record).
        return vec![value];
    }
    Vec::new()
}

/// Build a [`Photo`] from a post JSON object (contract layer).
///
/// # Parser status — PARKED / Needs-sample
///
/// Field names confirmed from hatobi/bereal-gdpr-photo-toolkit (process-photos.py):
/// - `primary` — rear/back-camera image object (has a `path` sub-key)
/// - `secondary` — front/selfie-camera image object (has a `path` sub-key)
/// - `takenAt` — ISO 8601 UTC capture timestamp
/// - `location` — optional GPS object (`latitude`/`longitude`)
/// - `caption` — optional caption string
/// No per-post `id` field observed in toolkit; guid falls back to takenAt +
/// primary-path basename (stable across GDPR re-exports).
fn photo_from_post(value: &Value, post_folders: &BTreeMap<String, PostEntry>) -> Option<Photo> {
    let obj = value.as_object()?;

    // ts: parse the capture timestamp first — it's used in the stable guid fallback.
    let ts_str = obj
        .get("takenAt")
        .or_else(|| obj.get("taken_at"))
        .or_else(|| obj.get("postedAt"))
        .or_else(|| obj.get("posted_at"))
        .or_else(|| obj.get("date"))
        .or_else(|| obj.get("timestamp"))
        .and_then(|v| parse_bereal_ts(v));

    let ts = ts_str?;

    // Back camera (main lens): BeReal export key is `primary`.
    // Path is a nested `path` string: entry["primary"]["path"].
    // Also try legacy names seen in older community reports.
    let back_path = obj
        .get("primary")
        .and_then(|v| v.get("path").or_else(|| v.get("url")))
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("backCamera")
                .or_else(|| obj.get("back_camera"))
                .or_else(|| obj.get("back"))
                .and_then(|v| v.get("url").or_else(|| v.get("path")).or(Some(v)))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let back_filename = Path::new(back_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(back_path.rsplit('/').next().unwrap_or(back_path))
        .to_string();

    // Front camera (selfie): BeReal export key is `secondary`.
    // Path is a nested `path` string: entry["secondary"]["path"].
    // Also try legacy names.
    let front_path = obj
        .get("secondary")
        .and_then(|v| v.get("path").or_else(|| v.get("url")))
        .and_then(Value::as_str)
        .or_else(|| {
            obj.get("frontCamera")
                .or_else(|| obj.get("front_camera"))
                .or_else(|| obj.get("front"))
                .and_then(|v| v.get("url").or_else(|| v.get("path")).or(Some(v)))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let front_filename = Path::new(front_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(front_path.rsplit('/').next().unwrap_or(front_path))
        .to_string();

    // guid: try explicit id fields first; fall back to a STABLE hash of only
    // the immutable capture identity (takenAt + primary path basename).
    // Hashing the whole post object is UNSTABLE — reactions/RealMojis mutate
    // across GDPR re-exports and would create duplicate contract rows.
    let guid = obj
        .get("id")
        .or_else(|| obj.get("postId"))
        .or_else(|| obj.get("memoryId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| format!("bereal:{s}"))
        .unwrap_or_else(|| {
            // Stable fallback: takenAt + primary-image filename (both immutable).
            let taken = obj
                .get("takenAt")
                .or_else(|| obj.get("taken_at"))
                .or_else(|| obj.get("date"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let primary_name = if back_filename.is_empty() { "" } else { &back_filename };
            if !taken.is_empty() {
                format!("bereal:moment:{taken}:{primary_name}")
            } else {
                // Last resort: hash ONLY the stable structural fields.
                let stable = json!({
                    "takenAt": obj.get("takenAt"),
                    "primaryPath": back_path,
                    "secondaryPath": front_path,
                });
                let mut h = Sha256::new();
                h.update(stable.to_string().as_bytes());
                format!("bereal:sha256:{:x}", h.finalize())
            }
        });

    let mut photo = Photo::new(SOURCE, guid, ts);
    photo.kind = "photo".into();

    // If we didn't get filenames from JSON fields, try the folder-based lookup.
    let (front_f, back_f) = if front_filename.is_empty() && back_filename.is_empty() {
        // The post may use a folder-name id — look up what we found in pass 1.
        let post_id = obj
            .get("id")
            .or_else(|| obj.get("postId"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(entry) = post_folders.get(post_id) {
            (
                entry.front_file.clone().unwrap_or_default(),
                entry.back_file.clone().unwrap_or_default(),
            )
        } else {
            (String::new(), String::new())
        }
    } else {
        (front_filename, back_filename)
    };

    if !front_f.is_empty() {
        photo.extra.insert("front_file".into(), Value::String(front_f));
    }
    if !back_f.is_empty() {
        photo.extra.insert("back_file".into(), Value::String(back_f));
    }

    // Location (if present).
    if let Some(loc) = obj.get("location") {
        photo.lat = loc
            .get("latitude")
            .or_else(|| loc.get("lat"))
            .and_then(|v| v.as_f64());
        photo.lon = loc
            .get("longitude")
            .or_else(|| loc.get("lon"))
            .or_else(|| loc.get("lng"))
            .and_then(|v| v.as_f64());
    }

    // Overflow: everything else → extra.
    // `primary` and `secondary` are the real BeReal export keys (confirmed from
    // hatobi/bereal-gdpr-photo-toolkit); legacy names kept for safety.
    let mapped = [
        "id", "postId", "memoryId", "takenAt", "taken_at", "postedAt", "posted_at",
        "date", "timestamp",
        // Real BeReal GDPR export keys (confirmed):
        "primary", "secondary",
        // Legacy / community-variant names (kept for safety):
        "frontCamera", "front_camera", "front", "backCamera",
        "back_camera", "back", "primaryPhoto",
        "location",
    ];
    for (k, v) in obj {
        if !mapped.contains(&k.as_str()) {
            photo.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    Some(photo)
}

/// Build a [`Photo`] directly from a post folder entry (fallback when no
/// posts.json manifest is present — timestamps come from the folder name).
///
/// Folder format: `YYYY-MM-DD-HH-MM-SS` (community documented).
fn photo_from_folder(folder_name: &str, entry: &PostEntry) -> Option<Photo> {
    let ts = parse_folder_timestamp(folder_name)?;
    // guid: use the folder name itself — it's the stable timestamp-derived id.
    let guid = format!("bereal:folder:{folder_name}");
    let mut photo = Photo::new(SOURCE, guid, ts);
    photo.kind = "photo".into();
    if let Some(f) = &entry.front_file {
        photo.extra.insert("front_file".into(), Value::String(f.clone()));
    }
    if let Some(b) = &entry.back_file {
        photo.extra.insert("back_file".into(), Value::String(b.clone()));
    }
    Some(photo)
}

// ---------------------------------------------------------------------------
// Timestamp parsing

/// Parse a BeReal timestamp value (ISO string, Unix epoch, or folder-style
/// `YYYY-MM-DD-HH-MM-SS`) into an RFC3339 local string.
fn parse_bereal_ts(v: &Value) -> Option<String> {
    // Case 1: ISO/RFC3339 string.
    if let Some(s) = v.as_str() {
        // Try ISO 8601 / RFC3339 parse directly via chrono.
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Some(dt.with_timezone(&Local).to_rfc3339());
        }
        // Try various other string formats common in JSON exports.
        for fmt in &[
            "%Y-%m-%dT%H:%M:%SZ",
            "%Y-%m-%dT%H:%M:%S%.fZ",
            "%Y-%m-%dT%H:%M:%S%:z",
            "%Y-%m-%d %H:%M:%S",
            "%Y-%m-%d",
        ] {
            if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
                // No timezone info → treat as UTC (most JSON exports use UTC).
                let dt = DateTime::from_timestamp(
                    ndt.and_utc().timestamp(),
                    ndt.and_utc().timestamp_subsec_nanos(),
                )?;
                return Some(dt.with_timezone(&Local).to_rfc3339());
            }
        }
        // Folder-style `YYYY-MM-DD-HH-MM-SS`.
        return parse_folder_timestamp(s);
    }
    // Case 2: Unix epoch (integer seconds or float).
    if let Some(secs) = v.as_i64() {
        let dt = DateTime::from_timestamp(secs, 0)?;
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    if let Some(f) = v.as_f64() {
        let dt = DateTime::from_timestamp(f as i64, 0)?;
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    None
}

/// Parse a BeReal post-folder name (`YYYY-MM-DD-HH-MM-SS`) into an RFC3339
/// local string. Treats the timestamp as local wall-clock time (UTC for
/// simplicity, since BeReal's server timezone is not known from the folder name
/// alone — this will be corrected when a real sample is available).
fn parse_folder_timestamp(s: &str) -> Option<String> {
    // Expected: 6 dash-separated numeric groups of the form YYYY-MM-DD-HH-MM-SS.
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() < 6 {
        return None;
    }
    // Take the last 6 numeric groups (some exports may prefix with `post_` etc.)
    let tail = &parts[parts.len().saturating_sub(6)..];
    if tail.len() < 6 {
        return None;
    }
    let (yy, mo, dd, hh, mm, ss) = (
        tail[0].parse::<i32>().ok()?,
        tail[1].parse::<u32>().ok()?,
        tail[2].parse::<u32>().ok()?,
        tail[3].parse::<u32>().ok()?,
        tail[4].parse::<u32>().ok()?,
        tail[5].parse::<u32>().ok()?,
    );
    let ndt = NaiveDateTime::new(
        chrono::NaiveDate::from_ymd_opt(yy, mo, dd)?,
        chrono::NaiveTime::from_hms_opt(hh, mm, ss)?,
    );
    // Treat as UTC (BeReal server time). Will be corrected with a real sample.
    let dt = DateTime::from_timestamp(ndt.and_utc().timestamp(), 0)?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Raw sections

/// Derive a stable section label from a ZIP entry path.
fn section_name(name: &str) -> String {
    let stem = name
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(name)
        .to_string();
    stem.trim_end_matches(".json").to_ascii_lowercase()
}

/// One JSON section value → a list of raw items. Handles:
/// - A bare top-level array.
/// - A single-key object wrapping an array (the DYI-style envelope).
/// - Any other value → the whole object as one raw item.
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

/// Content hash of a raw item (for re-import dedupe).
fn content_hash(item: &Value) -> String {
    let mut h = Sha256::new();
    h.update(item.to_string().as_bytes());
    format!("{:x}", h.finalize())
}

/// Guids already in a raw section file.
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
            .join(format!("trove-bereal-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Collect all Photo rows from the vault.
    fn photo_rows(v: &Vault) -> Vec<Photo> {
        let stream = v.stream(PHOTO_DIR, Partition::Month);
        let mut out = Vec::new();
        for key in stream.partitions().unwrap() {
            out.extend(stream.read::<Photo>(&key).unwrap());
        }
        out
    }

    /// Build a synthetic BeReal ZIP with the REAL export shape confirmed from
    /// hatobi/bereal-gdpr-photo-toolkit (process-photos.py):
    ///   - `primary` = rear/back-camera (has `path` sub-key → Photos/<uuid>.webp)
    ///   - `secondary` = front/selfie-camera (has `path` sub-key)
    ///   - `takenAt` = ISO 8601 UTC timestamp
    ///   - `location`, `caption` optional
    ///   - No per-post `id` field in real exports (toolkit does not read one)
    /// Image files in real exports are random-named flat .webp under Photos/,
    /// NOT date-named folders — we include stub entries to confirm parser skips them.
    fn synthetic_zip_with_posts_json(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-bereal-zip-{}-{name}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Posts manifest — real BeReal GDPR export field names.
        // primary = rear/back camera (main scene), secondary = front/selfie.
        // No `id` field: guid will be derived from takenAt + primary filename.
        z.start_file("posts.json", opts).unwrap();
        z.write_all(
            br#"[{
                "takenAt": "2024-03-15T14:22:07.000Z",
                "primary": {"path": "Photos/_OaBX9TnSgcfapL8.webp", "width": 1500, "height": 2000},
                "secondary": {"path": "Photos/_mKzR7YnTpwfbqN2.webp", "width": 1500, "height": 2000},
                "location": {"latitude": 48.8566, "longitude": 2.3522},
                "caption": "hello",
                "lateInSeconds": 0,
                "realmojis": []
            }]"#,
        )
        .unwrap();

        // Image file stubs — real exports use random-named flat .webp under Photos/.
        z.start_file("Photos/_OaBX9TnSgcfapL8.webp", opts).unwrap();
        z.write_all(b"").unwrap();
        z.start_file("Photos/_mKzR7YnTpwfbqN2.webp", opts).unwrap();
        z.write_all(b"").unwrap();

        // Account metadata.
        z.start_file("account.json", opts).unwrap();
        z.write_all(
            br#"{"username":"alice","phoneNumber":"+33600000000","createdAt":"2022-05-01T10:00:00Z"}"#,
        )
        .unwrap();

        z.finish().unwrap();
        path
    }

    /// ZIP with only folder-based image entries and NO posts.json (folder
    /// fallback path).
    fn synthetic_zip_folder_only(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "trove-bereal-zip-folder-{}-{name}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Three image stubs in a date-named folder — no JSON manifest at all.
        z.start_file("2023-11-20-08-05-00/front.jpg", opts).unwrap();
        z.write_all(b"").unwrap();
        z.start_file("2023-11-20-08-05-00/back.jpg", opts).unwrap();
        z.write_all(b"").unwrap();

        z.finish().unwrap();
        path
    }

    // -----------------------------------------------------------------------

    #[test]
    fn posts_json_yields_photo_row_with_lat_lon_and_extra() {
        // Happy-path: a posts.json with REAL BeReal GDPR export field names
        // (primary/secondary) produces a Photo contract row with ts, guid,
        // lat, lon, back_file (primary=rear), front_file (secondary=selfie).
        // No `id` field in the fixture → guid uses the stable moment fallback
        // (bereal:moment:<takenAt>:<primaryFilename>).
        let v = temp_vault("posts-json");
        let zip = synthetic_zip_with_posts_json("posts-json");
        let out = run(&v, &zip);

        assert_eq!(
            out.counts.get("photos").copied().unwrap_or(0),
            1,
            "one post indexed: {}",
            out.headline
        );

        let rows = photo_rows(&v);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.source, "bereal");
        assert_eq!(r.kind, "photo");
        // No explicit id in real exports → stable moment-based guid.
        assert!(
            r.guid.starts_with("bereal:moment:"),
            "stable moment guid (no id field in real exports): {}",
            r.guid
        );
        assert!(
            r.ts.contains("2024-03-15"),
            "ts carries the correct date: {}",
            r.ts
        );
        assert!((r.lat.unwrap() - 48.8566).abs() < 1e-3, "lat: {:?}", r.lat);
        assert!((r.lon.unwrap() - 2.3522).abs() < 1e-3, "lon: {:?}", r.lon);
        // back_file = primary (rear camera), front_file = secondary (selfie).
        assert!(
            r.extra.contains_key("front_file"),
            "front_file (secondary/selfie) in extra: {:?}",
            r.extra.keys().collect::<Vec<_>>()
        );
        assert!(
            r.extra.contains_key("back_file"),
            "back_file (primary/rear) in extra: {:?}",
            r.extra.keys().collect::<Vec<_>>()
        );
        // Confirm correct files: primary → back_file, secondary → front_file.
        let back = r.extra.get("back_file").and_then(Value::as_str).unwrap_or("");
        let front = r.extra.get("front_file").and_then(Value::as_str).unwrap_or("");
        assert!(back.contains("_OaBX9TnSgcfapL8"), "back_file = primary filename: {back}");
        assert!(front.contains("_mKzR7YnTpwfbqN2"), "front_file = secondary filename: {front}");
        // overflow keys (caption, lateInSeconds, realmojis) in extra.
        assert!(r.extra.contains_key("caption"), "overflow keys in extra");
    }

    #[test]
    fn account_json_lands_in_social_raw() {
        // Non-photo JSON (account.json) → social/bereal/raw/account.jsonl.
        let v = temp_vault("social-raw");
        let zip = synthetic_zip_with_posts_json("social-raw");
        run(&v, &zip);

        let raw_path = v.root().join("social/bereal/raw/account.jsonl");
        assert!(raw_path.exists(), "account.jsonl written to social/bereal/raw/");
        let body = fs::read_to_string(&raw_path).unwrap();
        assert!(!body.trim().is_empty(), "raw file not empty");
        let v: Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert!(
            v.get("guid").is_some() && v.get("raw").is_some(),
            "raw row has guid + raw"
        );
    }

    #[test]
    fn reimport_same_zip_dedupes_cleanly() {
        // Re-importing the same ZIP twice yields zero new rows and zero duplicates
        // in the photo count (guid-based dedupe via seen_photos). Raw items may
        // increment duplicates but photo count stays at 1.
        let v = temp_vault("dedupe");
        let zip = synthetic_zip_with_posts_json("dedupe");
        let out1 = run(&v, &zip);
        let out2 = run(&v, &zip);

        assert_eq!(out1.counts.get("photos").copied().unwrap_or(0), 1);
        assert_eq!(
            out2.counts.get("photos").copied().unwrap_or(0),
            0,
            "second import adds zero photo rows"
        );
        assert_eq!(photo_rows(&v).len(), 1, "row count stays at 1");
    }

    #[test]
    fn guid_is_stable_across_mutable_field_changes() {
        // Verifies fix for "unstable guid" defect: re-exporting from BeReal may
        // add new reactions/RealMojis to a post. The guid must remain the same
        // (based on takenAt + primary path) so the re-import dedupes rather
        // than creating a second row for the same moment.
        let original = json!({
            "takenAt": "2024-05-20T10:00:00.000Z",
            "primary": {"path": "Photos/abc123.webp"},
            "secondary": {"path": "Photos/def456.webp"},
            "realmojis": []
        });
        let with_new_reactions = json!({
            "takenAt": "2024-05-20T10:00:00.000Z",
            "primary": {"path": "Photos/abc123.webp"},
            "secondary": {"path": "Photos/def456.webp"},
            "realmojis": [{"user": "friend1", "emoji": "😀"}]
        });

        let folders: BTreeMap<String, PostEntry> = BTreeMap::new();
        let p1 = photo_from_post(&original, &folders).unwrap();
        let p2 = photo_from_post(&with_new_reactions, &folders).unwrap();

        assert_eq!(
            p1.guid, p2.guid,
            "guid must be stable across mutable field changes (reactions): '{}' vs '{}'",
            p1.guid, p2.guid
        );
        assert!(
            p1.guid.starts_with("bereal:moment:"),
            "stable guid uses moment prefix: {}",
            p1.guid
        );
    }

    #[test]
    fn folder_only_zip_yields_photo_row_from_folder_name() {
        // NOTE: This tests the folder-fallback path which exercises a THEORETICAL
        // date-folder layout (YYYY-MM-DD-HH-MM-SS/<file>). Real BeReal GDPR exports
        // use flat random-named .webp files under Photos/ with NO date folders —
        // the posts.json manifest is the authoritative source for real data.
        // This test remains to validate: (a) the manifest-absent graceful fallback,
        // and (b) the parse_folder_timestamp utility used by parse_bereal_ts.
        // The green result does NOT confirm real-data compatibility.
        let v = temp_vault("folder-fallback");
        let zip = synthetic_zip_folder_only("folder-fallback");
        let out = run(&v, &zip);

        assert_eq!(
            out.counts.get("photos").copied().unwrap_or(0),
            1,
            "folder-based Photo row: {}",
            out.headline
        );
        let rows = photo_rows(&v);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert!(
            r.guid.contains("2023-11-20-08-05-00"),
            "folder name in guid: {}",
            r.guid
        );
        assert!(r.ts.contains("2023-11-20"), "ts from folder: {}", r.ts);
        // front/back filenames enumerated from the folder entries.
        assert!(
            r.extra.contains_key("front_file") || r.extra.contains_key("back_file"),
            "at least one camera file in extra: {:?}",
            r.extra.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn parse_folder_timestamp_roundtrips() {
        // Known folder format: `2023-04-15-14-32-07`.
        let ts = parse_folder_timestamp("2023-04-15-14-32-07").unwrap();
        assert!(ts.contains("2023-04-15"), "date preserved: {ts}");
        assert!(ts.contains('T'), "RFC3339 shape: {ts}");
        // Invalid → None.
        assert!(parse_folder_timestamp("not-a-date").is_none());
        assert!(parse_folder_timestamp("2023").is_none());
    }

    #[test]
    fn serde_back_compat_photo_rows_still_deserialize() {
        // Old sparse Photo rows (only ts/source/guid) must still round-trip —
        // the Photo contract is additive and BeReal rows carry only what we know.
        let old = r#"{"ts":"2024-01-10T12:00:00+00:00","source":"bereal","guid":"bereal:xyz789"}"#;
        let p: Photo = serde_json::from_str(old).unwrap();
        assert_eq!(p.guid, "bereal:xyz789");
        assert!(p.lat.is_none() && p.extra.is_empty());
    }

    #[test]
    fn def_is_import_default_off_no_connection() {
        assert!(!DEF.meta.default_on, "bereal is default-off (import box)");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no auth — pure ZIP import");
        let spec = DEF.import_spec().unwrap();
        assert_eq!(spec.accepts, &["zip"]);
    }

    #[test]
    fn non_json_entries_and_non_zip_json_not_fatal() {
        // A ZIP that contains only image stubs and a non-JSON file should not
        // error; it produces zero photo rows (no manifest, no folder-named dirs).
        let path = std::env::temp_dir().join(format!(
            "trove-bereal-nojson-{}.zip",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("README.txt", opts).unwrap();
        z.write_all(b"hello").unwrap();
        z.start_file("corrupted.json", opts).unwrap();
        z.write_all(b"not valid json !!$#@").unwrap();
        z.finish().unwrap();
        let v = temp_vault("nojson");
        // Must not panic; zero rows expected.
        let out = run(&v, &path);
        assert_eq!(out.counts.get("photos").copied().unwrap_or(0), 0);
        assert!(out.counts.get("duplicates").copied().unwrap_or(0) == 0);
    }
}
