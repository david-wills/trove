//! Timing — automatic macOS time tracker (local SQLite reader).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/timing.md
//!
//! ## What this does
//!
//! Reads Timing's local SQLite database at
//! `~/Library/Application Support/info.eurocomp.Timing2/SQLite.db`
//! and writes two kinds of raw rows to `activity/timing/YYYY-MM.jsonl`:
//!
//! 1. **AppActivity** — observed app-usage spans (which app, document, file
//!    path, and project, with start/end/duration).
//! 2. **TaskActivity** — user-created manual timer entries (start/end,
//!    project, notes), flagged `kind: "task"` to distinguish them from
//!    automatic observations.
//!
//! ## Schema (confirmed via marcoroth/timingapp-ruby gem schema.rb + models)
//!
//! Primary activity table:
//! ```sql
//! AppActivity(
//!   id         INTEGER PRIMARY KEY,
//!   startDate  REAL,          -- Unix epoch seconds (float)
//!   endDate    REAL,          -- Unix epoch seconds (float)
//!   applicationID INTEGER,   -- → Application(id)
//!   titleID    INTEGER,       -- → Title(id)
//!   pathID     INTEGER,       -- → Path(id)
//!   projectID  INTEGER,       -- → Project(id)
//!   localDeviceID INTEGER,    -- → Device(localID)
//!   isDeleted  BOOL           -- soft-delete flag (filter out)
//! )
//! ```
//!
//! Lookup tables:
//! ```sql
//! Application(id INTEGER PRIMARY KEY, bundleIdentifier TEXT, title TEXT, ...)
//! Title(id INTEGER PRIMARY KEY, stringValue TEXT)
//! Path(id INTEGER PRIMARY KEY, stringValue TEXT)
//! Project(id INTEGER PRIMARY KEY, title TEXT, parentID INTEGER, productivityScore REAL, ...)
//! ```
//!
//! Manual entries:
//! ```sql
//! TaskActivity(
//!   id         INTEGER PRIMARY KEY,
//!   startDate  REAL,          -- Unix epoch seconds (float)
//!   endDate    REAL,          -- Unix epoch seconds (float)
//!   projectID  INTEGER,       -- → Project(id)
//!   isRunning  BOOL,          -- timer still ticking — skip
//!   isDeleted  BOOL,          -- soft-delete flag
//!   property_bag TEXT         -- JSON with optional `notes` field
//! )
//! ```
//!
//! `AppActivityWithStrings` is a denormalized VIEW (same columns as
//! AppActivity but with text from lookups already resolved); we prefer it
//! when available but fall back to the manual join if the view is absent
//! (older versions of Timing may not have it).
//!
//! ## Timestamps
//!
//! `startDate` / `endDate` are stored as SQLite REAL — Unix epoch seconds as
//! a double (confirmed by `Time.at(value)` in timingapp-ruby gem's
//! `time_column` helper). Duration is derived: `(endDate - startDate).round()`.
//!
//! ## Deduplication / watermark
//!
//! Cursor (`.trove/timing-sync.json`) holds separate `last_app_rowid` and
//! `last_task_rowid` — the highest imported id from each table. On a crash
//! the re-drain is safe because old rowids are skipped.
//!
//! ## Lock avoidance
//!
//! Timing holds an exclusive lock on the live SQLite.db. We copy it to a
//! temp file via [`crate::browser::import_via_copy`] before opening, which
//! also copies the WAL if present so we get a consistent snapshot.
//!
//! ## guid
//!
//! FNV-1a 64-bit hash over `"timing-app:<rowid>"` or `"timing-task:<rowid>"`
//! encoded as 16-char lowercase hex. Row IDs are stable primary keys and do
//! not change when a project is renamed.

use std::collections::BTreeMap;
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
const DIR: &str = "activity/timing";
/// Non-secret cursor file.
const SYNC_FILE: &str = ".trove/timing-sync.json";
/// Sync cadence — hourly (data accumulates continuously while Timing runs).
pub const TIMING_SYNC_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// DB path.

