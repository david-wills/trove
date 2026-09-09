//! EXIF / media-metadata import — loose image and video files the user drags
//! in (a folder, a camera card, individual files) into the unified `photos`
//! stream.
//!
//! The companion path to a future Apple Photos collector, for files (or whole
//! libraries) that never enter Photos.app. Each dropped file becomes one row in
//! `photos/exif-import/YYYY-MM.jsonl` carrying capture time, GPS, camera/lens,
//! and dimensions — **metadata only; the image/video bytes are never copied
//! into the vault.** A dropped directory is walked recursively; anything inside
//! a `.photoslibrary` bundle is skipped (the future `apple-photos` collector
//! owns that library, and double-counting it here would write the same asset
//! twice).
//!
//! Parsing is pure Rust ([`nom_exif`], compiled in — no ExifTool binary, no
//! network), so it keeps the standalone rule. It handles JPEG/HEIC/HEIF/AVIF/
//! TIFF/PNG plus the MOV/MP4/3GP video containers (and the CR3/RAF/IIQ raw
//! formats nom-exif already reads); other RAW (CR2/NEF/ARW) is deferred until a
//! sample set is in hand — a file nom-exif can't parse still lands a row off the
//! file's mtime (so nothing the user drops silently vanishes).
//!
//! `guid` is the **whole-file SHA-256** (`sha256:<hex>`): the same photo dropped
//! twice, or reached through two paths, dedupes to one row. The import loads the
//! guids already in the target month files and skips them before appending, so a
//! re-import adds nothing.
//!
//! **Privacy:** GPS geotags form a location trail, so this source is
//! `default_on: false` — a deliberate opt-in behind the import box's
//! acknowledgement. It carries no face data, so it never writes the opt-in
//! `people` / `people_name` columns.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Local, TimeZone, Utc};
use nom_exif::{
    EntryValue, Exif, ExifDateTime, ExifTag, Metadata, TrackInfo, TrackInfoTag,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "photos/exif-import";
const SOURCE: &str = "exif-import";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "exif-import",
        name: "Image Files (EXIF)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Extracts EXIF metadata — capture time, GPS, camera and lens, \
                      dimensions — from JPEG, HEIC, TIFF, PNG, and video files you \
                      drop in (a folder, a camera card, or single files). Image and \
                      video data is never copied into the vault. Re-runnable: the \
                      same file dropped twice never duplicates.",
        domain: "photos",
        vault_path: "photos/exif-import/",
        toggleable: false,
        setup: &[
            "Drop a folder, a camera card, or individual image/video files here — they're read in place; nothing is moved or copied.",
            "Heads-up: photo GPS tags form a location trail. Only import files whose location history you're comfortable indexing.",
        ],
        caveats: "Metadata only — the vault never stores the image or video itself. \
                  GPS, camera, and time come from whatever the file actually carries; \
                  a stripped or scanned image still lands with just the fields it has \
                  (timestamp falls back to the file's modified date). Mainstream RAW \
                  beyond CR3/RAF/IIQ (CR2/NEF/ARW) isn't parsed yet — those files land \
                  with a mtime row rather than full EXIF.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // Extensions the file picker offers; a dropped *directory* is walked and
    // these extensions are matched inside it. nom-exif also reads CR3/RAF/IIQ.
    accepts: &[
        "jpg", "jpeg", "heic", "heif", "avif", "tif", "tiff", "png", "mov", "mp4", "m4v", "3gp",
        "cr3", "raf", "iiq",
    ],
    params: &[],
    run: run_import,
};

