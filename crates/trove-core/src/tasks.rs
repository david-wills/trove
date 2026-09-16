//! Task collector — a normalized, source-agnostic task store plus the
//! TickTick sync, the first M5 ("cloud API pull of your own data") source.
//!
//! Layout, per source, under `tasks/<source>/` (e.g. `tasks/ticktick/`):
//!
//! - `tasks.jsonl` — the current snapshot of open tasks in the normalized
//!   schema below, rewritten on every sync. One task per line.
//! - `events/YYYY-MM.jsonl` — append-only event stream: the same task shape
//!   plus `{"time": …, "kind": "completed"|"created"|"deleted"}`. **This is
//!   the stream that cannot be backfilled**: TickTick's official API exposes
//!   only open tasks, so completion history exists only because each sync
//!   diffs the new snapshot against the previous one and logs what changed.
//! - `<project>.md` + `index.md` — human-readable checklists, regenerated
//!   each sync.
//!
//! The normalized schema is the multi-source contract: a collector for *any*
//! to-do app — Rust module, script, or AI agent — plugs in by writing these
//! files under its own `tasks/<source>/` directory; every field except
//! `source`/`id`/`title` defaults, so sparse sources (a CSV export converter)
//! can write minimal lines. Source-specific fields ride along in `extra`,
//! full fidelity at write time. Readers scan all of `tasks/*/`.
//!
//! Snapshot diffing: a task that disappears between syncs was either
//! completed or deleted; its fate is resolved by asking the source (TickTick:
//! fetch by id — completed tasks still resolve, deleted ones 404). When the
//! lookup fails transiently the task is carried forward and retried next
//! sync — fates are never guessed. A recurring task signals an instance
//! completion by advancing its `completed` time while staying open.
//!
//! Sync runs inside the watcher owner loop (see [`crate::runner`]), so the
//! single-writer lock covers the task stream too. OAuth (connect flow, token
//! store) is owned by [`crate::sync`] — this module only *reads* the saved
//! token, and machines without one silently skip the sync. The manual
//! `ticktick_pull` (Sync tab) delegates here, so there is exactly one writer
//! of `tasks/ticktick/` and a manual pull can never clobber the diff
//! baseline.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health::SeriesPoint;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::{slugify, Vault};

/// Seconds between task syncs in the watcher loop.
pub const TASKS_SYNC_SECS: u64 = 900;

// Both task sources: silent no-ops without their prerequisite (TickTick
// token / Reminders TCC grant).
fn ticktick_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_tasks()?;
    Ok(crate::registry::CollectOutcome::note_if(
        s.created + s.completed + s.deleted > 0,
        || {
            format!(
                "tasks synced — {} completed, {} created, {} deleted",
                s.completed, s.created, s.deleted
            )
        },
    ))
}

fn reminders_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_reminders()?;
    Ok(crate::registry::CollectOutcome::note_if(
        s.created + s.completed + s.deleted > 0,
        || {
            format!(
                "reminders synced — {} completed, {} created, {} deleted",
                s.completed, s.created, s.deleted
            )
        },
    ))
}

fn reminders_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "reminders",
        granted: Some(
            crate::eventkit::reminders_auth_status() == crate::eventkit::AuthStatus::Granted,
        ),
        required: true,
    }
}

/// Shared `last_data` hook body for any source in `.trove/tasks-sync.json`
/// (also used by [`crate::google_tasks`]).
pub(crate) fn source_last_data(vault: &Vault, source: &str) -> Option<String> {
    vault
        .read_tasks_sync()
        .and_then(|s| s.sources.get(source).map(|src| src.updated.clone()))
        .filter(|u| !u.is_empty())
}

fn reminders_last_data(vault: &Vault) -> Option<String> {
    source_last_data(vault, "apple-reminders")
}

fn ticktick_last_data(vault: &Vault) -> Option<String> {
    source_last_data(vault, "ticktick")
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static REMINDERS_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-reminders",
        name: "Apple Reminders",
        kind: IntegrationKind::LocalSync,
        default_on: true,
        description: "Open reminders and real completion times via EventKit, into the same normalized task store as TickTick.",
        domain: "tasks",
        vault_path: "tasks/apple-reminders/",
        toggleable: true,
        setup: &[
            "Approve the Reminders access prompt the first time the app asks for Full Access.",
            "If the prompt was declined: System Settings → Privacy & Security → Reminders → set Trove to Full Access.",
        ],
        caveats: "Completions are read from the Reminders store with their true times (90-day lookback), so brief collection gaps don't lose them. Completing one instance of a recurring reminder isn't logged as a completion yet.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(TASKS_SYNC_SECS), collect: reminders_collect },
    permission: Some(reminders_permission),
    last_data: Some(reminders_last_data),
    connection: None,
    pull: None,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static TICKTICK_DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ticktick",
        name: "TickTick",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Open-task snapshot plus a completion stream, pulled every 15 minutes through the official API.",
        domain: "tasks",
        vault_path: "tasks/ticktick/",
        toggleable: true,
        setup: &[],
        caveats: "The official API returns only open tasks — completions are reconstructed by diffing, so completion history accrues only while the sync runs. Tokens last ~180 days with no refresh (expiry means reconnecting). Inbox, habits, and focus records aren't in the API.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(TASKS_SYNC_SECS), collect: ticktick_collect },
    permission: None,
    last_data: Some(ticktick_last_data),
    connection: Some("ticktick"),
    pull: Some(crate::sync::ticktick::pull),
};

const SYNC_FILE: &str = ".trove/tasks-sync.json";
const SUMMARY_FILE: &str = ".trove/tasks-summary.json";
const TICKTICK_API: &str = "https://api.ticktick.com/open/v1";

