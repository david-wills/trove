//! Apple Photos metadata collector — copy-then-read of
//! `Photos Library.photoslibrary/database/Photos.sqlite` (per-asset metadata:
//! timestamps, GPS, favorites, albums, faces/people) plus
//! `database/search/psi.sqlite` (on-device ML scene/object labels).
//!
//! **Access:** both databases live under `~/Pictures/Photos Library.photoslibrary/`
//! and are readable with the existing Full Disk Access grant (the same
//! per-binary, no-prompt grant as Messages/Safari history).  No new permission
//! is needed.
//!
//! **Copy-then-read (WAL safety):** Photos keeps a WAL lock on both files while
//! it is running.  We copy the DB + its -wal sibling to temp files before
//! opening, exactly as [`crate::browser::import_via_copy`] does for Safari and
//! iMessage.  A torn copy just fails the query; the next sync retries.
//!
//! **Schema-adaptive:** the album-asset join table is named
//! `Z_{GenericAlbum_ENT}ASSETS` and its columns are
//! `Z_{GenericAlbum_ENT}ALBUMS` / `Z_{Asset_ENT}ASSETS`.  The entity numbers
//! are not fixed across macOS major releases.  We probe `Z_PRIMARYKEY` at
//! open time (as osxphotos does) and bail with a clear error if the expected
//! entities are missing — the schema shifted, but no silent garbage is written.
//!
//! **psi.sqlite UUID encoding:** psi stores asset UUIDs as two `i64` values
//! (`uuid_0`, `uuid_1`) that are the little-endian interpretations of the
//! first and second 8-byte halves of the 128-bit UUID string (verified against
//! the actual `Photos.sqlite` on disk — dogsheep-photos issue #16 describes
//! a big-endian variant that does not match the observed data here).
//!
//! **Privacy gate:** GPS geotags are a location trail; faces/people data is
//! sensitive.  Both are emitted only when the user opts in (this source is
//! `default_on: false`).  Trashed assets (`ZTRASHEDSTATE != 0`) are skipped.
//!
//! **Raw layer only:** the photos-metadata contract (Phase 3) is pending;
//! we write `photos/<source>/YYYY-MM.jsonl` using the existing [`crate::photos::Photo`]
//! type, which exactly matches the planned contract shape.  No contract struct,
//! DOMAINS entry, or spec_validation row is added.
//!
//! **Metadata only:** image or video bytes are never read or copied into the
//! vault.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::photos::Photo;
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

/// Seconds between Apple Photos syncs.  A 4-hour cadence is generous given
/// that photos are taken infrequently relative to messages.  The `on_change`
/// gate additionally skips the copy when the DB mtime hasn't moved since the
/// last pass.
pub const APPLE_PHOTOS_SYNC_SECS: u64 = 4 * 3600;

/// Seconds between the Unix epoch and the Core Data / Apple epoch (2001-01-01
/// UTC).  `ZDATECREATED` is seconds since this epoch.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

const SYNC_FILE: &str = ".trove/apple-photos-sync.json";
const SOURCE: &str = "apple-photos";
const PHOTOS_DIR: &str = "photos/apple-photos";