/// File extensions this collector treats as image/video candidates. A dropped
/// directory yields a row only for files matching one of these (skipping
/// `.photoslibrary` bundles); a directly-dropped file is always attempted (the
/// picker already constrained it), but an unknown extension still maps a `mime`
/// of "" and relies on nom-exif / the mtime fallback.
const CANDIDATE_EXTS: &[&str] = &[
    "jpg", "jpeg", "heic", "heif", "avif", "tif", "tiff", "png", "mov", "mp4", "m4v", "3gp", "cr3",
    "raf", "iiq",
];

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let stream = vault.stream(DIR, Partition::Month);
    // Already-stored content hashes, for a re-runnable import: a re-dropped
    // file (or one reached via a second path) dedupes by whole-file guid.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for p in stream.read::<Photo>(&key)? {
            if !p.guid.is_empty() {
                seen.insert(p.guid);
            }
        }
    }

    // Collect candidate files: a single dropped file, or every image/video
    // under a dropped directory (recursive, `.photoslibrary` bundles skipped).
    let mut files: Vec<PathBuf> = Vec::new();
    if path.is_dir() {
        walk_dir(path, &mut files);
    } else {
        files.push(path.to_path_buf());
    }
    files.sort();

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut rows: Vec<Photo> = Vec::new();
    let total = files.len().max(1);
    for (i, file) in files.iter().enumerate() {
        match photo_for(file) {
            Ok(Some(photo)) => {
                if !seen.insert(photo.guid.clone()) {
                    duplicates += 1;
                } else {
                    rows.push(photo);
                    imported += 1;
                }
            }
            // A non-image/video, or a file we genuinely could not place in a
            // month (no parseable time and no readable mtime): skip, never fatal.
            Ok(None) => skipped += 1,
            // One corrupt/unreadable file must never abort the whole import.
            Err(_) => skipped += 1,
        }
        if i % 25 == 0 {
            progress(ImportProgress {
                records: imported,
                percent: (i as f32 / total as f32) * 100.0,
            });
        }
    }

    stream.append(&rows, |p| &p.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} files indexed, {duplicates} duplicates skipped, {skipped} non-media skipped"
        ),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Recursively collect image/video files under `dir`, skipping any path inside
/// a `.photoslibrary` bundle (owned by the future `apple-photos` collector) and
/// dot-directories. Unreadable subdirectories are silently skipped.
///
/// Symlinks are **skipped entirely, never followed** (lstat via
/// `symlink_metadata`, the `cloud_folder.rs` precedent): a symlink inside a
/// dropped folder that points outside the tree must not pull metadata from
/// wherever it targets — the walk doesn't escape the dropped tree.
fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let p = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // lstat — do not follow symlinks. A symlink (to a file OR a directory)
        // is skipped outright, so the walk can never escape the dropped tree.
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            // Never descend into a Photos library bundle — `apple-photos` owns
            // it; indexing it here would double-count every asset.
            if name.ends_with(".photoslibrary") {
                continue;
            }
            // Skip hidden/system dirs (.git, .Trash, Letterboxd's deleted/, …
            // are not where loose photos live).
            if name.starts_with('.') {
                continue;
            }
            walk_dir(&p, out);
        } else if meta.is_file() && is_candidate(&p) {
            out.push(p);
        }
    }
}

/// Does this file's extension mark it as an image/video candidate?
fn is_candidate(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            CANDIDATE_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

/// MIME type inferred from the file extension. "" when unknown (still a valid
/// omit-empty field).
fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("heic") => "image/heic",
        Some("heif") => "image/heif",
        Some("avif") => "image/avif",
        Some("tif" | "tiff") => "image/tiff",
        Some("png") => "image/png",
        Some("mov") => "video/quicktime",
        Some("mp4" | "m4v") => "video/mp4",
        Some("3gp") => "video/3gpp",
        Some("cr3") => "image/x-canon-cr3",
        Some("raf") => "image/x-fuji-raf",
        Some("iiq") => "image/x-phaseone-iiq",
        _ => "",
    }
}

/// Build a [`Photo`] for one file. `Ok(None)` when the file can't be placed in
/// any month (no parseable capture time *and* no readable mtime — never expected
/// for a real local file, but handled rather than guessed). `Err` propagates a
/// read failure to the caller, which counts it as skipped.
fn photo_for(path: &Path) -> Result<Option<Photo>> {
    // Whole-file content hash — the dedupe key. Bounded by IO; fine for the
    // image/video sizes a user drags in.
    let bytes = std::fs::read(path)?;
    let guid = format!("sha256:{:x}", Sha256::digest(&bytes));
    drop(bytes); // we never keep, copy, or persist the pixels.

    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mime = mime_for(path);

    let mut photo = Photo::new(SOURCE, &guid, String::new());
    photo.filename = filename;
    photo.mime = mime.to_string();

    // Try a real metadata parse; on any parser error fall back to a mtime row
    // (the file is still real and droppable — don't lose it).
    match nom_exif::read_metadata(path) {
        Ok(Metadata::Exif(exif)) => fill_from_exif(&mut photo, &exif),
        Ok(Metadata::Track(track)) => fill_from_track(&mut photo, &track),
        Err(_) => {}
    }

    // `ts` fallback: the file's modified time, RFC3339 local. An EXIF-less file
    // still becomes a row (guid + filename + mime present).
    if photo.ts.is_empty() {
        match file_mtime_local(path) {
            Some(ts) => photo.ts = ts,
            None => return Ok(None),
        }
    }
    Ok(Some(photo))
}

