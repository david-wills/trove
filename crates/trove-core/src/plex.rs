//! Plex Media Server — local play history from the server SQLite database.
//!
//! Reads `com.plexapp.plugins.library.db` directly (copy-then-open so the DB
//! can be read whether or not the server is running and without holding the
//! server's WAL lock). No Plex Pass required — the `metadata_item_views`
//! table is populated by the free server.
//!
//! **What the DB can tell us:** `metadata_item_views` records one row per
//! discrete view event with `viewed_at` (Unix epoch seconds). This is the
//! denormalised per-view source and carries `grandparent_title` / `parent_title`
//! / `index` / `parent_index` directly. `metadata_item_settings` (keyed by
//! `guid`, NOT by a `metadata_item_id` FK) supplies `view_count` /
//! `view_offset` and acts as the per-item dedup watermark. `metadata_items`
//! provides `year` and the canonical `id` for dedup keys.
//!
//! **Two layers per item:**
//! - **raw** — the joined DB row verbatim in `media/plays/plex/raw/YYYY-MM.jsonl`
//! - **contract** — one [`MediaItem`] in `media/plays/plex/YYYY-MM.jsonl`
//!
//! **Metadata types** (from Plex's `metadata_type` column, verified against
//! the python-plexapi `SEARCHTYPES` dict):
//! - 1 = movie
//! - 2 = show  (view state on the show itself; skipped — not individual watches)
//! - 3 = season (skipped — not individual watches)
//! - 4 = episode
//! - 8 = artist (skipped — no direct play evidence)
//! - 9 = album  (skipped)
//! - 10 = track
//!
//! **Watermark**: `viewed_at` per `(guid, viewed_at)` pair, kept in
//! `.trove/plex-sync.json`. The cursor is a map of `guid` → last emitted
//! `viewed_at` (Unix secs). A new view_row re-emits only when its `viewed_at`
//! strictly advances. The cursor file is rebuildable from the vault.
//!
//! Brief: `docs/integrations/plex.md`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

const DIR: &str = "media/plays/plex";
const RAW_DIR: &str = "media/plays/plex/raw";
const SYNC_FILE: &str = ".trove/plex-sync.json";

/// Hourly cadence: Plex DB changes on new watches; the mtime gate skips
/// passes where nothing happened.
const PLEX_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Path helpers.

/// Default path to the Plex Media Server SQLite database.
fn default_plex_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join("Library/Application Support/Plex Media Server/Plug-in Support/Databases/com.plexapp.plugins.library.db")
}

fn plex_db_mtime() -> Option<SystemTime> {
    fs::metadata(default_plex_db_path()).ok()?.modified().ok()
}

fn plex_permission_ok() -> bool {
    crate::registry::readable(&default_plex_db_path())
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(plex_permission_ok()),
        required: false,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    if !plex_permission_ok() {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    let n = pull_db(vault, &default_plex_db_path())?;
    Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
        format!("plex: {n} new plays")
    }))
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let n = pull_db(vault, &default_plex_db_path())?;
    let headline = if n == 0 {
        "Plex is up to date — no new plays".to_string()
    } else {
        format!("Plex synced — {n} plays")
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("plays", n as u64)]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "plex",
        name: "Plex",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Read your Plex Media Server play history directly from its SQLite database, \
                      with no Plex Pass required. Movies, shows, and music — every item you have \
                      watched or listened to — land in the unified media stream.",
        domain: "media",
        vault_path: "media/plays/plex/",
        toggleable: true,
        setup: &[
            "Plex Media Server must be installed on this Mac (any version; no Plex Pass needed).",
            "Grant Trove Full Disk Access in System Settings → Privacy & Security → Full Disk Access.",
            "The server does not need to be running — Trove reads the database file directly.",
        ],
        caveats: "The Plex database records last-viewed state per item, not a full per-play log. \
                  Multiple rewatches on the same day may appear as one entry. \
                  Full Disk Access is required to read the Plex application database.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::on_change(PLEX_SYNC_SECS, plex_db_mtime),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor / sync state.

/// Persisted cursor: last `viewed_at` (Unix epoch seconds) per guid.
/// Items re-emit only when `viewed_at` strictly advances, preventing duplicate
/// rows on each sync. Keyed by guid (from `metadata_item_views`).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// `guid` -> last emitted `viewed_at` Unix seconds.
    #[serde(default)]
    seen: BTreeMap<String, i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_plex_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_plex_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row (full fidelity, unconditional).

/// A raw-layer line: the joined Plex DB row serialised verbatim.
/// `ts` drives the month-partition writer; only `value` is serialised on disk.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// DB read — pure function, fixture-testable.

