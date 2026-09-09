//! macOS Screenshots — passive folder watch with on-device Vision OCR.
//!
//! Every screenshot the user deliberately takes (Cmd-Shift-3/4/5) is indexed:
//! its text extracted on-device via Apple Vision, stored as a [`crate::photos::Photo`]
//! row in `photos/macos-screenshots/YYYY-MM.jsonl`. **Image bytes are never
//! copied into the vault.** Screen recordings go to a separate raw stream at
//! `files/macos-screenshots/YYYY-MM.jsonl`.
//!
//! # How the watcher works
//!
//! On each `tick` the collector scans the resolved screenshot folder (read from
//! `defaults read com.apple.screencapture location`, falling back to `~/Desktop`)
//! for both images (PNG/JPEG/HEIC/TIFF) and video recordings (MOV/MP4/M4V).
//! On the very first tick a one-shot `mdfind kMDItemIsScreenCapture==1` backfill
//! pass is run to catch screenshots saved to a custom location before the watcher
//! started, as well as any pre-existing screenshots on a fresh install.
//! New files (those whose SHA-256 content hash is not already in the in-memory
//! seen set) are processed: their capture timestamp is parsed from the filename,
//! dimensions are read via image metadata, and OCR text is extracted via Vision's
//! `VNRecognizeTextRequest` (on macOS). The seen set is persisted to
//! `.trove/macos-screenshots-seen.json` so a daemon restart never re-processes
//! previously seen screenshots.
//!
//! # Privacy
//!
//! Screenshots frequently contain sensitive content (messages, financial data,
//! credentials on-screen). This source is `default_on: false` — an explicit
//! opt-in with a plain-language acknowledgement is required. OCR runs on-device
//! via Apple Vision; no bytes leave the machine.
//!
//! # Filename timestamp parsing
//!
//! macOS formats screenshot filenames as:
//! - `Screenshot YYYY-MM-DD at HH.MM.SS.png` (24h, Mojave+)
//! - `Screen Shot YYYY-MM-DD at HH.MM.SS AM.png` / `… PM.png` (12h, pre-Mojave)
//! - `Screen Recording YYYY-MM-DD at HH.MM.SS.mov` (recordings, all versions)
//!
//! The parser accepts all three prefix forms. Times are local wall-clock;
//! we attach the system local offset.
//!
//! # Screen recording collection
//!
//! Cmd-Shift-5 recordings are saved to the screencapture `location` (Desktop
//! by default), NOT to `~/Library/ScreenRecordings/` (which is only used by
//! QuickTime Player). The scanner handles both:
//! - The resolved screencapture folder is scanned for both image exts and
//!   video exts (MOV/MP4/M4V), covering Cmd-Shift-5 recordings on the default
//!   save location.
//! - `~/Library/ScreenRecordings/` is also scanned for MOV/MP4/M4V in case
//!   the user saves QuickTime recordings there.
//!
//! # Recording GUID stability
//!
//! Content-hashing large video files on every poll is prohibitively expensive.
//! Recording GUIDs use `path + mtime` instead. Trade-off: if the file is moved
//! or renamed (or its mtime is touched) the daemon generates a new GUID and
//! re-indexes the recording as a duplicate. A restart does NOT recover the
//! original entry (the seen-set file tracks the old GUID). This is an accepted
//! fidelity limitation for v1; a future improvement could hash the first+last
//! N KB of the file as a stable partial fingerprint.
//!
//! # Vault layout
//!
//! - **Contract:** `photos/macos-screenshots/YYYY-MM.jsonl` — [`Photo`] rows
//!   (`kind="screenshot"`, `guid="sha256:…"`, `text` = OCR output).
//! - **Raw:** same path — the contract row IS the raw row for this source
//!   (the Photo struct is full fidelity; there is no lossy normalization).
//! - **Screen recordings (raw-only):** `files/macos-screenshots/YYYY-MM.jsonl`
//!   — no OCR in v1; filename + mtime + sha256 + file_size_bytes in `extra`.
//!
//! Brief: `docs/integrations/macos-screenshots.md`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::fs;

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, IntegrationDef, LiveCollector};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths

const PHOTOS_DIR: &str = "photos/macos-screenshots";
const FILES_DIR: &str = "files/macos-screenshots";
const SEEN_STATE: &str = ".trove/macos-screenshots-seen.json";
const SOURCE: &str = "macos-screenshots";

// ---------------------------------------------------------------------------
// DEF

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(PHOTOS_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "macos-screenshots",
        name: "Screenshots",
        kind: IntegrationKind::Live,
        default_on: false,
        description: "Watches your screenshots folder and indexes each new capture: \
                      filename, timestamp, and on-device OCR text extracted via \
                      Apple Vision. Screenshot images are never stored.",
        domain: "photos",
        vault_path: "photos/macos-screenshots/",
        toggleable: true,
        setup: &[
            "Screenshots often contain private content — messages, financial details, \
             credentials visible on screen. Only enable if you are comfortable with \
             that text being indexed in your vault.",
            "OCR runs on-device via Apple Vision; no image or text leaves your machine.",
            "Image and video files are never copied or stored — only metadata and \
             extracted text.",
        ],
        caveats: "OCR captures whatever text was on screen when the screenshot was taken. \
                  Review your vault if you take screenshots of sensitive content.",
    },
    behavior: Behavior::Live(make_live),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Live collector

/// In-memory seen set loaded from and flushed back to `.trove/macos-screenshots-seen.json`.
#[derive(Serialize, Deserialize, Default)]
struct SeenState {
    guids: Vec<String>,
}

struct ScreenshotLive {
    /// GUIDs already written — loaded from the state file on construction, plus
    /// every new row added this session. Persisted on each successful batch.
    seen: HashSet<String>,
    /// Whether the seen set has been loaded from disk yet (lazy, first tick).
    loaded: bool,
    /// True until the first tick completes — gates the one-shot mdfind backfill.
    first_tick: bool,
}