// ---------------------------------------------------------------------------
// DEF

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_apple_photos()?;
    Ok(crate::registry::CollectOutcome::note_if(s.new_photos > 0, || {
        format!("indexed {} photos", s.new_photos)
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(apple_photos_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_apple_photos_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-photos",
        name: "Apple Photos",
        kind: IntegrationKind::LocalSync,
        // GPS geotags + faces/people are sensitive — explicit opt-in.
        default_on: false,
        description: "Indexes metadata from your Photos library every 4 hours: capture \
                      time, GPS, dimensions, albums, on-device ML scene labels, and (opt-in) \
                      faces and people. Image and video files are never copied into the vault.",
        domain: "photos",
        vault_path: "photos/apple-photos/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
            "Heads-up: photo GPS tags form a location trail, and faces/people data is indexed. Only enable if you're comfortable indexing that information.",
        ],
        caveats: "Requires Full Disk Access. Metadata only — image and video bytes are never \
                  copied. Deleted photos lose their metadata on purge, so periodic sync (not \
                  import-once) is the right shape. The schema-probe requirement is the standing \
                  maintenance cost: every macOS major release may shift table prefixes.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::on_change(APPLE_PHOTOS_SYNC_SECS, photos_db_mtime),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Sync state

/// Incremental-sync state, persisted in `.trove/apple-photos-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ApplePhotosSyncState {
    /// RFC3339 local time of the last sync pass.
    pub updated: String,
    /// Highest `ZDATECREATED` (Core Data seconds) imported so far.  The
    /// watermark is advisory — dedupe is by `guid` (UUID) — so a lost state
    /// file never causes duplicates; it just re-scans the whole library.
    pub cursor: f64,
}

/// Result of one sync pass.
#[derive(Debug, Clone, Serialize)]
pub struct ApplePhotosSyncStats {
    /// False when the library is unreadable (no FDA or Photos never launched).
    pub available: bool,
    pub new_photos: u64,
}

// ---------------------------------------------------------------------------
// Locating the library

/// Home dir: `TROVE_HOME` when set and non-empty, else the real `$HOME`.
/// Tests set `TROVE_HOME` to a temp tree so no real library is touched.
fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// The System Photo Library (or the first one found under `~/Pictures/`).
/// Returns `None` when no `.photoslibrary` bundle is readable.
fn photos_library_path() -> Option<PathBuf> {
    let pictures = home_root()?.join("Pictures");
    // Prefer "Photos Library.photoslibrary" (the system default name).
    let default = pictures.join("Photos Library.photoslibrary");
    if default.is_dir() {
        return Some(default);
    }
    // Fall back to any `.photoslibrary` bundle in `~/Pictures/`.
    let entries = fs::read_dir(&pictures).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("photoslibrary") && p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// Path to the main Photos SQLite database.
fn photos_db_path() -> Option<PathBuf> {
    photos_library_path().map(|lib| lib.join("database/Photos.sqlite"))
}

/// Path to the psi (ML labels) database.
fn psi_db_path() -> Option<PathBuf> {
    photos_library_path().map(|lib| lib.join("database/search/psi.sqlite"))
}

/// The latest mtime across `Photos.sqlite` and its WAL siblings.  Used by the
/// `on_change` cadence gate so a sync is skipped entirely when Photos hasn't
/// been opened since the last pass.
pub fn photos_db_mtime() -> Option<SystemTime> {
    let db = photos_db_path()?;
    let base = db.display().to_string();
    [String::new(), "-wal".to_string(), "-shm".to_string()]
        .iter()
        .filter_map(|suffix| {
            fs::metadata(format!("{base}{suffix}")).and_then(|m| m.modified()).ok()
        })
        .max()
}

/// Whether this process can read the Photos database.  False → Full Disk
/// Access hasn't been granted to this binary, or Photos has never been launched.
pub fn apple_photos_permission_ok() -> bool {
    photos_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Core Data date

/// `ZDATECREATED` (Core Data seconds since 2001-01-01 UTC; may be fractional)
/// → local `DateTime`.
fn core_data_to_local(z: f64) -> Option<DateTime<Local>> {
    if !z.is_finite() || z <= 0.0 {
        return None;
    }
    let secs = z.trunc() as i64 + APPLE_EPOCH_OFFSET_S;
    let nanos = (z.fract().abs() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(secs, nanos).map(|t| t.with_timezone(&Local))
}

// ---------------------------------------------------------------------------
// UTI → MIME

/// Map an Apple Uniform Type Identifier to a MIME type string.  Returns `""`
/// when unknown — the `Photo` field is omit-empty.
fn uti_to_mime(uti: &str) -> &'static str {
    match uti {
        "public.jpeg" => "image/jpeg",
        "public.heic" => "image/heic",
        "public.heif" => "image/heif",
        "public.png" => "image/png",
        "public.tiff" => "image/tiff",
        "public.gif" => "image/gif",
        "org.webmproject.webp" => "image/webp",
        "public.avif" => "image/avif",
        "com.apple.quicktime-movie" => "video/quicktime",
        "public.mpeg-4" => "video/mp4",
        "com.apple.m4v-video" => "video/x-m4v",
        "public.3gpp" => "video/3gpp",
        "com.sony.arw-raw-image" => "image/x-sony-arw",
        "com.canon.cr2-raw-image" => "image/x-canon-cr2",
        "com.adobe.raw-image" => "image/x-adobe-dng",
        _ => "",
    }
}

/// `ZKIND` value → Photo `kind` string.
///
/// - 0 = image (still photo or live photo — distinguished by `ZPLAYBACKSTYLE`)
/// - 1 = video
/// `ZPLAYBACKSTYLE`: 3 = live photo; others treated as plain still.
fn asset_kind(z_kind: i64, z_playback_style: i64) -> &'static str {
    match (z_kind, z_playback_style) {
        (0, 3) => "live",
        (0, _) => "photo",
        (1, _) => "video",
        _ => "photo",
    }
}

// ---------------------------------------------------------------------------
// psi UUID conversion
//
// psi.sqlite stores asset UUIDs as two i64 columns (`uuid_0`, `uuid_1`) that
// are the **little-endian** interpretations of the first and second 8-byte
// halves of the UUID byte string (confirmed against the real DB: the big-endian
// interpretation does NOT match Photos.sqlite UUIDs).

/// Decode a two-character ASCII hex pair (e.g. `"4B"`) into a `u8`.
/// Returns `None` on any non-hex character.
#[cfg_attr(not(test), allow(dead_code))]
fn decode_hex_byte(hi: u8, lo: u8) -> Option<u8> {
    let h = match hi {
        b'0'..=b'9' => hi - b'0',
        b'a'..=b'f' => hi - b'a' + 10,
        b'A'..=b'F' => hi - b'A' + 10,
        _ => return None,
    };
    let l = match lo {
        b'0'..=b'9' => lo - b'0',
        b'a'..=b'f' => lo - b'a' + 10,
        b'A'..=b'F' => lo - b'A' + 10,
        _ => return None,
    };
    Some((h << 4) | l)
}

/// Convert a Photos.sqlite UUID string (e.g. `"0002E73C-4690-4B89-8FE5-6B18FB45EDFF"`)
/// to the `(uuid_0, uuid_1)` pair used as the primary key in psi.sqlite.
/// Returns `None` when the string is not a well-formed 32-hex-char UUID.
#[cfg_attr(not(test), allow(dead_code))]
fn uuid_to_psi(uuid: &str) -> Option<(i64, i64)> {
    // Strip dashes and collect hex bytes.
    let hex: Vec<u8> = uuid.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return None;
    }
    let mut raw = [0u8; 16];
    for i in 0..16 {
        raw[i] = decode_hex_byte(hex[i * 2], hex[i * 2 + 1])?;
    }
    let uuid_0 = i64::from_le_bytes(raw[..8].try_into().ok()?);
    let uuid_1 = i64::from_le_bytes(raw[8..].try_into().ok()?);
    Some((uuid_0, uuid_1))
}

/// Format 16 raw bytes as an uppercase UUID string `XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`.
fn bytes_to_uuid(raw: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        raw[0], raw[1], raw[2], raw[3],
        raw[4], raw[5],
        raw[6], raw[7],
        raw[8], raw[9],
        raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
    )
}

// ---------------------------------------------------------------------------
// Schema probe

/// Detect the asset table name: `ZASSET` (macOS Ventura+) vs `ZGENERICASSET`
/// (older macOS).  The brief notes this rename as a standing maintenance cost;
/// probing sqlite_master is the safe approach rather than hard-coding either name.
fn probe_asset_table(conn: &rusqlite::Connection) -> &'static str {
    let ok = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='ZASSET'")
        .and_then(|mut s| s.query([]).and_then(|mut r| r.next().map(|row| row.is_some())))
        .unwrap_or(false);
    if ok { "ZASSET" } else { "ZGENERICASSET" }
}

