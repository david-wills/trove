//! Todoist — cloud task manager. A **Periodic** cloud pull of your active
//! tasks into the already-bound [`crate::tasks`] contract. Catalogued in the
//! Phase 2 pass; brief: docs/integrations/todoist.md.
//!
//! Two destinations, written in one pass:
//!
//! - **tasks contract** under `tasks/todoist/` (the *existing* Rust-bound
//!   [`crate::tasks`] contract): every active task → a [`Task`], persisted via
//!   `apply_tasks_sync` exactly like the GitHub tasks-leg. The snapshot diff
//!   reconstructs completions/deletions: a task that vanishes between syncs is
//!   resolved by the fate closure (in the recent completed list ⇒ Completed
//!   with its `completed_at`; otherwise ⇒ Deleted; a transient error ⇒ Unknown
//!   carry-forward).
//! - **raw firehose** under `tasks/todoist/raw/YYYY-MM.jsonl`: the raw API task
//!   objects at full fidelity (partitioned by the task's `added_at` month,
//!   upserted by id) — the contract's `extra` plus this raw keeps nothing
//!   dropped.
//!
//! Auth is a personal API token (a secret), pasted via the connection's
//! [`ConnectMethod::TokenPaste`] and stored under `.trove/sync/` (0600) like
//! GitHub's PAT — it rides the `access_token` slot of a never-expiring
//! [`TokenSet`]. The token is verified with a real `GET /api/v1/projects` at
//! connect time; it never leaves the secret store (never logged, never in the
//! cursor or any vault file).
//!
//! ## API — Todoist v1 (`api.todoist.com/api/v1`; REST **v2 is deprecated**)
//!
//! Every call sends `Authorization: Bearer <token>`. The v1 list endpoints
//! paginate: `{ "results": [...], "next_cursor": "..." }` for `/projects` and
//! `/tasks`, `{ "items": [...], "next_cursor": "..." }` for the completed
//! endpoint. We drain `next_cursor` fully (`?cursor=<c>`) before mapping, and
//! read either wrapper key defensively.
//!
//! Endpoints used:
//! - `GET /api/v1/projects` → projects (`id`, `name`).
//! - `GET /api/v1/tasks` → active tasks (the unified v1 task shape: `id`,
//!   `content`, `description`, `project_id`, `section_id`, `parent_id`,
//!   `labels[]`, `priority` 1–4 where **4 = highest**, `due {date, datetime,
//!   string, is_recurring, timezone}`, `added_at`, `completed_at`).
//! - `GET /api/v1/tasks/completed/by_completion_date?since=…&until=…` →
//!   completed tasks (with `completed_at`), feeding the fate closure.
//!
//! ## Cursor / dedup
//!
//! `.trove/todoist-sync.json` (non-secret, rebuildable — the [`crate::github`]
//! cursor placement) holds the last successful sync time, used as the `since`
//! lower bound for the completed-window lookup. The raw firehose uses
//! upsert-into-partition (read the target month, merge by `id`, rewrite
//! sorted), so a re-sync never duplicates a task.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::sync::oauth::TokenSet;
use crate::tasks::{ProjectInfo, Task, TaskFate};
use crate::vault::Vault;

/// Raw firehose directory (full-fidelity API task objects).
const RAW_DIR: &str = "tasks/todoist/raw";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that's for 0600
/// secrets). Deleting it just widens the next completed-window lookup.
const SYNC_FILE: &str = ".trove/todoist-sync.json";

/// The service id under `.trove/sync/` where the API token is stored (the
/// GitHub-PAT slot: the token rides a never-expiring [`TokenSet`]).
const SERVICE: &str = "todoist";

const API_BASE: &str = "https://api.todoist.com/api/v1";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs in the watcher loop. Every 15 min, matching the other
/// task sources ([`crate::tasks::TASKS_SYNC_SECS`]).
pub const TODOIST_SYNC_SECS: u64 = 900;

/// How far back to ask for completed tasks when there is no prior cursor (a
/// first sync, or a deleted cursor). One week — Todoist's free plan only
/// retains a week of completion history anyway, so a wider window buys nothing.
const COMPLETED_LOOKBACK_DAYS: i64 = 7;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, "todoist")
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing token or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "todoist synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "todoist sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    let headline = format!(
        "Todoist synced — {} open tasks, {} completed, {} deleted",
        c("open"),
        c("completed"),
        c("deleted"),
    );
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "todoist",
        name: "Todoist",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Todoist active tasks and projects into the unified \
                      task store via the official API (api.todoist.com/api/v1), every \
                      15 minutes. Completions and deletions are reconstructed by \
                      diffing each sync against the last.",
        domain: "tasks",
        vault_path: "tasks/todoist/",
        toggleable: true,
        setup: &[
            "Connect with your Todoist API token on this card.",
            "Each sync snapshots your open tasks; completions accrue while the sync runs.",
        ],
        caveats: "The API returns only active tasks — completions are reconstructed by \
                  diffing, so completion history accrues only while the sync runs. \
                  Completed tasks older than one week need a Todoist paid plan; connect \
                  early to preserve history.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(TODOIST_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("todoist"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a personal API token, a SECRET).

