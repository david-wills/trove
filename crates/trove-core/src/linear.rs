//! Linear — engineering issue tracker via the official GraphQL API.
//!
//! Pulls issues assigned to the authenticated user into the bound
//! [`crate::tasks`] contract. Two destinations are written in one pass:
//!
//! - **tasks contract** under `tasks/linear/` — snapshot + event stream via
//!   [`crate::tasks::apply_tasks_sync`], exactly like the Todoist/Asana legs.
//! - **raw firehose** under `tasks/linear/raw/YYYY-MM.jsonl` — the full
//!   GraphQL issue nodes (with comments, labels, cycle/project refs),
//!   partitioned by `createdAt` month, upserted by `id`.
//!
//! # Auth
//!
//! Personal API key from Linear → Settings → Account → Security & Access →
//! Personal API keys. No app registration needed. Sent as the `Authorization`
//! header (bare key, not `Bearer`). Stored in `.trove/sync/linear` (0600) via
//! the never-expiring [`crate::sync::oauth::TokenSet`] pattern (Todoist
//! precedent).
//!
//! # API
//!
//! GraphQL: `POST https://api.linear.app/graphql`. Pagination follows the
//! connection pattern: each page returns `pageInfo { hasNextPage endCursor }`
//! and items under `nodes`. Subsequent pages pass `after: "<endCursor>"`.
//! Rate limit is ~100–300 requests/min per token, trivially fine here.
//!
//! Two queries per pull:
//! 1. `me { assignedIssues(filter: { state: { type: { in: ["backlog","unstarted","started","triage"] } } }) }`
//!    — always fetches the FULL current open set (no watermark filter, so no false-deletes).
//! 2. `me { assignedIssues(filter: { updatedAt: { gte: $since } }) }` restricted to
//!    completed/canceled issues — builds the fate map for apply_tasks_sync.
//!    Skipped on first sync (no watermark); completed issues from that run are
//!    handled by the open-set absence logic.
//!
//! Cursor watermark: the maximum `updatedAt` seen across both queries is
//! persisted in `.trove/linear-sync.json` (non-secret, rebuildable).
//!
//! ## Linear priority values (documented schema)
//!
//! 0 = No priority, 1 = Urgent, 2 = High, 3 = Normal, 4 = Low.
//! Mapped onto the contract scale (0 none, 1 low, 3 medium, 5 high).
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/linear.md.

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
const SOURCE: &str = "linear";

/// Raw firehose directory (full-fidelity GraphQL issue nodes).
const RAW_DIR: &str = "tasks/linear/raw";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that is 0600).
const SYNC_FILE: &str = ".trove/linear-sync.json";

/// The service id under `.trove/sync/` where the API key is stored (0600).
const SERVICE: &str = "linear";

const API_URL: &str = "https://api.linear.app/graphql";
/// Kept short so a hung connection cannot stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs: every 15 min, matching the other task sources.
pub const LINEAR_SYNC_SECS: u64 = 900;

/// Page size for GraphQL queries (nodes per page).
const PAGE_SIZE: u32 = 50;

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
                    "linear synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "linear sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Linear synced — {} open issues, {} completed, {} deleted",
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
        id: "linear",
        name: "Linear",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls issues assigned to you from Linear into the unified task store \
                      every 15 minutes. Connect with a personal API key from Linear Settings → \
                      Account → Security & Access.",
        domain: "tasks",
        vault_path: "tasks/linear/",
        toggleable: true,
        setup: &[
            "Connect with your Linear personal API key on this card.",
            "Each sync captures all issues assigned to you and tracks completions and cancellations.",
        ],
        caveats: "Only pulls issues assigned to you (the `me { assignedIssues }` query). \
                  Completion history accrues while the sync runs; issues completed before \
                  the first sync are captured on the initial pull.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(LINEAR_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("linear"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a personal API key, a SECRET).