/// Kept short because the sync runs inside the watcher owner loop: a hung
/// connection must not stall activity sampling for long (the watcher's
/// sleep-gap logic closes the open event cleanly if it does, but cheaply
/// bounded beats gracefully degraded).
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// One task, in the normalized cross-source schema. Times are RFC3339 local.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Task {
    /// Which collector wrote this ("ticktick", "reminders", …).
    pub source: String,
    /// Source-native task id.
    pub id: String,
    pub title: String,
    /// Project / list name, verbatim from the source (may carry emoji).
    #[serde(default)]
    pub project: String,
    /// Free-form notes/body.
    #[serde(default)]
    pub notes: String,
    /// "open" or "done" (snapshots normally hold only open tasks).
    #[serde(default = "default_status")]
    pub status: String,
    /// TickTick scale: 0 none, 1 low, 3 medium, 5 high. Other sources map in.
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(default)]
    pub all_day: bool,
    /// Raw RRULE when the task repeats.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recurrence: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtasks: Vec<Subtask>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    /// For a recurring task this is the last instance completion — the diff
    /// watches it advance to log instance completions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<String>,
    /// Everything source-specific the normalized fields don't carry
    /// (TickTick: projectId, columnId, etag, sortOrder, timeZone, …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

fn default_status() -> String {
    "open".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Subtask {
    pub title: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed: Option<String>,
}

/// One line of the append-only event stream: a task plus when/what happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    /// RFC3339 local time of the event.
    pub time: String,
    /// "completed", "created", or "deleted".
    pub kind: String,
    #[serde(flatten)]
    pub task: Task,
}

/// What became of a task that disappeared from the source between syncs.
pub enum TaskFate {
    /// Completed, with the source's completion time when it knows one.
    Completed(Option<String>),
    Deleted,
    /// Couldn't find out (network error, …) — carry the task forward and
    /// retry next sync.
    Unknown,
}

/// A project/list as the source reports it (kept even when empty, so the
/// human index shows lists with nothing open).
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
}

/// Per-project open count for the overview.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ProjectCount {
    pub source: String,
    pub project: String,
    pub open: u64,
}

/// Aggregate for the Tasks view, across all sources.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct TasksOverview {
    pub open: u64,
    pub due_today: u64,
    pub overdue: u64,
    /// Completions logged in the last 7 days (event stream).
    pub completed_7d: u64,
    /// Projects with open tasks, descending by count.
    pub projects: Vec<ProjectCount>,
}

/// Result of one sync pass, for logging/status.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TasksSyncStats {
    pub projects: u32,
    pub open: u64,
    pub created: u64,
    pub completed: u64,
    pub deleted: u64,
}

/// Per-source sync status, persisted in `.trove/tasks-sync.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct SourceSync {
    /// RFC3339 local time of the last successful sync of this source.
    pub updated: String,
    /// Why the last attempt failed (e.g. an expired token), for the UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct TasksSyncState {
    /// RFC3339 local time of the last sync attempt, any source.
    pub updated: String,
    pub sources: BTreeMap<String, SourceSync>,
}

/// Outcome of diffing a fresh pull against the previous snapshot.
struct SnapshotDiff {
    /// The new snapshot to persist (fresh tasks + unknown-fate carryovers).
    snapshot: Vec<Task>,
    events: Vec<TaskEvent>,
}

/// The diff engine: pure, no I/O, fates injected — the testable heart of the
/// completion stream (the [`crate::activity::Watcher`] pattern).
fn diff_snapshot(
    prev: &[Task],
    fresh: Vec<Task>,
    now: &str,
    mut fate: impl FnMut(&Task) -> TaskFate,
) -> SnapshotDiff {
    let prev_by_id: BTreeMap<&str, &Task> = prev.iter().map(|t| (t.id.as_str(), t)).collect();
    let fresh_ids: HashSet<String> = fresh.iter().map(|t| t.id.clone()).collect();
    let mut events = Vec::new();

    for task in &fresh {
        match prev_by_id.get(task.id.as_str()) {
            None => events.push(TaskEvent {
                // A brand-new task; stamp it with its own creation time when
                // the source reports one (first sync of a new source then
                // backfills creations into their true months).
                time: task.created.clone().unwrap_or_else(|| now.to_string()),
                kind: "created".into(),
                task: task.clone(),
            }),
            Some(before) => {
                // Still present but its completion time advanced: a recurring
                // task's instance was completed (the task itself stays open
                // with the next occurrence's due date).
                if task.completed.is_some() && task.completed != before.completed {
                    events.push(TaskEvent {
                        time: task.completed.clone().unwrap(),
                        kind: "completed".into(),
                        task: task.clone(),
                    });
                }
            }
        }
    }

    let mut snapshot = fresh;
    for before in prev {
        if fresh_ids.contains(before.id.as_str()) {
            continue;
        }
        match fate(before) {
            TaskFate::Completed(time) => {
                let time = time.unwrap_or_else(|| now.to_string());
                let mut task = before.clone();
                task.status = "done".into();
                task.completed = Some(time.clone());
                events.push(TaskEvent {
                    time,
                    kind: "completed".into(),
                    task,
                });
            }
            TaskFate::Deleted => events.push(TaskEvent {
                time: now.to_string(),
                kind: "deleted".into(),
                task: before.clone(),
            }),
            TaskFate::Unknown => snapshot.push(before.clone()),
        }
    }
    SnapshotDiff { snapshot, events }
}

