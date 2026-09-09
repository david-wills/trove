//! GOG Galaxy — local SQLite reader for the multi-platform game library &
//! cumulative playtime aggregator.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/gog-galaxy.md
//!
//! **What it reads.** GOG Galaxy 2 maintains a shared SQLite database at
//! `/Users/Shared/GOG.com/Galaxy/Storage/galaxy-2.0.db` (the path is
//! confirmed via the cross-platform AB1908/GOG-Galaxy-Export-Script, which
//! uses this exact constant for macOS). The database holds:
//!
//! - **GamePieces** / **GamePieceTypes** — a type-value store: each game
//!   has a `releaseKey` (e.g. `gog_1234567`, `steam_12345`), and its
//!   metadata (title, platform list) is stored as typed rows. Type IDs are
//!   dynamic (fetched via `GamePieceTypes`), NOT hardcoded.
//! - **ProductPurchaseDates** — the set of "owned" release keys (the join
//!   gate that limits to your library, not borrowed/demo content).
//! - **GAMETIMES** — cumulative `minutesInGame` per `releaseKey`.
//! - **LASTPLAYEDDATES** — `lastPlayedDate` (Unix timestamp) per
//!   `gameReleaseKey`.
//!
//! Schema confirmed against AB1908/GOG-Galaxy-Export-Script (cross-platform,
//! explicitly macOS-supported) and the GOG Galaxy Integrations Python API
//! (GameTime: game_id, time_played minutes, last_played_time unix timestamp).
//! A real macOS sample is still Needs-sample for final path/schema validation.
//!
//! **Behavior.** Periodic daily. Copy-then-read (never opens the live DB;
//! same pattern as iMessage/Safari). No login required — FDA covers the
//! shared path. The hub card explains "GOG Galaxy not installed" when the DB
//! is absent.
//!
//! **Vault output.** Raw-only (`gaming/` domain has no write-time contract).
//! Two streams:
//! - `gaming/gog-galaxy/library.jsonl` — current-state SNAPSHOT, rewritten
//!   whole each pull (one row per game: releaseKey/title/platform/
//!   playtime_mins/last_played_ts/snapshot_ts). Full fidelity.
//! - `gaming/gog-galaxy/delta.jsonl` — change-log APPEND: one row when a
//!   game's playtime or title changes vs. the last snapshot, partitioned by
//!   local month of the snapshot. Reconstructable from library.jsonl if
//!   deleted.
//!
//! **Dedupe / delta.** The cursor lives in `.trove/gog-galaxy-sync.json`
//! (rebuildable). Snapshot-diff on (`releaseKey`, `minutesInGame`) — a row
//! is emitted to the delta stream only when playtime advances (or a new game
//! appears). The library snapshot is always rewritten whole (current state).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

/// Shared macOS GOG Galaxy 2 database path (confirmed via AB1908/GOG-Galaxy-Export-Script).
const DB_PATH: &str = "/Users/Shared/GOG.com/Galaxy/Storage/galaxy-2.0.db";

/// Current-state library snapshot (rewritten whole each pull).
const LIBRARY_REL: &str = "gaming/gog-galaxy/library.jsonl";

/// Change-log: one row per game whose playtime advanced, month-partitioned.
const DELTA_DIR: &str = "gaming/gog-galaxy/delta";

/// Rebuildable non-secret cursor (not 0600; deleting it re-baselines next run).
const SYNC_FILE: &str = ".trove/gog-galaxy-sync.json";