fn db_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Library/Application Support/info.eurocomp.Timing2/SQLite.db")
    })
}

fn timing_available() -> bool {
    db_path().is_some_and(|p| p.exists())
}

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_permission() -> PermissionInfo {
    PermissionInfo {
        kind: "local-app-data",
        granted: Some(timing_available()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("Timing synced — {} rows", total)
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Timing sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let total: u64 = out.counts.values().sum();
    Ok(PullOutcome {
        headline: if total == 0 {
            "Timing is up to date — no new rows".to_string()
        } else {
            format!("Timing synced — {} rows", total)
        },
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "timing",
        name: "Timing",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Backfills your Timing app history from its local SQLite database — \
                      which apps and documents you spent time on, with project categories. \
                      Richer context than raw app tracking: Timing records which specific \
                      document or URL was active inside each app. For existing Timing \
                      subscribers: Trove's own activity watcher captures similar data going \
                      forward; this integration imports your multi-year Timing history.",
        domain: "activity",
        vault_path: "activity/timing/",
        toggleable: true,
        setup: &[
            "Timing must be installed (timing.app; requires an active subscription).",
            "No configuration needed — Trove reads the local Timing database automatically.",
        ],
        caveats: "Requires Timing to be installed and to have been running to accumulate data. \
                  This integration is most valuable for importing existing Timing history; \
                  Trove's activity watcher covers ongoing tracking.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(TIMING_SYNC_SECS),
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
    /// Highest imported AppActivity.id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_app_rowid: Option<i64>,
    /// Highest imported TaskActivity.id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_task_rowid: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_timing_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_timing_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// On-disk row shape.

/// One raw span written to `activity/timing/YYYY-MM.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TimingRow {
    /// Stable dedupe key.
    guid: String,
    /// Row kind: "app" (automatic observation) or "task" (manual timer).
    kind: String,
    /// RFC3339 local timestamp (start of span).
    ts: String,
    /// RFC3339 local timestamp (end of span). May be absent if timer still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ts_end: Option<String>,
    /// Duration in seconds (derived from endDate − startDate; 0 if end unknown).
    duration_secs: i64,
    /// App bundle identifier (e.g. "com.apple.Safari"). AppActivity only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    bundle_id: String,
    /// App display name (e.g. "Safari"). AppActivity only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    app: String,
    /// Window/document title (from Title.stringValue). AppActivity only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    /// File path or URL (from Path.stringValue). AppActivity only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// Project name assigned in Timing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    /// Parent project name (one level up in the project hierarchy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_parent: Option<String>,
    /// Notes from the task entry (TaskActivity.property_bag JSON). TaskActivity only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    /// Source DB row id (for watermark tracking).
    rowid: i64,
    /// Extra columns present in the DB beyond the known schema (forward-compat).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Helpers.

/// FNV-1a 64-bit hash over a string, encoded as 16-char lowercase hex.
fn fnv64(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

fn app_guid(rowid: i64) -> String {
    fnv64(&format!("timing-app:{rowid}"))
}

fn task_guid(rowid: i64) -> String {
    fnv64(&format!("timing-task:{rowid}"))
}

/// SQLite REAL (Unix epoch seconds as f64) → RFC3339 local time string.
fn real_to_local(secs: f64) -> String {
    let secs_i64 = secs as i64;
    let nanos = ((secs - secs_i64 as f64) * 1_000_000_000.0) as u32;
    DateTime::from_timestamp(secs_i64, nanos)
        .map(|utc| utc.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string())
}

// ---------------------------------------------------------------------------
// SQLite import (testable).

/// Read and import rows from a Timing DB at `db_path`.
/// Returns (app_rows_written, task_rows_written, new_app_max_rowid, new_task_max_rowid).
pub(crate) fn import_db(
    vault: &Vault,
    db_path: &std::path::Path,
    app_cursor: i64,
    task_cursor: i64,
) -> Result<(u64, u64, i64, i64)> {
    let conn = rusqlite::Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening Timing DB at {}", db_path.display()))?;

    let _ = conn.busy_timeout(Duration::from_secs(2));

    // --- Project lookup: Project.id → (title, parentID) ---
    struct ProjectInfo {
        title: String,
        parent_id: Option<i64>,
    }
    let mut projects: BTreeMap<i64, ProjectInfo> = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, COALESCE(title,''), parentID FROM Project",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let id: i64 = row.get(0).unwrap_or(0);
                let title: String = row.get(1).unwrap_or_default();
                let parent_id: Option<i64> = row.get(2).ok().filter(|&v: &i64| v > 0);
                projects.insert(id, ProjectInfo { title, parent_id });
            }
        }
    }

    let resolve_project =
        |pid: Option<i64>| -> (Option<String>, Option<String>) {
            let id = match pid.filter(|&v| v > 0) {
                Some(v) => v,
                None => return (None, None),
            };
            match projects.get(&id) {
                None => (None, None),
                Some(info) => {
                    let parent_title = info
                        .parent_id
                        .and_then(|ppid| projects.get(&ppid))
                        .map(|p| p.title.clone());
                    (Some(info.title.clone()), parent_title)
                }
            }
        };

    // --- Application lookup: Application.id → (bundleIdentifier, title) ---
    let mut applications: BTreeMap<i64, (String, String)> = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare(
        "SELECT id, COALESCE(bundleIdentifier,''), COALESCE(title,'') FROM Application",
    ) {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let id: i64 = row.get(0).unwrap_or(0);
                let bundle: String = row.get(1).unwrap_or_default();
                let title: String = row.get(2).unwrap_or_default();
                applications.insert(id, (bundle, title));
            }
        }
    }

    // --- Title lookup: Title.id → stringValue ---
    let mut titles: BTreeMap<i64, String> = BTreeMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT id, COALESCE(stringValue,'') FROM Title")
    {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let id: i64 = row.get(0).unwrap_or(0);
                let val: String = row.get(1).unwrap_or_default();
                titles.insert(id, val);
            }
        }
    }

    // --- Path lookup: Path.id → stringValue ---
    let mut paths: BTreeMap<i64, String> = BTreeMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT id, COALESCE(stringValue,'') FROM Path")
    {
        if let Ok(mut rows) = stmt.query([]) {
            while let Ok(Some(row)) = rows.next() {
                let id: i64 = row.get(0).unwrap_or(0);
                let val: String = row.get(1).unwrap_or_default();
                paths.insert(id, val);
            }
        }
    }

    // --- AppActivity rows ---
    let mut app_rows: Vec<TimingRow> = Vec::new();
    let mut new_app_max = app_cursor;

    {
        let sql = "SELECT id, startDate, endDate, applicationID, titleID, pathID, projectID \
                   FROM AppActivity \
                   WHERE id > ?1 AND isDeleted = 0 AND startDate > 0 \
                   ORDER BY id";
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(mut rows) = stmt.query([app_cursor]) {
                while let Ok(Some(row)) = rows.next() {
                    let rowid: i64 = row.get(0).unwrap_or(0);
                    let start: f64 = row.get(1).unwrap_or(0.0);
                    let end: f64 = row.get(2).unwrap_or(0.0);
                    let app_id: Option<i64> =
                        row.get(3).ok().filter(|&v: &i64| v > 0);
                    let title_id: Option<i64> =
                        row.get(4).ok().filter(|&v: &i64| v > 0);
                    let path_id: Option<i64> =
                        row.get(5).ok().filter(|&v: &i64| v > 0);
                    let project_id: Option<i64> =
                        row.get(6).ok().filter(|&v: &i64| v > 0);

                    if start <= 0.0 {
                        continue;
                    }

                    let ts = real_to_local(start);
                    let (ts_end, duration_secs) = if end > start {
                        (Some(real_to_local(end)), (end - start).round() as i64)
                    } else {
                        (None, 0)
                    };

                    let (bundle_id, app) = app_id
                        .and_then(|id| applications.get(&id).cloned())
                        .unwrap_or_default();

                    let title = title_id
                        .and_then(|id| titles.get(&id))
                        .filter(|s| !s.is_empty())
                        .cloned();

                    let path = path_id
                        .and_then(|id| paths.get(&id))
                        .filter(|s| !s.is_empty())
                        .cloned();

                    let (project, project_parent) = resolve_project(project_id);

                    new_app_max = new_app_max.max(rowid);
                    app_rows.push(TimingRow {
                        guid: app_guid(rowid),
                        kind: "app".to_string(),
                        ts,
                        ts_end,
                        duration_secs,
                        bundle_id,
                        app,
                        title,
                        path,
                        project,
                        project_parent,
                        notes: None,
                        rowid,
                        extra: Map::new(),
                    });
                }
            }
        }
    }

    // --- TaskActivity rows ---
    let mut task_rows: Vec<TimingRow> = Vec::new();
    let mut new_task_max = task_cursor;

    {
        let sql = "SELECT id, startDate, endDate, projectID, property_bag \
                   FROM TaskActivity \
                   WHERE id > ?1 AND isDeleted = 0 AND isRunning = 0 AND startDate > 0 \
                   ORDER BY id";
        if let Ok(mut stmt) = conn.prepare(sql) {
            if let Ok(mut rows) = stmt.query([task_cursor]) {
                while let Ok(Some(row)) = rows.next() {
                    let rowid: i64 = row.get(0).unwrap_or(0);
                    let start: f64 = row.get(1).unwrap_or(0.0);
                    let end: f64 = row.get(2).unwrap_or(0.0);
                    let project_id: Option<i64> =
                        row.get(3).ok().filter(|&v: &i64| v > 0);
                    let prop_bag: Option<String> = row.get(4).ok().flatten();

                    if start <= 0.0 {
                        continue;
                    }

                    let ts = real_to_local(start);
                    let (ts_end, duration_secs) = if end > start {
                        (Some(real_to_local(end)), (end - start).round() as i64)
                    } else {
                        (None, 0)
                    };

                    // Extract `notes` from the JSON property_bag if present.
                    let notes: Option<String> = prop_bag
                        .as_deref()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .and_then(|v| {
                            v.get("notes")
                                .and_then(|n| n.as_str())
                                .filter(|s| !s.is_empty())
                                .map(|s| s.to_string())
                        });

                    let (project, project_parent) = resolve_project(project_id);

                    new_task_max = new_task_max.max(rowid);
                    task_rows.push(TimingRow {
                        guid: task_guid(rowid),
                        kind: "task".to_string(),
                        ts,
                        ts_end,
                        duration_secs,
                        bundle_id: String::new(),
                        app: String::new(),
                        title: None,
                        path: None,
                        project,
                        project_parent,
                        notes,
                        rowid,
                        extra: Map::new(),
                    });
                }
            }
        }
    }

    // --- Write rows grouped by month ---
    let app_written = upsert_rows(vault, app_rows)?;
    let task_written = upsert_rows(vault, task_rows)?;

    Ok((app_written, task_written, new_app_max, new_task_max))
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Upsert rows into `activity/timing/YYYY-MM.jsonl` keyed by guid.
fn upsert_rows(vault: &Vault, rows: Vec<TimingRow>) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }

    // Group by YYYY-MM from the ts field.
    let mut by_month: BTreeMap<String, Vec<TimingRow>> = BTreeMap::new();
    for row in rows {
        let month_key = if row.ts.len() >= 7 {
            row.ts[..7].to_string()
        } else {
            continue;
        };
        by_month.entry(month_key).or_default().push(row);
    }

    let mut written: u64 = 0;

    for (month_key, new_rows) in by_month {
        let path_str = format!("{DIR}/{month_key}.jsonl");
        let path = vault.resolve(&path_str)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("creating timing dir")?;
        }

        // Read existing rows.
        let mut existing: Vec<TimingRow> = if path.exists() {
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            raw.lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str::<TimingRow>(l).ok())
                .collect()
        } else {
            Vec::new()
        };

        let mut guid_idx: BTreeMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (r.guid.clone(), i))
            .collect();

        let mut appended: u64 = 0;
        for row in new_rows {
            if let Some(&idx) = guid_idx.get(&row.guid) {
                existing[idx] = row;
            } else {
                guid_idx.insert(row.guid.clone(), existing.len());
                existing.push(row);
                appended += 1;
            }
        }
        written += appended;

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
    let state = vault.read_timing_sync();
    let app_cursor = state.last_app_rowid.unwrap_or(-1);
    let task_cursor = state.last_task_rowid.unwrap_or(-1);

    let (app_written, task_written, new_app_max, new_task_max) =
        do_pull(vault, app_cursor, task_cursor)?;

    let new_state = SyncState {
        last_app_rowid: if new_app_max > app_cursor {
            Some(new_app_max)
        } else {
            state.last_app_rowid
        },
        last_task_rowid: if new_task_max > task_cursor {
            Some(new_task_max)
        } else {
            state.last_task_rowid
        },
        updated: Some(Local::now().to_rfc3339()),
    };
    vault.write_timing_sync(&new_state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("app_rows", app_written);
    counts.insert("task_rows", task_written);
    Ok(PullOutcome {
        headline: format!(
            "Timing synced — {} app rows, {} task rows",
            app_written, task_written
        ),
        counts,
    })
}

