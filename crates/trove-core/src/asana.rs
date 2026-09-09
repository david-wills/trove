//! Asana — cloud project and task management via the official REST API.
//!
//! Pulls tasks assigned to the authenticated user across all their Asana
//! workspaces into the bound [`crate::tasks`] contract. Two destinations
//! are written in one pass:
//!
//! - **tasks contract** under `tasks/asana/` — snapshot + event stream via
//!   [`crate::tasks::apply_tasks_sync`], exactly like the Todoist/GitHub legs.
//! - **raw firehose** under `tasks/asana/raw/YYYY-MM.jsonl` — the API task
//!   objects for the requested `opt_fields` set, partitioned by `created_at`
//!   month, upserted by GID.
//!
//! # Auth
//!
//! Personal Access Token (PAT) from the Asana Developer Console (My apps →
//! Personal access tokens). Sent as `Authorization: Bearer <token>`. No app
//! registration needed — the PAT is instant and never expires (until revoked).
//! Stored in `.trove/sync/asana` (0600) via the never-expiring
//! [`crate::sync::oauth::TokenSet`] pattern (todoist precedent).
//!
//! # API
//!
//! Asana REST v1 (`app.asana.com/api/1.0`). Pagination is offset-based: a
//! response that has more data includes `next_page.offset`; the next request
//! sends `?offset=<token>`. We always pass `limit=100` and drain pages fully
//! before the first contract write.
//!
//! Endpoints used:
//! - `GET /workspaces` → workspace GIDs (all workspaces visible to the token).
//! - `GET /tasks?assignee=me&workspace={gid}&opt_fields=…&completed_since=…`
//!   → assigned tasks per workspace (open + recently completed in one call).
//!
//! Confirmed field names (Asana OpenAPI spec — defs/asana_oas.yaml):
//! `gid`, `name`, `notes`, `due_on` (date string), `due_at` (datetime),
//! `completed` (bool), `completed_at` (datetime), `created_at`, `modified_at`,
//! `assignee`, `projects[]`, `tags[]`, `followers[]`, `memberships[]`
//! (project+section), `workspace`, `permalink_url`, `resource_type`.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/asana.md.

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

/// The source id: folder name under `tasks/`, key in `.trove/tasks-sync.json`,
/// and every task row's `source` field.
const SOURCE: &str = "asana";

/// Raw firehose directory (requested opt_fields API task objects).
const RAW_DIR: &str = "tasks/asana/raw";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that is 0600).
/// Deleting it just widens the history window on the next pull.
const SYNC_FILE: &str = ".trove/asana-sync.json";

/// The service id under `.trove/sync/` where the PAT is stored (0600).
const SERVICE: &str = "asana";

const API_BASE: &str = "https://app.asana.com/api/1.0";
/// Kept short so a hung connection cannot stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs: every 15 min, matching the other task sources.
pub const ASANA_SYNC_SECS: u64 = 900;

/// Maximum page size the Asana API accepts.
const PAGE_LIMIT: u64 = 100;

/// `opt_fields` requested on every task fetch. Only these fields appear in the
/// response (the raw firehose stores exactly this set). `start_on`/`start_at`
/// are included so the contract `start` field is populated (parallel to
/// `due_on`/`due_at`).
const TASK_OPT_FIELDS: &str = "gid,name,notes,completed,completed_at,created_at,modified_at,\
    due_on,due_at,start_on,start_at,assignee.name,assignee.gid,projects.gid,projects.name,\
    tags.gid,tags.name,followers.gid,followers.name,memberships.project.gid,\
    memberships.project.name,memberships.section.gid,memberships.section.name,\
    workspace.gid,workspace.name,permalink_url,resource_type";

/// How far back to fetch completed tasks when there is no prior cursor.
const COMPLETED_LOOKBACK_DAYS: i64 = 90;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::tasks::source_last_data(vault, SOURCE)
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "asana synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "asana sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Asana synced — {} open tasks, {} completed, {} deleted",
            c("open"),
            c("completed"),
            c("deleted"),
        ),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "asana",
        name: "Asana",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls tasks assigned to you across all your Asana workspaces — open and \
                      recently completed — into the unified task store every 15 minutes. \
                      Connect with a Personal Access Token from your Asana Developer Console.",
        domain: "tasks",
        vault_path: "tasks/asana/",
        toggleable: true,
        setup: &[
            "Connect with your Asana Personal Access Token on this card.",
            "Each sync captures all tasks assigned to you and reconstructs completions/deletions.",
        ],
        caveats: "Requires a Personal Access Token from your Asana profile → \
                  My Apps → Personal access tokens. Completion history is captured \
                  going back 90 days on the first sync.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(ASANA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("asana"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a Personal Access Token, a SECRET).