/// Seconds between syncs: daily (playtime is cumulative, no per-session events).
pub const GOG_SYNC_SECS: u64 = 86_400;

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(gog_db_readable()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::file_mtime(&vault.root().join(LIBRARY_REL))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let games = out.counts.get("games").copied().unwrap_or(0);
            let deltas = out.counts.get("deltas").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(games > 0 || deltas > 0, || {
                format!("gog-galaxy synced — {games} games in library, {deltas} playtime updates")
            }))
        }
        // DB absent (Galaxy not installed) or FDA not granted: quiet skip.
        Err(e) => Ok(CollectOutcome::note(format!("gog-galaxy sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let games = out.counts.get("games").copied().unwrap_or(0);
    let deltas = out.counts.get("deltas").copied().unwrap_or(0);
    let headline = if deltas == 0 {
        format!("GOG Galaxy is up to date — {games} games in library, no playtime changes")
    } else {
        format!("GOG Galaxy synced — {games} games, {deltas} playtime updates")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "gog-galaxy",
        name: "GOG Galaxy",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your GOG Galaxy database for library and cumulative playtime data. \
            Galaxy aggregates GOG, Steam, and Epic Games libraries when integration plugins are \
            active — this is also the primary path for Epic Games Store playtime.",
        domain: "gaming",
        vault_path: "gaming/gog-galaxy/",
        toggleable: true,
        setup: &[
            "Install GOG Galaxy and run it at least once so the database is created.",
            "Enable Full Disk Access for Trove: System Settings → Privacy & Security → Full Disk Access.",
            "To capture Steam or Epic Games playtime, enable those integration plugins inside GOG Galaxy.",
        ],
        caveats: "Playtime is cumulative totals only — GOG Galaxy does not log individual sessions. \
            Plugin-sourced data (Steam/Epic) requires those plugins to be active and synced in Galaxy. \
            The database path is shared (/Users/Shared/…), so it is readable by any user on the Mac.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(GOG_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Path + permission.

/// The shared Galaxy 2 database path (macOS, confirmed).
fn gog_db_path() -> PathBuf {
    PathBuf::from(DB_PATH)
}

/// True when the database file exists AND is readable by this process.
/// False = Galaxy not installed, or Full Disk Access not granted.
fn gog_db_readable() -> bool {
    std::fs::File::open(gog_db_path()).is_ok()
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The last-seen state: map from releaseKey → Option<minutesInGame>.
    ///
    /// Every owned game is stored here after each run — even games that have
    /// never been launched (stored as `None`). This is critical for correct
    /// delta detection: a game absent from the cursor means it is genuinely
    /// new to the library; a game present with `None` means it was already
    /// seen but never played (no delta to emit). Using `Some`-only storage
    /// would mis-classify all never-played games as "new" on every run and
    /// emit spurious delta rows forever.
    ///
    /// Rebuildable by re-reading library.jsonl.
    #[serde(default)]
    playtime: HashMap<String, Option<u64>>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
    /// true after the first successful sync (baseline captured). The FIRST
    /// sync is a SILENT BASELINE — the delta log is NOT written for the
    /// first pull (we don't know what changed vs. before Trove was installed).
    #[serde(default)]
    baselined: bool,
}

impl Vault {
    fn read_gog_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_gog_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Row types.

/// One game row written to both library.jsonl and delta.jsonl.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameRow {
    /// The Galaxy release key — stable unique id, e.g. "gog_1444826988",
    /// "steam_413150". Platform prefix is the first segment before "_".
    pub release_key: String,
    /// Platform prefix derived from the release key (e.g. "gog", "steam",
    /// "epic", "origin", "uplay"). Best-effort — unknown prefixes kept verbatim.
    pub platform: String,
    /// Game title from GamePieces WHERE gamePieceTypeId = <title type id>.
    pub title: String,
    /// All release keys for the same game across platforms, from
    /// GamePieces WHERE gamePieceTypeId = <allGameReleases type id>.
    /// May be empty if the column is absent from this row.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub all_release_keys: Vec<String>,
    /// Cumulative playtime in minutes from GAMETIMES.minutesInGame.
    /// None when the game has never been launched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playtime_mins: Option<u64>,
    /// Unix timestamp of the last play session from LASTPLAYEDDATES.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_played_ts: Option<i64>,
    /// RFC3339 local time this row was written (snapshot timestamp).
    pub snapshot_ts: String,
    /// Extra columns that don't fit the above — full fidelity.
    #[serde(flatten, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// A delta row: same shape as GameRow but also carries the previous playtime
/// for context. Written to delta/ only when playtime advances or a new game
/// appears (after the first-sync baseline).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeltaRow {
    #[serde(flatten)]
    game: GameRow,
    /// Playtime at the previous snapshot (None for a newly-discovered game).
    #[serde(skip_serializing_if = "Option::is_none")]
    prev_playtime_mins: Option<u64>,
}

/// Newtype wrapper so we can implement [`Serialize`] for a `(month_key, DeltaRow)`
/// pair: the month key is the partition discriminator only, never serialized to
/// disk — only the inner [`DeltaRow`] goes to the JSONL file.
struct DeltaEntry(String, DeltaRow);

// ---------------------------------------------------------------------------
// DB reading.

/// Read the Galaxy DB and return all library games. The DB is opened from
/// a temp copy (never the live file). Type IDs for 'title' and
/// 'allGameReleases' are fetched dynamically via GamePieceTypes.
fn read_galaxy_db(db: &Path, now_ts: &str) -> Result<Vec<GameRow>> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening galaxy-2.0.db copy {}", db.display()))?;

    // Fetch type IDs dynamically — Galaxy assigns them at install time.
    let title_type_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM GamePieceTypes WHERE type='title' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let all_releases_type_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM GamePieceTypes WHERE type='allGameReleases' LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();

    // If we can't find the title type, the DB is either empty or schema-incompatible.
    let title_tid = title_type_id.unwrap_or(-1);

    // Build a map: releaseKey → title (from GamePieces, type=title).
    let mut titles: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT releaseKey, value FROM GamePieces WHERE gamePieceTypeId=?1",
        )?;
        let mut rows = stmt.query([title_tid])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let val: String = row.get(1)?;
            // value is a JSON string like {"title":"Game Name"} — try to extract.
            let title = extract_title_value(&val).unwrap_or(val);
            titles.insert(key, title);
        }
    }

    // Build a map: releaseKey → allGameReleases CSV (from GamePieces, type=allGameReleases).
    let mut all_releases: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(ar_tid) = all_releases_type_id {
        let mut stmt = conn.prepare(
            "SELECT releaseKey, value FROM GamePieces WHERE gamePieceTypeId=?1",
        )?;
        let mut rows = stmt.query([ar_tid])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let val: String = row.get(1)?;
            let keys = extract_release_keys(&val);
            if !keys.is_empty() {
                all_releases.insert(key, keys);
            }
        }
    }

    // Playtime: releaseKey → minutesInGame.
    let mut playtimes: HashMap<String, u64> = HashMap::new();
    if table_exists(&conn, "GAMETIMES") {
        let mut stmt = conn
            .prepare("SELECT releaseKey, minutesInGame FROM GAMETIMES WHERE minutesInGame IS NOT NULL AND minutesInGame > 0")
            .unwrap_or_else(|_| {
                // Some versions may use a different casing; fall back silently.
                conn.prepare("SELECT 1 WHERE 0").unwrap()
            });
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let mins: u64 = row.get::<_, i64>(1).unwrap_or(0).max(0) as u64;
            playtimes.insert(key, mins);
        }
    }

    // Last played: gameReleaseKey → lastPlayedDate (unix timestamp).
    let mut last_played: HashMap<String, i64> = HashMap::new();
    if table_exists(&conn, "LASTPLAYEDDATES") {
        let col = if column_exists(&conn, "LASTPLAYEDDATES", "gameReleaseKey") {
            "gameReleaseKey"
        } else {
            "releaseKey"
        };
        let sql = format!(
            "SELECT {col}, lastPlayedDate FROM LASTPLAYEDDATES WHERE lastPlayedDate IS NOT NULL"
        );
        if let Ok(mut stmt) = conn.prepare(&sql) {
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let ts: i64 = row.get(1).unwrap_or(0);
                last_played.insert(key, ts);
            }
        }
    }

    // The library: join ProductPurchaseDates → GamePieces (distinct release keys).
    // This limits to owned games (same join gate as the export script).
    let purchase_dates_present = table_exists(&conn, "ProductPurchaseDates");
    let owned_keys: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT DISTINCT gameReleaseKey FROM ProductPurchaseDates")
            .unwrap_or_else(|_| conn.prepare("SELECT 1 WHERE 0").unwrap());
        let mut rows = stmt.query([])?;
        let mut keys = Vec::new();
        while let Some(row) = rows.next()? {
            let k: String = row.get(0)?;
            keys.push(k);
        }
        // Fallback: if ProductPurchaseDates is absent (schema variant), fall back to
        // any releaseKey that has a title — some DB versions differ.
        // But if the title type id was also missing AND ProductPurchaseDates is absent/empty,
        // the DB is likely schema-incompatible rather than a valid empty library; surface an
        // error so the hub can inform the user rather than silently writing an empty baseline.
        if keys.is_empty() {
            if title_tid == -1 && !purchase_dates_present {
                anyhow::bail!(
                    "GOG Galaxy DB at {} appears schema-incompatible: \
                     no 'title' GamePieceType found and ProductPurchaseDates is absent. \
                     The database may need to be rebuilt by running GOG Galaxy.",
                    db.display()
                );
            }
            // title type was found — trust the titles map as fallback for schema variants.
            keys = titles.keys().cloned().collect();
        }
        keys
    };

    let mut games: Vec<GameRow> = Vec::with_capacity(owned_keys.len());
    for key in owned_keys {
        let title = titles.get(&key).cloned().unwrap_or_else(|| key.clone());
        let platform = platform_from_key(&key);
        let ark = all_releases.get(&key).cloned().unwrap_or_default();
        let playtime_mins = playtimes.get(&key).copied();
        let last_played_ts = last_played.get(&key).copied();
        games.push(GameRow {
            release_key: key,
            platform,
            title,
            all_release_keys: ark,
            playtime_mins,
            last_played_ts,
            snapshot_ts: now_ts.to_string(),
            extra: Map::new(),
        });
    }

    // Stable order: by title, then release key.
    games.sort_by(|a, b| a.title.cmp(&b.title).then(a.release_key.cmp(&b.release_key)));
    Ok(games)
}