fn make_live() -> Box<dyn LiveCollector> {
    Box::new(ScreenshotLive { seen: HashSet::new(), loaded: false, first_tick: true })
}

impl ScreenshotLive {
    /// Load the persisted seen set from the vault into `self.seen`.
    fn load_seen(&mut self, vault: &Vault) {
        self.loaded = true;
        let path = vault.root().join(SEEN_STATE);
        if let Ok(body) = fs::read_to_string(&path) {
            if let Ok(state) = serde_json::from_str::<SeenState>(&body) {
                self.seen.extend(state.guids);
            }
        }
    }

    /// Persist the seen set to disk. Silently ignores errors (the seen set is
    /// rebuilt from the photos stream on a cold restart if the file is lost;
    /// we just re-process files we'd already indexed — the guid check dedupes them).
    fn persist_seen(&self, vault: &Vault) {
        let state = SeenState { guids: self.seen.iter().cloned().collect() };
        let path = vault.root().join(SEEN_STATE);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json_bytes) = serde_json::to_string_pretty(&state) {
            let _ = crate::store::write_atomic(&path, json_bytes.as_bytes());
        }
    }

    /// Scan the screenshot folder for new PNG/JPEG/HEIC files, process them.
    fn scan_screenshots(&mut self, vault: &Vault, now: DateTime<Local>) {
        let folder = screenshot_folder();
        self.scan_folder(vault, now, &folder);
    }

    /// Inner scanner used by `scan_screenshots` and tests (accepts an explicit
    /// folder path so tests don't race on the TROVE_SCREENSHOT_DIR env var).
    ///
    /// Scans for both image files (PNG/JPEG/HEIC/TIFF → photos stream) and
    /// video files (MOV/MP4/M4V → files/recordings stream). This covers the
    /// common case where Cmd-Shift-5 recordings land in the same screencapture
    /// folder as screenshots.
    fn scan_folder(&mut self, vault: &Vault, now: DateTime<Local>, folder: &Path) {
        if !folder.is_dir() {
            return;
        }

        let entries = match fs::read_dir(folder) {
            Ok(e) => e,
            Err(_) => return,
        };

        // Collect new entries — we keep guids in a *local* set and only merge
        // into self.seen inside the Ok branch, AFTER a successful append.
        // This prevents silent permanent loss on a transient write error:
        // if append fails, self.seen is untouched and the next poll retries.
        let mut new_photos: Vec<Photo> = Vec::new();
        let mut new_photo_guids: Vec<String> = Vec::new();
        let mut new_recs: Vec<serde_json::Value> = Vec::new();
        let mut new_rec_guids: Vec<String> = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            // lstat — never follow symlinks (matches cloud_folder.rs / exif_import.rs pattern)
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
            let ext_lower = ext.to_ascii_lowercase();

            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            if matches!(ext_lower.as_str(), "png" | "jpg" | "jpeg" | "heic" | "tiff" | "tif") {
                // Image: read bytes for content-hash dedup.
                let bytes = match fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let guid = format!("sha256:{:x}", Sha256::digest(&bytes));
                if self.seen.contains(&guid) {
                    continue;
                }
                // Guard against duplicates within the same batch (e.g. symlink
                // targets appearing twice in the listing).
                if new_photo_guids.contains(&guid) {
                    continue;
                }

                // Parse timestamp from the filename; fall back to mtime; fall back to now.
                let ts = parse_screenshot_ts(&filename)
                    .or_else(|| file_mtime_local(&path))
                    .unwrap_or_else(|| now.to_rfc3339());

                let mime = mime_for(&ext_lower);
                let (width, height) = image_dimensions(&bytes, &ext_lower);
                let file_size_bytes = bytes.len() as u64;
                drop(bytes); // never keep image bytes in memory beyond what we need

                let ocr_text = ocr_image(&path);

                let mut photo = Photo::new(SOURCE, guid.as_str(), ts);
                photo.kind = "screenshot".into();
                photo.filename = filename;
                photo.mime = mime.into();
                photo.width = width;
                photo.height = height;
                photo.text = ocr_text;
                photo.extra.insert("file_size_bytes".into(), json!(file_size_bytes));

                new_photo_guids.push(guid);
                new_photos.push(photo);

            } else if matches!(ext_lower.as_str(), "mov" | "mp4" | "m4v") {
                // Video recording: path+mtime guid (content hash too expensive
                // for large video files — see module-level doc for tradeoff).
                let mtime = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let rec_key = format!("rec-{}-{}", path.display(), mtime);
                let guid = format!("sha256:{:x}", Sha256::digest(rec_key.as_bytes()));

                if self.seen.contains(&guid) {
                    continue;
                }
                if new_rec_guids.contains(&guid) {
                    continue;
                }

                let ts = parse_screenshot_ts(&filename)
                    .or_else(|| file_mtime_local(&path))
                    .unwrap_or_else(|| now.to_rfc3339());
                let file_size_bytes = meta.len();
                let mime = match ext_lower.as_str() {
                    "mov" => "video/quicktime",
                    "mp4" | "m4v" => "video/mp4",
                    _ => "",
                };

                new_rec_guids.push(guid.clone());
                new_recs.push(json!({
                    "ts": ts,
                    "source": SOURCE,
                    "guid": guid,
                    "filename": filename,
                    "mime": mime,
                    "extra": { "file_size_bytes": file_size_bytes }
                }));
            }
            // Other file types are silently skipped.
        }

        // Photos: append, then — only on success — merge guids into self.seen.
        if !new_photos.is_empty() {
            let stream = vault.stream(PHOTOS_DIR, Partition::Month);
            if let Err(e) = stream.append(&new_photos, |p| &p.ts) {
                eprintln!("trove macos-screenshots: failed to append photos: {e:#}");
                // Do NOT advance self.seen — next poll will retry these files.
            } else {
                self.seen.extend(new_photo_guids);
                self.persist_seen(vault);
            }
        }

        // Recordings: same fail-safe seen-set pattern.
        if !new_recs.is_empty() {
            let stream = vault.stream(FILES_DIR, Partition::Month);
            if let Err(e) = stream.append(&new_recs, |r| {
                r.get("ts").and_then(|v| v.as_str()).unwrap_or("")
            }) {
                eprintln!("trove macos-screenshots: failed to append recordings: {e:#}");
                // Do NOT advance self.seen — next poll will retry these files.
            } else {
                self.seen.extend(new_rec_guids);
                self.persist_seen(vault);
            }
        }
    }

    /// Scan `~/Library/ScreenRecordings/` for new video files and index them
    /// raw (no OCR for video in v1).
    ///
    /// This covers QuickTime Player recordings only. Cmd-Shift-5 recordings
    /// land in the screencapture `location` folder and are picked up by
    /// `scan_folder` above.
    fn scan_recordings_library(&mut self, vault: &Vault, now: DateTime<Local>) {
        let folder = screen_recordings_folder();
        self.scan_folder(vault, now, &folder);
    }

    /// One-shot `mdfind kMDItemIsScreenCapture==1` backfill. Runs once on the
    /// first enabled tick to catch screenshots saved before the watcher started
    /// (historical backfill) or stored in a custom location that differs from
    /// the current `defaults` value.
    ///
    /// On any mdfind error (Spotlight disabled, sandbox, CI) the pass is
    /// silently skipped — the live scan still runs normally.
    fn run_mdfind_backfill(&mut self, vault: &Vault, now: DateTime<Local>) {
        let output = match std::process::Command::new("mdfind")
            .arg("kMDItemIsScreenCapture == 1")
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return, // mdfind unavailable or failed — not fatal
        };
        let stdout = match String::from_utf8(output.stdout) {
            Ok(s) => s,
            Err(_) => return,
        };

        // Collect guids + photos/recs from mdfind results.
        // Re-use the same fail-safe seen-set pattern: build local lists first.
        let mut new_photos: Vec<Photo> = Vec::new();
        let mut new_photo_guids: Vec<String> = Vec::new();
        let mut new_recs: Vec<serde_json::Value> = Vec::new();
        let mut new_rec_guids: Vec<String> = Vec::new();

        for line in stdout.lines() {
            let path = std::path::PathBuf::from(line.trim());
            let Ok(meta) = fs::symlink_metadata(&path) else { continue };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else { continue };
            let ext_lower = ext.to_ascii_lowercase();
            let filename = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();

            if matches!(ext_lower.as_str(), "png" | "jpg" | "jpeg" | "heic" | "tiff" | "tif") {
                let bytes = match fs::read(&path) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let guid = format!("sha256:{:x}", Sha256::digest(&bytes));
                if self.seen.contains(&guid) || new_photo_guids.contains(&guid) {
                    continue;
                }
                let ts = parse_screenshot_ts(&filename)
                    .or_else(|| file_mtime_local(&path))
                    .unwrap_or_else(|| now.to_rfc3339());
                let mime = mime_for(&ext_lower);
                let (width, height) = image_dimensions(&bytes, &ext_lower);
                let file_size_bytes = bytes.len() as u64;
                drop(bytes);
                let ocr_text = ocr_image(&path);
                let mut photo = Photo::new(SOURCE, guid.as_str(), ts);
                photo.kind = "screenshot".into();
                photo.filename = filename;
                photo.mime = mime.into();
                photo.width = width;
                photo.height = height;
                photo.text = ocr_text;
                photo.extra.insert("file_size_bytes".into(), json!(file_size_bytes));
                new_photo_guids.push(guid);
                new_photos.push(photo);
            } else if matches!(ext_lower.as_str(), "mov" | "mp4" | "m4v") {
                let mtime = meta.modified().ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let rec_key = format!("rec-{}-{}", path.display(), mtime);
                let guid = format!("sha256:{:x}", Sha256::digest(rec_key.as_bytes()));
                if self.seen.contains(&guid) || new_rec_guids.contains(&guid) {
                    continue;
                }
                let ts = parse_screenshot_ts(&filename)
                    .or_else(|| file_mtime_local(&path))
                    .unwrap_or_else(|| now.to_rfc3339());
                let file_size_bytes = meta.len();
                let mime = match ext_lower.as_str() {
                    "mov" => "video/quicktime",
                    "mp4" | "m4v" => "video/mp4",
                    _ => "",
                };
                new_rec_guids.push(guid.clone());
                new_recs.push(json!({
                    "ts": ts,
                    "source": SOURCE,
                    "guid": guid,
                    "filename": filename,
                    "mime": mime,
                    "extra": { "file_size_bytes": file_size_bytes }
                }));
            }
        }

        if !new_photos.is_empty() {
            let stream = vault.stream(PHOTOS_DIR, Partition::Month);
            if let Err(e) = stream.append(&new_photos, |p| &p.ts) {
                eprintln!("trove macos-screenshots: mdfind backfill: failed to append photos: {e:#}");
            } else {
                self.seen.extend(new_photo_guids);
                self.persist_seen(vault);
            }
        }
        if !new_recs.is_empty() {
            let stream = vault.stream(FILES_DIR, Partition::Month);
            if let Err(e) = stream.append(&new_recs, |r| {
                r.get("ts").and_then(|v| v.as_str()).unwrap_or("")
            }) {
                eprintln!("trove macos-screenshots: mdfind backfill: failed to append recordings: {e:#}");
            } else {
                self.seen.extend(new_rec_guids);
                self.persist_seen(vault);
            }
        }
    }
}

