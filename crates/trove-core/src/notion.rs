//! Notion — cloud sync via official API. A **Periodic** cloud pull of pages
//! and database task items from a connected Notion workspace. Catalogued in the
//! Phase 2 pass; brief: docs/integrations/notion.md.
//!
//! Two destinations, written in one pass:
//!
//! - **tasks contract** under `tasks/notion/` (the ratified [`crate::tasks`]
//!   contract): database rows whose property set looks like tasks (status /
//!   checkbox / due date) → [`Task`], persisted via `apply_tasks_sync`.
//!   Fate closure: Notion has no separate completed-list endpoint — a task
//!   that disappears between syncs is assumed **Deleted** (the row was deleted
//!   or unshared), never Completed (we can't reconstruct the completion time
//!   without observing the property change). If a data source query fails
//!   (transient error / 5xx / 429), its tasks carry forward as
//!   `TaskFate::Unknown` rather than being marked deleted.
//!
//! - **raw firehose** under `notes/notion/raw/YYYY-MM.jsonl` (partitioned by
//!   `created_time` month): the raw API page objects at full fidelity, keyed
//!   by page id, upserted on each sync.
//!
//! Notes pages (pages **not** in any data source, or in a data source whose
//! properties don't include a task-shape) are written to the raw layer only
//! for now — the `notes/` contract write is parked until a notes-contract
//! pioneer ratifies it (brief: Needs-David). The raw layer already gives
//! full fidelity; the contract layer adds only if the schema is ratified.
//!
//! ## API shape (Notion REST API 2026-03-11)
//!
//! All requests: `Authorization: Bearer <token>`, `Notion-Version: 2026-03-11`.
//!
//! Per the 2025-09-03 API breaking change:
//!
//! - `POST /v1/search` (body `{"filter":{"value":"data_source","property":"object"}}`)
//!   → lists every data source the integration can see (paginated; `has_more` /
//!   `next_cursor`; `results[].id`, `results[].object = "data_source"`).
//!   NOTE: "database" is no longer a valid filter value under 2026-03-11;
//!   only "page" and "data_source" are valid.
//! - `POST /v1/data_sources/{data_source_id}/query`
//!   (body `{"page_size":100,"start_cursor":"…"}`)
//!   → pages/rows of a data source (paginated; same envelope shape).
//! - `POST /v1/search` (body `{"filter":{"value":"page","property":"object"}}`)
//!   → standalone pages the integration can see.
//!
//! All endpoints wrap results as:
//! ```json
//! { "object": "list", "results": [...], "next_cursor": "…", "has_more": true }
//! ```
//!
//! A data source object (from the search `data_source` filter) has:
//! - `id` (UUID), `object` = `"data_source"`,
//! - `title`: rich-text array → `[{"plain_text": "…", …}]`
//! - `parent`: `{"type":"database_id","database_id":"…"}` (most common)
//! - `properties`: the schema (not page content)
//!
//! A page / row (from `data_sources/{id}/query` or page search) has:
//! - `id` (UUID), `object` = `"page"`,
//! - `created_time` / `last_edited_time` (ISO 8601 UTC),
//! - `properties`: a JSON object where each key is a property name and the
//!   value is `{"id":"…","type":"<kind>","<kind>": …}`.
//!   - title property: `"title": [{"plain_text": "…", …}]`
//!   - status property: `"status": {"name": "…", "color": "…"}`
//!   - checkbox property: `"checkbox": true|false`
//!   - date property: `"date": {"start": "…", "end": …, "time_zone": …}`
//! - `parent`: `{"type":"data_source_id","data_source_id":"…","database_id":"…"}`
//!   or `{"type":"database_id","database_id":"…"}` for legacy rows.
//!
//! Rate limit: ~3 req/s (2,700/15 min). We sync every 30 min so a typical
//! moderate workspace (dozens of data sources, hundreds of rows) fits
//! comfortably within budget.
//!
//! ## Cursor / dedup
//!
//! `.trove/notion-sync.json` (non-secret, rebuildable) holds the last
//! successful sync time and, per-database, the highest `last_edited_time` seen
//! (used as a progress indicator; the API has no `since` param so we always
//! re-query but skip rows we've already written if they haven't changed — an
//! optimization for large workspaces). The raw firehose upserts by id so a
//! re-sync never duplicates rows. The task snapshot diff handles completions
//! and deletions via `apply_tasks_sync`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
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
use crate::tasks::{ProjectInfo, Task, TaskFate};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const RAW_NOTES_DIR: &str = "notes/notion/raw";
const SYNC_FILE: &str = ".trove/notion-sync.json";
const SERVICE: &str = "notion";
const API_BASE: &str = "https://api.notion.com";
const NOTION_VERSION: &str = "2026-03-11";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// 30-minute cadence: a moderate workspace fits 3 req/s well within budget.
const NOTION_SYNC_SECS: u64 = 1800;
/// Notion page size maximum for query/search (100 items per page).
const PAGE_SIZE: u32 = 100;
/// Brief sleep between API calls to avoid hitting the ~3 req/s rate limit.
const THROTTLE: Duration = Duration::from_millis(350);