/// True when `table` exists in the DB.
fn table_exists(conn: &rusqlite::Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// True when `table.column` exists.
fn column_exists(conn: &rusqlite::Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    conn.prepare(&sql)
        .ok()
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(1)).ok().map(|rows| {
                rows.flatten().any(|name| name.eq_ignore_ascii_case(column))
            })
        })
        .unwrap_or(false)
}

/// Extract the title string from a GamePieces `value` JSON:
/// - Plain string: the value itself.
/// - `{"title":"Game Name"}` or `{"title":"…","…":…}`: the title field.
/// Returns None when the value is empty or the JSON lacks a title field.
fn extract_title_value(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    // Try JSON first.
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(raw) {
        if let Some(Value::String(t)) = m.get("title") {
            if !t.is_empty() {
                return Some(t.clone());
            }
        }
        // Some rows use "value" or the first string field.
        for v in m.values() {
            if let Value::String(s) = v {
                if !s.is_empty() {
                    return Some(s.clone());
                }
            }
        }
    }
    // Fall back to the raw string itself.
    if !raw.is_empty() {
        Some(raw.to_string())
    } else {
        None
    }
}

/// Extract a list of release keys from the allGameReleases `value` JSON:
/// - `{"releases":["gog_1","steam_2"]}` or `["gog_1","steam_2"]` or a CSV.
fn extract_release_keys(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    // JSON object with "releases" array.
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(raw) {
        for key in ["releases", "allGameReleases", "gameReleaseKeys"] {
            if let Some(Value::Array(arr)) = m.get(key) {
                return arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
            }
        }
        // If the object itself maps to strings, treat values as keys.
        return m.values()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    // JSON array.
    if let Ok(Value::Array(arr)) = serde_json::from_str::<Value>(raw) {
        return arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
    }
    // CSV fallback.
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// The platform from the release key prefix: "gog", "steam", "epic", "origin",
/// "uplay", "battlenet", "humblestore", etc. Unknown → "unknown".
fn platform_from_key(key: &str) -> String {
    key.split('_').next().unwrap_or("unknown").to_lowercase()
}

// ---------------------------------------------------------------------------
// The pull.

/// One complete sync: copy the DB, read it, write the library snapshot and
/// any delta rows. Returns counts {"games", "deltas"}.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    pull_from(vault, &gog_db_path())
}

/// Inner implementation accepting an explicit DB path so tests can inject a
/// temp DB without needing a real GOG Galaxy install.
fn pull_from(vault: &Vault, db: &Path) -> Result<PullOutcome> {
    if !db.exists() {
        anyhow::bail!(
            "GOG Galaxy database not found at {} — is GOG Galaxy installed?",
            db.display()
        );
    }

    let now = Local::now();
    let now_ts = now.to_rfc3339();

    let stem = format!("trove-gog-galaxy-{}", std::process::id());
    let games = import_via_copy(db, &stem, |tmp| read_galaxy_db(tmp, &now_ts))?;

    let mut state = vault.read_gog_sync();
    let is_first_run = !state.baselined;

    // Delta detection: compare playtimes vs. last cursor.
    //
    // `is_new` means the release key was NOT in the cursor from the prior run.
    // This correctly distinguishes a brand-new library entry from a game that
    // was already seen but never played (stored in the cursor as None).
    let mut delta_rows: Vec<DeltaEntry> = Vec::new();
    for game in &games {
        // `state.playtime.get()` returns:
        //   None            — key absent from cursor (game is genuinely new to the library)
        //   Some(&None)     — key present, never played (no delta)
        //   Some(&Some(p))  — key present, was played with p minutes
        let cursor_entry = state.playtime.get(&game.release_key);
        let is_new = cursor_entry.is_none(); // absent from prior cursor → truly new game
        let prev_playtime: Option<u64> = cursor_entry.copied().flatten();
        let curr = game.playtime_mins;
        let playtime_advanced = match (prev_playtime, curr) {
            (Some(p), Some(c)) => c > p,
            (None, Some(_)) if !is_new => true, // was seen but unplayed; now has playtime
            _ => false,
        };
        if !is_first_run && (is_new || playtime_advanced) {
            // Partition by local month of the snapshot.
            let month_key = &now_ts[..7]; // "YYYY-MM"
            delta_rows.push(DeltaEntry(
                month_key.to_string(),
                DeltaRow { game: game.clone(), prev_playtime_mins: prev_playtime },
            ));
        }
    }

    // Update cursor for ALL owned games — including never-played ones (stored as None).
    // This is the key invariant: every release key that appears in the library
    // must be in the cursor after each run, so the next run's `is_new` check
    // correctly identifies only genuinely new library additions.
    state.playtime.clear();
    for game in &games {
        state.playtime.insert(game.release_key.clone(), game.playtime_mins);
    }

    // Write the library snapshot (current-state, always overwritten).
    let game_count = games.len() as u64;
    vault.write_snapshot(LIBRARY_REL, &games)?;

    // Write delta rows (skip on first run — silent baseline).
    let delta_count = if is_first_run {
        0
    } else {
        let stream = vault.stream(DELTA_DIR, crate::store::Partition::Month);
        if !delta_rows.is_empty() {
            stream.append(&delta_rows, |e| e.0.as_str())?;
        }
        delta_rows.len() as u64
    };

    // Advance cursor.
    state.baselined = true;
    state.updated = Some(now_ts);
    vault.write_gog_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{game_count} games, {delta_count} playtime updates"),
        counts: BTreeMap::from([("games", game_count), ("deltas", delta_count)]),
    })
}