/// A TickTick timestamp ("2026-07-05T16:30:00.000+0000") → RFC3339 local.
/// Unparseable values pass through verbatim rather than being dropped.
fn ticktick_time(s: &str) -> String {
    DateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.3f%z")
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Consume a string field off a raw API object, leaving non-strings in
/// place so they survive into `extra` (shared with [`crate::google_tasks`]).
pub(crate) fn take_str(obj: &mut Map<String, Value>, key: &str) -> Option<String> {
    match obj.remove(key)? {
        Value::String(s) => Some(s),
        other => {
            // Not a string — put it back rather than lose it.
            obj.insert(key.into(), other);
            None
        }
    }
}

fn take_time(obj: &mut Map<String, Value>, key: &str) -> Option<String> {
    take_str(obj, key).map(|s| ticktick_time(&s))
}

/// A raw TickTick API task object → normalized [`Task`]. Mapped fields are
/// consumed; everything else (projectId, columnId, etag, sortOrder, …) lands
/// in `extra`. Returns None for objects without an id or title.
fn normalize_ticktick(value: Value) -> Option<Task> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let id = take_str(&mut obj, "id")?;
    let title = take_str(&mut obj, "title")?;
    let status = match obj.remove("status").and_then(|v| v.as_i64()) {
        Some(2) => "done".to_string(),
        _ => default_status(),
    };
    let subtasks = match obj.remove("items") {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| {
                let Value::Object(mut it) = item else {
                    return None;
                };
                Some(Subtask {
                    title: take_str(&mut it, "title")?,
                    done: it.get("status").and_then(|v| v.as_i64()) == Some(1),
                    completed: it
                        .get("completedTime")
                        .and_then(|v| v.as_i64())
                        .and_then(DateTime::from_timestamp_millis)
                        .map(|t| t.with_timezone(&Local).to_rfc3339()),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    let tags = match obj.remove("tags") {
        Some(Value::Array(ts)) => ts
            .into_iter()
            .filter_map(|t| match t {
                Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some(Task {
        source: "ticktick".into(),
        id,
        title,
        project: take_str(&mut obj, "projectName").unwrap_or_default(),
        notes: take_str(&mut obj, "content").unwrap_or_default(),
        status,
        priority: obj.remove("priority").and_then(|v| v.as_i64()).unwrap_or(0),
        due: take_time(&mut obj, "dueDate"),
        start: take_time(&mut obj, "startDate"),
        all_day: obj
            .remove("isAllDay")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        recurrence: take_str(&mut obj, "repeatFlag"),
        tags,
        subtasks,
        created: take_time(&mut obj, "createdTime"),
        modified: take_time(&mut obj, "modifiedTime"),
        completed: take_time(&mut obj, "completedTime"),
        extra: obj,
    })
}

/// One task as a markdown checklist line (+ indented subtask lines):
/// `- [ ] Title · due 2026-06-12 · !high · #tag`.
fn task_markdown_line(t: &Task) -> String {
    let mut line = format!("- [ ] {}", t.title.trim());
    if let Some(due) = &t.due {
        line.push_str(&format!(" · due {}", &due[..10.min(due.len())]));
    }
    match t.priority {
        5 => line.push_str(" · !high"),
        3 => line.push_str(" · !medium"),
        1 => line.push_str(" · !low"),
        _ => {}
    }
    for tag in &t.tags {
        line.push_str(&format!(" · #{tag}"));
    }
    line.push('\n');
    for st in &t.subtasks {
        line.push_str(&format!(
            "  - [{}] {}\n",
            if st.done { 'x' } else { ' ' },
            st.title.trim()
        ));
    }
    line
}

/// "ticktick" → "TickTick"; otherwise just capitalize.
fn display_name(source: &str) -> String {
    match source {
        "ticktick" => "TickTick".into(),
        "google-tasks" => "Google Tasks".into(),
        other => {
            let mut cs = other.chars();
            match cs.next() {
                Some(f) => f.to_uppercase().collect::<String>() + cs.as_str(),
                None => String::new(),
            }
        }
    }
}

/// Thin TickTick Open API client. Kept out of the diff/writer paths so the
/// whole sync below it stays testable without a network. (Deliberately
/// separate from `sync::ticktick`'s helpers: fate resolution needs
/// status-code-level errors, which the anyhow-flattened path loses.)
struct TickTick {
    token: String,
}

impl TickTick {
    fn get(&self, path: &str) -> std::result::Result<Value, ureq::Error> {
        ureq::get(&format!("{TICKTICK_API}{path}"))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT)
            .call()?
            .into_json()
            .map_err(ureq::Error::from)
    }

    fn projects(&self) -> std::result::Result<Vec<ProjectInfo>, ureq::Error> {
        let Value::Array(ps) = self.get("/project")? else {
            return Ok(Vec::new());
        };
        Ok(ps
            .into_iter()
            .filter_map(|p| {
                Some(ProjectInfo {
                    id: p.get("id")?.as_str()?.to_string(),
                    name: p.get("name")?.as_str()?.to_string(),
                })
            })
            .collect())
    }

    /// All open tasks of one project, normalized.
    fn project_tasks(&self, project: &ProjectInfo) -> std::result::Result<Vec<Task>, ureq::Error> {
        let data = self.get(&format!("/project/{}/data", project.id))?;
        let tasks = match data.get("tasks") {
            Some(Value::Array(ts)) => ts.clone(),
            _ => Vec::new(),
        };
        Ok(tasks
            .into_iter()
            .filter_map(normalize_ticktick)
            .map(|mut t| {
                // /project/{id}/data tasks don't carry a project name.
                t.project = project.name.clone();
                t
            })
            .collect())
    }

    /// Resolve a disappeared task. Completed tasks still fetch by id (status
    /// 2, with their completion time); purged ones 404. Quirk found probing
    /// the real API: a *trashed* task also still fetches by id — 200, status
    /// 0, projectId unchanged — so "open by id, same project, yet missing
    /// from that project's data" means the trash (or status -1, abandoned)
    /// and maps to Deleted; without that rule trashed tasks would be carried
    /// forward and re-checked forever. A *different* projectId instead means
    /// the task moved somewhere this token can't see (e.g. the Inbox, which
    /// `/project` never lists) — keep it and retry next sync.
    fn fate(&self, task: &Task) -> TaskFate {
        let Some(pid) = task.extra.get("projectId").and_then(Value::as_str) else {
            return TaskFate::Unknown;
        };
        match self.get(&format!("/project/{pid}/task/{}", task.id)) {
            Ok(v) => match v.get("status").and_then(Value::as_i64) {
                Some(2) => {
                    let time = v
                        .get("completedTime")
                        .and_then(Value::as_str)
                        .map(ticktick_time);
                    TaskFate::Completed(time)
                }
                _ if v.get("projectId").and_then(Value::as_str) == Some(pid) => TaskFate::Deleted,
                _ => TaskFate::Unknown,
            },
            Err(ureq::Error::Status(404, _)) => TaskFate::Deleted,
            Err(_) => TaskFate::Unknown,
        }
    }
}

impl Vault {
    /// One TickTick sync pass: pull every project's open tasks, diff against
    /// the previous snapshot (logging completions/creations/deletions to the
    /// event stream), and rewrite the snapshot + human files. A silent no-op
    /// when no token is provisioned; per-project failures are logged and the
    /// affected tasks carried forward, never fatal.
    pub fn collect_tasks(&self) -> Result<TasksSyncStats> {
        let Some(token) = self.load_sync_token("ticktick")? else {
            return Ok(TasksSyncStats::default());
        };
        if token.expired() {
            let msg = "TickTick token expired — reconnect from the Sync tab";
            self.record_tasks_sync("ticktick", Some(msg.to_string()))?;
            return Err(anyhow!(msg));
        }
        let api = TickTick {
            token: token.access_token,
        };
        let projects = match api.projects() {
            Ok(ps) => ps,
            Err(e) => {
                let msg = match &e {
                    ureq::Error::Status(401, _) => {
                        "TickTick rejected the token (401) — reconnect from the Sync tab".into()
                    }
                    other => format!("TickTick project list failed: {other}"),
                };
                self.record_tasks_sync("ticktick", Some(msg.clone()))?;
                return Err(anyhow!(msg));
            }
        };
        let mut fresh = Vec::new();
        let mut failed_projects: HashSet<String> = HashSet::new();
        for p in &projects {
            match api.project_tasks(p) {
                Ok(ts) => fresh.extend(ts),
                Err(e) => {
                    eprintln!("trove tasks: ticktick project {:?} failed: {e}", p.name);
                    failed_projects.insert(p.id.clone());
                }
            }
        }
        let stats = self.apply_tasks_sync("ticktick", &projects, fresh, |t| {
            // A failed project pull explains every "missing" task in it —
            // don't burn fate lookups (or log spurious deletions) on those.
            match t.extra.get("projectId").and_then(Value::as_str) {
                Some(pid) if failed_projects.contains(pid) => TaskFate::Unknown,
                _ => api.fate(t),
            }
        })?;
        // Also feed the Sync tab's per-service status line.
        self.record_sync(
            "ticktick",
            &crate::sync::SyncReport {
                projects: stats.projects as usize,
                tasks: stats.open as usize,
            },
        )?;
        Ok(stats)
    }

    /// Diff a fresh pull against the stored snapshot and persist everything:
    /// events appended, snapshot rewritten, markdown + machine indexes
    /// regenerated, sync state recorded. Source-agnostic — the network (or
    /// EventKit, for Reminders) lives in the caller; tests drive this
    /// directly.
    pub(crate) fn apply_tasks_sync(
        &self,
        source: &str,
        projects: &[ProjectInfo],
        fresh: Vec<Task>,
        fate: impl FnMut(&Task) -> TaskFate,
    ) -> Result<TasksSyncStats> {
        let now = Local::now().to_rfc3339();
        let prev = self.load_tasks_snapshot(source)?;
        let diff = diff_snapshot(&prev, fresh, &now, fate);
        self.append_task_events(source, &diff.events)?;
        self.write_tasks_snapshot(source, &diff.snapshot)?;
        self.write_tasks_markdown(source, projects, &diff.snapshot, &now)?;
        self.write_tasks_summary(&now)?;
        self.record_tasks_sync(source, None)?;
        let count = |kind: &str| diff.events.iter().filter(|e| e.kind == kind).count() as u64;
        Ok(TasksSyncStats {
            projects: projects.len() as u32,
            open: diff.snapshot.len() as u64,
            created: count("created"),
            completed: count("completed"),
            deleted: count("deleted"),
        })
    }

    /// The stored snapshot of a source's open tasks. Lenient on shape: lines
    /// without a `source` field in a ticktick dir are raw TickTick API
    /// objects (the pre-app agent dump) and normalize on the way in, so the
    /// existing dump is a valid diff baseline and readable by the UI as-is.
    pub fn load_tasks_snapshot(&self, source: &str) -> Result<Vec<Task>> {
        let path = self.resolve(&format!("tasks/{source}/tasks.jsonl"))?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)
            .with_context(|| format!("reading tasks/{source}/tasks.jsonl"))?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let v: Value = serde_json::from_str(l).ok()?;
                if v.get("source").is_some() {
                    serde_json::from_value(v).ok()
                } else if source == "ticktick" {
                    normalize_ticktick(v)
                } else {
                    None
                }
            })
            .collect())
    }

    fn write_tasks_snapshot(&self, source: &str, tasks: &[Task]) -> Result<()> {
        self.write_snapshot(&format!("tasks/{source}/tasks.jsonl"), tasks)
    }

    /// Append events to their month's JSONL log (keyed by local event month).
    /// `pub(crate)` so [`crate::google_tasks`] can seed pre-connect
    /// completion history outside a diff pass.
    pub(crate) fn append_task_events(&self, source: &str, events: &[TaskEvent]) -> Result<()> {
        self.stream(&format!("tasks/{source}/events"), crate::store::Partition::Month)
            .append(events, |e| &e.time)
    }

    /// Source directories under `tasks/` — anything that writes the spec
    /// format shows up, no registration needed.
    fn tasks_sources(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.root().join("tasks")) else {
            return Vec::new();
        };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    /// All open tasks across every source.
    pub fn tasks_list(&self) -> Result<Vec<Task>> {
        let mut out = Vec::new();
        for source in self.tasks_sources() {
            out.extend(self.load_tasks_snapshot(&source)?);
        }
        Ok(out)
    }

    /// Task events across every source over an inclusive date range,
    /// chronological.
    pub fn task_events(&self, from: &str, to: &str) -> Result<Vec<TaskEvent>> {
        let (from_month, to_month) = (&from[..7.min(from.len())], &to[..7.min(to.len())]);
        let mut out = Vec::new();
        for source in self.tasks_sources() {
            let dir = self.root().join("tasks").join(&source).join("events");
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                    continue;
                }
                let Some(month) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if month < from_month || month > to_month {
                    continue;
                }
                let Ok(body) = fs::read_to_string(&path) else {
                    continue;
                };
                out.extend(
                    body.lines()
                        .filter_map(|l| serde_json::from_str::<TaskEvent>(l).ok())
                        .filter(|e| {
                            let day = &e.time[..10.min(e.time.len())];
                            day >= from && day <= to
                        }),
                );
            }
        }
        out.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(out)
    }

    /// Completions per day over an inclusive range — a trend series for the
    /// chart. Days with no completions are omitted.
    pub fn tasks_completed_daily(&self, from: &str, to: &str) -> Result<Vec<SeriesPoint>> {
        let mut per_day: BTreeMap<String, u64> = BTreeMap::new();
        for e in self.task_events(from, to)? {
            if e.kind == "completed" {
                *per_day.entry(e.time[..10].to_string()).or_default() += 1;
            }
        }
        Ok(per_day
            .into_iter()
            .map(|(date, n)| SeriesPoint {
                date,
                value: n as f64,
            })
            .collect())
    }

    /// The headline numbers for the Tasks view, across all sources.
    pub fn tasks_overview(&self) -> Result<TasksOverview> {
        let today = Local::now().format("%Y-%m-%d").to_string();
        let week_ago = (Local::now() - chrono::Duration::days(6))
            .format("%Y-%m-%d")
            .to_string();
        let mut open = 0u64;
        let mut due_today = 0u64;
        let mut overdue = 0u64;
        let mut projects: BTreeMap<(String, String), u64> = BTreeMap::new();
        for t in self.tasks_list()? {
            if t.status != "open" {
                continue;
            }
            open += 1;
            *projects
                .entry((t.source.clone(), t.project.clone()))
                .or_default() += 1;
            if let Some(due) = &t.due {
                let day = &due[..10.min(due.len())];
                if day == today {
                    due_today += 1;
                } else if day < today.as_str() {
                    overdue += 1;
                }
            }
        }
        let completed_7d = self
            .task_events(&week_ago, &today)?
            .iter()
            .filter(|e| e.kind == "completed")
            .count() as u64;
        let mut projects: Vec<ProjectCount> = projects
            .into_iter()
            .map(|((source, project), open)| ProjectCount {
                source,
                project,
                open,
            })
            .collect();
        projects.sort_by(|a, b| b.open.cmp(&a.open).then_with(|| a.project.cmp(&b.project)));
        Ok(TasksOverview {
            open,
            due_today,
            overdue,
            completed_7d,
            projects,
        })
    }

    /// The persisted sync state, if a sync has ever run.
    pub fn read_tasks_sync(&self) -> Option<TasksSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Record the outcome of a sync attempt for one source (error = None on
    /// success). Atomic write so readers never see a torn file. `pub(crate)`
    /// so [`crate::google_tasks`] can record an all-accounts-failed error.
    pub(crate) fn record_tasks_sync(&self, source: &str, error: Option<String>) -> Result<()> {
        let mut state = self.read_tasks_sync().unwrap_or_default();
        let now = Local::now().to_rfc3339();
        state.updated = now.clone();
        let entry = state.sources.entry(source.to_string()).or_default();
        if error.is_none() {
            entry.updated = now;
        }
        entry.error = error;
        let path = self.resolve(SYNC_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&state)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Regenerate the human-readable layer: one checklist per project plus
    /// an index table. Stale project files (renamed/deleted projects) are
    /// removed; duplicate project names get deduplicated slugs.
    fn write_tasks_markdown(
        &self,
        source: &str,
        projects: &[ProjectInfo],
        tasks: &[Task],
        synced: &str,
    ) -> Result<()> {
        let mut index = format!(
            "# {}\n\nLast sync: {synced}\n\n| Project | Open tasks |\n|---|---|\n",
            display_name(source)
        );
        let mut used_slugs: HashSet<String> = HashSet::new();
        for p in projects {
            let open: Vec<&Task> = tasks
                .iter()
                .filter(|t| t.project == p.name && t.status == "open")
                .collect();
            let mut slug = slugify(&p.name);
            let mut n = 1;
            while !used_slugs.insert(slug.clone()) {
                n += 1;
                slug = format!("{}-{n}", slugify(&p.name));
            }
            index.push_str(&format!("| [{}]({slug}.md) | {} |\n", p.name, open.len()));

            let mut md = format!(
                "---\nsource: {source}\n{source}_id: {}\nproject: \"{}\"\nopen_tasks: {}\nsynced: {synced}\n---\n\n# {}\n\n",
                p.id,
                p.name.replace('"', "\\\""),
                open.len(),
                p.name
            );
            for t in &open {
                md.push_str(&task_markdown_line(t));
            }
            fs::write(self.resolve(&format!("tasks/{source}/{slug}.md"))?, md)?;
        }
        // Remove markdown for projects that no longer exist (the events/ dir
        // and tasks.jsonl are untouched — only the regenerated layer churns).
        used_slugs.insert("index".into());
        let dir = self.root().join("tasks").join(source);
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) == Some("md")
                    && path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| !used_slugs.contains(s))
                {
                    let _ = fs::remove_file(&path);
                }
            }
        }
        fs::write(self.resolve(&format!("tasks/{source}/index.md"))?, index)?;
        Ok(())
    }

    /// Machine catalog across all sources — an index, rebuildable from the
    /// snapshots at any time.
    fn write_tasks_summary(&self, now: &str) -> Result<()> {
        let mut sources = Vec::new();
        for source in self.tasks_sources() {
            let tasks = self.load_tasks_snapshot(&source)?;
            let mut projects: BTreeMap<&str, u64> = BTreeMap::new();
            for t in &tasks {
                *projects.entry(t.project.as_str()).or_default() += 1;
            }
            sources.push(serde_json::json!({
                "source": source,
                "open": tasks.len(),
                "projects": projects.iter().map(|(name, open)| {
                    serde_json::json!({"name": name, "open": open})
                }).collect::<Vec<_>>(),
            }));
        }
        let summary = serde_json::json!({"updated": now, "sources": sources});
        let path = self.resolve(SUMMARY_FILE)?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(&summary)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-tasks-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn task(id: &str, title: &str) -> Task {
        Task {
            source: "ticktick".into(),
            id: id.into(),
            title: title.into(),
            project: "Inbox".into(),
            notes: String::new(),
            status: "open".into(),
            priority: 0,
            due: None,
            start: None,
            all_day: false,
            recurrence: None,
            tags: Vec::new(),
            subtasks: Vec::new(),
            created: Some("2026-06-01T10:00:00-07:00".into()),
            modified: None,
            completed: None,
            extra: Map::new(),
        }
    }

    const NOW: &str = "2026-06-10T12:00:00-07:00";

    #[test]
    fn diff_detects_completion_via_fate() {
        let prev = vec![task("a", "Stays"), task("b", "Gets done")];
        let fresh = vec![task("a", "Stays")];
        let d = diff_snapshot(&prev, fresh, NOW, |_| {
            TaskFate::Completed(Some("2026-06-10T09:30:00-07:00".into()))
        });
        assert_eq!(d.snapshot.len(), 1);
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.events[0].kind, "completed");
        assert_eq!(d.events[0].time, "2026-06-10T09:30:00-07:00");
        assert_eq!(d.events[0].task.status, "done");
        assert_eq!(d.events[0].task.title, "Gets done");
    }

    #[test]
    fn diff_detects_deletion() {
        let prev = vec![task("a", "Gets deleted")];
        let d = diff_snapshot(&prev, Vec::new(), NOW, |_| TaskFate::Deleted);
        assert!(d.snapshot.is_empty());
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.events[0].kind, "deleted");
        assert_eq!(d.events[0].time, NOW);
    }

    #[test]
    fn unknown_fate_carries_task_forward() {
        // Network hiccup: the task must survive in the snapshot, with no
        // event, so the next sync retries instead of guessing.
        let prev = vec![task("a", "Fate unknown")];
        let d = diff_snapshot(&prev, Vec::new(), NOW, |_| TaskFate::Unknown);
        assert_eq!(d.snapshot.len(), 1);
        assert_eq!(d.snapshot[0].id, "a");
        assert!(d.events.is_empty());
    }

    #[test]
    fn diff_detects_new_task_with_its_creation_time() {
        let fresh = vec![task("new", "Brand new")];
        let d = diff_snapshot(&[task("a", "Old")], fresh, NOW, |_| TaskFate::Unknown);
        let created: Vec<_> = d.events.iter().filter(|e| e.kind == "created").collect();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].time, "2026-06-01T10:00:00-07:00");
    }

    #[test]
    fn recurring_instance_completion_keeps_task_open() {
        let mut before = task("r", "Water plants");
        before.recurrence = Some("RRULE:FREQ=DAILY".into());
        before.completed = Some("2026-06-09T08:00:00-07:00".into());
        let mut after = before.clone();
        after.completed = Some("2026-06-10T08:15:00-07:00".into());
        after.due = Some("2026-06-11T00:00:00-07:00".into());

        let d = diff_snapshot(&[before], vec![after], NOW, |_| TaskFate::Unknown);
        assert_eq!(d.snapshot.len(), 1, "recurring task stays in snapshot");
        assert_eq!(d.snapshot[0].status, "open");
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.events[0].kind, "completed");
        assert_eq!(d.events[0].time, "2026-06-10T08:15:00-07:00");
    }

    #[test]
    fn unchanged_tasks_produce_no_events() {
        let prev = vec![task("a", "Same"), task("b", "Also same")];
        let d = diff_snapshot(&prev, prev.clone(), NOW, |_| {
            panic!("fate must not be consulted for present tasks")
        });
        assert!(d.events.is_empty());
        assert_eq!(d.snapshot.len(), 2);
    }

    /// A line in the shape of the original agent dump (raw TickTick API).
    const RAW_LINE: &str = r#"{"columnId":"67d8c7f6","content":"the body","createdTime":"2025-12-09T20:21:47.541+0000","dueDate":"2026-06-09T07:00:00.000+0000","etag":"ots89plf","id":"693884d6","isAllDay":true,"kind":"TEXT","modifiedTime":"2026-04-27T03:41:03.000+0000","priority":1,"projectId":"67d8ae7b","projectName":"💻Work","sortOrder":-7146832920576,"startDate":"2026-06-09T07:00:00.000+0000","status":0,"timeZone":"America/Los_Angeles","title":"Schedule passover meme","tags":["fathom"],"items":[{"id":"x","status":1,"completedTime":1776629910000,"title":"sub one"},{"id":"y","status":0,"title":"sub two"}]}"#;

    #[test]
    fn normalizes_raw_ticktick_objects() {
        let t = normalize_ticktick(serde_json::from_str(RAW_LINE).unwrap()).unwrap();
        assert_eq!(t.source, "ticktick");
        assert_eq!(t.id, "693884d6");
        assert_eq!(t.title, "Schedule passover meme");
        assert_eq!(t.project, "💻Work");
        assert_eq!(t.notes, "the body");
        assert_eq!(t.status, "open");
        assert_eq!(t.priority, 1);
        assert!(t.all_day);
        assert_eq!(t.tags, vec!["fathom"]);
        // Times become RFC3339 local; the calendar day survives either way.
        assert!(t.due.as_deref().unwrap().starts_with("2026-06-0"));
        assert_eq!(t.subtasks.len(), 2);
        assert!(t.subtasks[0].done);
        assert!(t.subtasks[0].completed.is_some());
        assert!(!t.subtasks[1].done);
        // Unmapped fields survive in extra.
        assert_eq!(
            t.extra.get("projectId").and_then(Value::as_str),
            Some("67d8ae7b")
        );
        assert!(t.extra.contains_key("etag"));
        assert!(t.extra.contains_key("sortOrder"));
    }

    #[test]
    fn raw_dump_loads_as_baseline_without_spurious_events() {
        let v = temp_vault("baseline");
        let dir = v.root().join("tasks/ticktick");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tasks.jsonl"), format!("{RAW_LINE}\n")).unwrap();

        let baseline = v.load_tasks_snapshot("ticktick").unwrap();
        assert_eq!(baseline.len(), 1);
        assert_eq!(baseline[0].title, "Schedule passover meme");

        // Re-sync with the identical task: no events, snapshot now normalized.
        let projects = [ProjectInfo {
            id: "67d8ae7b".into(),
            name: "💻Work".into(),
        }];
        let stats = v
            .apply_tasks_sync("ticktick", &projects, baseline.clone(), |_| {
                panic!("no task disappeared")
            })
            .unwrap();
        assert_eq!(stats.open, 1);
        assert_eq!(stats.created + stats.completed + stats.deleted, 0);

        let reloaded = v.load_tasks_snapshot("ticktick").unwrap();
        assert_eq!(reloaded, baseline);
        // The rewritten file is normalized now (has a source field).
        let body = fs::read_to_string(dir.join("tasks.jsonl")).unwrap();
        assert!(body.contains("\"source\":\"ticktick\""));
    }

    #[test]
    fn sync_writes_events_markdown_and_summary() {
        let v = temp_vault("sync");
        let projects = [
            ProjectInfo {
                id: "p1".into(),
                name: "🏚Home".into(),
            },
            ProjectInfo {
                id: "p2".into(),
                name: "Empty List".into(),
            },
        ];
        let mut a = task("a", "Fix the sink");
        a.project = "🏚Home".into();
        a.due = Some("2026-06-09T00:00:00-07:00".into());
        let mut b = task("b", "Paint the fence");
        b.project = "🏚Home".into();
        b.priority = 5;
        b.tags = vec!["weekend".into()];
        b.subtasks = vec![
            Subtask {
                title: "Buy paint".into(),
                done: true,
                completed: None,
            },
            Subtask {
                title: "Sand it".into(),
                done: false,
                completed: None,
            },
        ];

        // First sync establishes the baseline (two creations — a new source).
        let stats = v
            .apply_tasks_sync("ticktick", &projects, vec![a.clone(), b.clone()], |_| {
                TaskFate::Unknown
            })
            .unwrap();
        assert_eq!(stats.created, 2);
        assert_eq!(stats.open, 2);

        // Second sync: "a" was completed, "b" remains.
        let stats = v
            .apply_tasks_sync("ticktick", &projects, vec![b.clone()], |_| {
                TaskFate::Completed(Some("2026-06-10T09:00:00-07:00".into()))
            })
            .unwrap();
        assert_eq!(stats.completed, 1);
        assert_eq!(stats.open, 1);

        // Event landed in its month's log.
        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completed: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task.title, "Fix the sink");
        assert!(v
            .root()
            .join("tasks/ticktick/events/2026-06.jsonl")
            .exists());

        // Completions trend.
        let daily = v.tasks_completed_daily("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].date, "2026-06-10");
        assert_eq!(daily[0].value, 1.0);

        // Markdown matches the dump's format; empty projects keep a file.
        let home = fs::read_to_string(v.root().join("tasks/ticktick/home.md")).unwrap();
        assert!(home.starts_with("---\nsource: ticktick\nticktick_id: p1\n"));
        assert!(home.contains("- [ ] Paint the fence · !high · #weekend"));
        assert!(home.contains("  - [x] Buy paint"));
        assert!(home.contains("  - [ ] Sand it"));
        assert!(!home.contains("Fix the sink"));
        let index = fs::read_to_string(v.root().join("tasks/ticktick/index.md")).unwrap();
        assert!(index.starts_with("# TickTick\n"));
        assert!(index.contains("| [🏚Home](home.md) | 1 |"));
        assert!(index.contains("| [Empty List](empty-list.md) | 0 |"));
        assert!(v.root().join("tasks/ticktick/empty-list.md").exists());

        // Machine indexes.
        let summary: Value = serde_json::from_str(
            &fs::read_to_string(v.root().join(".trove/tasks-summary.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(summary["sources"][0]["open"], 1);
        let sync = v.read_tasks_sync().unwrap();
        assert!(sync.sources["ticktick"].error.is_none());
        assert!(!sync.sources["ticktick"].updated.is_empty());
    }

    #[test]
    fn events_split_across_month_files_and_range_filter() {
        let v = temp_vault("months");
        let events = vec![
            TaskEvent {
                time: "2026-05-31T23:00:00-07:00".into(),
                kind: "completed".into(),
                task: task("a", "May task"),
            },
            TaskEvent {
                time: "2026-06-01T08:00:00-07:00".into(),
                kind: "completed".into(),
                task: task("b", "June task"),
            },
        ];
        v.append_task_events("ticktick", &events).unwrap();
        assert!(v.root().join("tasks/ticktick/events/2026-05.jsonl").exists());
        assert!(v.root().join("tasks/ticktick/events/2026-06.jsonl").exists());

        let all = v.task_events("2026-05-01", "2026-06-30").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].task.title, "May task", "sorted chronologically");
        let june = v.task_events("2026-06-01", "2026-06-30").unwrap();
        assert_eq!(june.len(), 1);
        assert_eq!(june[0].task.title, "June task");
    }

    #[test]
    fn overview_counts_due_and_overdue_across_sources() {
        let v = temp_vault("overview");
        let today = Local::now().format("%Y-%m-%d").to_string();
        let mut due_today = task("a", "Due today");
        due_today.due = Some(format!("{today}T00:00:00-07:00"));
        let mut overdue = task("b", "Overdue");
        overdue.due = Some("2026-01-01T00:00:00-07:00".into());
        let no_due = task("c", "Someday");
        v.write_tasks_snapshot("ticktick", &[due_today, overdue, no_due])
            .unwrap();

        // A second, minimal source: proves readers are source-agnostic and
        // sparse lines parse (only source/id/title present).
        let dir = v.root().join("tasks/other");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("tasks.jsonl"),
            "{\"source\":\"other\",\"id\":\"x1\",\"title\":\"From another app\"}\n",
        )
        .unwrap();

        let o = v.tasks_overview().unwrap();
        assert_eq!(o.open, 4);
        assert_eq!(o.due_today, 1);
        assert_eq!(o.overdue, 1);
        assert_eq!(o.projects.len(), 2);
        assert_eq!(o.projects[0].project, "Inbox");
        assert_eq!(o.projects[0].open, 3);

        let list = v.tasks_list().unwrap();
        assert_eq!(list.len(), 4);
        let sparse = list.iter().find(|t| t.source == "other").unwrap();
        assert_eq!(sparse.title, "From another app");
        assert_eq!(sparse.status, "open");
    }

    #[test]
    fn markdown_dedupes_slugs_and_removes_stale_projects() {
        let v = temp_vault("md-stale");
        let two_same = [
            ProjectInfo {
                id: "a".into(),
                name: "Same".into(),
            },
            ProjectInfo {
                id: "b".into(),
                name: "Same".into(),
            },
        ];
        v.apply_tasks_sync("ticktick", &two_same, Vec::new(), |_| TaskFate::Unknown)
            .unwrap();
        let dir = v.root().join("tasks/ticktick");
        assert!(dir.join("same.md").exists());
        assert!(dir.join("same-2.md").exists());

        // Next sync: one project renamed away — its file must not linger.
        let renamed = [ProjectInfo {
            id: "a".into(),
            name: "Renamed".into(),
        }];
        v.apply_tasks_sync("ticktick", &renamed, Vec::new(), |_| TaskFate::Unknown)
            .unwrap();
        assert!(dir.join("renamed.md").exists());
        assert!(!dir.join("same.md").exists(), "stale project file removed");
        assert!(!dir.join("same-2.md").exists());
        assert!(dir.join("index.md").exists(), "index is never stale-swept");
    }

    #[test]
    fn sync_state_records_errors_then_recovery() {
        let v = temp_vault("syncstate");
        v.record_tasks_sync("ticktick", Some("token expired".into()))
            .unwrap();
        let s = v.read_tasks_sync().unwrap();
        assert_eq!(s.sources["ticktick"].error.as_deref(), Some("token expired"));
        assert!(s.sources["ticktick"].updated.is_empty(), "never succeeded");

        v.record_tasks_sync("ticktick", None).unwrap();
        let s = v.read_tasks_sync().unwrap();
        assert!(s.sources["ticktick"].error.is_none());
        assert!(!s.sources["ticktick"].updated.is_empty());
    }
}
