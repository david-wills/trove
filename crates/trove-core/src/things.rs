//! Things 3 — local task manager by Cultured Code. A **Periodic** collector
//! that reads the `main.sqlite` database inside Things 3's sandboxed Group
//! Container and feeds the already-bound [`crate::tasks`] contract.
//!
//! **Access:** `~/Library/Group Containers/JLMPQHK86H.com.culturedcode.ThingsMac/
//! ThingsData-*/Things Database.thingsdatabase/main.sqlite`; also handles the
//! beta container (`com.culturedcode.ThingsMac.beta`). Full Disk Access is the
//! only gate — Things 3 does not need to be closed; reads are read-only.
//!
//! **Two outputs:**
//! - **tasks contract** `tasks/things/` via `apply_tasks_sync`: one open task
//!   per line in `tasks.jsonl`; completions land in the event stream under
//!   `events/YYYY-MM.jsonl`.
//! - **raw firehose** `tasks/things/raw/YYYY-MM.jsonl`: the native
//!   `TMTask`+`TMChecklistItem`+tag rows at full fidelity (upserted by uuid,
//!   partitioned by `creationDate` month).
//!
//! **Cursor:** `.trove/things-sync.json` holds `updated` (last sync time) and
//! `cursor` (highest `userModificationDate` seen). Re-diff is always against
//! the stored snapshot, not the cursor, so nothing is lost on re-sync.
//!
//! **Date encoding:** `creationDate` / `userModificationDate` / `stopDate` are
//! Unix float timestamps (seconds since 1970, UTC). `startDate` and `deadline`
//! use Things' bit-packed date: `year<<16 | month<<12 | day<<7` (little-endian
//! bit decomposition per the things.py / things.sh community schemas). The
//! decoder lives in [`things_date_to_iso`].
//!
//! **Status mapping:** `TMTask.status` 0 = open, 3 = logged/done; `TMTask.type`
//! 0 = task, 1 = project, 2 = heading. We import tasks (type 0) only into the
//! contract; projects (type 1) are loaded separately for the project-name index.
//! Trashed tasks (`trashed=1`) are skipped from the contract but kept in raw.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::browser::import_via_copy;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::tasks::{ProjectInfo, Subtask, Task, TaskFate};
use crate::vault::Vault;

/// Seconds between Things syncs in the watcher loop (15 min, matching other
/// task sources).
pub const THINGS_SYNC_SECS: u64 = 900;

const SYNC_FILE: &str = ".trove/things-sync.json";
const RAW_DIR: &str = "tasks/things/raw";

// ---------------------------------------------------------------------------
// Registry face.

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "Things synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Things sync skipped: {e}"
        ))),
    }
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "full-disk-access",
        granted: Some(things_permission_ok()),
        required: true,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, "things")
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "things",
        name: "Things 3",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description: "Reads your tasks, projects, and areas from the Things 3 \
                       local SQLite database every 15 minutes. Full Disk Access \
                       is required; Things 3 does not need to be closed.",
        domain: "tasks",
        vault_path: "tasks/things/",
        toggleable: true,
        setup: &[
            "System Settings → Privacy & Security → Full Disk Access → add Trove and the troved binary.",
            "Restart the daemon after granting (grants apply to fresh processes only).",
        ],
        caveats: "Full Disk Access must be granted. Beta builds of Things 3 \
                  use a different Group Containers path and are also supported.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(THINGS_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Database location.

/// The home dir to resolve Things paths under: `TROVE_HOME` when set and
/// non-empty (for tests), else the real home dir. Mirrors `bear.rs`.
fn home_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("TROVE_HOME") {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir()
}

/// Things 3 release container ID.
const THINGS_CONTAINER: &str = "JLMPQHK86H.com.culturedcode.ThingsMac";
/// Things 3 beta container ID.
const THINGS_BETA_CONTAINER: &str = "JLMPQHK86H.com.culturedcode.ThingsMac.beta";