/// One parsed view-state row from the Plex DB join.
///
/// Primary source is `metadata_item_views` (one row per discrete view event).
/// `view_count` / `view_offset` are joined from `metadata_item_settings` on
/// `guid`. `year` and `item_id` (canonical library item id) are joined from
/// `metadata_items` on `guid`.
#[derive(Debug, Clone)]
pub struct PlexRow {
    /// `metadata_items.id` — canonical library item id (unique per library item).
    /// Used in the dedup guid to prevent collisions when two items share a guid.
    pub item_id: i64,
    /// `metadata_item_views.metadata_type` (1=movie, 4=episode, 10=track)
    pub metadata_type: i64,
    /// Primary title (`metadata_item_views.title`)
    pub title: String,
    /// For episodes: the show title (`metadata_item_views.grandparent_title`).
    /// For tracks: the artist name. Empty otherwise.
    pub show_title: String,
    /// For episodes: the season number (`metadata_item_views.parent_index`). 0 when unknown.
    pub season: i64,
    /// For episodes/tracks: item number (`metadata_item_views.index`). 0 when unknown.
    pub episode: i64,
    /// Release year (`metadata_items.year`). 0 when unknown.
    pub year: i64,
    /// `metadata_item_settings.view_count` (joined on guid). 0 when missing.
    pub view_count: i64,
    /// `metadata_item_views.viewed_at` (Unix epoch seconds). 0 = unset.
    pub last_viewed_at: i64,
    /// `metadata_item_settings.view_offset` (milliseconds, joined on guid). 0 when unknown.
    pub view_offset: i64,
    /// `metadata_item_views.guid` — a URI like `plex://movie/...`.
    pub guid: String,
}

/// Read all viewed items from the Plex DB at `path`. Returns a flat Vec of
/// [`PlexRow`] with `last_viewed_at > 0` (items the user has actually watched).
///
/// Primary source is `metadata_item_views` — one row per discrete view event,
/// carrying denormalized `grandparent_title`, `parent_title`, `title`,
/// `metadata_type`, `index` (episode/track number), `parent_index` (season),
/// and `viewed_at`. `metadata_item_settings` is LEFT-JOINed on `guid` to pick
/// up `view_count` and `view_offset`. `metadata_items` is LEFT-JOINed on
/// `guid` to supply `year` and the canonical item `id` (used in dedup keys).
///
/// We emit only metadata_type IN (1, 4, 10): movies, episodes, and music tracks.
pub fn read_plex_db(path: &Path) -> Result<Vec<PlexRow>> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("opening Plex DB copy")?;

    // Bail gracefully when the expected tables are absent.
    let miv_cols = pragma_columns(&conn, "metadata_item_views");
    if miv_cols.is_empty() {
        return Ok(Vec::new());
    }

    // metadata_item_views is the authoritative per-view source.
    // It carries grandparent_title, parent_title, index, parent_index, viewed_at.
    // metadata_item_settings is keyed by guid (NOT by a metadata_item_id FK).
    // metadata_items supplies year and the canonical item id.
    let sql = "SELECT
            miv.guid,
            miv.metadata_type,
            COALESCE(miv.title, '')             AS title,
            COALESCE(miv.grandparent_title, '') AS grandparent_title,
            COALESCE(miv.parent_title, '')      AS parent_title,
            COALESCE(miv.\"index\", 0)          AS episode_index,
            COALESCE(miv.parent_index, 0)       AS season_index,
            CAST(COALESCE(miv.viewed_at, 0) AS INTEGER) AS viewed_at,
            COALESCE(mis.view_count, 0)         AS view_count,
            COALESCE(mis.view_offset, 0)        AS view_offset,
            COALESCE(mi.year, 0)                AS year,
            COALESCE(mi.id, 0)                  AS item_id
         FROM metadata_item_views miv
         LEFT JOIN metadata_item_settings mis ON mis.guid = miv.guid
         LEFT JOIN metadata_items mi ON mi.guid = miv.guid
         WHERE miv.viewed_at IS NOT NULL
           AND CAST(miv.viewed_at AS INTEGER) > 0
           AND miv.metadata_type IN (1, 4, 10)";

    let mut stmt = conn.prepare(sql).context("preparing Plex query")?;
    let mut rows_out = Vec::new();
    let mut rows = stmt.query([]).context("executing Plex query")?;
    while let Ok(Some(row)) = rows.next() {
        let guid: String =
            row.get::<_, Option<String>>(0).unwrap_or_default().unwrap_or_default();
        let metadata_type: i64 = row.get(1).unwrap_or(0);
        let title: String =
            row.get::<_, Option<String>>(2).unwrap_or_default().unwrap_or_default();
        let grandparent_title: String =
            row.get::<_, Option<String>>(3).unwrap_or_default().unwrap_or_default();
        let parent_title: String =
            row.get::<_, Option<String>>(4).unwrap_or_default().unwrap_or_default();
        let episode_index: i64 = row.get(5).unwrap_or(0);
        let season_index: i64 = row.get(6).unwrap_or(0);
        let last_viewed_at: i64 = row.get(7).unwrap_or(0);
        let view_count: i64 = row.get(8).unwrap_or(0);
        let view_offset: i64 = row.get(9).unwrap_or(0);
        let year: i64 = row.get(10).unwrap_or(0);
        let item_id: i64 = row.get(11).unwrap_or(0);

        if last_viewed_at == 0 {
            continue;
        }

        // show_title: grandparent_title for episodes (show name), or
        // parent_title for tracks (artist name). grandparent_title is preferred.
        let show_title = if !grandparent_title.is_empty() {
            grandparent_title
        } else {
            parent_title
        };

        rows_out.push(PlexRow {
            item_id,
            metadata_type,
            title,
            show_title,
            season: season_index,
            episode: episode_index,
            year,
            view_count,
            last_viewed_at,
            view_offset,
            guid,
        });
    }

    Ok(rows_out)
}