/// Verify the pasted PAT with `GET /users/me`, then store it (0600).
/// A 401 bails with a clear reconnect message; the token is never logged.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Asana Personal Access Token");
    }
    let client = AsanaClient::new(API_BASE.to_string(), token.to_string());
    match client.get_list("/users/me", &[], None) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Asana rejected the token (401) — check it's a valid Personal Access Token \
             from your Asana profile → My Apps → Personal access tokens and hasn't been revoked"
        ),
        Err(e) => bail!("Asana /users/me check failed: {e}"),
    }
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

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Asana".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "asana",
    display_name: "Asana",
    methods: &[ConnectMethod::TokenPaste {
        label: "Asana Personal Access Token",
        help: "Paste a Personal Access Token from your Asana profile → \
               My Apps → Personal access tokens.",
        placeholder: "1/1234567890:abcdef0123456789…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["asana"],
    setup: &[
        "In Asana, click your profile photo → My Settings → Apps → Personal access tokens.",
        "Click \"New access token\", give it a name, and copy the generated token.",
        "Paste it here — it is stored locally and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page of results plus the pagination offset for the next page, if any.
struct Page {
    items: Vec<Value>,
    next_offset: Option<String>,
}

/// Status-level fetch errors. 401 = reconnect prompt; everything else is a
/// transient error string (the cursor is not advanced; the next tick retries).
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

/// The endpoints the pull needs. A trait so tests run fully offline.
trait AsanaApi {
    /// `GET <path>` with additional query `params` (key/value pairs). The
    /// `offset` token, when set, pages through results.
    fn get_list(
        &self,
        path: &str,
        params: &[(&str, &str)],
        offset: Option<&str>,
    ) -> Result<Page, FetchError>;
}

/// Thin ureq client with an injected base URL.
struct AsanaClient {
    base: String,
    token: String,
}

impl AsanaClient {
    fn new(base: String, token: String) -> Self {
        AsanaClient { base, token }
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

impl AsanaApi for AsanaClient {
    fn get_list(
        &self,
        path: &str,
        params: &[(&str, &str)],
        offset: Option<&str>,
    ) -> Result<Page, FetchError> {
        let url = format!("{}{path}", self.base);
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .query("limit", &PAGE_LIMIT.to_string());
        for (k, v) in params {
            req = req.query(k, v);
        }
        if let Some(off) = offset {
            req = req.query("offset", off);
        }
        AsanaClient::handle(req.call())
    }
}

/// Extract the `data` array and `next_page.offset` from an Asana list
/// response. Shape: `{ "data": [...], "next_page": { "offset": "…" } | null }`.
fn parse_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let items = o
                .get("data")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next_offset = o
                .get("next_page")
                .and_then(Value::as_object)
                .and_then(|np| np.get("offset"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Page { items, next_offset }
        }
        Value::Array(a) => Page { items: a, next_offset: None },
        _ => Page { items: Vec::new(), next_offset: None },
    }
}

/// Drain every page of a list endpoint, following `next_page.offset`.
fn drain_all(
    api: &impl AsanaApi,
    path: &str,
    params: &[(&str, &str)],
) -> Result<Vec<Value>, FetchError> {
    let mut items = Vec::new();
    let mut offset: Option<String> = None;
    loop {
        let page = api.get_list(path, params, offset.as_deref())?;
        items.extend(page.items);
        match page.next_offset {
            Some(o) => offset = Some(o),
            None => break,
        }
    }
    Ok(items)
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync. Used as the lower bound
    /// for `completed_since` on the next pull. Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sync: Option<String>,
}

impl Vault {
    fn read_asana_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_asana_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape — the full-fidelity API task object as returned by the pull.

/// One raw API task object in `tasks/asana/raw/YYYY-MM.jsonl`.
/// Written verbatim (no synthetic keys added), partitioned by `created_at`,
/// upserted by `gid`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawTask {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawTask {
    /// The task GID (stable dedup key).
    fn guid(&self) -> String {
        self.fields
            .get("gid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// `created_at` — partition key for the monthly raw files.
    fn created_at(&self) -> &str {
        self.fields
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// Build a `RawTask` from an API value. `None` when GID or `created_at` is
/// absent (can't dedup or partition).
fn raw_task(value: &Value) -> Option<RawTask> {
    let obj = value.as_object()?;
    obj.get("gid").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    obj.get("created_at").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    Some(RawTask { fields: obj.clone() })
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// An RFC3339 timestamp → RFC3339 local time. Unparseable values pass through
/// verbatim (the todoist/github pattern).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Pull a string field, trimmed; `None` when missing/non-string/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract a `name` field from an Asana compact object
/// (`{ "gid": "…", "name": "…" }`).
fn compact_name(v: &Value) -> Option<String> {
    v.as_object()
        .and_then(|o| o.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Map a raw Asana task API object → normalized [`Task`].
/// Returns `None` when the object has no GID or no name.
fn task_from_value(value: &Value) -> Option<Task> {
    let obj = value.as_object()?;
    let id = obj.get("gid").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let title = str_opt(value, "name")?;

    // Project name: take the first entry from `projects[]` (most tasks belong
    // to exactly one project; any extras ride in `extra.projects`).
    let project = obj
        .get("projects")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(compact_name)
        .unwrap_or_default();

    // Tags: collect `name` from each `tags[].name`.
    let tags: Vec<String> = obj
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(compact_name).collect())
        .unwrap_or_default();

    // Due: prefer `due_at` (datetime with time-of-day) over `due_on` (date-only).
    let due_at = str_opt(value, "due_at");
    let due_on = str_opt(value, "due_on");
    let all_day = due_at.is_none() && due_on.is_some();
    let due = due_at
        .map(|s| to_local(&s))
        .or_else(|| due_on.map(|s| to_local(&s)));

    // Start: prefer `start_at` (datetime) over `start_on` (date-only), parallel
    // to the due_at/due_on pattern (Asana OpenAPI TaskBase: start_at, start_on).
    let start = str_opt(value, "start_at")
        .map(|s| to_local(&s))
        .or_else(|| str_opt(value, "start_on").map(|s| to_local(&s)));

    let completed_bool = obj.get("completed").and_then(Value::as_bool).unwrap_or(false);
    let completed_at = str_opt(value, "completed_at").map(|s| to_local(&s));

    // Source-specific extras.
    let mut extra = Map::new();

    if let Some(memberships) = obj.get("memberships").and_then(Value::as_array) {
        if !memberships.is_empty() {
            extra.insert("memberships".into(), Value::Array(memberships.clone()));
        }
    }
    if let Some(followers) = obj.get("followers").and_then(Value::as_array) {
        if !followers.is_empty() {
            extra.insert("followers".into(), Value::Array(followers.clone()));
        }
    }
    if let Some(assignee) = obj.get("assignee").and_then(compact_name) {
        extra.insert("assignee".into(), Value::String(assignee));
    }
    if let Some(url) = str_opt(value, "permalink_url") {
        extra.insert("permalink_url".into(), Value::String(url));
    }
    // All projects (full compact list) for callers that want every membership.
    if let Some(projects) = obj.get("projects").and_then(Value::as_array) {
        if projects.len() > 1 {
            extra.insert("projects".into(), Value::Array(projects.clone()));
        }
    }
    // Workspace GID stored in extra so the per-workspace fate guard can
    // associate previously-seen tasks with the workspace that fetched them.
    if let Some(ws_gid) = obj
        .get("workspace")
        .and_then(Value::as_object)
        .and_then(|o| o.get("gid"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        extra.insert("workspace_gid".into(), Value::String(ws_gid.to_string()));
    }

    Some(Task {
        source: SOURCE.into(),
        id,
        title,
        project,
        notes: str_opt(value, "notes").unwrap_or_default(),
        status: if completed_bool { "done".into() } else { "open".into() },
        priority: 0, // Asana has no numeric priority in the free/REST API
        due,
        start,
        all_day,
        recurrence: None,
        tags,
        subtasks: Vec::new(),
        created: str_opt(value, "created_at").map(|s| to_local(&s)),
        modified: str_opt(value, "modified_at").map(|s| to_local(&s)),
        completed: completed_at,
        extra,
    })
}

/// One `/workspaces` item → [`ProjectInfo`]. `None` without both GID and name.
fn workspace_info(value: &Value) -> Option<ProjectInfo> {
    let obj = value.as_object()?;
    Some(ProjectInfo {
        id: obj.get("gid").and_then(Value::as_str)?.to_string(),
        name: str_opt(value, "name")?,
    })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition (the todoist/github idiom).

fn upsert_raw(vault: &Vault, rows: Vec<RawTask>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawTask>> = BTreeMap::new();
    for r in rows {
        let created = r.created_at().to_string();
        let key = Partition::Month
            .key(&created)
            .with_context(|| format!("asana: raw task created_at {created:?} has no month"))?
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
            a.created_at().cmp(b.created_at()).then_with(|| a.guid().cmp(&b.guid()))
        });
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync. Missing token ⇒ quiet skip on the periodic
/// path (mirror todoist/github), clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context(
            "Asana is not connected — add your Personal Access Token in the Integrations tab",
        )?;
    let client = AsanaClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client, Local::now())
}

/// The pull body over an injected API + clock — the testable seam.
fn pull_with(vault: &Vault, api: &impl AsanaApi, now: DateTime<Local>) -> Result<PullOutcome> {
    let mut state = vault.read_asana_sync();

    // `completed_since` lower bound: 90-day lookback on first sync, then from
    // last successful sync so completions between syncs are never missed.
    let completed_since = state
        .last_sync
        .as_deref()
        .map(to_utc_for_query)
        .unwrap_or_else(|| {
            (now - chrono::Duration::days(COMPLETED_LOOKBACK_DAYS))
                .with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        });

    // Enumerate all workspaces visible to this token.
    let workspace_items =
        drain_all(api, "/workspaces", &[("opt_fields", "gid,name")]).map_err(fetch_err)?;
    let workspaces: Vec<ProjectInfo> = workspace_items.iter().filter_map(workspace_info).collect();

    let mut all_raw: Vec<RawTask> = Vec::new();
    let mut all_fresh: Vec<Task> = Vec::new();
    // GID → completed_at (local RFC3339) for tasks returned as completed.
    let mut completed_map: HashMap<String, Option<String>> = HashMap::new();
    // Workspace GIDs for which we saw ≥1 task (open or completed) this cycle.
    // A workspace absent from this set returned 200+[] — we cannot safely
    // distinguish "all tasks removed" from a transient empty/partial response,
    // so we carry its previous tasks forward as Unknown instead of Deleted.
    let mut non_empty_workspaces: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for ws in &workspaces {
        // `completed_since` makes the API return BOTH open tasks AND tasks
        // completed since that date in one call — no separate completed fetch.
        let task_params = [
            ("assignee", "me"),
            ("workspace", ws.id.as_str()),
            ("completed_since", completed_since.as_str()),
            ("opt_fields", TASK_OPT_FIELDS),
        ];
        let task_items = drain_all(api, "/tasks", &task_params).map_err(fetch_err)?;

        if !task_items.is_empty() {
            non_empty_workspaces.insert(ws.id.clone());
        }

        for v in &task_items {
            if let Some(r) = raw_task(v) {
                all_raw.push(r);
            }
            let obj = match v.as_object() {
                Some(o) => o,
                None => continue,
            };
            let gid = match obj.get("gid").and_then(Value::as_str) {
                Some(g) if !g.is_empty() => g.to_string(),
                _ => continue,
            };
            let is_completed =
                obj.get("completed").and_then(Value::as_bool).unwrap_or(false);
            if is_completed {
                // Collect into the fate map; NOT into the open snapshot.
                let completed_at = str_opt(v, "completed_at").map(|s| to_local(&s));
                completed_map.insert(gid, completed_at);
            } else if let Some(t) = task_from_value(v) {
                all_fresh.push(t);
            }
        }
    }

    let raw_new = upsert_raw(vault, all_raw)?;

    // Workspaces serve as the project-level containers for the markdown index.
    let stats = vault
        .apply_tasks_sync(SOURCE, &workspaces, all_fresh, |t| {
            asana_fate(t, &completed_map, &non_empty_workspaces)
        })
        .context("asana: applying task sync")?;

    state.last_sync = Some(now.to_rfc3339());
    vault.write_asana_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open Asana tasks", stats.open),
        counts,
    })
}

/// Resolve a task that vanished from the open set.
///
/// - In `completed` → `Completed(its time)` (or `None` if missing).
/// - Not in `completed`, but its workspace returned ≥1 task this cycle →
///   `Deleted` (it was removed from the workspace or reassigned).
/// - Not in `completed`, and its workspace returned nothing this cycle →
///   `Unknown` (carry-forward): Asana returns 200+`[]` for workspaces with
///   no currently-assigned tasks; we cannot distinguish "legitimately empty"
///   from a transient partial response, so we never guess a deletion off an
///   empty picture (the todoist `None`-guard precedent).
///
/// The workspace association uses `extra["workspace_gid"]` stored when the
/// task was first parsed; tasks without this key are assumed to belong to an
/// active workspace (treated as Deleted on vanish).
fn asana_fate(
    task: &Task,
    completed: &HashMap<String, Option<String>>,
    non_empty_workspaces: &std::collections::HashSet<String>,
) -> TaskFate {
    match completed.get(&task.id) {
        Some(Some(when)) => TaskFate::Completed(Some(when.clone())),
        Some(None) => TaskFate::Completed(None),
        None => {
            // Check whether the workspace that owns this task returned any
            // tasks at all this cycle. If it returned nothing, carry forward.
            let ws_gid = task
                .extra
                .get("workspace_gid")
                .and_then(Value::as_str)
                .unwrap_or("");
            if !ws_gid.is_empty() && !non_empty_workspaces.contains(ws_gid) {
                TaskFate::Unknown
            } else {
                TaskFate::Deleted
            }
        }
    }
}

fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Asana rejected the token (401) — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Asana fetch failed: {other}"),
    }
}

/// RFC3339-local cursor → the UTC form the `completed_since` query wants.
fn to_utc_for_query(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
        .unwrap_or_else(|_| format!("{}Z", s.chars().take(19).collect::<String>()))
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-asana-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-06-14T12:00:00-07:00")
            .unwrap()
            .with_timezone(&Local)
    }

    // --- fixtures (Asana v1.0 OpenAPI-confirmed field names) --------------

    fn workspace_json(gid: &str, name: &str) -> Value {
        serde_json::json!({
            "gid": gid,
            "name": name,
            "resource_type": "workspace"
        })
    }

    /// An open task with a timed due date, tag, project membership, followers.
    fn task_open(gid: &str, ws_gid: &str, project_gid: &str) -> Value {
        serde_json::json!({
            "gid": gid,
            "resource_type": "task",
            "name": "Ship the release",
            "notes": "Cut the tag and push to production.",
            "completed": false,
            "completed_at": null,
            "created_at": "2026-06-01T08:00:00.000Z",
            "modified_at": "2026-06-10T09:00:00.000Z",
            "due_on": null,
            "due_at": "2026-06-15T17:00:00.000Z",
            "assignee": { "gid": "u1", "name": "Alice", "resource_type": "user" },
            "projects": [{ "gid": project_gid, "name": "Releases", "resource_type": "project" }],
            "tags": [{ "gid": "t1", "name": "urgent", "resource_type": "tag" }],
            "followers": [{ "gid": "u2", "name": "Bob", "resource_type": "user" }],
            "memberships": [{
                "project": { "gid": project_gid, "name": "Releases" },
                "section": { "gid": "s1", "name": "This week" }
            }],
            "workspace": { "gid": ws_gid, "name": "Acme", "resource_type": "workspace" },
            "permalink_url": "https://app.asana.com/0/12345/67890"
        })
    }

    /// A task with date-only due (all_day = true).
    fn task_all_day(gid: &str, ws_gid: &str) -> Value {
        serde_json::json!({
            "gid": gid,
            "resource_type": "task",
            "name": "Pay rent",
            "notes": "",
            "completed": false,
            "completed_at": null,
            "created_at": "2026-06-02T10:00:00.000Z",
            "modified_at": "2026-06-02T10:00:00.000Z",
            "due_on": "2026-06-20",
            "due_at": null,
            "assignee": null,
            "projects": [],
            "tags": [],
            "followers": [],
            "memberships": [],
            "workspace": { "gid": ws_gid, "name": "Personal" },
            "permalink_url": "https://app.asana.com/0/12345/99999"
        })
    }

    /// A completed task (returned within a `completed_since` window).
    fn task_completed(gid: &str, ws_gid: &str, completed_at: &str) -> Value {
        serde_json::json!({
            "gid": gid,
            "resource_type": "task",
            "name": "Write docs",
            "notes": "",
            "completed": true,
            "completed_at": completed_at,
            "created_at": "2026-06-01T07:00:00.000Z",
            "modified_at": "2026-06-13T16:30:00.000Z",
            "due_on": null,
            "due_at": null,
            "assignee": null,
            "projects": [],
            "tags": [],
            "followers": [],
            "memberships": [],
            "workspace": { "gid": ws_gid, "name": "Acme" },
            "permalink_url": "https://app.asana.com/0/12345/11111"
        })
    }

    // --- mock API ---------------------------------------------------------
    //
    // The queue key for /tasks is "workspace=<gid>" so that different
    // workspaces can return different pages. This mirrors the real per-workspace
    // behaviour that the blocking multi-workspace fate bug lives in.
    // All other paths match by path prefix as before.

    struct MockApi {
        /// Queue entries: (match-key, pages). For /tasks calls the key is
        /// "workspace=<gid>"; for other paths it's the path itself.
        pages: RefCell<Vec<(String, VecDeque<Page>)>>,
        unauthorized: RefCell<bool>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                unauthorized: RefCell::new(false),
            }
        }

        fn page(&self, key: &str, items: Vec<Value>) {
            self.pages.borrow_mut().push((
                key.into(),
                VecDeque::from(vec![Page { items, next_offset: None }]),
            ));
        }

        fn pages_seq(&self, key: &str, seq: Vec<Page>) {
            self.pages.borrow_mut().push((key.into(), VecDeque::from(seq)));
        }

        /// Register tasks for a specific workspace GID.
        fn tasks_for_workspace(&self, ws_gid: &str, items: Vec<Value>) {
            self.page(&format!("workspace={ws_gid}"), items);
        }
    }

    impl AsanaApi for MockApi {
        fn get_list(
            &self,
            path: &str,
            params: &[(&str, &str)],
            _offset: Option<&str>,
        ) -> Result<Page, FetchError> {
            if *self.unauthorized.borrow() {
                return Err(FetchError::Unauthorized);
            }
            // Build the workspace-scoped key for /tasks calls so different
            // workspaces can serve different queues.
            let ws_key: Option<String> = if path == "/tasks" {
                params
                    .iter()
                    .find(|(k, _)| *k == "workspace")
                    .map(|(_, v)| format!("workspace={v}"))
            } else {
                None
            };

            let mut pages = self.pages.borrow_mut();
            // Try the workspace-scoped key first (exact match for /tasks).
            if let Some(ref wk) = ws_key {
                for (key, queue) in pages.iter_mut() {
                    if key == wk {
                        if let Some(p) = queue.pop_front() {
                            return Ok(p);
                        }
                        // Queue exhausted for this workspace → empty result.
                        return Ok(Page { items: Vec::new(), next_offset: None });
                    }
                }
            }
            // Fall back to path-prefix matching for non-task endpoints.
            for (key, queue) in pages.iter_mut() {
                if path.starts_with(key.as_str()) || path.contains(key.as_str()) {
                    if let Some(p) = queue.pop_front() {
                        return Ok(p);
                    }
                }
            }
            Ok(Page { items: Vec::new(), next_offset: None })
        }
    }

    fn base_api(ws_items: Vec<Value>, task_items: Vec<Value>) -> MockApi {
        // For a single-workspace scenario the first workspace GID drives the
        // lookup key; extract it from the first workspace item.
        let api = MockApi::new();
        api.page("/workspaces", ws_items.clone());
        let ws_gid = ws_items
            .first()
            .and_then(|v| v.get("gid"))
            .and_then(Value::as_str)
            .unwrap_or("ws1");
        api.tasks_for_workspace(ws_gid, task_items);
        api
    }

    // --- parse_page -------------------------------------------------------

    #[test]
    fn parse_page_reads_data_and_next_page_offset() {
        let with_next = parse_page(serde_json::json!({
            "data": [1, 2],
            "next_page": { "offset": "PAGE2", "path": "/tasks?offset=PAGE2", "uri": "…" }
        }));
        assert_eq!(with_next.items.len(), 2);
        assert_eq!(with_next.next_offset.as_deref(), Some("PAGE2"));

        let last_page =
            parse_page(serde_json::json!({ "data": [3], "next_page": null }));
        assert_eq!(last_page.items.len(), 1);
        assert!(last_page.next_offset.is_none());

        let bare = parse_page(serde_json::json!([1, 2, 3]));
        assert_eq!(bare.items.len(), 3);
    }

    // --- pagination drain -------------------------------------------------

    #[test]
    fn drains_multiple_pages_via_next_offset() {
        let api = MockApi::new();
        api.page("/workspaces", vec![workspace_json("ws1", "Acme")]);
        // Key by workspace GID so the per-workspace dispatch finds the queue.
        api.pages_seq(
            "workspace=ws1",
            vec![
                Page {
                    items: vec![task_open("t1", "ws1", "p1")],
                    next_offset: Some("OFF2".into()),
                },
                Page { items: vec![task_all_day("t2", "ws1")], next_offset: None },
            ],
        );
        let v = temp_vault("drain");
        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("open"), Some(&2));
    }

    // --- task mapping tests -----------------------------------------------

    #[test]
    fn maps_open_task_fields_correctly() {
        let t = task_from_value(&task_open("T1", "ws1", "p1")).unwrap();
        assert_eq!(t.source, SOURCE);
        assert_eq!(t.id, "T1");
        assert_eq!(t.title, "Ship the release");
        assert_eq!(t.project, "Releases");
        assert_eq!(t.notes, "Cut the tag and push to production.");
        assert_eq!(t.status, "open");
        assert_eq!(t.priority, 0, "Asana has no free-API numeric priority");
        assert_eq!(t.tags, vec!["urgent"]);
        assert!(!t.all_day, "due_at → timed, not all-day");
        // Due instant preserved across timezone conversion.
        let due_ts = DateTime::parse_from_rfc3339(t.due.as_deref().unwrap()).unwrap().timestamp();
        let expected =
            DateTime::parse_from_rfc3339("2026-06-15T17:00:00.000Z").unwrap().timestamp();
        assert_eq!(due_ts, expected);
        // Extra fields.
        assert_eq!(t.extra.get("assignee").and_then(Value::as_str), Some("Alice"));
        assert!(t.extra.contains_key("permalink_url"));
        assert!(t.extra.contains_key("memberships"));
        assert!(t.extra.contains_key("followers"));
        assert!(t.created.is_some());
    }

    #[test]
    fn maps_all_day_task() {
        let t = task_from_value(&task_all_day("T2", "ws1")).unwrap();
        assert!(t.all_day, "date-only due → all_day");
        assert_eq!(t.project, "", "no projects → empty string");
        assert!(t.tags.is_empty());
        let due = t.due.as_deref().unwrap();
        // "2026-06-20" (date-only) → local RFC3339 that starts with that date.
        assert!(due.starts_with("2026-06-2"), "date survives conversion: {due}");
    }

    #[test]
    fn completed_task_has_done_status_and_completed_at() {
        let t =
            task_from_value(&task_completed("T3", "ws1", "2026-06-13T16:30:00.000Z")).unwrap();
        assert_eq!(t.status, "done");
        let c_ts =
            DateTime::parse_from_rfc3339(t.completed.as_deref().unwrap()).unwrap().timestamp();
        let expected =
            DateTime::parse_from_rfc3339("2026-06-13T16:30:00.000Z").unwrap().timestamp();
        assert_eq!(c_ts, expected);
    }

    #[test]
    fn workspace_info_extracts_gid_and_name() {
        let w = workspace_info(&workspace_json("ws1", "Acme")).unwrap();
        assert_eq!(w.id, "ws1");
        assert_eq!(w.name, "Acme");
    }

    // --- raw upsert -------------------------------------------------------

    #[test]
    fn raw_task_roundtrips_full_fidelity() {
        let r = raw_task(&task_open("T1", "ws1", "p1")).unwrap();
        assert_eq!(r.guid(), "T1");
        assert_eq!(r.created_at(), "2026-06-01T08:00:00.000Z");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("\"guid\""), "no synthetic guid column");
        let back: RawTask = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
        assert!(line.contains("\"followers\""), "raw keeps all source fields");
    }

    #[test]
    fn resync_dedupes_raw_by_gid() {
        let v = temp_vault("rawdedup");
        let api1 = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_open("T1", "ws1", "p1")],
        );
        pull_with(&v, &api1, now()).unwrap();

        let api2 = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_open("T1", "ws1", "p1")],
        );
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("raw"), Some(&0), "same GID not duplicated");
        let raw =
            std::fs::read_to_string(v.root().join("tasks/asana/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "one raw line, no duplicate");
    }

    // --- full pull --------------------------------------------------------

    #[test]
    fn full_pull_writes_contract_raw_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_open("T1", "ws1", "p1"), task_all_day("T2", "ws1")],
        );
        let out = pull_with(&v, &api, now()).unwrap();

        assert_eq!(out.counts.get("open"), Some(&2));
        assert_eq!(out.counts.get("created"), Some(&2), "first sync: 2 creations");
        assert_eq!(out.counts.get("raw"), Some(&2));

        // Contract snapshot.
        let snap = v.load_tasks_snapshot(SOURCE).unwrap();
        assert_eq!(snap.len(), 2);
        let t1 = snap.iter().find(|t| t.id == "T1").unwrap();
        assert_eq!(t1.project, "Releases");
        assert_eq!(t1.source, SOURCE);

        // Raw file partitioned by created_at month.
        assert!(v.root().join("tasks/asana/raw/2026-06.jsonl").exists());
        let raw =
            std::fs::read_to_string(v.root().join("tasks/asana/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"followers\""), "raw keeps all source fields");
        assert!(raw.contains("Ship the release"));

        // Cursor advanced; PAT never in cursor file.
        let st2 = v.read_asana_sync();
        assert!(st2.last_sync.is_some());
        let cursor =
            std::fs::read_to_string(v.root().join(".trove/asana-sync.json")).unwrap();
        assert!(!cursor.contains("access_token"), "PAT never in cursor");
    }

    // --- fate / completion / deletion tests --------------------------------

    #[test]
    fn completed_task_yields_completion_event_at_correct_time() {
        let v = temp_vault("fate-complete");
        // Sync 1: T1 is open.
        let api1 = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_open("T1", "ws1", "p1")],
        );
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: T1 returned as completed (with completed_at set).
        let api2 = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_completed("T1", "ws1", "2026-06-13T16:30:00.000Z")],
        );
        let out2 = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out2.counts.get("completed"), Some(&1));

        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completions: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].task.id, "T1");
        let ev_ts =
            DateTime::parse_from_rfc3339(&completions[0].time).unwrap().timestamp();
        let expected =
            DateTime::parse_from_rfc3339("2026-06-13T16:30:00.000Z").unwrap().timestamp();
        assert_eq!(ev_ts, expected);
        assert!(v.root().join("tasks/asana/events/2026-06.jsonl").exists());
    }

    #[test]
    fn task_absent_from_completed_window_is_deleted() {
        let v = temp_vault("fate-delete");
        let api1 = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_open("T1", "ws1", "p1")],
        );
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: workspace ws1 returns T2 (non-empty) but NOT T1, and T1 is
        // not in the completed window either → T1 is Deleted.
        // (A non-empty workspace response is required to distinguish deletion
        // from transient-empty — the multi-workspace guard only carries forward
        // tasks from workspaces that returned nothing at all this cycle.)
        let api2 = base_api(
            vec![workspace_json("ws1", "Acme")],
            vec![task_all_day("T2", "ws1")],
        );
        let out2 = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out2.counts.get("deleted"), Some(&1));

        // Deletions are stamped at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "T1"));
        assert!(!events.iter().any(|e| e.kind == "completed" && e.task.id == "T1"));
    }

    #[test]
    fn asana_fate_resolution_matrix() {
        let mut map: HashMap<String, Option<String>> = HashMap::new();
        map.insert("done".into(), Some("2026-06-13T16:30:00-07:00".into()));
        map.insert("done_no_ts".into(), None);

        let mut non_empty: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        non_empty.insert("ws1".to_string());

        // Task with a workspace_gid known to have returned tasks.
        let task_ws = |id: &str| {
            let mut extra = Map::new();
            extra.insert("workspace_gid".into(), Value::String("ws1".into()));
            Task {
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
                extra,
            }
        };

        // Task with no workspace_gid (treated as "active workspace" → Deleted).
        let task_no_ws = |id: &str| Task {
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

        match asana_fate(&task_ws("done"), &map, &non_empty) {
            TaskFate::Completed(Some(w)) => assert!(w.starts_with("2026-06-13")),
            _ => panic!("expected Completed(time)"),
        }
        assert!(matches!(
            asana_fate(&task_ws("done_no_ts"), &map, &non_empty),
            TaskFate::Completed(None)
        ));
        // "ghost" is in a ws1 workspace that returned tasks → Deleted.
        assert!(matches!(asana_fate(&task_ws("ghost"), &map, &non_empty), TaskFate::Deleted));
        // Task without workspace_gid in extra → Deleted (unknown workspace assumed active).
        assert!(matches!(asana_fate(&task_no_ws("ghost2"), &map, &non_empty), TaskFate::Deleted));

        // Task from workspace that returned nothing → Unknown (carry-forward).
        let empty_non_empty: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut extra_ws2 = Map::new();
        extra_ws2.insert("workspace_gid".into(), Value::String("ws2".into()));
        let task_ws2 = Task {
            source: SOURCE.into(),
            id: "ws2task".into(),
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
            extra: extra_ws2,
        };
        assert!(matches!(
            asana_fate(&task_ws2, &map, &empty_non_empty),
            TaskFate::Unknown
        ), "empty workspace must carry-forward, not delete");
    }

    // --- multi-workspace fate guard (blocking regression) -----------------

    /// WS-A returns tasks; WS-B returns [] this cycle.
    /// Prior WS-B tasks must NOT be deleted (carry-forward as Unknown).
    #[test]
    fn empty_workspace_does_not_delete_prior_tasks() {
        let v = temp_vault("mws-empty-guard");

        // Sync 1: both workspaces have tasks.
        let api1 = MockApi::new();
        api1.page(
            "/workspaces",
            vec![workspace_json("ws_a", "Acme"), workspace_json("ws_b", "Personal")],
        );
        api1.tasks_for_workspace("ws_a", vec![task_open("TA1", "ws_a", "pa1")]);
        api1.tasks_for_workspace("ws_b", vec![task_open("TB1", "ws_b", "pb1")]);
        let out1 = pull_with(&v, &api1, now()).unwrap();
        assert_eq!(out1.counts.get("open"), Some(&2));
        assert_eq!(out1.counts.get("created"), Some(&2));

        // Sync 2: WS-A returns a task; WS-B returns nothing (200+[]).
        let api2 = MockApi::new();
        api2.page(
            "/workspaces",
            vec![workspace_json("ws_a", "Acme"), workspace_json("ws_b", "Personal")],
        );
        api2.tasks_for_workspace("ws_a", vec![task_open("TA1", "ws_a", "pa1")]);
        // WS-B intentionally has no tasks_for_workspace registration → returns [].
        let out2 = pull_with(&v, &api2, now()).unwrap();

        // TB1 must NOT be deleted: WS-B returned empty, so we can't know if
        // it was removed or just a transient empty response.
        assert_eq!(
            out2.counts.get("deleted").copied().unwrap_or(0),
            0,
            "WS-B prior task must carry forward (Unknown), not be deleted"
        );
        // TA1 still open → 2 open total (TA1 fresh + TB1 carried forward).
        let snap = v.load_tasks_snapshot(SOURCE).unwrap();
        assert!(snap.iter().any(|t| t.id == "TB1"), "TB1 must survive in snapshot");
        assert!(snap.iter().any(|t| t.id == "TA1"), "TA1 still open");

        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        assert!(
            !events.iter().any(|e| e.kind == "deleted" && e.task.id == "TB1"),
            "no false deletion event for TB1"
        );
    }

    /// WS-A returns tasks and task T-gone is absent AND not completed → Deleted.
    /// WS-B returns [] → its prior tasks carry forward (Unknown).
    /// Verifies both behaviours in one pull.
    #[test]
    fn multi_workspace_deletion_only_from_non_empty_workspace() {
        let v = temp_vault("mws-mixed");

        // Sync 1: WS-A has T-gone + T-stay; WS-B has TB1.
        let api1 = MockApi::new();
        api1.page(
            "/workspaces",
            vec![workspace_json("ws_a", "Acme"), workspace_json("ws_b", "Personal")],
        );
        api1.tasks_for_workspace(
            "ws_a",
            vec![task_open("T-gone", "ws_a", "pa1"), task_open("T-stay", "ws_a", "pa1")],
        );
        api1.tasks_for_workspace("ws_b", vec![task_open("TB1", "ws_b", "pb1")]);
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: WS-A returns only T-stay (T-gone removed); WS-B returns [].
        let api2 = MockApi::new();
        api2.page(
            "/workspaces",
            vec![workspace_json("ws_a", "Acme"), workspace_json("ws_b", "Personal")],
        );
        api2.tasks_for_workspace("ws_a", vec![task_open("T-stay", "ws_a", "pa1")]);
        // WS-B: no registration → [].
        let out2 = pull_with(&v, &api2, now()).unwrap();

        // T-gone is from a non-empty workspace (ws_a) and vanished → Deleted.
        assert_eq!(out2.counts.get("deleted").copied().unwrap_or(0), 1);
        // Deletions are stamped at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "T-gone"));
        // TB1 from the empty WS-B must not be deleted → still in snapshot.
        let snap = v.load_tasks_snapshot(SOURCE).unwrap();
        assert!(snap.iter().any(|t| t.id == "TB1"), "TB1 carried forward");
        assert!(!events.iter().any(|e| e.kind == "deleted" && e.task.id == "TB1"));
    }

    // --- start_on / start_at mapping (major defect) -----------------------

    /// A task with a timed start_at should populate Task.start.
    #[test]
    fn maps_start_at_to_task_start() {
        let task_with_start = serde_json::json!({
            "gid": "S1",
            "resource_type": "task",
            "name": "Sprint planning",
            "notes": "",
            "completed": false,
            "completed_at": null,
            "created_at": "2026-06-01T08:00:00.000Z",
            "modified_at": "2026-06-01T08:00:00.000Z",
            "due_on": "2026-06-20",
            "due_at": "2026-06-20T17:00:00.000Z",
            "start_on": null,
            "start_at": "2026-06-16T09:00:00.000Z",
            "assignee": null,
            "projects": [],
            "tags": [],
            "followers": [],
            "memberships": [],
            "workspace": { "gid": "ws1", "name": "Acme" },
            "permalink_url": "https://app.asana.com/0/1/2"
        });
        let t = task_from_value(&task_with_start).unwrap();
        let start = t.start.as_deref().expect("start_at must map to Task.start");
        let start_ts = DateTime::parse_from_rfc3339(start).unwrap().timestamp();
        let expected =
            DateTime::parse_from_rfc3339("2026-06-16T09:00:00.000Z").unwrap().timestamp();
        assert_eq!(start_ts, expected, "start_at preserved across timezone conversion");
    }

    /// A task with only start_on (date, no time) should populate Task.start
    /// from the date string.
    #[test]
    fn maps_start_on_to_task_start_when_no_start_at() {
        let task_date_start = serde_json::json!({
            "gid": "S2",
            "resource_type": "task",
            "name": "Design sprint",
            "notes": "",
            "completed": false,
            "completed_at": null,
            "created_at": "2026-06-01T08:00:00.000Z",
            "modified_at": "2026-06-01T08:00:00.000Z",
            "due_on": "2026-06-25",
            "due_at": null,
            "start_on": "2026-06-18",
            "start_at": null,
            "assignee": null,
            "projects": [],
            "tags": [],
            "followers": [],
            "memberships": [],
            "workspace": { "gid": "ws1", "name": "Acme" },
            "permalink_url": "https://app.asana.com/0/1/3"
        });
        let t = task_from_value(&task_date_start).unwrap();
        let start = t.start.as_deref().expect("start_on must map to Task.start");
        assert!(start.starts_with("2026-06-18"), "start_on date preserved: {start}");
        assert!(t.all_day, "no due_at → all_day");
    }

    /// A task with neither start_on nor start_at: Task.start stays None.
    #[test]
    fn task_without_start_fields_has_none_start() {
        let t = task_from_value(&task_open("T-nostart", "ws1", "p1")).unwrap();
        assert!(t.start.is_none(), "no start fields → Task.start is None");
    }

    // --- workspace_gid stored in extra ------------------------------------

    #[test]
    fn task_from_value_stores_workspace_gid_in_extra() {
        let t = task_from_value(&task_open("T1", "ws_xyz", "p1")).unwrap();
        assert_eq!(
            t.extra.get("workspace_gid").and_then(Value::as_str),
            Some("ws_xyz"),
            "workspace GID must be stored in extra for fate guard"
        );
    }

    // --- connection tests -------------------------------------------------

    #[test]
    fn connection_stores_and_retrieves_token() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "1/fake_asana_pat_token".into(),
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
        assert_eq!(status.accounts[0].label, "Asana");
        assert_eq!(status.accounts[0].key, SERVICE);

        // PAT not in cursor file.
        v.write_asana_sync(&SyncState { last_sync: Some(now().to_rfc3339()) }).unwrap();
        let cursor =
            std::fs::read_to_string(v.root().join(".trove/asana-sync.json")).unwrap();
        assert!(!cursor.contains("fake_asana_pat"), "PAT never in cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("fake_asana_pat") {
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret file must be 0600");
                }
            }
        }

        def_disconnect(&v, SERVICE).unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn empty_token_is_rejected() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
    }

    #[test]
    fn pull_without_token_gives_clear_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_has_token_paste_method_and_correct_id() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "asana");
        assert_eq!(DEF.meta.id, "asana");
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_sync.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_sync":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert_eq!(partial.last_sync.as_deref(), Some("2026-06-01T00:00:00-07:00"));
        // Unknown future keys ignored (back-compat).
        let future: SyncState =
            serde_json::from_str(r#"{"last_sync":"2026-06-01T00:00:00-07:00","future_key":42}"#)
                .unwrap();
        assert!(future.last_sync.is_some());
    }
}