/// Locate the `main.sqlite` inside `~/Library/Group Containers/<id>/ThingsData-*/
/// Things Database.thingsdatabase/main.sqlite`. Returns the most-recently-
/// modified one (release preferred over beta when both exist).
pub(crate) fn things_db_path() -> Option<PathBuf> {
    let home = home_root()?;
    let group_containers = home.join("Library/Group Containers");
    let mut candidates: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    for container_id in [THINGS_CONTAINER, THINGS_BETA_CONTAINER] {
        let container = group_containers.join(container_id);
        if !container.is_dir() {
            continue;
        }
        // Walk ThingsData-* subdirectories.
        let Ok(entries) = fs::read_dir(&container) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.starts_with("ThingsData-") {
                continue;
            }
            let db = p
                .join("Things Database.thingsdatabase")
                .join("main.sqlite");
            if db.is_file() {
                let mtime = fs::metadata(&db).ok().and_then(|m| m.modified().ok());
                candidates.push((mtime.unwrap_or(std::time::UNIX_EPOCH), db));
            }
        }
    }
    // Pick the DB most recently modified.
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    candidates.into_iter().next().map(|(_, p)| p)
}

/// Whether this process can open the Things database. False = Full Disk
/// Access not granted (or Things has never been run).
pub fn things_permission_ok() -> bool {
    things_db_path().is_some_and(|p| fs::File::open(p).is_ok())
}

// ---------------------------------------------------------------------------
// Date helpers.

