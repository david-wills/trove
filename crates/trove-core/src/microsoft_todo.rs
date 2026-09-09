//! Microsoft To Do — cloud task manager via the Microsoft Graph API v1.0.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/microsoft-todo.md.
//!
//! Two destinations, written in one pass per connected Microsoft account:
//!
//! - **tasks contract** under `tasks/microsoft-todo/` (the existing Rust-bound
//!   [`crate::tasks`] contract): every active task → a [`Task`], persisted via
//!   `apply_tasks_sync` exactly like GitHub / Google Tasks. The snapshot diff
//!   reconstructs completions/deletions from the Graph `status` field plus the
//!   raw `completedDateTime`.
//! - **raw firehose** under `tasks/microsoft-todo/raw/YYYY-MM.jsonl`: verbatim
//!   Graph task objects at full fidelity (partitioned by `createdDateTime`
//!   month, upserted by `id`).
//!
//! ## API — Microsoft Graph v1.0
//!
//! All calls:
//! ```text
//! GET /me/todo/lists                                   → list pages (value[])
//! GET /me/todo/lists/{id}/tasks?$expand=checklistItems → task pages (value[])
//! ```
//! Pagination: Graph returns `@odata.nextLink` on the current page; follow
//! it verbatim until absent. The wrapper is always `{ "value": [...],
//! "@odata.nextLink": "..." }`.
//!
//! A task is open when `status != "completed"` and is considered completed
//! when the Graph `status == "completed"`. The `completedDateTime` object
//! carries `{ "dateTime": "<local>", "timeZone": "..." }`.
//!
//! ## Auth — shared `microsoft` connection
//!
//! This def sets `connection: Some("microsoft")` and rides the token from
//! [`crate::outlook`]. **NOTE:** the `microsoft` Provider currently requests
//! `Mail.Read Calendars.Read User.Read offline_access`; `Tasks.Read` must be
//! added to that scope bundle before live validation can succeed.
//! (Flagged `Needs-David(scope-add)` — no code change required here.)
//!
//! ## Cursor
//!
//! `.trove/microsoft-todo-sync.json` — a non-secret rebuildable map from
//! Microsoft account id → last-sync RFC3339 timestamp. Deleting it forces a
//! full re-sync (open tasks only; snapshot-diff reconstructs events from the
//! baseline).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::outlook::{microsoft_accounts, microsoft_fresh_token};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::tasks::{ProjectInfo, Task, TaskFate};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "microsoft-todo";
const RAW_DIR: &str = "tasks/microsoft-todo/raw";
const SYNC_FILE: &str = ".trove/microsoft-todo-sync.json";
const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Sync cadence: every 15 minutes, matching other task sources.
pub const TODO_SYNC_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, SOURCE)
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    if microsoft_accounts(vault).map(|a| a.is_empty()).unwrap_or(true) {
        return Ok(crate::registry::CollectOutcome::quiet());
    }
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "microsoft to do synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "microsoft to do sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let accounts = microsoft_accounts(vault)?;
    if accounts.is_empty() {
        anyhow::bail!("No Microsoft account is connected — sign in from the Integrations tab");
    }
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    let headline = format!(
        "Microsoft To Do synced — {} open tasks, {} completed, {} deleted",
        c("open"),
        c("completed"),
        c("deleted"),
    );
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: SOURCE,
        name: "Microsoft To Do",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your task lists from Microsoft To Do — including recurrence, \
                      reminders, and checklist items — via the Microsoft Graph API. Covers \
                      former Wunderlist users who migrated to To Do. Reuses the Microsoft \
                      login shared with Outlook.",
        domain: "tasks",
        vault_path: "tasks/microsoft-todo/",
        toggleable: true,
        setup: &[
            "Connect your Microsoft account on this card (or in Outlook — it's the same login).",
            "Each sync snapshots your open tasks; completions accrue while the sync runs.",
        ],
        caveats: "Requires Tasks.Read on the Microsoft OAuth scope — add it to your Azure app \
                  registration alongside Mail.Read and Calendars.Read, then reconnect.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(TODO_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("microsoft"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor — one entry per Microsoft account id.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync, keyed by account id.
    #[serde(default)]
    accounts: BTreeMap<String, String>,
}

