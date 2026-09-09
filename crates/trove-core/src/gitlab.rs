//! GitLab — periodic cloud sync of commits, merge requests, and issues via
//! the official REST API v4.
//!
//! A **Periodic** cloud pull. Two destinations written in one pass:
//!
//! - **raw firehose** under `developer/gitlab/` (the `developer/` raw-only
//!   taxonomy — same shape as GitHub so the vault output is unified):
//!   - `developer/gitlab/commits/YYYY-MM.jsonl` — your authored commits
//!     across all projects you're a member of, partitioned by authored date.
//!   - `developer/gitlab/issues/YYYY-MM.jsonl` — issues you're involved in,
//!     partitioned by creation month.
//!   - `developer/gitlab/mrs/YYYY-MM.jsonl` — merge requests you authored or
//!     were assigned to, partitioned by creation month.
//!
//! - **tasks contract** under `tasks/gitlab/` (the existing Rust-bound
//!   [`crate::tasks`] contract): issues *assigned to you and still open*
//!   become [`Task`]s via `apply_tasks_sync`, so the same assigned issue
//!   appears in BOTH the issues firehose and the task store.
//!
//! Auth is a GitLab Personal Access Token (PAT) with `read_api` + `read_user`
//! scopes, pasted via the connection's [`ConnectMethod::TokenPaste`]. For
//! self-hosted instances the user also provides the base URL; both ride the
//! `access_token` slot of the stored [`TokenSet`] as `<url> <token>` so the
//! two values survive as a single secret (the ambient_weather.rs composite
//! pattern). The default base is `https://gitlab.com`.
//!
//! ## Cursors / dedup
//!
//! `.trove/gitlab-sync.json` (non-secret, rebuildable) holds the discovered
//! username plus per-stream watermarks. A cursor advances only after its
//! endpoint fully drains this tick, so a partial fetch never strands items.
//! commits/issues/mrs use **upsert-into-partition** (same as github.rs) so a
//! re-sync never duplicates a guid.
//!
//! ## GitLab v4 differences from GitHub
//!
//! - Auth header: `PRIVATE-TOKEN: <PAT>` (not `Authorization: Bearer`).
//! - Pagination: `X-Next-Page` response header (a page number, not a URL);
//!   we reconstruct the absolute URL from the current URL + `?page=N`.
//! - Commits: `id` field for SHA; `authored_date` / `committed_date` (ISO8601)
//!   at the top level (no `commit` wrapper); `path_with_namespace` for project.
//! - Issues/MRs: `id` = global integer, `iid` = project-local integer;
//!   `web_url` direct; `assignees` array (each `{ id, username, name }`).
//! - Due date on issues: `due_date` as `YYYY-MM-DD` string (nullable).

use std::collections::{BTreeMap, HashSet};
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
use crate::sync::oauth::TokenSet;
use crate::tasks::{ProjectInfo, Task, TaskFate};
use crate::vault::Vault;

// Raw firehose directories (developer/ taxonomy, raw-only).
const COMMITS_DIR: &str = "developer/gitlab/commits";
const ISSUES_DIR: &str = "developer/gitlab/issues";
const MRS_DIR: &str = "developer/gitlab/mrs";
/// Probe target for `last_data`.
const LAST_DATA_DIR: &str = "developer/gitlab/issues";

/// Non-secret rebuildable cursor.
const SYNC_FILE: &str = ".trove/gitlab-sync.json";

/// The service id under `.trove/sync/` where the composite credential is stored.
const SERVICE: &str = "gitlab";