/// Verify the pasted key with a lightweight `me { id }` query, then store
/// it (0600). A 401 bails with a clear reconnect message; the key is never
/// logged.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Linear personal API key");
    }
    let client = LinearClient::new(API_URL.to_string(), token.to_string());
    let verify_query = r#"{"query":"{ me { id name } }"}"#;
    match client.graphql(verify_query) {
        Ok(v) => {
            // A successful response has a `data` key; errors have `errors`.
            if v.get("errors").is_some() && v.get("data").is_none() {
                bail!(
                    "Linear rejected the API key — check it's a valid personal API key from \
                     Settings → Account → Security & Access and hasn't been revoked"
                );
            }
        }
        Err(FetchError::Unauthorized) => bail!(
            "Linear rejected the API key (401) — check it's a valid personal API key from \
             Settings → Account → Security & Access and hasn't been revoked"
        ),
        Err(e) => bail!("Linear API check failed: {e}"),
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
            label: "Linear".to_string(),
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
    id: "linear",
    display_name: "Linear",
    methods: &[ConnectMethod::TokenPaste {
        label: "Linear personal API key",
        help: "Paste your Linear personal API key from Settings → Account → \
               Security & Access → Personal API keys.",
        placeholder: "lin_api_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["linear"],
    setup: &[
        "In Linear, open Settings → Account → Security & Access.",
        "Under \"Personal API keys\", click \"Create key\" and copy the generated key.",
        "Paste it here — it is stored locally and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

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

/// One page of assigned issues from the GraphQL query.
struct IssuePage {
    nodes: Vec<Value>,
    has_next_page: bool,
    end_cursor: Option<String>,
}

/// The API surface the pull needs. A trait so tests run fully offline.
trait LinearApi {
    /// POST a raw JSON body (already serialized GraphQL query) and return the
    /// parsed response value (the whole JSON body). The caller extracts `data`.
    fn graphql(&self, body: &str) -> Result<Value, FetchError>;
}

/// Thin ureq client.
struct LinearClient {
    url: String,
    token: String,
}

impl LinearClient {
    fn new(url: String, token: String) -> Self {
        LinearClient { url, token }
    }

    fn handle(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<Value, FetchError> {
        match resp {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
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

impl LinearApi for LinearClient {
    fn graphql(&self, body: &str) -> Result<Value, FetchError> {
        LinearClient::handle(
            ureq::post(&self.url)
                .timeout(HTTP_TIMEOUT)
                // Linear personal API key: sent as bare `Authorization` header (no Bearer prefix).
                .set("Authorization", &self.token)
                .set("Content-Type", "application/json")
                .send_string(body),
        )
    }
}

/// Build the GraphQL query body for one page of OPEN assigned issues.
///
/// Fetches issues whose state type is in the open set (backlog, unstarted,
/// started, triage). No watermark filter — always returns the FULL current open
/// set to avoid false-deletes on incremental syncs.
/// `cursor` is an optional `after` cursor for pagination.
fn open_issues_query(cursor: Option<&str>) -> String {
    let after = match cursor {
        Some(c) => format!(r#", after: "{c}""#),
        None => String::new(),
    };
    // Full issue shape including comments and labels for the raw layer.
    format!(
        r#"{{
  "query": "{{
    me {{
      assignedIssues(first: {PAGE_SIZE}, filter: {{ state: {{ type: {{ in: [\"backlog\", \"unstarted\", \"started\", \"triage\"] }} }} }}{after}) {{
        pageInfo {{
          hasNextPage
          endCursor
        }}
        nodes {{
          id
          identifier
          title
          priority
          dueDate
          completedAt
          canceledAt
          createdAt
          updatedAt
          url
          branchName
          state {{
            name
            type
          }}
          project {{
            id
            name
          }}
          cycle {{
            id
            name
            number
          }}
          labels {{
            nodes {{
              name
            }}
          }}
          comments {{
            nodes {{
              body
              createdAt
              user {{
                name
              }}
            }}
          }}
        }}
      }}
    }}
  }}"
}}"#
    )
}

/// Build the GraphQL query body for one page of recently COMPLETED/CANCELED
/// assigned issues. Uses the watermark `since` (RFC3339) to bound the query via
/// `updatedAt: { gte: … }`, so only issues that transitioned to done since the
/// last pull are returned. The results are used exclusively to build the fate
/// map — they are never put in the open-task `fresh` list.
/// `cursor` is an optional `after` cursor for pagination.
fn completed_issues_query(since: &str, cursor: Option<&str>) -> String {
    let after = match cursor {
        Some(c) => format!(r#", after: "{c}""#),
        None => String::new(),
    };
    format!(
        r#"{{
  "query": "{{
    me {{
      assignedIssues(first: {PAGE_SIZE}, filter: {{ updatedAt: {{ gte: \"{since}\" }}, state: {{ type: {{ in: [\"completed\", \"canceled\"] }} }} }}{after}) {{
        pageInfo {{
          hasNextPage
          endCursor
        }}
        nodes {{
          id
          completedAt
          canceledAt
          updatedAt
          state {{
            type
          }}
        }}
      }}
    }}
  }}"
}}"#
    )
}

/// Parse one page of the `me { assignedIssues { ... } }` response.
fn parse_issue_page(v: &Value) -> IssuePage {
    let conn = v
        .get("data")
        .and_then(|d| d.get("me"))
        .and_then(|me| me.get("assignedIssues"));
    let nodes = conn
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let has_next_page = conn
        .and_then(|c| c.get("pageInfo"))
        .and_then(|pi| pi.get("hasNextPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let end_cursor = conn
        .and_then(|c| c.get("pageInfo"))
        .and_then(|pi| pi.get("endCursor"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    IssuePage { nodes, has_next_page, end_cursor }
}

/// Drain all pages of assigned issues updated since `since` (if given).
/// Returns all nodes collected.
/// Drain all pages of OPEN assigned issues (no watermark filter).
/// Returns the full current open set.
fn drain_open_issues(api: &impl LinearApi) -> Result<Vec<Value>, FetchError> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let body = open_issues_query(cursor.as_deref());
        let resp = api.graphql(&body)?;
        // Surface GraphQL-level errors as a fetch error.
        if let Some(errs) = resp.get("errors") {
            return Err(FetchError::Other(format!("GraphQL errors: {errs}")));
        }
        let page = parse_issue_page(&resp);
        all.extend(page.nodes);
        if !page.has_next_page {
            break;
        }
        cursor = page.end_cursor;
        if cursor.is_none() {
            break; // defensive: no cursor to follow
        }
    }
    Ok(all)
}

/// Drain all pages of recently COMPLETED/CANCELED assigned issues updated since
/// `since`. Used only to build the fate map on incremental syncs.
fn drain_completed_issues(api: &impl LinearApi, since: &str) -> Result<Vec<Value>, FetchError> {
    let mut all = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let body = completed_issues_query(since, cursor.as_deref());
        let resp = api.graphql(&body)?;
        if let Some(errs) = resp.get("errors") {
            return Err(FetchError::Other(format!("GraphQL errors: {errs}")));
        }
        let page = parse_issue_page(&resp);
        all.extend(page.nodes);
        if !page.has_next_page {
            break;
        }
        cursor = page.end_cursor;
        if cursor.is_none() {
            break; // defensive: no cursor to follow
        }
    }
    Ok(all)
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The max `updatedAt` (UTC RFC3339) seen across all issues in the last
    /// successful pull. Used as the `updatedAt_gte` filter on the next pull
    /// so only changed issues are re-fetched. Not a secret — deleting this
    /// widens the window on the next pull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_updated_at: Option<String>,
}

impl Vault {
    fn read_linear_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_linear_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape — the full-fidelity GraphQL issue node.

/// One raw issue node in `tasks/linear/raw/YYYY-MM.jsonl`.
/// Written verbatim (no synthetic keys added), partitioned by `createdAt`,
/// upserted by `id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawIssue {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawIssue {
    /// The issue id (stable dedup key).
    fn guid(&self) -> String {
        self.fields
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// `createdAt` — partition key for the monthly raw files.
    fn created_at(&self) -> &str {
        self.fields
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

/// Build a `RawIssue` from an API node. `None` when `id` or `createdAt` is
/// absent (can't dedup or partition).
fn raw_issue(value: &Value) -> Option<RawIssue> {
    let obj = value.as_object()?;
    obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    obj.get("createdAt").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    Some(RawIssue { fields: obj.clone() })
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// An RFC3339 timestamp → RFC3339 local time.
/// Unparseable values pass through verbatim (the todoist/asana pattern).
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

/// Map a Linear issue priority integer onto the task contract scale.
///
/// Linear: 0 = No priority, 1 = Urgent, 2 = High, 3 = Medium, 4 = Low.
/// Contract: 0 = none, 1 = low, 3 = medium, 5 = high (TickTick scale).
/// Urgent maps to 5 (high, the same ceiling), since the contract has no
/// "urgent" tier; the raw `linear_priority` in `extra` preserves it.
fn map_priority(p: i64) -> i64 {
    match p {
        1 => 5, // Urgent → high
        2 => 5, // High → high
        3 => 3, // Normal → medium
        4 => 1, // Low → low
        _ => 0, // 0 (No priority) and unexpected
    }
}

/// Determine if the issue state type means it is completed or canceled.
///
/// Linear's WorkflowState.type enum uses US spelling: "canceled" (one L),
/// not "cancelled" (two L). See Linear GraphQL schema.
fn is_done_state(state_type: &str) -> bool {
    matches!(state_type, "completed" | "canceled")
}

/// Map a raw GraphQL issue node → normalized [`Task`].
/// Returns `None` when the node has no `id` or `title`.
fn task_from_value(value: &Value) -> Option<Task> {
    let obj = value.as_object()?;
    let id = obj.get("id").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let title = str_opt(value, "title")?;

    // State: name + type.
    let state_name = obj
        .get("state")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_string();
    let state_type = obj
        .get("state")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let is_done = is_done_state(&state_type);

    // Project name.
    let project = obj
        .get("project")
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // Labels: collect `name` from each `labels.nodes[].name`.
    let tags: Vec<String> = obj
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    // Due date (date string like "2026-06-15", no time component in Linear).
    let due = str_opt(value, "dueDate").map(|s| to_local(&s));
    let all_day = due.is_some(); // Linear only has date-level due dates, no time

    // Completion time.
    let completed_at = str_opt(value, "completedAt")
        .or_else(|| str_opt(value, "canceledAt"))
        .map(|s| to_local(&s));

    let linear_priority = obj.get("priority").and_then(Value::as_i64).unwrap_or(0);

    // Source-specific extras.
    let mut extra = Map::new();

    // Linear identifier (e.g. "ENG-123") — the human-readable issue number.
    if let Some(identifier) = str_opt(value, "identifier") {
        extra.insert("identifier".into(), Value::String(identifier));
    }
    if let Some(url) = str_opt(value, "url") {
        extra.insert("url".into(), Value::String(url));
    }
    if let Some(branch) = str_opt(value, "branchName") {
        extra.insert("branch_name".into(), Value::String(branch));
    }
    extra.insert("linear_priority".into(), Value::from(linear_priority));
    extra.insert("state_name".into(), Value::String(state_name.clone()));
    extra.insert("state_type".into(), Value::String(state_type));

    // Project id for fate guard (parallel to Asana's workspace_gid).
    if let Some(project_id) = obj
        .get("project")
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        extra.insert("project_id".into(), Value::String(project_id.to_string()));
    }

    // Cycle info.
    if let Some(cycle) = obj.get("cycle").filter(|c| !c.is_null()) {
        if let Some(cycle_name) = cycle.get("name").and_then(Value::as_str) {
            extra.insert("cycle_name".into(), Value::String(cycle_name.to_string()));
        }
        if let Some(cycle_num) = cycle.get("number").and_then(Value::as_i64) {
            extra.insert("cycle_number".into(), Value::from(cycle_num));
        }
    }

    // Comments: keep as an array in extra (full fidelity).
    if let Some(comments) = obj
        .get("comments")
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
    {
        if !comments.is_empty() {
            extra.insert("comments".into(), Value::Array(comments.clone()));
        }
    }

    Some(Task {
        source: SOURCE.into(),
        id,
        title,
        project,
        notes: String::new(), // Linear issues have no separate notes field in this query
        // Contract enum: "open" or "done". The rich Linear state name is
        // preserved in extra["state_name"] for display / filtering.
        status: if is_done { "done".into() } else { "open".into() },
        priority: map_priority(linear_priority),
        due,
        start: None,
        all_day,
        recurrence: None,
        tags,
        subtasks: Vec::new(),
        created: str_opt(value, "createdAt").map(|s| to_local(&s)),
        modified: str_opt(value, "updatedAt").map(|s| to_local(&s)),
        completed: completed_at,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition (the todoist/asana idiom).

fn upsert_raw(vault: &Vault, rows: Vec<RawIssue>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawIssue>> = BTreeMap::new();
    for r in rows {
        let created = r.created_at().to_string();
        let key = Partition::Month
            .key(&created)
            .with_context(|| format!("linear: raw issue createdAt {created:?} has no month"))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<RawIssue> = stream.read(&month)?;
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
// Fate resolution.

/// Resolve a task that vanished from the open set.
///
/// Linear returns all assigned issues in the `updatedAt_gte` window
/// regardless of state — completed/cancelled issues arrive with a non-null
/// `completedAt` or `canceledAt`. We collect those in a map (id → completed_at)
/// before calling `apply_tasks_sync`.
///
/// - In `completed_map` → `Completed(its time)`.
/// - Not in `completed_map` but the pull succeeded → `Deleted`
///   (unassigned or deleted from Linear).
/// - `completed_map` is `None` (the pull itself failed) → `Unknown`
///   (carry-forward).
fn linear_fate(
    task: &Task,
    completed_map: Option<&HashMap<String, Option<String>>>,
) -> TaskFate {
    match completed_map {
        Some(map) => match map.get(&task.id) {
            Some(Some(when)) => TaskFate::Completed(Some(when.clone())),
            Some(None) => TaskFate::Completed(None),
            None => TaskFate::Deleted,
        },
        None => TaskFate::Unknown,
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync. Missing token ⇒ quiet skip on the periodic
/// path (mirror todoist/asana), clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Linear is not connected — add your API key in the Integrations tab")?;
    let client = LinearClient::new(API_URL.to_string(), token);
    pull_with(vault, &client, Local::now())
}

/// The pull body over an injected API + clock — the testable seam.
fn pull_with(vault: &Vault, api: &impl LinearApi, _now: DateTime<Local>) -> Result<PullOutcome> {
    let mut state = vault.read_linear_sync();

    // Query 1: fetch the FULL current open assigned-issue set (no watermark).
    // This is always unfiltered so that open issues that haven't changed since
    // the last sync are still returned — avoiding false-deletes.
    let open_nodes = drain_open_issues(api).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Linear rejected the API key (401) — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Linear fetch failed: {other}"),
    })?;

    // Query 2 (incremental only): fetch recently completed/canceled issues since
    // the watermark to build the fate map.
    let since = state.last_updated_at.as_deref();
    let completed_nodes: Vec<Value> = if let Some(wm) = since {
        drain_completed_issues(api, wm).map_err(|e| match e {
            FetchError::Unauthorized => anyhow::anyhow!(
                "Linear rejected the API key (401) — reconnect from the Integrations tab"
            ),
            other => anyhow::anyhow!("Linear fetch failed (completed query): {other}"),
        })?
    } else {
        // First sync: no watermark, so no completed query. Any issues that are
        // already completed simply won't appear in the open set, which is correct.
        Vec::new()
    };

    let mut all_raw: Vec<RawIssue> = Vec::new();
    let mut fresh: Vec<Task> = Vec::new();
    let mut completed_map: HashMap<String, Option<String>> = HashMap::new();
    // Track the max updatedAt seen for the new watermark.
    let mut max_updated_at: Option<String> = None;

    // Process open issues into fresh tasks and raw firehose.
    for node in &open_nodes {
        if let Some(r) = raw_issue(node) {
            all_raw.push(r);
        }

        // Advance the updatedAt watermark.
        if let Some(updated) = node.get("updatedAt").and_then(Value::as_str) {
            match &max_updated_at {
                None => max_updated_at = Some(updated.to_string()),
                Some(cur) if updated > cur.as_str() => {
                    max_updated_at = Some(updated.to_string())
                }
                _ => {}
            }
        }

        if let Some(t) = task_from_value(node) {
            fresh.push(t);
        }
    }

    // Process completed/canceled issues into the fate map and raw firehose.
    for node in &completed_nodes {
        if let Some(r) = raw_issue(node) {
            all_raw.push(r);
        }

        // Advance the updatedAt watermark from completed nodes too.
        if let Some(updated) = node.get("updatedAt").and_then(Value::as_str) {
            match &max_updated_at {
                None => max_updated_at = Some(updated.to_string()),
                Some(cur) if updated > cur.as_str() => {
                    max_updated_at = Some(updated.to_string())
                }
                _ => {}
            }
        }

        let obj = match node.as_object() {
            Some(o) => o,
            None => continue,
        };
        let id = match obj.get("id").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => continue,
        };
        let state_type = obj
            .get("state")
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");

        // Only record in completed_map if it's truly done (safety guard, query
        // should already filter to completed/canceled state types).
        if is_done_state(state_type) {
            let completed_at = obj
                .get("completedAt")
                .or_else(|| obj.get("canceledAt"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(to_local);
            completed_map.insert(id, completed_at);
        }
    }

    let raw_new = upsert_raw(vault, all_raw)?;

    // Projects for the markdown index: collect distinct project names from
    // fresh tasks. We don't have a project list endpoint, so derive from the
    // tasks themselves.
    let mut project_set: BTreeMap<String, String> = BTreeMap::new();
    for t in &fresh {
        if !t.project.is_empty() {
            let proj_id = t
                .extra
                .get("project_id")
                .and_then(Value::as_str)
                .unwrap_or(&t.project)
                .to_string();
            project_set.insert(proj_id, t.project.clone());
        }
    }
    // Also include an "Inbox" project for unassigned issues.
    project_set.entry("_inbox".to_string()).or_insert_with(|| "Inbox".to_string());

    let projects: Vec<ProjectInfo> = project_set
        .into_iter()
        .map(|(id, name)| ProjectInfo { id, name })
        .collect();

    // Gate the fate map: if the pull returned zero nodes and we have no
    // watermark (first sync with an empty account) keep an empty-but-Some map
    // so we don't spuriously carry forward tasks on a truly empty first pull.
    // If we got nodes (non-empty pull), we have a full picture.
    let fate_map = Some(completed_map);

    let stats = vault
        .apply_tasks_sync(SOURCE, &projects, fresh, |t| {
            linear_fate(t, fate_map.as_ref())
        })
        .context("linear: applying task sync")?;

    // Advance the watermark only after a successful drain.
    if let Some(new_wm) = max_updated_at {
        // Only advance if the new watermark is newer (or there's no prior one).
        let advance = match &state.last_updated_at {
            None => true,
            Some(cur) => new_wm.as_str() > cur.as_str(),
        };
        if advance {
            state.last_updated_at = Some(new_wm);
        }
    }
    vault.write_linear_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open Linear issues", stats.open),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-linear-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-06-15T12:00:00-07:00")
            .unwrap()
            .with_timezone(&Local)
    }

    // --- fixtures (Linear GraphQL schema — confirmed field names) -----------

    /// An open issue with a cycle, project, labels, a comment, and a due date.
    fn issue_open(id: &str, project_id: &str) -> Value {
        json!({
            "id": id,
            "identifier": "ENG-42",
            "title": "Ship the feature",
            "priority": 2,
            "dueDate": "2026-06-20",
            "completedAt": null,
            "canceledAt": null,
            "createdAt": "2026-06-01T08:00:00.000Z",
            "updatedAt": "2026-06-10T09:00:00.000Z",
            "url": "https://linear.app/team/issue/ENG-42",
            "branchName": "eng-42-ship-the-feature",
            "state": {
                "name": "In Progress",
                "type": "started"
            },
            "project": {
                "id": project_id,
                "name": "Q2 Roadmap"
            },
            "cycle": {
                "id": "cycle1",
                "name": "Sprint 5",
                "number": 5
            },
            "labels": {
                "nodes": [
                    { "name": "backend" },
                    { "name": "urgent" }
                ]
            },
            "comments": {
                "nodes": [
                    {
                        "body": "Looking good, almost done",
                        "createdAt": "2026-06-09T14:00:00.000Z",
                        "user": { "name": "Alice" }
                    }
                ]
            }
        })
    }

    /// A completed issue.
    fn issue_completed(id: &str, completed_at: &str) -> Value {
        json!({
            "id": id,
            "identifier": "ENG-41",
            "title": "Fix the bug",
            "priority": 1,
            "dueDate": null,
            "completedAt": completed_at,
            "canceledAt": null,
            "createdAt": "2026-06-01T07:00:00.000Z",
            "updatedAt": completed_at,
            "url": "https://linear.app/team/issue/ENG-41",
            "branchName": "eng-41-fix-the-bug",
            "state": {
                "name": "Done",
                "type": "completed"
            },
            "project": null,
            "cycle": null,
            "labels": { "nodes": [] },
            "comments": { "nodes": [] }
        })
    }

    /// A cancelled issue.
    fn issue_cancelled(id: &str, canceled_at: &str) -> Value {
        json!({
            "id": id,
            "identifier": "ENG-40",
            "title": "Abandoned feature",
            "priority": 0,
            "dueDate": null,
            "completedAt": null,
            "canceledAt": canceled_at,
            "createdAt": "2026-05-01T07:00:00.000Z",
            "updatedAt": canceled_at,
            "url": "https://linear.app/team/issue/ENG-40",
            "branchName": null,
            "state": {
                "name": "Cancelled",
                "type": "canceled"
            },
            "project": null,
            "cycle": null,
            "labels": { "nodes": [] },
            "comments": { "nodes": [] }
        })
    }

    /// Helper to build the full GraphQL response envelope.
    fn gql_response(nodes: Vec<Value>, has_next_page: bool, end_cursor: Option<&str>) -> Value {
        json!({
            "data": {
                "me": {
                    "assignedIssues": {
                        "pageInfo": {
                            "hasNextPage": has_next_page,
                            "endCursor": end_cursor
                        },
                        "nodes": nodes
                    }
                }
            }
        })
    }

    // --- mock API -----------------------------------------------------------

    struct MockApi {
        responses: RefCell<std::collections::VecDeque<Value>>,
    }

    impl MockApi {
        fn new(responses: Vec<Value>) -> Self {
            MockApi {
                responses: RefCell::new(responses.into()),
            }
        }
    }

    impl LinearApi for MockApi {
        fn graphql(&self, _body: &str) -> Result<Value, FetchError> {
            match self.responses.borrow_mut().pop_front() {
                Some(v) => Ok(v),
                None => Ok(json!({"data": {"me": {"assignedIssues": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []}}}})),
            }
        }
    }

    // --- parse_issue_page tests --------------------------------------------

    #[test]
    fn parses_page_with_nodes_and_cursor() {
        let resp = gql_response(
            vec![issue_open("I1", "p1")],
            true,
            Some("CURSOR2"),
        );
        let page = parse_issue_page(&resp);
        assert_eq!(page.nodes.len(), 1);
        assert!(page.has_next_page);
        assert_eq!(page.end_cursor.as_deref(), Some("CURSOR2"));
    }

    #[test]
    fn parses_last_page_with_no_cursor() {
        let resp = gql_response(vec![issue_open("I1", "p1")], false, None);
        let page = parse_issue_page(&resp);
        assert!(!page.has_next_page);
        assert!(page.end_cursor.is_none());
    }

    #[test]
    fn parses_empty_response_gracefully() {
        let empty = json!({"data": {"me": {"assignedIssues": {"pageInfo": {"hasNextPage": false}, "nodes": null}}}});
        let page = parse_issue_page(&empty);
        assert!(page.nodes.is_empty());
        assert!(!page.has_next_page);
    }

    // --- priority mapping --------------------------------------------------

    #[test]
    fn priority_mapping_covers_all_linear_values() {
        assert_eq!(map_priority(0), 0, "No priority → none");
        assert_eq!(map_priority(1), 5, "Urgent → high");
        assert_eq!(map_priority(2), 5, "High → high");
        assert_eq!(map_priority(3), 3, "Medium → medium");
        assert_eq!(map_priority(4), 1, "Low → low");
        assert_eq!(map_priority(99), 0, "unknown → none");
    }

    // --- task mapping tests ------------------------------------------------

    #[test]
    fn maps_open_issue_fields_correctly() {
        let t = task_from_value(&issue_open("I1", "p1")).unwrap();
        assert_eq!(t.source, SOURCE);
        assert_eq!(t.id, "I1");
        assert_eq!(t.title, "Ship the feature");
        assert_eq!(t.project, "Q2 Roadmap");
        assert_eq!(t.status, "open", "open issue → contract status \"open\"");
        assert_eq!(t.priority, 5, "Linear priority 2 (High) → contract 5");
        assert_eq!(t.tags, vec!["backend", "urgent"]);
        assert!(t.all_day, "Linear only has date-level due dates");
        assert!(t.due.as_deref().unwrap().starts_with("2026-06-20"), "due date preserved");
        assert!(t.completed.is_none(), "open issue: no completion");
        assert!(t.created.is_some(), "createdAt mapped");
        assert!(t.modified.is_some(), "updatedAt mapped");

        // Extra fields.
        assert_eq!(
            t.extra.get("identifier").and_then(Value::as_str),
            Some("ENG-42"),
        );
        assert_eq!(t.extra.get("linear_priority").and_then(Value::as_i64), Some(2));
        assert_eq!(t.extra.get("state_type").and_then(Value::as_str), Some("started"));
        assert!(t.extra.contains_key("url"));
        assert_eq!(t.extra.get("branch_name").and_then(Value::as_str), Some("eng-42-ship-the-feature"));
        assert_eq!(t.extra.get("cycle_number").and_then(Value::as_i64), Some(5));
        assert_eq!(t.extra.get("cycle_name").and_then(Value::as_str), Some("Sprint 5"));
        assert!(t.extra.contains_key("comments"), "comments in extra");
        let comments = t.extra.get("comments").and_then(Value::as_array).unwrap();
        assert_eq!(comments.len(), 1);
    }

    #[test]
    fn completed_issue_has_done_status_and_no_mapping_as_open_task() {
        // Completed issues go into the fate map, not the fresh tasks list.
        // But task_from_value can still parse them.
        let t = task_from_value(&issue_completed("I2", "2026-06-13T16:30:00.000Z")).unwrap();
        assert_eq!(t.status, "done");
        // completed maps to completedAt.
        let completed = t.completed.as_deref().unwrap();
        let ts = DateTime::parse_from_rfc3339(completed).unwrap().timestamp();
        let expected = DateTime::parse_from_rfc3339("2026-06-13T16:30:00.000Z").unwrap().timestamp();
        assert_eq!(ts, expected);
        assert_eq!(t.priority, 5, "Urgent (1) → high");
        assert!(t.project.is_empty(), "null project → empty string");
    }

    #[test]
    fn cancelled_issue_maps_to_done_and_uses_canceled_at() {
        let t = task_from_value(&issue_cancelled("I3", "2026-06-10T10:00:00.000Z")).unwrap();
        assert_eq!(t.status, "done");
        assert!(t.completed.is_some(), "canceledAt mapped to completed");
        assert_eq!(t.priority, 0, "No priority (0) → none");
    }

    #[test]
    fn issue_without_project_gets_empty_project() {
        let no_project = json!({
            "id": "I4",
            "identifier": "ENG-1",
            "title": "Orphan task",
            "priority": 3,
            "dueDate": null,
            "completedAt": null,
            "canceledAt": null,
            "createdAt": "2026-06-01T08:00:00.000Z",
            "updatedAt": "2026-06-01T08:00:00.000Z",
            "url": "https://linear.app/team/issue/ENG-1",
            "branchName": null,
            "state": { "name": "Todo", "type": "unstarted" },
            "project": null,
            "cycle": null,
            "labels": { "nodes": [] },
            "comments": { "nodes": [] }
        });
        let t = task_from_value(&no_project).unwrap();
        assert_eq!(t.project, "", "null project → empty string");
        assert!(t.extra.get("project_id").is_none(), "no project_id in extra");
    }

    #[test]
    fn raw_issue_roundtrips_full_fidelity() {
        let r = raw_issue(&issue_open("I1", "p1")).unwrap();
        assert_eq!(r.guid(), "I1");
        assert_eq!(r.created_at(), "2026-06-01T08:00:00.000Z");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("\"guid\""), "no synthetic guid column on disk");
        let back: RawIssue = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r, "round-trips identically");
        assert!(line.contains("\"branchName\""), "raw keeps all source fields");
        assert!(line.contains("\"comments\""), "comments in raw layer");
    }

    // --- drain pagination --------------------------------------------------

    #[test]
    fn drains_all_pages_following_end_cursor() {
        let api = MockApi::new(vec![
            gql_response(vec![issue_open("I1", "p1")], true, Some("CUR2")),
            gql_response(vec![issue_open("I2", "p1")], false, None),
        ]);
        let nodes = drain_open_issues(&api).unwrap();
        assert_eq!(nodes.len(), 2, "both pages drained");
    }

    // --- full pull tests ---------------------------------------------------

    #[test]
    fn full_pull_writes_contract_raw_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(vec![
            gql_response(vec![issue_open("I1", "p1"), issue_open("I2", "p1")], false, None),
        ]);
        let out = pull_with(&v, &api, now()).unwrap();

        assert_eq!(out.counts.get("open"), Some(&2));
        assert_eq!(out.counts.get("created"), Some(&2), "first sync: 2 creations");
        assert_eq!(out.counts.get("raw"), Some(&2));

        // Contract snapshot.
        let snap = v.load_tasks_snapshot(SOURCE).unwrap();
        assert_eq!(snap.len(), 2);
        let i1 = snap.iter().find(|t| t.id == "I1").unwrap();
        assert_eq!(i1.project, "Q2 Roadmap");
        assert_eq!(i1.source, SOURCE);

        // Raw file partitioned by createdAt month.
        assert!(v.root().join("tasks/linear/raw/2026-06.jsonl").exists());
        let raw = std::fs::read_to_string(v.root().join("tasks/linear/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"branchName\""), "raw keeps all source fields");
        assert!(raw.contains("Ship the feature"));

        // Cursor advanced; API key never in cursor file.
        let st = v.read_linear_sync();
        assert!(st.last_updated_at.is_some(), "watermark advanced");
        let cursor = std::fs::read_to_string(v.root().join(".trove/linear-sync.json")).unwrap();
        assert!(!cursor.contains("access_token"), "API key never in cursor");
        assert!(!cursor.contains("lin_api_"), "API key never in cursor");
    }

    #[test]
    fn resync_dedupes_raw_by_id() {
        let v = temp_vault("rawdedup");
        let api1 = MockApi::new(vec![gql_response(vec![issue_open("I1", "p1")], false, None)]);
        pull_with(&v, &api1, now()).unwrap();

        let api2 = MockApi::new(vec![gql_response(vec![issue_open("I1", "p1")], false, None)]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("raw"), Some(&0), "same id not duplicated");
        let raw = std::fs::read_to_string(v.root().join("tasks/linear/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "one raw line, no duplicate");
    }

    // --- fate / completion / deletion tests --------------------------------

    #[test]
    fn completed_issue_yields_completion_event_at_correct_time() {
        let v = temp_vault("fate-complete");
        // Sync 1: I1 is open. First pull: only open query (no watermark → no completed query).
        let api1 = MockApi::new(vec![gql_response(vec![issue_open("I1", "p1")], false, None)]);
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: I1 is no longer open (completed). Two-query approach:
        // - open query: empty (I1 not in open set)
        // - completed query (uses watermark): returns I1 as completed
        let api2 = MockApi::new(vec![
            gql_response(vec![], false, None), // open query: I1 gone from open
            gql_response(
                vec![issue_completed("I1", "2026-06-13T16:30:00.000Z")],
                false,
                None,
            ), // completed query: I1 completed since watermark
        ]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("completed"), Some(&1));

        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completions: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].task.id, "I1");
        let ev_ts = DateTime::parse_from_rfc3339(&completions[0].time).unwrap().timestamp();
        let expected =
            DateTime::parse_from_rfc3339("2026-06-13T16:30:00.000Z").unwrap().timestamp();
        assert_eq!(ev_ts, expected);
        assert!(v.root().join("tasks/linear/events/2026-06.jsonl").exists());
    }

    #[test]
    fn issue_absent_from_all_results_is_deleted() {
        let v = temp_vault("fate-delete");
        // Sync 1: I1 open. First pull: only open query (no watermark).
        let api1 = MockApi::new(vec![gql_response(vec![issue_open("I1", "p1")], false, None)]);
        pull_with(&v, &api1, now()).unwrap();

        // Sync 2: I1 absent from open set (unassigned or deleted in Linear).
        // Two-query approach:
        // - open query: only I2 (I1 is gone)
        // - completed query: empty (I1 was not completed — it was deleted/unassigned)
        // I1 absent from open + absent from completed_map → Deleted.
        let api2 = MockApi::new(vec![
            gql_response(vec![issue_open("I2", "p1")], false, None), // open query
            gql_response(vec![], false, None), // completed query: nothing completed
        ]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("deleted"), Some(&1));

        // Deletions are stamped at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "I1"));
        assert!(!events.iter().any(|e| e.kind == "completed" && e.task.id == "I1"));
    }

    #[test]
    fn linear_fate_matrix() {
        let mut map = HashMap::new();
        map.insert("done".to_string(), Some("2026-06-13T16:30:00-07:00".to_string()));
        map.insert("done_no_ts".to_string(), None);

        let task = |id: &str| Task {
            source: SOURCE.into(),
            id: id.into(),
            title: "x".into(),
            project: String::new(),
            notes: String::new(),
            status: "in progress".into(),
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

        match linear_fate(&task("done"), Some(&map)) {
            TaskFate::Completed(Some(w)) => assert!(w.starts_with("2026-06-13")),
            _ => panic!("expected Completed(time)"),
        }
        assert!(matches!(linear_fate(&task("done_no_ts"), Some(&map)), TaskFate::Completed(None)));
        // "ghost" is absent from the map → Deleted.
        assert!(matches!(linear_fate(&task("ghost"), Some(&map)), TaskFate::Deleted));
        // None map (pull failed) → Unknown.
        assert!(matches!(linear_fate(&task("done"), None), TaskFate::Unknown));
    }

    // --- connection tests --------------------------------------------------

    #[test]
    fn connection_stores_and_retrieves_api_key() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "lin_api_fake_key_for_test".into(),
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
        assert_eq!(status.accounts[0].label, "Linear");
        assert_eq!(status.accounts[0].key, SERVICE);

        // API key NOT in the cursor file.
        v.write_linear_sync(&SyncState {
            last_updated_at: Some("2026-06-01T00:00:00.000Z".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/linear-sync.json")).unwrap();
        assert!(!cursor.contains("lin_api_"), "API key never in cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("lin_api_fake_key_for_test") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret file must be 0600");
                }
            }
            assert!(found, "token was stored under .trove/sync");
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
        assert_eq!(CONNECTION.id, "linear");
        assert_eq!(DEF.meta.id, "linear");
        assert_eq!(DEF.meta.domain, "tasks");
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_updated_at.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_updated_at":"2026-06-01T00:00:00.000Z"}"#).unwrap();
        assert_eq!(
            partial.last_updated_at.as_deref(),
            Some("2026-06-01T00:00:00.000Z")
        );
        // Unknown future keys are ignored (forward compat).
        let future: SyncState = serde_json::from_str(
            r#"{"last_updated_at":"2026-06-01T00:00:00.000Z","future_key":42}"#,
        )
        .unwrap();
        assert!(future.last_updated_at.is_some());
    }

    #[test]
    fn watermark_advances_to_max_updated_at() {
        let v = temp_vault("watermark");
        // Two issues with different updatedAt; the later one should be the watermark.
        let i_early = json!({
            "id": "IE",
            "identifier": "ENG-1",
            "title": "Early",
            "priority": 0,
            "dueDate": null,
            "completedAt": null,
            "canceledAt": null,
            "createdAt": "2026-06-01T08:00:00.000Z",
            "updatedAt": "2026-06-01T08:00:00.000Z",
            "url": "https://linear.app/team/issue/ENG-1",
            "branchName": null,
            "state": { "name": "Todo", "type": "unstarted" },
            "project": null,
            "cycle": null,
            "labels": { "nodes": [] },
            "comments": { "nodes": [] }
        });
        let i_late = json!({
            "id": "IL",
            "identifier": "ENG-2",
            "title": "Late",
            "priority": 0,
            "dueDate": null,
            "completedAt": null,
            "canceledAt": null,
            "createdAt": "2026-06-01T08:00:00.000Z",
            "updatedAt": "2026-06-14T22:00:00.000Z",
            "url": "https://linear.app/team/issue/ENG-2",
            "branchName": null,
            "state": { "name": "Todo", "type": "unstarted" },
            "project": null,
            "cycle": null,
            "labels": { "nodes": [] },
            "comments": { "nodes": [] }
        });
        let api = MockApi::new(vec![gql_response(vec![i_early, i_late], false, None)]);
        pull_with(&v, &api, now()).unwrap();
        let state = v.read_linear_sync();
        assert_eq!(
            state.last_updated_at.as_deref(),
            Some("2026-06-14T22:00:00.000Z"),
            "watermark must be the latest updatedAt seen"
        );
    }
}