impl Vault {
    fn read_ms_todo_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ms_todo_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

/// One page of Graph results plus an optional `@odata.nextLink` to follow.
struct Page {
    items: Vec<Value>,
    next_link: Option<String>,
}

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The endpoints the pull needs. A trait so tests drive logic with fixtures.
trait TodoApi {
    /// `GET <url>` — full URL (base + path). Returns one page.
    fn get_page(&self, url: &str) -> Result<Page, FetchError>;
}

/// Thin Graph client; base URL injected (testable seam).
struct GraphClient {
    base: String,
    token: String,
}

impl TodoApi for GraphClient {
    fn get_page(&self, url: &str) -> Result<Page, FetchError> {
        let full = if url.starts_with("https://") {
            url.to_string() // nextLink is already absolute
        } else {
            format!("{}{url}", self.base)
        };
        let resp = ureq::get(&full)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .call();
        match resp {
            Ok(r) => {
                let v: Value = r
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parse: {e}")))?;
                Ok(parse_page(v))
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

/// Parse a Graph list response: `{ "value": [...], "@odata.nextLink": "..." }`.
fn parse_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let items = o
                .get("value")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next_link = o
                .get("@odata.nextLink")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Page { items, next_link }
        }
        Value::Array(a) => Page { items: a, next_link: None },
        _ => Page { items: Vec::new(), next_link: None },
    }
}

/// Drain every page of a Graph endpoint (following `@odata.nextLink`).
fn drain_all(api: &impl TodoApi, path: &str, base: &str) -> Result<Vec<Value>, FetchError> {
    let first_url = if path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{base}{path}")
    };
    let mut items = Vec::new();
    let mut next: Option<String> = Some(first_url);
    while let Some(url) = next {
        let page = api.get_page(&url)?;
        items.extend(page.items);
        next = page.next_link;
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Raw row shape.

/// One verbatim Graph task object in `tasks/microsoft-todo/raw/YYYY-MM.jsonl`.
/// The on-disk line is the unmodified API object; guid and created_dt are read
/// off it via accessors, not stored as extra columns.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawTask {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawTask {
    fn guid(&self) -> String {
        self.fields
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// `createdDateTime` (always UTC ISO8601 from Graph) — partition key.
    fn created_dt(&self) -> &str {
        self.fields
            .get("createdDateTime")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

fn raw_task(value: &Value) -> Option<RawTask> {
    let obj = value.as_object()?;
    obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    obj.get("createdDateTime").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    Some(RawTask { fields: obj.clone() })
}

// ---------------------------------------------------------------------------
// Pure mapping.

/// Pull a string field from a JSON object, trimmed; None when missing/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Graph `importance` string → task contract priority (TickTick scale).
/// Graph values: "low" | "normal" | "high".
fn map_importance(s: &str) -> i64 {
    match s {
        "low" => 1,
        "high" => 5,
        _ => 3, // "normal"
    }
}

/// Graph `dateTimeTimeZone` object → RFC3339 local. The object has:
/// `{ "dateTime": "2026-06-15T17:00:00.0000000", "timeZone": "UTC" }`.
/// Graph returns a naive datetime string (no `Z` or offset) for v1.0 when no
/// `Prefer: outlook.timezone` header is sent — we treat it as UTC and convert
/// to local. The string may also carry sub-second digits we strip before
/// appending the `Z` needed for RFC3339 parsing.
fn dttz_to_local(obj: &Value) -> Option<String> {
    let dt_str = obj.get("dateTime").and_then(Value::as_str)?;
    if dt_str.is_empty() {
        return None;
    }
    // Check whether the string already carries a UTC marker or numeric offset.
    // A trailing 'Z' or a '+'/'-' AFTER the time part (past position 10)
    // means it's already timezone-qualified.
    // Guard: if the string is shorter than 10 bytes it is malformed — skip
    // rather than panic on the slice.
    if dt_str.len() < 10 {
        return None;
    }
    let has_tz = dt_str.ends_with('Z')
        || dt_str[10..].contains('+')
        || dt_str[10..].contains('-');
    let normalized = if has_tz {
        // Already has a timezone; strip sub-second digits if present so
        // RFC3339 parsing succeeds: "2026-06-15T17:00:00.1234567Z" →
        // "2026-06-15T17:00:00Z".
        if let Some(dot_pos) = dt_str[..dt_str.len().min(26)].find('.') {
            // Find where the timezone marker starts (Z or +/-).
            let tz_start = dt_str[dot_pos..]
                .find(|c: char| c == 'Z' || c == '+' || c == '-')
                .map(|p| dot_pos + p)
                .unwrap_or(dt_str.len());
            format!("{}{}", &dt_str[..dot_pos], &dt_str[tz_start..])
        } else {
            dt_str.to_string()
        }
    } else {
        // Naive datetime from Graph — treat as UTC. Truncate to 19 chars
        // (YYYY-MM-DDTHH:MM:SS) then append Z.
        let truncated = &dt_str[..dt_str.len().min(19)];
        format!("{truncated}Z")
    };
    DateTime::parse_from_rfc3339(&normalized)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .ok()
}

/// Extract the calendar date from a Graph `dateTimeTimeZone` object and return
/// it as a stable local-midnight RFC3339 string (e.g. "2026-06-15T00:00:00-07:00").
///
/// Microsoft To Do due/start dates are inherently date-only — the UI has no
/// time component; Graph returns them as midnight expressed in the user's tz
/// (e.g. `{"dateTime":"2026-06-15T00:00:00.0000000","timeZone":"UTC"}`).
/// We read the YYYY-MM-DD portion directly from the dateTime string (the first
/// 10 bytes are always the date, regardless of timezone) so the calendar day is
/// preserved even when the user's local tz differs from the stored tz.
fn dttz_to_all_day_date(obj: &Value) -> Option<String> {
    let dt_str = obj.get("dateTime").and_then(Value::as_str)?;
    if dt_str.len() < 10 {
        return None;
    }
    // Parse the date portion directly — avoids any timezone day-shift.
    let date = NaiveDate::parse_from_str(&dt_str[..10], "%Y-%m-%d").ok()?;
    // Express as local midnight so callers get a valid RFC3339 timestamp.
    let midnight = Local.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).single()?;
    Some(midnight.to_rfc3339())
}

/// Parse a bare RFC3339 / ISO8601 UTC datetime string (like `createdDateTime`,
/// `checkedDateTime`) → RFC3339 local. Handles nanosecond fractions.
fn dt_to_local(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    // Strip sub-second digits before the Z/offset so chrono can parse it.
    let normalized = if let Some(dot_pos) = s.find('.') {
        let tz_start = s[dot_pos..]
            .find(|c: char| c == 'Z' || c == '+' || (c == '-' && dot_pos + 1 < s.len()))
            .map(|p| dot_pos + p)
            .unwrap_or(s.len());
        let rest = if tz_start < s.len() { &s[tz_start..] } else { "Z" };
        format!("{}{rest}", &s[..dot_pos])
    } else if s.ends_with('Z') || s.contains('+') {
        s.to_string()
    } else {
        // Naive — treat as UTC.
        format!("{}Z", &s[..s.len().min(19)])
    };
    DateTime::parse_from_rfc3339(&normalized)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .ok()
}

/// Extract subtasks from `checklistItems` (available via `$expand`).
fn extract_subtasks(task_obj: &Value) -> Vec<crate::tasks::Subtask> {
    task_obj
        .get("checklistItems")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let name = str_opt(item, "displayName")?;
                    let done = item
                        .get("isChecked")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    // checkedDateTime is a bare DateTimeOffset string, not a
                    // dateTimeTimeZone object.
                    let completed = item
                        .get("checkedDateTime")
                        .and_then(Value::as_str)
                        .and_then(dt_to_local);
                    Some(crate::tasks::Subtask { title: name, done, completed })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map a Graph todoTask JSON object → normalized [`Task`].
/// `account_id` + `account_email` are used to namespace the task id for
/// multi-account safety (same Graph task id could appear across tenants).
/// `list_name` is the containing list's `displayName`.
fn task_from_value(
    value: &Value,
    list_name: &str,
    account_id: &str,
    account_email: &str,
) -> Option<Task> {
    let obj = value.as_object()?;
    let raw_id = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    // Namespace: "<account_id>:<task_id>" for multi-account dedup.
    let id = format!("{account_id}:{raw_id}");

    let title = str_opt(value, "title")?;

    // body.content → notes
    let notes = obj
        .get("body")
        .and_then(|b| str_opt(b, "content"))
        .unwrap_or_default();

    let status_str = obj
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("notStarted");
    // Open if not "completed"; Graph statuses: notStarted/inProgress/
    // completed/waitingOnOthers/deferred — all non-completed go in the snapshot.
    let status = if status_str == "completed" { "done" } else { "open" }.to_string();

    let importance = obj
        .get("importance")
        .and_then(Value::as_str)
        .unwrap_or("normal");
    let priority = map_importance(importance);

    // dueDateTime: { "dateTime": "...", "timeZone": "UTC" }
    // Microsoft To Do due/start dates are inherently date-only (the UI has no
    // time component). Graph returns them as midnight in the user's tz. We
    // always set all_day=true and read only the YYYY-MM-DD portion to avoid
    // day-shifting when the machine tz differs from the stored tz.
    let due = obj.get("dueDateTime").and_then(|d| dttz_to_all_day_date(d));
    let all_day = obj.get("dueDateTime").is_some() || obj.get("startDateTime").is_some();

    // startDateTime
    let start = obj.get("startDateTime").and_then(|d| dttz_to_all_day_date(d));

    // completedDateTime
    let completed = obj.get("completedDateTime").and_then(|d| dttz_to_local(d));

    // recurrence: the contract field holds an RRULE string. Graph returns a
    // patternedRecurrence object with no RRULE equivalent. We cannot produce a
    // valid RRULE here, so we follow the google_tasks precedent: set None and
    // rely solely on extra.recurrence_pattern for raw fidelity.
    let recurrence: Option<String> = None;

    // categories → tags
    let tags: Vec<String> = obj
        .get("categories")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
        .unwrap_or_default();

    // checklistItems (via $expand)
    let subtasks = extract_subtasks(value);

    let created = obj
        .get("createdDateTime")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Local).to_rfc3339());

    let modified = obj
        .get("lastModifiedDateTime")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&Local).to_rfc3339());

