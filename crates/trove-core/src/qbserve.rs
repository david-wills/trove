//! Qbserve — automatic Mac app & website time tracker (local SQLite reader).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/qbserve.md
//!
//! ## What this does
//!
//! Reads Qbserve's local SQLite database (`~/Library/Application Support/Qbserve/Backup.sqlite`
//! or a fresh copy of `UserDatabase.sqlite`) and writes raw usage-span rows to
//! `activity/qbserve/YYYY-MM.jsonl`.
//!
//! ## Schema (confirmed via adamfortuna/qbserve_to_exist and Avery2/Qbserve-Blocker)
//!
//! History is stored in month-partitioned tables: `z_HistoryLog_YYYY_M`
//! (year + month without zero-pad, e.g. `z_HistoryLog_2022_5`). The
//! `HistoryTablesIndex` table lists all active partition table names.
//!
//! ```sql
//! -- History partitions
//! z_HistoryLog_YYYY_M(_id INTEGER PRIMARY KEY,
//!                     activity_id INTEGER,   -- → Activities._id (NOT Apps._id)
//!                     start_time INTEGER,    -- Unix epoch seconds
//!                     duration INTEGER,      -- seconds
//!                     project_id INTEGER)
//!
//! -- Activity lookup (intermediate — may wrap an app or a website/window)
//! Activities(_id INTEGER PRIMARY KEY, title TEXT, category_id INTEGER,
//!            app_id INTEGER -- optional; links to Apps._id when non-null)
//!
//! -- App lookup (optional hop from Activities)
//! Apps(_id INTEGER PRIMARY KEY, bundle TEXT, localized_name TEXT,
//!      is_ignored INTEGER,
//!      track_windows INTEGER, show_windows_as_activities INTEGER,
//!      latest_use INTEGER)
//!
//! -- Productivity category lookup
//! -- `productivity`: -1 = distracting, 0 = neutral, 1 = productive
//! Categories(_id INTEGER PRIMARY KEY, productivity INTEGER)
//!
//! -- Which z_HistoryLog_* tables exist
//! HistoryTablesIndex(table_name TEXT)
//! ```
//!
//! Join chain (verified against two independent community projects):
//!   `z_HistoryLog.activity_id → Activities._id`  (title, category_id, app_id)
//!   `Activities.category_id  → Categories._id`   (productivity int → label)
//!   `Activities.app_id       → Apps._id`          (bundle, localized_name — best-effort)
//!
//! Note: Activities.app_id existence is inferred from community sources; a full
//! `.schema` dump is the definitive reference (Needs-sample flag still applies).
//!
//! ## Deduplication / watermark
//!
//! Cursor (`.trove/qbserve-sync.json`) holds `last_rowid` — the highest
//! `z_HistoryLog_*._id` imported (per-partition max, tracked globally).
//! Each sync reads all partitions for rows with `_id > last_rowid`. On a
//! crash the re-drain is safe because rows with old rowids are skipped.
//! Cursor advances only after all rows are written.
//!
//! ## Lock avoidance
//!
//! Qbserve holds a write lock on `UserDatabase.sqlite` while running. We
//! prefer `Backup.sqlite` (written daily by Qbserve itself); if absent we
//! copy the live DB to a temp file via [`crate::browser::import_via_copy`].
//!
//! ## guid
//!
//! FNV-1a 64-bit hash over `"qbserve:<rowid>"` encoded as 16-char lowercase
//! hex. Row IDs across partitions are unique within the DB so this is stable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind, PermissionInfo};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::write_json_atomic;
use crate::vault::Vault;

/// Where raw rows land.
const DIR: &str = "activity/qbserve";
/// Non-secret cursor file.
const SYNC_FILE: &str = ".trove/qbserve-sync.json";
/// Seconds between syncs (~1 hour; data only changes as often as the daily Backup.sqlite).
pub const QBSERVE_SYNC_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// DB paths.

fn db_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/Application Support/Qbserve"))
}

fn backup_db_path() -> Option<PathBuf> {
    db_dir().map(|d| d.join("Backup.sqlite"))
}

fn live_db_path() -> Option<PathBuf> {
    db_dir().map(|d| d.join("UserDatabase.sqlite"))
}

