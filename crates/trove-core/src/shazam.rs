//! Shazam — music-discovery tag history from the local ShazamDataModel SQLite
//! database (synced via iCloud from iPhone to Mac within minutes).
//!
//! **Behavior:** Periodic collector (every 15 minutes). Reads the local
//! `ShazamDataModel.sqlite` via copy-then-read (WAL-safe), watermarks on
//! `ROWID` (same rationale as the iMessage collector: iCloud backfill inserts
//! old-timestamped rows with fresh ROWIDs, so a timestamp cursor would miss
//! them). Writes raw-only to `media/shazam/YYYY-MM.jsonl` (one row per tag,
//! month-partitioned by local tag time). No contract layer — Shazams are
//! discovery events, not play events; they do not join the media-plays
//! contract.
//!
//! **Schema note:** The Core Data table `ZSHTAGRESULTMO` uses `ZDATE` for the
//! tag timestamp across all documented community schemas (Vaughan Harper verbatim
//! SQL "seconds since 2001-01-01 UTC", sn3p/shazam-tags `ORDER BY ZDATE`,
//! sophiegblog gist `ZDATE`+11323-day offset, TechTraumas `zdate`). Three
//! column-name variants have been observed:
//!
//! - **TrackSubtitle** (community-documented, 2017–2021): `ZTRACKNAME` (song)
//!   + `ZSUBTITLE`; artist name in related `ZSHARTISTMO.ZNAME` joined via
//!   `ZSHARTISTMO.ZTAGRESULT = ZSHTAGRESULTMO.Z_PK`.
//! - **TitleArtist** (post-2021 Mac app): `ZTITLE` + `ZARTIST` directly on
//!   `ZSHTAGRESULTMO`; Needs-sample confirmation for ZSHAZAMID column.
//! - **Minimal**: neither recognized variant — advance cursor without metadata.
//!
//! We use `PRAGMA table_info` at open time to detect which variant is present
//! and build the query accordingly — the same schema-adaptive pattern the
//! iMessage and Podcasts collectors use.
//!
//! **Location data:** Primary community sources confirm `ZLATITUDE`/`ZLONGITUDE`
//! columns on `ZSHTAGRESULTMO` (NULL-safe; will be NULL when location was not
//! captured at tag time).
//!
//! **Paths:**
//! - Primary:  `~/Library/Containers/com.shazam.mac.Shazam/Data/Documents/ShazamDataModel.sqlite`
//! - Fallback: `~/Library/Group Containers/*.group.com.shazam/...`
//!
//! Requires Full Disk Access. No network required. Standalone-clean.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

/// Seconds between the Apple/Core Data epoch (2001-01-01 UTC) and Unix epoch.
const APPLE_EPOCH_OFFSET_S: i64 = 978_307_200;

/// 15-minute cadence (Shazams sync from iPhone in minutes).
const SHAZAM_SYNC_SECS: u64 = 900;

/// Persisted watermark (vault-relative, rebuildable from logs).
const SYNC_FILE: &str = ".trove/shazam-sync.json";

/// Raw vault stream directory.
const STREAM_DIR: &str = "media/shazam";