    // Extra: source-specific fields the contract doesn't carry.
    let mut extra = Map::new();
    extra.insert("microsoft_task_id".into(), Value::from(raw_id));
    extra.insert("account".into(), Value::from(account_email));
    extra.insert("account_id".into(), Value::from(account_id));
    extra.insert("importance".into(), Value::from(importance));
    extra.insert("status_raw".into(), Value::from(status_str));
    if let Some(is_reminder) = obj.get("isReminderOn").and_then(Value::as_bool) {
        extra.insert("is_reminder_on".into(), Value::from(is_reminder));
    }
    if let Some(recur_obj) = obj.get("recurrence") {
        if !recur_obj.is_null() {
            extra.insert("recurrence_pattern".into(), recur_obj.clone());
        }
    }
    if let Some(reminder_dt) = obj.get("reminderDateTime").and_then(|d| dttz_to_local(d)) {
        extra.insert("reminder_at".into(), Value::from(reminder_dt));
    }
    // linkedResources (if present via $expand or inline)
    if let Some(links) = obj.get("linkedResources").and_then(Value::as_array) {
        if !links.is_empty() {
            extra.insert("linked_resources".into(), Value::Array(links.to_vec()));
        }
    }

    Some(Task {
        source: SOURCE.into(),
        id,
        title,
        project: list_name.to_string(),
        notes,
        status,
        priority,
        due,
        start,
        all_day,
        recurrence,
        tags,
        subtasks,
        created,
        modified,
        completed,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition (partitioned by createdDateTime month).

fn upsert_raw(vault: &Vault, rows: Vec<RawTask>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawTask>> = BTreeMap::new();
    for r in rows {
        let created = r.created_dt().to_string();
        if created.is_empty() {
            continue;
        }
        let key = match Partition::Month.key(&created) {
            Some(k) => k.to_string(),
            None => continue,
        };
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<RawTask> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (r.guid(), i))
            .collect();
        for r in fresh {
            match idx.get(&r.guid()).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.guid(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| {
            a.created_dt().cmp(b.created_dt()).then_with(|| a.guid().cmp(&b.guid()))
        });
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// Per-account pull.

/// Result of pulling one account.
struct AccountPull {
    projects: Vec<ProjectInfo>,
    open: Vec<Task>,
    raw_rows: Vec<RawTask>,
    /// Tasks that Graph returned with status="completed" (for the fate closure).
    completed_ids: HashMap<String, Option<String>>, // id → completed_at
}

fn pull_account(
    api: &impl TodoApi,
    account_id: &str,
    account_email: &str,
    base: &str,
) -> Result<AccountPull, FetchError> {
    // 1. Fetch all task lists.
    let list_items = drain_all(api, "/me/todo/lists", base)?;

    let mut projects = Vec::new();
    let mut open = Vec::new();
    let mut raw_rows = Vec::new();
    let mut completed_ids = HashMap::new();

    for list_item in &list_items {
        let list_id = match list_item.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };
        let list_name = list_item
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or("Tasks")
            .to_string();

        projects.push(ProjectInfo { id: list_id.clone(), name: list_name.clone() });

        // 2. Fetch all tasks in this list (expand checklistItems inline).
        let path = format!(
            "/me/todo/lists/{list_id}/tasks?$expand=checklistItems"
        );
        let task_items = drain_all(api, &path, base)?;

        for task_val in &task_items {
            // Collect raw row.
            if let Some(r) = raw_task(task_val) {
                raw_rows.push(r);
            }
            // Map to contract task.
            if let Some(t) = task_from_value(task_val, &list_name, account_id, account_email) {
                // Track completed tasks for the fate closure.
                let status_raw = task_val
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if status_raw == "completed" {
                    let completed_at = task_val
                        .get("completedDateTime")
                        .and_then(|d| dttz_to_local(d));
                    completed_ids.insert(t.id.clone(), completed_at);
                    // Completed tasks do NOT go into the open snapshot.
                } else {
                    open.push(t);
                }
            }
        }
    }

    Ok(AccountPull { projects, open, raw_rows, completed_ids })
}

// ---------------------------------------------------------------------------
// The main pull (multi-account).

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let accounts = microsoft_accounts(vault)?;
    if accounts.is_empty() {
        anyhow::bail!("no Microsoft account connected — sign in from the Integrations tab");
    }

    let mut state = vault.read_ms_todo_sync();
    let mut all_projects: Vec<ProjectInfo> = Vec::new();
    let mut all_open: Vec<Task> = Vec::new();
    let mut all_raw: Vec<RawTask> = Vec::new();
    let mut all_completed: HashMap<String, Option<String>> = HashMap::new();
    // Track which account ids were fully drained this pass.  Only tasks whose
    // account id is in this set may be treated as Deleted when absent from the
    // fresh open set; tasks from accounts that failed are Unknown (carry-forward).
    let mut succeeded_accounts: HashSet<String> = HashSet::new();
    let mut any_ok = false;
    let mut first_err: Option<String> = None;

    for acct in &accounts {
        if acct.needs_reconnect {
            // Account needs re-auth — skip without guessing fates.
            continue;
        }
        let token = match microsoft_fresh_token(vault, &acct.id) {
            Ok(t) => t,
            Err(e) => {
                first_err.get_or_insert_with(|| format!("{e:#}"));
                continue;
            }
        };
        let client = GraphClient { base: GRAPH_BASE.to_string(), token };
        match pull_account(&client, &acct.id, &acct.email, GRAPH_BASE) {
            Ok(ap) => {
                any_ok = true;
                succeeded_accounts.insert(acct.id.clone());
                all_projects.extend(ap.projects);
                all_open.extend(ap.open);
                all_raw.extend(ap.raw_rows);
                all_completed.extend(ap.completed_ids);
                state.accounts.insert(acct.id.clone(), Local::now().to_rfc3339());
            }
            Err(FetchError::Unauthorized) => {
                let msg = format!(
                    "Microsoft account {} rejected the token (401) — reconnect from the \
                     Integrations tab",
                    acct.email
                );
                first_err.get_or_insert(msg);
            }
            Err(e) => {
                first_err.get_or_insert_with(|| format!("{e}"));
            }
        }
    }

    if !any_ok {
        return Err(anyhow!(
            first_err.unwrap_or_else(|| "Microsoft To Do: all accounts failed".to_string())
        ));
    }

    // Raw upsert.
    let raw_new = upsert_raw(vault, all_raw)?;

    // Snapshot diff via the bound tasks contract.
    // Only treat a vanished task as Deleted when its account was fully drained
    // this pass.  If the account failed (not in succeeded_accounts), the task
    // is Unknown (carry-forward) — no false deletion flood, no false re-creation
    // on recovery.
    let completed_snap = all_completed.clone();
    let stats = vault
        .apply_tasks_sync(SOURCE, &all_projects, all_open, |t| {
            ms_todo_fate(t, &completed_snap, &succeeded_accounts)
        })
        .context("microsoft-todo: applying task sync")?;

    vault.write_ms_todo_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);

    Ok(PullOutcome {
        headline: format!("{} open Microsoft To Do tasks", stats.open),
        counts,
    })
}

/// Resolve the fate of a task that vanished from the open set.
///
/// The `/tasks` endpoint (with no `$filter`) returns tasks of ALL statuses
/// inline — both open and completed.  Completed tasks are collected into
/// `completed_ids` from the same drained pages (status == "completed").  So:
///
/// - In `completed_ids` → Completed(its `completedDateTime` or None).
/// - Not in `completed_ids`, account succeeded → Deleted (we drained all
///   pages for this account, so absence from both open and completed_ids
///   means deletion).
/// - Not in `completed_ids`, account DID NOT succeed → Unknown (carry
///   forward — transient failure for this account; do not emit a false
///   Deleted event that can't be un-rung in the event stream).
///
/// Task ids are namespaced as `<account_id>:<raw_task_id>`; we split on ':'
/// to extract the account_id prefix for the succeeded-accounts guard.
fn ms_todo_fate(
    task: &Task,
    completed: &HashMap<String, Option<String>>,
    succeeded: &HashSet<String>,
) -> TaskFate {
    match completed.get(&task.id) {
        Some(Some(when)) => TaskFate::Completed(Some(when.clone())),
        Some(None) => TaskFate::Completed(None),
        None => {
            // Extract the account_id prefix ("acctXXX" from "acctXXX:taskYYY").
            let account_ok = task
                .id
                .splitn(2, ':')
                .next()
                .map(|prefix| succeeded.contains(prefix))
                .unwrap_or(false);
            if account_ok {
                TaskFate::Deleted
            } else {
                // This account failed — don't falsely delete its tasks.
                TaskFate::Unknown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-ms-todo-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures — exact shapes from the Graph v1.0 docs.

    /// A todoTaskList object.
    fn list_json(id: &str, name: &str) -> Value {
        serde_json::json!({
            "@odata.type": "#microsoft.graph.todoTaskList",
            "id": id,
            "displayName": name,
            "isOwner": true,
            "isShared": false,
            "wellknownListName": "none"
        })
    }

    /// A todoTask with importance=high, due date/time, recurrence, categories.
    /// Matches the exact Graph v1.0 response shape (confirmed via docs).
    fn task_high_priority(id: &str) -> Value {
        serde_json::json!({
            "@odata.type": "#microsoft.graph.todoTask",
            "id": id,
            "title": "Ship the release",
            "body": {
                "content": "Cut the tag, push to production",
                "contentType": "text"
            },
            "importance": "high",
            "status": "notStarted",
            "isReminderOn": true,
            "categories": ["Work", "Urgent"],
            "createdDateTime": "2026-06-01T08:00:00Z",
            "lastModifiedDateTime": "2026-06-10T09:00:00Z",
            "dueDateTime": {
                "dateTime": "2026-06-15T17:00:00.0000000",
                "timeZone": "UTC"
            },
            "reminderDateTime": {
                "dateTime": "2026-06-15T09:00:00.0000000",
                "timeZone": "UTC"
            },
            "recurrence": {
                "pattern": {
                    "type": "weekly",
                    "interval": 1,
                    "daysOfWeek": ["monday"]
                },
                "range": {
                    "type": "noEnd"
                }
            },
            "checklistItems": [
                {
                    "id": "ci1",
                    "displayName": "Write release notes",
                    "isChecked": true,
                    "checkedDateTime": "2026-06-14T16:00:00.0000000Z",
                    "createdDateTime": "2026-06-01T08:00:00Z"
                },
                {
                    "id": "ci2",
                    "displayName": "Tag the repo",
                    "isChecked": false,
                    "createdDateTime": "2026-06-01T08:00:00Z"
                }
            ],
            "linkedResources": [
                {
                    "applicationName": "Partner App",
                    "displayName": "Related email",
                    "externalId": "ext123",
                    "id": "lr1"
                }
            ]
        })
    }

    /// A normal-importance, no-due task.
    fn task_normal(id: &str) -> Value {
        serde_json::json!({
            "@odata.type": "#microsoft.graph.todoTask",
            "id": id,
            "title": "Buy groceries",
            "body": { "content": "", "contentType": "text" },
            "importance": "normal",
            "status": "notStarted",
            "isReminderOn": false,
            "categories": [],
            "createdDateTime": "2026-06-02T10:00:00Z",
            "lastModifiedDateTime": "2026-06-02T10:00:00Z",
            "checklistItems": []
        })
    }

    /// A completed task (Graph returns these when the tasks endpoint is called
    /// without a status filter — the docs say it returns all tasks in the list).
    fn task_completed(id: &str, completed_at: &str) -> Value {
        serde_json::json!({
            "@odata.type": "#microsoft.graph.todoTask",
            "id": id,
            "title": "Done thing",
            "body": { "content": "", "contentType": "text" },
            "importance": "normal",
            "status": "completed",
            "isReminderOn": false,
            "categories": [],
            "createdDateTime": "2026-06-01T08:00:00Z",
            "lastModifiedDateTime": "2026-06-13T16:30:00Z",
            "completedDateTime": {
                "dateTime": completed_at,
                "timeZone": "UTC"
            },
            "checklistItems": []
        })
    }

    // ---------------------------------------------------------------------------
    // Mock API.

    /// Maps a URL substring to a queue of pages.
    struct MockApi {
        pages: RefCell<Vec<(String, VecDeque<Page>)>>,
        requests: RefCell<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn add(&self, url_substr: &str, items: Vec<Value>, next_link: Option<String>) {
            self.pages.borrow_mut().push((
                url_substr.into(),
                VecDeque::from(vec![Page { items, next_link }]),
            ));
        }

        fn add_pages(&self, url_substr: &str, pages: Vec<Page>) {
            self.pages.borrow_mut().push((url_substr.into(), VecDeque::from(pages)));
        }

        fn requested(&self, needle: &str) -> bool {
            self.requests.borrow().iter().any(|u| u.contains(needle))
        }
    }

    impl TodoApi for MockApi {
        fn get_page(&self, url: &str) -> Result<Page, FetchError> {
            self.requests.borrow_mut().push(url.to_string());
            let mut pages = self.pages.borrow_mut();
            for (substr, queue) in pages.iter_mut() {
                if url.contains(substr.as_str()) {
                    if let Some(p) = queue.pop_front() {
                        return Ok(p);
                    }
                }
            }
            Ok(Page { items: Vec::new(), next_link: None })
        }
    }

    /// Build a mock that serves one list with the given tasks.
    fn one_list_mock(list_id: &str, list_name: &str, tasks: Vec<Value>) -> MockApi {
        let api = MockApi::new();
        api.add("/me/todo/lists", vec![list_json(list_id, list_name)], None);
        api.add(&format!("/me/todo/lists/{list_id}/tasks"), tasks, None);
        api
    }

    // ---------------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_high_priority_task_with_all_fields() {
        let t =
            task_from_value(&task_high_priority("TID1"), "Work", "acct1", "user@example.com")
                .unwrap();
        assert_eq!(t.source, SOURCE);
        assert_eq!(t.id, "acct1:TID1", "namespaced id");
        assert_eq!(t.title, "Ship the release");
        assert_eq!(t.project, "Work");
        assert_eq!(t.notes, "Cut the tag, push to production");
        assert_eq!(t.status, "open");
        assert_eq!(t.priority, 5, "importance=high → 5");
        assert_eq!(t.tags, vec!["Work", "Urgent"]);
        assert!(t.due.is_some(), "due datetime mapped");
        assert!(t.all_day, "To Do due dates are always all-day");
        // The contract recurrence field is for RRULE strings; Graph has no RRULE.
        // We leave it None and store the raw patternedRecurrence in extra.
        assert!(
            t.recurrence.is_none(),
            "recurrence is None — Graph pattern stored in extra.recurrence_pattern"
        );
        assert!(
            t.extra.get("recurrence_pattern").is_some(),
            "raw patternedRecurrence preserved in extra"
        );
        assert_eq!(t.subtasks.len(), 2);
        assert!(t.subtasks[0].done, "first checklist item is checked");
        assert!(!t.subtasks[1].done, "second checklist item not checked");
        assert_eq!(t.subtasks[0].title, "Write release notes");
        assert_eq!(t.subtasks[1].title, "Tag the repo");
        // extra fields
        assert_eq!(
            t.extra.get("microsoft_task_id").and_then(Value::as_str),
            Some("TID1")
        );
        assert_eq!(
            t.extra.get("account").and_then(Value::as_str),
            Some("user@example.com")
        );
        assert_eq!(
            t.extra.get("importance").and_then(Value::as_str),
            Some("high")
        );
        assert!(
            t.extra.get("linked_resources").is_some(),
            "linked resources in extra"
        );
    }

    #[test]
    fn maps_normal_priority_no_due() {
        let t =
            task_from_value(&task_normal("TID2"), "Personal", "acct1", "user@example.com")
                .unwrap();
        assert_eq!(t.title, "Buy groceries");
        assert_eq!(t.priority, 3, "importance=normal → 3");
        assert!(t.due.is_none());
        assert!(t.subtasks.is_empty());
        assert!(t.recurrence.is_none());
    }

    #[test]
    fn all_day_is_always_true_for_to_do_due_dates() {
        // Microsoft To Do due/start dates are inherently date-only.  all_day
        // must be true whenever dueDateTime is present, regardless of whether
        // the datetime parses successfully.
        let t =
            task_from_value(&task_high_priority("TID3"), "Work", "a1", "u@e.com").unwrap();
        assert!(t.all_day, "task with dueDateTime → all_day=true");
        // The date in the due string should be the same calendar date as the
        // fixture (2026-06-15) — no day-shifting.
        assert!(
            t.due.as_deref().unwrap_or("").starts_with("2026-06-15"),
            "calendar date preserved without tz day-shift: {:?}", t.due
        );

        // A task with no dueDateTime or startDateTime → all_day=false.
        let t2 =
            task_from_value(&task_normal("TID4"), "Personal", "a1", "u@e.com").unwrap();
        assert!(!t2.all_day, "task without due/start dates → all_day=false");
    }

    #[test]
    fn dttz_to_local_does_not_panic_on_short_string() {
        // Strings shorter than 10 bytes must return None, not panic.
        let short = serde_json::json!({ "dateTime": "2026-06", "timeZone": "UTC" });
        assert!(dttz_to_local(&short).is_none(), "short dateTime → None");
        let empty = serde_json::json!({ "dateTime": "", "timeZone": "UTC" });
        assert!(dttz_to_local(&empty).is_none(), "empty dateTime → None");
        // A well-formed full string still works.
        let ok = serde_json::json!({
            "dateTime": "2026-06-15T17:00:00.0000000",
            "timeZone": "UTC"
        });
        assert!(dttz_to_local(&ok).is_some(), "valid dateTime still parses");
    }

    #[test]
    fn importance_mapping() {
        assert_eq!(map_importance("low"), 1);
        assert_eq!(map_importance("normal"), 3);
        assert_eq!(map_importance("high"), 5);
        assert_eq!(map_importance("unknown"), 3, "unknown → normal");
    }

    #[test]
    fn completed_task_is_excluded_from_open_and_tracked() {
        // When Graph returns a completed task, pull_account should NOT add it
        // to `open` but should track its id in `completed_ids`.
        let api = one_list_mock(
            "L1",
            "Work",
            vec![task_high_priority("T_OPEN"), task_completed("T_DONE", "2026-06-13T16:30:00")],
        );
        let ap = pull_account(&api, "acct1", "user@example.com", "").unwrap();
        assert_eq!(ap.open.len(), 1, "only open task in the open list");
        assert_eq!(ap.open[0].id, "acct1:T_OPEN");
        assert!(
            ap.completed_ids.contains_key("acct1:T_DONE"),
            "completed task tracked by id"
        );
    }

    #[test]
    fn parse_page_reads_value_array_and_next_link() {
        let p = parse_page(serde_json::json!({
            "value": [1, 2, 3],
            "@odata.nextLink": "https://graph.microsoft.com/v1.0/me/todo/lists?$skiptoken=ABC"
        }));
        assert_eq!(p.items.len(), 3);
        assert!(p.next_link.as_deref().unwrap().contains("skiptoken=ABC"));

        let last = parse_page(serde_json::json!({"value": [1]}));
        assert_eq!(last.items.len(), 1);
        assert!(last.next_link.is_none());
    }

    #[test]
    fn drain_all_follows_next_link_chain() {
        let api = MockApi::new();
        api.add_pages(
            "/me/todo/lists",
            vec![
                Page {
                    items: vec![list_json("L1", "Work")],
                    next_link: Some("https://graph.microsoft.com/v1.0/me/todo/lists/page2".into()),
                },
                Page { items: vec![list_json("L2", "Personal")], next_link: None },
            ],
        );
        // The second page uses the full absolute nextLink URL.
        api.add("lists/page2", vec![list_json("L2", "Personal")], None);

        // Drain starting from the first page URL.
        let items = drain_all(&api, "/me/todo/lists", "").unwrap();
        assert_eq!(items.len(), 2, "both pages drained");
    }

    #[test]
    fn fate_in_completed_set_yields_completed() {
        let mut completed = HashMap::new();
        completed.insert(
            "acct1:T1".to_string(),
            Some("2026-06-13T16:30:00+00:00".to_string()),
        );
        completed.insert("acct1:T2".to_string(), None);

        let fake_task = |id: &str| Task {
            source: SOURCE.into(),
            id: id.into(),
            title: "x".into(),
            project: String::new(),
            notes: String::new(),
            status: "open".into(),
            priority: 0,
            due: None,
            start: None,
            all_day: false,
            recurrence: None,
            tags: Vec::new(),
            subtasks: Vec::new(),
            created: None,
            modified: None,
            completed: None,
            extra: Map::new(),
        };

        let mut succeeded: HashSet<String> = HashSet::new();
        succeeded.insert("acct1".to_string());

        match ms_todo_fate(&fake_task("acct1:T1"), &completed, &succeeded) {
            TaskFate::Completed(Some(w)) => assert!(w.contains("2026-06-13")),
            _ => panic!("expected Completed(Some(...))"),
        }
        assert!(matches!(
            ms_todo_fate(&fake_task("acct1:T2"), &completed, &succeeded),
            TaskFate::Completed(None)
        ));
        // Not in the set, account succeeded → Deleted.
        assert!(matches!(
            ms_todo_fate(&fake_task("acct1:T_GHOST"), &completed, &succeeded),
            TaskFate::Deleted
        ));
        // Not in the set, account did NOT succeed → Unknown (carry-forward, no flood).
        assert!(matches!(
            ms_todo_fate(&fake_task("acct2:T_GHOST"), &completed, &succeeded),
            TaskFate::Unknown
        ));
    }

    #[test]
    fn partial_account_failure_does_not_delete_other_account_tasks() {
        // This tests the blocking bug: when >=2 accounts are connected and one
        // fails, the surviving account's success (any_ok=true) must NOT cause
        // tasks from the failed account to be marked Deleted.
        let completed: HashMap<String, Option<String>> = HashMap::new();
        let mut succeeded: HashSet<String> = HashSet::new();
        succeeded.insert("acct_ok".to_string());
        // acct_fail is NOT in succeeded_accounts.

        let fake_task = |id: &str| Task {
            source: SOURCE.into(),
            id: id.into(),
            title: "x".into(),
            project: String::new(),
            notes: String::new(),
            status: "open".into(),
            priority: 0,
            due: None,
            start: None,
            all_day: false,
            recurrence: None,
            tags: Vec::new(),
            subtasks: Vec::new(),
            created: None,
            modified: None,
            completed: None,
            extra: Map::new(),
        };

        // Task from the account that succeeded → Deleted (absent means truly gone).
        assert!(
            matches!(
                ms_todo_fate(&fake_task("acct_ok:TASK1"), &completed, &succeeded),
                TaskFate::Deleted
            ),
            "task from succeeded account absent from fresh → Deleted"
        );

        // Task from the account that FAILED → Unknown (carry-forward, no flood).
        assert!(
            matches!(
                ms_todo_fate(&fake_task("acct_fail:TASK2"), &completed, &succeeded),
                TaskFate::Unknown
            ),
            "task from failed account must be Unknown, not Deleted"
        );
    }

    #[test]
    fn full_pull_account_writes_raw_and_open_tasks() {
        let vault = temp_vault("full-pull");
        let api = one_list_mock(
            "L1",
            "Work",
            vec![task_high_priority("T1"), task_normal("T2")],
        );
        let ap = pull_account(&api, "acct1", "user@example.com", "").unwrap();
        assert_eq!(ap.open.len(), 2);
        assert_eq!(ap.raw_rows.len(), 2);
        assert_eq!(ap.projects.len(), 1);
        assert_eq!(ap.projects[0].name, "Work");

        // Upsert raw and verify the file.
        let new_count = upsert_raw(&vault, ap.raw_rows).unwrap();
        assert_eq!(new_count, 2);
        let raw_path = vault.root().join("tasks/microsoft-todo/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw partition created for 2026-06");
        let raw_body = std::fs::read_to_string(&raw_path).unwrap();
        assert!(raw_body.contains("Ship the release"), "raw keeps title");
        // No synthetic guid column (the flatten keeps the real id field).
        assert!(!raw_body.contains("\"guid\""), "no synthetic guid column");
    }

    #[test]
    fn resync_dedupes_raw_by_id() {
        let vault = temp_vault("raw-dedup");
        let api1 = one_list_mock("L1", "Work", vec![task_high_priority("T1")]);
        let ap1 = pull_account(&api1, "acct1", "u@e.com", "").unwrap();
        upsert_raw(&vault, ap1.raw_rows).unwrap();

        // Re-sync same task — no new raw row.
        let api2 = one_list_mock("L1", "Work", vec![task_high_priority("T1")]);
        let ap2 = pull_account(&api2, "acct1", "u@e.com", "").unwrap();
        let new_count = upsert_raw(&vault, ap2.raw_rows).unwrap();
        assert_eq!(new_count, 0, "duplicate id → no new raw row");
        let raw_body =
            std::fs::read_to_string(vault.root().join("tasks/microsoft-todo/raw/2026-06.jsonl"))
                .unwrap();
        assert_eq!(raw_body.lines().count(), 1, "exactly one line after dedup");
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.accounts.is_empty());

        let partial: SyncState = serde_json::from_str(
            r#"{"accounts": {"acct1": "2026-06-01T00:00:00-07:00"}}"#,
        )
        .unwrap();
        assert_eq!(
            partial.accounts.get("acct1").map(String::as_str),
            Some("2026-06-01T00:00:00-07:00")
        );
    }

    #[test]
    fn checklist_items_map_to_subtasks_correctly() {
        let t =
            task_from_value(&task_high_priority("T1"), "Work", "a1", "u@e.com").unwrap();
        assert_eq!(t.subtasks.len(), 2);
        assert_eq!(t.subtasks[0].title, "Write release notes");
        assert!(t.subtasks[0].done);
        assert!(t.subtasks[0].completed.is_some(), "checkedDateTime → subtask.completed");
        assert_eq!(t.subtasks[1].title, "Tag the repo");
        assert!(!t.subtasks[1].done);
        assert!(t.subtasks[1].completed.is_none());
    }

    #[test]
    fn raw_task_roundtrips_full_fidelity() {
        let val = task_high_priority("T1");
        let r = raw_task(&val).unwrap();
        assert_eq!(r.guid(), "T1");
        assert_eq!(r.created_dt(), "2026-06-01T08:00:00Z");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("\"guid\""), "no synthetic guid column");
        let back: RawTask = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r, "round-trips identically");
        assert!(line.contains("Ship the release"), "full fidelity kept");
    }

    #[test]
    fn multi_account_ids_are_namespaced() {
        let t1 = task_from_value(&task_normal("SAME_ID"), "List", "acct1", "a@e.com").unwrap();
        let t2 = task_from_value(&task_normal("SAME_ID"), "List", "acct2", "b@e.com").unwrap();
        assert_ne!(t1.id, t2.id, "same Graph id, different account → different task ids");
        assert!(t1.id.starts_with("acct1:"));
        assert!(t2.id.starts_with("acct2:"));
    }

    #[test]
    fn def_exposes_periodic_behavior_and_microsoft_connection() {
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert_eq!(DEF.connection, Some("microsoft"));
        assert_eq!(DEF.meta.id, SOURCE);
        assert_eq!(DEF.meta.domain, "tasks");
    }
}