/// True when the Qbserve data directory exists and at least one DB is readable.
fn qbserve_available() -> bool {
    backup_db_path()
        .is_some_and(|p| p.exists())
        || live_db_path().is_some_and(|p| std::fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_permission() -> PermissionInfo {
    PermissionInfo {
        kind: "local-app-data",
        granted: Some(qbserve_available()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let r = out.counts.get("rows").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(r > 0, || {
                format!("qbserve synced — {r} activity rows")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "qbserve sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let r = out.counts.get("rows").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Qbserve synced — {r} activity rows"),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "qbserve",
        name: "Qbserve",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Backfills your Qbserve app-usage history from its local SQLite database — \
                      which apps and websites you spent time on, with productivity categories. \
                      For existing Qbserve users: Trove's own activity watcher captures similar \
                      data going forward; this integration imports years of existing history.",
        domain: "activity",
        vault_path: "activity/qbserve/",
        toggleable: true,
        setup: &[
            "Qbserve must be installed (one-time purchase from qotoqot.com/qbserve).",
            "No configuration needed — Trove reads the local database automatically.",
            "If Qbserve is running, its daily Backup.sqlite is used to avoid lock conflicts.",
        ],
        caveats: "Requires Qbserve to be installed. URL tracking is only available if \
                  you installed the Qbserve browser extension for Firefox, Vivaldi, Opera, \
                  or Yandex (not Chrome or Safari). This integration is most valuable for \
                  historical backfill; Trove's activity watcher covers ongoing tracking.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(QBSERVE_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Highest imported row _id across all z_HistoryLog_* partitions.
    /// None = first sync (baseline — import everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_rowid: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_qbserve_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_qbserve_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// On-disk row shape.

/// One raw span row written to `activity/qbserve/YYYY-MM.jsonl`.
#[derive(Debug, Serialize, Deserialize)]
struct SpanRow {
    /// Stable dedupe key: FNV-1a 64-bit hex over "qbserve:<rowid>".
    guid: String,
    /// RFC3339 local timestamp (start of span).
    ts: String,
    /// Duration in seconds.
    duration_secs: i64,
    /// Source DB row id (for watermark tracking).
    rowid: i64,
    /// App bundle identifier (e.g. "com.apple.Safari").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    bundle_id: String,
    /// Localized app name (e.g. "Safari").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    app: String,
    /// Productivity label derived from `Categories.productivity` int:
    /// -1 → "Distracting", 0 → "Neutral", 1 → "Productive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    /// project_id from the DB (raw, for full fidelity; may be 0 or NULL → -1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_id: Option<i64>,
    /// Any extra columns present in the DB beyond the known schema (forward-compat).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    extra: Map<String, Value>,
}

/// FNV-1a 64-bit hash over a string, encoded as 16-char lowercase hex.
fn fnv64(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

fn row_guid(rowid: i64) -> String {
    fnv64(&format!("qbserve:{rowid}"))
}

/// Unix epoch seconds → RFC3339 local time string. Falls back to epoch on
/// conversion failure (never errors — the write-set check below gates zero-ts rows).
fn unix_to_local(secs: i64) -> String {
    DateTime::from_timestamp(secs, 0)
        .map(|utc| utc.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string())
}

// ---------------------------------------------------------------------------
// SQLite import (testable).

/// Read and import rows from a DB at `db_path` with rowids > `cursor`.
/// Returns (rows written, new max rowid).
fn import_db(vault: &Vault, db_path: &Path, cursor: i64) -> Result<(u64, i64)> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening Qbserve DB at {}", db_path.display()))?;

    // Optionally set busy timeout to avoid SQLITE_BUSY on the live DB.
    let _ = conn.busy_timeout(Duration::from_secs(2));

    // --- Build category lookup: Categories._id → productivity label ---
    // Real schema: Categories.productivity is -1/0/1 (NOT a title text column).
    // Verified against adamfortuna/qbserve_to_exist index.js and Avery2/Qbserve-Blocker.
    let mut cats: BTreeMap<i64, String> = BTreeMap::new();
    // Categories table may not exist in all versions — degrade gracefully.
    if let Ok(mut stmt) = conn.prepare("SELECT _id, productivity FROM Categories") {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let id: i64 = row.get(0).unwrap_or(0);
                let productivity: i64 = row.get(1).unwrap_or(0);
                let label = match productivity {
                    1 => "Productive",
                    -1 => "Distracting",
                    _ => "Neutral",
                };
                cats.insert(id, label.to_string());
            }
        }
    }

    // --- Build Activities lookup: Activities._id → (title, category_id, app_id?) ---
    // Correct join chain: z_HistoryLog.activity_id → Activities._id (NOT Apps._id).
    // Activities.category_id → Categories._id for the productivity label.
    // Activities.app_id → Apps._id for bundle/name (best-effort; column may not exist).
    struct ActivityInfo {
        title: String,
        category_id: i64,
        app_id: Option<i64>,
    }
    let mut activities: BTreeMap<i64, ActivityInfo> = BTreeMap::new();
    // Try with app_id first; fall back to without if the column is absent.
    let activities_sql_with_app =
        "SELECT _id, COALESCE(title,''), COALESCE(category_id,-1), app_id FROM Activities";
    let activities_sql_no_app =
        "SELECT _id, COALESCE(title,''), COALESCE(category_id,-1) FROM Activities";
    let has_app_id = conn.prepare(activities_sql_with_app).is_ok();
    if has_app_id {
        let mut stmt = conn
            .prepare(activities_sql_with_app)
            .context("preparing Activities query (with app_id)")?;
        let mut rows = stmt.query([]).context("querying Activities")?;
        while let Ok(Some(row)) = rows.next() {
            let id: i64 = row.get(0).unwrap_or(0);
            let title: String = row.get(1).unwrap_or_default();
            let cat_id: i64 = row.get(2).unwrap_or(-1);
            let app_id: Option<i64> = row.get(3).ok().filter(|&v: &i64| v > 0);
            activities.insert(id, ActivityInfo { title, category_id: cat_id, app_id });
        }
    } else {
        let mut stmt = conn
            .prepare(activities_sql_no_app)
            .context("preparing Activities query (no app_id)")?;
        let mut rows = stmt.query([]).context("querying Activities")?;
        while let Ok(Some(row)) = rows.next() {
            let id: i64 = row.get(0).unwrap_or(0);
            let title: String = row.get(1).unwrap_or_default();
            let cat_id: i64 = row.get(2).unwrap_or(-1);
            activities.insert(id, ActivityInfo { title, category_id: cat_id, app_id: None });
        }
    }

    // --- Build Apps lookup: Apps._id → (bundle, localized_name) ---
    // Only used as a secondary hop from Activities.app_id when available.
    let mut apps: BTreeMap<i64, (String, String)> = BTreeMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT _id, COALESCE(bundle,''), COALESCE(localized_name,'') FROM Apps")
    {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let id: i64 = row.get(0).unwrap_or(0);
                let bundle: String = row.get(1).unwrap_or_default();
                let name: String = row.get(2).unwrap_or_default();
                apps.insert(id, (bundle, name));
            }
        }
    }

    // --- Enumerate history partition tables ---
    // HistoryTablesIndex holds the table names; fall back to sqlite_master scan.
    let mut table_names: Vec<String> = Vec::new();
    {
        // Try HistoryTablesIndex first.
        let idx_ok = conn
            .prepare("SELECT table_name FROM HistoryTablesIndex")
            .and_then(|mut stmt| {
                let mut rows = stmt.query([])?;
                while let Some(row) = rows.next()? {
                    let name: String = row.get(0)?;
                    table_names.push(name);
                }
                Ok(())
            });

        if idx_ok.is_err() || table_names.is_empty() {
            // Fallback: scan sqlite_master for z_HistoryLog_* tables.
            table_names.clear();
            let mut stmt = conn
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type='table' \
                     AND name LIKE 'z_HistoryLog_%' ORDER BY name",
                )
                .context("scanning sqlite_master for history tables")?;
            let mut rows = stmt.query([]).context("querying sqlite_master")?;
            while let Some(row) = rows.next().context("iterating sqlite_master")? {
                let name: String = row.get(0)?;
                table_names.push(name);
            }
        }
    }

    if table_names.is_empty() {
        // No history yet (fresh install or empty backup).
        return Ok((0, cursor));
    }

    // --- Read rows from each partition, collect all into a vec ---
    let mut all_rows: Vec<SpanRow> = Vec::new();
    let mut new_max_rowid: i64 = cursor;

    for table in &table_names {
        // Validate table name: must match z_HistoryLog_DDDD_D[D] to prevent injection.
        if !is_safe_table_name(table) {
            continue;
        }

        let sql = format!(
            "SELECT h._id, h.activity_id, h.start_time, h.duration, \
                    COALESCE(h.project_id, -1) \
             FROM {table} h \
             WHERE h._id > ?1 AND h.duration > 0 \
             ORDER BY h._id"
        );

        let stmt_result = conn.prepare(&sql);
        let mut stmt = match stmt_result {
            Ok(s) => s,
            Err(_) => continue, // Table may not match schema — skip gracefully.
        };

        let mut rows = match stmt.query([cursor]) {
            Ok(r) => r,
            Err(_) => continue,
        };

        while let Ok(Some(row)) = rows.next() {
            let rowid: i64 = row.get(0).unwrap_or(0);
            let activity_id: i64 = row.get(1).unwrap_or(0);
            let start_time: i64 = row.get(2).unwrap_or(0);
            let duration: i64 = row.get(3).unwrap_or(0);
            let project_id: i64 = row.get(4).unwrap_or(-1);

            if start_time <= 0 || duration <= 0 {
                continue;
            }

            let ts = unix_to_local(start_time);

            // Correct join: activity_id → Activities._id → title/category_id/app_id
            let (activity_title, category, bundle_id, app_name) =
                if let Some(act) = activities.get(&activity_id) {
                    let cat = if act.category_id >= 0 {
                        cats.get(&act.category_id).cloned()
                    } else {
                        None
                    };
                    // Optional second hop: Activities.app_id → Apps._id
                    let (bundle, name) = act
                        .app_id
                        .and_then(|aid| apps.get(&aid).cloned())
                        .unwrap_or_default();
                    (act.title.clone(), cat, bundle, name)
                } else {
                    (String::new(), None, String::new(), String::new())
                };

            // `app` field: prefer the Apps.localized_name when available; fall back
            // to the Activities.title (which is also the display name for non-app
            // activities like websites tracked via the browser extension).
            let app_display = if !app_name.is_empty() {
                app_name
            } else {
                activity_title
            };

            new_max_rowid = new_max_rowid.max(rowid);

            all_rows.push(SpanRow {
                guid: row_guid(rowid),
                ts,
                duration_secs: duration,
                rowid,
                bundle_id,
                app: app_display,
                category,
                project_id: if project_id >= 0 { Some(project_id) } else { None },
                extra: Map::new(),
            });
        }
    }

    if all_rows.is_empty() {
        return Ok((0, cursor));
    }

    // --- Write rows grouped by month partition ---
    let written = upsert_rows(vault, all_rows)?;
    Ok((written, new_max_rowid))
}

/// Validate that a table name is a safe `z_HistoryLog_YYYY_M[M]` string.
/// Prevents SQL injection from a tampered DB.
fn is_safe_table_name(name: &str) -> bool {
    // Pattern: z_HistoryLog_<4digits>_<1or2digits>
    let Some(rest) = name.strip_prefix("z_HistoryLog_") else {
        return false;
    };
    let parts: Vec<&str> = rest.splitn(2, '_').collect();
    if parts.len() != 2 {
        return false;
    }
    parts[0].len() == 4
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && (1..=2).contains(&parts[1].len())
        && parts[1].chars().all(|c| c.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Upsert rows into `activity/qbserve/YYYY-MM.jsonl` keyed by guid.
fn upsert_rows(vault: &Vault, rows: Vec<SpanRow>) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }

    // Group by month partition key extracted from the ts field.
    let mut by_month: BTreeMap<String, Vec<SpanRow>> = BTreeMap::new();
    for row in rows {
        // ts is RFC3339, e.g. "2022-05-01T10:30:00+07:00" — month = first 7 chars.
        let month_key = if row.ts.len() >= 7 {
            row.ts[..7].to_string()
        } else {
            continue; // malformed ts — skip
        };
        by_month.entry(month_key).or_default().push(row);
    }

    let mut written: u64 = 0;

    for (month_key, new_rows) in by_month {
        let path_str = format!("{DIR}/{month_key}.jsonl");
        let path = vault.resolve(&path_str)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating qbserve dir")?;
        }

        // Read existing rows for this month.
        let mut existing: Vec<SpanRow> = if path.exists() {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<SpanRow>(l).ok())
                .collect()
        } else {
            Vec::new()
        };

        // Build guid → index map for upsert.
        let mut guid_idx: BTreeMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (r.guid.clone(), i)).collect();

        let mut appended: u64 = 0;
        for row in new_rows {
            if let Some(&idx) = guid_idx.get(&row.guid) {
                existing[idx] = row; // replace in place (idempotent re-import)
            } else {
                guid_idx.insert(row.guid.clone(), existing.len());
                existing.push(row);
                appended += 1;
            }
        }
        written += appended;

        // Re-write the month file atomically.
        let mut out = String::new();
        for r in &existing {
            out.push_str(&serde_json::to_string(r)?);
            out.push('\n');
        }
        crate::store::write_atomic(&path, out.as_bytes())?;
    }

    Ok(written)
}