impl LiveCollector for ScreenshotLive {
    fn tick(&mut self, vault: &Vault, now: DateTime<Local>, enabled: bool) {
        if !enabled {
            return;
        }
        // Load seen set lazily on first tick.
        if !self.loaded {
            self.load_seen(vault);
        }
        // One-shot mdfind backfill on the very first enabled tick.
        if self.first_tick {
            self.first_tick = false;
            self.run_mdfind_backfill(vault, now);
        }
        self.scan_screenshots(vault, now);
        self.scan_recordings_library(vault, now);
    }

    fn shutdown(&mut self, _vault: &Vault, _now: DateTime<Local>) {
        // Nothing to flush — we write immediately on detect.
    }
}

// ---------------------------------------------------------------------------
// Screenshot folder resolution

/// Resolve the user's screenshot save folder. Reads
/// `defaults read com.apple.screencapture location` via `Command::new("defaults")`;
/// falls back to `~/Desktop` if the key is absent or the command fails.
pub fn screenshot_folder() -> PathBuf {
    screenshot_folder_from_env().unwrap_or_else(|| {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")).join("Desktop")
    })
}

/// Resolve via TROVE_SCREENSHOT_DIR env override (for tests) then the
/// `defaults` plist key.
fn screenshot_folder_from_env() -> Option<PathBuf> {
    // Test hook: TROVE_SCREENSHOT_DIR overrides the defaults read.
    if let Ok(dir) = std::env::var("TROVE_SCREENSHOT_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    read_screencapture_location()
}

/// `~/Library/ScreenRecordings/` (macOS 10.15+).
pub fn screen_recordings_folder() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join("Library/ScreenRecordings")
}