/// Map an image's EXIF onto the [`Photo`] columns, and preserve *every* tag the
/// normalized columns don't carry under `extra` (lossless — the brief's
/// "remaining tags in extra"). `lens`/`altitude`/`orientation` land in `extra`
/// per the spec, not as top-level columns.
fn fill_from_exif(photo: &mut Photo, exif: &Exif) {
    photo.kind = "photo".into();

    // Capture time. nom-exif composes DateTimeOriginal with OffsetTimeOriginal:
    // offset-aware → `Aware(DateTime<FixedOffset>)`, naive → `Naive`. Never
    // synthesize UTC — a naive time gets the system local offset.
    if let Some(dt) = exif.get(ExifTag::DateTimeOriginal).and_then(|v| v.as_datetime()) {
        photo.ts = datetime_to_local_rfc3339(dt);
    }

    if let Some(make) = exif.get(ExifTag::Make).and_then(|v| v.as_str()) {
        photo.camera_make = make.trim().to_string();
    }
    if let Some(model) = exif.get(ExifTag::Model).and_then(|v| v.as_str()) {
        photo.camera_model = model.trim().to_string();
    }

    // Dimensions: prefer the EXIF PixelXDimension/PixelYDimension (0xa002/3,
    // the real pixel size), fall back to the TIFF ImageWidth/ImageHeight.
    photo.width = exif
        .get(ExifTag::ExifImageWidth)
        .or_else(|| exif.get(ExifTag::ImageWidth))
        .and_then(|v| v.as_u32());
    photo.height = exif
        .get(ExifTag::ExifImageHeight)
        .or_else(|| exif.get(ExifTag::ImageHeight))
        .and_then(|v| v.as_u32());

    // GPS: already signed WGS84 decimal degrees. Guard None — omit lat/lon if
    // the asset carries no geotag.
    if let Some(gps) = exif.gps_info() {
        photo.lat = gps.latitude_decimal();
        photo.lon = gps.longitude_decimal();
        if let Some(alt) = gps.altitude_meters() {
            photo.extra.insert("altitude".into(), json_num(alt));
        }
    }

    // Full fidelity: every REAL tag NOT mapped to a column goes into `extra`,
    // keyed by its tag name (lens/orientation/exposure/iso/… all land here).
    //
    // Determinism: nom-exif's `iter()` yields entries in HashMap order (not
    // stable), and the same tag can appear in IFD0 *and* the thumbnail IFD
    // (ResolutionUnit/XResolution/…). Flattening blindly would let the
    // last-visited copy win nondeterministically → unstable vault output and
    // noisy git diffs. So we sort by (IFD, tag code) — IFD0 first — and keep
    // the FIRST value per tag name, so the primary-image value always wins.
    let mut entries: Vec<_> = exif.iter().collect();
    entries.sort_by_key(|e| (e.ifd, e.tag.code()));
    for entry in entries {
        if is_mapped_exif_tag(entry.tag.tag()) || is_pointer_or_noise_tag(&entry) {
            continue;
        }
        let key = tag_name(&entry.tag);
        // First-wins: a thumbnail-IFD duplicate never clobbers the IFD0 value.
        photo
            .extra
            .entry(key)
            .or_insert_with(|| entry_value_to_json(entry.value));
    }
}

/// Internal IFD structure that is not photo metadata and must not pollute
/// `extra`: the offset/pointer pseudo-tags that locate sub-IFDs and the
/// thumbnail (`ExifOffset`, `GPSInfo`, `InteropOffset`, `ThumbnailOffset`,
/// `ThumbnailLength`), plus an *empty* `OffsetTime` (a blank timezone string
/// — `OffsetTimeOriginal` already feeds `ts`). Genuine `OffsetTime` content,
/// and every other real tag, is preserved.
fn is_pointer_or_noise_tag(entry: &nom_exif::ExifEntry) -> bool {
    match entry.tag.tag() {
        Some(
            ExifTag::ExifOffset
            | ExifTag::GPSInfo
            | ExifTag::InteropOffset
            | ExifTag::ThumbnailOffset
            | ExifTag::ThumbnailLength,
        ) => true,
        // Strip OffsetTime only when it carries no actual offset string.
        Some(ExifTag::OffsetTime) => {
            entry.value.as_str().map(|s| s.trim().is_empty()).unwrap_or(true)
        }
        _ => false,
    }
}