// ---------------------------------------------------------------------------
// Registry face

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_NOTES_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "notion synced — {} task rows, {} raw pages",
                    c("tasks"),
                    c("raw")
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "notion sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    let headline = format!(
        "Notion synced — {} task rows, {} raw pages",
        c("tasks"),
        c("raw")
    );
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "notion",
        name: "Notion",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Notion databases and pages using a personal \
                      integration token. Database rows that look like tasks \
                      (status/checkbox + optional due date) land in the \
                      unified task store; all pages are preserved raw at \
                      full fidelity.",
        domain: "notes",
        vault_path: "notes/notion/",
        toggleable: true,
        setup: &[
            "Go to app.notion.com/profile/integrations and create a new integration (type: Internal).",
            "Copy the 'Internal Integration Secret' — it starts with 'ntn_'.",
            "In Notion, open each page or database you want Trove to see, click '···' → \
             Connections → add your integration.",
            "Paste the token here.",
        ],
        caveats: "Only pages/databases shared with the integration are visible. \
                 Task completions are not reconstructed from the API — connect early \
                 and keep syncing to capture state changes. Large workspaces may \
                 take a few minutes on the first sync due to the API rate limit.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(NOTION_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("notion"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a personal integration secret, a SECRET).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Notion integration secret");
    }
    // Probe: use the same data_source filter the pull uses so a version/shape
    // mismatch fails loudly at connect time rather than silently during sync.
    let client = NotionClient::new(API_BASE.to_string(), token.to_string());
    let probe_body = serde_json::json!({
        "filter": { "value": "data_source", "property": "object" },
        "page_size": 1
    });
    match client.post_json("/v1/search", &probe_body) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Notion rejected the token (401) — check it's the 'Internal Integration Secret' \
             from app.notion.com/profile/integrations and hasn't been revoked"
        ),
        Err(e) => bail!("Notion search check failed: {e}"),
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
            label: "Notion".to_string(),
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
    id: "notion",
    display_name: "Notion",
    methods: &[ConnectMethod::TokenPaste {
        label: "Notion integration secret",
        help: "Paste your Notion internal integration secret from \
               app.notion.com/profile/integrations.",
        placeholder: "ntn_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["notion"],
    setup: &[
        "Open app.notion.com/profile/integrations and create a new Internal integration.",
        "Copy the 'Internal Integration Secret' (starts with 'ntn_').",
        "Share each Notion page/database: open the page, click '···' → Connections → add your integration.",
        "Paste the secret here — stored locally, never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page of results from a list/query/search response.
struct Page {
    results: Vec<Value>,
    next_cursor: Option<String>,
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

trait NotionApi {
    /// POST `path` with `body`, return one page of results.
    fn post_json(&self, path: &str, body: &Value) -> Result<Page, FetchError>;
}

struct NotionClient {
    base: String,
    token: String,
}

impl NotionClient {
    fn new(base: String, token: String) -> Self {
        NotionClient { base, token }
    }

    fn handle(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<Page, FetchError> {
        match resp {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(parse_page(v))
            }
            Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
                Err(FetchError::Unauthorized)
            }
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

impl NotionApi for NotionClient {
    fn post_json(&self, path: &str, body: &Value) -> Result<Page, FetchError> {
        NotionClient::handle(
            ureq::post(&format!("{}{path}", self.base))
                .timeout(HTTP_TIMEOUT)
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Notion-Version", NOTION_VERSION)
                .set("Content-Type", "application/json")
                .send_json(body.clone()),
        )
    }
}

/// Parse a Notion list response: `{ results: [...], next_cursor: …, has_more: … }`.
fn parse_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let results = o
                .get("results")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let next_cursor = o
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Page { results, next_cursor }
        }
        _ => Page { results: Vec::new(), next_cursor: None },
    }
}

/// Drain all pages of a paginated POST request (following `start_cursor`).
fn drain_all(api: &impl NotionApi, path: &str, base_body: &Value) -> Result<Vec<Value>, FetchError> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = base_body.clone();
        if let Some(c) = &cursor {
            if let Value::Object(ref mut m) = body {
                m.insert("start_cursor".into(), Value::String(c.clone()));
            }
        }
        if let Value::Object(ref mut m) = body {
            m.insert("page_size".into(), Value::Number(PAGE_SIZE.into()));
        }
        let page = api.post_json(path, &body)?;
        all.extend(page.results);
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
        thread::sleep(THROTTLE);
    }
    Ok(all)
}

// ---------------------------------------------------------------------------
// Cursor

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_sync: Option<String>,
}