/// Things' bit-packed date encoding (used for `startDate` and `deadline`):
/// `year<<16 | month<<12 | day<<7`.
/// Returns `None` for zero/null values.
///
/// Verified against real DB rows:
/// - 132743040 → 2025-07-31
/// - 132484992 → 2021-08-31
/// - 132742912 → 2025-07-30
pub(crate) fn things_date_to_iso(v: i64) -> Option<String> {
    if v == 0 {
        return None;
    }
    let year = (v >> 16) & 0x7FF;
    let month = (v >> 12) & 0xF;
    let day = (v >> 7) & 0x1F;
    if year == 0 || month == 0 || day == 0 {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Unix float timestamp (seconds since 1970 UTC) → RFC3339 local.
fn unix_float_to_rfc3339(f: f64) -> Option<String> {
    if !f.is_finite() || f <= 0.0 {
        return None;
    }
    let secs = f.trunc() as i64;
    let nanos = (f.fract().abs() * 1_000_000_000.0).round() as u32;
    DateTime::from_timestamp(secs, nanos)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Sync state (cursor).

/// Persisted in `.trove/things-sync.json`. Not a secret; rebuildable from the
/// raw firehose.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ThingsSyncState {
    /// RFC3339 local time of the last successful sync.
    #[serde(default)]
    pub updated: String,
    /// Highest `userModificationDate` (Unix float) seen in the last sync.
    #[serde(default)]
    pub cursor: f64,
}

impl Vault {
    fn read_things_sync(&self) -> ThingsSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_things_sync(&self, state: &ThingsSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape.

/// One raw task row in `tasks/things/raw/YYYY-MM.jsonl`. Partitioned by
/// `creationDate` month; upserted by `uuid`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawTask {
    uuid: String,
    title: String,
    #[serde(rename = "type")]
    kind: i64,
    status: i64,
    trashed: i64,
    creation_date: f64,
    modification_date: f64,
    stop_date: f64,
    start: i64,
    start_date: i64,
    deadline: i64,
    area: String,
    project: String,
    heading: String,
    notes: String,
    /// Tags associated with this task (joined from TMTaskTag+TMTag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    /// Checklist items for this task.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    checklist: Vec<RawChecklistItem>,
}

/// One checklist item (raw).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawChecklistItem {
    uuid: String,
    title: String,
    status: i64,
    stop_date: f64,
    creation_date: f64,
}

// ---------------------------------------------------------------------------
// The DB reader (runs on a copy; called via import_via_copy).

/// Read all tasks from a Things `main.sqlite` copy and return:
/// - all raw task rows (type=0, all statuses including trashed)
/// - project list (type=1, non-trashed) for the contract project index
fn read_db(db: &Path) -> Result<(Vec<RawTask>, Vec<ProjectInfo>)> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("opening Things DB copy {}", db.display()))?;

    // --- tags: uuid → title -----------------------------------------------
    let mut tag_map: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT uuid, title FROM TMTag")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let uuid: String = row.get(0)?;
            let title: String = row.get(1).unwrap_or_default();
            tag_map.insert(uuid, title);
        }
    }

    // --- task-tag join: task-uuid → [tag-titles] --------------------------
    let mut task_tags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut stmt = conn.prepare("SELECT tasks, tags FROM TMTaskTag")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let task_uuid: String = row.get(0)?;
            let tag_uuid: String = row.get(1)?;
            if let Some(name) = tag_map.get(&tag_uuid) {
                task_tags.entry(task_uuid).or_default().push(name.clone());
            }
        }
    }

    // --- checklist items: task-uuid → [items] (ordered by index) ----------
    let mut checklist_map: BTreeMap<String, Vec<RawChecklistItem>> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT uuid, COALESCE(title,''), status, COALESCE(stopDate,0.0), \
                    COALESCE(creationDate,0.0), task \
             FROM TMChecklistItem ORDER BY task, \"index\"",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let item = RawChecklistItem {
                uuid: row.get(0)?,
                title: row.get(1)?,
                status: row.get(2)?,
                stop_date: row.get(3)?,
                creation_date: row.get(4)?,
            };
            let task_uuid: String = row.get(5)?;
            checklist_map.entry(task_uuid).or_default().push(item);
        }
    }

    // --- projects (type=1): build ProjectInfo list for apply_tasks_sync ---
    let mut project_map: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT uuid, COALESCE(title,'') FROM TMTask WHERE type=1 AND trashed=0",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let uuid: String = row.get(0)?;
            let title: String = row.get(1)?;
            project_map.insert(uuid, title);
        }
    }

    // --- all tasks (type=0, all statuses + trashed) -----------------------
    let mut stmt = conn.prepare(
        "SELECT uuid, COALESCE(title,''), type, status, trashed, \
                COALESCE(creationDate,0.0), COALESCE(userModificationDate,0.0), \
                COALESCE(stopDate,0.0), COALESCE(start,0), \
                COALESCE(startDate,0), COALESCE(deadline,0), \
                COALESCE(area,''), COALESCE(project,''), \
                COALESCE(heading,''), COALESCE(notes,'') \
         FROM TMTask WHERE type=0 ORDER BY creationDate",
    )?;
    let mut rows = stmt.query([])?;
    let mut tasks = Vec::new();
    while let Some(row) = rows.next()? {
        let uuid: String = row.get(0)?;
        let tags = task_tags.remove(&uuid).unwrap_or_default();
        let checklist = checklist_map.remove(&uuid).unwrap_or_default();
        tasks.push(RawTask {
            uuid: uuid.clone(),
            title: row.get(1)?,
            kind: row.get(2)?,
            status: row.get(3)?,
            trashed: row.get(4)?,
            creation_date: row.get(5)?,
            modification_date: row.get(6)?,
            stop_date: row.get(7)?,
            start: row.get(8)?,
            start_date: row.get(9)?,
            deadline: row.get(10)?,
            area: row.get(11)?,
            project: row.get(12)?,
            heading: row.get(13)?,
            notes: row.get(14)?,
            tags,
            checklist,
        });
    }

    let projects: Vec<ProjectInfo> = project_map
        .into_iter()
        .map(|(id, name)| ProjectInfo { id, name })
        .collect();

    Ok((tasks, projects))
}

// ---------------------------------------------------------------------------
// Raw upsert (partition by creationDate month).

