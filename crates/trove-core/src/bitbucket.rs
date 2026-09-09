//! Bitbucket — periodic cloud sync of commits and pull requests via the
//! Bitbucket Cloud REST API v2.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/bitbucket.md.
//!
//! A **Periodic** cloud pull. One destination, all raw (developer/ is
//! raw-only — no contract):
//!
//! - `developer/bitbucket/commits/YYYY-MM.jsonl` — your authored commits
//!   across all workspaces you belong to, partitioned by commit date.
//! - `developer/bitbucket/prs/YYYY-MM.jsonl` — pull requests you authored,
//!   partitioned by creation month.
//!
//! Auth is a Bitbucket App Password (user-specific credential created in
//! account settings → App passwords) used as HTTP Basic Auth. The username
//! is discovered from `GET /2.0/user` at connect time and cached in the
//! non-secret cursor; only the App Password is stored in the secret store
//! as `<username>:<app-password>` in the `access_token` slot.
//!
//! ## API notes
//!
//! - Base: `https://api.bitbucket.org/2.0`
//! - Commits: `GET /repositories/{workspace}/{slug}/commits` — no user-level
//!   events feed exists; we must iterate workspaces → repos → commits.
//! - Commit field names differ from GitHub: `hash` (not sha), `date` (not
//!   `authored_date`), `author.raw` + `author.user` (not a top-level git
//!   committer object). Confirmed from live API responses.
//! - PRs: `GET /workspaces/{workspace}/pullrequests/{selected_user}?state=…`
//!   where `{selected_user}` is the user's `account_id` (UUID form, stable
//!   identifier). There is no bare account-level `/pullrequests` endpoint;
//!   the Atlassian OpenAPI spec has no such path. We iterate the same workspace
//!   list used for commits and query each workspace.
//!   Fields: `id` (integer), `title`, `state`, `created_on`, `updated_on`,
//!   `source.branch.name`, `destination.branch.name`, `author`.
//! - Pagination: Bitbucket returns a `next` URL at the top level of every
//!   paginated response (alongside `values`, `pagelen`, `size`, `page`).
//!   We follow `next` directly (same as GitHub's `Link: rel="next"` approach).
//!
//! ## Cursors / dedup
//!
//! `.trove/bitbucket-sync.json` (non-secret, rebuildable) holds:
//!   - `username` — authenticated Bitbucket display nickname (from `/2.0/user`)
//!   - `account_id` — stable Bitbucket account_id UUID (from `/2.0/user`),
//!     used as the `{selected_user}` path segment in the PR endpoint
//!   - `commits_since` — max `date` ever written; used for client-side
//!     short-circuit (server-side q= filter is NOT supported on /commits)
//!   - `prs_updated` — max `updated_on` ever seen; next pull asks `?q=updated_on>timestamp`
//!     (the PR endpoint DOES honour q= filters)
//!
//! A cursor advances only after the endpoint fully drains for this tick;
//! a partial fetch leaves the cursor unadvanced. upsert-into-partition by
//! guid (commit hash / PR id) prevents duplicates on overlapping windows.

use std::collections::BTreeMap;
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
use crate::vault::Vault;

// Raw firehose directories (developer/ taxonomy, raw-only).
const COMMITS_DIR: &str = "developer/bitbucket/commits";
const PRS_DIR: &str = "developer/bitbucket/prs";
/// Probe target for `last_data` — PRs are the liveliest stream.
const LAST_DATA_DIR: &str = "developer/bitbucket/prs";

/// Non-secret rebuildable cursor.
const SYNC_FILE: &str = ".trove/bitbucket-sync.json";

/// The service id under `.trove/sync/` where the App Password is stored.
const SERVICE: &str = "bitbucket";