/// Map a video container's track metadata onto the [`Photo`] columns. `kind` is
/// `"video"`; `ts` is the container creation time; `duration_secs` from
/// `DurationMs`.
fn fill_from_track(photo: &mut Photo, track: &TrackInfo) {
    photo.kind = "video".into();

    if let Some(dt) = track.get(TrackInfoTag::CreateDate).and_then(|v| v.as_datetime()) {
        // nom-exif returns the container creation time already offset-aware
        // (`ExifDateTime::Aware`, e.g. +08:00) — we preserve that offset as-is,
        // never re-zone it. (A rare naive value would take the system local
        // offset, like the image path; both go through the same helper.)
        photo.ts = datetime_to_local_rfc3339(dt);
    }
    if let Some(ms) = track.get(TrackInfoTag::DurationMs).and_then(|v| v.try_as_integer()) {
        photo.duration_secs = Some(ms as f64 / 1000.0);
    }
    if let Some(make) = track.get(TrackInfoTag::Make).and_then(|v| v.as_str()) {
        photo.camera_make = make.trim().to_string();
    }
    if let Some(model) = track.get(TrackInfoTag::Model).and_then(|v| v.as_str()) {
        photo.camera_model = model.trim().to_string();
    }
    photo.width = track.get(TrackInfoTag::Width).and_then(|v| v.as_u32());
    photo.height = track.get(TrackInfoTag::Height).and_then(|v| v.as_u32());

    if let Some(gps) = track.gps_info() {
        photo.lat = gps.latitude_decimal();
        photo.lon = gps.longitude_decimal();
        if let Some(alt) = gps.altitude_meters() {
            photo.extra.insert("altitude".into(), json_num(alt));
        }
    }

    // Full fidelity: any track tag not mapped to a column → `extra`.
    for (tag, value) in track.iter() {
        if matches!(
            tag,
            TrackInfoTag::CreateDate
                | TrackInfoTag::DurationMs
                | TrackInfoTag::Make
                | TrackInfoTag::Model
                | TrackInfoTag::Width
                | TrackInfoTag::Height
                | TrackInfoTag::GpsIso6709
        ) {
            continue;
        }
        photo
            .extra
            .insert(format!("{tag:?}"), entry_value_to_json(value));
    }
}

/// EXIF tags that already have a normalized [`Photo`] column, so they are not
/// duplicated into `extra`. (DateTimeOriginal/OffsetTimeOriginal → `ts`,
/// Make/Model → camera_*, the dimension tags → width/height, GPS tags are
/// parsed via `gps_info`.) Orientation and lens are deliberately NOT here — the
/// spec keeps them in `extra`.
fn is_mapped_exif_tag(tag: Option<ExifTag>) -> bool {
    matches!(
        tag,
        Some(
            ExifTag::DateTimeOriginal
                | ExifTag::OffsetTimeOriginal
                | ExifTag::Make
                | ExifTag::Model
                | ExifTag::ExifImageWidth
                | ExifTag::ExifImageHeight
                | ExifTag::ImageWidth
                | ExifTag::ImageHeight
                | ExifTag::GPSLatitude
                | ExifTag::GPSLatitudeRef
                | ExifTag::GPSLongitude
                | ExifTag::GPSLongitudeRef
                | ExifTag::GPSAltitude
                | ExifTag::GPSAltitudeRef
        )
    )
}

/// A stable, human-readable key for an EXIF tag in `extra`: the recognized tag
/// name (e.g. `"LensModel"`, `"Orientation"`) or `"Unknown(0xNNNN)"` for codes
/// nom-exif doesn't name. Uses the crate's `Display`.
fn tag_name(tag: &nom_exif::TagOrCode) -> String {
    tag.to_string()
}

/// Convert an [`ExifDateTime`] to an RFC3339 *local* string. An offset-aware
/// value keeps its own offset; a naive value is attached to the **system local**
/// offset (never UTC). A naive time that the local zone can't resolve uniquely
/// (a DST gap) falls back to the naive time formatted with the current local
/// offset.
fn datetime_to_local_rfc3339(dt: ExifDateTime) -> String {
    match dt {
        ExifDateTime::Aware(d) => d.to_rfc3339(),
        ExifDateTime::Naive(ndt) => match Local.from_local_datetime(&ndt).single() {
            Some(local) => local.to_rfc3339(),
            None => Local
                .from_local_datetime(&ndt)
                .earliest()
                .map(|d| d.to_rfc3339())
                .unwrap_or_else(|| {
                    DateTime::<Local>::from(Utc.from_utc_datetime(&ndt)).to_rfc3339()
                }),
        },
    }
}