fn upsert_raw(vault: &Vault, rows: Vec<RawTask>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawTask>> = BTreeMap::new();
    for r in rows {
        let ts = unix_float_to_rfc3339(r.creation_date).unwrap_or_else(|| {
            // Fallback for tasks with no creation date: use the current month.
            Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
        });
        let key = Partition::Month
            .key(&ts)
            .with_context(|| {
                format!("things: raw task creationDate ts {:?} has no month", ts)
            })?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<RawTask> = stream.read(&month)?;
        let mut idx: BTreeMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (r.uuid.clone(), i))
            .collect();
        for r in fresh {
            match idx.get(&r.uuid).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.uuid.clone(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| {
            a.creation_date
                .partial_cmp(&b.creation_date)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.uuid.cmp(&b.uuid))
        });
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// Mapping to the task contract.

/// A raw task row → a normalized [`Task`]. Returns `None` for rows without
/// a uuid or title.
fn raw_to_task(r: &RawTask, project_names: &BTreeMap<String, String>) -> Option<Task> {
    if r.uuid.is_empty() || r.title.is_empty() {
        return None;
    }

    // status 3 = logged/done; 0 = open.
    let status = if r.status == 3 { "done" } else { "open" }.to_string();

    // Resolve project uuid → name.
    let project = project_names.get(&r.project).cloned().unwrap_or_default();

    // `deadline` = the hard due date (Things' terminology).
    let due = things_date_to_iso(r.deadline);

    // `startDate` = the scheduled/activation date → `start` on the contract.
    let start = things_date_to_iso(r.start_date);

    // Completion time.
    let completed = unix_float_to_rfc3339(r.stop_date);

    // Creation / modification times.
    let created = unix_float_to_rfc3339(r.creation_date);
    let modified = unix_float_to_rfc3339(r.modification_date);

    // Subtasks from checklist items.
    let subtasks: Vec<Subtask> = r
        .checklist
        .iter()
        .map(|item| Subtask {
            title: item.title.clone(),
            done: item.status == 3,
            completed: if item.stop_date > 0.0 {
                unix_float_to_rfc3339(item.stop_date)
            } else {
                None
            },
        })
        .collect();

    // Source-specific overflow → extra.
    let mut extra = Map::new();
    if !r.area.is_empty() {
        extra.insert("area_uuid".into(), Value::String(r.area.clone()));
    }
    if !r.project.is_empty() {
        extra.insert("project_uuid".into(), Value::String(r.project.clone()));
    }
    if !r.heading.is_empty() {
        extra.insert("heading_uuid".into(), Value::String(r.heading.clone()));
    }
    // `start` field: 0=Inbox, 1=Anytime, 2=Someday — always write so consumers
    // can distinguish Inbox (0) from absent.
    extra.insert("start_bucket".into(), Value::Number(r.start.into()));

    Some(Task {
        source: "things".into(),
        id: r.uuid.clone(),
        title: r.title.clone(),
        project,
        notes: r.notes.clone(),
        status,
        priority: 0, // Things 3 has no priority field
        due,
        start,
        all_day: true, // Things dates are always day-precision
        recurrence: None,
        tags: r.tags.clone(),
        subtasks,
        created,
        modified,
        completed,
        extra,
    })
}

// ---------------------------------------------------------------------------
// The pull.

/// Entry point for both the periodic collect and the manual "Sync now" button.
/// When the DB is unreadable (no FDA or Things never run), returns a quiet
/// `PullOutcome` with zero counts (no error propagated to the watcher loop).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let Some(db_path) = things_db_path() else {
        return Ok(PullOutcome {
            headline: "Things 3 database not found — grant Full Disk Access".into(),
            counts: {
                let mut m = BTreeMap::new();
                m.insert("open", 0u64);
                m
            },
        });
    };

    // Abort gracefully if not readable (no FDA).
    if fs::File::open(&db_path).is_err() {
        return Ok(PullOutcome {
            headline: "Things 3 database not readable — grant Full Disk Access".into(),
            counts: {
                let mut m = BTreeMap::new();
                m.insert("open", 0u64);
                m
            },
        });
    }

    // Copy-then-open so we never lock the live DB.
    let stem = format!("trove-things-{}", std::process::id());
    let (tasks, projects) = import_via_copy(&db_path, &stem, read_db)?;

    // Build project-uuid → name lookup for contract mapping.
    let project_names: BTreeMap<String, String> = projects
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();

    // --- raw firehose (unconditionally; all tasks including trashed) ------
    let raw_new = upsert_raw(vault, tasks.clone())?;

    // --- contract layer: open, non-trashed tasks only ---------------------
    let fresh: Vec<Task> = tasks
        .iter()
        .filter(|r| r.trashed == 0 && r.status == 0)
        .filter_map(|r| raw_to_task(r, &project_names))
        .collect();

    // Build lookups for fate resolution.
    // Completed = status=3, non-trashed, has a stop_date.
    let completed_by_uuid: BTreeMap<String, String> = tasks
        .iter()
        .filter(|r| r.trashed == 0 && r.status == 3)
        .filter_map(|r| Some((r.uuid.clone(), unix_float_to_rfc3339(r.stop_date)?)))
        .collect();

    let trashed_uuids: std::collections::HashSet<String> = tasks
        .iter()
        .filter(|r| r.trashed != 0)
        .map(|r| r.uuid.clone())
        .collect();

    // Canceled tasks (status=2, non-trashed) must be resolved to Deleted so
    // they leave the open snapshot.  Without this they fall through to Unknown
    // and linger in tasks.jsonl forever.
    let canceled_uuids: std::collections::HashSet<String> = tasks
        .iter()
        .filter(|r| r.trashed == 0 && r.status == 2)
        .map(|r| r.uuid.clone())
        .collect();

    let stats = vault
        .apply_tasks_sync("things", &projects, fresh, |t| {
            if let Some(when) = completed_by_uuid.get(&t.id) {
                TaskFate::Completed(Some(when.clone()))
            } else if trashed_uuids.contains(&t.id) || canceled_uuids.contains(&t.id) {
                TaskFate::Deleted
            } else {
                // Archived in a completed project or other opaque removal:
                // carry forward and retry next sync.
                TaskFate::Unknown
            }
        })
        .context("things: applying task sync")?;

    // Persist cursor.
    let new_cursor = tasks.iter().map(|r| r.modification_date).fold(0f64, f64::max);
    let mut state = vault.read_things_sync();
    state.updated = Local::now().to_rfc3339();
    if new_cursor > state.cursor {
        state.cursor = new_cursor;
    }
    vault.write_things_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open Things tasks", stats.open),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-things-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Date decoder tests (verified against real DB rows).

    #[test]
    fn things_date_decode_known_values() {
        // From real DB: startDate 132743040 = 2025-07-31
        assert_eq!(things_date_to_iso(132743040), Some("2025-07-31".into()));
        // startDate 132484992 = 2021-08-31 (task created on that day)
        assert_eq!(things_date_to_iso(132484992), Some("2021-08-31".into()));
        // startDate 132742912 = 2025-07-30
        assert_eq!(things_date_to_iso(132742912), Some("2025-07-30".into()));
        // Zero = no date
        assert_eq!(things_date_to_iso(0), None);
    }

    #[test]
    fn things_date_decode_bit_structure() {
        // Manually encode 2026-06-15 and decode it back.
        let year = 2026i64;
        let month = 6i64;
        let day = 15i64;
        let encoded = (year << 16) | (month << 12) | (day << 7);
        assert_eq!(things_date_to_iso(encoded), Some("2026-06-15".into()));
    }

    #[test]
    fn unix_float_roundtrip() {
        // 1630426134.1275 = 2021-08-31 16:08:54 UTC
        let t = unix_float_to_rfc3339(1630426134.1275);
        assert!(t.is_some());
        let s = t.unwrap();
        // The date part should be 2021-08-31 regardless of timezone offset.
        assert!(s.starts_with("2021-08-"), "got: {s}");
        // Zero → None
        assert!(unix_float_to_rfc3339(0.0).is_none());
        // Negative → None
        assert!(unix_float_to_rfc3339(-1.0).is_none());
    }

    // -----------------------------------------------------------------------
    // On-disk SQLite fixture.

    /// Build the fixture DB directly on a temp file path.
    /// We open the connection on the target path directly instead of using
    /// the `backup` feature (not compiled in by default). Takes a `tag` so
    /// parallel tests each get a unique file.
    fn fixture_db_path(tag: &str) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!(
            "trove-things-fixture-{}-{tag}.sqlite",
            std::process::id()
        ));
        let _ = fs::remove_file(&path);
        // Open directly on the file path so all DDL/DML persists to disk.
        let conn = rusqlite::Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE TMTag (uuid TEXT PRIMARY KEY, title TEXT, shortcut TEXT,
                usedDate REAL, parent TEXT, \"index\" INTEGER);
             CREATE TABLE TMArea (uuid TEXT PRIMARY KEY, title TEXT, visible INTEGER,
                \"index\" INTEGER);
             CREATE TABLE TMTask (
                uuid TEXT PRIMARY KEY,
                title TEXT,
                type INTEGER DEFAULT 0,
                status INTEGER DEFAULT 0,
                trashed INTEGER DEFAULT 0,
                creationDate REAL,
                userModificationDate REAL,
                stopDate REAL,
                start INTEGER DEFAULT 0,
                startDate INTEGER DEFAULT 0,
                deadline INTEGER DEFAULT 0,
                area TEXT DEFAULT '',
                project TEXT DEFAULT '',
                heading TEXT DEFAULT '',
                notes TEXT DEFAULT ''
             );
             CREATE TABLE TMTaskTag (tasks TEXT, tags TEXT);
             CREATE TABLE TMChecklistItem (
                uuid TEXT PRIMARY KEY,
                title TEXT,
                status INTEGER DEFAULT 0,
                stopDate REAL DEFAULT 0.0,
                creationDate REAL DEFAULT 0.0,
                \"index\" INTEGER DEFAULT 0,
                task TEXT
             );",
        )?;
        // Area.
        conn.execute(
            "INSERT INTO TMArea (uuid, title, visible) VALUES ('area1','Work',1)",
            [],
        )?;
        // Project (type=1).
        conn.execute(
            "INSERT INTO TMTask (uuid, title, type, status, trashed, creationDate, \
                userModificationDate, area) \
             VALUES ('proj1','Work Project',1,0,0,1640000000.0,1641000000.0,'area1')",
            [],
        )?;
        // Tag.
        conn.execute(
            "INSERT INTO TMTag (uuid, title) VALUES ('tag1','urgent')",
            [],
        )?;
        // Open task (type=0) with due date, project, tag, checklist.
        conn.execute(
            "INSERT INTO TMTask (uuid, title, type, status, trashed, creationDate,
                userModificationDate, startDate, deadline, project, area, notes)
             VALUES ('task1','Buy groceries',0,0,0,1640000000.0,1641000000.0,
                     132743040,132743040,'proj1','area1','Pick up veggies')",
            [],
        )?;
        conn.execute("INSERT INTO TMTaskTag (tasks, tags) VALUES ('task1','tag1')", [])?;
        conn.execute(
            "INSERT INTO TMChecklistItem (uuid, title, status, stopDate, creationDate, task)
             VALUES ('ci1','Get spinach',0,0.0,1640000000.0,'task1'),
                    ('ci2','Get carrots',3,1641500000.0,1640000000.0,'task1')",
            [],
        )?;
        // Completed task (status=3).
        conn.execute(
            "INSERT INTO TMTask (uuid, title, type, status, trashed, creationDate,
                userModificationDate, stopDate, project)
             VALUES ('task2','Done task',0,3,0,1639000000.0,1641500000.0,1641500000.0,'proj1')",
            [],
        )?;
        // Trashed task.
        conn.execute(
            "INSERT INTO TMTask (uuid, title, type, status, trashed, creationDate,
                userModificationDate)
             VALUES ('task3','Trashed task',0,0,1,1639000000.0,1641000000.0)",
            [],
        )?;
        // Drop conn so the WAL is flushed before `read_db` opens the file.
        drop(conn);
        Ok(path)
    }

    #[test]
    fn fixture_db_reads_correctly() -> Result<()> {
        let db_path = fixture_db_path("reads")?;
        let (tasks, projects) = read_db(&db_path)?;
        let _ = fs::remove_file(&db_path);

        // 1 project, 3 type=0 tasks (open, done, trashed).
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].name, "Work Project");
        assert_eq!(tasks.len(), 3);

        let t1 = tasks.iter().find(|t| t.uuid == "task1").unwrap();
        assert_eq!(t1.title, "Buy groceries");
        assert_eq!(t1.status, 0);
        assert_eq!(t1.trashed, 0);
        assert_eq!(t1.tags, vec!["urgent"]);
        assert_eq!(t1.checklist.len(), 2);
        assert_eq!(t1.checklist[0].title, "Get spinach");
        assert_eq!(t1.checklist[0].status, 0); // open
        assert_eq!(t1.checklist[1].title, "Get carrots");
        assert_eq!(t1.checklist[1].status, 3); // done (Things status=3 = completed)
        assert_eq!(things_date_to_iso(t1.deadline), Some("2025-07-31".into()));
        assert_eq!(things_date_to_iso(t1.start_date), Some("2025-07-31".into()));
        assert_eq!(t1.notes, "Pick up veggies");
        assert_eq!(t1.project, "proj1");

        let t2 = tasks.iter().find(|t| t.uuid == "task2").unwrap();
        assert_eq!(t2.status, 3); // completed
        assert!(t2.stop_date > 0.0);

        let t3 = tasks.iter().find(|t| t.uuid == "task3").unwrap();
        assert_eq!(t3.trashed, 1);

        Ok(())
    }

    #[test]
    fn mapping_to_contract_task() -> Result<()> {
        let db_path = fixture_db_path("mapping")?;
        let (tasks, projects) = read_db(&db_path)?;
        let _ = fs::remove_file(&db_path);
        let project_names: BTreeMap<String, String> =
            projects.iter().map(|p| (p.id.clone(), p.name.clone())).collect();

        let t1 = tasks.iter().find(|t| t.uuid == "task1").unwrap();
        let task = raw_to_task(t1, &project_names).unwrap();

        assert_eq!(task.source, "things");
        assert_eq!(task.id, "task1");
        assert_eq!(task.title, "Buy groceries");
        assert_eq!(task.project, "Work Project");
        assert_eq!(task.notes, "Pick up veggies");
        assert_eq!(task.status, "open");
        assert_eq!(task.due, Some("2025-07-31".into()));
        assert_eq!(task.start, Some("2025-07-31".into()));
        assert!(task.all_day);
        assert_eq!(task.tags, vec!["urgent"]);
        assert_eq!(task.subtasks.len(), 2);
        assert!(!task.subtasks[0].done);
        assert!(task.subtasks[1].done);
        assert!(task.subtasks[1].completed.is_some());
        assert_eq!(task.priority, 0);
        assert!(task.created.is_some());
        assert!(task.extra.contains_key("project_uuid"));
        assert!(task.extra.contains_key("area_uuid"));

        Ok(())
    }

    #[test]
    fn full_sync_applies_contract_and_writes_raw() -> Result<()> {
        let db_path = fixture_db_path("full-sync")?;
        let (tasks, projects) = read_db(&db_path)?;
        let _ = fs::remove_file(&db_path);
        let project_names: BTreeMap<String, String> =
            projects.iter().map(|p| (p.id.clone(), p.name.clone())).collect();

        let vault = temp_vault("full_sync");

        // Raw upsert includes all tasks (even trashed/done).
        let raw_new = upsert_raw(&vault, tasks.clone())?;
        assert_eq!(raw_new, 3);

        // Contract sync: open + non-trashed only.
        let fresh: Vec<Task> = tasks
            .iter()
            .filter(|r| r.trashed == 0 && r.status == 0)
            .filter_map(|r| raw_to_task(r, &project_names))
            .collect();
        assert_eq!(fresh.len(), 1); // only task1

        let completed_by_uuid: BTreeMap<String, String> = tasks
            .iter()
            .filter(|r| r.trashed == 0 && r.status == 3)
            .filter_map(|r| Some((r.uuid.clone(), unix_float_to_rfc3339(r.stop_date)?)))
            .collect();

        let trashed_uuids: HashSet<String> = tasks
            .iter()
            .filter(|r| r.trashed != 0)
            .map(|r| r.uuid.clone())
            .collect();

        let stats = vault
            .apply_tasks_sync("things", &projects, fresh, |t| {
                if let Some(when) = completed_by_uuid.get(&t.id) {
                    TaskFate::Completed(Some(when.clone()))
                } else if trashed_uuids.contains(&t.id) {
                    TaskFate::Deleted
                } else {
                    TaskFate::Unknown
                }
            })
            .context("apply_tasks_sync")?;

        assert_eq!(stats.open, 1);
        assert_eq!(stats.created, 1); // task1 is new

        // tasks.jsonl written.
        let snapshot_path = vault.root().join("tasks/things/tasks.jsonl");
        assert!(snapshot_path.exists());
        let snap_body = fs::read_to_string(&snapshot_path)?;
        assert!(snap_body.contains("\"id\":\"task1\""));
        assert!(snap_body.contains("\"source\":\"things\""));
        // Completed task not in open snapshot.
        assert!(!snap_body.contains("\"id\":\"task2\""));

        // Raw JSONL written.
        let raw_dir = vault.root().join("tasks/things/raw");
        assert!(raw_dir.exists());
        let raw_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect();
        assert!(!raw_files.is_empty(), "raw JSONL should be written");

        Ok(())
    }

    #[test]
    fn second_sync_detects_completion() -> Result<()> {
        let db_path = fixture_db_path("completion")?;
        let (tasks, projects) = read_db(&db_path)?;
        let _ = fs::remove_file(&db_path);
        let project_names: BTreeMap<String, String> =
            projects.iter().map(|p| (p.id.clone(), p.name.clone())).collect();

        let vault = temp_vault("completion");

        // First sync: task1 is open.
        let fresh1: Vec<Task> = tasks
            .iter()
            .filter(|r| r.trashed == 0 && r.status == 0)
            .filter_map(|r| raw_to_task(r, &project_names))
            .collect();
        vault
            .apply_tasks_sync("things", &projects, fresh1, |_| TaskFate::Unknown)
            .context("sync1")?;

        // Second sync: task1 disappeared (now completed).
        let completed_by_uuid: BTreeMap<String, String> = [(
            "task1".to_string(),
            "2026-06-15T10:00:00-07:00".to_string(),
        )]
        .into();
        let stats = vault
            .apply_tasks_sync("things", &projects, Vec::new(), |t| {
                if let Some(when) = completed_by_uuid.get(&t.id) {
                    TaskFate::Completed(Some(when.clone()))
                } else {
                    TaskFate::Unknown
                }
            })
            .context("sync2")?;

        assert_eq!(stats.completed, 1);
        assert_eq!(stats.open, 0);

        // Event logged.
        let events = vault.task_events("2026-06-01", "2026-06-30")?;
        let completed: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task.title, "Buy groceries");

        Ok(())
    }

    /// Regression: canceled tasks (status=2, trashed=0) must leave the open
    /// snapshot on the sync they transition open→canceled, not linger forever.
    #[test]
    fn canceled_task_leaves_open_snapshot() -> Result<()> {
        let project_names: BTreeMap<String, String> = BTreeMap::new();
        let projects: Vec<ProjectInfo> = Vec::new();
        let vault = temp_vault("canceled");

        // Synthesise a minimal open RawTask.
        let open_task = RawTask {
            uuid: "ctask1".into(),
            title: "Cancel me".into(),
            kind: 0,
            status: 0,
            trashed: 0,
            creation_date: 1640000000.0,
            modification_date: 1641000000.0,
            stop_date: 0.0,
            start_date: 0,
            deadline: 0,
            project: String::new(),
            area: String::new(),
            heading: String::new(),
            notes: String::new(),
            start: 0,
            tags: Vec::new(),
            checklist: Vec::new(),
        };

        // First sync: task is open.
        let fresh1: Vec<Task> = vec![raw_to_task(&open_task, &project_names).unwrap()];
        let stats1 = vault
            .apply_tasks_sync("things", &projects, fresh1, |_| TaskFate::Unknown)
            .context("sync1")?;
        assert_eq!(stats1.open, 1);

        // Second sync: task is now canceled (status=2).  It is absent from fresh
        // (status==0 filter) and present in canceled_uuids → Deleted.
        let canceled_task = RawTask { status: 2, ..open_task };
        let canceled_uuids: std::collections::HashSet<String> =
            std::iter::once(canceled_task.uuid.clone()).collect();
        let stats2 = vault
            .apply_tasks_sync("things", &projects, Vec::new(), |t| {
                if canceled_uuids.contains(&t.id) {
                    TaskFate::Deleted
                } else {
                    TaskFate::Unknown
                }
            })
            .context("sync2")?;
        assert_eq!(stats2.open, 0, "canceled task must leave open snapshot");
        assert_eq!(stats2.deleted, 1, "canceled task must be counted as deleted");

        Ok(())
    }

    #[test]
    fn fda_unreadable_returns_graceful_no_op() {
        // With TROVE_HOME pointing at a non-existent dir, things_db_path() is
        // None — pull returns zero counts and no error.
        std::env::set_var("TROVE_HOME", "/tmp/no-such-home-things-test");
        let vault = temp_vault("fda_no_op");
        let out = pull(&vault).unwrap();
        assert_eq!(out.counts.get("open").copied().unwrap_or(0), 0);
        std::env::remove_var("TROVE_HOME");
    }
}