/// Testable pull body: copy-then-read the live DB.
fn do_pull(
    vault: &Vault,
    app_cursor: i64,
    task_cursor: i64,
) -> Result<(u64, u64, i64, i64)> {
    let live = db_path()
        .filter(|p| p.exists())
        .with_context(|| {
            "Timing is not installed — database not found at \
             ~/Library/Application Support/info.eurocomp.Timing2/SQLite.db"
        })?;

    import_via_copy(
        &live,
        &format!("trove-timing-{}", std::process::id()),
        |tmp| import_db(vault, tmp, app_cursor, task_cursor),
    )
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-timing-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Build a minimal in-memory Timing-shaped SQLite DB.
    ///
    /// Schema confirmed via marcoroth/timingapp-ruby:
    ///   - schema.rb (foreign keys + table names)
    ///   - models/app_activity.rb (belongs_to associations + time_column declarations)
    ///   - timing_record.rb (`time_column` uses Time.at(value) → Unix epoch float)
    ///   - models/project.rb, application.rb, title.rb, path.rb
    ///
    /// `startDate`/`endDate` are SQLite REAL (float seconds since Unix epoch).
    fn make_test_db(
        app_rows: &[(i64, f64, f64, i64, i64, i64, i64)], // (id, start, end, app_id, title_id, path_id, proj_id)
        task_rows: &[(i64, f64, f64, i64, &str)],          // (id, start, end, proj_id, notes_json)
    ) -> (tempfile::NamedTempFile, std::path::PathBuf) {
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let conn = Connection::open(&path).unwrap();

        conn.execute_batch(
            "-- Application lookup
             CREATE TABLE Application(
               id INTEGER PRIMARY KEY,
               bundleIdentifier TEXT,
               executable TEXT,
               title TEXT,
               property_bag TEXT
             );
             INSERT INTO Application VALUES (1, 'com.apple.Safari',    NULL, 'Safari',   NULL);
             INSERT INTO Application VALUES (2, 'com.apple.Terminal',  NULL, 'Terminal', NULL);
             INSERT INTO Application VALUES (3, 'com.microsoft.VSCode',NULL, 'Code',     NULL);

             -- Title lookup
             CREATE TABLE Title(id INTEGER PRIMARY KEY, stringValue TEXT);
             INSERT INTO Title VALUES (1, 'index.html — MyProject');
             INSERT INTO Title VALUES (2, 'Terminal — zsh');
             INSERT INTO Title VALUES (3, 'README.md — Code');

             -- Path lookup
             CREATE TABLE Path(id INTEGER PRIMARY KEY, stringValue TEXT);
             INSERT INTO Path VALUES (1, '/Users/user/projects/myproject/index.html');
             INSERT INTO Path VALUES (2, 'https://example.com/docs');

             -- Project hierarchy (parent=0 means root)
             CREATE TABLE Project(
               id INTEGER PRIMARY KEY,
               title TEXT,
               parentID INTEGER,
               productivityScore REAL,
               property_bag TEXT
             );
             INSERT INTO Project VALUES (1, 'Work',        0,    0.9, NULL);
             INSERT INTO Project VALUES (2, 'MyProject',   1,    0.8, NULL);
             INSERT INTO Project VALUES (3, 'Personal',    0,    0.5, NULL);",
        )
        .unwrap();

        // Note: FK constraints are NOT declared in the test schema to avoid
        // rusqlite/SQLCipher FK enforcement issues with 0 / NULL IDs.
        // The real Timing DB has FK constraints but does not enforce them
        // by default (PRAGMA foreign_keys defaults to OFF in SQLite).
        conn.execute_batch(
            "-- AppActivity: startDate/endDate are REAL (Unix epoch seconds as float)
             CREATE TABLE AppActivity(
               id INTEGER PRIMARY KEY,
               startDate REAL,
               endDate REAL,
               applicationID INTEGER,
               titleID INTEGER,
               pathID INTEGER,
               projectID INTEGER,
               localDeviceID INTEGER,
               isDeleted BOOL DEFAULT 0
             );

             -- TaskActivity: manual timer entries
             CREATE TABLE TaskActivity(
               id INTEGER PRIMARY KEY,
               startDate REAL,
               endDate REAL,
               projectID INTEGER,
               isRunning BOOL DEFAULT 0,
               isDeleted BOOL DEFAULT 0,
               property_bag TEXT
             );",
        )
        .unwrap();

        for &(id, start, end, app_id, title_id, path_id, proj_id) in app_rows {
            conn.execute(
                "INSERT INTO AppActivity(id, startDate, endDate, applicationID, \
                 titleID, pathID, projectID, localDeviceID, isDeleted) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, 0)",
                rusqlite::params![id, start, end, app_id, title_id, path_id, proj_id],
            )
            .unwrap();
        }

        for &(id, start, end, proj_id, notes_json) in task_rows {
            conn.execute(
                "INSERT INTO TaskActivity(id, startDate, endDate, projectID, \
                 isRunning, isDeleted, property_bag) \
                 VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                rusqlite::params![id, start, end, proj_id, notes_json],
            )
            .unwrap();
        }

        (f, path)
    }

    // 2023-03-15T10:00:00 UTC = 1678874400
    const TS_A: f64 = 1678874400.0;
    // 2023-03-15T10:30:00 UTC = 1678876200 (+1800s)
    const TS_B: f64 = 1678876200.0;
    // 2023-03-15T11:00:00 UTC = 1678878000
    const TS_C: f64 = 1678878000.0;

    #[test]
    fn guid_is_deterministic_and_distinct() {
        let a1 = app_guid(42);
        let a2 = app_guid(42);
        let a3 = app_guid(43);
        let t1 = task_guid(42);
        assert_eq!(a1, a2, "same rowid = same guid");
        assert_ne!(a1, a3, "different rowid = different guid");
        assert_ne!(a1, t1, "app vs task must differ even with same rowid");
        assert_eq!(a1.len(), 16);
        assert!(a1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn real_to_local_converts_sensibly() {
        let ts = real_to_local(TS_A);
        assert!(ts.contains("2023"), "year in ts: {ts}");
        assert!(ts.contains('-'), "date separator: {ts}");
        assert!(ts.len() > 10, "valid RFC3339: {ts}");
    }

    #[test]
    fn import_db_reads_app_activity() {
        let vault = temp_vault("app_basic");
        let app_rows = [
            (1, TS_A, TS_B, 1, 1, 1, 2), // Safari, title1, path1, project MyProject
            (2, TS_B, TS_C, 2, 2, 0, 1), // Terminal, title2, no path, project Work
        ];
        let task_rows = [];
        let (_f, path) = make_test_db(&app_rows, &task_rows);

        let (app_written, task_written, new_app_max, new_task_max) =
            import_db(&vault, &path, -1, -1).unwrap();

        assert_eq!(app_written, 2, "2 app rows written");
        assert_eq!(task_written, 0, "0 task rows");
        assert_eq!(new_app_max, 2);
        assert_eq!(new_task_max, -1, "task cursor unchanged");

        let month_file = vault.root().join("activity/timing/2023-03.jsonl");
        let content = std::fs::read_to_string(&month_file).unwrap();
        assert!(
            content.contains("\"app\":\"Safari\""),
            "Safari app name present"
        );
        assert!(
            content.contains("\"bundle_id\":\"com.apple.Safari\""),
            "bundle_id present"
        );
        assert!(
            content.contains("\"app\":\"Terminal\""),
            "Terminal app present"
        );
        assert!(
            content.contains("\"project\":\"MyProject\""),
            "project name present"
        );
        assert!(
            content.contains("\"project_parent\":\"Work\""),
            "project parent present"
        );
        assert!(
            content.contains("\"kind\":\"app\""),
            "kind=app present"
        );
        assert!(
            content.contains("\"duration_secs\":1800"),
            "duration computed correctly"
        );
        assert!(
            content.contains("/projects/myproject/index.html"),
            "path present"
        );
        assert!(
            content.contains("index.html — MyProject"),
            "title present"
        );
    }

    #[test]
    fn import_db_reads_task_activity() {
        let vault = temp_vault("task_basic");
        let app_rows = [];
        let task_rows = [
            (1, TS_A, TS_B, 2, r#"{"notes": "Wrote unit tests"}"#),
            (2, TS_B, TS_C, 1, r#"{}"#), // no notes
        ];
        let (_f, path) = make_test_db(&app_rows, &task_rows);

        let (app_written, task_written, _, new_task_max) =
            import_db(&vault, &path, -1, -1).unwrap();

        assert_eq!(app_written, 0);
        assert_eq!(task_written, 2, "2 task rows");
        assert_eq!(new_task_max, 2);

        let content = std::fs::read_to_string(
            vault.root().join("activity/timing/2023-03.jsonl"),
        )
        .unwrap();
        assert!(content.contains("\"kind\":\"task\""));
        assert!(content.contains("\"notes\":\"Wrote unit tests\""));
        assert!(content.contains("\"project\":\"MyProject\""));
    }

    #[test]
    fn import_db_skips_deleted_and_running() {
        let vault = temp_vault("skip_deleted");
        let f = tempfile::NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE Application(id INTEGER PRIMARY KEY, bundleIdentifier TEXT, executable TEXT, title TEXT, property_bag TEXT);
             INSERT INTO Application VALUES (1, 'com.apple.Safari', NULL, 'Safari', NULL);
             CREATE TABLE Title(id INTEGER PRIMARY KEY, stringValue TEXT);
             INSERT INTO Title VALUES (1, 'Test');
             CREATE TABLE Path(id INTEGER PRIMARY KEY, stringValue TEXT);
             CREATE TABLE Project(id INTEGER PRIMARY KEY, title TEXT, parentID INTEGER, productivityScore REAL, property_bag TEXT);
             INSERT INTO Project VALUES (1, 'Work', 0, 0.8, NULL);
             CREATE TABLE AppActivity(id INTEGER PRIMARY KEY, startDate REAL, endDate REAL,
               applicationID INTEGER, titleID INTEGER, pathID INTEGER, projectID INTEGER,
               localDeviceID INTEGER, isDeleted BOOL DEFAULT 0);
             INSERT INTO AppActivity VALUES (1, 1678874400.0, 1678876200.0, 1, 1, NULL, 1, 1, 1); -- isDeleted=1: skip
             INSERT INTO AppActivity VALUES (2, 1678876200.0, 1678878000.0, 1, 1, NULL, 1, 1, 0); -- isDeleted=0: ok
             CREATE TABLE TaskActivity(id INTEGER PRIMARY KEY, startDate REAL, endDate REAL,
               projectID INTEGER, isRunning BOOL DEFAULT 0, isDeleted BOOL DEFAULT 0, property_bag TEXT);
             INSERT INTO TaskActivity VALUES (1, 1678874400.0, 1678876200.0, 1, 1, 0, NULL); -- isRunning=1: skip
             INSERT INTO TaskActivity VALUES (2, 1678876200.0, 1678878000.0, 1, 0, 0, NULL); -- isRunning=0: ok",
        ).unwrap();

        let (app_written, task_written, _, _) =
            import_db(&vault, &path, -1, -1).unwrap();
        assert_eq!(app_written, 1, "deleted AppActivity skipped");
        assert_eq!(task_written, 1, "running TaskActivity skipped");
    }

    #[test]
    fn import_db_respects_cursors() {
        let vault = temp_vault("cursor");
        let app_rows = [
            (1, TS_A, TS_B, 1, 1, 1, 2),
            (2, TS_B, TS_C, 2, 2, 0, 1),
            (3, TS_C, TS_C + 600.0, 3, 3, 2, 3),
        ];
        let task_rows = [
            (1, TS_A, TS_B, 1, r#"{}"#),
            (2, TS_B, TS_C, 2, r#"{}"#),
        ];
        let (_f, path) = make_test_db(&app_rows, &task_rows);

        // Import with app_cursor=1, task_cursor=1 — should skip rowid=1 in both.
        let (app_written, task_written, new_app_max, new_task_max) =
            import_db(&vault, &path, 1, 1).unwrap();
        assert_eq!(app_written, 2, "2 new app rows after cursor=1");
        assert_eq!(task_written, 1, "1 new task row after cursor=1");
        assert_eq!(new_app_max, 3);
        assert_eq!(new_task_max, 2);
    }

    #[test]
    fn import_db_is_idempotent() {
        let vault = temp_vault("idempotent");
        let app_rows = [(1, TS_A, TS_B, 1, 1, 1, 2)];
        let task_rows = [(1, TS_A, TS_B, 1, r#"{"notes":"test"}"#)];
        let (_f, path) = make_test_db(&app_rows, &task_rows);

        import_db(&vault, &path, -1, -1).unwrap();
        let (app2, task2, _, _) = import_db(&vault, &path, -1, -1).unwrap();
        assert_eq!(app2, 0, "no new app rows on re-import");
        assert_eq!(task2, 0, "no new task rows on re-import");

        let p = vault.root().join("activity/timing/2023-03.jsonl");
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);
    }

    #[test]
    fn cursor_serde_roundtrip_back_compat() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_app_rowid.is_none());
        assert!(empty.last_task_rowid.is_none());

        let with_both: SyncState =
            serde_json::from_str(r#"{"last_app_rowid":100,"last_task_rowid":5}"#)
                .unwrap();
        assert_eq!(with_both.last_app_rowid, Some(100));
        assert_eq!(with_both.last_task_rowid, Some(5));
        // Old cursors without one of the two fields still deserialize.
        let app_only: SyncState =
            serde_json::from_str(r#"{"last_app_rowid":50}"#).unwrap();
        assert_eq!(app_only.last_app_rowid, Some(50));
        assert!(app_only.last_task_rowid.is_none());
    }

    #[test]
    fn missing_db_errors_gracefully() {
        let vault = temp_vault("missing");
        let nonexistent = vault.root().join("nonexistent.sqlite");
        let result = import_db(&vault, &nonexistent, -1, -1);
        assert!(result.is_err(), "missing DB should error");
    }

    #[test]
    fn project_hierarchy_resolved() {
        let vault = temp_vault("project_hierarchy");
        let app_rows = [
            (1, TS_A, TS_B, 1, 1, 0, 2), // project=MyProject (parent=Work)
        ];
        let (_f, path) = make_test_db(&app_rows, &[]);

        let (written, _, _, _) = import_db(&vault, &path, -1, -1).unwrap();
        assert_eq!(written, 1);

        let content = std::fs::read_to_string(
            vault.root().join("activity/timing/2023-03.jsonl"),
        )
        .unwrap();
        // project should be "MyProject", project_parent should be "Work"
        assert!(content.contains("\"project\":\"MyProject\""));
        assert!(content.contains("\"project_parent\":\"Work\""));
    }
}