// ---------------------------------------------------------------------------
// The pull entry point.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_qbserve_sync();
    let cursor = state.last_rowid.unwrap_or(-1);

    let (written, new_max_rowid) = do_pull(vault, cursor)?;

    // Advance cursor only after all rows are written.
    let new_state = SyncState {
        last_rowid: if new_max_rowid > cursor { Some(new_max_rowid) } else { state.last_rowid },
        updated: Some(Local::now().to_rfc3339()),
    };
    vault.write_qbserve_sync(&new_state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("rows", written);
    Ok(PullOutcome {
        headline: if written == 0 {
            "Qbserve is up to date — no new rows".to_string()
        } else {
            format!("Qbserve synced — {written} activity rows")
        },
        counts,
    })
}

/// Testable pull body: picks the DB path and delegates to `import_db`.
fn do_pull(vault: &Vault, cursor: i64) -> Result<(u64, i64)> {
    // Prefer Backup.sqlite (written daily by Qbserve; avoids the write lock).
    if let Some(backup) = backup_db_path().filter(|p| p.exists()) {
        return import_db(vault, &backup, cursor);
    }

    // Fall back to copy-then-read of the live DB.
    let live = live_db_path()
        .filter(|p| p.exists())
        .with_context(|| {
            "Qbserve is not installed — database not found at \
             ~/Library/Application Support/Qbserve/"
        })?;

    import_via_copy(
        &live,
        &format!("trove-qbserve-{}", std::process::id()),
        |tmp| import_db(vault, tmp, cursor),
    )
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-qbserve-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a minimal in-memory Qbserve-shaped SQLite DB using the REAL two-level
    /// join schema confirmed by adamfortuna/qbserve_to_exist and Avery2/Qbserve-Blocker:
    ///
    ///   z_HistoryLog.activity_id → Activities._id  (title, category_id, app_id)
    ///   Activities.category_id   → Categories._id  (productivity int: 1/0/-1)
    ///   Activities.app_id        → Apps._id         (bundle, localized_name)
    ///
    /// Row tuples: (history_id, activity_id, start_time, duration)
    /// Activity ids used here: 1=Safari, 2=Terminal
    fn make_test_db(rows: &[(i64, i64, i64, i64)]) -> (tempfile::NamedTempFile, PathBuf) {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let conn = Connection::open(&path).unwrap();

        // Categories: productivity is an integer (-1=Distracting, 0=Neutral, 1=Productive).
        // NOT a title text column — that was the bug.
        conn.execute_batch(
            "CREATE TABLE Categories(_id INTEGER PRIMARY KEY, productivity INTEGER);
             INSERT INTO Categories VALUES (1,  1);   -- Productive
             INSERT INTO Categories VALUES (2,  0);   -- Neutral
             INSERT INTO Categories VALUES (3, -1);   -- Distracting

             -- Apps: stores bundle/localized_name; accessed via Activities.app_id
             CREATE TABLE Apps(_id INTEGER PRIMARY KEY, bundle TEXT, localized_name TEXT,
                               is_ignored INTEGER, track_windows INTEGER,
                               show_windows_as_activities INTEGER, latest_use INTEGER);
             INSERT INTO Apps VALUES (10, 'com.apple.Safari',   'Safari',   0, 1, 0, 1651362755);
             INSERT INTO Apps VALUES (20, 'com.apple.Terminal', 'Terminal', 0, 1, 0, 1651362800);

             -- Activities: the correct join target for z_HistoryLog.activity_id.
             -- app_id links to Apps._id (optional).
             CREATE TABLE Activities(_id INTEGER PRIMARY KEY, title TEXT,
                                     category_id INTEGER, app_id INTEGER);
             INSERT INTO Activities VALUES (1, 'Safari',   2, 10);   -- activity 1: Neutral, app=Safari
             INSERT INTO Activities VALUES (2, 'Terminal', 1, 20);   -- activity 2: Productive, app=Terminal

             CREATE TABLE HistoryTablesIndex(table_name TEXT);
             INSERT INTO HistoryTablesIndex VALUES ('z_HistoryLog_2022_5');",
        )
        .unwrap();

        conn.execute_batch(
            "CREATE TABLE z_HistoryLog_2022_5 \
             (_id INTEGER PRIMARY KEY, activity_id INTEGER, start_time INTEGER, \
              duration INTEGER, project_id INTEGER);",
        )
        .unwrap();

        for (id, activity_id, start_time, duration) in rows {
            conn.execute(
                "INSERT INTO z_HistoryLog_2022_5 VALUES (?1, ?2, ?3, ?4, 0)",
                [id, activity_id, start_time, duration],
            )
            .unwrap();
        }
        (f, path)
    }

    // Unix timestamp for 2022-05-01T10:00:00 UTC
    const TS_MAY_1_2022: i64 = 1651399200;

    #[test]
    fn row_guid_is_deterministic_and_unique() {
        let g1 = row_guid(42);
        let g2 = row_guid(42);
        let g3 = row_guid(43);
        assert_eq!(g1, g2, "same rowid = same guid");
        assert_ne!(g1, g3, "different rowid = different guid");
        assert_eq!(g1.len(), 16, "16-char hex");
        assert!(g1.chars().all(|c| c.is_ascii_hexdigit()), "all hex");
    }

    #[test]
    fn is_safe_table_name_allows_valid_patterns() {
        assert!(is_safe_table_name("z_HistoryLog_2022_5"));
        assert!(is_safe_table_name("z_HistoryLog_2022_12"));
        assert!(is_safe_table_name("z_HistoryLog_2021_4"));
        assert!(!is_safe_table_name("z_HistoryLog_2022_"));
        assert!(!is_safe_table_name("z_HistoryLog_22_5"));
        assert!(!is_safe_table_name("other_table"));
        assert!(!is_safe_table_name("z_HistoryLog_2022_5; DROP TABLE Apps--"));
        assert!(!is_safe_table_name(""));
    }

    #[test]
    fn unix_to_local_converts_sensibly() {
        let ts = unix_to_local(TS_MAY_1_2022);
        // Should be a valid RFC3339 string.
        assert!(ts.len() > 10, "non-empty RFC3339: {ts}");
        assert!(ts.contains("2022"), "year in ts: {ts}");
        assert!(ts.contains('-'), "date separator: {ts}");
    }

    #[test]
    fn import_db_reads_rows_and_resolves_app_names() {
        let vault = temp_vault("import_basic");
        let rows = vec![
            (1, 1, TS_MAY_1_2022, 60),        // Safari, 60s
            (2, 2, TS_MAY_1_2022 + 100, 120), // Terminal, 120s
            (3, 1, TS_MAY_1_2022 + 300, 30),  // Safari, 30s
        ];
        let (_f, path) = make_test_db(&rows);

        let (written, max_rowid) = import_db(&vault, &path, -1).unwrap();
        assert_eq!(written, 3, "3 new rows");
        assert_eq!(max_rowid, 3, "max rowid = 3");

        // Check vault file.
        let p = vault.root().join("activity/qbserve/2022-05.jsonl");
        let content = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(content.contains("\"app\":\"Safari\""));
        assert!(content.contains("\"app\":\"Terminal\""));
        assert!(content.contains("\"bundle_id\":\"com.apple.Safari\""));
        assert!(content.contains("\"category\":\"Neutral\""));
        assert!(content.contains("\"category\":\"Productive\""));
        assert!(content.contains("\"duration_secs\":60"));
        assert!(content.contains("\"duration_secs\":120"));
    }

    #[test]
    fn import_db_skips_rows_with_zero_duration() {
        let vault = temp_vault("skip_zero");
        // Row with duration=0 should be skipped.
        let rows = vec![(1, 1, TS_MAY_1_2022, 0), (2, 2, TS_MAY_1_2022 + 60, 45)];
        let (_f, path) = make_test_db(&rows);

        let (written, _) = import_db(&vault, &path, -1).unwrap();
        assert_eq!(written, 1, "zero-duration row skipped");
    }

    #[test]
    fn import_db_respects_cursor() {
        let vault = temp_vault("cursor");
        let rows = vec![
            (1, 1, TS_MAY_1_2022, 30),
            (2, 2, TS_MAY_1_2022 + 60, 45),
            (3, 1, TS_MAY_1_2022 + 200, 90),
        ];
        let (_f, path) = make_test_db(&rows);

        // Import with cursor=1 (skip rowid 1).
        let (written, max_rowid) = import_db(&vault, &path, 1).unwrap();
        assert_eq!(written, 2, "2 rows after cursor=1");
        assert_eq!(max_rowid, 3);

        // Re-import with same cursor: 2 rows but they are existing — 0 new.
        let (written2, _) = import_db(&vault, &path, 1).unwrap();
        assert_eq!(written2, 0, "no new rows on re-import");
    }

    #[test]
    fn import_db_is_idempotent() {
        let vault = temp_vault("idempotent");
        let rows = vec![(1, 1, TS_MAY_1_2022, 60), (2, 2, TS_MAY_1_2022 + 100, 120)];
        let (_f, path) = make_test_db(&rows);

        import_db(&vault, &path, -1).unwrap();
        // Second import with cursor=-1: same guids → 0 new.
        let (written2, _) = import_db(&vault, &path, -1).unwrap();
        assert_eq!(written2, 0, "idempotent second import");

        // Check no duplicates.
        let p = vault.root().join("activity/qbserve/2022-05.jsonl");
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);
    }

    #[test]
    fn pull_advances_cursor_after_write() {
        let vault = temp_vault("cursor_advance");
        let rows = vec![(1, 1, TS_MAY_1_2022, 60), (2, 2, TS_MAY_1_2022 + 100, 90)];
        let (_f, path) = make_test_db(&rows);

        import_db(&vault, &path, -1).unwrap();

        // Manually update cursor as pull() would.
        let state = SyncState { last_rowid: Some(2), updated: Some(Local::now().to_rfc3339()) };
        vault.write_qbserve_sync(&state).unwrap();

        let loaded = vault.read_qbserve_sync();
        assert_eq!(loaded.last_rowid, Some(2));
        assert!(loaded.updated.is_some());
    }

    #[test]
    fn no_data_without_qbserve_db() {
        let vault = temp_vault("missing_db");
        let nonexistent = vault.root().join("nonexistent.sqlite");
        // Should error gracefully (not panic).
        let result = import_db(&vault, &nonexistent, -1);
        assert!(result.is_err(), "missing DB should error");
    }

    #[test]
    fn cursor_serde_roundtrip_back_compat() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_rowid.is_none());
        let with_rowid: SyncState =
            serde_json::from_str(r#"{"last_rowid":12345}"#).unwrap();
        assert_eq!(with_rowid.last_rowid, Some(12345));
        assert!(with_rowid.updated.is_none());
    }

    #[test]
    fn two_partition_tables_are_both_read() {
        let vault = temp_vault("two_partitions");
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let conn = Connection::open(&path).unwrap();
        // Real two-level schema: Activities intermediary, Categories.productivity int.
        conn.execute_batch(
            "CREATE TABLE Categories(_id INTEGER PRIMARY KEY, productivity INTEGER);
             INSERT INTO Categories VALUES (1, 0);  -- Neutral
             CREATE TABLE Apps(_id INTEGER PRIMARY KEY, bundle TEXT,
                               localized_name TEXT, is_ignored INTEGER,
                               track_windows INTEGER, show_windows_as_activities INTEGER,
                               latest_use INTEGER);
             INSERT INTO Apps VALUES (10, 'com.apple.Safari', 'Safari', 0, 1, 0, 0);
             CREATE TABLE Activities(_id INTEGER PRIMARY KEY, title TEXT,
                                     category_id INTEGER, app_id INTEGER);
             INSERT INTO Activities VALUES (1, 'Safari', 1, 10);
             CREATE TABLE HistoryTablesIndex(table_name TEXT);
             INSERT INTO HistoryTablesIndex VALUES ('z_HistoryLog_2022_4');
             INSERT INTO HistoryTablesIndex VALUES ('z_HistoryLog_2022_5');
             CREATE TABLE z_HistoryLog_2022_4 (_id INTEGER PRIMARY KEY,
               activity_id INTEGER, start_time INTEGER, duration INTEGER, project_id INTEGER);
             INSERT INTO z_HistoryLog_2022_4 VALUES (1, 1, 1648800000, 60, 0);
             CREATE TABLE z_HistoryLog_2022_5 (_id INTEGER PRIMARY KEY,
               activity_id INTEGER, start_time INTEGER, duration INTEGER, project_id INTEGER);
             INSERT INTO z_HistoryLog_2022_5 VALUES (2, 1, 1651399200, 90, 0);",
        )
        .unwrap();

        let (written, max_rowid) = import_db(&vault, &path, -1).unwrap();
        assert_eq!(written, 2, "rows from both partitions");
        assert_eq!(max_rowid, 2);

        // Both months should have files.
        assert!(vault.root().join("activity/qbserve/2022-04.jsonl").exists());
        assert!(vault.root().join("activity/qbserve/2022-05.jsonl").exists());
    }

    #[test]
    fn sqlite_master_fallback_when_no_history_tables_index() {
        let vault = temp_vault("no_index");
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let conn = Connection::open(&path).unwrap();
        // Use a timestamp well into 2022-05 UTC so it is 2022-05 regardless of
        // local timezone (even UTC-12: 1651356000 = 2022-04-30 22:00 UTC = 2022-04-30
        // in some zones — use a midday ts to be safe).
        // 1651400000 = 2022-05-01T10:13:20Z — safely May 2022 in any timezone.
        // Real two-level schema: Activities intermediary, Categories.productivity int.
        conn.execute_batch(
            // No HistoryTablesIndex table — should fall back to sqlite_master scan.
            "CREATE TABLE Categories(_id INTEGER PRIMARY KEY, productivity INTEGER);
             INSERT INTO Categories VALUES (1, 1);  -- Productive
             CREATE TABLE Apps(_id INTEGER PRIMARY KEY, bundle TEXT,
                               localized_name TEXT, is_ignored INTEGER,
                               track_windows INTEGER, show_windows_as_activities INTEGER,
                               latest_use INTEGER);
             INSERT INTO Apps VALUES (20, 'com.apple.Terminal', 'Terminal', 0, 1, 0, 0);
             CREATE TABLE Activities(_id INTEGER PRIMARY KEY, title TEXT,
                                     category_id INTEGER, app_id INTEGER);
             INSERT INTO Activities VALUES (10, 'Terminal', 1, 20);
             CREATE TABLE z_HistoryLog_2022_5 (_id INTEGER PRIMARY KEY,
               activity_id INTEGER, start_time INTEGER, duration INTEGER, project_id INTEGER);
             INSERT INTO z_HistoryLog_2022_5 VALUES (5, 10, 1651400000, 120, 0);",
        )
        .unwrap();

        let (written, _) = import_db(&vault, &path, -1).unwrap();
        assert_eq!(written, 1, "fallback sqlite_master scan found 1 row");
        // The file should land in 2022-05 (the local month of 1651400000).
        let content = vault
            .root()
            .join("activity/qbserve")
            .read_dir()
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                if p.extension().map_or(false, |x| x == "jsonl") {
                    std::fs::read_to_string(&p).ok()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(content.contains("\"app\":\"Terminal\""), "app name in output");
        assert!(content.contains("\"category\":\"Productive\""), "category in output");
    }
}