impl Vault {
    fn read_notion_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_notion_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers

/// Extract the plain-text value from a Notion `title` property array.
/// `props["Name"]["title"][0]["plain_text"]` → `"My task"`.
fn extract_title(properties: &Map<String, Value>) -> Option<String> {
    // Notion's title property can have any name but always has type = "title".
    for (_name, prop) in properties {
        if prop.get("type").and_then(Value::as_str) == Some("title") {
            let title = prop
                .get("title")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(|rt| rt.get("plain_text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("");
            if !title.trim().is_empty() {
                return Some(title.trim().to_string());
            }
        }
    }
    None
}

/// Extract a non-empty string field from a flat JSON object.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Notion ISO 8601 UTC timestamp (`2024-01-15T10:30:00.000Z`) → RFC3339 local.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .or_else(|_| {
            // Notion sometimes omits the trailing 'Z'; try with 'Z' appended.
            let with_z = format!("{s}Z");
            DateTime::parse_from_rfc3339(&with_z)
        })
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Does this page look like a task database row?
/// True when the properties include any of: a `status` prop, a `checkbox`
/// prop, or a `date` prop named "due"/"Due"/"Due date"/"deadline"/"Deadline".
fn looks_like_task(properties: &Map<String, Value>) -> bool {
    for (name, prop) in properties {
        match prop.get("type").and_then(Value::as_str) {
            Some("status") | Some("checkbox") => return true,
            Some("date") => {
                let lc = name.to_lowercase();
                if lc == "due" || lc == "due date" || lc == "deadline" {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Returns the data source id that owns this page, for use as the project key.
///
/// Under the 2026-03-11 API, pages returned by `data_sources/{id}/query` carry
/// `parent.type = "data_source_id"` and a `parent.data_source_id` field.
/// Legacy rows (and standalone pages in databases) may still carry
/// `parent.type = "database_id"`. We accept both so that task rows from data
/// source queries resolve to a project, regardless of which parent shape the
/// API returns.
fn parent_datasource_id(page: &Value) -> Option<String> {
    let parent = page.get("parent")?;
    match parent.get("type").and_then(Value::as_str) {
        Some("data_source_id") => parent
            .get("data_source_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        Some("database_id") => parent
            .get("database_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

/// Map one Notion data-source-row page to a [`Task`]. Returns `None` when the
/// page has no id or no title.
fn task_from_page(
    page: &Value,
    datasource_titles: &HashMap<String, String>,
) -> Option<Task> {
    let obj = page.as_object()?;
    let id = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let props = obj.get("properties")?.as_object()?;
    let title = extract_title(props)?;

    // Project = the data source's title (or its id when the title isn't known).
    let ds_id = parent_datasource_id(page);
    let project = ds_id
        .as_ref()
        .and_then(|did| datasource_titles.get(did))
        .cloned()
        .unwrap_or_else(|| ds_id.clone().unwrap_or_default());

    // Status: look for a `status` prop first, then a `checkbox` prop.
    let mut status = "open".to_string();
    for (_name, prop) in props {
        match prop.get("type").and_then(Value::as_str) {
            Some("status") => {
                let name = prop
                    .get("status")
                    .and_then(|s| s.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                // Common "done" status names from Notion's default templates.
                if matches!(name.as_str(), "done" | "complete" | "completed" | "finished" | "closed") {
                    status = "done".to_string();
                }
                break;
            }
            Some("checkbox") => {
                if prop.get("checkbox").and_then(Value::as_bool).unwrap_or(false) {
                    status = "done".to_string();
                }
                break;
            }
            _ => {}
        }
    }

    // Due date: look for a `date` prop whose name suggests a deadline.
    let mut due: Option<String> = None;
    let mut all_day = false;
    for (name, prop) in props {
        if prop.get("type").and_then(Value::as_str) == Some("date") {
            let lc = name.to_lowercase();
            if lc == "due" || lc == "due date" || lc == "deadline" {
                if let Some(date_obj) = prop.get("date") {
                    if let Some(start) = date_obj.get("start").and_then(Value::as_str) {
                        // A "date-only" start has no time component (length ≤ 10).
                        all_day = start.len() <= 10;
                        due = Some(if all_day {
                            // date-only: keep as-is with a local noon anchor.
                            format!("{start}T12:00:00")
                        } else {
                            to_local(start)
                        });
                    }
                }
                break;
            }
        }
    }

    let created = str_opt(page, "created_time").map(|s| to_local(&s));
    let modified = str_opt(page, "last_edited_time").map(|s| to_local(&s));

    // Extra: source-specific overflow.
    let mut extra = Map::new();
    if let Some(did) = &ds_id {
        extra.insert("datasource_id".into(), Value::from(did.as_str()));
    }
    let page_url = format!("https://www.notion.so/{}", id.replace('-', ""));
    extra.insert("url".into(), Value::from(page_url));
    // Preserve raw properties for full fidelity.
    extra.insert("notion_properties".into(), Value::Object(props.clone()));

    Some(Task {
        source: "notion".into(),
        id,
        title,
        project,
        notes: String::new(),
        status,
        priority: 0,
        due,
        start: None,
        all_day,
        recurrence: None,
        tags: Vec::new(),
        subtasks: Vec::new(),
        created,
        modified,
        completed: None,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition: read the target month, merge by page id,
// rewrite sorted. A re-sync never duplicates a page.

fn upsert_raw_pages(vault: &Vault, pages: Vec<Value>) -> Result<u64> {
    let stream = vault.stream(RAW_NOTES_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for page in pages {
        let created = page.get("created_time").and_then(Value::as_str).unwrap_or("");
        let key = match Partition::Month.key(created) {
            Some(k) => k.to_string(),
            None => continue, // can't partition without a creation time
        };
        by_month.entry(key).or_default().push(page);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<Value> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> = existing
            .iter()
            .enumerate()
            .filter_map(|(i, v)| {
                v.get("id").and_then(Value::as_str).map(|id| (id.to_string(), i))
            })
            .collect();
        for page in fresh {
            let id = page.get("id").and_then(Value::as_str).map(str::to_string);
            match id.as_ref().and_then(|id| idx.get(id.as_str())).copied() {
                Some(i) => existing[i] = page, // refresh with the latest snapshot
                None => {
                    let id = id.unwrap_or_default();
                    idx.insert(id, existing.len());
                    existing.push(page);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| {
            let ta = a.get("created_time").and_then(Value::as_str).unwrap_or("");
            let tb = b.get("created_time").and_then(Value::as_str).unwrap_or("");
            ta.cmp(tb)
                .then_with(|| {
                    let ia = a.get("id").and_then(Value::as_str).unwrap_or("");
                    let ib = b.get("id").and_then(Value::as_str).unwrap_or("");
                    ia.cmp(ib)
                })
        });
        vault.write_snapshot(&format!("{RAW_NOTES_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull

/// Resolve credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Notion is not connected — add your integration token in the Integrations tab")?;
    let client = NotionClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client, Local::now())
}

fn map_fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Notion rejected the token (401/403) — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Notion fetch failed: {other}"),
    }
}

/// The pull body over an injected API + clock — the testable seam.
fn pull_with(
    vault: &Vault,
    api: &impl NotionApi,
    now: DateTime<Local>,
) -> Result<PullOutcome> {
    let mut state = vault.read_notion_sync();

    // ---- 1. Discover all accessible data sources (for project names) -------
    //
    // Per the 2025-09-03 API breaking change, the only valid filter values for
    // POST /v1/search with property="object" are "page" and "data_source".
    // Sending "database" returns HTTP 400 under Notion-Version 2026-03-11.
    // Each data source result has: object="data_source", id, title (rich-text
    // array), parent.type="database_id"|"data_source_id".
    let ds_search_body = serde_json::json!({
        "filter": { "value": "data_source", "property": "object" }
    });
    let ds_results = drain_all(api, "/v1/search", &ds_search_body).map_err(map_fetch_err)?;

    // Map data_source_id → title string.
    let datasource_titles: HashMap<String, String> = ds_results
        .iter()
        .filter_map(|ds| {
            let id = ds.get("id").and_then(Value::as_str)?.to_string();
            // Title is always a top-level `title` rich-text array on data_source objects.
            let title = ds
                .get("title")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|rt| rt.get("plain_text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("")
                })
                .unwrap_or_default();
            Some((id, if title.trim().is_empty() { "Untitled".to_string() } else { title.trim().to_string() }))
        })
        .collect();

    // ---- 2. Query each data source for its rows ----------------------------
    //
    // POST /v1/data_sources/{data_source_id}/query replaces the deprecated
    // POST /v1/databases/{id}/query under Notion-Version 2026-03-11.
    //
    // Track which data sources failed so we can carry their tasks forward with
    // TaskFate::Unknown rather than incorrectly marking them as deleted (fixes
    // the snapshot-diff deletion flood on transient 5xx/429/403 errors).
    let mut all_pages: Vec<Value> = Vec::new();
    let mut projects: Vec<ProjectInfo> = Vec::new();
    let mut task_ids_seen: HashSet<String> = HashSet::new();
    let mut failed_datasource_ids: HashSet<String> = HashSet::new();

    for (ds_id, ds_title) in &datasource_titles {
        thread::sleep(THROTTLE);
        let query_body = serde_json::json!({});
        let rows = match drain_all(
            api,
            &format!("/v1/data_sources/{ds_id}/query"),
            &query_body,
        ) {
            Ok(r) => r,
            Err(e) => {
                // A forbidden / not-found / transient error: skip this data
                // source's rows but mark it as failed so its tasks carry
                // forward instead of being deleted.
                eprintln!("trove notion: data_source {ds_id:?} query failed: {e}");
                failed_datasource_ids.insert(ds_id.clone());
                continue;
            }
        };
        // Collect data source as a project.
        projects.push(ProjectInfo { id: ds_id.clone(), name: ds_title.clone() });
        all_pages.extend(rows);
    }

    // ---- 3. Search for standalone pages ------------------------------------
    thread::sleep(THROTTLE);
    let page_search_body = serde_json::json!({
        "filter": { "value": "page", "property": "object" }
    });
    let page_results = drain_all(api, "/v1/search", &page_search_body).map_err(map_fetch_err)?;
    all_pages.extend(page_results);

    // ---- 4. Deduplicate by page id (a page may appear in DS results AND search) --
    let mut seen_ids: HashSet<String> = HashSet::new();
    all_pages.retain(|p| {
        let id = p.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        seen_ids.insert(id)
    });

    // ---- 5. Raw layer (unconditional, full fidelity) -----------------------
    let raw_new = upsert_raw_pages(vault, all_pages.clone())?;

    // ---- 6. Task contract layer for DS rows that look like tasks -----------
    let fresh_tasks: Vec<Task> = all_pages
        .iter()
        .filter(|page| {
            // Only rows from a data source / database (not standalone pages).
            parent_datasource_id(page).is_some()
        })
        .filter(|page| {
            page.get("properties")
                .and_then(Value::as_object)
                .map(|props| looks_like_task(props))
                .unwrap_or(false)
        })
        .filter_map(|page| task_from_page(page, &datasource_titles))
        .inspect(|t| { task_ids_seen.insert(t.id.clone()); })
        .collect();

    let task_count = fresh_tasks.len() as u64;

    // Fate closure: for tasks not present in this sync —
    //   • TaskFate::Unknown  → task's data source query FAILED (transient error);
    //                          carry the task forward, retry next run.
    //   • TaskFate::Deleted  → data source was queried successfully but the row
    //                          is absent (deleted or unshared). No completed-list
    //                          endpoint exists in Notion, so we can't distinguish
    //                          deletion from completion; we record Deleted.
    // Note: apply_tasks_sync advances the watermark + writes the diff.
    vault
        .apply_tasks_sync("notion", &projects, fresh_tasks, |t| {
            // Extract the data source id from the task's extra map.
            let ds_id = t.extra.get("datasource_id").and_then(Value::as_str).unwrap_or("");
            if failed_datasource_ids.contains(ds_id) {
                TaskFate::Unknown
            } else {
                TaskFate::Deleted
            }
        })
        .context("notion: applying task sync")?;

    // ---- 7. Advance cursor --------------------------------------------------
    state.last_sync = Some(now.to_rfc3339());
    vault.write_notion_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("tasks", task_count);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} Notion task rows, {} raw pages", task_count, raw_new),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-notion-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-06-14T12:00:00-07:00")
            .unwrap()
            .with_timezone(&Local)
    }

    // --- fixtures (real Notion REST API 2026-03-11 shapes) ------------------

    /// A Notion data_source object as returned by POST /v1/search with
    /// filter.value="data_source".  Matches the live 2026-03-11 API shape:
    /// object="data_source", top-level `title` rich-text array.
    fn datasource_json(id: &str, title: &str) -> Value {
        serde_json::json!({
            "object": "data_source",
            "id": id,
            "title": [
                {
                    "type": "text",
                    "text": { "content": title, "link": null },
                    "plain_text": title,
                    "href": null
                }
            ],
            "created_time": "2026-01-01T00:00:00.000Z",
            "last_edited_time": "2026-06-01T00:00:00.000Z",
            "parent": { "type": "database_id", "database_id": "parent-db-1" },
            "properties": {
                "Name": {
                    "id": "title",
                    "type": "title",
                    "title": {}
                },
                "Status": {
                    "id": "status",
                    "type": "status",
                    "status": { "options": [], "groups": [] }
                },
                "Due": {
                    "id": "due",
                    "type": "date",
                    "date": {}
                }
            }
        })
    }

    /// A task page row returned by POST /v1/data_sources/{id}/query.
    /// parent.type="data_source_id" matches the real 2026-03-11 shape.
    fn task_row(id: &str, ds_id: &str, title: &str, status_name: &str, due_date: Option<&str>) -> Value {
        let due_val = due_date
            .map(|d| serde_json::json!({ "start": d, "end": null, "time_zone": null }))
            .unwrap_or(Value::Null);
        serde_json::json!({
            "object": "page",
            "id": id,
            "created_time": "2026-06-01T08:00:00.000Z",
            "last_edited_time": "2026-06-10T09:00:00.000Z",
            "parent": {
                "type": "data_source_id",
                "data_source_id": ds_id,
                "database_id": "parent-db-1"
            },
            "properties": {
                "Name": {
                    "id": "title",
                    "type": "title",
                    "title": [
                        {
                            "type": "text",
                            "text": { "content": title, "link": null },
                            "plain_text": title,
                            "href": null
                        }
                    ]
                },
                "Status": {
                    "id": "status_id",
                    "type": "status",
                    "status": { "id": "s1", "name": status_name, "color": "default" }
                },
                "Due": {
                    "id": "due_id",
                    "type": "date",
                    "date": due_val
                }
            }
        })
    }

    /// A checkbox-type task row (parent: data_source_id).
    fn checkbox_row(id: &str, ds_id: &str, title: &str, checked: bool) -> Value {
        serde_json::json!({
            "object": "page",
            "id": id,
            "created_time": "2026-06-02T10:00:00.000Z",
            "last_edited_time": "2026-06-02T10:00:00.000Z",
            "parent": {
                "type": "data_source_id",
                "data_source_id": ds_id,
                "database_id": "parent-db-1"
            },
            "properties": {
                "Task": {
                    "id": "title",
                    "type": "title",
                    "title": [
                        { "plain_text": title }
                    ]
                },
                "Done": {
                    "id": "done_id",
                    "type": "checkbox",
                    "checkbox": checked
                }
            }
        })
    }

    /// A standalone page (not in any data source).
    fn standalone_page(id: &str, title: &str) -> Value {
        serde_json::json!({
            "object": "page",
            "id": id,
            "created_time": "2026-05-15T10:00:00.000Z",
            "last_edited_time": "2026-05-15T10:00:00.000Z",
            "parent": { "type": "workspace", "workspace": true },
            "properties": {
                "title": {
                    "id": "title",
                    "type": "title",
                    "title": [
                        { "plain_text": title }
                    ]
                }
            }
        })
    }

    /// Wrap a list of results in the Notion paginated list envelope.
    fn page_of(results: Vec<Value>, next_cursor: Option<&str>) -> Value {
        serde_json::json!({
            "object": "list",
            "results": results,
            "next_cursor": next_cursor,
            "has_more": next_cursor.is_some()
        })
    }

    // --- mock API -----------------------------------------------------------

    struct MockApi {
        // path_prefix → queue of pages
        pages: RefCell<Vec<(String, VecDeque<Value>)>>,
        requests: RefCell<Vec<(String, Value)>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn register(&self, prefix: &str, responses: Vec<Value>) {
            self.pages
                .borrow_mut()
                .push((prefix.into(), VecDeque::from(responses)));
        }

        fn recorded_requests(&self) -> Vec<(String, Value)> {
            self.requests.borrow().clone()
        }
    }

    impl NotionApi for MockApi {
        fn post_json(&self, path: &str, body: &Value) -> Result<Page, FetchError> {
            self.requests.borrow_mut().push((path.to_string(), body.clone()));
            let mut pages = self.pages.borrow_mut();
            for (prefix, queue) in pages.iter_mut() {
                if path.starts_with(prefix.as_str()) || path.contains(prefix.as_str()) {
                    if let Some(v) = queue.pop_front() {
                        return Ok(parse_page(v));
                    }
                }
            }
            // Default empty response.
            Ok(parse_page(page_of(vec![], None)))
        }
    }

    /// Build a simple single-data-source mock (mirrors the real 2026-03-11 flow).
    /// The first /v1/search call returns the data_source discovery result;
    /// the second returns standalone pages. Rows come from /v1/data_sources/{id}/query.
    fn simple_mock(ds_id: &str, ds_title: &str, rows: Vec<Value>, pages: Vec<Value>) -> MockApi {
        let api = MockApi::new();
        api.register("/v1/search", vec![
            page_of(vec![datasource_json(ds_id, ds_title)], None), // data_source search
            page_of(pages, None),                                   // page search
        ]);
        api.register("/v1/data_sources", vec![page_of(rows, None)]);
        api
    }

    // --- pure mapping tests -------------------------------------------------

    #[test]
    fn extracts_title_from_title_type_property() {
        let row = task_row("p1", "ds1", "Ship the release", "In Progress", Some("2026-06-15"));
        let props = row["properties"].as_object().unwrap();
        assert_eq!(extract_title(props).as_deref(), Some("Ship the release"));
    }

    #[test]
    fn maps_status_open_and_done() {
        let mut ds_titles = HashMap::new();
        ds_titles.insert("ds1".to_string(), "Work".to_string());

        let open = task_from_page(&task_row("p1", "ds1", "Open task", "In Progress", None), &ds_titles).unwrap();
        assert_eq!(open.status, "open");

        let done = task_from_page(&task_row("p2", "ds1", "Done task", "Done", None), &ds_titles).unwrap();
        assert_eq!(done.status, "done");
    }

    #[test]
    fn maps_checkbox_done_flag() {
        let mut ds_titles = HashMap::new();
        ds_titles.insert("ds1".to_string(), "Checklists".to_string());

        let checked = task_from_page(&checkbox_row("c1", "ds1", "Buy milk", true), &ds_titles).unwrap();
        assert_eq!(checked.status, "done");

        let unchecked = task_from_page(&checkbox_row("c2", "ds1", "Clean kitchen", false), &ds_titles).unwrap();
        assert_eq!(unchecked.status, "open");
    }

    #[test]
    fn maps_due_date_and_all_day_flag() {
        let mut ds_titles = HashMap::new();
        ds_titles.insert("ds1".to_string(), "Tasks".to_string());

        // All-day date (≤10 chars).
        let t = task_from_page(&task_row("p1", "ds1", "All day", "In Progress", Some("2026-06-20")), &ds_titles).unwrap();
        assert!(t.all_day, "date-only → all_day=true");
        assert!(t.due.as_deref().unwrap().starts_with("2026-06-20"));

        // No due date.
        let no_due = task_from_page(&task_row("p2", "ds1", "No due", "In Progress", None), &ds_titles).unwrap();
        assert_eq!(no_due.due, None);
        assert!(!no_due.all_day);
    }

    #[test]
    fn project_set_from_datasource_title() {
        let mut ds_titles = HashMap::new();
        ds_titles.insert("ds1".to_string(), "Work".to_string());
        let t = task_from_page(&task_row("p1", "ds1", "My task", "Open", None), &ds_titles).unwrap();
        assert_eq!(t.project, "Work");
    }

    #[test]
    fn extra_carries_datasource_id_and_url() {
        let mut ds_titles = HashMap::new();
        ds_titles.insert("ds1".to_string(), "Work".to_string());
        let t = task_from_page(&task_row("p1", "ds1", "My task", "Open", None), &ds_titles).unwrap();
        assert_eq!(t.extra.get("datasource_id").and_then(Value::as_str), Some("ds1"));
        assert!(t.extra.get("url").and_then(Value::as_str).unwrap().contains("notion.so"));
    }

    #[test]
    fn standalone_page_not_a_task_row() {
        let page = standalone_page("s1", "My notes page");
        let props = page["properties"].as_object().unwrap();
        assert!(!looks_like_task(props), "standalone page without status/checkbox/due is not a task");
    }

    #[test]
    fn looks_like_task_detects_status_and_checkbox_and_due() {
        let status_row = task_row("p1", "ds1", "t", "Open", None);
        assert!(looks_like_task(status_row["properties"].as_object().unwrap()));
        let checkbox_r = checkbox_row("p2", "ds1", "t", false);
        assert!(looks_like_task(checkbox_r["properties"].as_object().unwrap()));
    }

    #[test]
    fn to_local_parses_notion_timestamps() {
        let ts = "2026-06-14T12:00:00.000Z";
        let local = to_local(ts);
        // The local form is valid RFC3339.
        assert!(DateTime::parse_from_rfc3339(&local).is_ok(), "local: {local}");
        // The instant is the same.
        let orig = DateTime::parse_from_rfc3339(ts).unwrap().timestamp();
        let got = DateTime::parse_from_rfc3339(&local).unwrap().timestamp();
        assert_eq!(orig, got);
    }

    // --- full pull tests ---------------------------------------------------

    #[test]
    fn full_pull_writes_tasks_and_raw_and_cursor() {
        let v = temp_vault("fullpull");
        let api = simple_mock(
            "ds1",
            "Work",
            vec![
                task_row("p1", "ds1", "Ship the release", "In Progress", Some("2026-06-15")),
                task_row("p2", "ds1", "Write tests", "Done", None),
            ],
            vec![],
        );

        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("tasks"), Some(&2));
        assert!(out.counts.get("raw").copied().unwrap_or(0) >= 2, "raw rows written");

        // Task snapshot in bound contract.
        let snap = v.load_tasks_snapshot("notion").unwrap();
        assert_eq!(snap.len(), 2, "two tasks in snapshot");
        let ship = snap.iter().find(|t| t.id == "p1").unwrap();
        assert_eq!(ship.project, "Work");
        assert_eq!(ship.status, "open");
        let done = snap.iter().find(|t| t.id == "p2").unwrap();
        assert_eq!(done.status, "done");

        // Raw firehose (partitioned by created_time month = 2026-06).
        assert!(v.root().join("notes/notion/raw/2026-06.jsonl").exists());
        let raw_content = std::fs::read_to_string(v.root().join("notes/notion/raw/2026-06.jsonl")).unwrap();
        assert!(raw_content.contains("Ship the release"));

        // Cursor advanced, no token in cursor.
        let state = v.read_notion_sync();
        assert!(state.last_sync.is_some());
        let cursor_body = std::fs::read_to_string(v.root().join(".trove/notion-sync.json")).unwrap();
        assert!(!cursor_body.contains("access_token"));
    }

    /// Asserts that the pull sends the correct 2026-03-11 API filter values:
    /// - data_source search filter (not "database")
    /// - data_sources query path (not /v1/databases/{id}/query)
    #[test]
    fn full_pull_uses_correct_2026_api_filter_values() {
        let v = temp_vault("apifilter");
        let api = simple_mock(
            "ds1",
            "Work",
            vec![task_row("p1", "ds1", "My task", "Open", None)],
            vec![],
        );

        pull_with(&v, &api, now()).unwrap();

        let reqs = api.recorded_requests();

        // First search call must use filter.value = "data_source" (not "database").
        let (search_path, search_body) = reqs.iter().find(|(p, _)| *p == "/v1/search").unwrap();
        assert_eq!(search_path, "/v1/search");
        let filter_val = search_body
            .get("filter")
            .and_then(|f| f.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // Must be "data_source" — "database" returns 400 under 2026-03-11.
        assert_eq!(filter_val, "data_source",
            "search filter must be 'data_source' under Notion-Version 2026-03-11, got: {filter_val:?}");

        // Data source query must go to /v1/data_sources/{id}/query (not /v1/databases/{id}/query).
        let ds_query = reqs.iter().find(|(p, _)| p.contains("/v1/data_sources/"));
        assert!(ds_query.is_some(),
            "pull must query /v1/data_sources/{{id}}/query; requests: {:?}",
            reqs.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>());
        let (query_path, _) = ds_query.unwrap();
        assert!(query_path.contains("ds1"), "query path must contain the data_source id");
        assert!(query_path.ends_with("/query"), "query path must end with /query");
        // Must NOT use the deprecated /v1/databases/{id}/query path.
        assert!(!query_path.contains("/v1/databases/"),
            "deprecated /v1/databases/{{id}}/query must not be used under 2026-03-11");
    }

    #[test]
    fn standalone_pages_land_in_raw_only_not_tasks() {
        let v = temp_vault("standalone");
        let api = MockApi::new();
        api.register("/v1/search", vec![
            page_of(vec![], None),                                  // data_source search
            page_of(vec![standalone_page("s1", "My notes")], None), // page search
        ]);

        let out = pull_with(&v, &api, now()).unwrap();
        // Standalone pages are not tasks.
        assert_eq!(out.counts.get("tasks"), Some(&0));
        // But they ARE in the raw layer.
        assert!(out.counts.get("raw").copied().unwrap_or(0) >= 1, "standalone page in raw");
        let snap = v.load_tasks_snapshot("notion").unwrap();
        assert!(snap.is_empty(), "no task rows from standalone pages");
    }

    #[test]
    fn resync_dedupes_raw_by_page_id() {
        let v = temp_vault("rawdedup");
        let api1 = simple_mock(
            "ds1",
            "Work",
            vec![task_row("p1", "ds1", "Task", "Open", None)],
            vec![],
        );
        let out1 = pull_with(&v, &api1, now()).unwrap();
        let raw1 = out1.counts.get("raw").copied().unwrap_or(0);
        assert!(raw1 >= 1, "first sync writes raw rows");

        // Re-sync the SAME page: no new raw rows.
        let api2 = simple_mock(
            "ds1",
            "Work",
            vec![task_row("p1", "ds1", "Task", "Open", None)],
            vec![],
        );
        let out2 = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out2.counts.get("raw"), Some(&0), "resync: no new raw ids");
        let raw_file = v.root().join("notes/notion/raw/2026-06.jsonl");
        let lines = std::fs::read_to_string(&raw_file).unwrap().lines().count();
        assert_eq!(lines, 1, "one deduped raw row");
    }

    #[test]
    fn task_then_deleted_is_logged_as_deleted_event() {
        let v = temp_vault("deleted");
        let api1 = simple_mock(
            "ds1",
            "Work",
            vec![task_row("p1", "ds1", "Vanishing task", "Open", None)],
            vec![],
        );
        pull_with(&v, &api1, now()).unwrap();
        assert_eq!(v.load_tasks_snapshot("notion").unwrap().len(), 1);

        // Second sync: the task is gone (data source queried OK but row absent).
        let api2 = simple_mock("ds1", "Work", vec![], vec![]);
        let out2 = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out2.counts.get("tasks"), Some(&0));

        // apply_tasks_sync should log a deleted event. Deletions are stamped
        // at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "p1"),
            "vanished task logged as deleted");
        assert!(v.load_tasks_snapshot("notion").unwrap().is_empty());
    }

    /// Tasks from a data source whose query fails are carried forward as Unknown
    /// (not deleted), preventing a deletion flood on transient 5xx/429 errors.
    #[test]
    fn failed_datasource_query_carries_tasks_forward_not_deleted() {
        let v = temp_vault("failedds");

        // First sync: establish a task in ds1.
        let api1 = simple_mock(
            "ds1",
            "Work",
            vec![task_row("p1", "ds1", "Important task", "Open", None)],
            vec![],
        );
        pull_with(&v, &api1, now()).unwrap();
        assert_eq!(v.load_tasks_snapshot("notion").unwrap().len(), 1);

        // Second sync: ds1 is discovered again but its query fails with a 500.
        let api2 = MockApi::new();
        api2.register("/v1/search", vec![
            page_of(vec![datasource_json("ds1", "Work")], None), // ds1 still discovered
            page_of(vec![], None),                               // page search
        ]);
        // Register the data_sources path to return an error.
        {
            // We inject an error response by not registering the path — the mock
            // returns empty for unknown paths. To simulate a real error we use
            // a sub-mock that explicitly returns FetchError::Other.
        }
        // Build a mock that simulates the data source query failure by using
        // a separate error-injecting mock.
        struct FailingDsApi {
            inner: MockApi,
        }
        impl NotionApi for FailingDsApi {
            fn post_json(&self, path: &str, body: &Value) -> Result<Page, FetchError> {
                if path.contains("/v1/data_sources/") && path.ends_with("/query") {
                    return Err(FetchError::Other("HTTP 500: internal server error".into()));
                }
                self.inner.post_json(path, body)
            }
        }
        let failing = FailingDsApi {
            inner: {
                let m = MockApi::new();
                m.register("/v1/search", vec![
                    page_of(vec![datasource_json("ds1", "Work")], None),
                    page_of(vec![], None),
                ]);
                m
            },
        };

        let out2 = pull_with(&v, &failing, now()).unwrap();
        // No new task rows from the failed query, but...
        assert_eq!(out2.counts.get("tasks"), Some(&0));

        // ...the task must be CARRIED FORWARD (Unknown fate), not deleted.
        let snap = v.load_tasks_snapshot("notion").unwrap();
        assert_eq!(snap.len(), 1, "task must survive a failed data source query");
        assert_eq!(snap[0].id, "p1");

        // No deleted event should be logged.
        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        assert!(!events.iter().any(|e| e.kind == "deleted" && e.task.id == "p1"),
            "task must not be logged as deleted when its data source query failed");
    }

    #[test]
    fn multi_page_pagination_is_drained() {
        let v = temp_vault("pagination");
        let api = MockApi::new();
        // Data source search: two pages of results.
        api.register("/v1/search", vec![
            // First call (data_source search): page 1 with cursor, page 2.
            page_of(vec![datasource_json("ds1", "Work")], Some("c1")),
            page_of(vec![], None),
            // Second call (page search): no standalone pages.
            page_of(vec![], None),
        ]);
        // Data source query: two pages of rows.
        api.register("/v1/data_sources", vec![
            page_of(vec![task_row("p1", "ds1", "Task A", "Open", None)], Some("r1")),
            page_of(vec![task_row("p2", "ds1", "Task B", "Open", None)], None),
        ]);

        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("tasks"), Some(&2), "both pages of tasks fetched");
    }

    #[test]
    fn connection_stores_token_and_status_reflects_it() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "ntn_test_token".into(),
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
        assert_eq!(status.accounts[0].label, "Notion");

        def_disconnect(&v, "notion").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_token_rejected() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_sync.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_sync":"2026-06-01T00:00:00-07:00"}"#).unwrap();
        assert_eq!(partial.last_sync.as_deref(), Some("2026-06-01T00:00:00-07:00"));
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "notion");
    }
}