const API_BASE: &str = "https://api.bitbucket.org/2.0";
const USER_AGENT: &str = "Trove (https://github.com/; personal-data-vault)";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs — hourly like github.rs/gitlab.rs.
pub const BITBUCKET_SYNC_SECS: u64 = 3600;

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
                    "bitbucket synced — {} commits, {} prs",
                    c("commits"),
                    c("prs"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "bitbucket sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let total: u64 = out.counts.values().sum();
    let headline = if total == 0 {
        "Bitbucket is up to date — no new activity".to_string()
    } else {
        let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
        format!(
            "Bitbucket synced — {} commits, {} PRs",
            c("commits"),
            c("prs"),
        )
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "bitbucket",
        name: "Bitbucket",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Bitbucket commits and pull requests via the \
                      official Cloud REST API v2, storing them in the developer \
                      activity stream alongside GitHub and GitLab.",
        domain: "developer",
        vault_path: "developer/bitbucket/",
        toggleable: true,
        setup: &[
            "Connect with a Bitbucket App Password on this card.",
            "Each sync incrementally fetches new commits across all your workspaces and your authored pull requests.",
        ],
        caveats: "Bitbucket has no user-level activity events feed; commits are discovered \
                  by iterating repositories in each workspace, which is slower than the \
                  GitHub/GitLab equivalents. Self-hosted Bitbucket Data Center has a separate \
                  API and is not supported in v1.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(BITBUCKET_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("bitbucket"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — App Password as Basic Auth credential).

/// At connect time: verify the App Password via `GET /2.0/user`, store the
/// composite credential `<username>:<app-password>` in the secret store, and
/// cache the discovered username in the non-secret cursor.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty — paste a Bitbucket App Password (from Settings → App passwords)");
    }
    // The token may be pasted as "username:password" or just the App Password.
    // Detect and normalize: if it contains ':' assume "username:app-password";
    // otherwise it's the App Password alone and we'll discover the username.
    let (username, app_password) = if let Some((u, p)) = token.split_once(':') {
        let u = u.trim();
        let p = p.trim();
        if u.is_empty() || p.is_empty() {
            bail!("format should be 'username:app-password' or just the App Password alone");
        }
        (u.to_string(), p.to_string())
    } else {
        // App Password only — we need the username from the API. But we need a
        // username for Basic Auth. Ask the user to provide "username:password".
        bail!(
            "paste both your Bitbucket username and App Password separated by a colon: \
             myusername:ATBB-xxxxxxxxxxxx"
        )
    };

    let client = BitbucketClient::new(API_BASE.to_string(), username.clone(), app_password.clone());
    let discovered = match client.current_user() {
        Ok(u) => u,
        Err(FetchError::Unauthorized) => bail!(
            "Bitbucket rejected the credentials (401) — check the username and App Password are correct"
        ),
        Err(e) => bail!("Bitbucket /user check failed: {e}"),
    };

    // Store the composite credential (0600). The `username:app-password` pair
    // rides the `access_token` slot of a never-expiring TokenSet.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: format!("{username}:{app_password}"),
            refresh_token: None,
            token_type: Some("BasicAuth".into()),
            scope: None,
            expires_at: None,
        },
    )?;

    // Cache the confirmed username in the non-secret cursor.
    let mut state = vault.read_bitbucket_sync();
    state.username = Some(discovered);
    vault.write_bitbucket_sync(&state)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        let label = vault
            .read_bitbucket_sync()
            .username
            .unwrap_or_else(|| "Bitbucket".to_string());
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
    id: "bitbucket",
    display_name: "Bitbucket",
    methods: &[ConnectMethod::TokenPaste {
        label: "Bitbucket credentials",
        help: "Paste your Bitbucket username and App Password separated by a colon: \
               myusername:ATBB-xxxxxxxxxxxx. Create an App Password at \
               bitbucket.org → Settings → App passwords with Repositories (read) \
               and Pull requests (read) permissions.",
        placeholder: "myusername:ATBB-xxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["bitbucket"],
    setup: &[
        "Go to bitbucket.org → your avatar → Settings → App passwords.",
        "Create an App Password with Repositories (read) and Pull requests (read) permissions.",
        "Paste your Bitbucket username and the App Password here, separated by a colon: myusername:ATBB-xxxxxxxxxxxx",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page of results plus the absolute URL for the next page.
/// Bitbucket returns `next` at the top level alongside `values`.
struct Page {
    items: Vec<Value>,
    next: Option<String>,
}

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    /// 429 or 403 with rate-limit indicator.
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

/// The endpoints the pull needs. A trait so tests drive the logic offline.
trait BitbucketApi {
    /// `GET <path>` (absolute URL or API-relative path) → one page of `values`.
    fn get_page(&self, path: &str) -> Result<Page, FetchError>;
    /// Follow an absolute next-page URL.
    fn get_page_abs(&self, url: &str) -> Result<Page, FetchError>;
    /// `GET <path>` → a single JSON object.
    fn get_one(&self, path: &str) -> Result<Value, FetchError>;
    /// Authenticated user's display nickname from `GET /2.0/user`.
    fn current_user(&self) -> Result<String, FetchError>;
    /// Authenticated user's stable `account_id` UUID from `GET /2.0/user`.
    /// This is the identifier required in the `/workspaces/{ws}/pullrequests/{selected_user}` path.
    fn current_account_id(&self) -> Result<String, FetchError>;
}

/// Thin ureq client using HTTP Basic Auth.
struct BitbucketClient {
    base: String,
    username: String,
    app_password: String,
}

impl BitbucketClient {
    fn new(base: String, username: String, app_password: String) -> Self {
        BitbucketClient { base, username, app_password }
    }

    fn req(&self, url: &str) -> ureq::Request {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!(
                "Basic {}",
                base64_encode(&format!("{}:{}", self.username, self.app_password))
            ))
            .set("Accept", "application/json")
            .set("User-Agent", USER_AGENT)
    }

    fn handle_page(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<Page, FetchError> {
        match resp {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                // Bitbucket paginates with a top-level `next` URL + `values` array.
                let next = v.get("next").and_then(Value::as_str).map(str::to_string);
                let items = v
                    .get("values")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(Page { items, next })
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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
}

/// Minimal base64 encoding for the Basic Auth header (no external crate — the
/// `base64` crate is already in Cargo.toml for coinbase.rs).
fn base64_encode(input: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(input.as_bytes())
}

impl BitbucketApi for BitbucketClient {
    fn get_page(&self, path: &str) -> Result<Page, FetchError> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.base)
        };
        BitbucketClient::handle_page(self.req(&url).call())
    }

    fn get_page_abs(&self, url: &str) -> Result<Page, FetchError> {
        BitbucketClient::handle_page(self.req(url).call())
    }

    fn get_one(&self, path: &str) -> Result<Value, FetchError> {
        let url = if path.starts_with("http") {
            path.to_string()
        } else {
            format!("{}{path}", self.base)
        };
        match self.req(&url).call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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

    fn current_user(&self) -> Result<String, FetchError> {
        let v = self.get_one("/user")?;
        // Bitbucket /user returns `account_id` (stable UUID, immutable) and
        // `nickname` (mutable display handle — NOT a stable identifier; can
        // contain spaces and special chars like "mkemp [Atlassian]").
        // We return the nickname for display; the caller also caches account_id.
        v.get("nickname")
            .or_else(|| v.get("account_id"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| FetchError::Other("GET /user returned no username/account_id".into()))
    }

    /// Fetch the stable `account_id` from `/user` (the UUID used in API paths).
    fn current_account_id(&self) -> Result<String, FetchError> {
        let v = self.get_one("/user")?;
        v.get("account_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| FetchError::Other("GET /user returned no account_id".into()))
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Authenticated Bitbucket display nickname (from `GET /2.0/user`).
    /// Mutable — do NOT use as a stable identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    /// Stable Bitbucket `account_id` UUID (from `GET /2.0/user`).
    /// This is the `{selected_user}` path segment required by the PR endpoint
    /// `GET /workspaces/{ws}/pullrequests/{account_id}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    /// commits: max `date` (ISO8601) ever written. Used for client-side
    /// short-circuit since the /commits endpoint does NOT support q= filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commits_since: Option<String>,
    /// prs: max `updated_on` ever seen. Next pull asks `?q=updated_on>"<this>"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prs_updated: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_bitbucket_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_bitbucket_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shapes (developer/ is contract-free — this module owns each shape).
//
// Field names match the Bitbucket API v2 exactly (confirmed from live
// api.bitbucket.org responses):
//   Commit: `hash` (not sha), `date` (ISO8601), `author.raw` (email string),
//           `author.user` (object with `nickname`, `account_id`, `display_name`).
//   PR: `id` (integer), `title`, `state`, `created_on`, `updated_on`,
//       `source.branch.name`, `destination.branch.name`, `author.display_name`.

/// One commit row in `developer/bitbucket/commits/YYYY-MM.jsonl`.
/// `guid = "<workspace>/<repo>:<hash>"`, partitioned by `date`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitRow {
    pub guid: String,
    /// `<workspace>/<repo_slug>` — the Bitbucket full repo path.
    pub repo: String,
    /// The commit hash (Bitbucket calls this `hash`).
    pub hash: String,
    /// Commit message.
    #[serde(default)]
    pub message: String,
    /// ISO8601 UTC timestamp (`date` field on the commit).
    pub date: String,
    /// Human-readable author string from `author.raw` (e.g. "Name <email>").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_raw: Option<String>,
    /// Bitbucket account nickname of the author (`author.user.nickname`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_nickname: Option<String>,
    /// Bitbucket account_id of the author (`author.user.account_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_account_id: Option<String>,
    /// `html` link to the commit on bitbucket.org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    /// Unknown keys, preserved verbatim (full fidelity).
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

/// One pull request row in `developer/bitbucket/prs/YYYY-MM.jsonl`.
/// `guid = "<workspace>/<repo_slug>#<id>"`, partitioned by `created_on`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrRow {
    pub guid: String,
    /// `<workspace>/<repo_slug>`.
    pub repo: String,
    /// PR integer id (unique per repo).
    pub id: i64,
    #[serde(default)]
    pub title: String,
    /// Bitbucket states: OPEN, MERGED, DECLINED, SUPERSEDED.
    #[serde(default)]
    pub state: String,
    /// ISO8601 UTC (`created_on`) — the partition key.
    pub created_on: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_on: Option<String>,
    /// Source branch name (`source.branch.name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_branch: Option<String>,
    /// Destination branch name (`destination.branch.name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_branch: Option<String>,
    /// PR author display name (`author.display_name`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_display_name: Option<String>,
    /// PR author nickname (`author.nickname`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_nickname: Option<String>,
    /// PR description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Link to the PR on bitbucket.org.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    /// Unknown keys, preserved verbatim.
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// Pure mapping helpers (fixture-tested, no network).

/// Pull a string field trimmed; `None` when missing/non-string/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Nested field: `parent_key.child_key` string, tolerating a null/missing parent.
fn nested_str(v: &Value, parent: &str, child: &str) -> Option<String> {
    v.get(parent)
        .and_then(|p| p.as_object())
        .and_then(|o| o.get(child))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `links.html.href` — the standard Bitbucket web URL pattern.
fn html_href(v: &Value) -> Option<String> {
    v.get("links")
        .and_then(|l| l.get("html"))
        .and_then(|h| h.get("href"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Map one API commit object → [`CommitRow`].
/// `None` when `hash` or `date` are absent (can't partition or dedup).
pub(crate) fn map_commit(repo: &str, value: Value) -> Option<CommitRow> {
    let Value::Object(mut obj) = value else {
        return None;
    };

    let hash = match obj.remove("hash") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other {
                obj.insert("hash".into(), o);
            }
            return None;
        }
    };

    let date = match obj.remove("date") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other {
                obj.insert("date".into(), o);
            }
            return None;
        }
    };

    let snap = Value::Object(obj.clone());
    let message = snap
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // `author` is an object with `.raw` (email string) and optionally `.user`
    // (a Bitbucket account object with `nickname`, `account_id`, `display_name`).
    let author_raw = nested_str(&snap, "author", "raw");
    let author_nickname = snap
        .get("author")
        .and_then(|a| a.get("user"))
        .and_then(|u| u.get("nickname"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let author_account_id = snap
        .get("author")
        .and_then(|a| a.get("user"))
        .and_then(|u| u.get("account_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let html_url = html_href(&snap);

    // Consume mapped keys; preserve the rest in `extra`.
    for k in ["message", "author", "links", "parents", "summary", "rendered"] {
        obj.remove(k);
    }

    Some(CommitRow {
        guid: format!("{repo}:{hash}"),
        repo: repo.to_string(),
        hash,
        message,
        date,
        author_raw,
        author_nickname,
        author_account_id,
        html_url,
        extra: obj,
    })
}

/// Map one API pull request object → [`PrRow`].
/// `None` when `id` or `created_on` are absent (can't dedup or partition).
pub(crate) fn map_pr(repo: &str, value: Value) -> Option<PrRow> {
    let Value::Object(mut obj) = value else {
        return None;
    };

    let id = match obj.get("id").and_then(Value::as_i64) {
        Some(n) => n,
        None => return None,
    };

    let created_on = match obj.remove("created_on") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other {
                obj.insert("created_on".into(), o);
            }
            return None;
        }
    };

    let snap = Value::Object(obj.clone());
    let title = str_opt(&snap, "title").unwrap_or_default();
    let state = str_opt(&snap, "state").unwrap_or_default();
    let updated_on = str_opt(&snap, "updated_on");
    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let html_url = html_href(&snap);

    // Branch names live at `source.branch.name` and `destination.branch.name`.
    let source_branch = snap
        .get("source")
        .and_then(|s| s.get("branch"))
        .and_then(|b| b.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let destination_branch = snap
        .get("destination")
        .and_then(|d| d.get("branch"))
        .and_then(|b| b.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let author_display_name = nested_str(&snap, "author", "display_name");
    let author_nickname = nested_str(&snap, "author", "nickname");

    // Consume mapped keys; rest go to `extra`.
    for k in [
        "id",
        "title",
        "state",
        "updated_on",
        "description",
        "links",
        "author",
        "source",
        "destination",
        "summary",
        "reviewers",
        "participants",
        "merge_commit",
        "closed_by",
    ] {
        obj.remove(k);
    }

    Some(PrRow {
        guid: format!("{repo}#{id}"),
        repo: repo.to_string(),
        id,
        title,
        state,
        created_on,
        updated_on,
        source_branch,
        destination_branch,
        author_display_name,
        author_nickname,
        description,
        html_url,
        extra: obj,
    })
}

// ---------------------------------------------------------------------------
// Upsert-into-partition (the github.rs / gitlab.rs idiom).

/// Group rows by partition month, then upsert each group into its
/// `<dir>/<YYYY-MM>.jsonl`. Returns the count of *new* guids.
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
            .with_context(|| format!("bitbucket: row ts {:?} has no month prefix", ts(&r)))?
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
// Pull logic.

/// Resolve credentials from the secret store and run the sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let secret = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context(
            "Bitbucket is not connected — add credentials in the Integrations tab",
        )?;
    // The credential is stored as "username:app-password".
    let (username, app_password) = secret
        .split_once(':')
        .map(|(u, p)| (u.to_string(), p.to_string()))
        .context("Bitbucket credential malformed — reconnect to fix")?;

    let client = BitbucketClient::new(API_BASE.to_string(), username, app_password);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl BitbucketApi) -> Result<PullOutcome> {
    let mut state = vault.read_bitbucket_sync();

    // Discover and cache username + account_id if missing.
    // account_id is the stable UUID required by the PR workspace endpoint.
    let username = match state.username.clone() {
        Some(u) => u,
        None => {
            let u = api
                .current_user()
                .map_err(|e| anyhow::anyhow!("Bitbucket /user lookup failed: {e}"))?;
            state.username = Some(u.clone());
            u
        }
    };
    let account_id = match state.account_id.clone() {
        Some(id) => id,
        None => {
            let id = api
                .current_account_id()
                .map_err(|e| anyhow::anyhow!("Bitbucket /user account_id lookup failed: {e}"))?;
            state.account_id = Some(id.clone());
            id
        }
    };

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- commits (via workspace → repo iteration) -------------------------
    match sync_commits(vault, api, &username, state.commits_since.as_deref()) {
        Ok((n, max_date)) => {
            counts.insert("commits", n);
            if let Some(d) = max_date {
                advance(&mut state.commits_since, d);
            }
        }
        Err(SyncStop::RateLimited) => { /* leave cursor; resume next tick */ }
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- pull requests (workspace-scoped, by account_id) ------------------
    match sync_prs(vault, api, &account_id, state.prs_updated.as_deref()) {
        Ok((n, max_updated)) => {
            counts.insert("prs", n);
            if let Some(u) = max_updated {
                advance(&mut state.prs_updated, u);
            }
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_bitbucket_sync(&state)?;

    let total: u64 = counts.values().sum();
    Ok(PullOutcome {
        headline: format!("{total} new Bitbucket records"),
        counts,
    })
}

/// Advance a cursor only when the candidate is strictly greater (lexical
/// compare is correct for ISO8601 timestamps).
fn advance(cursor: &mut Option<String>, candidate: String) {
    if cursor.as_deref().is_none_or(|c| candidate.as_str() > c) {
        *cursor = Some(candidate);
    }
}

/// Why a per-endpoint sync stopped early.
enum SyncStop {
    RateLimited,
    Fatal(anyhow::Error),
}

impl SyncStop {
    fn from_fetch(e: FetchError) -> Self {
        match e {
            FetchError::RateLimited => SyncStop::RateLimited,
            other => SyncStop::Fatal(anyhow::anyhow!("bitbucket fetch failed: {other}")),
        }
    }
}

/// Drain all pages starting from `start_path`, collecting items.
/// A rate limit between pages returns `RateLimited` (cursor left unadvanced).
fn drain_pages(
    api: &impl BitbucketApi,
    start_path: &str,
    between: Duration,
) -> Result<Vec<Value>, SyncStop> {
    let first = api.get_page(start_path).map_err(SyncStop::from_fetch)?;
    let mut items = first.items;
    let mut next = first.next;
    while let Some(url) = next {
        thread::sleep(between);
        let page = api.get_page_abs(&url).map_err(SyncStop::from_fetch)?;
        items.extend(page.items);
        next = page.next;
    }
    Ok(items)
}

/// Discover workspaces → repos → commits authored since the cursor.
/// Returns (new guids, max commit date).
///
/// NOTE: The Bitbucket `/repositories/{ws}/{slug}/commits` endpoint does NOT
/// support query filtering (`q=` is silently ignored). We use a client-side
/// short-circuit instead: since Bitbucket returns commits in reverse-
/// chronological order, we stop paging as soon as we see a commit whose
/// `date` is <= `since` (the cursor). The guid-dedup in upsert_partitioned
/// is the correctness safety net; the short-circuit limits request volume.
fn sync_commits(
    vault: &Vault,
    api: &impl BitbucketApi,
    username: &str,
    since: Option<&str>,
) -> Result<(u64, Option<String>), SyncStop> {
    // 1. List all workspaces the user belongs to.
    let workspace_items = drain_pages(api, "/user/permissions/workspaces?pagelen=50", Duration::ZERO)?;
    let workspaces: Vec<String> = workspace_items
        .into_iter()
        .filter_map(|v| {
            v.get("workspace")
                .and_then(|w| w.get("slug"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();

    let mut rows: Vec<CommitRow> = Vec::new();
    let mut max_date: Option<String> = None;

    for ws in &workspaces {
        // 2. List repos in the workspace where the user is a member.
        let repo_path = format!("/repositories/{ws}?role=member&pagelen=50");
        let repo_items = match drain_pages(api, &repo_path, Duration::ZERO) {
            Ok(items) => items,
            Err(SyncStop::RateLimited) => return Err(SyncStop::RateLimited),
            Err(SyncStop::Fatal(_)) => continue, // forbidden workspace — skip
        };

        for repo_v in repo_items {
            let slug = match repo_v.get("slug").and_then(Value::as_str) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let repo_full = format!("{ws}/{slug}");

            // 3. Commits in this repo — paginated, reverse-chronological.
            // We do NOT pass q= because the /commits endpoint ignores it.
            // Instead we drain pages until we hit a commit older than `since`,
            // then break out of the loop (client-side short-circuit).
            // Author filtering is by account_id (stable) or nickname fallback.
            let commit_path = format!("/repositories/{ws}/{slug}/commits?pagelen=50");

            // We page manually rather than using drain_pages so we can break early.
            let mut next_url: Option<String> = None;
            let mut first = true;
            'pages: loop {
                let page = if first {
                    first = false;
                    match api.get_page(&commit_path) {
                        Ok(p) => p,
                        Err(FetchError::RateLimited) => return Err(SyncStop::RateLimited),
                        Err(_) => break 'pages, // 403/404 on this repo — skip
                    }
                } else if let Some(url) = next_url.take() {
                    thread::sleep(Duration::from_millis(100));
                    match api.get_page_abs(&url) {
                        Ok(p) => p,
                        Err(FetchError::RateLimited) => return Err(SyncStop::RateLimited),
                        Err(_) => break 'pages,
                    }
                } else {
                    break 'pages;
                };

                let has_next = page.next.is_some();
                next_url = page.next;

                for v in page.items {
                    // Client-side short-circuit: commits arrive newest-first.
                    // If this commit's date is at or before the cursor we have
                    // already stored it (or it's older than anything we want).
                    if let Some(s) = since {
                        let commit_date = v.get("date").and_then(Value::as_str).unwrap_or("");
                        if !commit_date.is_empty() && commit_date <= s {
                            break 'pages; // all remaining commits are even older
                        }
                    }

                    // Author filter: prefer stable account_id; fall back to nickname.
                    let author_obj = v.get("author").and_then(|a| a.get("user"));
                    let author_matches = author_obj.map_or(false, |u| {
                        // account_id is the stable identifier (fix for minor defect).
                        let by_id = u.get("account_id").and_then(Value::as_str);
                        let by_nick = u.get("nickname").and_then(Value::as_str);
                        // `username` here is the nickname from SyncState; we also
                        // accept an account_id match if the caller passes one.
                        by_nick.map_or(false, |n| n.eq_ignore_ascii_case(username))
                            || by_id.map_or(false, |id| id == username)
                    });
                    if !author_matches {
                        continue;
                    }

                    if let Some(row) = map_commit(&repo_full, v) {
                        max_date = Some(match max_date {
                            Some(m) if m >= row.date => m,
                            _ => row.date.clone(),
                        });
                        rows.push(row);
                    }
                }

                if !has_next {
                    break 'pages;
                }
            }
        }
    }

    let n = upsert_partitioned(
        vault,
        COMMITS_DIR,
        rows,
        |r| &r.guid,
        |r| &r.date,
        |_new, _old| false, // commits are immutable
    )
    .map_err(SyncStop::Fatal)?;
    Ok((n, max_date))
}

/// Fetch all pull requests authored by this user across all workspaces, since cursor.
/// Returns (new guids, max updated_on).
///
/// The Atlassian OpenAPI spec (swagger.v3.json) has NO bare `/pullrequests`
/// account-level path. The only user-scoped PR endpoint is:
///   `GET /workspaces/{workspace}/pullrequests/{selected_user}`
/// where `{selected_user}` MUST be the user's `account_id` UUID (not nickname).
/// We reuse the workspace list already fetched for commits.
fn sync_prs(
    vault: &Vault,
    api: &impl BitbucketApi,
    account_id: &str,
    updated_since: Option<&str>,
) -> Result<(u64, Option<String>), SyncStop> {
    // Discover workspaces (same endpoint as commits path).
    let workspace_items = drain_pages(api, "/user/permissions/workspaces?pagelen=50", Duration::ZERO)?;
    let workspaces: Vec<String> = workspace_items
        .into_iter()
        .filter_map(|v| {
            v.get("workspace")
                .and_then(|w| w.get("slug"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();

    let mut rows: Vec<PrRow> = Vec::new();
    let mut max_updated: Option<String> = None;

    for ws in &workspaces {
        // Build the workspace-scoped PR path.
        // state= is a repeatable query param; q= for updated_on IS supported here.
        let mut path = format!(
            "/workspaces/{ws}/pullrequests/{account_id}?state=OPEN&state=MERGED&state=DECLINED&state=SUPERSEDED&pagelen=50"
        );
        if let Some(since) = updated_since {
            let q = format!("updated_on>\"{since}\"");
            path.push_str(&format!("&q={}", urlencode(&q)));
        }

        let pr_items = match drain_pages(api, &path, Duration::from_millis(200)) {
            Ok(items) => items,
            Err(SyncStop::RateLimited) => return Err(SyncStop::RateLimited),
            Err(SyncStop::Fatal(_)) => continue, // workspace 403/404 — skip
        };

        for v in pr_items {
            // Extract repo from `destination.repository.full_name`.
            let repo = v
                .get("destination")
                .and_then(|d| d.get("repository"))
                .and_then(|r| r.get("full_name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown/unknown")
                .to_string();

            // Track max updated_on for cursor.
            if let Some(upd) = v.get("updated_on").and_then(Value::as_str) {
                max_updated = Some(match max_updated {
                    Some(m) if m.as_str() >= upd => m,
                    _ => upd.to_string(),
                });
            }

            if let Some(row) = map_pr(&repo, v) {
                rows.push(row);
            }
        }
    }

    let n = upsert_partitioned(
        vault,
        PRS_DIR,
        rows,
        |r| &r.guid,
        |r| &r.created_on,
        |new, old| {
            // A PR's state/updated_on can change — keep the freshest.
            new.updated_on.as_deref().unwrap_or("") > old.updated_on.as_deref().unwrap_or("")
        },
    )
    .map_err(SyncStop::Fatal)?;
    Ok((n, max_updated))
}

/// URL-encode a string for use in a query parameter.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                out.push(char::from_digit((b & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
            }
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

    // -----------------------------------------------------------------------
    // Mapping tests (pure, no network, no vault).

    #[test]
    fn map_commit_extracts_confirmed_fields() {
        // Field names confirmed from live api.bitbucket.org responses:
        // `hash` (not sha), `date`, `author.raw`, `author.user.nickname`,
        // `author.user.account_id`, `links.html.href`.
        let raw = serde_json::json!({
            "type": "commit",
            "hash": "abc123def456abc123def456abc123def456abc1",
            "date": "2026-06-10T09:00:00+00:00",
            "message": "fix: correct off-by-one error in pagination\n",
            "author": {
                "type": "author",
                "raw": "Alice Dev <alice@example.com>",
                "user": {
                    "type": "user",
                    "display_name": "Alice Dev",
                    "nickname": "alice",
                    "account_id": "5c355119b393bf4ce9561ec3"
                }
            },
            "links": {
                "html": { "href": "https://bitbucket.org/acme/my-repo/commits/abc123def456abc123def456abc123def456abc1" }
            },
            "parents": [],
            "summary": { "raw": "fix: correct off-by-one error", "markup": "markdown", "html": "<p>fix</p>" }
        });

        let row = map_commit("acme/my-repo", raw).expect("should map successfully");
        assert_eq!(row.guid, "acme/my-repo:abc123def456abc123def456abc123def456abc1");
        assert_eq!(row.repo, "acme/my-repo");
        assert_eq!(row.hash, "abc123def456abc123def456abc123def456abc1");
        assert_eq!(row.date, "2026-06-10T09:00:00+00:00");
        assert!(row.message.contains("fix: correct off-by-one"));
        assert_eq!(row.author_raw.as_deref(), Some("Alice Dev <alice@example.com>"));
        assert_eq!(row.author_nickname.as_deref(), Some("alice"));
        assert_eq!(row.author_account_id.as_deref(), Some("5c355119b393bf4ce9561ec3"));
        assert_eq!(
            row.html_url.as_deref(),
            Some("https://bitbucket.org/acme/my-repo/commits/abc123def456abc123def456abc123def456abc1")
        );
        // `parents` and `summary` go to extra (or are consumed), not to named fields.
        assert!(!row.extra.contains_key("hash"), "hash consumed from extra");
        assert!(!row.extra.contains_key("date"), "date consumed from extra");
        assert!(!row.extra.contains_key("author"), "author consumed from extra");
    }

    #[test]
    fn map_commit_missing_hash_returns_none() {
        let raw = serde_json::json!({
            "date": "2026-06-10T09:00:00+00:00",
            "message": "oops — no hash"
        });
        assert!(map_commit("ws/repo", raw).is_none(), "no hash → None");
    }

    #[test]
    fn map_commit_missing_date_returns_none() {
        let raw = serde_json::json!({
            "hash": "abc123",
            "message": "oops — no date"
        });
        assert!(map_commit("ws/repo", raw).is_none(), "no date → None");
    }

    #[test]
    fn map_pr_extracts_confirmed_fields() {
        // Field names confirmed from live api.bitbucket.org responses:
        // `id` (integer), `title`, `state`, `created_on`, `updated_on`,
        // `source.branch.name`, `destination.branch.name`,
        // `author.display_name`, `author.nickname`.
        let raw = serde_json::json!({
            "type": "pullrequest",
            "id": 5349,
            "title": "feat: Add dark mode support",
            "state": "MERGED",
            "created_on": "2026-06-05T16:52:02.943122+00:00",
            "updated_on": "2026-06-16T21:52:27.468417+00:00",
            "description": "Implements a dark mode toggle in the settings panel.",
            "source": {
                "branch": { "name": "feature/dark-mode" },
                "repository": { "full_name": "acme/my-repo" }
            },
            "destination": {
                "branch": { "name": "main" },
                "repository": { "full_name": "acme/my-repo" }
            },
            "author": {
                "type": "user",
                "display_name": "Alice Dev",
                "nickname": "alice",
                "account_id": "5c355119b393bf4ce9561ec3"
            },
            "links": {
                "html": { "href": "https://bitbucket.org/acme/my-repo/pull-requests/5349" }
            },
            "comment_count": 3,
            "task_count": 0
        });

        let row = map_pr("acme/my-repo", raw).expect("should map successfully");
        assert_eq!(row.guid, "acme/my-repo#5349");
        assert_eq!(row.repo, "acme/my-repo");
        assert_eq!(row.id, 5349);
        assert_eq!(row.title, "feat: Add dark mode support");
        assert_eq!(row.state, "MERGED");
        assert_eq!(row.created_on, "2026-06-05T16:52:02.943122+00:00");
        assert_eq!(row.updated_on.as_deref(), Some("2026-06-16T21:52:27.468417+00:00"));
        assert_eq!(row.source_branch.as_deref(), Some("feature/dark-mode"));
        assert_eq!(row.destination_branch.as_deref(), Some("main"));
        assert_eq!(row.author_display_name.as_deref(), Some("Alice Dev"));
        assert_eq!(row.author_nickname.as_deref(), Some("alice"));
        assert_eq!(
            row.description.as_deref(),
            Some("Implements a dark mode toggle in the settings panel.")
        );
        assert_eq!(
            row.html_url.as_deref(),
            Some("https://bitbucket.org/acme/my-repo/pull-requests/5349")
        );
        // Extra keys preserved (comment_count, task_count) but mapped keys removed.
        assert!(!row.extra.contains_key("id"), "id consumed");
        assert!(!row.extra.contains_key("title"), "title consumed");
        assert!(
            row.extra.contains_key("comment_count"),
            "comment_count preserved in extra"
        );
    }

    #[test]
    fn map_pr_missing_id_returns_none() {
        let raw = serde_json::json!({
            "created_on": "2026-06-05T16:52:02.943122+00:00",
            "title": "no id"
        });
        assert!(map_pr("ws/repo", raw).is_none(), "no id → None");
    }

    #[test]
    fn map_pr_missing_created_on_returns_none() {
        let raw = serde_json::json!({ "id": 1, "title": "no created_on" });
        assert!(map_pr("ws/repo", raw).is_none(), "no created_on → None");
    }

    // -----------------------------------------------------------------------
    // Cursor round-trip tests.

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-bitbucket-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn cursor_round_trips() {
        let v = temp_vault("cursor");
        let state = SyncState {
            username: Some("alice".into()),
            account_id: Some("5c355119b393bf4ce9561ec3".into()),
            commits_since: Some("2026-06-01T00:00:00+00:00".into()),
            prs_updated: Some("2026-06-10T00:00:00+00:00".into()),
            updated: None,
        };
        v.write_bitbucket_sync(&state).unwrap();
        let back = v.read_bitbucket_sync();
        assert_eq!(back.username.as_deref(), Some("alice"));
        assert_eq!(back.account_id.as_deref(), Some("5c355119b393bf4ce9561ec3"));
        assert_eq!(back.commits_since.as_deref(), Some("2026-06-01T00:00:00+00:00"));
        assert_eq!(back.prs_updated.as_deref(), Some("2026-06-10T00:00:00+00:00"));
    }

    #[test]
    fn empty_cursor_deserializes() {
        let v = temp_vault("empty-cursor");
        let state = v.read_bitbucket_sync();
        assert!(state.username.is_none());
        assert!(state.commits_since.is_none());
        assert!(state.prs_updated.is_none());
    }

    // -----------------------------------------------------------------------
    // Integration tests via mock API.

    /// Scripted mock API: each registered path returns a pre-canned response.
    struct MockApi {
        pages: RefCell<std::collections::HashMap<String, VecDeque<Page>>>,
        user: String,
    }

    impl MockApi {
        fn new(user: &str) -> Self {
            MockApi {
                pages: RefCell::new(std::collections::HashMap::new()),
                user: user.to_string(),
            }
        }

        /// Register a one-page response for a path prefix (the mock matches by
        /// prefix so test code doesn't need to spell out exact query strings).
        fn add_page(&self, path_prefix: &str, items: Vec<Value>) {
            self.pages
                .borrow_mut()
                .entry(path_prefix.to_string())
                .or_default()
                .push_back(Page { items, next: None });
        }

        fn find_page(&self, url: &str) -> Result<Page, FetchError> {
            let mut map = self.pages.borrow_mut();
            for (prefix, queue) in map.iter_mut() {
                if url.contains(prefix.as_str()) {
                    if let Some(p) = queue.pop_front() {
                        return Ok(p);
                    }
                }
            }
            // No registered response → empty page (simulates no-data endpoint).
            Ok(Page { items: Vec::new(), next: None })
        }
    }

    impl BitbucketApi for MockApi {
        fn get_page(&self, path: &str) -> Result<Page, FetchError> {
            self.find_page(path)
        }

        fn get_page_abs(&self, url: &str) -> Result<Page, FetchError> {
            self.find_page(url)
        }

        fn get_one(&self, _path: &str) -> Result<Value, FetchError> {
            Ok(serde_json::json!({ "nickname": self.user, "account_id": "acc-001" }))
        }

        fn current_user(&self) -> Result<String, FetchError> {
            Ok(self.user.clone())
        }

        fn current_account_id(&self) -> Result<String, FetchError> {
            // The stable account_id used in workspace PR paths.
            Ok("acc-001".to_string())
        }
    }

    #[test]
    fn pull_with_commits_and_prs_writes_vault() {
        let vault = temp_vault("pull-full");
        let api = MockApi::new("alice");

        // Workspaces response.
        api.add_page(
            "/user/permissions/workspaces",
            vec![serde_json::json!({
                "workspace": { "slug": "acme", "type": "workspace" },
                "permission": "owner"
            })],
        );

        // Repos response.
        api.add_page(
            "/repositories/acme",
            vec![
                serde_json::json!({ "slug": "my-repo", "full_name": "acme/my-repo", "type": "repository" }),
            ],
        );

        // Commits response for the repo.
        api.add_page(
            "/repositories/acme/my-repo/commits",
            vec![serde_json::json!({
                "type": "commit",
                "hash": "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                "date": "2026-06-10T10:00:00+00:00",
                "message": "initial commit",
                "author": {
                    "type": "author",
                    "raw": "Alice Dev <alice@example.com>",
                    "user": {
                        "nickname": "alice",
                        "account_id": "acc-001",
                        "display_name": "Alice Dev"
                    }
                },
                "links": {
                    "html": { "href": "https://bitbucket.org/acme/my-repo/commits/deadbeefdeadbeefdeadbeefdeadbeefdeadbeef" }
                }
            })],
        );

        // Workspaces response for PRs (sync_prs also calls /user/permissions/workspaces).
        api.add_page(
            "/user/permissions/workspaces",
            vec![serde_json::json!({
                "workspace": { "slug": "acme", "type": "workspace" },
                "permission": "owner"
            })],
        );

        // PRs response — workspace-scoped endpoint with account_id path segment.
        // sync_prs calls GET /workspaces/{ws}/pullrequests/{account_id}
        api.add_page(
            "/workspaces/acme/pullrequests/acc-001",
            vec![serde_json::json!({
                "type": "pullrequest",
                "id": 42,
                "title": "Add amazing feature",
                "state": "OPEN",
                "created_on": "2026-06-08T14:00:00+00:00",
                "updated_on": "2026-06-09T15:00:00+00:00",
                "description": "This PR adds an amazing feature.",
                "source": {
                    "branch": { "name": "feature/amazing" },
                    "repository": { "full_name": "acme/my-repo" }
                },
                "destination": {
                    "branch": { "name": "main" },
                    "repository": { "full_name": "acme/my-repo" }
                },
                "author": { "display_name": "Alice Dev", "nickname": "alice", "account_id": "acc-001" },
                "links": { "html": { "href": "https://bitbucket.org/acme/my-repo/pull-requests/42" } },
                "comment_count": 0,
                "task_count": 0
            })],
        );

        let outcome = pull_with(&vault, &api).expect("pull_with should succeed");
        assert_eq!(outcome.counts.get("commits").copied().unwrap_or(0), 1, "one new commit");
        assert_eq!(outcome.counts.get("prs").copied().unwrap_or(0), 1, "one new PR");

        // Verify vault files exist.
        assert!(
            vault.root().join("developer/bitbucket/commits/2026-06.jsonl").exists(),
            "commits partition created"
        );
        assert!(
            vault.root().join("developer/bitbucket/prs/2026-06.jsonl").exists(),
            "PRs partition created"
        );

        // Verify cursor was advanced.
        let state = vault.read_bitbucket_sync();
        assert_eq!(state.username.as_deref(), Some("alice"));
        assert!(state.commits_since.is_some(), "commits cursor advanced");
        assert!(state.prs_updated.is_some(), "prs cursor advanced");
    }

    #[test]
    fn idempotent_resync_no_duplicates() {
        let vault = temp_vault("idempotent");
        let api = MockApi::new("alice");

        let commit = serde_json::json!({
            "type": "commit",
            "hash": "cafebabecafebabecafebabecafebabecafebabe",
            "date": "2026-05-15T08:00:00+00:00",
            "message": "refactor: clean up",
            "author": {
                "raw": "Alice Dev <alice@example.com>",
                "user": { "nickname": "alice", "account_id": "acc-001", "display_name": "Alice Dev" }
            },
            "links": { "html": { "href": "https://bitbucket.org/acme/repo/commits/cafebabe" } }
        });

        // First run.
        api.add_page("/user/permissions/workspaces", vec![
            serde_json::json!({ "workspace": { "slug": "acme" } })
        ]);
        api.add_page("/repositories/acme", vec![
            serde_json::json!({ "slug": "repo", "full_name": "acme/repo" })
        ]);
        api.add_page("/repositories/acme/repo/commits", vec![commit.clone()]);
        // sync_prs also calls /user/permissions/workspaces then workspace PR endpoint.
        api.add_page("/user/permissions/workspaces", vec![
            serde_json::json!({ "workspace": { "slug": "acme" } })
        ]);
        api.add_page("/workspaces/acme/pullrequests/acc-001", vec![]);

        let out1 = pull_with(&vault, &api).unwrap();
        assert_eq!(out1.counts.get("commits").copied().unwrap_or(0), 1);

        // Second run with the same commit — should be deduped.
        api.add_page("/user/permissions/workspaces", vec![
            serde_json::json!({ "workspace": { "slug": "acme" } })
        ]);
        api.add_page("/repositories/acme", vec![
            serde_json::json!({ "slug": "repo", "full_name": "acme/repo" })
        ]);
        api.add_page("/repositories/acme/repo/commits", vec![commit]);
        api.add_page("/user/permissions/workspaces", vec![
            serde_json::json!({ "workspace": { "slug": "acme" } })
        ]);
        api.add_page("/workspaces/acme/pullrequests/acc-001", vec![]);

        let out2 = pull_with(&vault, &api).unwrap();
        assert_eq!(out2.counts.get("commits").copied().unwrap_or(0), 0, "duplicate deduped");

        // Exactly one row in the partition.
        let stream = vault.stream(COMMITS_DIR, crate::store::Partition::Month);
        let rows: Vec<CommitRow> = stream.read("2026-05").unwrap();
        assert_eq!(rows.len(), 1, "no duplicate rows on disk");
    }

    #[test]
    fn urlencode_handles_special_chars() {
        assert_eq!(urlencode("hello"), "hello");
        assert_eq!(urlencode("date>\"2026-06-01T00:00:00+00:00\""), "date%3E%222026-06-01T00%3A00%3A00%2B00%3A00%22");
    }

    #[test]
    fn advance_cursor_only_moves_forward() {
        let mut cursor: Option<String> = None;
        advance(&mut cursor, "2026-06-01".into());
        assert_eq!(cursor.as_deref(), Some("2026-06-01"));
        advance(&mut cursor, "2026-05-01".into()); // older — no-op
        assert_eq!(cursor.as_deref(), Some("2026-06-01"));
        advance(&mut cursor, "2026-07-01".into()); // newer — advances
        assert_eq!(cursor.as_deref(), Some("2026-07-01"));
    }
}