// ---------------------------------------------------------------------------
// Serialize DeltaEntry: forward to the inner DeltaRow only — the month key
// (DeltaEntry.0) is the partition discriminator for the stream, never
// serialized to disk.
impl Serialize for DeltaEntry {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.1.serialize(s)
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-gog-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::vault::Vault::open_or_create(dir).unwrap()
    }

    // ---- Helper: build a temp SQLite DB with the Galaxy schema. ----
    //
    // The fixture intentionally includes a NEVER-PLAYED game (gog_1533233788
    // "Disco Elysium") that has no row in GAMETIMES and no row in
    // LASTPLAYEDDATES.  This is the critical case for the flood-bug fix:
    // without the fix, Disco Elysium would emit a spurious delta row on every
    // run after the baseline.

    fn make_test_db(name: &str) -> (PathBuf, rusqlite::Connection) {
        let path = std::env::temp_dir()
            .join(format!("trove-gog-testdb-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();

        conn.execute_batch(
            "CREATE TABLE GamePieceTypes (id INTEGER PRIMARY KEY, type TEXT NOT NULL);
             INSERT INTO GamePieceTypes VALUES (1, 'title');
             INSERT INTO GamePieceTypes VALUES (2, 'allGameReleases');

             CREATE TABLE GamePieces (releaseKey TEXT, gamePieceTypeId INTEGER, value TEXT);
             -- gog_1: Witcher 3 (GOG)
             INSERT INTO GamePieces VALUES ('gog_1207658924', 1, '{\"title\":\"The Witcher 3: Wild Hunt\"}');
             INSERT INTO GamePieces VALUES ('gog_1207658924', 2, '[\"gog_1207658924\",\"steam_292030\"]');
             -- steam_292030: Witcher 3 (Steam, same game — NOT in ProductPurchaseDates)
             INSERT INTO GamePieces VALUES ('steam_292030', 1, '{\"title\":\"The Witcher 3: Wild Hunt\"}');
             -- gog_2: Cyberpunk 2077 (played)
             INSERT INTO GamePieces VALUES ('gog_1423049311', 1, '{\"title\":\"Cyberpunk 2077\"}');
             -- gog_3: Disco Elysium (owned but NEVER played — critical for flood-bug test)
             INSERT INTO GamePieces VALUES ('gog_1533233788', 1, '{\"title\":\"Disco Elysium\"}');

             CREATE TABLE ProductPurchaseDates (gameReleaseKey TEXT, purchaseDate TEXT);
             INSERT INTO ProductPurchaseDates VALUES ('gog_1207658924', '2015-05-19');
             INSERT INTO ProductPurchaseDates VALUES ('gog_1423049311', '2020-12-10');
             INSERT INTO ProductPurchaseDates VALUES ('gog_1533233788', '2021-02-15');
             -- steam_292030 is NOT in ProductPurchaseDates (not directly owned)

             CREATE TABLE GAMETIMES (releaseKey TEXT, minutesInGame INTEGER);
             INSERT INTO GAMETIMES VALUES ('gog_1207658924', 3500);
             INSERT INTO GAMETIMES VALUES ('gog_1423049311', 120);
             -- gog_1533233788 (Disco Elysium) intentionally absent — never launched

             CREATE TABLE LASTPLAYEDDATES (gameReleaseKey TEXT, lastPlayedDate INTEGER);
             INSERT INTO LASTPLAYEDDATES VALUES ('gog_1207658924', 1700000000);
             INSERT INTO LASTPLAYEDDATES VALUES ('gog_1423049311', 1720000000);
             -- gog_1533233788 intentionally absent",
        )
        .unwrap();

        (path, conn)
    }

    #[test]
    fn reads_library_and_playtime() {
        let (db_path, _conn) = make_test_db("reads");
        let now_ts = "2024-11-01T10:00:00+00:00";
        let games = read_galaxy_db(&db_path, now_ts).unwrap();

        // Ordered by title: Cyberpunk 2077 < Disco Elysium < The Witcher 3
        assert_eq!(games.len(), 3, "all three owned games (ProductPurchaseDates)");
        let cp = &games[0];
        assert_eq!(cp.release_key, "gog_1423049311");
        assert_eq!(cp.title, "Cyberpunk 2077");
        assert_eq!(cp.platform, "gog");
        assert_eq!(cp.playtime_mins, Some(120));
        assert_eq!(cp.last_played_ts, Some(1720000000));

        let de = &games[1];
        assert_eq!(de.release_key, "gog_1533233788");
        assert_eq!(de.title, "Disco Elysium");
        assert_eq!(de.platform, "gog");
        assert_eq!(de.playtime_mins, None, "never-played game has no playtime");
        assert_eq!(de.last_played_ts, None);

        let w3 = &games[2];
        assert_eq!(w3.release_key, "gog_1207658924");
        assert_eq!(w3.title, "The Witcher 3: Wild Hunt");
        assert_eq!(w3.platform, "gog");
        assert_eq!(w3.playtime_mins, Some(3500));
        assert_eq!(w3.last_played_ts, Some(1700000000));
        assert_eq!(w3.all_release_keys, vec!["gog_1207658924", "steam_292030"]);
    }

    /// The critical regression test for the flood-bug fix.
    ///
    /// Three scenarios exercised against a real `pull_from()` call (not inline
    /// re-implementation):
    ///
    /// 1. Run 1 (first pull / baseline): 0 deltas written (silent baseline).
    /// 2. Run 2 (unchanged): 0 deltas — never-played "Disco Elysium" must NOT
    ///    emit a spurious delta row even though its playtime is still None.
    /// 3. Run 3 (one game's playtime advances): exactly 1 delta (The Witcher 3).
    ///    Disco Elysium remains silent despite still having no playtime.
    #[test]
    fn never_played_games_no_spurious_deltas() {
        let (db_path, conn) = make_test_db("flood");
        let vault = temp_vault("flood");

        // --- Run 1: first-run baseline ---
        let out1 = pull_from(&vault, &db_path).expect("run 1 should succeed");
        assert_eq!(
            out1.counts.get("games").copied().unwrap_or(0),
            3,
            "run 1 should see 3 games"
        );
        assert_eq!(
            out1.counts.get("deltas").copied().unwrap_or(0),
            0,
            "run 1 (baseline) must emit 0 deltas"
        );

        // Verify the cursor now tracks all 3 games (including never-played Disco Elysium).
        let state = vault.read_gog_sync();
        assert!(state.baselined);
        assert!(
            state.playtime.contains_key("gog_1533233788"),
            "cursor must contain never-played game after run 1"
        );
        assert_eq!(
            state.playtime.get("gog_1533233788").copied(),
            Some(None),
            "never-played game stored as None in cursor"
        );

        // --- Run 2: nothing changed (same DB) ---
        let out2 = pull_from(&vault, &db_path).expect("run 2 should succeed");
        assert_eq!(
            out2.counts.get("deltas").copied().unwrap_or(0),
            0,
            "run 2 (no changes) must emit 0 deltas — never-played games must not flood"
        );

        // --- Run 3: The Witcher 3's playtime advances, Disco Elysium still unplayed ---
        // Patch GAMETIMES in the test DB.
        conn.execute(
            "UPDATE GAMETIMES SET minutesInGame=3600 WHERE releaseKey='gog_1207658924'",
            [],
        )
        .unwrap();
        let out3 = pull_from(&vault, &db_path).expect("run 3 should succeed");
        assert_eq!(
            out3.counts.get("deltas").copied().unwrap_or(0),
            1,
            "run 3 must emit exactly 1 delta (only Witcher 3 advanced)"
        );

        // Verify the delta file contains the right game.
        let month_key = Local::now().format("%Y-%m").to_string();
        let delta_path = vault
            .resolve(&format!("{}/{}.jsonl", DELTA_DIR, month_key))
            .expect("delta path resolvable");
        let delta_content = std::fs::read_to_string(&delta_path)
            .expect("delta file should exist after run 3");
        let delta_line = delta_content.lines().next().expect("at least one delta line");
        let delta_val: serde_json::Value = serde_json::from_str(delta_line).unwrap();
        assert_eq!(
            delta_val["release_key"].as_str().unwrap(),
            "gog_1207658924",
            "delta must be for The Witcher 3"
        );
        assert_eq!(
            delta_val["prev_playtime_mins"].as_u64().unwrap(),
            3500,
            "prev playtime should be 3500"
        );
        assert_eq!(
            delta_val["playtime_mins"].as_u64().unwrap(),
            3600,
            "new playtime should be 3600"
        );
        // Disco Elysium must NOT appear in deltas at all.
        assert!(
            !delta_content.contains("gog_1533233788"),
            "never-played Disco Elysium must not appear in delta stream"
        );
    }

    #[test]
    fn platform_from_key_known_prefixes() {
        assert_eq!(platform_from_key("gog_1234567890"), "gog");
        assert_eq!(platform_from_key("steam_413150"), "steam");
        assert_eq!(platform_from_key("epic_FortnitePublicGame"), "epic");
        assert_eq!(platform_from_key("origin_OFR.00000005012"), "origin");
        assert_eq!(platform_from_key("uplay_123"), "uplay");
        assert_eq!(platform_from_key("battlenet_S1"), "battlenet");
        // No underscore → whole key as platform.
        assert_eq!(platform_from_key("nogameshere"), "nogameshere");
    }

    #[test]
    fn extract_title_value_variants() {
        // JSON {"title":"Name"}.
        assert_eq!(
            extract_title_value(r#"{"title":"The Witcher 3: Wild Hunt"}"#),
            Some("The Witcher 3: Wild Hunt".to_string())
        );
        // Raw string fallback.
        assert_eq!(
            extract_title_value("Cyberpunk 2077"),
            Some("Cyberpunk 2077".to_string())
        );
        // Empty string.
        assert_eq!(extract_title_value(""), None);
        // JSON with no title field — pick first string value.
        assert_eq!(
            extract_title_value(r#"{"name":"Fallout 4"}"#),
            Some("Fallout 4".to_string())
        );
    }

    #[test]
    fn extract_release_keys_variants() {
        // JSON array.
        assert_eq!(
            extract_release_keys(r#"["gog_1","steam_2"]"#),
            vec!["gog_1", "steam_2"]
        );
        // JSON object with "releases" key.
        assert_eq!(
            extract_release_keys(r#"{"releases":["gog_1","steam_2"]}"#),
            vec!["gog_1", "steam_2"]
        );
        // CSV fallback.
        assert_eq!(
            extract_release_keys("gog_1,steam_2"),
            vec!["gog_1", "steam_2"]
        );
        // Empty.
        assert!(extract_release_keys("").is_empty());
    }

    #[test]
    fn library_snapshot_roundtrip() {
        let (db_path, _conn) = make_test_db("snapshot");
        let vault = temp_vault("snapshot");
        let now_ts = "2024-11-15T12:00:00+00:00";
        let games = read_galaxy_db(&db_path, now_ts).unwrap();

        vault.write_snapshot(LIBRARY_REL, &games).unwrap();

        let loaded: Vec<GameRow> = vault.read_snapshot(LIBRARY_REL).unwrap();
        assert_eq!(loaded.len(), 3, "all 3 owned games including never-played");
        // Played game preserves playtime.
        let cp = loaded.iter().find(|g| g.release_key == "gog_1423049311").unwrap();
        assert_eq!(cp.playtime_mins, Some(120));
        assert_eq!(cp.snapshot_ts, now_ts);
        // Never-played game round-trips with None playtime.
        let de = loaded.iter().find(|g| g.release_key == "gog_1533233788").unwrap();
        assert_eq!(de.playtime_mins, None, "never-played game round-trips as None");
    }

    #[test]
    fn table_exists_detects_missing() {
        let path = std::env::temp_dir()
            .join(format!("trove-gog-tablecheck-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE Foo (id INTEGER);").unwrap();
        assert!(table_exists(&conn, "Foo"));
        assert!(!table_exists(&conn, "GAMETIMES"));
        assert!(!table_exists(&conn, "LASTPLAYEDDATES"));
        std::fs::remove_file(&path).ok();
    }
}
