//! Clockify — free cloud time-tracking service with a full public REST API.
//! A **Periodic** pull of the user's time entries into the bound
//! [`crate::time_entries`] contract (same domain as Toggl Track; both write
//! `time-entries/<source>/YYYY-MM.jsonl`). Catalogued in the Phase 2 pass;
//! brief: docs/integrations/clockify.md.
//!
//! ## Identity resolution
//!
//! The collector first calls `GET /v1/user` to resolve the current user's `id`
//! and `activeWorkspace` — the two IDs needed to address the time-entries
//! endpoint. They are persisted in the non-secret cursor
//! (`.trove/clockify-sync.json`, rebuildable) so subsequent syncs skip the
//! resolution call when IDs are already known.
//!
//! ## Time entries endpoint & pagination
//!
//! `GET /v1/workspaces/{workspaceId}/user/{userId}/time-entries`
//!
//! Clockify does NOT return a `since`-style incremental cursor. Instead we
//! page forward using page/page-size until a short page signals the end.
//! On the **first sync** we drain all pages (no date bound). On **subsequent
//! syncs** we pass `start=<last_start_utc>` to bound the window. Because we
//! advance the cursor to the latest `start` seen *after* a full drain, a
//! re-poll of the boundary page is harmless — the `id` dedupe absorbs it.
//!
//! Running timers have `timeInterval.end` and `timeInterval.duration` set to
//! `null`; those fields are omitted from the contract row.
//!
//! ## Two vault layers
//!
//! - **Raw**: `time-entries/clockify/raw/YYYY-MM.jsonl` — verbatim API objects,
//!   deduped by (`id`, `updatedAt`) so distinct states survive (running ->
//!   stopped transition preserved) while identical re-polls are idempotent.
//! - **Contract**: `time-entries/clockify/YYYY-MM.jsonl` — one
//!   [`crate::time_entries::TimeEntry`] per entry; append-only, first-observed-
//!   id wins (same convention as Toggl Track).
//!
//! ## Auth
//!
//! Personal API key (Profile Settings -> API) pasted via `TokenPaste`,
//! stored 0600 under `.trove/sync/`. Passed as the `X-Api-Key` request header.

use std::collections::{BTreeMap, HashMap, HashSet};
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
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::time_entries::TimeEntry;
use crate::vault::Vault;