/// Default GitLab instance base URL (no trailing slash).
const DEFAULT_BASE: &str = "https://gitlab.com";
const USER_AGENT: &str = "Trove (https://github.com/; personal-data-vault)";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Seconds between syncs — hourly like github.rs.
pub const GITLAB_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(LAST_DATA_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "gitlab synced — {} commits, {} issues, {} mrs, {} tasks",
                    c("commits"),
                    c("issues"),
                    c("mrs"),
                    c("tasks"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "gitlab sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let total: u64 = out.counts.values().sum();
    let headline = if total == 0 {
        "GitLab is up to date — no new activity".to_string()
    } else {
        let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
        format!(
            "GitLab synced — {} commits, {} issues, {} MRs",
            c("commits"),
            c("issues"),
            c("mrs"),
        )
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "gitlab",
        name: "GitLab",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your GitLab activity — commits, merge requests, and \
                      issues — via the official REST API v4. Issues assigned to \
                      you also flow into the unified task store. Works with both \
                      gitlab.com and self-hosted instances.",
        domain: "developer",
        vault_path: "developer/gitlab/",
        toggleable: true,
        setup: &[
            "Connect with a GitLab personal access token on this card.",
            "Each sync fetches new commits, issues, and MRs incrementally.",
        ],
        caveats: "Self-hosted GitLab instances are supported; prefix the token \
                  with your instance URL when connecting (e.g. \
                  https://gitlab.mycompany.com glpat-…). The events feed covers \
                  the last 3 months by default; older history is covered by the \
                  incremental per-project commit walk.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(GITLAB_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("gitlab"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — composite credential: optional URL + PAT).

/// Parse the pasted credential string.
///
/// Formats accepted:
/// - `glpat-xxxx` — just the PAT; uses gitlab.com.
/// - `https://gitlab.mycompany.com glpat-xxxx` — URL then PAT (space-separated).
/// - `https://gitlab.mycompany.com/api/v4 glpat-xxxx` — URL with api path; we
///   strip any `/api/v4` suffix before storing.
///
/// Returns `(base_url, token)`.
fn parse_credential(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your GitLab personal access token (optionally prefix with your instance URL)");
    }
    // Split on the first whitespace: everything before is the URL (if present),
    // everything after is the token.
    if let Some((first, rest)) = pasted.split_once(|c: char| c.is_ascii_whitespace()) {
        let token = rest.trim().to_string();
        if token.is_empty() {
            bail!("missing token after URL — format: https://gitlab.mycompany.com glpat-…");
        }
        // Strip any trailing /api/v4 from the URL so the base is the root.
        let base = first
            .trim()
            .trim_end_matches('/')
            .trim_end_matches("/api/v4")
            .trim_end_matches('/')
            .to_string();
        if base.is_empty() {
            bail!("invalid URL before token");
        }
        Ok((base, token))
    } else {
        // No whitespace → the whole string is the token; default to gitlab.com.
        Ok((DEFAULT_BASE.to_string(), pasted.to_string()))
    }
}

/// Serialize a `(base_url, token)` pair into the stored composite string.
fn encode_credential(base: &str, token: &str) -> String {
    if base == DEFAULT_BASE {
        token.to_string()
    } else {
        format!("{base} {token}")
    }
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (base, token) = parse_credential(pasted)?;
    let client = GitlabClient::new(base.clone(), token.clone());
    let username = match client.current_user_username() {
        Ok(u) => u,
        Err(FetchError::Unauthorized) => bail!(
            "GitLab rejected the token (401) — check it has read_api + read_user scopes and hasn't expired"
        ),
        Err(e) => bail!("GitLab /user check failed: {e}"),
    };
    // Store the composite credential (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: encode_credential(&base, &token),
            refresh_token: None,
            token_type: Some("GitlabPAT".into()),
            scope: None,
            expires_at: None,
        },
    )?;
    // Cache the discovered username in the non-secret cursor.
    let mut state = vault.read_gitlab_sync();
    state.username = Some(username);
    state.base_url = if base == DEFAULT_BASE { None } else { Some(base) };
    vault.write_gitlab_sync(&state)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        let state = vault.read_gitlab_sync();
        let label = state.username.unwrap_or_else(|| "GitLab".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
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
    id: "gitlab",
    display_name: "GitLab",
    methods: &[ConnectMethod::TokenPaste {
        label: "GitLab token",
        help: "Paste a GitLab personal access token with read_api and read_user scopes. \
               For a self-hosted instance, prefix with your instance URL: \
               https://gitlab.mycompany.com glpat-xxxxxxxxxxxxxxxxxxxx",
        placeholder: "glpat-xxxxxxxxxxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["gitlab"],
    setup: &[
        "In GitLab, go to your avatar → Edit profile → Access tokens.",
        "Create a token with read_api and read_user scopes. No expiry is fine.",
        "Paste the token here (prefix with your instance URL if self-hosted: https://gitlab.mycompany.com glpat-…). Stored locally, never sent anywhere.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page of results + a page number to follow for the next page.
/// `None` = last page.
struct Page {
    items: Vec<Value>,
    /// Absolute URL for the next page, if any.
    next: Option<String>,
}

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    /// 403 / 429 — rate limited.
    RateLimited,
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The endpoints the pull needs.
trait GitlabApi {
    /// `GET <path>` (API-relative, e.g. `/user`) → a single JSON object.
    fn get_one(&self, path: &str) -> Result<Value, FetchError>;
    /// `GET <path>` (API-relative) → one page + the next URL (if any).
    fn get_page(&self, path: &str) -> Result<Page, FetchError>;
    /// Follow an absolute next-page URL.
    fn get_page_abs(&self, url: &str) -> Result<Page, FetchError>;
}

/// Thin ureq client. `base` is the root (e.g. `https://gitlab.com`).
/// API calls prepend `/api/v4`.
struct GitlabClient {
    base: String,
    token: String,
}

impl GitlabClient {
    fn new(base: String, token: String) -> Self {
        GitlabClient { base, token }
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/api/v4{path}", self.base)
    }

    fn req(&self, url: &str) -> ureq::Request {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("PRIVATE-TOKEN", &self.token)
            .set("User-Agent", USER_AGENT)
    }

    fn handle_page(&self, resp: std::result::Result<ureq::Response, ureq::Error>, req_url: &str)
        -> Result<Page, FetchError>
    {
        match resp {
            Ok(resp) => {
                // GitLab pagination: X-Next-Page is a page NUMBER.
                // Reconstruct the absolute next URL by appending ?page=N to the
                // current request URL (or replacing an existing page= param).
                let next = resp.header("x-next-page").and_then(|s| {
                    let s = s.trim();
                    if s.is_empty() { return None; }
                    Some(set_page_param(req_url, s))
                });
                let v: Value = resp.into_json().map_err(|e| {
                    FetchError::Other(format!("parsing response: {e}"))
                })?;
                let items = match v {
                    Value::Array(a) => a,
                    _ => Vec::new(),
                };
                Ok(Page { items, next })
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code @ (403 | 429), _)) => {
                Err(if code == 429 { FetchError::RateLimited }
                    else { FetchError::Other(format!("HTTP {code} (forbidden — check token permissions)")) })
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

    fn current_user_username(&self) -> Result<String, FetchError> {
        let v = self.get_one("/user")?;
        v.get("username")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| FetchError::Other("GET /user returned no username".into()))
    }
}

/// Replace or add the `page=` query parameter in an absolute URL.
fn set_page_param(url: &str, page: &str) -> String {
    // Simple approach: strip existing page= param then append.
    let (base_url, fragment) = match url.split_once('#') {
        Some((b, f)) => (b, Some(f)),
        None => (url, None),
    };
    let (path, query) = match base_url.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (base_url, None),
    };
    let mut params: Vec<&str> = query
        .map(|q| q.split('&').filter(|p| !p.starts_with("page=")).collect())
        .unwrap_or_default();
    let page_param = format!("page={page}");
    params.push(&page_param);
    let new_query = params.join("&");
    match fragment {
        Some(f) => format!("{path}?{new_query}#{f}"),
        None => format!("{path}?{new_query}"),
    }
}

impl GitlabApi for GitlabClient {
    fn get_one(&self, path: &str) -> Result<Value, FetchError> {
        let url = self.api_url(path);
        match self.req(&url).call() {
            Ok(resp) => resp.into_json().map_err(|e| {
                FetchError::Other(format!("parsing response: {e}"))
            }),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code @ (403 | 429), _)) => {
                Err(if code == 429 { FetchError::RateLimited }
                    else { FetchError::Other(format!("HTTP {code}")) })
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

    fn get_page(&self, path: &str) -> Result<Page, FetchError> {
        let url = self.api_url(path);
        self.handle_page(self.req(&url).call(), &url)
    }

    fn get_page_abs(&self, url: &str) -> Result<Page, FetchError> {
        self.handle_page(self.req(url).call(), url)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Authenticated username (from `GET /api/v4/user`). Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    /// Self-hosted base URL, when not gitlab.com.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_url: Option<String>,
    /// All confirmed email addresses for the authenticated user (from `GET
    /// /user` `public_email` + `GET /user/emails`). Used to filter commits
    /// client-side — GitLab's commit `author=` param matches the git author
    /// *name* string (not the login handle), so email-based filtering is the
    /// only reliable way to find your own commits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    user_emails: Vec<String>,
    /// commits: max `committed_date` ever written. Next pull uses `?since=`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commits_since: Option<String>,
    /// issues: max `updated_at` ever seen. Next pull filters `?updated_after=`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issues_updated: Option<String>,
    /// mrs: max `updated_at` ever seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mrs_updated: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_gitlab_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_gitlab_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shapes (this module's own — developer/ is contract-free).

/// One commit row in `developer/gitlab/commits/YYYY-MM.jsonl`.
/// `guid = "<project_path>:<sha>"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitRow {
    pub guid: String,
    pub project: String,
    /// Full commit SHA (`id` in the GitLab v4 response).
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message: String,
    /// RFC3339 UTC (`authored_date`).
    pub authored_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

/// One issue or MR row. `guid = global "id"` (unique integer across the
/// instance); partitioned by `created_at`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueRow {
    pub guid: String,
    /// Global integer id.
    pub id: i64,
    /// Project-local integer id.
    pub iid: i64,
    pub project_id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    /// RFC3339 UTC (`created_at`) — the partition key.
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// `username` of the primary assignee (if any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// `username` of the author.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `YYYY-MM-DD` due date (issues only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    /// True when this is a merge request (from the MR endpoint).
    #[serde(default)]
    pub is_mr: bool,
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Pure mapping helpers.

fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `username` field from a nested user object (`{ id, username, name, ... }`).
fn nested_username(v: &Value, parent: &str) -> Option<String> {
    v.get(parent)
        .and_then(|p| p.as_object())
        .and_then(|o| o.get("username"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// One project-commits list item → [`CommitRow`]. `None` when no `id`
/// (SHA) or no `authored_date` (can't be partitioned).
fn map_commit(project_path: &str, value: Value) -> Option<CommitRow> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    // GitLab uses `id` for the SHA.
    let sha = match obj.remove("id") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other { obj.insert("id".into(), o); }
            return None;
        }
    };
    let authored_at = match obj.remove("authored_date") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other { obj.insert("authored_date".into(), o); }
            return None;
        }
    };
    let snap = Value::Object(obj.clone());
    let row = CommitRow {
        guid: format!("{project_path}:{sha}"),
        project: project_path.to_string(),
        sha,
        short_id: str_opt(&snap, "short_id"),
        title: obj.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        message: obj.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
        authored_at,
        committed_at: str_opt(&snap, "committed_date"),
        author_name: str_opt(&snap, "author_name"),
        author_email: str_opt(&snap, "author_email"),
        web_url: str_opt(&snap, "web_url"),
        extra: {
            for k in ["short_id", "title", "message", "committed_date",
                      "author_name", "author_email", "web_url"] {
                obj.remove(k);
            }
            obj
        },
    };
    Some(row)
}

/// One issue or MR object → [`IssueRow`]. `is_mr` distinguishes them.
/// `None` only when `id` or `created_at` is missing.
fn map_item(value: Value, is_mr: bool) -> Option<IssueRow> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let id = obj.get("id").and_then(Value::as_i64)?;
    let created_at = match obj.remove("created_at") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other { obj.insert("created_at".into(), o); }
            return None;
        }
    };
    let snap = Value::Object(obj.clone());
    let labels = obj
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let assignees: Vec<String> = obj
        .get("assignees")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    a.get("username").and_then(Value::as_str).map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    let row = IssueRow {
        guid: id.to_string(),
        id,
        iid: obj.get("iid").and_then(Value::as_i64).unwrap_or(0),
        project_id: obj.get("project_id").and_then(Value::as_i64).unwrap_or(0),
        title: obj.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        state: obj.get("state").and_then(Value::as_str).unwrap_or("").to_string(),
        created_at,
        updated_at: str_opt(&snap, "updated_at"),
        closed_at: str_opt(&snap, "closed_at"),
        merged_at: str_opt(&snap, "merged_at"),
        web_url: str_opt(&snap, "web_url"),
        labels,
        assignee: nested_username(&snap, "assignee"),
        assignees,
        author: nested_username(&snap, "author"),
        description: obj
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        due_date: str_opt(&snap, "due_date"),
        is_mr,
        extra: {
            for k in ["id", "iid", "project_id", "title", "state", "updated_at",
                      "closed_at", "merged_at", "web_url", "labels", "assignee",
                      "assignees", "author", "description", "due_date"] {
                obj.remove(k);
            }
            obj
        },
    };
    Some(row)
}

/// An assigned-and-open [`IssueRow`] → a normalized [`Task`].
fn issue_to_task(row: &IssueRow) -> Task {
    let mut extra = Map::new();
    extra.insert("iid".into(), Value::from(row.iid));
    extra.insert("project_id".into(), Value::from(row.project_id));
    if let Some(url) = &row.web_url {
        extra.insert("web_url".into(), Value::from(url.clone()));
    }
    if let Some(d) = &row.due_date {
        extra.insert("due_date".into(), Value::from(d.clone()));
    }
    extra.insert(
        "assignees".into(),
        Value::from(row.assignees.iter().map(|s| Value::from(s.clone())).collect::<Vec<_>>()),
    );
    extra.insert("state".into(), Value::from(row.state.clone()));
    Task {
        source: "gitlab".into(),
        id: row.guid.clone(),
        title: row.title.clone(),
        // The /issues list endpoint only carries `project_id` (a number), not
        // `path_with_namespace`. We store the numeric id here rather than
        // making an extra /projects/{id} call per task. The human-readable path
        // is recoverable from `web_url` or via /projects/{project_id}, and
        // `project_id` is also stored in `extra` for programmatic use.
        project: row.project_id.to_string(),
        notes: row.description.clone().unwrap_or_default(),
        status: "open".into(),
        priority: 0,
        due: row.due_date.as_deref().and_then(|d| {
            // Convert YYYY-MM-DD to RFC3339 local midnight.
            chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .ok()
                .and_then(|nd| nd.and_hms_opt(0, 0, 0))
                .map(|ndt| {
                    use chrono::TimeZone;
                    Local.from_local_datetime(&ndt)
                        .earliest()
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| d.to_string())
                })
        }),
        start: None,
        all_day: row.due_date.is_some(),
        recurrence: None,
        tags: row.labels.clone(),
        subtasks: Vec::new(),
        created: row.created_at.parse::<DateTime<chrono::FixedOffset>>()
            .ok()
            .map(|t| t.with_timezone(&Local).to_rfc3339()),
        modified: row.updated_at.as_deref()
            .and_then(|s| s.parse::<DateTime<chrono::FixedOffset>>().ok())
            .map(|t| t.with_timezone(&Local).to_rfc3339()),
        completed: None,
        extra,
    }
}

// ---------------------------------------------------------------------------
// Upsert-into-partition (same pattern as github.rs).

fn upsert_partitioned<T>(
    vault: &Vault,
    dir: &str,
    rows: Vec<T>,
    guid: impl Fn(&T) -> &str,
    ts: impl Fn(&T) -> &str,
    fresher: impl Fn(&T, &T) -> bool,
) -> Result<u64>
where
    T: Serialize + serde::de::DeserializeOwned + Clone,
{
    use crate::store::Partition;
    let stream = vault.stream(dir, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month
            .key(ts(&r))
            .with_context(|| format!("gitlab: row ts {:?} has no month prefix", ts(&r)))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<T> = stream.read(&month)?;
        let mut idx: BTreeMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (guid(r).to_string(), i))
            .collect();
        for r in fresh {
            match idx.get(guid(&r)).copied() {
                Some(i) => {
                    if fresher(&r, &existing[i]) {
                        existing[i] = r;
                    }
                }
                None => {
                    idx.insert(guid(&r).to_string(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| ts(a).cmp(ts(b)).then_with(|| guid(a).cmp(guid(b))));
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync every stream.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let stored = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("GitLab is not connected — add a personal access token in the Integrations tab")?;
    let (base, token) = parse_credential(&stored)?;
    let client = GitlabClient::new(base, token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl GitlabApi) -> Result<PullOutcome> {
    let mut state = vault.read_gitlab_sync();

    // Discover + cache username and user emails if missing.
    // GitLab's commits `author=` param filters by git *name*, not login handle,
    // so we collect the user's registered emails and filter client-side instead.
    let username = match state.username.clone() {
        Some(u) => u,
        None => {
            let user_obj = api
                .get_one("/user")
                .map_err(|e| anyhow::anyhow!("GitLab /user lookup failed: {e}"))?;
            let u = user_obj
                .get("username")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .context("GET /user returned no username")?;
            state.username = Some(u.clone());

            // Seed emails from public_email on the /user object.
            let mut emails: Vec<String> = Vec::new();
            if let Some(e) = user_obj.get("public_email").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                emails.push(e.to_string());
            }
            state.user_emails = emails;
            u
        }
    };

    // Refresh the full email list if we haven't populated it yet (tolerates
    // old cached states that predate this field).
    if state.user_emails.is_empty() {
        let mut emails: Vec<String> = Vec::new();
        // /user gives public_email.
        if let Ok(user_obj) = api.get_one("/user") {
            if let Some(e) = user_obj.get("public_email").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                emails.push(e.to_string());
            }
        }
        // /user/emails returns all confirmed addresses (requires read_user scope).
        if let Ok(Page { items, .. }) = api.get_page("/user/emails") {
            for item in items {
                if let Some(e) = item.get("email").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                    if !emails.contains(&e.to_string()) {
                        emails.push(e.to_string());
                    }
                }
            }
        }
        state.user_emails = emails;
    }

    let user_emails: Vec<String> = state.user_emails.clone();

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- commits ---------------------------------------------------------
    match sync_commits(vault, api, &user_emails, state.commits_since.as_deref()) {
        Ok((n, max_date)) => {
            counts.insert("commits", n);
            if let Some(d) = max_date {
                advance(&mut state.commits_since, d);
            }
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- issues ----------------------------------------------------------
    match sync_issues(vault, api, &username, state.issues_updated.as_deref()) {
        Ok((n, max_updated)) => {
            counts.insert("issues", n);
            if let Some(u) = max_updated {
                advance(&mut state.issues_updated, u);
            }
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- merge requests --------------------------------------------------
    match sync_mrs(vault, api, state.mrs_updated.as_deref()) {
        Ok((n, max_updated)) => {
            counts.insert("mrs", n);
            if let Some(u) = max_updated {
                advance(&mut state.mrs_updated, u);
            }
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- tasks: assigned open issues -------------------------------------
    match sync_tasks(vault, api) {
        Ok(n) => {
            counts.insert("tasks", n);
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_gitlab_sync(&state)?;

    let total: u64 = counts.values().sum();
    Ok(PullOutcome {
        headline: format!("{total} new GitLab records"),
        counts,
    })
}

fn advance(cursor: &mut Option<String>, candidate: String) {
    if cursor.as_deref().is_none_or(|c| candidate.as_str() > c) {
        *cursor = Some(candidate);
    }
}

enum SyncStop {
    RateLimited,
    Fatal(anyhow::Error),
}

impl SyncStop {
    fn from_fetch(e: FetchError) -> Self {
        match e {
            FetchError::RateLimited => SyncStop::RateLimited,
            other => SyncStop::Fatal(anyhow::anyhow!("gitlab fetch failed: {other}")),
        }
    }
}

/// Walk all pages starting at `start_path` (API-relative), collecting items.
fn drain_pages(
    api: &impl GitlabApi,
    start_path: &str,
    between: Duration,
) -> Result<Vec<Value>, SyncStop> {
    let mut items = Vec::new();
    let first = api.get_page(start_path).map_err(SyncStop::from_fetch)?;
    items.extend(first.items);
    let mut next = first.next;
    while let Some(url) = next {
        thread::sleep(between);
        let page = api.get_page_abs(&url).map_err(SyncStop::from_fetch)?;
        items.extend(page.items);
        next = page.next;
    }
    Ok(items)
}

/// commits: list all projects the user is a member of, then per project pull
/// commits and filter client-side by `author_email`.
///
/// GitLab's commit `author=` query param matches the git commit *author name*
/// string (e.g. "Jane Smith"), NOT the login handle (e.g. "jsmith"). Because
/// username != name for virtually all users, passing `author={username}` would
/// silently return zero results. We therefore omit the server-side filter and
/// match the `author_email` field against the user's registered email addresses
/// (fetched once and cached in [`SyncState::user_emails`]).
///
/// Returns (new guids, max committed_date).
fn sync_commits(
    vault: &Vault,
    api: &impl GitlabApi,
    user_emails: &[String],
    since: Option<&str>,
) -> Result<(u64, Option<String>), SyncStop> {
    // Build a lowercase email set for O(1) lookup.
    let email_set: HashSet<String> = user_emails
        .iter()
        .map(|e| e.to_lowercase())
        .collect();

    // List all member projects (paginated).
    let proj_items = drain_pages(
        api,
        "/projects?membership=true&per_page=100&order_by=last_activity_at",
        Duration::from_millis(0),
    )?;

    let mut rows: Vec<CommitRow> = Vec::new();
    let mut max_date: Option<String> = None;

    for proj in &proj_items {
        let project_path = proj
            .get("path_with_namespace")
            .and_then(Value::as_str)
            .unwrap_or("");
        let proj_id = proj.get("id").and_then(Value::as_i64).unwrap_or(0);
        if project_path.is_empty() || proj_id == 0 {
            continue;
        }
        // Skip projects with no activity in the window when we have a cursor.
        if let Some(since) = since {
            if let Some(last) = proj.get("last_activity_at").and_then(Value::as_str) {
                if last < since {
                    continue;
                }
            }
        }
        // Note: we do NOT pass `author=` to GitLab — that param filters by the
        // git author *name* string, not by login, so it would mismatch for most
        // users. We filter by author_email client-side below instead.
        //
        // `since` must be URL-encoded: GitLab timestamps contain `+00:00`
        // offsets; a bare `+` decodes server-side as a space, corrupting the
        // date filter on incremental syncs.
        let mut path = format!(
            "/projects/{proj_id}/repository/commits?per_page=100"
        );
        if let Some(since) = since {
            path.push_str(&format!("&since={}", urlencode(since)));
        }
        let items = match drain_pages(api, &path, Duration::from_millis(0)) {
            Ok(items) => items,
            Err(SyncStop::RateLimited) => return Err(SyncStop::RateLimited),
            // 404/403 on individual repos is non-fatal (e.g. access removed).
            Err(SyncStop::Fatal(_)) => continue,
        };
        for v in items {
            // Client-side filter: keep only commits authored by this user.
            // When user_emails is empty (token has no read_user scope) we keep
            // all commits rather than silently discarding everything.
            let authored_by_user = if email_set.is_empty() {
                true
            } else {
                v.get("author_email")
                    .and_then(Value::as_str)
                    .map(|e| email_set.contains(&e.to_lowercase()))
                    .unwrap_or(false)
            };
            if !authored_by_user {
                continue;
            }
            if let Some(row) = map_commit(project_path, v) {
                let cursor_date = row.committed_at.clone().unwrap_or_else(|| row.authored_at.clone());
                max_date = Some(match max_date {
                    Some(m) if m >= cursor_date => m,
                    _ => cursor_date,
                });
                rows.push(row);
            }
        }
    }

    let n = upsert_partitioned(
        vault,
        COMMITS_DIR,
        rows,
        |r| &r.guid,
        |r| &r.authored_at,
        |_new, _old| false, // commits are immutable
    ).map_err(SyncStop::Fatal)?;
    Ok((n, max_date))
}

/// issues: `GET /issues?scope=all&updated_after=<cursor>&per_page=100`.
/// Returns (new guids, max updated_at).
fn sync_issues(
    vault: &Vault,
    api: &impl GitlabApi,
    _username: &str,
    updated_since: Option<&str>,
) -> Result<(u64, Option<String>), SyncStop> {
    let mut path = "/issues?scope=all&per_page=100&order_by=updated_at&sort=asc".to_string();
    if let Some(since) = updated_since {
        path.push_str(&format!("&updated_after={}", urlencode(since)));
    }
    let items = drain_pages(api, &path, Duration::from_millis(200))?;
    let rows: Vec<IssueRow> = items.into_iter().filter_map(|v| map_item(v, false)).collect();
    let max_updated = rows.iter().filter_map(|r| r.updated_at.clone()).max();
    let n = upsert_partitioned(
        vault,
        ISSUES_DIR,
        rows,
        |r| &r.guid,
        |r| &r.created_at,
        |new, old| match (new.updated_at.as_deref(), old.updated_at.as_deref()) {
            (Some(a), Some(b)) => a >= b,
            (Some(_), None) => true,
            _ => false,
        },
    ).map_err(SyncStop::Fatal)?;
    Ok((n, max_updated))
}

/// mrs: `GET /merge_requests?scope=created_by_me` (+ `updated_after`).
fn sync_mrs(
    vault: &Vault,
    api: &impl GitlabApi,
    updated_since: Option<&str>,
) -> Result<(u64, Option<String>), SyncStop> {
    let mut path =
        "/merge_requests?scope=created_by_me&per_page=100&order_by=updated_at&sort=asc"
            .to_string();
    if let Some(since) = updated_since {
        path.push_str(&format!("&updated_after={}", urlencode(since)));
    }
    let items = drain_pages(api, &path, Duration::from_millis(200))?;
    let rows: Vec<IssueRow> = items.into_iter().filter_map(|v| map_item(v, true)).collect();
    let max_updated = rows.iter().filter_map(|r| r.updated_at.clone()).max();
    let n = upsert_partitioned(
        vault,
        MRS_DIR,
        rows,
        |r| &r.guid,
        |r| &r.created_at,
        |new, old| match (new.updated_at.as_deref(), old.updated_at.as_deref()) {
            (Some(a), Some(b)) => a >= b,
            (Some(_), None) => true,
            _ => false,
        },
    ).map_err(SyncStop::Fatal)?;
    Ok((n, max_updated))
}

/// tasks: assigned open issues → [`Task`]s via `apply_tasks_sync`.
fn sync_tasks(vault: &Vault, api: &impl GitlabApi) -> Result<u64, SyncStop> {
    let items = drain_pages(
        api,
        "/issues?scope=assigned_to_me&state=opened&per_page=100",
        Duration::from_millis(200),
    )?;
    let rows: Vec<IssueRow> = items.into_iter().filter_map(|v| map_item(v, false)).collect();
    let fresh: Vec<Task> = rows.iter().map(issue_to_task).collect();
    // Distinct project ids as projects.
    let mut projects: Vec<ProjectInfo> = Vec::new();
    let mut seen: HashSet<i64> = HashSet::new();
    for r in &rows {
        if seen.insert(r.project_id) {
            projects.push(ProjectInfo {
                id: r.project_id.to_string(),
                name: r.project_id.to_string(),
            });
        }
    }
    let stats = vault
        .apply_tasks_sync("gitlab", &projects, fresh, |t| gitlab_fate(api, t))
        .map_err(SyncStop::Fatal)?;
    Ok(stats.created + stats.completed + stats.deleted)
}

/// Resolve a task that left the assigned-open set.
fn gitlab_fate(api: &impl GitlabApi, task: &Task) -> TaskFate {
    let proj_id = task.extra.get("project_id").and_then(Value::as_i64);
    let iid = task.extra.get("iid").and_then(Value::as_i64);
    let (proj_id, iid) = match (proj_id, iid) {
        (Some(p), Some(i)) => (p, i),
        _ => return TaskFate::Unknown,
    };
    match api.get_one(&format!("/projects/{proj_id}/issues/{iid}")) {
        Ok(v) => match v.get("state").and_then(Value::as_str) {
            Some("closed") => {
                let when = v
                    .get("closed_at")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<DateTime<chrono::FixedOffset>>().ok())
                    .map(|t| t.with_timezone(&Local).to_rfc3339());
                TaskFate::Completed(when)
            }
            Some("opened") => TaskFate::Deleted, // still open, just unassigned
            _ => TaskFate::Unknown,
        },
        Err(_) => TaskFate::Unknown,
    }
}

/// Minimal percent-encoding for URL parameter values.
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

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-gitlab-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---- fixtures (realistic GitLab v4 field names) ----------------------

    fn commit_json(sha: &str, authored: &str, committed: &str) -> Value {
        serde_json::json!({
            "id": sha,
            "short_id": &sha[..8],
            "title": format!("Fix something ({sha})"),
            "message": format!("Fix something ({sha})\n\nSigned-off-by: dev"),
            "authored_date": authored,
            "committed_date": committed,
            "author_name": "Jane Dev",
            "author_email": "jane@example.com",
            "web_url": format!("https://gitlab.com/group/project/-/commit/{sha}"),
            "parent_ids": [],
            "trailers": {}
        })
    }

    fn issue_json(id: i64, iid: i64, created: &str, updated: &str, state: &str) -> Value {
        serde_json::json!({
            "id": id,
            "iid": iid,
            "project_id": 42,
            "title": format!("Issue {iid}"),
            "description": "some description",
            "state": state,
            "created_at": created,
            "updated_at": updated,
            "closed_at": null,
            "web_url": format!("https://gitlab.com/group/project/-/issues/{iid}"),
            "labels": ["bug", "p1"],
            "due_date": "2026-07-01",
            "assignee": null,
            "assignees": [],
            "author": { "id": 7, "username": "octocat", "name": "Octo Cat" },
            "milestone": null
        })
    }

    fn assigned_issue_json(id: i64, iid: i64, login: &str) -> Value {
        serde_json::json!({
            "id": id,
            "iid": iid,
            "project_id": 42,
            "title": format!("Assigned {iid}"),
            "description": "do the work",
            "state": "opened",
            "created_at": "2026-06-01T08:00:00.000Z",
            "updated_at": "2026-06-05T09:00:00.000Z",
            "closed_at": null,
            "web_url": format!("https://gitlab.com/group/project/-/issues/{iid}"),
            "labels": ["task"],
            "due_date": "2026-07-15",
            "assignee": { "id": 1, "username": login, "name": "Test User" },
            "assignees": [{ "id": 1, "username": login, "name": "Test User" }],
            "author": { "id": 7, "username": "octocat", "name": "Octo Cat" }
        })
    }

    fn mr_json(id: i64, iid: i64) -> Value {
        serde_json::json!({
            "id": id,
            "iid": iid,
            "project_id": 42,
            "title": format!("MR {iid}"),
            "description": "merge this",
            "state": "opened",
            "created_at": "2026-04-10T00:00:00.000Z",
            "updated_at": "2026-04-11T00:00:00.000Z",
            "closed_at": null,
            "merged_at": null,
            "web_url": format!("https://gitlab.com/group/project/-/merge_requests/{iid}"),
            "labels": [],
            "author": { "id": 7, "username": "octocat", "name": "Octo Cat" },
            "assignees": []
        })
    }

    fn project_json(id: i64, path: &str, last_activity: &str) -> Value {
        serde_json::json!({
            "id": id,
            "path_with_namespace": path,
            "name_with_namespace": path,
            "name": path.split('/').last().unwrap_or(path),
            "last_activity_at": last_activity,
            "web_url": format!("https://gitlab.com/{path}")
        })
    }

    // ---- mock API --------------------------------------------------------

    struct MockApi {
        username: String,
        pages: RefCell<Vec<(String, VecDeque<Page>)>>,
        singles: RefCell<BTreeMap<String, Value>>,
    }

    impl MockApi {
        fn new(username: &str) -> Self {
            MockApi {
                username: username.into(),
                pages: RefCell::new(Vec::new()),
                singles: RefCell::new(BTreeMap::new()),
            }
        }

        fn page(&self, prefix: &str, items: Vec<Value>) {
            self.pages
                .borrow_mut()
                .push((prefix.into(), VecDeque::from(vec![Page { items, next: None }])));
        }

        #[allow(dead_code)]
        fn single(&self, path: &str, v: Value) {
            self.singles.borrow_mut().insert(path.into(), v);
        }

        fn next_page_for(&self, key: &str) -> Result<Page, FetchError> {
            let mut pages = self.pages.borrow_mut();
            for (prefix, queue) in pages.iter_mut() {
                if key.contains(prefix.as_str()) || key.starts_with(prefix.as_str()) {
                    if let Some(p) = queue.pop_front() {
                        return Ok(p);
                    }
                }
            }
            Ok(Page { items: Vec::new(), next: None })
        }
    }

    impl GitlabApi for MockApi {
        fn get_one(&self, path: &str) -> Result<Value, FetchError> {
            if path == "/user" {
                // Include public_email so pull_with seeds user_emails on first call.
                return Ok(serde_json::json!({
                    "id": 1,
                    "username": self.username,
                    "name": "Dev User",
                    "public_email": "jane@example.com"
                }));
            }
            self.singles
                .borrow()
                .get(path)
                .cloned()
                .ok_or(FetchError::Other(format!("no single for {path}")))
        }

        fn get_page(&self, path: &str) -> Result<Page, FetchError> {
            self.next_page_for(path)
        }

        fn get_page_abs(&self, url: &str) -> Result<Page, FetchError> {
            self.next_page_for(url)
        }
    }

    // ---- pure mapping tests ----------------------------------------------

    #[test]
    fn maps_commit_correct_fields() {
        let sha = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let row = map_commit("group/project", commit_json(sha, "2026-05-20T10:00:00.000Z", "2026-05-20T10:01:00.000Z")).unwrap();
        assert_eq!(row.guid, format!("group/project:{sha}"));
        assert_eq!(row.sha, sha);
        assert_eq!(row.project, "group/project");
        assert_eq!(row.authored_at, "2026-05-20T10:00:00.000Z");
        assert_eq!(row.committed_at.as_deref(), Some("2026-05-20T10:01:00.000Z"));
        assert_eq!(row.author_name.as_deref(), Some("Jane Dev"));
        assert_eq!(row.author_email.as_deref(), Some("jane@example.com"));
        assert!(row.web_url.is_some());
        // Unknown keys preserved in extra; known keys not duplicated.
        assert!(row.extra.contains_key("parent_ids"), "parent_ids preserved in extra");
        assert!(!row.extra.contains_key("id"), "sha not in extra");
        assert!(!row.extra.contains_key("authored_date"), "authored_date not in extra");
    }

    #[test]
    fn maps_issue_correct_fields() {
        let row = map_item(
            issue_json(100, 5, "2026-05-02T12:00:00.000Z", "2026-05-03T12:00:00.000Z", "opened"),
            false,
        ).unwrap();
        assert_eq!(row.guid, "100");
        assert_eq!(row.id, 100);
        assert_eq!(row.iid, 5);
        assert_eq!(row.project_id, 42);
        assert_eq!(row.state, "opened");
        assert_eq!(row.labels, vec!["bug", "p1"]);
        assert_eq!(row.due_date.as_deref(), Some("2026-07-01"));
        assert_eq!(row.author.as_deref(), Some("octocat"));
        assert!(!row.is_mr);
    }

    #[test]
    fn maps_mr_sets_is_mr() {
        let row = map_item(mr_json(200, 3), true).unwrap();
        assert!(row.is_mr);
        assert_eq!(row.guid, "200");
        assert_eq!(row.merged_at, None);
    }

    #[test]
    fn credential_parse_roundtrip() {
        // PAT only → gitlab.com.
        let (base, tok) = parse_credential("glpat-xxxx").unwrap();
        assert_eq!(base, DEFAULT_BASE);
        assert_eq!(tok, "glpat-xxxx");

        // URL + PAT.
        let (base2, tok2) = parse_credential("https://gitlab.myco.com glpat-yyyy").unwrap();
        assert_eq!(base2, "https://gitlab.myco.com");
        assert_eq!(tok2, "glpat-yyyy");

        // URL with /api/v4 suffix is stripped.
        let (base3, _) = parse_credential("https://gitlab.myco.com/api/v4 glpat-zzzz").unwrap();
        assert_eq!(base3, "https://gitlab.myco.com");

        // Encode round-trip: gitlab.com PAT stored as plain token.
        assert_eq!(encode_credential(DEFAULT_BASE, "glpat-xxxx"), "glpat-xxxx");
        // Self-hosted stored with prefix.
        let enc = encode_credential("https://gitlab.myco.com", "glpat-yyyy");
        let (b, t) = parse_credential(&enc).unwrap();
        assert_eq!(b, "https://gitlab.myco.com");
        assert_eq!(t, "glpat-yyyy");
    }

    #[test]
    fn set_page_param_replaces_and_appends() {
        // No existing page param → appended.
        let url = "https://gitlab.com/api/v4/projects?membership=true&per_page=100";
        let next = set_page_param(url, "2");
        assert!(next.contains("page=2"), "page appended: {next}");

        // Replace existing page=.
        let url2 = "https://gitlab.com/api/v4/projects?page=3&per_page=100";
        let next2 = set_page_param(url2, "4");
        assert!(next2.contains("page=4"), "replaced: {next2}");
        assert!(!next2.contains("page=3"), "old page gone: {next2}");
    }

    #[test]
    fn upsert_dedupes_over_overlapping_window() {
        let v = temp_vault("upsert");
        let sha1 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let sha2 = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let sha3 = "cccccccccccccccccccccccccccccccccccccccc";
        let rows1 = vec![
            map_commit("g/p", commit_json(sha1, "2026-05-10T00:00:00.000Z", "2026-05-10T00:00:00.000Z")).unwrap(),
            map_commit("g/p", commit_json(sha2, "2026-05-11T00:00:00.000Z", "2026-05-11T00:00:00.000Z")).unwrap(),
        ];
        let n1 = upsert_partitioned(&v, COMMITS_DIR, rows1, |r| &r.guid, |r| &r.authored_at, |_, _| false).unwrap();
        assert_eq!(n1, 2);

        // Re-sync overlapping window: sha2 again + new sha3.
        let rows2 = vec![
            map_commit("g/p", commit_json(sha2, "2026-05-11T00:00:00.000Z", "2026-05-11T00:00:00.000Z")).unwrap(),
            map_commit("g/p", commit_json(sha3, "2026-05-12T00:00:00.000Z", "2026-05-12T00:00:00.000Z")).unwrap(),
        ];
        let n2 = upsert_partitioned(&v, COMMITS_DIR, rows2, |r| &r.guid, |r| &r.authored_at, |_, _| false).unwrap();
        assert_eq!(n2, 1, "only sha3 is new");

        let body = std::fs::read_to_string(v.root().join("developer/gitlab/commits/2026-05.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 3, "3 unique commits");
    }

    #[test]
    fn full_pull_writes_all_streams_and_advances_cursors() {
        let sha = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
        let v = temp_vault("fullpull");
        let api = MockApi::new("devuser");

        api.page("/projects", vec![project_json(42, "group/project", "2026-05-21T00:00:00.000Z")]);
        api.page("/projects/42/repository/commits", vec![
            commit_json(sha, "2026-05-20T10:00:00.000Z", "2026-05-20T10:01:00.000Z"),
        ]);
        api.page("/issues?scope=all", vec![
            issue_json(100, 5, "2026-05-02T12:00:00.000Z", "2026-05-03T12:00:00.000Z", "opened"),
        ]);
        api.page("/merge_requests?scope=created_by_me", vec![mr_json(200, 3)]);
        api.page("/issues?scope=assigned_to_me", vec![
            assigned_issue_json(101, 6, "devuser"),
        ]);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("commits"), Some(&1));
        assert_eq!(out.counts.get("issues"), Some(&1));
        assert_eq!(out.counts.get("mrs"), Some(&1));
        assert_eq!(out.counts.get("tasks"), Some(&1), "one assigned issue → task");

        assert!(v.root().join("developer/gitlab/commits/2026-05.jsonl").exists());
        assert!(v.root().join("developer/gitlab/issues/2026-05.jsonl").exists());
        assert!(v.root().join("developer/gitlab/mrs/2026-04.jsonl").exists());
        assert!(v.root().join("tasks/gitlab/tasks.jsonl").exists());

        let state = v.read_gitlab_sync();
        assert_eq!(state.commits_since.as_deref(), Some("2026-05-20T10:01:00.000Z"));
        assert_eq!(state.issues_updated.as_deref(), Some("2026-05-03T12:00:00.000Z"));
        assert_eq!(state.mrs_updated.as_deref(), Some("2026-04-11T00:00:00.000Z"));
        assert!(state.updated.is_some());
    }

    #[test]
    fn issue_to_task_maps_fields_correctly() {
        let row = map_item(
            assigned_issue_json(101, 6, "devuser"),
            false,
        ).unwrap();
        let task = issue_to_task(&row);
        assert_eq!(task.source, "gitlab");
        assert_eq!(task.id, "101");
        assert_eq!(task.title, "Assigned 6");
        assert_eq!(task.status, "open");
        assert_eq!(task.tags, vec!["task"]);
        assert_eq!(task.extra.get("iid").and_then(Value::as_i64), Some(6));
        assert!(task.due.is_some(), "due_date should be converted");
    }

    /// Defect fix: commits API `author=` matches git author *name*, not login.
    /// Verify that commits with a non-matching email are silently dropped and
    /// only commits matching the user's email are stored.
    #[test]
    fn sync_commits_filters_by_author_email_not_username() {
        let sha_mine = "1111111111111111111111111111111111111111";
        let sha_other = "2222222222222222222222222222222222222222";
        let v = temp_vault("emailfilter");
        let api = MockApi::new("devuser");

        // Return two commits: one authored by the user, one by someone else.
        // The mock doesn't filter on `author=`, so both come back — mirroring
        // what GitLab returns when we omit the unreliable author= param.
        let mut own_commit = commit_json(sha_mine, "2026-05-01T10:00:00.000Z", "2026-05-01T10:01:00.000Z");
        own_commit["author_email"] = serde_json::json!("jane@example.com");
        let mut other_commit = commit_json(sha_other, "2026-05-02T10:00:00.000Z", "2026-05-02T10:01:00.000Z");
        other_commit["author_email"] = serde_json::json!("someone.else@example.com");

        api.page("/projects", vec![project_json(42, "group/project", "2026-05-03T00:00:00.000Z")]);
        api.page("/projects/42/repository/commits", vec![own_commit, other_commit]);
        // The /user/emails page is empty — only public_email from /user matters.

        // seed user_emails directly through pull_with (no issues/mrs/tasks needed).
        api.page("/issues?scope=all", vec![]);
        api.page("/merge_requests?scope=created_by_me", vec![]);
        api.page("/issues?scope=assigned_to_me", vec![]);

        let out = pull_with(&v, &api).unwrap();
        // Only the matching commit should be written.
        assert_eq!(out.counts.get("commits"), Some(&1), "only own commit kept");

        let body = std::fs::read_to_string(
            v.root().join("developer/gitlab/commits/2026-05.jsonl")
        ).unwrap();
        assert_eq!(body.lines().count(), 1, "exactly one commit in vault");
        let stored: CommitRow = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(stored.sha, sha_mine, "stored commit is the user's own");
        assert_eq!(stored.author_email.as_deref(), Some("jane@example.com"));
    }

    /// Defect fix: the `since` cursor may contain a `+00:00` TZ offset which
    /// must be percent-encoded before inserting into the query string —
    /// otherwise `+` decodes as a space on the server side.
    #[test]
    fn sync_commits_since_cursor_url_encodes_plus_offset() {
        // A timestamp with +00:00 offset — the form GitLab returns for
        // committed_date on some API responses.
        let since = "2026-05-20T10:01:00.000+00:00";
        let encoded = urlencode(since);
        assert!(
            encoded.contains("%2B"),
            "plus sign in TZ offset must be percent-encoded, got: {encoded}"
        );
        assert!(
            !encoded.contains('+'),
            "raw + must not remain in encoded string, got: {encoded}"
        );
        // Verify the since= injection doesn't corrupt the path.
        let path = format!("/projects/1/repository/commits?per_page=100&since={}", urlencode(since));
        assert!(path.contains("since=2026-05-20T10%3A01%3A00.000%2B00%3A00") ||
                path.contains("%2B00%3A00"),
                "encoded since in path: {path}");
    }
}