/// Entity numbers for the album-asset join table, derived from `Z_PRIMARYKEY`.
struct EntityNums {
    /// `Z_ENT` value for `GenericAlbum`.
    album: u32,
    /// `Z_ENT` value for `Asset`.
    asset: u32,
}

/// Probe `Z_PRIMARYKEY` to find the entity numbers that govern the album-asset
/// join table name.  Returns `None` when either entity is absent (schema
/// changed beyond our knowledge — callers skip the album join rather than
/// writing garbage).
fn probe_entity_nums(conn: &rusqlite::Connection) -> Option<EntityNums> {
    let mut stmt = conn
        .prepare("SELECT Z_NAME, Z_ENT FROM Z_PRIMARYKEY WHERE Z_NAME IN ('GenericAlbum', 'Asset')")
        .ok()?;
    let mut rows = stmt.query([]).ok()?;
    let mut album: Option<u32> = None;
    let mut asset: Option<u32> = None;
    while let Some(row) = rows.next().ok()? {
        let name: String = row.get(0).ok()?;
        let ent: u32 = row.get(1).ok()?;
        match name.as_str() {
            "GenericAlbum" => album = Some(ent),
            "Asset" => asset = Some(ent),
            _ => {}
        }
    }
    Some(EntityNums { album: album?, asset: asset? })
}

// ---------------------------------------------------------------------------
// psi label loading

/// Load the ML labels from psi.sqlite: returns a map from Photos.sqlite UUID
/// string to a deduplicated, sorted list of `content_string` labels.
/// Silently returns an empty map if psi is unreadable or malformed.
fn load_psi_labels(psi_db: &Path) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let conn = match rusqlite::Connection::open(psi_db) {
        Ok(c) => c,
        Err(_) => return out,
    };

    // assets: uuid_0 i64, uuid_1 i64 (rowid-based); ga: groupid, assetid;
    // groups: rowid is the join target (ga.groupid → groups.rowid), owning_groupid
    // is a separate many-to-one parent/canonical-group pointer — the two are
    // distinct columns.  Both osxphotos and dogsheep-photos join on rowid.
    let sql = "
        SELECT a.uuid_0, a.uuid_1, g.content_string
        FROM assets a
        JOIN ga ON ga.assetid = a.rowid
        JOIN groups g ON g.rowid = ga.groupid
        WHERE g.content_string IS NOT NULL AND g.content_string != ''
    ";
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(_) => return out,
    };
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return out,
    };
    while let Ok(Some(row)) = rows.next() {
        let uuid_0: i64 = match row.get(0) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let uuid_1: i64 = match row.get(1) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let label_raw: String = match row.get(2) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // psi content_string values can carry trailing NUL bytes; strip them
        // before storing so dedup/compare works correctly (dogsheep-photos
        // does an explicit replace('\x00','') for the same reason).
        let label: String = label_raw.replace('\u{0}', "");
        if label.is_empty() {
            continue;
        }
        // Reverse the little-endian encoding to reconstruct the UUID string.
        let mut raw = [0u8; 16];
        raw[..8].copy_from_slice(&uuid_0.to_le_bytes());
        raw[8..].copy_from_slice(&uuid_1.to_le_bytes());
        let uuid_str = bytes_to_uuid(&raw);
        out.entry(uuid_str).or_default().push(label);
    }
    // Dedup and sort for stable vault output.
    for labels in out.values_mut() {
        labels.sort_unstable();
        labels.dedup();
    }
    out
}

// ---------------------------------------------------------------------------
// Person loading

/// Load person names keyed by ZPERSON Z_PK, from Photos.sqlite.
fn load_persons(conn: &rusqlite::Connection) -> HashMap<i64, (String, String)> {
    let mut out: HashMap<i64, (String, String)> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT Z_PK, COALESCE(ZFULLNAME, ''), COALESCE(ZPERSONUUID, '') FROM ZPERSON",
    ) else {
        return out;
    };
    let Ok(mut rows) = stmt.query([]) else { return out };
    while let Ok(Some(row)) = rows.next() {
        let pk: i64 = match row.get(0) { Ok(v) => v, Err(_) => continue };
        let name: String = match row.get(1) { Ok(v) => v, Err(_) => continue };
        let puuid: String = match row.get(2) { Ok(v) => v, Err(_) => continue };
        out.insert(pk, (name, puuid));
    }
    out
}

/// Load detected-face rows: asset Z_PK → list of ZPERSON Z_PKs (with a name).
fn load_face_persons(
    conn: &rusqlite::Connection,
    persons: &HashMap<i64, (String, String)>,
) -> HashMap<i64, Vec<i64>> {
    let mut out: HashMap<i64, Vec<i64>> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT ZASSETFORFACE, ZPERSONFORFACE FROM ZDETECTEDFACE
         WHERE ZASSETFORFACE IS NOT NULL AND ZPERSONFORFACE IS NOT NULL",
    ) else {
        return out;
    };
    let Ok(mut rows) = stmt.query([]) else { return out };
    while let Ok(Some(row)) = rows.next() {
        let asset_pk: i64 = match row.get(0) { Ok(v) => v, Err(_) => continue };
        let person_pk: i64 = match row.get(1) { Ok(v) => v, Err(_) => continue };
        if persons.contains_key(&person_pk) {
            out.entry(asset_pk).or_default().push(person_pk);
        }
    }
    // Deduplicate: the same person may appear multiple times per asset.
    for pks in out.values_mut() {
        pks.sort_unstable();
        pks.dedup();
    }
    out
}