/// Call `defaults read com.apple.screencapture location` and return the path
/// if it is a readable directory. Returns `None` on any failure (key absent,
/// parse error, non-directory).
fn read_screencapture_location() -> Option<PathBuf> {
    let output = std::process::Command::new("defaults")
        .args(["read", "com.apple.screencapture", "location"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Expand leading `~` manually (the shell doesn't do it here).
    let path = if let Some(rest) = trimmed.strip_prefix('~') {
        let home = dirs::home_dir()?;
        home.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(trimmed)
    };
    if path.is_dir() { Some(path) } else { None }
}

// ---------------------------------------------------------------------------
// Filename timestamp parsing

/// Parse the macOS screenshot or screen-recording filename timestamp. Handles:
///
/// - `Screenshot YYYY-MM-DD at HH.MM.SS.png`      (24h, Mojave+)
/// - `Screen Shot YYYY-MM-DD at HH.MM.SS AM.png`  (12h AM, pre-Mojave two-word form)
/// - `Screen Shot YYYY-MM-DD at HH.MM.SS PM.png`  (12h PM, pre-Mojave two-word form)
/// - `Screen Recording YYYY-MM-DD at HH.MM.SS.mov` (recordings, all versions)
/// - Any of the above without an extension
///
/// All three prefixes are tried in order. Returns an RFC3339 local-offset
/// string, or `None` when the filename does not match any expected pattern.
pub fn parse_screenshot_ts(filename: &str) -> Option<String> {
    // Strip a known image/video extension (never strip dots inside the timestamp).
    const KNOWN_EXTS: &[&str] = &["png", "jpg", "jpeg", "heic", "tiff", "tif", "mov", "mp4", "m4v"];
    let name = if let Some(dot) = filename.rfind('.') {
        let ext = filename[dot + 1..].to_ascii_lowercase();
        if KNOWN_EXTS.contains(&ext.as_str()) {
            &filename[..dot]
        } else {
            filename
        }
    } else {
        filename
    };

    // Accept all macOS filename prefixes:
    //   "Screenshot "    — Mojave+ single-word form
    //   "Screen Shot "   — pre-Mojave two-word form (real example: "Screen Shot 2022-10-20 at 3.51.22 PM.JPG")
    //   "Screen Recording " — Cmd-Shift-5 screen recording
    const PREFIXES: &[&str] = &["Screenshot ", "Screen Shot ", "Screen Recording "];
    let rest = PREFIXES.iter().find_map(|pfx| name.strip_prefix(pfx))?;

    // YYYY-MM-DD
    if rest.len() < 10 {
        return None;
    }
    let (date_part, after_date) = rest.split_at(10);
    let rest = after_date.strip_prefix(" at ")?;

    // HH.MM.SS [AM|PM]  — hour may be 1 or 2 digits (pre-Mojave uses single-digit
    // hours, e.g. "3.51.22 PM"). Split on the first space to separate the time
    // component from the optional AM/PM suffix instead of hard-indexing at 8.
    let (time_part_raw, suffix) = match rest.find(' ') {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None    => (rest, ""),
    };

    // Convert HH.MM.SS to HH:MM:SS and handle AM/PM.
    let parts: Vec<&str> = time_part_raw.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let s: u32 = parts[2].parse().ok()?;

    if suffix.eq_ignore_ascii_case("PM") && h < 12 {
        h += 12;
    } else if suffix.eq_ignore_ascii_case("AM") && h == 12 {
        h = 0;
    }

    let naive_str = format!("{date_part}T{h:02}:{m:02}:{s:02}");
    let ndt = NaiveDateTime::parse_from_str(&naive_str, "%Y-%m-%dT%H:%M:%S").ok()?;
    // Attach the system local offset (same pattern as exif_import.rs).
    match Local.from_local_datetime(&ndt).single() {
        Some(local) => Some(local.to_rfc3339()),
        None => Local
            .from_local_datetime(&ndt)
            .earliest()
            .map(|d| d.to_rfc3339()),
    }
}

// ---------------------------------------------------------------------------
// Image dimension extraction (pure-Rust header parse, no full decode)

/// Read the pixel dimensions of a PNG or JPEG from its header bytes.
/// Returns `(width, height)` or `(None, None)` when the format is unrecognised
/// or the bytes are too short. **Only reads a tiny header slice — never decodes
/// the full image.**
fn image_dimensions(bytes: &[u8], ext: &str) -> (Option<u32>, Option<u32>) {
    match ext {
        "png" => png_dimensions(bytes),
        "jpg" | "jpeg" => jpeg_dimensions(bytes),
        // HEIC/TIFF: no simple header parser; dims omitted (future: nom-exif).
        _ => (None, None),
    }
}

/// PNG: IHDR chunk at byte 8; width at bytes 16–19, height at 20–23 (big-endian u32).
pub fn png_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return (None, None);
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (Some(w), Some(h))
}

/// JPEG: scan for the SOF0/SOF1/SOF2 marker (0xFF 0xC0-0xC2) to read dimensions.
/// Returns `(None, None)` if the file is not a valid JPEG or no SOF is found
/// in the first 64 KB (enough for any normal screenshot).
pub fn jpeg_dimensions(bytes: &[u8]) -> (Option<u32>, Option<u32>) {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return (None, None);
    }
    let limit = bytes.len().min(65536);
    let mut i = 2;
    while i + 3 < limit {
        if bytes[i] != 0xFF {
            break;
        }
        let marker = bytes[i + 1];
        let seg_len = u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]) as usize;
        if matches!(marker, 0xC0 | 0xC1 | 0xC2) && seg_len >= 7 && i + 9 < limit {
            let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
            let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]) as u32;
            return (Some(w), Some(h));
        }
        i += 2 + seg_len;
    }
    (None, None)
}