/// File modified time as RFC3339 local. `None` when unreadable.
fn file_mtime_local(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Local>::from(modified).to_rfc3339())
}

/// JSON number for an f64 (NaN/inf are not representable → null, harmless).
fn json_num(x: f64) -> Value {
    serde_json::Number::from_f64(x).map(Value::Number).unwrap_or(Value::Null)
}

/// Lossless-ish JSON encoding of an arbitrary EXIF [`EntryValue`] for `extra`:
/// numbers stay numbers, text stays text, everything else (rationals, arrays,
/// undefined blobs, datetimes) falls back to the crate's `Display` string so no
/// tag is ever dropped.
fn entry_value_to_json(v: &EntryValue) -> Value {
    match v {
        EntryValue::Text(s) => Value::String(s.clone()),
        EntryValue::U8(_)
        | EntryValue::U16(_)
        | EntryValue::U32(_)
        | EntryValue::U64(_)
        | EntryValue::I8(_)
        | EntryValue::I16(_)
        | EntryValue::I32(_)
        | EntryValue::I64(_) => v
            .try_as_integer()
            .map(|n| Value::Number(n.into()))
            .unwrap_or_else(|| Value::String(v.to_string())),
        EntryValue::F32(f) => json_num(*f as f64),
        EntryValue::F64(f) => json_num(*f),
        // Rationals, arrays, datetimes, undefined: keep the value's own readable
        // rendering rather than dropping it.
        _ => Value::String(v.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-exif-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// The RFC3339 offset suffix this machine's local zone gives a naive
    /// wall-clock time (e.g. `-07:00`). Mirrors the collector's naive→local path
    /// so the assertion is offset-agnostic across CI machines (never UTC unless
    /// the machine itself runs UTC).
    fn local_offset_suffix(naive: &str) -> String {
        let ndt = NaiveDateTime::parse_from_str(naive, "%Y-%m-%dT%H:%M:%S").unwrap();
        let local = Local.from_local_datetime(&ndt).single().unwrap();
        let rfc = local.to_rfc3339();
        // The offset is the trailing `+HH:MM` / `-HH:MM` (or `Z`? to_rfc3339
        // never emits Z for a fixed offset — it's always ±HH:MM).
        rfc[rfc.len() - 6..].to_string()
    }

    /// The canonical nom-exif test corpus, copied into this crate's fixture
    /// tree, exercises the *real* parse path (no hand-rolled byte blobs).
    fn fixtures_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/exif")
    }

    fn fixture(name: &str) -> PathBuf {
        fixtures_dir().join(name)
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    fn rows(v: &Vault) -> Vec<Photo> {
        let stream = v.stream(DIR, Partition::Month);
        let mut out = Vec::new();
        for key in stream.partitions().unwrap() {
            out.extend(stream.read::<Photo>(&key).unwrap());
        }
        out
    }

    #[test]
    fn gps_bearing_image_yields_lat_lon_camera_dims_ts() {
        // exif-no-tz.jpg (nom-exif corpus, small) carries GPS, Make/Model
        // (vivo / vivo X90 Pro+), 3072x4096 dimensions, and a *naive*
        // DateTimeOriginal — the rich-row case AND the naive→local-offset ts path.
        let v = temp_vault("gps");
        let out = run(&v, &fixture("exif-no-tz.jpg"));
        assert_eq!(out.counts.get("imported"), Some(&1), "{}", out.headline);

        let r = &rows(&v)[0];
        assert_eq!(r.source, "exif-import");
        assert_eq!(r.kind, "photo");
        assert!(r.guid.starts_with("sha256:"), "content-hash guid: {}", r.guid);
        assert_eq!(r.mime, "image/jpeg");
        // Signed WGS84 decimal degrees: 22°31'52.08"N, 114°1'17.33"E → both positive.
        let lat = r.lat.expect("lat present");
        let lon = r.lon.expect("lon present");
        assert!((lat - 22.5311).abs() < 1e-3, "signed lat ~+22.531: {lat}");
        assert!((lon - 114.0215).abs() < 1e-3, "signed lon ~+114.021: {lon}");
        assert_eq!(r.width, Some(3072));
        assert_eq!(r.height, Some(4096));
        assert_eq!(r.camera_make, "vivo");
        assert_eq!(r.camera_model, "vivo X90 Pro+");
        // Naive DateTimeOriginal → attached to the system local offset (never UTC).
        // The local wall-clock time is preserved; the offset is whatever this
        // machine runs in, so assert RFC3339 shape + the date, not a fixed offset.
        assert!(r.ts.starts_with("2023-07-09T20:36:33"), "naive-local ts: {}", r.ts);
        assert!(
            r.ts.ends_with(&local_offset_suffix("2023-07-09T20:36:33")),
            "ts carries the system local offset, not UTC: {}",
            r.ts
        );
        // Lossless: unmapped tags (lens, exposure, orientation, …) are preserved
        // in `extra`, never dropped — and never promoted to top-level columns.
        assert!(!r.extra.is_empty(), "remaining tags preserved in extra");
        let json = serde_json::to_value(r).unwrap();
        for col in ["lens", "orientation", "altitude_meters", "FNumber"] {
            assert!(json.get(col).is_none(), "{col} is not a top-level column");
        }
        // GPSAltitude was 0 → altitude lives in extra, not a column.
        assert_eq!(r.extra.get("altitude"), Some(&json!(0.0)));
        // people columns are never emitted (exif-import has no face data).
        assert!(r.people.is_empty() && r.people_name.is_empty());
    }

    #[test]
    fn image_without_gps_yields_sparse_row_no_lat_lon() {
        // tif.tif carries EXIF (Orientation, dimensions) but NO GPS and no
        // DateTimeOriginal → a sparse row: no lat/lon, mtime-fallback ts,
        // Orientation preserved in `extra` (a column the spec keeps out of top).
        let v = temp_vault("nogps");
        let out = run(&v, &fixture("tif.tif"));
        assert_eq!(out.counts.get("imported"), Some(&1), "{}", out.headline);
        let r = &rows(&v)[0];
        assert!(r.lat.is_none() && r.lon.is_none(), "no geotag → omit lat/lon");
        assert_eq!(r.mime, "image/tiff");
        assert_eq!(r.kind, "photo");
        assert!(!r.ts.is_empty());
        // Orientation is in extra, not a top-level column.
        assert!(r.extra.contains_key("Orientation"), "orientation preserved in extra");
        // Omit-empty: a GPS-less serialized row has no lat/lon keys at all.
        let json = serde_json::to_value(r).unwrap();
        assert!(json.get("lat").is_none() && json.get("lon").is_none());
        assert!(json.get("orientation").is_none(), "orientation never a top-level column");
    }

    #[test]
    fn no_exif_file_falls_back_to_mtime_ts() {
        // text-only.png is a real, parser-valid PNG with no EXIF at all
        // (read_metadata returns Err) — it still becomes a row: guid + filename
        // + mime present, ts from the file's mtime (local RFC3339).
        let v = temp_vault("noexif");
        let out = run(&v, &fixture("text-only.png"));
        assert_eq!(out.counts.get("imported"), Some(&1), "{}", out.headline);
        let r = &rows(&v)[0];
        assert_eq!(r.mime, "image/png");
        assert_eq!(r.filename, "text-only.png");
        assert!(r.guid.starts_with("sha256:"));
        assert!(!r.ts.is_empty() && r.ts.contains('T'), "mtime-fallback ts: {}", r.ts);
        // No EXIF → none of the EXIF-derived fields, and no kind set.
        assert!(r.lat.is_none() && r.camera_make.is_empty() && r.kind.is_empty());
        assert!(r.extra.is_empty(), "no tags to preserve");
    }

    #[test]
    fn video_yields_kind_video_and_duration() {
        // meta.mov (corpus): kind:"video", DurationMs=500 → 0.5s, 720x1280,
        // GPS, container creation time.
        let v = temp_vault("video");
        let out = run(&v, &fixture("meta.mov"));
        assert_eq!(out.counts.get("imported"), Some(&1), "{}", out.headline);
        let r = &rows(&v)[0];
        assert_eq!(r.kind, "video");
        assert_eq!(r.mime, "video/quicktime");
        assert_eq!(r.duration_secs, Some(0.5), "DurationMs 500 → 0.5s");
        assert_eq!(r.width, Some(720));
        assert_eq!(r.height, Some(1280));
        // GpsIso6709 → parsed signed decimal (27.1281, 100.2508).
        assert!((r.lat.unwrap() - 27.1281).abs() < 1e-3, "video lat: {:?}", r.lat);
        assert!((r.lon.unwrap() - 100.2508).abs() < 1e-3, "video lon: {:?}", r.lon);
        assert!(!r.ts.is_empty() && r.ts.contains('T'));
    }

    #[test]
    fn folder_drop_walks_recursively_and_dedupes_on_second_drop() {
        // A directory drop is walked recursively; dropping the same folder twice
        // adds zero rows (whole-file hash dedupe).
        let v = temp_vault("folder");
        let root = v.root().join("drop");
        let nested = root.join("sub");
        fs::create_dir_all(&nested).unwrap();
        fs::copy(fixture("exif-no-tz.jpg"), root.join("a.jpg")).unwrap();
        fs::copy(fixture("text-only.png"), nested.join("b.png")).unwrap();

        let out = run(&v, &root);
        let n = out.counts.get("imported").copied().unwrap_or(0);
        assert_eq!(n, 2, "both files (incl. nested) indexed: {}", out.headline);
        assert_eq!(rows(&v).len(), 2);

        // Second drop of the same folder: every file is a duplicate, 0 new.
        let again = run(&v, &root);
        assert_eq!(again.counts.get("imported"), Some(&0), "re-import adds nothing");
        assert_eq!(again.counts.get("duplicates"), Some(&2));
        assert_eq!(rows(&v).len(), 2, "row count unchanged after re-drop");
    }

    #[test]
    fn same_image_via_two_paths_is_one_row() {
        // Two copies of the same bytes under different names dedupe to one row
        // (the guid is the content hash, not the path).
        let v = temp_vault("twopaths");
        let root = v.root().join("dup");
        fs::create_dir_all(&root).unwrap();
        fs::copy(fixture("exif-no-tz.jpg"), root.join("first.jpg")).unwrap();
        fs::copy(fixture("exif-no-tz.jpg"), root.join("second.jpg")).unwrap();
        let out = run(&v, &root);
        assert_eq!(out.counts.get("imported"), Some(&1), "same bytes → one row");
        assert_eq!(out.counts.get("duplicates"), Some(&1));
        assert_eq!(rows(&v).len(), 1);
    }

    #[test]
    fn photoslibrary_nested_files_are_skipped() {
        // A file inside a `.photoslibrary` bundle must not be indexed (the
        // future apple-photos collector owns it — no double-counting).
        let v = temp_vault("photoslib");
        let root = v.root().join("drop");
        let lib = root.join("My Library.photoslibrary").join("originals");
        fs::create_dir_all(&lib).unwrap();
        // One loose file outside the bundle, one inside it.
        fs::copy(fixture("exif-no-tz.jpg"), root.join("loose.jpg")).unwrap();
        fs::copy(fixture("exif-no-tz.jpg"), lib.join("inside.jpg")).unwrap();

        let out = run(&v, &root);
        // Only the loose file is seen; the bundle's copy is skipped before it
        // can even be hashed (so it isn't even a "duplicate").
        assert_eq!(out.counts.get("imported"), Some(&1), "only the loose file: {}", out.headline);
        assert_eq!(rows(&v).len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn symlink_inside_drop_pointing_outside_is_not_indexed() {
        // The walk must not escape the dropped tree: a symlink inside the
        // dropped folder pointing at a GPS-bearing file OUTSIDE it is skipped
        // (not followed), so that outside file is never indexed.
        use std::os::unix::fs::symlink;
        let v = temp_vault("symlink");
        // An "outside" target the user did NOT drop — its own real directory.
        let outside_dir = v.root().join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        let outside_file = outside_dir.join("secret-location.jpg");
        fs::copy(fixture("exif-no-tz.jpg"), &outside_file).unwrap();

        // The dropped folder: one real loose file + a symlink to the outside
        // file + a symlink to the outside directory.
        let root = v.root().join("drop");
        fs::create_dir_all(&root).unwrap();
        fs::copy(fixture("text-only.png"), root.join("loose.png")).unwrap();
        symlink(&outside_file, root.join("link-to-secret.jpg")).unwrap();
        symlink(&outside_dir, root.join("link-to-outside-dir")).unwrap();

        let out = run(&v, &root);
        // Only the real loose file is indexed; neither symlink is followed.
        assert_eq!(out.counts.get("imported"), Some(&1), "symlinks not followed: {}", out.headline);
        let r = rows(&v);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].filename, "loose.png");
        // The outside file's GPS never leaked into the vault.
        assert!(r.iter().all(|p| p.lat.is_none()), "no outside GPS pulled in via symlink");
    }

    #[test]
    fn extra_is_deterministic_no_thumbnail_clobber_and_no_pointer_tags() {
        // exif-no-tz.jpg has both a primary (IFD0) and a thumbnail (IFD1) IFD
        // with overlapping tags (ResolutionUnit/XResolution/YResolution). The
        // fill must be deterministic across runs (IFD0 value wins, never the
        // HashMap-order-dependent thumbnail copy) and must strip the internal
        // offset/pointer pseudo-tags.
        let v = temp_vault("extra-det");
        run(&v, &fixture("exif-no-tz.jpg"));
        let extra = rows(&v)[0].extra.clone();

        // Pointer/offset pseudo-tags are container structure, not photo
        // metadata — they must be absent from extra.
        for noise in ["ExifOffset", "GPSInfo", "InteropOffset", "ThumbnailOffset", "ThumbnailLength"] {
            assert!(!extra.contains_key(noise), "{noise} (pointer pseudo-tag) must be stripped");
        }
        // A genuine tag is still preserved (full fidelity intact).
        assert!(extra.contains_key("FNumber"), "real tags still preserved in extra");

        // Determinism: re-parsing the same bytes yields the byte-identical extra
        // map every time (so vault output is stable / git diffs are quiet),
        // including the colliding ResolutionUnit/XResolution tags.
        for _ in 0..8 {
            let v2 = temp_vault("extra-det-rerun");
            run(&v2, &fixture("exif-no-tz.jpg"));
            assert_eq!(
                serde_json::to_string(&rows(&v2)[0].extra).unwrap(),
                serde_json::to_string(&extra).unwrap(),
                "extra map must be byte-stable across runs (no thumbnail-IFD clobber)"
            );
        }
    }

    #[test]
    fn corrupt_file_is_skipped_not_fatal() {
        // A file with an image extension but garbage bytes: no EXIF parse, no
        // crash — it still lands a mtime row (it's a real droppable file). The
        // import as a whole succeeds.
        let v = temp_vault("corrupt");
        let root = v.root().join("drop");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("broken.jpg"), b"not a real jpeg").unwrap();
        fs::copy(fixture("exif-no-tz.jpg"), root.join("good.jpg")).unwrap();
        // Must not panic / error; the good file is indexed.
        let out = run(&v, &root);
        assert!(out.counts.get("imported").copied().unwrap_or(0) >= 1, "good file survives a bad neighbor: {}", out.headline);
        // The broken file got a mtime row (real file, parse failed gracefully).
        assert!(rows(&v).iter().any(|r| r.filename == "broken.jpg"));
    }

    #[test]
    fn def_is_default_off_opt_in_with_import_box() {
        // Privacy: GPS trails ⇒ opt-in.
        assert!(!DEF.meta.default_on, "exif-import is default-off (GPS trails)");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none(), "no login — pure file parsing");
        let import = DEF.import_spec().unwrap();
        assert!(import.accepts.contains(&"heic") && import.accepts.contains(&"mov"));
        // The hub surfaces it from the registry, with the GPS acknowledgement
        // in the setup copy.
        let v = temp_vault("def");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "exif-import").unwrap();
        assert!(card.import.is_some(), "import box info present");
        assert!(!card.enabled, "default-off until the user opts in");
        assert!(
            card.setup.iter().any(|s| s.to_lowercase().contains("location")),
            "setup copy acknowledges the location trail"
        );
    }

    #[test]
    fn manifest_indexes_exif_import_as_a_photos_source() {
        let v = temp_vault("manifest");
        run(&v, &fixture("exif-no-tz.jpg"));
        let m = v.rebuild_manifest().unwrap();
        let photos = m.domains.iter().find(|d| d.domain == "photos").expect("photos domain");
        assert!(photos.sources.contains(&"exif-import".to_string()));
        assert!(!photos.spec.is_empty(), "photos is a named contract");
    }

    #[test]
    fn serde_back_compat_old_lines_still_deserialize() {
        // An older sparse line (only the three required keys) and a rich line
        // both deserialize — the additive schema never breaks old data.
        let old = r#"{"ts":"2020-01-02T03:04:05-08:00","source":"exif-import","guid":"sha256:deadbeef"}"#;
        let p: Photo = serde_json::from_str(old).unwrap();
        assert_eq!(p.guid, "sha256:deadbeef");
        assert!(p.lat.is_none() && p.kind.is_empty());
    }
}