// ---------------------------------------------------------------------------
// Album loading

/// Load album names: asset Z_PK → list of album ZTITLE strings.
fn load_albums(conn: &rusqlite::Connection, ents: &EntityNums) -> HashMap<i64, Vec<String>> {
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    // Table and columns are named with the entity numbers probed at open time.
    let join_table = format!("Z_{}ASSETS", ents.album);
    let col_album = format!("Z_{}ALBUMS", ents.album);
    let col_asset = format!("Z_{}ASSETS", ents.asset);
    let sql = format!(
        "SELECT j.{col_asset}, a.ZTITLE
         FROM {join_table} j
         JOIN ZGENERICALBUM a ON a.Z_PK = j.{col_album}
         WHERE a.ZTITLE IS NOT NULL AND a.ZTITLE != ''
         ORDER BY j.{col_asset}, a.ZTITLE"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else { return out };
    let Ok(mut rows) = stmt.query([]) else { return out };
    while let Ok(Some(row)) = rows.next() {
        let asset_pk: i64 = match row.get(0) { Ok(v) => v, Err(_) => continue };
        let title: String = match row.get(1) { Ok(v) => v, Err(_) => continue };
        out.entry(asset_pk).or_default().push(title);
    }
    // Dedup (the join can return duplicates when the album ENT isn't exactly
    // right, but also just for cleanliness).
    for titles in out.values_mut() {
        titles.sort_unstable();
        titles.dedup();
    }
    out
}

// ---------------------------------------------------------------------------
// Main import

/// Read assets from a copy of Photos.sqlite and merge psi labels.  Returns
/// `(rows_imported, new_cursor)`.  Split from the locate/copy path so tests
/// can feed a synthetic DB.
pub(crate) fn import_photos_db(
    vault: &Vault,
    photos_db: &Path,
    psi_db: Option<&Path>,
    cursor: f64,
) -> Result<(u64, f64)> {
    let conn = rusqlite::Connection::open(photos_db)
        .with_context(|| format!("opening Photos.sqlite copy {}", photos_db.display()))?;

    // Probe entity numbers for the album join table.
    let ents = probe_entity_nums(&conn);

    // Detect the asset table name: ZASSET (Ventura+) or ZGENERICASSET (older).
    let asset_table = probe_asset_table(&conn);

    // Load auxiliary data (persons, albums) before the main asset scan.
    let persons = load_persons(&conn);
    let face_persons = load_face_persons(&conn, &persons);
    let albums = ents.as_ref().map(|e| load_albums(&conn, e)).unwrap_or_default();

    // Load psi ML labels (best-effort: silently skip if psi is unavailable).
    let psi_labels: HashMap<String, Vec<String>> = psi_db
        .map(|p| load_psi_labels(p))
        .unwrap_or_default();

    // Already-stored UUIDs for dedupe.
    let known = vault.apple_photos_guids()?;

    // Main asset scan — ordered by ZDATECREATED so the cursor advances
    // monotonically.
    let asset_sql = format!(
        "SELECT
            a.Z_PK,
            COALESCE(a.ZUUID, ''),
            COALESCE(a.ZDATECREATED, 0.0),
            COALESCE(a.ZLATITUDE, 0.0),
            COALESCE(a.ZLONGITUDE, 0.0),
            COALESCE(a.ZWIDTH, 0),
            COALESCE(a.ZHEIGHT, 0),
            COALESCE(a.ZFAVORITE, 0),
            COALESCE(a.ZHIDDEN, 0),
            COALESCE(a.ZDURATION, 0.0),
            COALESCE(a.ZKIND, 0),
            COALESCE(a.ZPLAYBACKSTYLE, 0),
            COALESCE(a.ZFILENAME, ''),
            COALESCE(a.ZUNIFORMTYPEIDENTIFIER, '')
         FROM {asset_table} a
         WHERE a.ZTRASHEDSTATE = 0
           AND a.ZDATECREATED IS NOT NULL
         ORDER BY a.ZDATECREATED"
    );
    let mut stmt = conn.prepare(&asset_sql)?;
    let mut rows = stmt.query([])?;
    let mut out: Vec<Photo> = Vec::new();
    let mut max = cursor;

    while let Some(row) = rows.next()? {
        let z_pk: i64 = row.get(0)?;
        let uuid: String = row.get(1)?;
        let z_date: f64 = row.get(2)?;
        let lat: f64 = row.get(3)?;
        let lon: f64 = row.get(4)?;
        let width: i64 = row.get(5)?;
        let height: i64 = row.get(6)?;
        let favorite: i64 = row.get(7)?;
        let hidden: i64 = row.get(8)?;
        let duration: f64 = row.get(9)?;
        let z_kind: i64 = row.get(10)?;
        let z_playback: i64 = row.get(11)?;
        let filename: String = row.get(12)?;
        let uti: String = row.get(13)?;

        if uuid.is_empty() {
            continue;
        }
        if known.contains(&uuid) {
            max = max.max(z_date);
            continue;
        }

        let Some(local) = core_data_to_local(z_date) else {
            continue;
        };

        let mut photo = Photo::new(SOURCE, &uuid, local.to_rfc3339());
        photo.kind = asset_kind(z_kind, z_playback).to_string();
        photo.filename = filename;
        photo.mime = uti_to_mime(&uti).to_string();

        if width > 0 {
            photo.width = Some(width as u32);
        }
        if height > 0 {
            photo.height = Some(height as u32);
        }
        // GPS: only emit when non-zero (the DB stores 0.0 when no geotag).
        if lat.abs() > 1e-10 || lon.abs() > 1e-10 {
            photo.lat = Some(lat);
            photo.lon = Some(lon);
        }
        if favorite != 0 {
            photo.favorite = Some(true);
        }
        if duration > 0.0 && duration.is_finite() {
            photo.duration_secs = Some(duration);
        }

        // Albums.
        if let Some(alb) = albums.get(&z_pk) {
            photo.albums = alb.clone();
        }

        // People (opt-in — written when present; face data is what it is at
        // write time; the user can clear the vault to remove).
        if let Some(pks) = face_persons.get(&z_pk) {
            for pk in pks {
                if let Some((name, puuid)) = persons.get(pk) {
                    photo.people.push(puuid.clone());
                    photo.people_name.push(name.clone());
                }
            }
        }

        // psi ML labels → extra["labels"].
        if let Some(labels) = psi_labels.get(&uuid) {
            if !labels.is_empty() {
                let arr: Vec<Value> = labels.iter().map(|s| Value::String(s.clone())).collect();
                photo.extra.insert("labels".into(), Value::Array(arr));
            }
        }

        // Source-specific overflow.
        let mut extra_local: Map<String, Value> = Map::new();
        if hidden != 0 {
            extra_local.insert("hidden".into(), Value::Bool(true));
        }
        extra_local.insert("z_pk".into(), Value::from(z_pk));
        // Merge into photo.extra (labels already set above; z_pk is metadata).
        for (k, v) in extra_local {
            photo.extra.insert(k, v);
        }

        max = max.max(z_date);
        out.push(photo);
    }

    if !out.is_empty() {
        vault.append_apple_photos(&out)?;
    }
    Ok((out.len() as u64, max))
}

// ---------------------------------------------------------------------------
// Vault impl

impl Vault {
    /// One incremental sync pass over the Apple Photos library.  Silently
    /// a no-op (`available:false`) while the database is unreadable — the hub
    /// surfaces the permission state; no log noise on every pass.
    pub fn collect_apple_photos(&self) -> Result<ApplePhotosSyncStats> {
        if !apple_photos_permission_ok() {
            return Ok(ApplePhotosSyncStats { available: false, new_photos: 0 });
        }
        let photos_db = photos_db_path().expect("permission_ok implies path");
        let psi = psi_db_path();
        let mut state = self.read_apple_photos_sync().unwrap_or_default();

        let stem = format!("trove-applephotos-{}", std::process::id());
        let n = import_via_copy(&photos_db, &stem, |tmp| {
            // Also copy psi.sqlite alongside if it exists.
            let psi_tmp = psi.as_ref().and_then(|p| {
                let t = std::env::temp_dir().join(format!("{stem}-psi.db"));
                let t_wal = std::env::temp_dir().join(format!("{stem}-psi.db-wal"));
                let _ = fs::remove_file(&t);
                let _ = fs::remove_file(&t_wal);
                if fs::copy(p, &t).is_ok() {
                    let wal = PathBuf::from(format!("{}-wal", p.display()));
                    if wal.exists() {
                        let _ = fs::copy(&wal, &t_wal);
                    }
                    Some(t)
                } else {
                    None
                }
            });
            let result = import_photos_db(self, tmp, psi_tmp.as_deref(), state.cursor);
            // Clean up psi tmp files.
            if let Some(ref t) = psi_tmp {
                let _ = fs::remove_file(t);
                let _ = fs::remove_file(
                    t.with_file_name(format!("{}-wal", t.file_name().unwrap_or_default().to_string_lossy())),
                );
            }
            result
        })?;

        state.cursor = n.1;
        state.updated = Local::now().to_rfc3339();
        self.write_apple_photos_sync(&state)?;
        Ok(ApplePhotosSyncStats { available: true, new_photos: n.0 })
    }

    /// Append photo metadata rows to `photos/apple-photos/YYYY-MM.jsonl`,
    /// partitioned by the local month of `ts`.
    pub fn append_apple_photos(&self, photos: &[Photo]) -> Result<()> {
        self.stream(PHOTOS_DIR, Partition::Month).append(photos, |p| &p.ts)
    }

    /// Every `guid` (UUID) already stored in this source's stream — the dedupe
    /// set.  A re-run never duplicates a row whose UUID was already imported.
    pub(crate) fn apple_photos_guids(&self) -> Result<HashSet<String>> {
        let stream = self.stream(PHOTOS_DIR, Partition::Month);
        let mut set = HashSet::new();
        for key in stream.partitions()? {
            for p in stream.read::<Photo>(&key)? {
                if !p.guid.is_empty() {
                    set.insert(p.guid);
                }
            }
        }
        Ok(set)
    }

    /// Read the persisted sync state, if any.
    pub fn read_apple_photos_sync(&self) -> Option<ApplePhotosSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_apple_photos_sync(&self, state: &ApplePhotosSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-applephotos-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Core Data seconds for a fixed local datetime.
    fn z_date(y: i32, m: u32, d: u32, h: u32) -> f64 {
        let t = Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        (t.timestamp() - APPLE_EPOCH_OFFSET_S) as f64
    }

    /// Create a minimal Photos.sqlite in `dir` with the core tables we read,
    /// using fixed entity numbers (album=32, asset=3 — matching the real DB).
    fn minimal_photos_db(path: &Path) -> rusqlite::Connection {
        let _ = fs::remove_file(path);
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE Z_PRIMARYKEY (Z_NAME TEXT, Z_ENT INTEGER);
             INSERT INTO Z_PRIMARYKEY VALUES ('Asset', 3);
             INSERT INTO Z_PRIMARYKEY VALUES ('GenericAlbum', 32);
             INSERT INTO Z_PRIMARYKEY VALUES ('Person', 59);

             CREATE TABLE ZASSET (
                 Z_PK INTEGER PRIMARY KEY,
                 ZUUID TEXT,
                 ZDATECREATED REAL,
                 ZLATITUDE REAL,
                 ZLONGITUDE REAL,
                 ZWIDTH INTEGER,
                 ZHEIGHT INTEGER,
                 ZFAVORITE INTEGER DEFAULT 0,
                 ZHIDDEN INTEGER DEFAULT 0,
                 ZDURATION REAL DEFAULT 0,
                 ZKIND INTEGER DEFAULT 0,
                 ZPLAYBACKSTYLE INTEGER DEFAULT 1,
                 ZFILENAME TEXT,
                 ZUNIFORMTYPEIDENTIFIER TEXT,
                 ZTRASHEDSTATE INTEGER DEFAULT 0
             );

             CREATE TABLE ZGENERICALBUM (
                 Z_PK INTEGER PRIMARY KEY,
                 ZTITLE TEXT,
                 ZUUID TEXT
             );

             -- Join table with entity-number-based name (album=32, asset=3)
             CREATE TABLE Z_32ASSETS (
                 Z_32ALBUMS INTEGER,
                 Z_3ASSETS INTEGER
             );

             CREATE TABLE ZPERSON (
                 Z_PK INTEGER PRIMARY KEY,
                 ZFULLNAME TEXT,
                 ZPERSONUUID TEXT
             );

             CREATE TABLE ZDETECTEDFACE (
                 Z_PK INTEGER PRIMARY KEY,
                 ZASSETFORFACE INTEGER,
                 ZPERSONFORFACE INTEGER
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn core_data_epoch_roundtrip() {
        // 2026-06-10T00:00:00 UTC in Core Data seconds.
        let unix_ts = 1_781_049_600i64; // 2026-06-10T00:00:00Z
        let z = (unix_ts - APPLE_EPOCH_OFFSET_S) as f64;
        let local = core_data_to_local(z).unwrap();
        assert_eq!(local.timestamp(), unix_ts);
        // NaN and zero → None.
        assert!(core_data_to_local(f64::NAN).is_none());
        assert!(core_data_to_local(0.0).is_none());
    }

    #[test]
    fn uti_to_mime_mapping() {
        assert_eq!(uti_to_mime("public.jpeg"), "image/jpeg");
        assert_eq!(uti_to_mime("public.heic"), "image/heic");
        assert_eq!(uti_to_mime("com.apple.quicktime-movie"), "video/quicktime");
        assert_eq!(uti_to_mime("unknown.type"), "");
    }

    #[test]
    fn asset_kind_mapping() {
        assert_eq!(asset_kind(0, 1), "photo");
        assert_eq!(asset_kind(0, 3), "live");
        assert_eq!(asset_kind(1, 4), "video");
        assert_eq!(asset_kind(0, 99), "photo"); // unknown playback style
    }

    #[test]
    fn uuid_to_psi_roundtrip() {
        // Verified against the real psi.sqlite: the UUID
        // "0002E73C-4690-4B89-8FE5-6B18FB45EDFF" maps to uuid_0=-8553584435916242432,
        // uuid_1=-5271079808670321 (little-endian encoding).
        let uuid = "0002E73C-4690-4B89-8FE5-6B18FB45EDFF";
        let (u0, u1) = uuid_to_psi(uuid).unwrap();
        assert_eq!(u0, -8553584435916242432i64, "uuid_0 little-endian");
        assert_eq!(u1, -5271079808670321i64, "uuid_1 little-endian");

        // Round-trip: reconstruct UUID from the int64 pair.
        let mut raw = [0u8; 16];
        raw[..8].copy_from_slice(&u0.to_le_bytes());
        raw[8..].copy_from_slice(&u1.to_le_bytes());
        let reconstructed = bytes_to_uuid(&raw);
        assert_eq!(reconstructed, uuid, "round-trip UUID reconstruction");
    }

    #[test]
    fn uuid_to_psi_rejects_malformed() {
        assert!(uuid_to_psi("").is_none());
        assert!(uuid_to_psi("not-a-uuid").is_none());
        // Too short (31 hex chars after stripping dashes).
        assert!(uuid_to_psi("0002E73C-4690-4B89-8FE5-6B18FB45EDF").is_none());
    }

    #[test]
    fn imports_basic_photo_row() {
        let v = temp_vault("basic");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-basic-{}-test.db",
            std::process::id()
        ));
        let conn = minimal_photos_db(&db_path);
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZLATITUDE, ZLONGITUDE,
             ZWIDTH, ZHEIGHT, ZFAVORITE, ZKIND, ZPLAYBACKSTYLE, ZFILENAME,
             ZUNIFORMTYPEIDENTIFIER, ZTRASHEDSTATE)
             VALUES (1, 'AAAA0001-0000-0000-0000-000000000001',
             ?1, 34.03, -118.45, 4032, 3024, 1, 0, 1,
             'IMG_0001.HEIC', 'public.heic', 0)",
            [z_date(2026, 6, 10, 15)],
        )
        .unwrap();
        drop(conn);

        let (n, max) = import_photos_db(&v, &db_path, None, 0.0).unwrap();
        assert_eq!(n, 1, "one photo imported");
        assert!(max > 0.0);

        let photos =
            v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2026-06").unwrap();
        assert_eq!(photos.len(), 1);
        let p = &photos[0];
        assert_eq!(p.source, "apple-photos");
        assert_eq!(p.guid, "AAAA0001-0000-0000-0000-000000000001");
        assert_eq!(p.kind, "photo");
        assert_eq!(p.mime, "image/heic");
        assert_eq!(p.filename, "IMG_0001.HEIC");
        assert_eq!(p.width, Some(4032));
        assert_eq!(p.height, Some(3024));
        assert!((p.lat.unwrap() - 34.03).abs() < 1e-6, "lat matches");
        assert!((p.lon.unwrap() - -118.45).abs() < 1e-6, "lon matches");
        assert_eq!(p.favorite, Some(true));
        assert!(p.ts.starts_with("2026-06-10"), "ts in June 2026: {}", p.ts);

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn live_photo_and_video_kinds() {
        let v = temp_vault("kinds");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-kinds-{}-test.db",
            std::process::id()
        ));
        let conn = minimal_photos_db(&db_path);
        // Live photo: ZKIND=0, ZPLAYBACKSTYLE=3
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZKIND, ZPLAYBACKSTYLE,
             ZUNIFORMTYPEIDENTIFIER, ZTRASHEDSTATE)
             VALUES (1, 'BBBB0001-0000-0000-0000-000000000001', ?1, 0, 3, 'public.heic', 0)",
            [z_date(2026, 6, 10, 10)],
        )
        .unwrap();
        // Video: ZKIND=1, ZPLAYBACKSTYLE=4
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZKIND, ZPLAYBACKSTYLE,
             ZDURATION, ZUNIFORMTYPEIDENTIFIER, ZTRASHEDSTATE)
             VALUES (2, 'BBBB0002-0000-0000-0000-000000000002', ?1, 1, 4, 35.5,
             'com.apple.quicktime-movie', 0)",
            [z_date(2026, 6, 10, 11)],
        )
        .unwrap();
        drop(conn);

        import_photos_db(&v, &db_path, None, 0.0).unwrap();
        let photos = v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2026-06").unwrap();
        assert_eq!(photos.len(), 2);
        let live = photos.iter().find(|p| p.guid.contains("0001")).unwrap();
        let video = photos.iter().find(|p| p.guid.contains("0002")).unwrap();
        assert_eq!(live.kind, "live");
        assert_eq!(video.kind, "video");
        assert_eq!(video.duration_secs, Some(35.5));
        assert_eq!(video.mime, "video/quicktime");

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn trashed_assets_skipped() {
        let v = temp_vault("trash");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-trash-{}-test.db",
            std::process::id()
        ));
        let conn = minimal_photos_db(&db_path);
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZTRASHEDSTATE)
             VALUES (1, 'CCCC0001-0000-0000-0000-000000000001', ?1, 1)",
            [z_date(2026, 6, 10, 10)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZTRASHEDSTATE)
             VALUES (2, 'CCCC0002-0000-0000-0000-000000000002', ?1, 0)",
            [z_date(2026, 6, 10, 11)],
        )
        .unwrap();
        drop(conn);

        let (n, _) = import_photos_db(&v, &db_path, None, 0.0).unwrap();
        assert_eq!(n, 1, "trashed asset is skipped");
        let photos = v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2026-06").unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].guid, "CCCC0002-0000-0000-0000-000000000002");

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn no_gps_photo_omits_lat_lon() {
        let v = temp_vault("nogps");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-nogps-{}-test.db",
            std::process::id()
        ));
        let conn = minimal_photos_db(&db_path);
        // ZLATITUDE/ZLONGITUDE stored as 0.0 in the DB when no geotag.
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZLATITUDE, ZLONGITUDE, ZTRASHEDSTATE)
             VALUES (1, 'DDDD0001-0000-0000-0000-000000000001', ?1, 0.0, 0.0, 0)",
            [z_date(2026, 6, 10, 10)],
        )
        .unwrap();
        drop(conn);

        import_photos_db(&v, &db_path, None, 0.0).unwrap();
        let photos = v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2026-06").unwrap();
        assert_eq!(photos.len(), 1);
        let p = &photos[0];
        assert!(p.lat.is_none() && p.lon.is_none(), "no-GPS asset omits lat/lon");
        // Serialized form also omits the keys.
        let json = serde_json::to_value(p).unwrap();
        assert!(json.get("lat").is_none() && json.get("lon").is_none());

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn albums_and_persons_joined() {
        let v = temp_vault("albums");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-albums-{}-test.db",
            std::process::id()
        ));
        let conn = minimal_photos_db(&db_path);
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZTRASHEDSTATE)
             VALUES (1, 'EEEE0001-0000-0000-0000-000000000001', ?1, 0)",
            [z_date(2026, 6, 10, 12)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZGENERICALBUM (Z_PK, ZTITLE, ZUUID) VALUES (10, 'Summer 2026', 'ALB-1')",
            [],
        )
        .unwrap();
        // Join table: Z_32ASSETS with Z_32ALBUMS (album) and Z_3ASSETS (asset).
        conn.execute(
            "INSERT INTO Z_32ASSETS (Z_32ALBUMS, Z_3ASSETS) VALUES (10, 1)",
            [],
        )
        .unwrap();
        // Person + face detection.
        conn.execute(
            "INSERT INTO ZPERSON (Z_PK, ZFULLNAME, ZPERSONUUID) VALUES (20, 'Alice', 'PERSON-UUID-1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZDETECTEDFACE (Z_PK, ZASSETFORFACE, ZPERSONFORFACE) VALUES (1, 1, 20)",
            [],
        )
        .unwrap();
        drop(conn);

        import_photos_db(&v, &db_path, None, 0.0).unwrap();
        let photos = v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2026-06").unwrap();
        assert_eq!(photos.len(), 1);
        let p = &photos[0];
        assert_eq!(p.albums, vec!["Summer 2026".to_string()], "album joined");
        assert_eq!(p.people, vec!["PERSON-UUID-1".to_string()], "person uuid");
        assert_eq!(p.people_name, vec!["Alice".to_string()], "person name");

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn dedupe_on_rerun() {
        let v = temp_vault("dedup");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-dedup-{}-test.db",
            std::process::id()
        ));
        let conn = minimal_photos_db(&db_path);
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZTRASHEDSTATE)
             VALUES (1, 'FFFF0001-0000-0000-0000-000000000001', ?1, 0)",
            [z_date(2026, 6, 10, 8)],
        )
        .unwrap();
        drop(conn);

        let (n1, max1) = import_photos_db(&v, &db_path, None, 0.0).unwrap();
        assert_eq!(n1, 1);
        let (n2, max2) = import_photos_db(&v, &db_path, None, max1).unwrap();
        assert_eq!(n2, 0, "already-stored UUID deduped on re-run");
        assert!((max2 - max1).abs() < 1.0, "cursor stable");
        let photos = v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2026-06").unwrap();
        assert_eq!(photos.len(), 1, "no duplicate rows on disk");

        let _ = fs::remove_file(&db_path);
    }

    #[test]
    fn psi_labels_loaded_and_written_to_extra() {
        // Build a synthetic psi.sqlite and verify labels end up in extra["labels"].
        let v = temp_vault("psi");
        let db_path = std::env::temp_dir().join(format!(
            "trove-applephotos-psi-{}-test.db",
            std::process::id()
        ));
        let psi_path = std::env::temp_dir().join(format!(
            "trove-applephotos-psi-{}-psi.db",
            std::process::id()
        ));

        // Photos.sqlite with one asset.
        let uuid = "0002E73C-4690-4B89-8FE5-6B18FB45EDFF";
        let conn = minimal_photos_db(&db_path);
        conn.execute(
            "INSERT INTO ZASSET (Z_PK, ZUUID, ZDATECREATED, ZTRASHEDSTATE)
             VALUES (1, ?, ?2, 0)",
            rusqlite::params![uuid, z_date(2021, 8, 28, 21)],
        )
        .unwrap();
        drop(conn);

        // psi.sqlite: uuid_0/uuid_1 computed from the little-endian encoding.
        let (u0, u1) = uuid_to_psi(uuid).unwrap();
        let _ = fs::remove_file(&psi_path);
        let pconn = rusqlite::Connection::open(&psi_path).unwrap();
        // Realistic psi.sqlite schema: groups.rowid is the join target
        // (ga.groupid references groups.rowid), while owning_groupid is a
        // separate parent/canonical-group pointer.  We use explicit rowids (10,
        // 20) distinct from owning_groupid (99) so that a regression to the wrong
        // join key `g.owning_groupid = ga.groupid` would return zero rows.
        pconn
            .execute_batch(
                "CREATE TABLE assets (uuid_0 INT, uuid_1 INT, creationDate INT);
                 CREATE TABLE groups (owning_groupid INT, content_string TEXT, normalized_string TEXT);
                 CREATE TABLE ga (groupid INT, assetid INT);",
            )
            .unwrap();
        pconn
            .execute(
                "INSERT INTO assets (uuid_0, uuid_1, creationDate) VALUES (?1, ?2, 1234567)",
                rusqlite::params![u0, u1],
            )
            .unwrap();
        // assets.rowid == 1 (the inserted row above).
        // groups: explicit rowid 10/20, owning_groupid=99 (a different canonical group).
        // ga: groupid=10/20 point at groups.rowid (not owning_groupid=99).
        pconn.execute(
            "INSERT INTO groups (rowid, owning_groupid, content_string, normalized_string) VALUES (10, 99, 'Beach', 'beach')",
            [],
        ).unwrap();
        pconn.execute(
            "INSERT INTO groups (rowid, owning_groupid, content_string, normalized_string) VALUES (20, 99, 'Sunset', 'sunset')",
            [],
        ).unwrap();
        pconn.execute("INSERT INTO ga (groupid, assetid) VALUES (10, 1)", []).unwrap();
        pconn.execute("INSERT INTO ga (groupid, assetid) VALUES (20, 1)", []).unwrap();
        drop(pconn);

        import_photos_db(&v, &db_path, Some(&psi_path), 0.0).unwrap();
        let photos = v.stream(PHOTOS_DIR, Partition::Month).read::<Photo>("2021-08").unwrap();
        assert_eq!(photos.len(), 1);
        let labels = photos[0].extra.get("labels").expect("labels in extra");
        let arr = labels.as_array().unwrap();
        let strings: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
        assert!(strings.contains(&"Beach"), "Beach label present");
        assert!(strings.contains(&"Sunset"), "Sunset label present");

        let _ = fs::remove_file(&db_path);
        let _ = fs::remove_file(&psi_path);
    }

    #[test]
    fn serde_back_compat_sparse_line() {
        // An older sparse line (only the three required keys) still deserializes.
        let old = r#"{"ts":"2021-08-28T20:59:27-07:00","source":"apple-photos","guid":"0002E73C-4690-4B89-8FE5-6B18FB45EDFF"}"#;
        let p: Photo = serde_json::from_str(old).unwrap();
        assert_eq!(p.guid, "0002E73C-4690-4B89-8FE5-6B18FB45EDFF");
        assert!(p.lat.is_none() && p.albums.is_empty() && p.kind.is_empty());
    }

    #[test]
    fn def_is_default_off_periodic_no_connection() {
        assert!(!DEF.meta.default_on, "GPS+faces → default-off");
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert!(DEF.connection.is_none(), "no external login needed");
        assert!(DEF.meta.id == "apple-photos");
        assert!(DEF.meta.domain == "photos");
        // Privacy: setup copy must acknowledge the location trail.
        assert!(
            DEF.meta.setup.iter().any(|s| s.to_lowercase().contains("location")
                || s.to_lowercase().contains("gps")
                || s.to_lowercase().contains("location trail")),
            "setup copy acknowledges the location/GPS trail"
        );
    }

    #[test]
    fn no_photos_library_returns_not_available() {
        // With TROVE_HOME set to a temp dir that has no Photos library, the
        // permission check returns false and collect is a silent no-op.
        let dir = std::env::temp_dir().join(format!(
            "trove-applephotos-nolibrary-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // TROVE_HOME redirects photos_library_path to the temp dir.
        std::env::set_var("TROVE_HOME", dir.to_str().unwrap());
        let ok = apple_photos_permission_ok();
        std::env::remove_var("TROVE_HOME");
        assert!(!ok, "no library in temp dir → not available");
        let _ = fs::remove_dir_all(&dir);
    }
}