// ---------------------------------------------------------------------------
// DEF hooks

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    let s = vault.collect_shazams()?;
    Ok(CollectOutcome::note_if(s.new_tags > 0, || {
        format!("imported {} Shazam tag{}", s.new_tags, if s.new_tags == 1 { "" } else { "s" })
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(shazam_db_path().is_some_and(|p| fs::File::open(p).is_ok())),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_shazam_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "shazam",
        name: "Shazam",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your Shazam history from the local Shazam app database. \
                      Songs identified on your iPhone sync to Mac via iCloud, so the \
                      full cross-device history is captured without a network call.",
        domain: "media",
        vault_path: "media/shazam/",
        toggleable: true,
        setup: &[
            "Install the Shazam app from the Mac App Store and sign in with your Apple ID.",
            "Enable iCloud sync in Shazam Settings so iPhone Shazams appear on Mac.",
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
        ],
        caveats: "Shazam records when you identified a track, not when you listened to it. \
                  Full Disk Access is required to read the database. \
                  If the Shazam app is not installed, this integration stays inactive.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(SHAZAM_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// DB path resolution

fn shazam_primary_db() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Containers/com.shazam.mac.Shazam/Data/Documents/ShazamDataModel.sqlite")
}

fn shazam_group_db() -> Option<PathBuf> {
    let groups = dirs::home_dir()?.join("Library/Group Containers");
    let entries = fs::read_dir(&groups).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.ends_with(".group.com.shazam")
            || name_str.contains(".group.com.shazam.")
        {
            let candidate =
                entry.path().join("Data/Documents/ShazamDataModel.sqlite");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Primary container path first, group-container fallback. `None` when
/// neither exists (Shazam not installed or never launched).
pub fn shazam_db_path() -> Option<PathBuf> {
    let primary = shazam_primary_db();
    if primary.exists() {
        return Some(primary);
    }
    shazam_group_db()
}

// ---------------------------------------------------------------------------
// Core Data epoch conversion

fn apple_secs_to_rfc3339(secs: f64) -> Option<String> {
    if secs <= 0.0 {
        return None;
    }
    chrono::DateTime::from_timestamp(secs as i64 + APPLE_EPOCH_OFFSET_S, 0)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Schema detection

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaVariant {
    /// Newer layout (post-2021 Mac app, Needs-sample): `ZTITLE` + `ZARTIST`
    /// directly on ZSHTAGRESULTMO.
    TitleArtist,
    /// Older layout (community-documented 2017–2021): `ZTRACKNAME` +
    /// `ZSUBTITLE` on ZSHTAGRESULTMO, artist name via JOIN to
    /// `ZSHARTISTMO.ZNAME` on `ZSHARTISTMO.ZTAGRESULT = ZSHTAGRESULTMO.Z_PK`.
    TrackSubtitle,
    /// Unexpected layout: advance cursor on ZDATE only, no metadata.
    Minimal,
}

fn detect_schema(conn: &rusqlite::Connection) -> SchemaVariant {
    let cols = table_columns(conn, "ZSHTAGRESULTMO");
    if cols.contains("ZTITLE") && cols.contains("ZARTIST") {
        SchemaVariant::TitleArtist
    } else if cols.contains("ZTRACKNAME") {
        SchemaVariant::TrackSubtitle
    } else {
        SchemaVariant::Minimal
    }
}

/// Return a set of column names for `table` (uppercase), or empty on error.
fn table_columns(conn: &rusqlite::Connection, table: &str) -> HashSet<String> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return HashSet::new(),
    };
    let rows = match stmt.query_map([], |row| row.get::<_, String>(1)) {
        Ok(r) => r,
        Err(_) => return HashSet::new(),
    };
    rows.filter_map(|r| r.ok()).collect()
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

fn col_exists(conn: &rusqlite::Connection, table: &str, col: &str) -> bool {
    table_columns(conn, table).into_iter().any(|name| name.eq_ignore_ascii_case(col))
}

// ---------------------------------------------------------------------------
// Raw row shape

/// One raw Shazam tag — written to `media/shazam/YYYY-MM.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShazamTag {
    /// RFC3339 local time of the Shazam tag.
    pub ts: String,
    /// Song title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Artist name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artist: String,
    /// Shazam's own numeric track ID — present in some schema variants
    /// (Needs-sample confirmation). Empty when not available.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub shazam_id: String,
    /// DB ROWID — incremental cursor; not the stable track guid.
    pub rowid: i64,
    /// Subtitle field from the older schema variant (often album/show name).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    /// Apple Music track URL if present.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub apple_music_url: String,
    /// Latitude where the tag was made (NULL-safe; None when not captured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// Longitude where the tag was made (NULL-safe; None when not captured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
}

// ---------------------------------------------------------------------------
// DB import (copy-then-read, watermarked on ROWID)

fn import_shazam_db(vault: &Vault, db: &Path, cursor: i64) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening ShazamDataModel copy {}", db.display()))?;

    let tags: Vec<ShazamTag> = match detect_schema(&conn) {
        SchemaVariant::TitleArtist => import_title_artist(&conn, cursor)?,
        SchemaVariant::TrackSubtitle => import_track_subtitle(&conn, cursor)?,
        SchemaVariant::Minimal => import_minimal(&conn, cursor)?,
    };

    if tags.is_empty() {
        return Ok((0, cursor));
    }
    let new_cursor = tags.iter().map(|t| t.rowid).max().unwrap_or(cursor);
    vault
        .stream(STREAM_DIR, Partition::Month)
        .append(&tags, |t| &t.ts)
        .context("writing shazam tags")?;
    Ok((tags.len() as u64, new_cursor))
}

fn import_title_artist(conn: &rusqlite::Connection, cursor: i64) -> Result<Vec<ShazamTag>> {
    let has_url = col_exists(conn, "ZSHTAGRESULTMO", "ZAPPLEMUSICSONGURL");
    let has_shazam_id = col_exists(conn, "ZSHTAGRESULTMO", "ZSHAZAMID");
    let has_loc = col_exists(conn, "ZSHTAGRESULTMO", "ZLATITUDE");
    let sql = format!(
        "SELECT ROWID, COALESCE(ZTITLE,''), COALESCE(ZARTIST,''), \
                {sid}, ZDATE, {url}, {lat}, {lon} \
         FROM ZSHTAGRESULTMO WHERE ROWID > ?1 AND ZDATE > 0 ORDER BY ROWID",
        sid = if has_shazam_id { "COALESCE(CAST(ZSHAZAMID AS TEXT),'')" } else { "''" },
        url = if has_url { "COALESCE(ZAPPLEMUSICSONGURL,'')" } else { "''" },
        lat = if has_loc { "ZLATITUDE" } else { "NULL" },
        lon = if has_loc { "ZLONGITUDE" } else { "NULL" },
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([cursor])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let title: String = row.get(1)?;
        let artist: String = row.get(2)?;
        let shazam_id: String = row.get(3)?;
        let ts_secs: f64 = row.get(4)?;
        let apple_url: String = row.get(5)?;
        let latitude: Option<f64> = row.get(6)?;
        let longitude: Option<f64> = row.get(7)?;
        let Some(ts) = apple_secs_to_rfc3339(ts_secs) else { continue };
        out.push(ShazamTag {
            ts,
            title,
            artist,
            shazam_id,
            rowid,
            subtitle: String::new(),
            apple_music_url: apple_url,
            latitude,
            longitude,
        });
    }
    Ok(out)
}

fn import_track_subtitle(conn: &rusqlite::Connection, cursor: i64) -> Result<Vec<ShazamTag>> {
    let has_artist_table = table_exists(conn, "ZSHARTISTMO");
    let has_loc = col_exists(conn, "ZSHTAGRESULTMO", "ZLATITUDE");
    // Community-documented JOIN: ZSHARTISTMO.ZTAGRESULT is a foreign key
    // pointing at ZSHTAGRESULTMO.Z_PK (not Z_PK==Z_PK, which would match
    // wrong rows — see Vaughan Harper, sn3p/shazam-tags, sophiegblog sources).
    let sql = if has_artist_table {
        format!(
            "SELECT t.ROWID, COALESCE(t.ZTRACKNAME,''), \
                    COALESCE(a.ZNAME, t.ZSUBTITLE, ''), \
                    '', t.ZDATE, \
                    COALESCE(t.ZSUBTITLE,''), {lat}, {lon} \
             FROM ZSHTAGRESULTMO t \
             LEFT JOIN ZSHARTISTMO a ON a.ZTAGRESULT = t.Z_PK \
             WHERE t.ROWID > ?1 AND t.ZDATE > 0 ORDER BY t.ROWID",
            lat = if has_loc { "t.ZLATITUDE" } else { "NULL" },
            lon = if has_loc { "t.ZLONGITUDE" } else { "NULL" },
        )
    } else {
        format!(
            "SELECT ROWID, COALESCE(ZTRACKNAME,''), COALESCE(ZSUBTITLE,''), \
                    '', ZDATE, \
                    COALESCE(ZSUBTITLE,''), {lat}, {lon} \
             FROM ZSHTAGRESULTMO WHERE ROWID > ?1 AND ZDATE > 0 ORDER BY ROWID",
            lat = if has_loc { "ZLATITUDE" } else { "NULL" },
            lon = if has_loc { "ZLONGITUDE" } else { "NULL" },
        )
    };
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([cursor])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let title: String = row.get(1)?;
        let artist: String = row.get(2)?;
        let shazam_id: String = row.get(3)?;
        let ts_secs: f64 = row.get(4)?;
        let subtitle: String = row.get(5)?;
        let latitude: Option<f64> = row.get(6)?;
        let longitude: Option<f64> = row.get(7)?;
        let Some(ts) = apple_secs_to_rfc3339(ts_secs) else { continue };
        out.push(ShazamTag {
            ts,
            title,
            artist,
            shazam_id,
            rowid,
            subtitle,
            apple_music_url: String::new(),
            latitude,
            longitude,
        });
    }
    Ok(out)
}

fn import_minimal(conn: &rusqlite::Connection, cursor: i64) -> Result<Vec<ShazamTag>> {
    let has_loc = col_exists(conn, "ZSHTAGRESULTMO", "ZLATITUDE");
    let sql = format!(
        "SELECT ROWID, ZDATE, {lat}, {lon} FROM ZSHTAGRESULTMO \
         WHERE ROWID > ?1 AND ZDATE > 0 ORDER BY ROWID",
        lat = if has_loc { "ZLATITUDE" } else { "NULL" },
        lon = if has_loc { "ZLONGITUDE" } else { "NULL" },
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([cursor])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let ts_secs: f64 = row.get(1)?;
        let latitude: Option<f64> = row.get(2)?;
        let longitude: Option<f64> = row.get(3)?;
        let Some(ts) = apple_secs_to_rfc3339(ts_secs) else { continue };
        out.push(ShazamTag {
            ts,
            title: String::new(),
            artist: String::new(),
            shazam_id: String::new(),
            rowid,
            subtitle: String::new(),
            apple_music_url: String::new(),
            latitude,
            longitude,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Sync state

/// Persisted watermark for the Shazam collector.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShazamSyncState {
    pub updated: String,
    pub cursor: i64,
}

/// Result of one sync pass.
#[derive(Debug, Clone)]
pub struct ShazamSyncStats {
    pub available: bool,
    pub new_tags: u64,
}

impl Vault {
    /// One incremental Shazam sync pass. Silently a no-op while the DB is
    /// unreadable (Shazam not installed, or Full Disk Access not granted).
    pub fn collect_shazams(&self) -> Result<ShazamSyncStats> {
        let Some(db) = shazam_db_path() else {
            return Ok(ShazamSyncStats { available: false, new_tags: 0 });
        };
        if fs::File::open(&db).is_err() {
            return Ok(ShazamSyncStats { available: false, new_tags: 0 });
        }
        let mut state = self.read_shazam_sync().unwrap_or_default();
        let stem = format!("trove-shazam-{}", std::process::id());
        let (n, max) =
            import_via_copy(&db, &stem, |tmp| import_shazam_db(self, tmp, state.cursor))?;
        if max > state.cursor {
            state.cursor = max;
        }
        state.updated = Local::now().to_rfc3339();
        self.write_shazam_sync(&state)?;
        Ok(ShazamSyncStats { available: true, new_tags: n })
    }

    pub fn read_shazam_sync(&self) -> Option<ShazamSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_shazam_sync(&self, state: &ShazamSyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }

    /// Rebuild cursor from JSONL logs (used when sync file is missing).
    pub fn rebuild_shazam_sync(&self) -> ShazamSyncState {
        let mut cursor = 0i64;
        let dir = self.root().join(STREAM_DIR);
        let Ok(entries) = fs::read_dir(&dir) else {
            return ShazamSyncState::default();
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(body) = fs::read_to_string(&path) else { continue };
            for line in body.lines() {
                if let Ok(tag) = serde_json::from_str::<ShazamTag>(line) {
                    cursor = cursor.max(tag.rowid);
                }
            }
        }
        ShazamSyncState { updated: String::new(), cursor }
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-shazam-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn fake_db_ta(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir()
            .join(format!("trove-shazam-ta-{}-{name}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            // ZDATE is the community-documented timestamp column name.
            // ZSHAZAMID is optional (Needs-sample); included here for the
            // TitleArtist fixture path only.
            "CREATE TABLE ZSHTAGRESULTMO (
                ROWID INTEGER PRIMARY KEY,
                ZTITLE TEXT,
                ZARTIST TEXT,
                ZSHAZAMID INTEGER,
                ZDATE REAL
             );",
        )
        .unwrap();
        (path, conn)
    }

    fn fake_db_ts(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir()
            .join(format!("trove-shazam-ts-{}-{name}.db", std::process::id()));
        let _ = fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            // ZDATE is the community-documented timestamp column name.
            // ZSHARTISTMO.ZTAGRESULT is a FK pointing at ZSHTAGRESULTMO.Z_PK
            // (not Z_PK==Z_PK; all three primary community sources confirm this).
            // Z_PK values intentionally differ between the two tables so the
            // JOIN is truly exercising ZTAGRESULT and cannot accidentally match
            // on a coincidental PK equality.
            "CREATE TABLE ZSHTAGRESULTMO (
                ROWID INTEGER PRIMARY KEY,
                Z_PK INTEGER,
                ZTRACKNAME TEXT,
                ZSUBTITLE TEXT,
                ZDATE REAL
             );
             CREATE TABLE ZSHARTISTMO (
                ROWID INTEGER PRIMARY KEY,
                Z_PK INTEGER,
                ZTAGRESULT INTEGER,
                ZNAME TEXT
             );",
        )
        .unwrap();
        (path, conn)
    }

    /// 2025-03-15T12:00:00Z = Apple secs 763732800.
    const TAG_TS: f64 = 763_732_800.0;
    /// Same date + 2 h.
    const TAG_TS2: f64 = 763_740_000.0;

    #[test]
    fn apple_epoch_conversion() {
        let rfc = apple_secs_to_rfc3339(TAG_TS).unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(&rfc).unwrap();
        assert_eq!(parsed.timestamp(), 1_742_040_000);
        assert!(apple_secs_to_rfc3339(0.0).is_none());
        assert!(apple_secs_to_rfc3339(-1.0).is_none());
    }

    #[test]
    fn title_artist_schema_round_trip() {
        let v = temp_vault("ta");
        let (db, conn) = fake_db_ta("ta");
        conn.execute(
            "INSERT INTO ZSHTAGRESULTMO VALUES (1, 'Bohemian Rhapsody', 'Queen', 789456123, ?1)",
            [TAG_TS],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ZSHTAGRESULTMO VALUES (2, 'Stairway to Heaven', 'Led Zeppelin', 456789012, ?1)",
            [TAG_TS2],
        )
        .unwrap();
        drop(conn);

        let (n, cursor) = import_shazam_db(&v, &db, 0).unwrap();
        assert_eq!(n, 2);
        assert_eq!(cursor, 2);

        let parts = v.stream(STREAM_DIR, Partition::Month).partitions().unwrap();
        assert_eq!(parts.len(), 1, "both tags in the same month");
        let tags: Vec<ShazamTag> =
            v.stream(STREAM_DIR, Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(tags.len(), 2);
        assert_eq!(tags[0].title, "Bohemian Rhapsody");
        assert_eq!(tags[0].artist, "Queen");
        assert_eq!(tags[0].shazam_id, "789456123");
        assert_eq!(tags[0].rowid, 1);
        assert_eq!(tags[1].title, "Stairway to Heaven");
        assert_eq!(tags[1].artist, "Led Zeppelin");
        assert_eq!(tags[1].rowid, 2);

        // Idempotent: re-import from cursor returns nothing new.
        let (n2, cursor2) = import_shazam_db(&v, &db, cursor).unwrap();
        assert_eq!(n2, 0);
        assert_eq!(cursor2, cursor);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn track_subtitle_schema_with_artist_join() {
        let v = temp_vault("ts");
        let (db, conn) = fake_db_ts("ts");
        // Tag row: ROWID=1, Z_PK=42 (tag's own PK — distinct from artist PK
        // so an accidental Z_PK==Z_PK join would return no match).
        // ZSHTAGRESULTMO cols: ROWID, Z_PK, ZTRACKNAME, ZSUBTITLE, ZDATE
        conn.execute(
            "INSERT INTO ZSHTAGRESULTMO (ROWID, Z_PK, ZTRACKNAME, ZSUBTITLE, ZDATE) \
             VALUES (1, 42, 'Come Together', 'Abbey Road', ?1)",
            [TAG_TS],
        )
        .unwrap();
        // Artist row: Z_PK=99 (≠ 42), ZTAGRESULT=42 (FK → tag Z_PK).
        // A correct `ON a.ZTAGRESULT = t.Z_PK` will match; Z_PK==Z_PK would not.
        conn.execute(
            "INSERT INTO ZSHARTISTMO (ROWID, Z_PK, ZTAGRESULT, ZNAME) VALUES (1, 99, 42, 'The Beatles')",
            [],
        )
        .unwrap();
        drop(conn);

        let (n, cursor) = import_shazam_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1);
        assert_eq!(cursor, 1);

        let parts = v.stream(STREAM_DIR, Partition::Month).partitions().unwrap();
        let tags: Vec<ShazamTag> =
            v.stream(STREAM_DIR, Partition::Month).read(&parts[0]).unwrap();
        assert_eq!(tags[0].title, "Come Together");
        assert_eq!(tags[0].artist, "The Beatles", "artist from ZSHARTISTMO join via ZTAGRESULT");
        assert_eq!(tags[0].subtitle, "Abbey Road");
        // TrackSubtitle path emits no shazam_id (not documented in older schema).
        assert_eq!(tags[0].shazam_id, "");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn incremental_watermark_no_duplicates() {
        let v = temp_vault("incr");
        let (db, conn) = fake_db_ta("incr");
        conn.execute("INSERT INTO ZSHTAGRESULTMO VALUES (1, 'Song A', 'Artist A', 100, ?1)", [TAG_TS])
            .unwrap();

        let (n, cursor) = import_shazam_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1);
        assert_eq!(cursor, 1);

        conn.execute("INSERT INTO ZSHTAGRESULTMO VALUES (2, 'Song B', 'Artist B', 200, ?1)", [TAG_TS2])
            .unwrap();
        drop(conn);

        let (n2, cursor2) = import_shazam_db(&v, &db, cursor).unwrap();
        assert_eq!(n2, 1, "only the new row");
        assert_eq!(cursor2, 2);

        let total: usize = v
            .stream(STREAM_DIR, Partition::Month)
            .partitions()
            .unwrap()
            .iter()
            .map(|m| {
                v.stream(STREAM_DIR, Partition::Month)
                    .read::<ShazamTag>(m)
                    .unwrap()
                    .len()
            })
            .sum();
        assert_eq!(total, 2, "exactly 2 rows, no duplicates");

        let _ = fs::remove_file(db);
    }

    #[test]
    fn zero_timestamp_rows_skipped() {
        let v = temp_vault("zerots");
        let (db, conn) = fake_db_ta("zerots");
        conn.execute("INSERT INTO ZSHTAGRESULTMO VALUES (1, 'Good Song', 'Good Artist', 111, ?1)", [TAG_TS])
            .unwrap();
        // ZDATE=0.0 means no timestamp captured; WHERE ZDATE > 0 must skip it.
        conn.execute("INSERT INTO ZSHTAGRESULTMO VALUES (2, 'No Time', 'Nobody', 222, 0.0)", [])
            .unwrap();
        drop(conn);

        let (n, cursor) = import_shazam_db(&v, &db, 0).unwrap();
        assert_eq!(n, 1, "zero-ZDATE row skipped");
        // The cursor only advances to the max ROWID among valid rows that were
        // imported; ROWID 2 was skipped so cursor is 1.
        assert_eq!(cursor, 1);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn rebuild_cursor_from_jsonl() {
        let v = temp_vault("rebuild");
        let (db, conn) = fake_db_ta("rebuild");
        conn.execute("INSERT INTO ZSHTAGRESULTMO VALUES (7, 'Track', 'Band', 999, ?1)", [TAG_TS])
            .unwrap();
        drop(conn);

        import_shazam_db(&v, &db, 0).unwrap();
        let rebuilt = v.rebuild_shazam_sync();
        assert_eq!(rebuilt.cursor, 7);

        let _ = fs::remove_file(db);
    }

    #[test]
    fn schema_detection_identifies_both_variants() {
        let ta_path = std::env::temp_dir()
            .join(format!("trove-shazam-detect-ta-{}.db", std::process::id()));
        let conn = rusqlite::Connection::open(&ta_path).unwrap();
        conn.execute_batch(
            // ZDATE is the correct community-documented timestamp column.
            "CREATE TABLE ZSHTAGRESULTMO (ROWID INTEGER PRIMARY KEY, ZTITLE TEXT, ZARTIST TEXT, ZSHAZAMID INTEGER, ZDATE REAL);",
        )
        .unwrap();
        assert_eq!(detect_schema(&conn), SchemaVariant::TitleArtist);
        drop(conn);
        let _ = fs::remove_file(&ta_path);

        let ts_path = std::env::temp_dir()
            .join(format!("trove-shazam-detect-ts-{}.db", std::process::id()));
        let conn = rusqlite::Connection::open(&ts_path).unwrap();
        conn.execute_batch(
            // ZDATE is the correct community-documented timestamp column.
            "CREATE TABLE ZSHTAGRESULTMO (ROWID INTEGER PRIMARY KEY, ZTRACKNAME TEXT, ZSUBTITLE TEXT, ZDATE REAL);",
        )
        .unwrap();
        assert_eq!(detect_schema(&conn), SchemaVariant::TrackSubtitle);
        drop(conn);
        let _ = fs::remove_file(&ts_path);
    }

    #[test]
    fn tag_serializes_and_deserializes_with_optional_fields() {
        // Fields with skip_serializing_if must survive a round-trip from a
        // sparse line (back-compat for old records without new fields).
        let minimal =
            r#"{"ts":"2025-03-15T12:00:00+00:00","rowid":1}"#;
        let t: ShazamTag = serde_json::from_str(minimal).unwrap();
        assert_eq!(t.title, "");
        assert_eq!(t.artist, "");
        assert_eq!(t.shazam_id, "");
        assert!(t.subtitle.is_empty());
        assert!(t.apple_music_url.is_empty());

        // Empty optional fields must not appear in the serialized output.
        let out = serde_json::to_string(&t).unwrap();
        assert!(!out.contains("title"), "empty title omitted");
        assert!(!out.contains("artist"), "empty artist omitted");
        assert!(!out.contains("shazam_id"), "empty shazam_id omitted");
    }
}