/// Column names for a table via `PRAGMA table_info`. Returned in definition
/// order; empty when the table does not exist.
fn pragma_columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut names = Vec::new();
    let mut rows = match stmt.query([]) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    while let Ok(Some(row)) = rows.next() {
        let name: String = row.get(1).unwrap_or_default();
        names.push(name);
    }
    names
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-testable.

/// Convert a [`PlexRow`] into a `(MediaItem, Value)` pair (contract + raw).
/// Returns `None` if the row lacks the fields required for a valid play.
pub fn row_to_item(row: &PlexRow) -> Option<(MediaItem, Value)> {
    if row.title.is_empty() || row.last_viewed_at == 0 {
        return None;
    }

    // `last_viewed_at` is Unix epoch seconds. Convert to local RFC3339.
    let ts = DateTime::from_timestamp(row.last_viewed_at, 0)
        .map(|t| t.with_timezone(&Local).to_rfc3339())?;

    let (category, title, subtitle, detail) = match row.metadata_type {
        1 => {
            // Movie: subtitle = "Title (Year)" for chart grouping.
            let subtitle = if row.year > 0 {
                format!("{} ({})", row.title, row.year)
            } else {
                row.title.clone()
            };
            ("video", row.title.clone(), subtitle, String::new())
        }
        4 => {
            // Episode: title=ep title, subtitle=show, detail=SxxExx.
            let detail = if row.season > 0 && row.episode > 0 {
                format!("S{:02}E{:02}", row.season, row.episode)
            } else if row.episode > 0 {
                format!("E{:02}", row.episode)
            } else {
                String::new()
            };
            let subtitle = if row.show_title.is_empty() {
                row.title.clone()
            } else {
                row.show_title.clone()
            };
            ("video", row.title.clone(), subtitle, detail)
        }
        10 => {
            // Music track: subtitle = artist (from the grandparent chain).
            ("music", row.title.clone(), row.show_title.clone(), String::new())
        }
        _ => return None,
    };

    // play vs partial: if view_count == 0 but view_offset > 0, the user
    // started but did not finish. Otherwise it is a completed play.
    let kind = if row.view_count == 0 && row.view_offset > 0 { "partial" } else { "play" };

    // Guid: incorporate both item_id and guid-fragment + timestamp.
    // item_id is unique per library item (unlike guid, which can be shared
    // by the same recording on different albums). Two distinct watches produce
    // distinct guids because each metadata_item_views row has its own viewed_at.
    let guid = if row.guid.is_empty() {
        format!("plex-{}-{}", row.item_id, row.last_viewed_at)
    } else {
        let g = row.guid.trim_start_matches("plex://").trim_start_matches("com.plexapp.agents.");
        if row.item_id > 0 {
            format!("plex-{}-{g}-{}", row.item_id, row.last_viewed_at)
        } else {
            format!("plex-{g}-{}", row.last_viewed_at)
        }
    };

    let mut extra: Map<String, Value> = Map::new();
    extra.insert("item_id".into(), Value::Number(row.item_id.into()));
    extra.insert("view_count".into(), Value::Number(row.view_count.into()));
    if row.view_offset > 0 {
        extra.insert("view_offset_ms".into(), Value::Number(row.view_offset.into()));
    }
    if row.metadata_type == 4 && row.season > 0 {
        extra.insert("season".into(), Value::Number(row.season.into()));
    }
    if row.metadata_type == 4 && row.episode > 0 {
        extra.insert("episode".into(), Value::Number(row.episode.into()));
    }
    if row.year > 0 {
        extra.insert("year".into(), Value::Number(row.year.into()));
    }
    if !row.guid.is_empty() {
        extra.insert("plex_guid".into(), Value::String(row.guid.clone()));
    }

    let raw_val = serde_json::json!({
        "item_id": row.item_id,
        "metadata_type": row.metadata_type,
        "title": row.title,
        "show_title": row.show_title,
        "season": row.season,
        "episode": row.episode,
        "year": row.year,
        "view_count": row.view_count,
        "last_viewed_at": row.last_viewed_at,
        "view_offset": row.view_offset,
        "guid": row.guid,
    });

    Some((
        MediaItem {
            ts,
            source: "plex".into(),
            category: category.into(),
            device: String::new(),
            kind: kind.into(),
            title,
            subtitle,
            detail,
            seconds: 0,
            favicon: String::new(),
            guid,
            extra,
        },
        raw_val,
    ))
}