/// Verify the pasted token with `GET /api/v1/projects`, then store it (0600).
/// A 401 bails with a clear message; the token is never logged.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Todoist API token");
    }
    let client = TodoistClient::new(API_BASE.to_string(), token.to_string());
    // A real call proves the token works and the account is reachable.
    match client.get_page("/projects", None) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Todoist rejected the token (401) — check it's your API token from \
             Settings → Integrations → Developer and hasn't been revoked"
        ),
        Err(e) => bail!("Todoist /projects check failed: {e}"),
    }
    // The token goes ONLY through the secret store (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored token. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the token is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Todoist".to_string(),
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // the API token doesn't expire
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: a personal token is self-service.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "todoist",
    display_name: "Todoist",
    methods: &[ConnectMethod::TokenPaste {
        label: "Todoist API token",
        help: "Paste your Todoist API token from Settings → Integrations → Developer.",
        placeholder: "0123456789abcdef…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["todoist"],
    setup: &[
        "In Todoist, open Settings → Integrations → Developer.",
        "Copy your API token.",
        "Paste it here — it's stored locally and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page plus the cursor to follow for the next page (`next_cursor`, null on
/// the last page). The items are the wrapper's `results` (or `items` for the
/// completed endpoint) array.
struct Page {
    items: Vec<Value>,
    next_cursor: Option<String>,
}

/// Status-level fetch errors: 401 wants distinct handling (clear reconnect),
/// everything else is a message. (Todoist's docs return 429 with a
/// `Retry-After` for rate limits, but the watcher loop's bounded cadence keeps
/// us well under the budget for a personal account, so a 429 is just treated as
/// a transient `Other` — the cursor is not advanced and the next tick retries.)
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

/// The endpoints the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait TodoistApi {
    /// `GET <path>` (path is API-relative, e.g. `/tasks`). `cursor`, when set,
    /// is appended as `cursor=<c>` to fetch the next page. Returns one page +
    /// its `next_cursor`.
    fn get_page(&self, path: &str, cursor: Option<&str>) -> Result<Page, FetchError>;
}

/// Thin client; base URL injected (the github/oura/lastfm pattern).
struct TodoistClient {
    base: String,
    token: String,
}

impl TodoistClient {
    fn new(base: String, token: String) -> Self {
        TodoistClient { base, token }
    }

    fn handle(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<Page, FetchError> {
        match resp {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(parse_page(v))
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl TodoistApi for TodoistClient {
    fn get_page(&self, path: &str, cursor: Option<&str>) -> Result<Page, FetchError> {
        // Path may already carry a `?` query (the completed window): append the
        // cursor with the right separator.
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = match cursor {
            Some(c) => format!("{}{path}{sep}cursor={}", self.base, urlencode(c)),
            None => format!("{}{path}", self.base),
        };
        TodoistClient::handle(
            ureq::get(&url)
                .timeout(HTTP_TIMEOUT)
                .set("Authorization", &format!("Bearer {}", self.token))
                .call(),
        )
    }
}

/// Pull the items array + `next_cursor` out of a v1 list response. The active
/// endpoints wrap items under `results`, the completed endpoint under `items` —
/// accept either. A bare array (some endpoints / a future shape change) is
/// tolerated as a single un-paged page.
fn parse_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let items = o
                .get("results")
                .or_else(|| o.get("items"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next_cursor = o
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Page { items, next_cursor }
        }
        Value::Array(a) => Page { items: a, next_cursor: None },
        _ => Page { items: Vec::new(), next_cursor: None },
    }
}

/// Drain every page of an API-relative path (following `next_cursor`),
/// collecting all items.
fn drain_all(api: &impl TodoistApi, path: &str) -> Result<Vec<Value>, FetchError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = api.get_page(path, cursor.as_deref())?;
        items.extend(page.items);
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync. The completed-window
    /// `since` lower bound for the next pull (so we ask only for completions
    /// since we last looked, with a small floor). Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sync: Option<String>,
    /// Optional Sync-API `sync_token` for a future incremental path (not used
    /// by the v1 REST pull yet; kept so enabling it later is back-compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sync_token: Option<String>,
}

impl Vault {
    fn read_todoist_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_todoist_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity API task object, kept for the raw firehose).

/// One raw API task object in `tasks/todoist/raw/YYYY-MM.jsonl`. The on-disk
/// line is the verbatim API object (flattened — no synthetic keys added, so a
/// raw row round-trips byte-identically and stays full-fidelity). `guid` (id)
/// and `added_at` (partition key) are read back off the object via accessors,
/// not stored as extra columns — storing them too would duplicate the JSON
/// keys and break deserialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawTask {
    /// The complete API object, untouched.
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawTask {
    /// The task id (dedup key).
    fn guid(&self) -> String {
        self.fields.get("id").and_then(value_id).unwrap_or_default()
    }

    /// The creation timestamp (partition key): `added_at`, falling back to
    /// `created_at` from any older shape.
    fn added_at(&self) -> &str {
        self.fields
            .get("added_at")
            .or_else(|| self.fields.get("created_at"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// A raw v1 task object → [`RawTask`] for the firehose. `None` only when it has
/// no id (can't dedup) or no creation timestamp (can't be partitioned).
fn raw_task(value: &Value) -> Option<RawTask> {
    let obj = value.as_object()?;
    obj.get("id").and_then(value_id)?; // must have an id
    obj.get("added_at")
        .or_else(|| obj.get("created_at"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?; // must have a creation timestamp
    Some(RawTask { fields: obj.clone() })
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// An id field that may be a string or a JSON number (Todoist returns string
/// ids in v1, but be defensive) → a `String`.
fn value_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Pull a string field, trimmed; `None` when missing/non-string/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Map Todoist's priority (1 normal … 4 highest, where 1 is the API default)
/// onto the task contract's TickTick scale (0 none, 1 low, 3 medium, 5 high).
/// The raw todoist priority is preserved in `extra` regardless.
fn map_priority(p: i64) -> i64 {
    match p {
        2 => 1,
        3 => 3,
        4 => 5,
        _ => 0, // 1 (the API default "no priority") and anything unexpected
    }
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable values pass through
/// verbatim rather than being dropped (the [`crate::github::to_local`] idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// A raw v1 task object → a normalized [`Task`], resolving the project NAME
/// from `projects` (id→name). Times become RFC3339 **local** (the task
/// contract is local time); the raw layer keeps the source's UTC. `None` only
/// when the object has no id or no `content`.
fn task_from_value(value: &Value, project_names: &HashMap<String, String>) -> Option<Task> {
    let obj = value.as_object()?;
    let id = obj.get("id").and_then(value_id)?;
    let title = str_opt(value, "content")?;

    let project_id = obj.get("project_id").and_then(value_id);
    let project = project_id
        .as_ref()
        .and_then(|pid| project_names.get(pid))
        .cloned()
        .unwrap_or_default();

    // due: { date, datetime, string, is_recurring, timezone }. `datetime`
    // (timed) wins over `date` (all-day) for the due instant; absent → no due.
    let due_obj = obj.get("due").and_then(Value::as_object);
    let datetime = due_obj.and_then(|d| str_opt(&Value::Object(d.clone()), "datetime"));
    let date = due_obj.and_then(|d| str_opt(&Value::Object(d.clone()), "date"));
    let is_recurring = due_obj
        .and_then(|d| d.get("is_recurring"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let due_string = due_obj.and_then(|d| str_opt(&Value::Object(d.clone()), "string"));
    let all_day = datetime.is_none() && date.is_some();
    let due = datetime
        .clone()
        .or_else(|| date.clone())
        .map(|s| to_local(&s));

    let labels: Vec<String> = obj
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let todoist_priority = obj.get("priority").and_then(Value::as_i64).unwrap_or(1);

    // Source-specific fields ride in `extra` (sub-task parent ids, section,
    // url, the raw priority, recurrence flag, …).
    let mut extra = Map::new();
    if let Some(section) = obj.get("section_id") {
        if !section.is_null() {
            extra.insert("section_id".into(), section.clone());
        }
    }
    if let Some(parent) = obj.get("parent_id") {
        if !parent.is_null() {
            extra.insert("parent_id".into(), parent.clone());
        }
    }
    // `url` is derived client-side from id+content in v1; keep a server-sent
    // one if present, else synthesize the canonical app URL.
    let url = str_opt(value, "url")
        .unwrap_or_else(|| format!("https://app.todoist.com/app/task/{id}"));
    extra.insert("url".into(), Value::from(url));
    extra.insert("todoist_priority".into(), Value::from(todoist_priority));
    extra.insert("is_recurring".into(), Value::from(is_recurring));

    let created = obj
        .get("added_at")
        .or_else(|| obj.get("created_at"))
        .and_then(Value::as_str)
        .map(to_local);

    Some(Task {
        source: "todoist".into(),
        id,
        title,
        project,
        notes: str_opt(value, "description").unwrap_or_default(),
        status: "open".into(),
        priority: map_priority(todoist_priority),
        due,
        start: None,
        all_day,
        recurrence: if is_recurring { due_string } else { None },
        tags: labels,
        subtasks: Vec::new(),
        created,
        modified: str_opt(value, "updated_at").map(|s| to_local(&s)),
        completed: None,
        extra,
    })
}

/// One `/projects` item → [`ProjectInfo`] (id + name). `None` without both.
fn project_info(value: &Value) -> Option<ProjectInfo> {
    let obj = value.as_object()?;
    Some(ProjectInfo {
        id: obj.get("id").and_then(value_id)?,
        name: str_opt(value, "name")?,
    })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition (the github idiom): read the target month, merge
// new rows by id keeping the freshest, rewrite that partition sorted. A re-sync
// over an overlapping window never duplicates an id.

fn upsert_raw(vault: &Vault, rows: Vec<RawTask>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawTask>> = BTreeMap::new();
    for r in rows {
        let added = r.added_at().to_string();
        let key = Partition::Month
            .key(&added)
            .with_context(|| format!("todoist: raw task added_at {added:?} has no month"))?
            .to_string();
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
                Some(i) => existing[i] = r, // always take the freshest snapshot
                None => {
                    idx.insert(r.guid(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| {
            a.added_at().cmp(b.added_at()).then_with(|| a.guid().cmp(&b.guid()))
        });
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync. Missing token ⇒ a quiet skip on the periodic
/// path (mirror github/lastfm), a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Todoist is not connected — add your API token in the Integrations tab")?;
    let client = TodoistClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client, Local::now())
}

/// The pull body over an injected API + clock — the testable seam.
fn pull_with(vault: &Vault, api: &impl TodoistApi, now: DateTime<Local>) -> Result<PullOutcome> {
    let mut state = vault.read_todoist_sync();

    // --- projects: id→name + ProjectInfo list ----------------------------
    let project_items = drain_all(api, "/projects").map_err(fetch_err)?;
    let projects: Vec<ProjectInfo> = project_items.iter().filter_map(project_info).collect();
    let project_names: HashMap<String, String> = projects
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();

    // --- active tasks: raw firehose + normalized Tasks -------------------
    let task_items = drain_all(api, "/tasks").map_err(fetch_err)?;
    let raw_rows: Vec<RawTask> = task_items.iter().filter_map(raw_task).collect();
    let raw_new = upsert_raw(vault, raw_rows)?;
    let fresh: Vec<Task> = task_items
        .iter()
        .filter_map(|v| task_from_value(v, &project_names))
        .collect();

    // --- completed window (for the fate closure), fetched once per pull ---
    // Ask from the last sync (with a small floor) to now. Built lazily and
    // cached so the closure does no network per vanished task.
    let since = state
        .last_sync
        .as_deref()
        .map(to_utc_for_query)
        .unwrap_or_else(|| {
            (now - chrono::Duration::days(COMPLETED_LOOKBACK_DAYS))
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        });
    let until = now
        .with_timezone(&chrono::Utc)
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let completed = fetch_completed(api, &since, &until);

    // --- diff into the bound task contract via apply_tasks_sync ----------
    let stats = vault
        .apply_tasks_sync("todoist", &projects, fresh, |t| {
            todoist_fate(t, completed.as_ref())
        })
        .context("todoist: applying task sync")?;

    state.last_sync = Some(now.to_rfc3339());
    vault.write_todoist_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open Todoist tasks", stats.open),
        counts,
    })
}

/// Map a [`FetchError`] at the top of the pull into an anyhow error with a
/// clear reconnect message for 401.
fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Todoist rejected the token (401) — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Todoist fetch failed: {other}"),
    }
}

/// A stored RFC3339-local cursor → the `YYYY-MM-DDTHH:MM:SS` UTC form the
/// completed-window query wants. Falls back to the first 19 chars if it can't
/// parse (still a valid datetime prefix).
fn to_utc_for_query(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
        .unwrap_or_else(|_| format!("{}Z", s.chars().take(19).collect::<String>()))
}

/// id → completed_at (local), for every recently-completed task. `None` when
/// the completed lookup itself failed (network/parse) — the fate closure then
/// returns `Unknown` for every vanished task (carry-forward), never guessing a
/// deletion off an incomplete picture.
fn fetch_completed(
    api: &impl TodoistApi,
    since: &str,
    until: &str,
) -> Option<HashMap<String, String>> {
    let path = format!(
        "/tasks/completed/by_completion_date?since={}&until={}",
        urlencode(since),
        urlencode(until)
    );
    let items = drain_all(api, &path).ok()?;
    let mut map = HashMap::new();
    for v in &items {
        if let Some(obj) = v.as_object() {
            if let Some(id) = obj.get("id").and_then(value_id) {
                let when = obj
                    .get("completed_at")
                    .and_then(Value::as_str)
                    .map(to_local);
                // Completed but no timestamp: still record (empty) so the fate
                // resolves to Completed (stamped with `now`) not Deleted.
                map.insert(id, when.unwrap_or_default());
            }
        }
    }
    Some(map)
}

/// Resolve a task that vanished from the active set. In the recent completed
/// list ⇒ Completed(its `completed_at`, or `now` when blank); else, if we
/// *have* a completed list and it's absent ⇒ Deleted; if the completed lookup
/// failed (`None`) ⇒ Unknown carry-forward. Mirrors `github_fate`'s structure.
fn todoist_fate(task: &Task, completed: Option<&HashMap<String, String>>) -> TaskFate {
    match completed {
        Some(map) => match map.get(&task.id) {
            Some(when) if !when.is_empty() => TaskFate::Completed(Some(when.clone())),
            Some(_) => TaskFate::Completed(None), // completed, no timestamp → now
            None => TaskFate::Deleted,
        },
        None => TaskFate::Unknown,
    }
}

/// Minimal percent-encoding for query values (the `:` in timestamps, `+` in an
/// offset, etc.) — the [`crate::github::urlencode`] idiom.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{HashSet, VecDeque};

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-todoist-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-06-14T12:00:00-07:00")
            .unwrap()
            .with_timezone(&Local)
    }

    // --- fixtures (the official v1 unified task/project shapes) ----------

    fn project_json(id: &str, name: &str) -> Value {
        serde_json::json!({
            "id": id,
            "name": name,
            "color": "charcoal",
            "is_favorite": false,
            "is_archived": false,
            "view_style": "list",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        })
    }

    /// A timed task (datetime due) with a label and p4 (highest).
    fn task_timed(id: &str, project_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "user_id": "u1",
            "project_id": project_id,
            "section_id": "sec1",
            "parent_id": null,
            "content": "Ship the release",
            "description": "cut the tag, push",
            "labels": ["work", "urgent"],
            "priority": 4,
            "due": {
                "date": "2026-06-15",
                "datetime": "2026-06-15T17:00:00Z",
                "string": "Jun 15 5pm",
                "is_recurring": false,
                "timezone": "America/Los_Angeles"
            },
            "deadline": null,
            "duration": null,
            "checked": false,
            "is_deleted": false,
            "added_at": "2026-06-01T08:00:00Z",
            "updated_at": "2026-06-10T09:00:00Z",
            "completed_at": null,
            "child_order": 1
        })
    }

    /// An all-day task (date-only due), p1 (default/none), no labels.
    fn task_all_day(id: &str, project_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "project_id": project_id,
            "section_id": null,
            "parent_id": null,
            "content": "Pay rent",
            "description": "",
            "labels": [],
            "priority": 1,
            "due": {
                "date": "2026-06-20",
                "string": "Jun 20",
                "is_recurring": false
            },
            "checked": false,
            "is_deleted": false,
            "added_at": "2026-06-02T10:00:00Z",
            "completed_at": null
        })
    }

    /// A recurring task: is_recurring true, p3 (medium), date-only.
    fn task_recurring(id: &str, project_id: &str) -> Value {
        serde_json::json!({
            "id": id,
            "project_id": project_id,
            "section_id": null,
            "parent_id": null,
            "content": "Water plants",
            "description": "",
            "labels": ["home"],
            "priority": 3,
            "due": {
                "date": "2026-06-15",
                "string": "every day",
                "is_recurring": true
            },
            "checked": false,
            "is_deleted": false,
            "added_at": "2026-05-01T07:00:00Z",
            "completed_at": null
        })
    }

    /// A completed task as the completed-by-date endpoint returns it (same
    /// unified shape, with completed_at set).
    fn completed_json(id: &str, project_id: &str, completed_at: &str) -> Value {
        serde_json::json!({
            "id": id,
            "project_id": project_id,
            "content": "Ship the release",
            "priority": 4,
            "labels": [],
            "checked": true,
            "is_deleted": false,
            "added_at": "2026-06-01T08:00:00Z",
            "completed_at": completed_at
        })
    }

    // --- a scripted mock API --------------------------------------------

    /// Maps a request path *prefix* to a queue of pages (front = page 1), so a
    /// multi-page (cursor) endpoint can be drained.
    struct MockApi {
        pages: RefCell<Vec<(String, VecDeque<Page>)>>,
        // path-prefix that should return Unauthorized.
        unauthorized: RefCell<HashSet<String>>,
        // every (path, cursor) request, recorded.
        requests: RefCell<Vec<(String, Option<String>)>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                unauthorized: RefCell::new(HashSet::new()),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn page(&self, prefix: &str, items: Vec<Value>, next_cursor: Option<String>) {
            self.pages
                .borrow_mut()
                .push((prefix.into(), VecDeque::from(vec![Page { items, next_cursor }])));
        }

        fn pages_seq(&self, prefix: &str, seq: Vec<Page>) {
            self.pages.borrow_mut().push((prefix.into(), VecDeque::from(seq)));
        }

        fn requested(&self, needle: &str) -> bool {
            self.requests.borrow().iter().any(|(p, _)| p.contains(needle))
        }
    }

    impl TodoistApi for MockApi {
        fn get_page(&self, path: &str, cursor: Option<&str>) -> Result<Page, FetchError> {
            self.requests
                .borrow_mut()
                .push((path.to_string(), cursor.map(str::to_string)));
            if self.unauthorized.borrow().iter().any(|p| path.starts_with(p)) {
                return Err(FetchError::Unauthorized);
            }
            let mut pages = self.pages.borrow_mut();
            for (prefix, queue) in pages.iter_mut() {
                if path.starts_with(prefix.as_str()) || path.contains(prefix.as_str()) {
                    if let Some(p) = queue.pop_front() {
                        return Ok(p);
                    }
                }
            }
            Ok(Page { items: Vec::new(), next_cursor: None })
        }
    }

    /// Register the projects + tasks + (empty) completed pages for a pull.
    /// The completed prefix is registered before `/tasks` so the more specific
    /// `/tasks/completed` match wins.
    fn base_mock(projects: Vec<Value>, tasks: Vec<Value>, completed: Vec<Value>) -> MockApi {
        let api = MockApi::new();
        api.page("/projects", projects, None);
        api.page("/tasks/completed", completed, None);
        api.page("/tasks", tasks, None);
        api
    }

    // --- pure mapping tests ---------------------------------------------

    #[test]
    fn maps_active_task_with_name_priority_due_local_labels_guid() {
        let names: HashMap<String, String> =
            [("p1".to_string(), "Work".to_string())].into_iter().collect();
        let t = task_from_value(&task_timed("T1", "p1"), &names).unwrap();
        assert_eq!(t.source, "todoist");
        assert_eq!(t.id, "T1", "guid is the task id");
        assert_eq!(t.title, "Ship the release");
        assert_eq!(t.project, "Work", "project resolved to NAME from project_id");
        assert_eq!(t.notes, "cut the tag, push");
        assert_eq!(t.status, "open");
        assert_eq!(t.priority, 5, "todoist p4 → contract 5 (high)");
        assert_eq!(t.tags, vec!["work", "urgent"]);
        assert!(!t.all_day, "has a datetime → timed");
        // due = the datetime, converted to local (same instant as the UTC).
        let due = t.due.as_deref().unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(due).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-15T17:00:00Z").unwrap().timestamp(),
        );
        // created from added_at, local.
        assert!(t.created.is_some());
        // extra carries the source-specific bits.
        assert_eq!(t.extra.get("section_id").and_then(Value::as_str), Some("sec1"));
        assert_eq!(t.extra.get("todoist_priority").and_then(Value::as_i64), Some(4));
        assert_eq!(t.extra.get("is_recurring").and_then(Value::as_bool), Some(false));
        assert!(t.extra.get("url").and_then(Value::as_str).unwrap().contains("T1"));
    }

    #[test]
    fn priority_mapping_covers_all_four_levels() {
        assert_eq!(map_priority(1), 0, "p1 (default) → none");
        assert_eq!(map_priority(2), 1, "p2 → low");
        assert_eq!(map_priority(3), 3, "p3 → medium");
        assert_eq!(map_priority(4), 5, "p4 (highest) → high");
        assert_eq!(map_priority(0), 0, "unexpected → none");
    }

    #[test]
    fn all_day_vs_timed_due() {
        let names = HashMap::new();
        let timed = task_from_value(&task_timed("a", "p1"), &names).unwrap();
        assert!(!timed.all_day);
        let allday = task_from_value(&task_all_day("b", "p1"), &names).unwrap();
        assert!(allday.all_day, "date-only due is all-day");
        assert_eq!(allday.priority, 0);
        // Due is the date, converted (the day survives).
        assert!(allday.due.as_deref().unwrap().starts_with("2026-06-2"));
    }

    #[test]
    fn recurring_task_carries_recurrence_string() {
        let names = HashMap::new();
        let r = task_from_value(&task_recurring("r", "p1"), &names).unwrap();
        assert_eq!(r.recurrence.as_deref(), Some("every day"));
        assert_eq!(r.priority, 3);
        assert_eq!(r.extra.get("is_recurring").and_then(Value::as_bool), Some(true));
        // A non-recurring task gets no recurrence even though it has a due string.
        let nr = task_from_value(&task_timed("t", "p1"), &names).unwrap();
        assert_eq!(nr.recurrence, None);
    }

    #[test]
    fn project_maps_id_and_name() {
        let p = project_info(&project_json("p9", "Errands")).unwrap();
        assert_eq!(p.id, "p9");
        assert_eq!(p.name, "Errands");
    }

    #[test]
    fn parse_page_reads_results_items_and_bare_array() {
        let results = parse_page(serde_json::json!({"results": [1, 2], "next_cursor": "c1"}));
        assert_eq!(results.items.len(), 2);
        assert_eq!(results.next_cursor.as_deref(), Some("c1"));
        let items = parse_page(serde_json::json!({"items": [1], "next_cursor": null}));
        assert_eq!(items.items.len(), 1);
        assert_eq!(items.next_cursor, None);
        let bare = parse_page(serde_json::json!([1, 2, 3]));
        assert_eq!(bare.items.len(), 3);
    }

    // --- pagination drain ------------------------------------------------

    #[test]
    fn drains_all_pages_following_next_cursor() {
        let api = MockApi::new();
        api.pages_seq(
            "/tasks",
            vec![
                Page { items: vec![task_all_day("a", "p1")], next_cursor: Some("CUR2".into()) },
                Page { items: vec![task_all_day("b", "p1")], next_cursor: None },
            ],
        );
        let items = drain_all(&api, "/tasks").unwrap();
        assert_eq!(items.len(), 2, "both pages drained");
        // The second request carried the cursor.
        assert!(api.requests.borrow().iter().any(|(_, c)| c.as_deref() == Some("CUR2")));
    }

    // --- full pull + dual write -----------------------------------------

    #[test]
    fn full_pull_writes_snapshot_raw_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = base_mock(
            vec![project_json("p1", "Work")],
            vec![task_timed("T1", "p1"), task_all_day("T2", "p1")],
            vec![],
        );

        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("open"), Some(&2));
        assert_eq!(out.counts.get("created"), Some(&2), "first sync: two creations");
        assert_eq!(out.counts.get("raw"), Some(&2));

        // Snapshot in the bound contract.
        let snap = v.load_tasks_snapshot("todoist").unwrap();
        assert_eq!(snap.len(), 2);
        let t1 = snap.iter().find(|t| t.id == "T1").unwrap();
        assert_eq!(t1.project, "Work");
        assert_eq!(t1.priority, 5);

        // Raw firehose partitioned by added_at month, full fidelity.
        assert!(v.root().join("tasks/todoist/raw/2026-06.jsonl").exists());
        let raw = std::fs::read_to_string(v.root().join("tasks/todoist/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"user_id\":\"u1\""), "raw keeps fields the contract drops");
        assert!(raw.contains("Ship the release"));

        // Cursor (non-secret) advanced; carries NO token.
        let state = v.read_todoist_sync();
        assert!(state.last_sync.is_some());
        let cursor_body = std::fs::read_to_string(v.root().join(".trove/todoist-sync.json")).unwrap();
        assert!(!cursor_body.contains("access_token"));
        assert!(!cursor_body.contains("secret"));
    }

    #[test]
    fn resync_dedupes_raw_by_guid() {
        let v = temp_vault("rawdedup");
        let api1 = base_mock(vec![project_json("p1", "Work")], vec![task_timed("T1", "p1")], vec![]);
        pull_with(&v, &api1, now()).unwrap();

        // Re-sync the SAME task: no new raw row, no duplicate line.
        let api2 = base_mock(vec![project_json("p1", "Work")], vec![task_timed("T1", "p1")], vec![]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("raw"), Some(&0), "no new raw guids on re-sync");
        let raw = std::fs::read_to_string(v.root().join("tasks/todoist/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "guid dedup — one line");
    }

    // --- THE fate tests (completed / deleted) ----------------------------

    #[test]
    fn task_then_completed_logs_completed_event_with_completed_time() {
        let v = temp_vault("fate-complete");
        // Sync 1: one open task.
        let api1 = base_mock(vec![project_json("p1", "Work")], vec![task_timed("T1", "p1")], vec![]);
        pull_with(&v, &api1, now()).unwrap();
        assert_eq!(v.load_tasks_snapshot("todoist").unwrap().len(), 1);

        // Sync 2: the task vanished from active, and shows up in the completed
        // list with a completed_at → a completed event at that time.
        let api2 = base_mock(
            vec![project_json("p1", "Work")],
            vec![], // no active tasks now
            vec![completed_json("T1", "p1", "2026-06-13T16:30:00Z")],
        );
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("completed"), Some(&1));

        assert!(v.load_tasks_snapshot("todoist").unwrap().is_empty());
        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completed: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completed.len(), 1, "the completed task logged a completion");
        assert_eq!(completed[0].task.id, "T1");
        assert_eq!(
            DateTime::parse_from_rfc3339(&completed[0].time).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-13T16:30:00Z").unwrap().timestamp(),
        );
        assert!(v.root().join("tasks/todoist/events/2026-06.jsonl").exists());
    }

    #[test]
    fn task_then_gone_and_not_completed_is_deleted() {
        let v = temp_vault("fate-delete");
        let api1 = base_mock(vec![project_json("p1", "Work")], vec![task_timed("T1", "p1")], vec![]);
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: gone from active AND absent from the completed list → Deleted.
        let api2 = base_mock(vec![project_json("p1", "Work")], vec![], vec![]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("deleted"), Some(&1));

        // Deletions are stamped at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "T1"));
        assert!(!events.iter().any(|e| e.kind == "completed" && e.task.id == "T1"));
    }

    #[test]
    fn completed_lookup_failure_carries_task_forward_unknown() {
        let v = temp_vault("fate-unknown");
        let api1 = base_mock(vec![project_json("p1", "Work")], vec![task_timed("T1", "p1")], vec![]);
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: active empty; the completed endpoint ERRORS (simulated by
        // making the completed path return Unauthorized). The fate is Unknown.
        let api2 = MockApi::new();
        api2.page("/projects", vec![project_json("p1", "Work")], None);
        api2.page("/tasks/completed", vec![], None);
        api2.page("/tasks", vec![], None);
        api2.unauthorized.borrow_mut().insert("/tasks/completed".into());
        pull_with(&v, &api2, now()).unwrap();

        // Unknown fate → the task stays in the snapshot, no event.
        let snap = v.load_tasks_snapshot("todoist").unwrap();
        assert_eq!(snap.len(), 1, "carried forward on an unknown fate");
        assert_eq!(snap[0].id, "T1");
        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        assert!(!events.iter().any(|e| e.task.id == "T1" && (e.kind == "deleted" || e.kind == "completed")));
    }

    #[test]
    fn todoist_fate_resolution_matrix() {
        let mut map = HashMap::new();
        map.insert("done".to_string(), "2026-06-13T16:30:00-07:00".to_string());
        map.insert("done_no_ts".to_string(), String::new());
        let t = |id: &str| Task {
            source: "todoist".into(),
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
        // In the completed list with a time → Completed(time).
        match todoist_fate(&t("done"), Some(&map)) {
            TaskFate::Completed(Some(w)) => assert!(w.starts_with("2026-06-13")),
            _ => panic!("expected Completed(time)"),
        }
        // Completed but blank time → Completed(None) (diff stamps `now`).
        assert!(matches!(todoist_fate(&t("done_no_ts"), Some(&map)), TaskFate::Completed(None)));
        // Have a list, absent from it → Deleted.
        assert!(matches!(todoist_fate(&t("ghost"), Some(&map)), TaskFate::Deleted));
        // No list (lookup failed) → Unknown.
        assert!(matches!(todoist_fate(&t("done"), None), TaskFate::Unknown));
    }

    // --- connection tests -----------------------------------------------

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        // Store directly (def_connect needs the network for /projects).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tdt_secret_abc".into(),
                refresh_token: None,
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Todoist");
        assert_eq!(status.accounts[0].key, "todoist");

        // The token is NOT in any non-secret file (the cursor).
        v.write_todoist_sync(&SyncState {
            last_sync: Some(now().to_rfc3339()),
            sync_token: Some("a-sync-token-not-the-api-token".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/todoist-sync.json")).unwrap();
        assert!(!cursor.contains("tdt_secret_abc"), "API token never in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Find the stored secret file under .trove/sync and assert 0600.
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("tdt_secret_abc") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
            assert!(found, "the token was stored under .trove/sync");
        }

        def_disconnect(&v, "todoist").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn empty_token_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_sync.is_none());
        assert!(empty.sync_token.is_none());
        // A cursor with only last_sync still deserializes (an older file).
        let partial: SyncState =
            serde_json::from_str(r#"{"last_sync":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert_eq!(partial.last_sync.as_deref(), Some("2026-06-01T00:00:00-07:00"));
        assert!(partial.sync_token.is_none());
    }

    #[test]
    fn completed_window_query_uses_cursor_since_on_resync() {
        let v = temp_vault("window");
        // Seed a cursor so the second pull asks since the last sync.
        v.write_todoist_sync(&SyncState {
            last_sync: Some("2026-06-13T00:00:00-07:00".into()),
            sync_token: None,
        })
        .unwrap();
        let api = base_mock(vec![project_json("p1", "Work")], vec![], vec![]);
        pull_with(&v, &api, now()).unwrap();
        // The completed query carried a `since` derived from the cursor (the
        // 2026-06-13 day, in UTC form).
        assert!(
            api.requested("/tasks/completed/by_completion_date") && api.requested("since=2026-06-13"),
            "completed window since= came from the cursor: {:?}",
            api.requests.borrow()
        );
    }

    #[test]
    fn raw_task_roundtrips_full_fidelity() {
        // A raw row written then read back must be byte-identical (no synthetic
        // guid/added_at columns added), and its accessors must work.
        let r = raw_task(&task_timed("T1", "p1")).unwrap();
        assert_eq!(r.guid(), "T1");
        assert_eq!(r.added_at(), "2026-06-01T08:00:00Z");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("\"guid\""), "no synthetic guid column on disk");
        let back: RawTask = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r, "round-trips identically");
        assert!(line.contains("\"user_id\":\"u1\""), "all source fields kept");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "todoist");
    }
}