const DIR: &str = "time-entries/clockify";
const RAW_DIR: &str = "time-entries/clockify/raw";
const SYNC_FILE: &str = ".trove/clockify-sync.json";
const SERVICE: &str = "clockify";
const API_BASE: &str = "https://api.clockify.me/api/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hourly -- fine for a personal pull; the API limit is 10 req/s.
const SYNC_SECS: u64 = 3600;
const PAGE_SIZE: u32 = 50;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("clockify synced -- {} time entries", c("entries"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "clockify sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Clockify synced -- {} time entries", c("entries")),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "clockify",
        name: "Clockify",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your time entries from Clockify via the official v1 API \
                      using a personal API key (available on all plans including free). \
                      Syncs hourly; first sync pulls your full history, later syncs are incremental.",
        domain: "time-entries",
        vault_path: "time-entries/clockify/",
        toggleable: true,
        setup: &[
            "Connect with your Clockify API key on this card.",
            "First sync pulls your full time-entry history; later syncs are incremental.",
        ],
        caveats: "Workspace and user IDs are resolved automatically via the /user endpoint. \
                  Project and task names are resolved from the workspace project list (a deleted \
                  project leaves the entry's project blank). Tag names are resolved from the \
                  workspace tags list. Running timers are pulled without an end or duration; the \
                  final values land in the raw layer once the timer stops.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("clockify"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste -- the Clockify personal API key).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty API key -- copy it from Profile Settings > API in your Clockify account");
    }
    let client = ClockifyClient::new(API_BASE.to_string(), token.to_string());
    match client.get_user() {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Clockify rejected the API key (401) -- copy it fresh from Profile Settings > API"
        ),
        Err(e) => bail!("Clockify auth check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: None,
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
            label: "Clockify".to_string(),
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
    id: "clockify",
    display_name: "Clockify",
    methods: &[ConnectMethod::TokenPaste {
        label: "Clockify API key",
        help: "Paste your personal API key -- stored locally and used only to reach Clockify.",
        placeholder: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["clockify"],
    setup: &[
        "Sign in to Clockify and click your avatar > Profile Settings.",
        "Scroll to the API section at the bottom of the page.",
        "Click Generate, copy the API key, and paste it here.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP errors.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// API trait (injectable for offline tests).

trait ClockifyApi {
    /// `GET /v1/user` -> returns (userId, workspaceId).
    fn get_user(&self) -> Result<(String, String), FetchError>;
    /// `GET /v1/workspaces/{ws}/projects` -> all projects (paged).
    fn get_projects(&self, workspace_id: &str) -> Result<Vec<Value>, FetchError>;
    /// `GET /v1/workspaces/{ws}/tags` -> all tags (paged).
    fn get_tags(&self, workspace_id: &str) -> Result<Vec<Value>, FetchError>;
    /// `GET /v1/workspaces/{ws}/user/{user}/time-entries?page=N&page-size=S&start=ISO`
    /// Returns (entries, is_last_page). `start` is `None` on first sync.
    fn get_time_entries(
        &self,
        workspace_id: &str,
        user_id: &str,
        start: Option<&str>,
        page: u32,
    ) -> Result<(Vec<Value>, bool), FetchError>;
}

// ---------------------------------------------------------------------------
// Real HTTP client.

struct ClockifyClient {
    base: String,
    api_key: String,
}

impl ClockifyClient {
    fn new(base: String, api_key: String) -> Self {
        ClockifyClient { base, api_key }
    }

    fn get_json(&self, url: &str, query: &[(&str, String)]) -> Result<Value, FetchError> {
        let mut req = ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("X-Api-Key", &self.api_key)
            .set("Content-Type", "application/json");
        for (k, v) in query {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing JSON: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
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

    /// Paginate a list endpoint fully, returning all items.
    fn get_all_paged(
        &self,
        url: &str,
        extra_query: &[(&str, String)],
    ) -> Result<Vec<Value>, FetchError> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let mut query: Vec<(&str, String)> = vec![
                ("page", page.to_string()),
                ("page-size", PAGE_SIZE.to_string()),
            ];
            query.extend_from_slice(extra_query);
            let v = self.get_json(url, &query)?;
            let arr = match v {
                Value::Array(a) => a,
                _ => Vec::new(),
            };
            let len = arr.len();
            all.extend(arr);
            if len < PAGE_SIZE as usize {
                break; // short page = last page
            }
            page += 1;
        }
        Ok(all)
    }

    fn get_time_entries_page(
        &self,
        workspace_id: &str,
        user_id: &str,
        start: Option<&str>,
        page: u32,
    ) -> Result<(Vec<Value>, bool), FetchError> {
        let url = format!(
            "{}/workspaces/{}/user/{}/time-entries",
            self.base, workspace_id, user_id
        );
        let mut query: Vec<(&str, String)> = vec![
            ("page", page.to_string()),
            ("page-size", PAGE_SIZE.to_string()),
        ];
        if let Some(s) = start {
            query.push(("start", s.to_string()));
        }
        let v = self.get_json(&url, &query)?;
        let arr = match v {
            Value::Array(a) => a,
            _ => Vec::new(),
        };
        let is_last = arr.len() < PAGE_SIZE as usize;
        Ok((arr, is_last))
    }
}

impl ClockifyApi for ClockifyClient {
    fn get_user(&self) -> Result<(String, String), FetchError> {
        let url = format!("{}/user", self.base);
        let v = self.get_json(&url, &[])?;
        let user_id = v
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| FetchError::Other("missing id in /user response".into()))?
            .to_string();
        let ws_id = v
            .get("activeWorkspace")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| FetchError::Other("missing activeWorkspace in /user response".into()))?
            .to_string();
        Ok((user_id, ws_id))
    }

    fn get_projects(&self, workspace_id: &str) -> Result<Vec<Value>, FetchError> {
        let url = format!("{}/workspaces/{}/projects", self.base, workspace_id);
        self.get_all_paged(&url, &[])
    }

    fn get_tags(&self, workspace_id: &str) -> Result<Vec<Value>, FetchError> {
        let url = format!("{}/workspaces/{}/tags", self.base, workspace_id);
        self.get_all_paged(&url, &[])
    }

    fn get_time_entries(
        &self,
        workspace_id: &str,
        user_id: &str,
        start: Option<&str>,
        page: u32,
    ) -> Result<(Vec<Value>, bool), FetchError> {
        self.get_time_entries_page(workspace_id, user_id, start, page)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Resolved user id from `/v1/user` -- cached to skip the resolution call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    /// Resolved workspace id from `/v1/user` -- cached similarly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    /// The latest `timeInterval.start` (RFC3339 UTC) seen across the drain.
    /// Passed as the `start` query param on the next sync to bound the window.
    /// `None` on a first sync -> drain all pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_start: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_clockify_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_clockify_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row (verbatim API object).

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Helpers.

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// An RFC3339-ish timestamp -> RFC3339 local. Returns the input verbatim when
/// unparseable (don't silently drop bad data).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// An RFC3339-ish timestamp -> RFC3339 UTC string (for the cursor `last_start`).
/// Returns `None` when unparseable.
fn to_utc_str(s: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.to_utc().to_rfc3339())
}

/// Parse an ISO 8601 duration string (e.g. "PT1H30M" or "PT5400S") into
/// seconds. Returns `None` when the string is missing or unparseable.
/// Clockify emits the form PT{n}H{m}M{s}S with common sub-combinations.
fn parse_iso_duration(s: &str) -> Option<i64> {
    // Strip leading "PT" -- Clockify emits only time components (no date parts).
    let s = s.strip_prefix("PT")?;
    let mut secs: i64 = 0;
    let mut cur = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            cur.push(ch);
        } else {
            let n: i64 = cur.parse().ok()?;
            cur.clear();
            match ch {
                'H' => secs += n * 3600,
                'M' => secs += n * 60,
                'S' => secs += n,
                _ => return None, // unexpected designator -- bail gracefully
            }
        }
    }
    Some(secs)
}

/// One Clockify project object -> (id, name). `None` without both.
fn project_id_name(v: &Value) -> Option<(String, String)> {
    let id = str_field(v, "id");
    let name = str_field(v, "name");
    if id.is_empty() || name.is_empty() { None } else { Some((id, name)) }
}

/// One Clockify tag object -> (id, name). `None` without both.
fn tag_id_name(v: &Value) -> Option<(String, String)> {
    let id = str_field(v, "id");
    let name = str_field(v, "name");
    if id.is_empty() || name.is_empty() { None } else { Some((id, name)) }
}

/// The raw-layer dedupe key: (`id`, `updatedAt`).
/// `updatedAt` advances on mutation, so a running->stopped re-emit has a
/// different key and IS appended to raw; an unchanged re-poll collapses.
fn raw_key(v: &Value) -> Option<(String, String)> {
    let id = str_field(v, "id");
    if id.is_empty() { return None; }
    let updated = str_field(v, "updatedAt");
    Some((id, updated))
}

/// Map a Clockify time-entry object to a [`TimeEntry`].
///
/// The Clockify v1 response shape (confirmed from API docs):
/// ```json
/// {
///   "id": "657f1a9b2c3d4e5f6a7b8c9d",
///   "description": "Writing the collector",
///   "billable": true,
///   "projectId": "64f1a2b3c4d5e6f7a8b9c0d1",
///   "taskId": "64f1a2b3c4d5e6f7a8b9c0d2",
///   "tagIds": ["64f1a2b3c4d5e6f7a8b9c0d3"],
///   "workspaceId": "64f1a2b3c4d5e6f7a8b9c000",
///   "userId": "64f1a2b3c4d5e6f7a8b9c001",
///   "isLocked": false,
///   "updatedAt": "2026-06-10T18:00:05Z",
///   "timeInterval": {
///     "start": "2026-06-10T16:00:00Z",
///     "end": "2026-06-10T17:30:00Z",
///     "duration": "PT1H30M"
///   }
/// }
/// ```
/// A running timer has `timeInterval.end = null` and `timeInterval.duration = null`.
fn entry_from(
    e: &Value,
    project_names: &HashMap<String, String>,
    tag_names: &HashMap<String, String>,
) -> Option<TimeEntry> {
    let id = str_field(e, "id");
    if id.is_empty() { return None; }

    let interval = e.get("timeInterval")?;
    let raw_start = str_field(interval, "start");
    if raw_start.is_empty() { return None; }
    let start = to_local(&raw_start);
    // Must resolve to a month partition, otherwise we can't file it.
    Partition::Month.key(&start)?;

    // Running timer: end and duration are null.
    let raw_end = interval.get("end").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let end = if raw_end.is_empty() { String::new() } else { to_local(&raw_end) };

    let raw_dur = interval.get("duration").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let duration_secs = if raw_dur.is_empty() { None } else { parse_iso_duration(&raw_dur) };

    // Project name resolved from the workspace project list.
    let project_id = str_field(e, "projectId");
    let project = if project_id.is_empty() {
        String::new()
    } else {
        project_names.get(&project_id).cloned().unwrap_or_default()
    };

    // Tags: tagIds -> resolved names.
    let tag_ids: Vec<String> = e
        .get("tagIds")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let tags: Vec<String> = tag_ids
        .iter()
        .map(|tid| tag_names.get(tid).cloned().unwrap_or_default())
        .filter(|n| !n.is_empty())
        .collect();

    let billable = e.get("billable").and_then(Value::as_bool);

    // Source-specific fields -> extra (full fidelity beyond the contract).
    let mut extra = Map::new();
    let ws_id = str_field(e, "workspaceId");
    if !ws_id.is_empty() {
        extra.insert("workspaceId".into(), Value::String(ws_id));
    }
    let user_id_field = str_field(e, "userId");
    if !user_id_field.is_empty() {
        extra.insert("userId".into(), Value::String(user_id_field));
    }
    if !project_id.is_empty() {
        extra.insert("projectId".into(), Value::String(project_id));
    }
    let task_id = str_field(e, "taskId");
    if !task_id.is_empty() {
        extra.insert("taskId".into(), Value::String(task_id));
    }
    if !tag_ids.is_empty() {
        extra.insert(
            "tagIds".into(),
            Value::Array(tag_ids.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if let Some(is_locked) = e.get("isLocked").and_then(Value::as_bool) {
        extra.insert("isLocked".into(), Value::Bool(is_locked));
    }
    let updated_at = str_field(e, "updatedAt");
    if !updated_at.is_empty() {
        extra.insert("updatedAt".into(), Value::String(to_local(&updated_at)));
    }

    Some(TimeEntry {
        source: "clockify".into(),
        id,
        start,
        end,
        duration_secs,
        description: str_field(e, "description"),
        project,
        // Client names need a separate /clients call; not fetched.
        client: String::new(),
        // Task names need a separate /tasks call; taskId preserved in extra.
        task: String::new(),
        tags,
        billable,
        extra,
    })
}

/// Latest `timeInterval.start` (as UTC RFC3339 string) across a batch.
fn max_start_utc(entries: &[Value]) -> Option<String> {
    entries
        .iter()
        .filter_map(|e| {
            let s = e.get("timeInterval")?.get("start")?.as_str()?;
            to_utc_str(s)
        })
        .max()
}

// ---------------------------------------------------------------------------
// Vault writes.

fn write_entries(vault: &Vault, rows: Vec<(TimeEntry, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    let mut seen_ids: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let id = str_field(&v, "id");
            if !id.is_empty() {
                seen_ids.insert(id);
            }
        }
    }

    let mut seen_raw: HashSet<(String, String)> = HashSet::new();
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
            if let Some(k) = raw_key(&v) {
                seen_raw.insert(k);
            }
        }
    }

    let mut new_rows: Vec<TimeEntry> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        if let Some(k) = raw_key(&raw_val) {
            if seen_raw.insert(k) {
                new_raws.push(RawLine { ts: row.start.clone(), value: raw_val });
            }
        }
        if !row.id.is_empty() && seen_ids.insert(row.id.clone()) {
            new_rows.push(row);
        }
    }

    contract.append(&new_rows, |r| &r.start)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Clockify is not connected -- add your API key in the Integrations tab")?;
    let client = ClockifyClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl ClockifyApi) -> Result<PullOutcome> {
    let mut state = vault.read_clockify_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- resolve user + workspace IDs ------------------------------------
    let (user_id, workspace_id) = match (state.user_id.clone(), state.workspace_id.clone()) {
        (Some(u), Some(w)) => (u, w),
        _ => {
            let (u, w) = api.get_user().map_err(|e| fetch_err("user", e))?;
            state.user_id = Some(u.clone());
            state.workspace_id = Some(w.clone());
            (u, w)
        }
    };

    // --- projects: id -> name -------------------------------------------
    let project_items = api
        .get_projects(&workspace_id)
        .map_err(|e| fetch_err("projects", e))?;
    let project_names: HashMap<String, String> =
        project_items.iter().filter_map(project_id_name).collect();

    // --- tags: id -> name -----------------------------------------------
    let tag_items = api
        .get_tags(&workspace_id)
        .map_err(|e| fetch_err("tags", e))?;
    let tag_names: HashMap<String, String> = tag_items.iter().filter_map(tag_id_name).collect();

    // --- time entries: paginate until last page -------------------------
    let start_bound = state.last_start.as_deref();
    let mut page = 1u32;
    let mut all_entries: Vec<Value> = Vec::new();
    loop {
        let (batch, is_last) = api
            .get_time_entries(&workspace_id, &user_id, start_bound, page)
            .map_err(|e| fetch_err("time-entries", e))?;
        all_entries.extend(batch);
        if is_last {
            break;
        }
        page += 1;
    }

    // Compute watermark BEFORE writing; advance only after full successful drain.
    let new_watermark = max_start_utc(&all_entries);

    let rows: Vec<(TimeEntry, Value)> = all_entries
        .iter()
        .filter_map(|e| entry_from(e, &project_names, &tag_names).map(|t| (t, e.clone())))
        .collect();
    let written = write_entries(vault, rows)?;
    counts.insert("entries", written);

    // Advance cursor only forward.
    if let Some(w) = new_watermark {
        if state.last_start.as_ref().is_none_or(|cur| w > *cur) {
            state.last_start = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_clockify_sync(&state)?;

    let e = counts.get("entries").copied().unwrap_or(0);
    Ok(PullOutcome { headline: format!("{e} time entries"), counts })
}

fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Clockify rejected the API key (401) on {endpoint} -- reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Clockify rate limited {endpoint} (429) -- it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Clockify {endpoint} fetch failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-clockify-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (Clockify v1 documented shape) -------------------------

    /// A stopped time entry. timeInterval.start/end are RFC3339 UTC;
    /// duration is ISO 8601 "PT..." form (e.g. "PT1H30M").
    fn entry_stopped() -> Value {
        json!({
            "id": "657f1a9b2c3d4e5f6a7b8c9d",
            "description": "Quarterly traffic report",
            "billable": true,
            "projectId": "64f1a2b3c4d5e6f7a8b9c0d1",
            "taskId": "64f1a2b3c4d5e6f7a8b9c0d2",
            "tagIds": ["64f1a2b3c4d5e6f7a8b9c0d3"],
            "workspaceId": "64f1a2b3c4d5e6f7a8b9c000",
            "userId": "64f1a2b3c4d5e6f7a8b9c001",
            "isLocked": false,
            "updatedAt": "2026-06-10T17:30:05Z",
            "timeInterval": {
                "start": "2026-06-10T16:00:00Z",
                "end": "2026-06-10T17:30:00Z",
                "duration": "PT1H30M"
            }
        })
    }

    /// A running timer -- end and duration are null in the Clockify API.
    fn entry_running() -> Value {
        json!({
            "id": "657f1a9b2c3d4e5f6a7b8c9e",
            "description": "Writing the collector",
            "billable": false,
            "projectId": "64f1a2b3c4d5e6f7a8b9c0d1",
            "taskId": null,
            "tagIds": [],
            "workspaceId": "64f1a2b3c4d5e6f7a8b9c000",
            "userId": "64f1a2b3c4d5e6f7a8b9c001",
            "isLocked": false,
            "updatedAt": "2026-06-15T15:00:01Z",
            "timeInterval": {
                "start": "2026-06-15T15:00:00Z",
                "end": null,
                "duration": null
            }
        })
    }

    /// An entry without a project (free-floating entry).
    fn entry_no_project() -> Value {
        json!({
            "id": "657f1a9b2c3d4e5f6a7b0000",
            "description": "Admin work",
            "billable": false,
            "projectId": "",
            "taskId": "",
            "tagIds": [],
            "workspaceId": "64f1a2b3c4d5e6f7a8b9c000",
            "userId": "64f1a2b3c4d5e6f7a8b9c001",
            "isLocked": false,
            "updatedAt": "2026-06-11T10:00:00Z",
            "timeInterval": {
                "start": "2026-06-11T09:00:00Z",
                "end": "2026-06-11T10:00:00Z",
                "duration": "PT1H"
            }
        })
    }

    fn project_json(id: &str, name: &str) -> Value {
        json!({"id": id, "name": name, "workspaceId": "64f1a2b3c4d5e6f7a8b9c000"})
    }

    fn tag_json(id: &str, name: &str) -> Value {
        json!({"id": id, "name": name, "workspaceId": "64f1a2b3c4d5e6f7a8b9c000"})
    }

    // --- ISO 8601 duration parser ----------------------------------------

    #[test]
    fn iso_duration_common_forms() {
        assert_eq!(parse_iso_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_iso_duration("PT5400S"), Some(5400));
        assert_eq!(parse_iso_duration("PT1H"), Some(3600));
        assert_eq!(parse_iso_duration("PT30M"), Some(1800));
        assert_eq!(parse_iso_duration("PT0S"), Some(0));
        assert_eq!(parse_iso_duration("PT1H30M45S"), Some(5445));
        assert_eq!(parse_iso_duration(""), None);
        assert_eq!(parse_iso_duration("P1D"), None); // date part not supported
    }

    // --- mapping tests ---------------------------------------------------

    #[test]
    fn maps_stopped_entry_with_project_name_and_tag() {
        let projects: HashMap<String, String> = [(
            "64f1a2b3c4d5e6f7a8b9c0d1".to_string(),
            "Editorial".to_string(),
        )]
        .into_iter()
        .collect();
        let tags: HashMap<String, String> = [(
            "64f1a2b3c4d5e6f7a8b9c0d3".to_string(),
            "deep-work".to_string(),
        )]
        .into_iter()
        .collect();

        let e = entry_from(&entry_stopped(), &projects, &tags).unwrap();
        assert_eq!(e.source, "clockify");
        assert_eq!(e.id, "657f1a9b2c3d4e5f6a7b8c9d");
        assert_eq!(e.description, "Quarterly traffic report");
        assert_eq!(e.project, "Editorial");
        assert_eq!(e.tags, vec!["deep-work"]);
        assert_eq!(e.duration_secs, Some(5400));
        assert_eq!(e.billable, Some(true));
        assert!(e.client.is_empty(), "no client name (no /clients call)");
        assert!(e.task.is_empty(), "task name not resolved");
        assert_eq!(
            DateTime::parse_from_rfc3339(&e.start).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T16:00:00Z").unwrap().timestamp(),
        );
        assert_eq!(
            DateTime::parse_from_rfc3339(&e.end).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T17:30:00Z").unwrap().timestamp(),
        );
        assert_eq!(e.extra.get("workspaceId"), Some(&json!("64f1a2b3c4d5e6f7a8b9c000")));
        assert_eq!(e.extra.get("projectId"), Some(&json!("64f1a2b3c4d5e6f7a8b9c0d1")));
        assert_eq!(e.extra.get("taskId"), Some(&json!("64f1a2b3c4d5e6f7a8b9c0d2")));
        assert_eq!(e.extra.get("tagIds"), Some(&json!(["64f1a2b3c4d5e6f7a8b9c0d3"])));
        assert_eq!(e.extra.get("isLocked"), Some(&json!(false)));
    }

    #[test]
    fn running_timer_omits_end_and_duration() {
        let e = entry_from(&entry_running(), &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(e.id, "657f1a9b2c3d4e5f6a7b8c9e");
        assert!(e.end.is_empty(), "running timer has no end");
        assert!(e.duration_secs.is_none(), "running timer has no duration_secs");
        let re = serde_json::to_value(&e).unwrap();
        assert!(re.get("end").is_none());
        assert!(re.get("duration_secs").is_none());
        assert_eq!(re.get("billable"), Some(&json!(false)));
    }

    #[test]
    fn entry_without_project_is_mapped() {
        let e = entry_from(&entry_no_project(), &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(e.id, "657f1a9b2c3d4e5f6a7b0000");
        assert!(e.project.is_empty(), "no project -> empty string");
        assert_eq!(e.duration_secs, Some(3600)); // PT1H
        assert!(e.extra.get("projectId").is_none(), "empty projectId not stored in extra");
        assert!(e.extra.get("taskId").is_none(), "empty taskId not stored in extra");
    }

    #[test]
    fn max_start_utc_picks_latest() {
        let entries = vec![entry_stopped(), entry_running(), entry_no_project()];
        let w = max_start_utc(&entries).unwrap();
        assert_eq!(w, to_utc_str("2026-06-15T15:00:00Z").unwrap());
    }

    #[test]
    fn project_and_tag_helpers() {
        assert_eq!(
            project_id_name(&project_json("pid1", "Editorial")),
            Some(("pid1".into(), "Editorial".into()))
        );
        assert!(project_id_name(&json!({"id": "pid1"})).is_none());
        assert!(project_id_name(&json!({"name": "x"})).is_none());
        assert_eq!(
            tag_id_name(&tag_json("tid1", "deep-work")),
            Some(("tid1".into(), "deep-work".into()))
        );
    }

    // --- mock API + full-pull tests -------------------------------------

    struct MockApi {
        user: (String, String),
        projects: Vec<Value>,
        tags: Vec<Value>,
        pages: RefCell<Vec<(Vec<Value>, bool)>>,
        starts_seen: RefCell<Vec<Option<String>>>,
    }

    impl MockApi {
        fn single_page(entries: Vec<Value>) -> Self {
            MockApi {
                user: ("uid1".into(), "ws1".into()),
                projects: vec![project_json("64f1a2b3c4d5e6f7a8b9c0d1", "Editorial")],
                tags: vec![tag_json("64f1a2b3c4d5e6f7a8b9c0d3", "deep-work")],
                pages: RefCell::new(vec![(entries, true)]),
                starts_seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl ClockifyApi for MockApi {
        fn get_user(&self) -> Result<(String, String), FetchError> {
            Ok(self.user.clone())
        }
        fn get_projects(&self, _ws: &str) -> Result<Vec<Value>, FetchError> {
            Ok(self.projects.clone())
        }
        fn get_tags(&self, _ws: &str) -> Result<Vec<Value>, FetchError> {
            Ok(self.tags.clone())
        }
        fn get_time_entries(
            &self,
            _ws: &str,
            _uid: &str,
            start: Option<&str>,
            _page: u32,
        ) -> Result<(Vec<Value>, bool), FetchError> {
            self.starts_seen.borrow_mut().push(start.map(str::to_string));
            Ok(self.pages.borrow_mut().pop().unwrap_or_else(|| (Vec::new(), true)))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::single_page(vec![entry_stopped(), entry_running(), entry_no_project()]);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&3));

        let jun =
            std::fs::read_to_string(v.root().join("time-entries/clockify/2026-06.jsonl")).unwrap();
        assert_eq!(jun.lines().count(), 3);
        assert!(jun.contains("\"source\":\"clockify\""));
        assert!(jun.contains("\"id\":\"657f1a9b2c3d4e5f6a7b8c9d\""));
        assert!(jun.contains("\"project\":\"Editorial\""));
        assert!(jun.contains("\"duration_secs\":5400"));
        assert!(jun.contains("\"tags\":[\"deep-work\"]"));
        let running_line = jun.lines().find(|l| l.contains("657f1a9b2c3d4e5f6a7b8c9e")).unwrap();
        assert!(!running_line.contains("duration_secs"));
        assert!(!running_line.contains("\"end\""));

        let raw = std::fs::read_to_string(
            v.root().join("time-entries/clockify/raw/2026-06.jsonl"),
        )
        .unwrap();
        assert!(raw.contains("\"timeInterval\""));
        assert!(raw.contains("\"PT1H30M\""), "raw keeps ISO duration string verbatim");

        let state = v.read_clockify_sync();
        let expected = to_utc_str("2026-06-15T15:00:00Z").unwrap();
        assert_eq!(state.last_start.as_deref(), Some(expected.as_str()));
        assert!(state.user_id.is_some());
        let cursor =
            std::fs::read_to_string(v.root().join(".trove/clockify-sync.json")).unwrap();
        assert!(!cursor.contains("access_token") && !cursor.contains("api_key"));
        assert_eq!(api.starts_seen.borrow().as_slice(), &[None]);

        let api2 =
            MockApi::single_page(vec![entry_stopped(), entry_running(), entry_no_project()]);
        let again = pull_with(&v, &api2).unwrap();
        assert_eq!(again.counts.get("entries"), Some(&0));
        let jun2 =
            std::fs::read_to_string(v.root().join("time-entries/clockify/2026-06.jsonl")).unwrap();
        assert_eq!(jun, jun2, "contract file byte-identical after re-run");
        assert_eq!(api2.starts_seen.borrow()[0].as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn running_then_stopped_keeps_first_contract_row_but_both_raw() {
        let v = temp_vault("running-stop");
        let api1 = MockApi::single_page(vec![entry_running()]);
        pull_with(&v, &api1).unwrap();
        let after1 =
            std::fs::read_to_string(v.root().join("time-entries/clockify/2026-06.jsonl")).unwrap();
        assert!(!after1.contains("duration_secs"), "still running");

        let stopped_same_id = json!({
            "id": "657f1a9b2c3d4e5f6a7b8c9e",
            "description": "Writing the collector",
            "billable": false,
            "projectId": "64f1a2b3c4d5e6f7a8b9c0d1",
            "tagIds": [],
            "workspaceId": "64f1a2b3c4d5e6f7a8b9c000",
            "userId": "64f1a2b3c4d5e6f7a8b9c001",
            "isLocked": false,
            "updatedAt": "2026-06-15T16:00:00Z",
            "timeInterval": {
                "start": "2026-06-15T15:00:00Z",
                "end": "2026-06-15T16:00:00Z",
                "duration": "PT1H"
            }
        });
        let api2 = MockApi::single_page(vec![stopped_same_id]);
        let out = pull_with(&v, &api2).unwrap();
        assert_eq!(out.counts.get("entries"), Some(&0), "same id, no new contract row");

        let after2 =
            std::fs::read_to_string(v.root().join("time-entries/clockify/2026-06.jsonl")).unwrap();
        assert_eq!(after2.lines().count(), 1, "contract still one open line");
        assert!(!after2.contains("duration_secs"), "first-observed open row preserved");

        let raw =
            std::fs::read_to_string(v.root().join("time-entries/clockify/raw/2026-06.jsonl"))
                .unwrap();
        assert_eq!(raw.lines().count(), 2, "raw has both: running + stopped snapshots");
        assert!(raw.contains("\"PT1H\""), "stopped snapshot in raw");
    }

    #[test]
    fn fetch_error_does_not_advance_cursor() {
        let v = temp_vault("fetcherr");
        let watermark = to_utc_str("2026-06-10T16:00:00Z").unwrap();
        v.write_clockify_sync(&SyncState {
            user_id: Some("uid1".into()),
            workspace_id: Some("ws1".into()),
            last_start: Some(watermark.clone()),
            updated: None,
        })
        .unwrap();

        struct ErrApi;
        impl ClockifyApi for ErrApi {
            fn get_user(&self) -> Result<(String, String), FetchError> {
                Ok(("uid1".into(), "ws1".into()))
            }
            fn get_projects(&self, _: &str) -> Result<Vec<Value>, FetchError> {
                Ok(Vec::new())
            }
            fn get_tags(&self, _: &str) -> Result<Vec<Value>, FetchError> {
                Ok(Vec::new())
            }
            fn get_time_entries(
                &self,
                _: &str,
                _: &str,
                _: Option<&str>,
                _: u32,
            ) -> Result<(Vec<Value>, bool), FetchError> {
                Err(FetchError::Other("network failure".into()))
            }
        }

        let err = pull_with(&v, &ErrApi).unwrap_err().to_string();
        assert!(err.contains("time-entries"), "{err}");
        let state = v.read_clockify_sync();
        assert_eq!(state.last_start.as_deref(), Some(watermark.as_str()), "cursor untouched");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_start.is_none());
        assert!(empty.user_id.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_start":"2026-06-10T16:00:00+00:00"}"#).unwrap();
        assert_eq!(partial.last_start.as_deref(), Some("2026-06-10T16:00:00+00:00"));
        assert!(partial.user_id.is_none());
    }

    #[test]
    fn connection_stores_and_status_reflects() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "clk_secret_abc".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Clockify");
        assert_eq!(status.accounts[0].key, "clockify");

        def_disconnect(&v, "clockify").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn connection_exposes_token_paste_method_and_ids() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "clockify");
        assert_eq!(DEF.connection, Some("clockify"));
    }

    #[test]
    fn empty_api_key_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "  ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "{err}");
    }
}