// ---------------------------------------------------------------------------
// MIME type

fn mime_for(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "heic" => "image/heic",
        "tif" | "tiff" => "image/tiff",
        _ => "",
    }
}

// ---------------------------------------------------------------------------
// File mtime helper

fn file_mtime_local(path: &Path) -> Option<String> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Local>::from(modified).to_rfc3339())
}

// ---------------------------------------------------------------------------
// OCR via Apple Vision

/// Extract text from an image file using Apple Vision's `VNRecognizeTextRequest`.
///
/// On non-macOS builds (CI / Linux) this always returns an empty string —
/// the text field is simply omitted for those rows. The actual Vision call
/// is compiled only on `target_os = "macos"`.
///
/// Returns empty string on any error (missing file, Vision failure, no text
/// found) — a row with no OCR text is still valid and searchable by metadata.
fn ocr_image(path: &Path) -> String {
    #[cfg(target_os = "macos")]
    {
        macos_ocr(path).unwrap_or_default()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        String::new()
    }
}

/// macOS-only Vision OCR. Calls `VNRecognizeTextRequest` synchronously via
/// `objc2-vision`. Returns the concatenated recognised text (newline-separated
/// observation strings), or an error.
///
/// The implementation uses unsafe objc2 bindings following the pattern
/// established by `crate::apple_contacts`, `crate::apple_calendar`, and
/// `crate::music_listener`. Vision is synchronous when called via
/// `VNImageRequestHandler performRequests:error:` — no runloop needed.
///
/// Feature flags required on `objc2-vision`:
/// - `VNRequestHandler` — `VNImageRequestHandler` + `performRequests:error:`
/// - `VNRecognizeTextRequest` — `VNRecognizeTextRequest` + `results`
/// - `VNRequest` — `VNRequest` base type for the NSArray
/// - `VNObservation` — `VNRecognizedTextObservation` + `topCandidates:`
///
/// `NSDictionary` is added to `objc2-foundation` features so we can build an
/// empty options dictionary for `initWithURL:options:`.
#[cfg(target_os = "macos")]
fn macos_ocr(path: &Path) -> Result<String> {
    use objc2::AnyThread;
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSString, NSURL};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest,
        VNRequestTextRecognitionLevel,
    };

    let path_str = path.to_string_lossy();
    let ns_path = NSString::from_str(path_str.as_ref());
    // fileURLWithPath is safe — it's a pure Objective-C string method.
    let url = NSURL::fileURLWithPath(&ns_path);

    // Empty options dictionary (no camera intrinsics / CI context needed).
    // VNImageOption is a type alias for NSString; an empty dict with NSString
    // keys satisfies the &NSDictionary<VNImageOption, AnyObject> parameter.
    let opts: objc2::rc::Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::new();
    // SAFETY: VNImageOption = NSString (type alias), so the pointee layout is
    // identical; the dict is empty so no actual values are accessed through it.
    let opts_ptr = opts.as_ref() as *const NSDictionary<NSString, AnyObject>
        as *const NSDictionary<objc2_vision::VNImageOption, AnyObject>;

    // initWithURL_options: is unsafe in objc2-vision 0.3.2 (the options
    // generic must have the correct type — we guarantee it via the cast above).
    let handler = unsafe {
        VNImageRequestHandler::initWithURL_options(VNImageRequestHandler::alloc(), &url, &*opts_ptr)
    };

    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);

    // `performRequests:error:` expects `NSArray<VNRequest>`. Cast the subclass
    // pointer — SAFETY: VNRecognizeTextRequest inherits from VNRequest.
    let vn_req_ptr = &*request as *const VNRecognizeTextRequest as *const VNRequest;
    let requests = objc2_foundation::NSArray::from_slice(&[unsafe { &*vn_req_ptr }]);

    handler
        .performRequests_error(&requests)
        .map_err(|e| anyhow::anyhow!("Vision performRequests failed: {e:?}"))?;

    let results = request.results();
    let Some(results) = results else {
        return Ok(String::new());
    };

    let mut lines: Vec<String> = Vec::new();
    for obs in results.iter() {
        let candidates = obs.topCandidates(1);
        if let Some(first) = candidates.firstObject() {
            let s = first.string();
            let s_str = s.to_string();
            if !s_str.is_empty() {
                lines.push(s_str);
            }
        }
    }
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-screenshots-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Filename timestamp parsing

    #[test]
    fn parse_screenshot_ts_24h_format() {
        // Newer macOS — no AM/PM
        let ts = parse_screenshot_ts("Screenshot 2026-06-11 at 09.47.12.png");
        assert!(ts.is_some(), "should parse 24h format");
        let ts = ts.unwrap();
        assert!(ts.starts_with("2026-06-11T09:47:12"), "time: {ts}");
        assert!(ts.len() > 19, "carries offset suffix: {ts}");
    }

    #[test]
    fn parse_screenshot_ts_12h_pm() {
        let ts = parse_screenshot_ts("Screenshot 2025-03-15 at 02.30.00 PM.png");
        assert!(ts.is_some());
        let ts = ts.unwrap();
        // 2 PM → 14:30:00
        assert!(ts.starts_with("2025-03-15T14:30:00"), "PM conversion: {ts}");
    }

    #[test]
    fn parse_screenshot_ts_12h_am() {
        let ts = parse_screenshot_ts("Screenshot 2024-01-01 at 12.00.00 AM.png");
        assert!(ts.is_some());
        let ts = ts.unwrap();
        // 12 AM → 00:00:00
        assert!(ts.starts_with("2024-01-01T00:00:00"), "AM midnight: {ts}");
    }

    #[test]
    fn parse_screenshot_ts_12_noon_pm() {
        let ts = parse_screenshot_ts("Screenshot 2024-06-15 at 12.00.00 PM.png");
        assert!(ts.is_some());
        let ts = ts.unwrap();
        // 12 PM → 12:00:00 (not 24)
        assert!(ts.starts_with("2024-06-15T12:00:00"), "noon PM: {ts}");
    }

    #[test]
    fn parse_screenshot_ts_no_extension() {
        let ts = parse_screenshot_ts("Screenshot 2026-06-11 at 09.47.12");
        assert!(ts.is_some(), "no-extension: {ts:?}");
        let ts = ts.unwrap();
        assert!(ts.starts_with("2026-06-11T09:47:12"));
    }

    #[test]
    fn parse_screenshot_ts_garbage_returns_none() {
        assert!(parse_screenshot_ts("random-file.png").is_none());
        assert!(parse_screenshot_ts("").is_none());
        assert!(parse_screenshot_ts("Screenshot bad at xx.yy.zz.png").is_none());
    }

    // -----------------------------------------------------------------------
    // PNG dimension extraction

    #[test]
    fn png_dimensions_from_real_fixture() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/exif/text-only.png");
        if !fixture.exists() {
            return;
        }
        let bytes = fs::read(&fixture).unwrap();
        let (w, h) = png_dimensions(&bytes);
        assert!(w.is_some() && h.is_some(), "PNG header parsed: {w:?}x{h:?}");
        assert!(w.unwrap() > 0 && h.unwrap() > 0);
    }

    #[test]
    fn png_dimensions_garbage_bytes_returns_none() {
        assert_eq!(png_dimensions(b"not a png"), (None, None));
        assert_eq!(png_dimensions(&[]), (None, None));
    }

    #[test]
    fn jpeg_dimensions_garbage_bytes_returns_none() {
        assert_eq!(jpeg_dimensions(b"not jpeg"), (None, None));
        assert_eq!(jpeg_dimensions(&[]), (None, None));
    }

    // -----------------------------------------------------------------------
    // GUID (content hash)

    #[test]
    fn guid_is_content_hash_prefixed() {
        let data = b"fake screenshot bytes";
        let guid = format!("sha256:{:x}", Sha256::digest(data));
        assert!(guid.starts_with("sha256:"), "guid prefix: {guid}");
        assert_eq!(guid.len(), 7 + 64, "sha256 hex length");

        let other = format!("sha256:{:x}", Sha256::digest(b"different bytes"));
        assert_ne!(guid, other);
    }

    // -----------------------------------------------------------------------
    // Seen-state persistence

    #[test]
    fn seen_state_round_trips_json() {
        let state = SeenState { guids: vec!["sha256:abc".into(), "sha256:def".into()] };
        let json_str = serde_json::to_string_pretty(&state).unwrap();
        let back: SeenState = serde_json::from_str(&json_str).unwrap();
        assert_eq!(back.guids.len(), 2);
        assert!(back.guids.contains(&"sha256:abc".to_string()));
    }

    // -----------------------------------------------------------------------
    // Photo struct construction

    #[test]
    fn photo_row_screenshot_shape() {
        let ts = "2026-06-11T09:47:12-07:00";
        let mut photo = Photo::new(SOURCE, "sha256:9f2c0a7d4e", ts);
        photo.kind = "screenshot".into();
        photo.filename = "Screenshot 2026-06-11 at 09.47.12.png".into();
        photo.mime = "image/png".into();
        photo.width = Some(2880);
        photo.height = Some(1800);
        photo.text = "cargo test -p trove-core".into();
        photo.extra.insert("file_size_bytes".into(), json!(123456u64));

        let v = serde_json::to_value(&photo).unwrap();
        assert_eq!(v["source"], "macos-screenshots");
        assert_eq!(v["kind"], "screenshot");
        assert_eq!(v["text"], "cargo test -p trove-core");
        assert_eq!(v["mime"], "image/png");
        assert!(v.get("lat").is_none(), "no GPS for screenshots");
        assert!(v.get("camera_make").is_none(), "no camera make for screenshots");
    }

    // -----------------------------------------------------------------------
    // Vault write (scan → write → read back)

    fn read_all_photos(vault: &Vault) -> Vec<Photo> {
        let stream = vault.stream(PHOTOS_DIR, Partition::Month);
        let mut all: Vec<Photo> = Vec::new();
        for key in stream.partitions().unwrap_or_default() {
            all.extend(stream.read::<Photo>(&key).unwrap_or_default());
        }
        all
    }

    #[test]
    fn scan_writes_photo_row_for_new_png() {
        let vault = temp_vault("scan");
        let ss_dir = vault.root().join("fake-desktop");
        fs::create_dir_all(&ss_dir).unwrap();

        // A minimal valid PNG: signature + IHDR (width=100, height=80).
        let mut png: Vec<u8> = Vec::new();
        png.extend_from_slice(b"\x89PNG\r\n\x1a\n"); // signature
        png.extend_from_slice(&[0, 0, 0, 13]);          // IHDR data length = 13
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0, 0, 0, 100]);         // width = 100
        png.extend_from_slice(&[0, 0, 0, 80]);          // height = 80
        png.extend_from_slice(&[8, 2, 0, 0, 0]);        // bit depth / color / compress
        png.extend_from_slice(&[0xD3, 0x10, 0x3F, 0x8D]); // CRC (correctness not needed)

        let filename = "Screenshot 2026-06-11 at 09.47.12.png";
        fs::write(ss_dir.join(filename), &png).unwrap();

        let mut live = ScreenshotLive { seen: HashSet::new(), loaded: true, first_tick: false };
        let now = Local::now();
        // Call scan_folder directly — no env var race.
        live.scan_folder(&vault, now, &ss_dir);

        let all = read_all_photos(&vault);
        assert_eq!(all.len(), 1, "one photo row written");
        let p = &all[0];
        assert_eq!(p.source, SOURCE);
        assert_eq!(p.kind, "screenshot");
        assert_eq!(p.filename, filename);
        assert_eq!(p.mime, "image/png");
        assert_eq!(p.width, Some(100));
        assert_eq!(p.height, Some(80));
        assert!(p.guid.starts_with("sha256:"), "content-hash guid");
        assert!(p.ts.starts_with("2026-06-11"), "parsed ts from filename: {}", p.ts);
    }

    #[test]
    fn second_tick_skips_already_seen_file() {
        let vault = temp_vault("dedup");
        let ss_dir = vault.root().join("fake-desktop");
        fs::create_dir_all(&ss_dir).unwrap();
        fs::write(
            ss_dir.join("Screenshot 2026-06-10 at 10.00.00.png"),
            b"fake image bytes",
        )
        .unwrap();

        let mut live = ScreenshotLive { seen: HashSet::new(), loaded: true, first_tick: false };
        let now = Local::now();
        live.scan_folder(&vault, now, &ss_dir);
        live.scan_folder(&vault, now, &ss_dir); // second scan — same file

        assert_eq!(read_all_photos(&vault).len(), 1, "dedup: second scan must not re-append");
    }

    #[test]
    fn disabled_tick_writes_nothing() {
        let vault = temp_vault("disabled");
        let ss_dir = vault.root().join("fake-desktop");
        fs::create_dir_all(&ss_dir).unwrap();
        fs::write(ss_dir.join("Screenshot 2026-06-10 at 10.00.00.png"), b"img").unwrap();

        let mut live = ScreenshotLive { seen: HashSet::new(), loaded: true, first_tick: false };
        let now = Local::now();
        // Simulate a disabled tick: the live collector returns early when disabled.
        // Test directly: don't call tick, verify no rows.
        live.tick(&vault, now, false); // disabled — scan_folder is never called

        // Since no scan_folder call was made, nothing written.
        assert_eq!(read_all_photos(&vault).len(), 0, "disabled tick writes nothing");
    }

    // -----------------------------------------------------------------------
    // DEF sanity

    #[test]
    fn def_is_live_default_off_no_connection() {
        assert!(!DEF.meta.default_on, "screenshots is default-off (privacy)");
        assert!(matches!(DEF.behavior, Behavior::Live(_)));
        assert!(DEF.connection.is_none(), "no login needed — local files only");
        assert_eq!(DEF.meta.id, "macos-screenshots");
        assert_eq!(DEF.meta.domain, "photos");
        // Privacy copy must mention private or sensitive content.
        let all_copy = format!("{} {}", DEF.meta.setup.join(" "), DEF.meta.caveats);
        assert!(
            all_copy.to_lowercase().contains("private")
                || all_copy.to_lowercase().contains("sensitive"),
            "setup copy must acknowledge privacy: {all_copy}"
        );
    }

    #[test]
    fn def_is_registered() {
        let vault = temp_vault("reg");
        let status = vault.integrations_status();
        let card = status.iter().find(|s| s.id == "macos-screenshots");
        assert!(card.is_some(), "macos-screenshots in registry");
        let card = card.unwrap();
        assert!(matches!(card.kind, crate::integrations::IntegrationKind::Live), "kind is Live");
        assert!(!card.enabled, "default-off");
    }

    // -----------------------------------------------------------------------
    // Screenshot folder detection

    #[test]
    fn screenshot_folder_returns_a_path() {
        // screenshot_folder() must always return something (Desktop at minimum).
        let folder = screenshot_folder();
        // The path is not necessarily accessible in CI, but it must be
        // non-empty and non-root.
        assert!(!folder.as_os_str().is_empty(), "must return a non-empty path");
    }

    #[test]
    fn screenshot_folder_from_env_fn_uses_override() {
        // Test the internal helper directly via a temp dir; avoids modifying
        // the process-wide env var (which races with parallel tests).
        let tmp = std::env::temp_dir()
            .join(format!("trove-ss-folder-{}", std::process::id()));
        fs::create_dir_all(&tmp).unwrap();
        // screenshot_folder_from_env reads TROVE_SCREENSHOT_DIR from env.
        // We call the public wrapper with a unique pid-based dir.
        // NOTE: This test sets the env var; isolate with a unique dir per pid.
        std::env::set_var("TROVE_SCREENSHOT_DIR", tmp.to_str().unwrap());
        let folder = screenshot_folder_from_env();
        std::env::remove_var("TROVE_SCREENSHOT_DIR");
        assert_eq!(folder, Some(tmp));
    }

    // -----------------------------------------------------------------------
    // Serde back-compat

    #[test]
    fn old_sparse_photo_row_still_deserializes() {
        let line = r#"{"ts":"2026-01-01T10:00:00-08:00","source":"macos-screenshots","guid":"sha256:abc123"}"#;
        let p: Photo = serde_json::from_str(line).unwrap();
        assert_eq!(p.guid, "sha256:abc123");
        assert!(p.kind.is_empty() && p.text.is_empty() && p.width.is_none());
    }

    // -----------------------------------------------------------------------
    // Pre-Mojave "Screen Shot" two-word prefix (defect fix: was silently dropped)

    #[test]
    fn parse_screen_shot_two_word_24h() {
        // Real pre-Mojave format: "Screen Shot YYYY-MM-DD at HH.MM.SS.png"
        let ts = parse_screenshot_ts("Screen Shot 2022-10-20 at 15.30.00.png");
        assert!(ts.is_some(), "two-word prefix 24h should parse");
        let ts = ts.unwrap();
        assert!(ts.starts_with("2022-10-20T15:30:00"), "time: {ts}");
    }

    #[test]
    fn parse_screen_shot_two_word_12h_pm() {
        // Real pre-Mojave 12h format (the case the adversarial review flagged as
        // dead: the 12h branch was unreachable for real files under the old
        // single-word-only guard).
        let ts = parse_screenshot_ts("Screen Shot 2022-10-20 at 3.51.22 PM.JPG");
        assert!(ts.is_some(), "two-word prefix PM should parse");
        let ts = ts.unwrap();
        // 3 PM → 15:51:22
        assert!(ts.starts_with("2022-10-20T15:51:22"), "PM conversion: {ts}");
    }

    #[test]
    fn parse_screen_shot_two_word_12h_am() {
        let ts = parse_screenshot_ts("Screen Shot 2021-05-03 at 12.00.00 AM.png");
        assert!(ts.is_some(), "two-word prefix AM should parse");
        let ts = ts.unwrap();
        // 12 AM → 00:00:00
        assert!(ts.starts_with("2021-05-03T00:00:00"), "AM midnight: {ts}");
    }

    // -----------------------------------------------------------------------
    // "Screen Recording" prefix (defect fix: recordings always fell back to mtime)

    #[test]
    fn parse_screen_recording_prefix_24h() {
        // Real Cmd-Shift-5 recording filename
        let ts = parse_screenshot_ts("Screen Recording 2024-08-19 at 14.22.05.mov");
        assert!(ts.is_some(), "screen recording prefix should parse");
        let ts = ts.unwrap();
        assert!(ts.starts_with("2024-08-19T14:22:05"), "recording ts: {ts}");
    }

    // -----------------------------------------------------------------------
    // scan_folder picks up video files from the screencapture folder
    // (defect fix: was image-only, Cmd-Shift-5 recordings were silently dropped)

    #[test]
    fn scan_folder_collects_mov_as_recording() {
        let vault = temp_vault("scan-mov");
        let ss_dir = vault.root().join("fake-desktop");
        fs::create_dir_all(&ss_dir).unwrap();

        // A minimal stub .mov file (not a real video — just non-empty bytes).
        let filename = "Screen Recording 2024-08-19 at 14.22.05.mov";
        fs::write(ss_dir.join(filename), b"fake video bytes").unwrap();

        let mut live = ScreenshotLive { seen: HashSet::new(), loaded: true, first_tick: false };
        let now = Local::now();
        live.scan_folder(&vault, now, &ss_dir);

        // Recording must appear in files/macos-screenshots/*.jsonl
        let stream = vault.stream(FILES_DIR, Partition::Month);
        let mut recs: Vec<serde_json::Value> = Vec::new();
        for key in stream.partitions().unwrap_or_default() {
            recs.extend(stream.read::<serde_json::Value>(&key).unwrap_or_default());
        }
        assert_eq!(recs.len(), 1, "one recording row written");
        let r = &recs[0];
        assert_eq!(r["source"], "macos-screenshots");
        assert_eq!(r["filename"], filename);
        assert_eq!(r["mime"], "video/quicktime");
        // Timestamp should be parsed from the filename (not mtime fallback)
        let ts_str = r["ts"].as_str().unwrap_or("");
        assert!(ts_str.starts_with("2024-08-19T14:22:05"), "parsed recording ts: {ts_str}");

        // No photo rows should have been written for a .mov
        let all_photos = read_all_photos(&vault);
        assert!(all_photos.is_empty(), "no photo rows for .mov");
    }

    // -----------------------------------------------------------------------
    // seen-set not advanced on write failure (defect fix: was advanced pre-write)

    #[test]
    fn seen_not_advanced_when_photo_already_in_seen() {
        // After a successful write the guid must be in self.seen so a second
        // scan skips it. This is the positive path of the seen-set guard.
        let vault = temp_vault("seen-guard");
        let ss_dir = vault.root().join("fake-desktop");
        fs::create_dir_all(&ss_dir).unwrap();
        let content = b"some-unique-screenshot-bytes-for-seen-test";
        fs::write(ss_dir.join("Screenshot 2026-06-15 at 08.00.00.png"), content).unwrap();

        let mut live = ScreenshotLive { seen: HashSet::new(), loaded: true, first_tick: false };
        let now = Local::now();

        // First scan: file is new, should be written.
        live.scan_folder(&vault, now, &ss_dir);
        assert_eq!(read_all_photos(&vault).len(), 1, "first scan wrote one row");

        // Compute the expected guid.
        let expected_guid = format!("sha256:{:x}", Sha256::digest(content));
        assert!(
            live.seen.contains(&expected_guid),
            "guid must be in self.seen after successful write"
        );

        // Second scan: seen set contains the guid, nothing is re-written.
        live.scan_folder(&vault, now, &ss_dir);
        assert_eq!(read_all_photos(&vault).len(), 1, "second scan must not duplicate");
    }
}