// ---------------------------------------------------------------------------
// Pull: copy DB -> parse -> write new rows -> advance cursor.

/// Run one sync pass from the DB at `db_path` (injectable for tests).
/// Returns the count of new plays written.
pub fn pull_db(vault: &Vault, db_path: &Path) -> Result<usize> {
    if !db_path.exists() {
        // Server not installed — silent no-op.
        return Ok(0);
    }

    // Stem is unique per (process, db-path) to avoid test-parallel collisions
    // when two pull_db calls race on different db_path arguments.
    let path_hash: u64 = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        db_path.hash(&mut h);
        h.finish()
    };
    let stem = format!("trove-plex-{}-{path_hash:x}", std::process::id());
    let rows = import_via_copy(db_path, &stem, |tmp| read_plex_db(tmp))?;
    if rows.is_empty() {
        return Ok(0);
    }

    let mut state = vault.read_plex_sync();

    // Build the set of guids already on disk for dedup (backstop if cursor lost).
    let mut seen_guids: HashSet<String> = HashSet::new();
    let contract = vault.stream(DIR, Partition::Month);
    for key in contract.partitions()? {
        for item in contract.read::<MediaItem>(&key)? {
            if !item.guid.is_empty() {
                seen_guids.insert(item.guid);
            }
        }
    }
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut new_items: Vec<MediaItem> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();

    for row in &rows {
        // Watermark gate: emit only when viewed_at strictly advances per guid.
        // guid is the natural key in metadata_item_views; each distinct view
        // event has its own viewed_at. Multiple rewatches produce multiple rows
        // with advancing viewed_at values.
        let key = row.guid.clone();
        let prev = state.seen.get(&key).copied().unwrap_or(0);
        if row.last_viewed_at <= prev {
            continue;
        }

        let Some((item, raw_val)) = row_to_item(row) else {
            // Advance cursor even when we skip (e.g. empty title) so we do
            // not retry a permanently-skippable row every sync.
            state.seen.insert(key, row.last_viewed_at);
            continue;
        };

        if !seen_guids.insert(item.guid.clone()) {
            // Already on disk — just advance the cursor.
            state.seen.insert(key, row.last_viewed_at);
            continue;
        }

        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val });
        new_items.push(item);
        state.seen.insert(key, row.last_viewed_at);
    }

    let count = new_items.len();

    raw_stream.append(&new_raws, |r| &r.ts)?;
    contract.append(&new_items, |i| &i.ts)?;

    // Persist cursor only after a successful write (crash safety).
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_plex_sync(&state)?;

    Ok(count)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-plex-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build an on-disk Plex-like SQLite DB at `path` using the REAL Plex
    /// schema (verified against pwinn/schemas com.plexapp.plugins.library.db.sql).
    ///
    /// Key schema facts:
    ///  - `metadata_item_settings` is keyed by `guid` (VARCHAR), NOT by a
    ///    `metadata_item_id` FK. It carries view_count / view_offset / last_viewed_at.
    ///  - `metadata_item_views` has one row per discrete view event and carries
    ///    the denormalized grandparent_title / parent_title / index / parent_index
    ///    / viewed_at. It is the primary source for read_plex_db.
    ///  - `metadata_items` has NO grandparent_title or parent_title columns.
    fn make_plex_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            // metadata_items: real schema (no grandparent_title / parent_title)
            "CREATE TABLE metadata_items (
                id                INTEGER PRIMARY KEY,
                library_section_id INTEGER,
                parent_id         INTEGER,
                metadata_type     INTEGER,
                guid              TEXT,
                title             TEXT,
                year              INTEGER,
                \"index\"         INTEGER
            );

            -- metadata_item_settings: keyed by guid, NOT by metadata_item_id
            CREATE TABLE metadata_item_settings (
                id             INTEGER PRIMARY KEY,
                account_id     INTEGER,
                guid           VARCHAR(255),
                rating         FLOAT,
                view_offset    INTEGER,
                view_count     INTEGER,
                last_viewed_at DATETIME,
                created_at     DATETIME,
                updated_at     DATETIME,
                skip_count     INTEGER DEFAULT 0,
                last_skipped_at DATETIME DEFAULT NULL,
                changed_at     INTEGER DEFAULT 0,
                extra_data     VARCHAR(255),
                last_rated_at  DATETIME
            );

            -- metadata_item_views: one row per discrete view event, with
            -- denormalized grandparent_title, parent_title, index, parent_index
            CREATE TABLE metadata_item_views (
                id                    INTEGER PRIMARY KEY,
                account_id            INTEGER,
                guid                  VARCHAR(255),
                metadata_type         INTEGER,
                library_section_id    INTEGER,
                grandparent_title     VARCHAR(255),
                parent_index          INTEGER,
                parent_title          VARCHAR(255),
                \"index\"             INTEGER,
                title                 VARCHAR(255),
                thumb_url             VARCHAR(255),
                viewed_at             DATETIME,
                grandparent_guid      VARCHAR(255),
                originally_available_at DATETIME,
                device_id             INTEGER
            );

            -- metadata_items: canonical library items (no grandparent/parent title cols)
            -- Item 1: movie (type 1)
            INSERT INTO metadata_items VALUES (1, 1, NULL, 1, 'plex://movie/matrix', 'The Matrix', 1999, NULL);
            -- Item 2: episode (type 4)
            INSERT INTO metadata_items VALUES (2, 1, 11, 4, 'plex://episode/pilot', 'Pilot', NULL, 1);
            -- Item 3: music track (type 10)
            INSERT INTO metadata_items VALUES (3, 2, NULL, 10, 'plex://track/brhapsody', 'Bohemian Rhapsody', 1975, NULL);
            -- Item 4: show (type 2) — excluded by type filter
            INSERT INTO metadata_items VALUES (4, 1, NULL, 2, NULL, 'The Wire', 2002, NULL);
            -- Item 5: partial episode (view_count=0, view_offset>0)
            INSERT INTO metadata_items VALUES (5, 1, 11, 4, 'plex://episode/737', 'Seven Thirty-Seven', NULL, 2);

            -- metadata_item_settings: keyed by guid, supplies view_count / view_offset
            -- 1749578400 = 2026-06-10T18:00:00Z
            INSERT INTO metadata_item_settings (id, account_id, guid, view_count, last_viewed_at, view_offset)
                VALUES (1, 1, 'plex://movie/matrix',   3, 1749578400, 0);
            -- 1749672000 = 2026-06-11T20:00:00Z
            INSERT INTO metadata_item_settings (id, account_id, guid, view_count, last_viewed_at, view_offset)
                VALUES (2, 1, 'plex://episode/pilot',  1, 1749672000, 0);
            -- 1749722400 = 2026-06-12T10:00:00Z
            INSERT INTO metadata_item_settings (id, account_id, guid, view_count, last_viewed_at, view_offset)
                VALUES (3, 1, 'plex://track/brhapsody', 5, 1749722400, 0);
            -- Show — excluded by type filter in metadata_item_views; no mis row needed
            -- Partial episode: 1749750000 = 2026-06-12T17:20:00Z
            INSERT INTO metadata_item_settings (id, account_id, guid, view_count, last_viewed_at, view_offset)
                VALUES (5, 1, 'plex://episode/737', 0, 1749750000, 60000);

            -- metadata_item_views: one row per discrete view event (primary source)
            -- grandparent_title = show/artist, parent_title = season/album,
            -- parent_index = season number, index = episode/track number
            -- 1749578400 = movie The Matrix
            INSERT INTO metadata_item_views (id, account_id, guid, metadata_type, grandparent_title, parent_index, parent_title, \"index\", title, viewed_at)
                VALUES (1, 1, 'plex://movie/matrix', 1, NULL, NULL, NULL, NULL, 'The Matrix', 1749578400);
            -- 1749672000 = episode Pilot of Breaking Bad S01E01
            INSERT INTO metadata_item_views (id, account_id, guid, metadata_type, grandparent_title, parent_index, parent_title, \"index\", title, viewed_at)
                VALUES (2, 1, 'plex://episode/pilot', 4, 'Breaking Bad', 1, 'Season 1', 1, 'Pilot', 1749672000);
            -- 1749722400 = music track Bohemian Rhapsody by Queen
            INSERT INTO metadata_item_views (id, account_id, guid, metadata_type, grandparent_title, parent_index, parent_title, \"index\", title, viewed_at)
                VALUES (3, 1, 'plex://track/brhapsody', 10, 'Queen', NULL, 'Greatest Hits', NULL, 'Bohemian Rhapsody', 1749722400);
            -- show-level view (type 2) — excluded by metadata_type filter
            INSERT INTO metadata_item_views (id, account_id, guid, metadata_type, grandparent_title, parent_index, parent_title, \"index\", title, viewed_at)
                VALUES (4, 1, NULL, 2, NULL, NULL, NULL, NULL, 'The Wire', 1749578400);
            -- 1749750000 = partial episode Seven Thirty-Seven of Breaking Bad S01E02
            INSERT INTO metadata_item_views (id, account_id, guid, metadata_type, grandparent_title, parent_index, parent_title, \"index\", title, viewed_at)
                VALUES (5, 1, 'plex://episode/737', 4, 'Breaking Bad', 1, 'Season 1', 2, 'Seven Thirty-Seven', 1749750000);",
        )
        .unwrap();
    }

    // -----------------------------------------------------------------------

    #[test]
    fn reads_movie_episode_track_excludes_show() {
        let dir = std::env::temp_dir()
            .join(format!("trove-plex-read-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("plex.db");
        make_plex_db(&db_path);

        let rows = read_plex_db(&db_path).unwrap();
        // movie (1) + episode (2) + track (3) + partial episode (5) = 4
        // show (type 2) excluded by metadata_type IN (1, 4, 10)
        assert_eq!(rows.len(), 4, "movie + episode + track + partial = 4 rows");

        // Movie: joined from metadata_items for year; view_count from settings
        let movie = rows.iter().find(|r| r.metadata_type == 1).unwrap();
        assert_eq!(movie.title, "The Matrix");
        assert_eq!(movie.year, 1999, "year from metadata_items join");
        assert_eq!(movie.view_count, 3, "view_count from metadata_item_settings join");
        assert_eq!(movie.last_viewed_at, 1749578400);
        assert_eq!(movie.item_id, 1, "item_id from metadata_items join on guid");

        // Episode: grandparent_title = show, parent_index = season, index = episode
        let episode = rows.iter().find(|r| r.metadata_type == 4 && r.title == "Pilot").unwrap();
        assert_eq!(episode.title, "Pilot");
        assert_eq!(episode.show_title, "Breaking Bad", "grandparent_title from metadata_item_views");
        assert_eq!(episode.season, 1, "parent_index from metadata_item_views");
        assert_eq!(episode.episode, 1, "index from metadata_item_views");
        assert_eq!(episode.view_count, 1, "view_count from metadata_item_settings join");
        assert_eq!(episode.item_id, 2, "item_id from metadata_items join on guid");

        // Track: grandparent_title = artist
        let track = rows.iter().find(|r| r.metadata_type == 10).unwrap();
        assert_eq!(track.title, "Bohemian Rhapsody");
        assert_eq!(track.show_title, "Queen", "grandparent_title used as artist for tracks");
        assert_eq!(track.view_count, 5);

        // Partial episode: view_count=0, view_offset>0; guid matched via metadata_item_settings
        let partial = rows.iter().find(|r| r.title == "Seven Thirty-Seven").unwrap();
        assert_eq!(partial.view_count, 0, "view_count=0 for partial");
        assert_eq!(partial.view_offset, 60000, "view_offset from metadata_item_settings join");
        assert_eq!(partial.last_viewed_at, 1749750000);
        assert_eq!(partial.item_id, 5, "item_id from metadata_items join");
    }

    #[test]
    fn row_to_item_movie() {
        let row = PlexRow {
            item_id: 1,
            metadata_type: 1,
            title: "The Matrix".into(),
            show_title: String::new(),
            season: 0,
            episode: 0,
            year: 1999,
            view_count: 3,
            last_viewed_at: 1749578400,
            view_offset: 0,
            guid: "plex://movie/matrix".into(),
        };
        let (item, raw) = row_to_item(&row).unwrap();
        assert_eq!(item.title, "The Matrix");
        assert_eq!(item.subtitle, "The Matrix (1999)");
        assert_eq!(item.detail, "", "movies have no SxxExx detail");
        assert_eq!(item.category, "video");
        assert_eq!(item.kind, "play");
        assert_eq!(item.source, "plex");
        assert_eq!(item.seconds, 0);
        // guid encodes the plex uri fragment + timestamp
        assert!(item.guid.contains("matrix"), "guid contains plex guid fragment: {}", item.guid);
        assert!(item.guid.contains("1749578400"), "guid contains timestamp: {}", item.guid);
        assert_eq!(item.extra.get("view_count"), Some(&Value::Number(3.into())));
        assert_eq!(item.extra.get("year"), Some(&Value::Number(1999.into())));
        assert_eq!(raw["metadata_type"], 1);
        assert_eq!(raw["title"].as_str(), Some("The Matrix"));
    }

    #[test]
    fn row_to_item_episode() {
        let row = PlexRow {
            item_id: 2,
            metadata_type: 4,
            title: "Pilot".into(),
            show_title: "Breaking Bad".into(),
            season: 1,
            episode: 1,
            year: 0,
            view_count: 1,
            last_viewed_at: 1749672000,
            view_offset: 0,
            guid: "plex://episode/pilot".into(),
        };
        let (item, _) = row_to_item(&row).unwrap();
        assert_eq!(item.title, "Pilot");
        assert_eq!(item.subtitle, "Breaking Bad");
        assert_eq!(item.detail, "S01E01");
        assert_eq!(item.category, "video");
        assert_eq!(item.kind, "play");
        assert_eq!(item.extra.get("season"), Some(&Value::Number(1.into())));
        assert_eq!(item.extra.get("episode"), Some(&Value::Number(1.into())));
    }

    #[test]
    fn row_to_item_track() {
        let row = PlexRow {
            item_id: 3,
            metadata_type: 10,
            title: "Bohemian Rhapsody".into(),
            show_title: "Queen".into(),
            season: 0,
            episode: 0,
            year: 1975,
            view_count: 5,
            last_viewed_at: 1749722400,
            view_offset: 0,
            guid: "plex://track/brhapsody".into(),
        };
        let (item, _) = row_to_item(&row).unwrap();
        assert_eq!(item.title, "Bohemian Rhapsody");
        assert_eq!(item.subtitle, "Queen");
        assert_eq!(item.category, "music");
        assert_eq!(item.kind, "play");
    }

    #[test]
    fn row_to_item_partial_watch() {
        let row = PlexRow {
            item_id: 5,
            metadata_type: 4,
            title: "Seven Thirty-Seven".into(),
            show_title: "Breaking Bad".into(),
            season: 1,
            episode: 2,
            year: 0,
            view_count: 0,
            last_viewed_at: 1749750000,
            view_offset: 60000,
            guid: String::new(),
        };
        let (item, _) = row_to_item(&row).unwrap();
        assert_eq!(item.kind, "partial", "view_count=0 + view_offset>0 = partial");
        assert_eq!(item.detail, "S01E02");
        assert!(item.guid.starts_with("plex-5-"), "falls back to item_id-based guid: {}", item.guid);
        assert_eq!(item.extra.get("view_offset_ms"), Some(&Value::Number(60000.into())));
    }

    #[test]
    fn row_to_item_skips_empty_title() {
        let row = PlexRow {
            item_id: 99,
            metadata_type: 1,
            title: String::new(),
            show_title: String::new(),
            season: 0,
            episode: 0,
            year: 2020,
            view_count: 1,
            last_viewed_at: 1749578400,
            view_offset: 0,
            guid: String::new(),
        };
        assert!(row_to_item(&row).is_none(), "empty title must be skipped");
    }

    #[test]
    fn pull_db_writes_contract_raw_and_advances_cursor() {
        let dir = std::env::temp_dir()
            .join(format!("trove-plex-pull-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("plex.db");
        make_plex_db(&db_path);

        let v = temp_vault("pull");
        let n = pull_db(&v, &db_path).unwrap();
        assert_eq!(n, 4, "4 new plays on first run (movie + episode + track + partial)");

        // Contract layer: check rows
        let contract = v.stream(DIR, Partition::Month);
        let mut on_disk: Vec<MediaItem> = Vec::new();
        for key in contract.partitions().unwrap() {
            for item in contract.read::<MediaItem>(&key).unwrap() {
                on_disk.push(item);
            }
        }
        assert_eq!(on_disk.len(), 4, "4 contract rows on disk");

        let movie = on_disk.iter().find(|i| i.title == "The Matrix").unwrap();
        assert_eq!(movie.subtitle, "The Matrix (1999)");
        assert_eq!(movie.category, "video");
        assert_eq!(movie.source, "plex");

        let ep = on_disk.iter().find(|i| i.title == "Pilot").unwrap();
        assert_eq!(ep.subtitle, "Breaking Bad");
        assert_eq!(ep.detail, "S01E01");

        let track = on_disk.iter().find(|i| i.title == "Bohemian Rhapsody").unwrap();
        assert_eq!(track.category, "music");

        // Raw layer: full fidelity
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0usize;
        for key in raw_stream.partitions().unwrap() {
            for item in raw_stream.read::<Value>(&key).unwrap() {
                raw_count += 1;
                assert!(item.get("metadata_type").is_some(), "raw has metadata_type");
                assert!(item.get("last_viewed_at").is_some(), "raw has last_viewed_at");
            }
        }
        assert_eq!(raw_count, 4, "4 raw rows on disk");

        // Cursor advanced — keyed by guid (from metadata_item_views)
        let state = v.read_plex_sync();
        assert!(
            state.seen.contains_key("plex://movie/matrix"),
            "cursor has matrix movie guid"
        );
        assert_eq!(state.seen["plex://movie/matrix"], 1749578400);
        assert!(state.updated.is_some());
    }

    #[test]
    fn pull_db_idempotent_no_duplicates() {
        let dir = std::env::temp_dir()
            .join(format!("trove-plex-idem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("plex.db");
        make_plex_db(&db_path);

        let v = temp_vault("idem");
        pull_db(&v, &db_path).unwrap();

        let n2 = pull_db(&v, &db_path).unwrap();
        assert_eq!(n2, 0, "no new plays on re-run with same timestamps");

        let contract = v.stream(DIR, Partition::Month);
        let mut count = 0usize;
        for key in contract.partitions().unwrap() {
            count += contract.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(count, 4, "still 4 rows after re-run (no duplicates)");
    }

    #[test]
    fn pull_db_emits_new_row_when_last_viewed_at_advances() {
        let dir = std::env::temp_dir()
            .join(format!("trove-plex-advance-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("plex.db");
        make_plex_db(&db_path);

        let v = temp_vault("advance");
        let n1 = pull_db(&v, &db_path).unwrap();
        assert_eq!(n1, 4);

        // Simulate a rewatch of The Matrix: new metadata_item_views row with
        // an advanced viewed_at. The metadata_item_settings row is also updated
        // to reflect the new view_count (as Plex does on a real rewatch).
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            // 1749981600 = 2026-06-15T10:00:00Z
            conn.execute(
                "INSERT INTO metadata_item_views (id, account_id, guid, metadata_type, grandparent_title, parent_index, parent_title, \"index\", title, viewed_at)
                 VALUES (10, 1, 'plex://movie/matrix', 1, NULL, NULL, NULL, NULL, 'The Matrix', 1749981600)",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE metadata_item_settings SET last_viewed_at = 1749981600, view_count = 4 WHERE guid = 'plex://movie/matrix'",
                [],
            )
            .unwrap();
        }

        let n2 = pull_db(&v, &db_path).unwrap();
        assert_eq!(n2, 1, "one new play row when last_viewed_at advances");

        // Both watches present on disk
        let contract = v.stream(DIR, Partition::Month);
        let mut total = 0usize;
        let mut matrix_count = 0usize;
        for key in contract.partitions().unwrap() {
            for item in contract.read::<MediaItem>(&key).unwrap() {
                total += 1;
                if item.title == "The Matrix" {
                    matrix_count += 1;
                }
            }
        }
        assert_eq!(total, 5, "5 rows total after the rewatch");
        assert_eq!(matrix_count, 2, "The Matrix appears twice (two distinct timestamps)");
    }

    #[test]
    fn pull_db_no_op_when_db_absent() {
        let v = temp_vault("absent");
        let absent = PathBuf::from("/nonexistent/path/plex.db");
        let n = pull_db(&v, &absent).unwrap();
        assert_eq!(n, 0, "absent DB path is a silent no-op");
    }

    #[test]
    fn sync_state_back_compat() {
        // Empty cursor must deserialise cleanly.
        let old: SyncState = serde_json::from_str("{}").unwrap();
        assert!(old.seen.is_empty());
        assert!(old.updated.is_none());

        // Cursor with entries — keyed by guid strings.
        let s: SyncState = serde_json::from_str(
            r#"{"seen":{"plex://movie/matrix":1749578400,"plex://episode/pilot":1749672000},"updated":"2026-06-10T18:00:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!(s.seen.get("plex://movie/matrix").copied(), Some(1749578400));
        assert_eq!(s.seen.get("plex://episode/pilot").copied(), Some(1749672000));
        assert!(s.updated.is_some());
    }
}
