//! GitHub — periodic cloud sync of commits, PRs, issues, stars, and gists via
//! the official REST API. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/github.md.
//!
//! A **Periodic** cloud pull. Two destinations, written by independent record
//! types in one pass:
//!
//! - **raw firehose** under `developer/github/` (the [`crate::claude_code`]
//!   `developer/` raw-only taxonomy — no media-plays/domain contract, this
//!   module owns each row shape):
//!   - `developer/github/commits/YYYY-MM.jsonl` — your authored commits across
//!     every repo, partitioned by the commit's author date.
//!   - `developer/github/issues/YYYY-MM.jsonl` and `.../prs/YYYY-MM.jsonl` —
//!     issues/PRs you're involved in, partitioned by their creation month.
//!   - `developer/github/stars.jsonl` and `.../gists.jsonl` — flat snapshots,
//!     fully re-pulled each sync (bounded lists, simplest correct dedup).
//! - **tasks contract** under `tasks/github/` (the *existing* Rust-bound
//!   [`crate::tasks`] contract): issues *assigned to you and still open*
//!   become [`Task`]s, persisted via `apply_tasks_sync` — so the same assigned
//!   issue appears in BOTH the issues firehose and the task store.
//!
//! Auth is a fine-grained personal access token (a secret), pasted via the
//! connection's [`ConnectMethod::TokenPaste`] and stored under `.trove/sync/`
//! (0600) like Oura's PAT — it rides the `access_token` slot of a
//! never-expiring [`TokenSet`]. The username is discovered from `GET /user` at
//! connect time and cached in the non-secret cursor; the PAT itself never
//! leaves the secret store (never logged, never in the cursor or any vault
//! file).
//!
//! ## Cursors / dedup
//!
//! `.trove/github-sync.json` (non-secret, rebuildable — the [`crate::lastfm`]
//! cursor placement) holds the discovered login plus a `since`/`updated:>=`
//! watermark per incremental endpoint (commits, issues, prs). stars/gists need
//! no cursor (full snapshot every time). A cursor advances only after its
//! endpoint fully drains this tick (all `Link` pages walked), so a partial
//! fetch never strands items behind an advanced watermark.
//!
//! commits/issues/prs use **upsert-into-partition** (read the target month
//! partition, merge by a stable `guid`, rewrite sorted) — the
//! [`crate::claude_code`] idiom — so a re-sync over an overlapping window never
//! duplicates a guid.

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

// Deferred scope (intentionally not built in v1):
// TODO(github): commit `stats` (additions/deletions) — the repo-commits LIST
//   response omits them; fetching needs one extra single-commit GET per sha.
// TODO(github): account-archive ZIP import backfill for full history beyond the
//   search 1000-result cap — needs a real archive sample (format undocumented);
//   a separate `Behavior::Import` DEF, like letterboxd.rs.
// TODO(github): an OAuth connect method — TokenPaste (fine-grained PAT) suffices
//   for v1; OAuth would add a baked GitHub App + the device/web flow.

// Raw firehose directories (developer/ taxonomy, raw-only).
const COMMITS_DIR: &str = "developer/github/commits";
const ISSUES_DIR: &str = "developer/github/issues";
const PRS_DIR: &str = "developer/github/prs";
const STARS_FILE: &str = "developer/github/stars.jsonl";
const GISTS_FILE: &str = "developer/github/gists.jsonl";
/// Probe target for `last_data` — the issues firehose is the liveliest stream.
const LAST_DATA_DIR: &str = "developer/github/issues";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that's for 0600
/// secrets). Deleting it re-discovers the login and re-walks every endpoint.
const SYNC_FILE: &str = ".trove/github-sync.json";

/// The service id under `.trove/sync/` where the PAT is stored (the Oura-PAT
/// slot: the token rides a never-expiring [`TokenSet`]).
const SERVICE: &str = "github";

const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2022-11-28";
const USER_AGENT: &str = "Trove (https://github.com/; personal-data-vault)";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs in the watcher loop. Hourly: dev activity trickles in
/// and the incremental polls are cheap when idle.
pub const GITHUB_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(LAST_DATA_DIR))
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
                    "github synced — {} commits, {} issues, {} prs, {} stars, {} gists, {} tasks",
                    c("commits"),
                    c("issues"),
                    c("prs"),
                    c("stars"),
                    c("gists"),
                    c("tasks"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "github sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let total: u64 = out.counts.values().sum();
    let headline = if total == 0 {
        "GitHub is up to date — no new activity".to_string()
    } else {
        let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
        format!(
            "GitHub synced — {} commits, {} issues, {} PRs",
            c("commits"),
            c("issues"),
            c("prs"),
        )
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "github",
        name: "GitHub",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your GitHub activity — commits, pull requests, issues, \
                      starred repositories, and gists — via the official REST API \
                      using a fine-grained personal access token. Issues assigned \
                      to you also flow into the unified task store.",
        domain: "developer",
        vault_path: "developer/github/",
        toggleable: true,
        setup: &[
            "Connect with a GitHub fine-grained personal access token on this card.",
            "Each sync fetches new commits, issues, and PRs incrementally; stars and gists are re-snapshotted.",
        ],
        caveats: "The search-based issue/PR pull caps at 1000 results per query and uses a \
                  separate 30-requests-per-minute budget; a full historical backfill needs the \
                  account-archive ZIP import (not yet built). Commit additions/deletions aren't \
                  fetched (the repo-commits list omits them). Rate-limited endpoints stop \
                  gracefully for the tick and resume next time.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(GITHUB_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("github"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a fine-grained PAT, a SECRET).

/// Verify the pasted PAT with `GET /user`, then store it (0600) and cache the
/// discovered login in the non-secret cursor. A 401 bails with a clear
/// message; the token is never logged. The login is needed by every pull
/// query (`author=`, `involves:`, `assignee:`), so we resolve it once here.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste a GitHub fine-grained personal access token");
    }
    let client = GithubClient::new(API_BASE.to_string(), token.to_string());
    let login = match client.viewer_login() {
        Ok(login) => login,
        Err(FetchError::Unauthorized) => bail!(
            "GitHub rejected the token (401) — check it has the listed read permissions and hasn't expired"
        ),
        Err(e) => bail!("GitHub /user check failed: {e}"),
    };
    // The PAT goes ONLY through the secret store (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        },
    )?;
    // Cache the discovered login in the non-secret cursor for the pull.
    let mut state = vault.read_github_sync();
    state.login = Some(login);
    vault.write_github_sync(&state)
}

/// Forget the stored PAT. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the PAT is stored; the label is the cached login when known.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        let label = vault
            .read_github_sync()
            .login
            .unwrap_or_else(|| "GitHub".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // fine-grained PATs expire, but we don't parse it
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: a PAT is self-service, so always "configured".
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "github",
    display_name: "GitHub",
    methods: &[ConnectMethod::TokenPaste {
        label: "GitHub token",
        help: "Paste a GitHub fine-grained personal access token. Create one at \
               github.com/settings/personal-access-tokens with read access to Contents, Issues, \
               Pull requests, Metadata, plus account Starring + Gists.",
        placeholder: "github_pat_…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["github"],
    setup: &[
        "Go to github.com/settings/personal-access-tokens and generate a fine-grained token.",
        "Grant read access to Contents, Issues, Pull requests, and Metadata (repository permissions), plus Starring and Gists (account permissions).",
        "Paste the token here — it's stored locally and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page plus the URL to follow for the next page (the `Link` header's
/// `rel="next"` target, absolute). `None` = last page.
struct Page {
    items: Vec<Value>,
    next: Option<String>,
}

/// Status-level fetch errors (the oura/trakt split): 401/403/429 want distinct
/// handling, everything else is a message. A `RateLimited` stops the offending
/// endpoint for this tick (cursor left unadvanced) rather than waiting out the
/// reset inside the watcher loop.
#[derive(Debug)]
enum FetchError {
    /// Primary/secondary rate limit hit (403/429 with remaining=0, or 429).
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

/// The endpoints the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait GithubApi {
    /// `GET <path>` (path is API-relative, e.g. `/user/repos?...`) → one page +
    /// the `rel="next"` URL. `accept` overrides the default `Accept` media type
    /// for this call only (e.g. `application/vnd.github.star+json` so
    /// `/user/starred` returns `starred_at`); `None` = the default JSON type.
    fn get_page(&self, path: &str, accept: Option<&str>) -> Result<Page, FetchError>;
    /// Follow an absolute next-page URL from a `Link` header. `accept` carries
    /// the same per-call override so paged endpoints keep their media type.
    fn get_page_abs(&self, url: &str, accept: Option<&str>) -> Result<Page, FetchError>;
    /// `GET <path>` returning a single object (no pagination).
    fn get_one(&self, path: &str) -> Result<Value, FetchError>;
    /// The authenticated user's login (`GET /user` → `login`).
    fn viewer_login(&self) -> Result<String, FetchError>;
}

/// Thin client; base URL injected (the oura/trakt/lastfm pattern).
struct GithubClient {
    base: String,
    token: String,
}

impl GithubClient {
    fn new(base: String, token: String) -> Self {
        GithubClient { base, token }
    }

    /// Shared request builder with the standard GitHub headers. `accept`
    /// overrides the default media type for this call only (e.g. the star
    /// media type that makes `/user/starred` return `starred_at`).
    fn req_accept(&self, url: &str, accept: Option<&str>) -> ureq::Request {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", accept.unwrap_or("application/vnd.github+json"))
            .set("X-GitHub-Api-Version", API_VERSION)
            .set("User-Agent", USER_AGENT)
    }

    /// Shared request builder with the default JSON `Accept` (single-object
    /// GETs and `/user`).
    fn req(&self, url: &str) -> ureq::Request {
        self.req_accept(url, None)
    }

    /// Map a ureq result into our [`FetchError`] split, distinguishing a
    /// rate-limit 403 (remaining=0) from a plain forbidden.
    fn handle(resp: std::result::Result<ureq::Response, ureq::Error>) -> Result<Page, FetchError> {
        match resp {
            Ok(resp) => {
                let next = parse_link_next(resp.header("link"));
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                let items = match v {
                    // /search/issues wraps items in `{ items: [...] }`; list
                    // endpoints return a bare array.
                    Value::Object(ref o) => o
                        .get("items")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                    Value::Array(a) => a,
                    _ => Vec::new(),
                };
                Ok(Page { items, next })
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code @ (403 | 429), resp)) => Err(classify_limit(code, &resp)),
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

/// A 403/429 is a rate limit only when `x-ratelimit-remaining: 0` or a
/// `retry-after` is present; otherwise it's a genuine forbidden (bad perms).
fn classify_limit(code: u16, resp: &ureq::Response) -> FetchError {
    let remaining = resp
        .header("x-ratelimit-remaining")
        .and_then(|s| s.trim().parse::<i64>().ok());
    let retry_after = resp
        .header("retry-after")
        .and_then(|s| s.trim().parse::<u64>().ok());
    if code == 429 || remaining == Some(0) || retry_after.is_some() {
        FetchError::RateLimited
    } else {
        FetchError::Other(format!("HTTP {code} (forbidden — check token permissions)"))
    }
}

impl GithubApi for GithubClient {
    fn get_page(&self, path: &str, accept: Option<&str>) -> Result<Page, FetchError> {
        GithubClient::handle(self.req_accept(&format!("{}{path}", self.base), accept).call())
    }

    fn get_page_abs(&self, url: &str, accept: Option<&str>) -> Result<Page, FetchError> {
        GithubClient::handle(self.req_accept(url, accept).call())
    }

    fn get_one(&self, path: &str) -> Result<Value, FetchError> {
        match self.req(&format!("{}{path}", self.base)).call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code @ (403 | 429), resp)) => Err(classify_limit(code, &resp)),
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

    fn viewer_login(&self) -> Result<String, FetchError> {
        let v = self.get_one("/user")?;
        v.get("login")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| FetchError::Other("GET /user returned no login".into()))
    }
}

/// Extract the `rel="next"` URL from a `Link` header (RFC 5988 / GitHub
/// pagination). We follow `next` only — never compute from `last`, so a
/// shrinking result set can't over-page. `None` when absent.
fn parse_link_next(header: Option<&str>) -> Option<String> {
    let header = header?;
    for part in header.split(',') {
        // `<https://api.github.com/...&page=2>; rel="next"`
        let mut bits = part.splitn(2, ';');
        let url_part = bits.next()?.trim();
        let rel_part = bits.next()?.trim();
        if rel_part.contains("rel=\"next\"") {
            let url = url_part.trim_start_matches('<').trim_end_matches('>').trim();
            if !url.is_empty() {
                return Some(url.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The authenticated login, discovered at connect time. Every pull query
    /// keys off it; not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    login: Option<String>,
    /// commits: max `commit.author.date` (RFC3339, as GitHub returns) ever
    /// written. Next pull asks `since=<this>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commits_since: Option<String>,
    /// issues: max `updated_at` ever seen. Next pull asks `updated:>=<this>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issues_updated: Option<String>,
    /// prs: max `updated_at` ever seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prs_updated: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_github_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_github_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shapes (this module's own — developer/ is contract-free). Each keeps
// a synthesized `guid` for dedup, maps the documented fields, and flattens any
// unknown keys into `extra` so full source fidelity survives.

/// A repo from `GET /user/repos`, slimmed to what the commit walk needs:
/// `full_name` already encodes owner/repo, and `pushed_at` lets us skip repos
/// with no pushes in the window. (The repo list is a traversal aid, not a
/// vault stream — owner/name/default_branch live in each commit's raw row, so
/// they aren't re-modeled here.)
#[derive(Debug, Clone, Deserialize)]
struct Repo {
    full_name: String,
    #[serde(default)]
    pushed_at: Option<String>,
}

/// One commit row in `developer/github/commits/YYYY-MM.jsonl`.
/// `guid = "<repo_full_name>:<sha>"`; partitioned by `commit.author.date`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitRow {
    pub guid: String,
    pub repo: String,
    pub sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default)]
    pub message: String,
    /// RFC3339 UTC as GitHub returns it (`commit.author.date`).
    pub authored_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_email: Option<String>,
    /// The GitHub account login — **nullable** in the API (a commit by an
    /// email not linked to an account has `author: null`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author_login: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    /// Any unknown keys, preserved verbatim.
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

/// One issue or PR row (same shape; the `prs` partition gets the PRs).
/// `guid = node_id`; partitioned by `created_at`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueRow {
    pub guid: String,
    pub number: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: String,
    /// RFC3339 UTC (`created_at`) — the partition key.
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Nullable in the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    /// Derived owner/repo (split off `repository_url`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repo: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Nullable single assignee, plus the full list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// True when `pull_request` was present on the search item.
    #[serde(default)]
    pub is_pr: bool,
    /// Nullable body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Map::is_empty", flatten)]
    pub extra: Map<String, Value>,
}

/// One starred repo in `developer/github/stars.jsonl`. `guid = repo node_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StarRow {
    pub guid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starred_at: Option<String>,
    #[serde(default)]
    pub repo_full_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_html_url: Option<String>,
}

/// One gist in `developer/github/gists.jsonl`. `guid = node_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GistRow {
    pub guid: String,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub html_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub public: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    /// Filenames (the API's `files` is an object keyed by filename).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested). `*_obj` helpers consume mapped keys off a
// JSON object and flatten the rest into `extra`, so unknown source keys never
// drop.

/// Pull a string field, trimmed; `None` when missing/non-string/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A `*.login` two levels down, tolerating a null parent (commit `author`,
/// issue `assignee`).
fn nested_login(v: &Value, parent: &str) -> Option<String> {
    v.get(parent)
        .and_then(|p| p.as_object())
        .and_then(|o| o.get("login"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `repository_url` ("https://api.github.com/repos/octocat/Hello-World") →
/// "octocat/Hello-World". Empty when unparseable.
fn repo_from_api_url(url: &str) -> String {
    match url.split("/repos/").nth(1) {
        Some(tail) => tail.trim_end_matches('/').to_string(),
        None => String::new(),
    }
}

/// One repo-commits list item → [`CommitRow`]. Consumes mapped keys; the rest
/// of the object lands in `extra`. `None` only when it has no sha or no author
/// date (can't be partitioned).
fn map_commit(repo_full_name: &str, value: Value) -> Option<CommitRow> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let sha = match obj.remove("sha") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other {
                obj.insert("sha".into(), o);
            }
            return None;
        }
    };
    // `commit` sub-object carries message + author/committer dates.
    let commit = obj.remove("commit").unwrap_or(Value::Null);
    let authored_at = commit
        .get("author")
        .and_then(|a| a.get("date"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let row = CommitRow {
        guid: format!("{repo_full_name}:{sha}"),
        repo: repo_full_name.to_string(),
        sha,
        node_id: str_opt(&Value::Object(obj.clone()), "node_id"),
        message: commit
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        authored_at,
        committed_at: commit
            .get("committer")
            .and_then(|c| c.get("date"))
            .and_then(Value::as_str)
            .map(str::to_string),
        author_name: commit
            .get("author")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        author_email: commit
            .get("author")
            .and_then(|a| a.get("email"))
            .and_then(Value::as_str)
            .map(str::to_string),
        // `author` (top-level) is the GitHub account — nullable.
        author_login: nested_login(&Value::Object(obj.clone()), "author"),
        html_url: str_opt(&Value::Object(obj.clone()), "html_url"),
        extra: {
            // Keep unknown keys; drop the ones we mapped or that are noisy
            // duplicates (author/committer sub-objects, node_id, html_url).
            obj.remove("node_id");
            obj.remove("html_url");
            obj.remove("author");
            obj.remove("committer");
            obj
        },
    };
    Some(row)
}

/// One `/search/issues` item → [`IssueRow`]. `is_pr` flags PRs. `None` only
/// when it has no `created_at` (can't be partitioned).
fn map_issue(value: Value) -> Option<IssueRow> {
    let Value::Object(mut obj) = value else {
        return None;
    };
    let created_at = match obj.remove("created_at") {
        Some(Value::String(s)) if !s.is_empty() => s,
        other => {
            if let Some(o) = other {
                obj.insert("created_at".into(), o);
            }
            return None;
        }
    };
    let snap = Value::Object(obj.clone());
    let repo = obj
        .get("repository_url")
        .and_then(Value::as_str)
        .map(repo_from_api_url)
        .unwrap_or_default();
    let labels = obj
        .get("labels")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let assignees = obj
        .get("assignees")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("login").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let is_pr = obj.get("pull_request").is_some();
    let row = IssueRow {
        guid: str_opt(&snap, "node_id").unwrap_or_else(|| {
            // Fall back to a stable composite if node_id is ever absent.
            format!(
                "{repo}#{}",
                obj.get("number").and_then(Value::as_i64).unwrap_or(0)
            )
        }),
        number: obj.get("number").and_then(Value::as_i64).unwrap_or(0),
        node_id: str_opt(&snap, "node_id"),
        title: obj.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        state: obj.get("state").and_then(Value::as_str).unwrap_or("").to_string(),
        created_at,
        updated_at: str_opt(&snap, "updated_at"),
        closed_at: str_opt(&snap, "closed_at"),
        html_url: str_opt(&snap, "html_url"),
        repo,
        labels,
        assignee: nested_login(&snap, "assignee"),
        assignees,
        is_pr,
        body: obj
            .get("body")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        extra: {
            for k in [
                "number",
                "node_id",
                "title",
                "state",
                "updated_at",
                "closed_at",
                "html_url",
                "labels",
                "assignee",
                "assignees",
                "body",
            ] {
                obj.remove(k);
            }
            obj
        },
    };
    Some(row)
}

/// One `/user/starred` item → [`StarRow`]. With the star+json media type the
/// item is `{ starred_at, repo: {...} }`; we read `repo` when present and the
/// star timestamp alongside it. As a SAFETY NET against a missing Accept header
/// (which makes GitHub return a BARE repo object with no `repo` wrapper and no
/// `starred_at`), fall back to reading the repo fields off the top level — so a
/// header regression degrades to "no starred_at" instead of dropping the row.
fn map_star(value: Value) -> Option<StarRow> {
    // `{ starred_at, repo: {...} }` (star+json) vs a bare repo object.
    let repo = value.get("repo").unwrap_or(&value);
    let node_id = repo.get("node_id").and_then(Value::as_str)?.to_string();
    Some(StarRow {
        guid: node_id.clone(),
        starred_at: str_opt(&value, "starred_at"),
        repo_full_name: repo
            .get("full_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        repo_id: repo.get("id").and_then(Value::as_i64),
        repo_node_id: Some(node_id),
        repo_html_url: repo.get("html_url").and_then(Value::as_str).map(str::to_string),
    })
}

/// One `/gists` item → [`GistRow`].
fn map_gist(value: Value) -> Option<GistRow> {
    let node_id = value.get("node_id").and_then(Value::as_str)?.to_string();
    let files = value
        .get("files")
        .and_then(Value::as_object)
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    Some(GistRow {
        guid: node_id.clone(),
        id: value.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        node_id: Some(node_id),
        html_url: value.get("html_url").and_then(Value::as_str).map(str::to_string),
        description: value
            .get("description")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        public: value.get("public").and_then(Value::as_bool).unwrap_or(false),
        created_at: str_opt(&value, "created_at"),
        updated_at: str_opt(&value, "updated_at"),
        files,
    })
}

/// An [`IssueRow`] assigned to the viewer → a normalized [`Task`]. Times become
/// RFC3339 **local** (the task contract is local time); the raw layer keeps the
/// source's UTC. `project` is the repo full name.
fn issue_to_task(row: &IssueRow) -> Task {
    let mut extra = Map::new();
    extra.insert("number".into(), Value::from(row.number));
    if let Some(url) = &row.html_url {
        extra.insert("html_url".into(), Value::from(url.clone()));
    }
    if !row.repo.is_empty() {
        extra.insert(
            "repository_url".into(),
            Value::from(format!("{API_BASE}/repos/{}", row.repo)),
        );
    }
    extra.insert(
        "assignees".into(),
        Value::from(row.assignees.iter().map(|s| Value::from(s.clone())).collect::<Vec<_>>()),
    );
    extra.insert("state".into(), Value::from(row.state.clone()));
    Task {
        source: "github".into(),
        id: row.guid.clone(),
        title: row.title.clone(),
        project: row.repo.clone(),
        notes: row.body.clone().unwrap_or_default(),
        status: "open".into(),
        priority: 0,
        due: None,
        start: None,
        all_day: false,
        recurrence: None,
        tags: row.labels.clone(),
        subtasks: Vec::new(),
        created: Some(to_local(&row.created_at)),
        modified: row.updated_at.as_deref().map(to_local),
        completed: None,
        extra,
    }
}

/// An RFC3339 UTC timestamp → RFC3339 local. Unparseable values pass through
/// verbatim rather than being dropped.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

// ---------------------------------------------------------------------------
// Upsert-into-partition (the claude_code idiom): read the target month, merge
// new rows by guid keeping the freshest, rewrite that partition sorted. A
// re-sync over an overlapping window never duplicates a guid.

/// Group rows by their partition key (`ts(row)`'s month), then upsert each
/// group into its `<dir>/<month>.jsonl`. Returns the number of *new* guids
/// (rows whose guid wasn't already on disk) for the headline count.
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
    // Bucket fresh rows by month.
    let mut by_month: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month
            .key(ts(&r))
            .with_context(|| format!("github: row ts {:?} has no month prefix", ts(&r)))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<T> = stream.read(&month)?;
        // Index existing by guid.
        let mut idx: BTreeMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (guid(r).to_string(), i))
            .collect();
        for r in fresh {
            match idx.get(guid(&r)).copied() {
                Some(i) => {
                    // Replace only if the fresh row is at least as new (keep
                    // the freshest; a stale re-fetch never clobbers).
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
        // Deterministic on-disk order: sort by (ts, guid).
        existing.sort_by(|a, b| {
            ts(a).cmp(ts(b)).then_with(|| guid(a).cmp(guid(b)))
        });
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

/// Write a flat snapshot file deduped by guid (stars/gists). Fully replaces
/// the file each sync.
fn write_flat<T>(vault: &Vault, rel: &str, rows: Vec<T>, guid: impl Fn(&T) -> &str) -> Result<u64>
where
    T: Serialize,
{
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if seen.insert(guid(&r).to_string()) {
            out.push(r);
        }
    }
    let n = out.len() as u64;
    vault.write_snapshot(rel, &out)?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync every stream. Missing token ⇒ a quiet skip
/// (mirror lastfm's "missing creds is not an error" — here surfaced to the
/// manual path as a clear error, swallowed by the periodic path).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("GitHub is not connected — add a personal access token in the Integrations tab")?;
    let client = GithubClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam. Each endpoint
/// drains fully (all `Link` pages) before its cursor advances; a rate limit
/// stops *that* endpoint for the tick (cursor unadvanced) and the others
/// proceed.
fn pull_with(vault: &Vault, api: &impl GithubApi) -> Result<PullOutcome> {
    let mut state = vault.read_github_sync();
    // The login is required for every query; discover + cache it if missing
    // (e.g. an older cursor, or a token saved out of band).
    let login = match state.login.clone() {
        Some(l) => l,
        None => {
            let l = api
                .viewer_login()
                .map_err(|e| anyhow::anyhow!("GitHub /user lookup failed: {e}"))?;
            state.login = Some(l.clone());
            l
        }
    };

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- commits ---------------------------------------------------------
    match sync_commits(vault, api, &login, state.commits_since.as_deref()) {
        Ok((n, max_date)) => {
            counts.insert("commits", n);
            if let Some(d) = max_date {
                advance(&mut state.commits_since, d);
            }
        }
        Err(SyncStop::RateLimited) => { /* leave cursor; resume next tick */ }
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- issues ----------------------------------------------------------
    match sync_search(vault, api, &login, "is:issue", ISSUES_DIR, state.issues_updated.as_deref()) {
        Ok((n, max_updated, _)) => {
            counts.insert("issues", n);
            if let Some(u) = max_updated {
                advance(&mut state.issues_updated, u);
            }
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- prs -------------------------------------------------------------
    match sync_search(vault, api, &login, "is:pr", PRS_DIR, state.prs_updated.as_deref()) {
        Ok((n, max_updated, _)) => {
            counts.insert("prs", n);
            if let Some(u) = max_updated {
                advance(&mut state.prs_updated, u);
            }
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- stars (full snapshot) -------------------------------------------
    match sync_stars(vault, api) {
        Ok(n) => {
            counts.insert("stars", n);
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- gists (full snapshot) -------------------------------------------
    match sync_gists(vault, api) {
        Ok(n) => {
            counts.insert("gists", n);
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    // --- tasks: assigned open issues -------------------------------------
    match sync_tasks(vault, api, &login) {
        Ok(n) => {
            counts.insert("tasks", n);
        }
        Err(SyncStop::RateLimited) => {}
        Err(SyncStop::Fatal(e)) => return Err(e),
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_github_sync(&state)?;

    let total: u64 = counts.values().sum();
    Ok(PullOutcome {
        headline: format!("{total} new GitHub records"),
        counts,
    })
}

/// Move a cursor forward to `candidate` only when it's strictly greater
/// (lexical compare is correct for the RFC3339 `…Z` form GitHub returns).
fn advance(cursor: &mut Option<String>, candidate: String) {
    if cursor.as_deref().is_none_or(|c| candidate.as_str() > c) {
        *cursor = Some(candidate);
    }
}

/// Why a per-endpoint sync stopped early. A rate limit is not fatal — the
/// cursor is left unadvanced and the endpoint resumes next tick.
enum SyncStop {
    RateLimited,
    Fatal(anyhow::Error),
}

impl SyncStop {
    /// Map a [`FetchError`] at a drain site: rate limit → graceful stop;
    /// 401/other → fatal (surfaced to the manual pull).
    fn from_fetch(e: FetchError) -> Self {
        match e {
            FetchError::RateLimited => SyncStop::RateLimited,
            other => SyncStop::Fatal(anyhow::anyhow!("github fetch failed: {other}")),
        }
    }
}

/// Walk every `Link` page of an API-relative starting path, collecting all
/// items. Stops (gracefully) on a rate limit *only between pages already
/// drained* — a partial drain returns `RateLimited` so the caller leaves the
/// cursor unadvanced (never strands items). `between` is an optional polite
/// inter-page pause.
fn drain_pages(
    api: &impl GithubApi,
    start_path: &str,
    between: Duration,
) -> Result<Vec<Value>, SyncStop> {
    drain_pages_accept(api, start_path, between, None)
}

/// As [`drain_pages`], but sends `accept` as the `Accept` media type on every
/// page (first + each `rel="next"` follow). Used by `/user/starred` to request
/// `application/vnd.github.star+json` — without it GitHub returns a BARE repo
/// array (no `starred_at`, no `repo` wrapper), silently dropping every star.
fn drain_pages_accept(
    api: &impl GithubApi,
    start_path: &str,
    between: Duration,
    accept: Option<&str>,
) -> Result<Vec<Value>, SyncStop> {
    let mut items = Vec::new();
    let first = api.get_page(start_path, accept).map_err(SyncStop::from_fetch)?;
    items.extend(first.items);
    let mut next = first.next;
    while let Some(url) = next {
        thread::sleep(between);
        let page = api.get_page_abs(&url, accept).map_err(SyncStop::from_fetch)?;
        items.extend(page.items);
        next = page.next;
    }
    Ok(items)
}

/// commits: list the viewer's repos, then per repo pull commits authored by
/// the viewer since the cursor. Returns (new guids, max COMMITTER date).
/// NOTE: the list `since=` param filters by **committer** date, so the cursor
/// watermark tracks committer date (`commit.committer.date`) — not author date
/// — even though rows partition by author date. (A rebase can make committer
/// date > author date; watermarking on the author date would needlessly
/// re-fetch on the next `since=`.)
fn sync_commits(
    vault: &Vault,
    api: &impl GithubApi,
    login: &str,
    since: Option<&str>,
) -> Result<(u64, Option<String>), SyncStop> {
    // List repos (paginated). A repo with `pushed_at < since` had no pushes in
    // the window, so it can't have new authored commits — skip its call.
    let repo_items = drain_pages(
        api,
        "/user/repos?per_page=100&affiliation=owner,collaborator,organization_member",
        Duration::from_millis(0),
    )?;
    let repos: Vec<Repo> = repo_items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();

    let mut rows: Vec<CommitRow> = Vec::new();
    let mut max_date: Option<String> = None;
    for repo in &repos {
        if let (Some(since), Some(pushed)) = (since, repo.pushed_at.as_deref()) {
            if pushed < since {
                continue; // no pushes in the window — save the call
            }
        }
        let mut path = format!(
            "/repos/{}/commits?author={}&per_page=100",
            repo.full_name, login
        );
        if let Some(since) = since {
            path.push_str(&format!("&since={since}"));
        }
        // An empty repo (409) or one without commit access is non-fatal — skip.
        let items = match drain_pages(api, &path, Duration::from_millis(0)) {
            Ok(items) => items,
            Err(SyncStop::RateLimited) => return Err(SyncStop::RateLimited),
            Err(SyncStop::Fatal(_)) => continue,
        };
        for v in items {
            if let Some(row) = map_commit(&repo.full_name, v) {
                // Watermark off committer date (what `since=` filters on),
                // falling back to author date only if the committer date is
                // absent. Partitioning still uses author date (below).
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
        // A commit is immutable once authored — guid collisions are identical,
        // so keep whichever (treat any as fresh-enough to no-op).
        |_new, _old| false,
    )
    .map_err(SyncStop::Fatal)?;
    Ok((n, max_date))
}

/// issues/prs: one `/search/issues` query (`involves:<login>` since the
/// cursor), partitioned by created_at. `kind` is `is:issue` or `is:pr`.
/// Returns (new guids, max updated_at, the mapped rows — for reuse).
fn sync_search(
    vault: &Vault,
    api: &impl GithubApi,
    login: &str,
    kind: &str,
    dir: &str,
    updated_since: Option<&str>,
) -> Result<(u64, Option<String>, Vec<IssueRow>), SyncStop> {
    // Search query: involves:<login> + the kind + an updated lower bound.
    let mut q = format!("involves:{login} {kind}");
    if let Some(since) = updated_since {
        // GitHub search wants a date (or datetime) bound; use the full ISO.
        // `updated:>=<cursor>` + asc sort compose: items == cursor are
        // re-fetched and deduped (harmless), and the window starts at the
        // cursor and grows upward.
        q.push_str(&format!(" updated:>={since}"));
    }
    // `sort=updated&order=asc` is what keeps the 1000-result cap safe: oldest-
    // updated first means a truncated window drops the NEWEST tail (which the
    // cursor hasn't reached yet — we get it next tick) rather than a middle
    // slice that would be skipped forever. The cursor advances to the max
    // updated_at actually processed (the prefix's top), so progress is
    // monotonic and nothing between the truncation point and the window max is
    // lost.
    let path = format!(
        "/search/issues?q={}&advanced_search=true&sort=updated&order=asc&per_page=100",
        urlencode(&q)
    );
    // Search has a separate 30 req/min budget — pace pages a touch.
    let items = drain_pages(api, &path, Duration::from_millis(500))?;
    let rows: Vec<IssueRow> = items.into_iter().filter_map(map_issue).collect();
    // Max updated_at actually processed = the watermark. With asc ordering this
    // is the last returned row's updated_at (the top of the un-truncated
    // prefix); `.max()` is order-independent so a partial page is still safe.
    let max_updated = rows
        .iter()
        .filter_map(|r| r.updated_at.clone())
        .max();
    let n = upsert_partitioned(
        vault,
        dir,
        rows.clone(),
        |r| &r.guid,
        |r| &r.created_at,
        // Replace when the fresh row's updated_at is newer (an issue's state
        // can change — keep the latest).
        |new, old| match (new.updated_at.as_deref(), old.updated_at.as_deref()) {
            (Some(a), Some(b)) => a >= b,
            (Some(_), None) => true,
            _ => false,
        },
    )
    .map_err(SyncStop::Fatal)?;
    Ok((n, max_updated, rows))
}

/// stars: full re-pull + flat snapshot (bounded list).
fn sync_stars(vault: &Vault, api: &impl GithubApi) -> Result<u64, SyncStop> {
    // The star+json media type is REQUIRED here: it makes GitHub wrap each item
    // as `{ starred_at, repo: {...} }`. Without it the response is a bare repo
    // array (no `starred_at`, no `repo` key) and `map_star` drops every row.
    let items = drain_pages_accept(
        api,
        "/user/starred?per_page=100",
        Duration::from_millis(0),
        Some("application/vnd.github.star+json"),
    )?;
    let rows: Vec<StarRow> = items.into_iter().filter_map(map_star).collect();
    write_flat(vault, STARS_FILE, rows, |r| &r.guid).map_err(SyncStop::Fatal)
}

/// gists: full re-pull + flat snapshot.
fn sync_gists(vault: &Vault, api: &impl GithubApi) -> Result<u64, SyncStop> {
    let items = drain_pages(api, "/gists?per_page=100", Duration::from_millis(0))?;
    let rows: Vec<GistRow> = items.into_iter().filter_map(map_gist).collect();
    write_flat(vault, GISTS_FILE, rows, |r| &r.guid).map_err(SyncStop::Fatal)
}

/// tasks: assigned + open issues → the bound task contract via
/// `apply_tasks_sync`. The fate closure resolves a vanished task by fetching
/// the issue (closed ⇒ Completed; still open ⇒ Deleted i.e. just unassigned;
/// error ⇒ Unknown carry-forward). Returns created+completed+deleted.
fn sync_tasks(vault: &Vault, api: &impl GithubApi, login: &str) -> Result<u64, SyncStop> {
    let q = format!("assignee:{login} is:issue is:open");
    let path = format!(
        "/search/issues?q={}&advanced_search=true&per_page=100",
        urlencode(&q)
    );
    let items = drain_pages(api, &path, Duration::from_millis(500))?;
    let rows: Vec<IssueRow> = items.into_iter().filter_map(map_issue).collect();
    let fresh: Vec<Task> = rows.iter().map(issue_to_task).collect();
    // Distinct repos among the assigned issues, as projects.
    let mut projects: Vec<ProjectInfo> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for r in &rows {
        if !r.repo.is_empty() && seen.insert(r.repo.clone()) {
            projects.push(ProjectInfo {
                id: r.repo.clone(),
                name: r.repo.clone(),
            });
        }
    }
    let stats = vault
        .apply_tasks_sync("github", &projects, fresh, |t| github_fate(api, t))
        .map_err(SyncStop::Fatal)?;
    Ok(stats.created + stats.completed + stats.deleted)
}

/// Resolve a task that left the assigned-open set. Fetch the issue by
/// repo+number (carried in `extra`): closed ⇒ Completed(closed_at→local);
/// open ⇒ Deleted (still open, just unassigned); any error ⇒ Unknown.
fn github_fate(api: &impl GithubApi, task: &Task) -> TaskFate {
    let repo = task.project.as_str();
    let number = task.extra.get("number").and_then(Value::as_i64);
    let (repo, number) = match (repo.is_empty(), number) {
        (false, Some(n)) => (repo, n),
        _ => return TaskFate::Unknown,
    };
    match api.get_one(&format!("/repos/{repo}/issues/{number}")) {
        Ok(v) => match v.get("state").and_then(Value::as_str) {
            Some("closed") => {
                let when = v.get("closed_at").and_then(Value::as_str).map(to_local);
                TaskFate::Completed(when)
            }
            Some("open") => TaskFate::Deleted,
            _ => TaskFate::Unknown,
        },
        Err(_) => TaskFate::Unknown,
    }
}

/// Minimal percent-encoding for the search `q` (spaces, `+`, `:`, `>`, `#`,
/// `=`, `/`). GitHub search accepts `+` for spaces but we encode fully to be
/// safe across the values we build.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-github-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (realistic JSON per the official field maps) -----------

    fn repo_json(full: &str, pushed: &str) -> Value {
        serde_json::json!({
            "full_name": full,
            "owner": { "login": full.split('/').next().unwrap() },
            "name": full.split('/').nth(1).unwrap(),
            "private": false,
            "fork": false,
            "pushed_at": pushed,
            "default_branch": "main"
        })
    }

    /// One commit with a NULL top-level author (email not linked to an account)
    /// — must map with `author_login: None`, never panic.
    fn commit_null_author() -> Value {
        serde_json::json!({
            "sha": "abc123",
            "node_id": "C_abc",
            "commit": {
                "message": "Fix the thing",
                "author": { "name": "Jane Dev", "email": "jane@example.com", "date": "2026-05-20T10:00:00Z" },
                "committer": { "name": "GitHub", "email": "noreply@github.com", "date": "2026-05-20T10:01:00Z" }
            },
            "author": null,
            "html_url": "https://github.com/octocat/repo/commit/abc123",
            "comments_url": "https://api.github.com/repos/octocat/repo/commits/abc123/comments"
        })
    }

    fn commit_with_author(sha: &str, date: &str) -> Value {
        serde_json::json!({
            "sha": sha,
            "node_id": format!("C_{sha}"),
            "commit": {
                "message": "Another commit",
                "author": { "name": "Jane Dev", "email": "jane@example.com", "date": date },
                "committer": { "date": date }
            },
            "author": { "login": "octocat", "id": 1 },
            "html_url": format!("https://github.com/octocat/repo/commit/{sha}")
        })
    }

    /// A search issue item with a NULL assignee (still must map).
    fn issue_json(number: i64, node: &str, created: &str, updated: &str, state: &str) -> Value {
        serde_json::json!({
            "number": number,
            "node_id": node,
            "title": format!("Issue {number}"),
            "state": state,
            "created_at": created,
            "updated_at": updated,
            "closed_at": null,
            "html_url": format!("https://github.com/octocat/repo/issues/{number}"),
            "repository_url": "https://api.github.com/repos/octocat/repo",
            "labels": [ { "name": "bug" }, { "name": "p1" } ],
            "assignee": null,
            "assignees": [],
            "body": "the body"
        })
    }

    /// An assigned issue (assignee = the viewer), for the tasks path.
    fn assigned_issue(number: i64, node: &str, login: &str) -> Value {
        serde_json::json!({
            "number": number,
            "node_id": node,
            "title": format!("Assigned {number}"),
            "state": "open",
            "created_at": "2026-06-01T08:00:00Z",
            "updated_at": "2026-06-05T09:00:00Z",
            "closed_at": null,
            "html_url": format!("https://github.com/octocat/repo/issues/{number}"),
            "repository_url": "https://api.github.com/repos/octocat/repo",
            "labels": [ { "name": "task" } ],
            "assignee": { "login": login },
            "assignees": [ { "login": login } ],
            "body": "do the work"
        })
    }

    fn pr_json(number: i64, node: &str) -> Value {
        let mut v = issue_json(number, node, "2026-04-10T00:00:00Z", "2026-04-11T00:00:00Z", "open");
        v["pull_request"] = serde_json::json!({ "url": "https://api.github.com/repos/octocat/repo/pulls/7" });
        v
    }

    fn star_json(node: &str, full: &str, starred: &str) -> Value {
        serde_json::json!({
            "starred_at": starred,
            "repo": {
                "id": 42,
                "node_id": node,
                "full_name": full,
                "html_url": format!("https://github.com/{full}")
            }
        })
    }

    fn gist_json(node: &str) -> Value {
        serde_json::json!({
            "id": "g1",
            "node_id": node,
            "html_url": "https://gist.github.com/g1",
            "description": "a gist",
            "public": true,
            "created_at": "2026-03-01T00:00:00Z",
            "updated_at": "2026-03-02T00:00:00Z",
            "files": { "a.txt": { "filename": "a.txt" }, "b.rs": { "filename": "b.rs" } }
        })
    }

    // --- a scripted mock API --------------------------------------------

    /// Maps a request path (or path *prefix*) to a queue of `Page`s, so a
    /// multi-page endpoint can be drained. Absolute next-URLs encode the page
    /// in a query the mock recognizes. Single-object GETs come from `singles`.
    struct MockApi {
        login: String,
        // path-prefix -> queued pages (front = page 1)
        pages: RefCell<Vec<(String, VecDeque<Page>)>>,
        // exact path -> single object
        singles: RefCell<BTreeMap<String, Value>>,
        // a path-prefix that should return RateLimited on first call
        rate_limited: RefCell<HashSet<String>>,
        // every page request, recorded as (path-or-url, Accept override).
        requests: RefCell<Vec<(String, Option<String>)>>,
    }

    impl MockApi {
        fn new(login: &str) -> Self {
            MockApi {
                login: login.into(),
                pages: RefCell::new(Vec::new()),
                singles: RefCell::new(BTreeMap::new()),
                rate_limited: RefCell::new(HashSet::new()),
                requests: RefCell::new(Vec::new()),
            }
        }

        /// The `Accept` override recorded for the first request whose path
        /// contains `needle` (or `None` if that request sent the default).
        fn accept_for(&self, needle: &str) -> Option<String> {
            self.requests
                .borrow()
                .iter()
                .find(|(p, _)| p.contains(needle))
                .and_then(|(_, a)| a.clone())
        }

        /// Register a single-page response for any path starting with `prefix`.
        fn page(&self, prefix: &str, items: Vec<Value>, next: Option<String>) {
            self.pages
                .borrow_mut()
                .push((prefix.into(), VecDeque::from(vec![Page { items, next }])));
        }

        /// Register a multi-page sequence for a prefix.
        fn pages_seq(&self, prefix: &str, seq: Vec<Page>) {
            self.pages.borrow_mut().push((prefix.into(), VecDeque::from(seq)));
        }

        fn single(&self, path: &str, v: Value) {
            self.singles.borrow_mut().insert(path.into(), v);
        }

        fn rate_limit(&self, prefix: &str) {
            self.rate_limited.borrow_mut().insert(prefix.into());
        }

        fn next_page(&self, path: &str) -> Result<Page, FetchError> {
            if self
                .rate_limited
                .borrow_mut()
                .iter()
                .any(|p| path.starts_with(p))
            {
                return Err(FetchError::RateLimited);
            }
            let mut pages = self.pages.borrow_mut();
            for (prefix, queue) in pages.iter_mut() {
                if path.starts_with(prefix.as_str()) || path.contains(prefix.as_str()) {
                    if let Some(p) = queue.pop_front() {
                        return Ok(p);
                    }
                }
            }
            // Unknown endpoint → empty page (e.g. repos with no matching commits).
            Ok(Page { items: Vec::new(), next: None })
        }
    }

    impl GithubApi for MockApi {
        fn get_page(&self, path: &str, accept: Option<&str>) -> Result<Page, FetchError> {
            self.requests
                .borrow_mut()
                .push((path.to_string(), accept.map(str::to_string)));
            self.next_page(path)
        }
        fn get_page_abs(&self, url: &str, accept: Option<&str>) -> Result<Page, FetchError> {
            self.requests
                .borrow_mut()
                .push((url.to_string(), accept.map(str::to_string)));
            self.next_page(url)
        }
        fn get_one(&self, path: &str) -> Result<Value, FetchError> {
            self.singles
                .borrow()
                .get(path)
                .cloned()
                .ok_or(FetchError::Other(format!("no single for {path}")))
        }
        fn viewer_login(&self) -> Result<String, FetchError> {
            Ok(self.login.clone())
        }
    }

    // --- pure mapping tests ---------------------------------------------

    #[test]
    fn maps_commit_with_null_author_and_keeps_unknown_keys_in_extra() {
        let row = map_commit("octocat/repo", commit_null_author()).unwrap();
        assert_eq!(row.guid, "octocat/repo:abc123");
        assert_eq!(row.repo, "octocat/repo");
        assert_eq!(row.sha, "abc123");
        assert_eq!(row.message, "Fix the thing");
        assert_eq!(row.authored_at, "2026-05-20T10:00:00Z");
        assert_eq!(row.committed_at.as_deref(), Some("2026-05-20T10:01:00Z"));
        assert_eq!(row.author_name.as_deref(), Some("Jane Dev"));
        assert_eq!(row.author_email.as_deref(), Some("jane@example.com"));
        assert_eq!(row.author_login, None, "null author tolerated");
        assert!(row.html_url.is_some());
        // Unknown key preserved in extra; mapped keys NOT duplicated there.
        assert!(row.extra.contains_key("comments_url"), "unknown key kept");
        assert!(!row.extra.contains_key("sha"));
        assert!(!row.extra.contains_key("commit"));
        assert!(!row.extra.contains_key("author"));
    }

    #[test]
    fn maps_issue_with_null_assignee_and_derives_repo() {
        let row = map_issue(issue_json(5, "I_5", "2026-05-02T12:00:00Z", "2026-05-03T12:00:00Z", "open")).unwrap();
        assert_eq!(row.guid, "I_5");
        assert_eq!(row.number, 5);
        assert_eq!(row.repo, "octocat/repo", "derived from repository_url");
        assert_eq!(row.labels, vec!["bug", "p1"]);
        assert_eq!(row.assignee, None, "null assignee tolerated");
        assert!(!row.is_pr);
        assert_eq!(row.body.as_deref(), Some("the body"));
    }

    #[test]
    fn maps_pr_flags_is_pr() {
        let row = map_issue(pr_json(7, "PR_7")).unwrap();
        assert!(row.is_pr, "pull_request present ⇒ PR");
    }

    #[test]
    fn map_star_and_gist() {
        let s = map_star(star_json("R_1", "rust-lang/rust", "2026-01-01T00:00:00Z")).unwrap();
        assert_eq!(s.guid, "R_1");
        assert_eq!(s.repo_full_name, "rust-lang/rust");
        assert_eq!(s.repo_id, Some(42));
        let g = map_gist(gist_json("G_1")).unwrap();
        assert_eq!(g.guid, "G_1");
        let mut files = g.files.clone();
        files.sort();
        assert_eq!(files, vec!["a.txt", "b.rs"]);
        assert!(g.public);
    }

    #[test]
    fn link_header_next_is_followed_not_last() {
        let h = "<https://api.github.com/x?page=2>; rel=\"next\", <https://api.github.com/x?page=9>; rel=\"last\"";
        assert_eq!(parse_link_next(Some(h)).as_deref(), Some("https://api.github.com/x?page=2"));
        // Last page: a prev/first set with no next.
        let h2 = "<https://api.github.com/x?page=8>; rel=\"prev\", <https://api.github.com/x?page=1>; rel=\"first\"";
        assert_eq!(parse_link_next(Some(h2)), None);
        assert_eq!(parse_link_next(None), None);
    }

    // --- persistence / upsert tests -------------------------------------

    #[test]
    fn upsert_partition_dedupes_over_overlapping_window() {
        let v = temp_vault("upsert");
        let rows1 = vec![
            map_commit("octocat/repo", commit_with_author("aaa", "2026-05-10T00:00:00Z")).unwrap(),
            map_commit("octocat/repo", commit_with_author("bbb", "2026-05-11T00:00:00Z")).unwrap(),
        ];
        let n1 = upsert_partitioned(&v, COMMITS_DIR, rows1, |r| &r.guid, |r| &r.authored_at, |_, _| false).unwrap();
        assert_eq!(n1, 2);

        // Re-sync overlapping: bbb again + a new ccc. Only ccc is new.
        let rows2 = vec![
            map_commit("octocat/repo", commit_with_author("bbb", "2026-05-11T00:00:00Z")).unwrap(),
            map_commit("octocat/repo", commit_with_author("ccc", "2026-05-12T00:00:00Z")).unwrap(),
        ];
        let n2 = upsert_partitioned(&v, COMMITS_DIR, rows2, |r| &r.guid, |r| &r.authored_at, |_, _| false).unwrap();
        assert_eq!(n2, 1, "only the new guid counted");

        // The May partition has exactly 3 distinct guids, no dup.
        let body = std::fs::read_to_string(v.root().join("developer/github/commits/2026-05.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 3, "no duplicate guids");
        let guids: HashSet<String> = body
            .lines()
            .map(|l| serde_json::from_str::<CommitRow>(l).unwrap().guid)
            .collect();
        assert_eq!(guids.len(), 3);
    }

    #[test]
    fn full_pull_writes_all_streams_and_advances_cursors() {
        let v = temp_vault("fullpull");
        let api = MockApi::new("octocat");
        // repos: one repo, pushed recently.
        api.page("/user/repos", vec![repo_json("octocat/repo", "2026-05-21T00:00:00Z")], None);
        // commits for that repo.
        api.page(
            "/repos/octocat/repo/commits",
            vec![commit_null_author(), commit_with_author("def456", "2026-06-02T00:00:00Z")],
            None,
        );
        // issues search (involves), prs search (involves), and the assigned
        // (tasks) search are all distinguished by the q content.
        api.page("involves%3Aoctocat%20is%3Aissue", vec![issue_json(1, "I_1", "2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z", "open")], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![pr_json(7, "PR_7")], None);
        api.page("assignee%3Aoctocat", vec![assigned_issue(9, "I_9", "octocat")], None);
        // stars + gists.
        api.page("/user/starred", vec![star_json("R_1", "rust-lang/rust", "2026-01-01T00:00:00Z")], None);
        api.page("/gists", vec![gist_json("G_1")], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("commits"), Some(&2));
        assert_eq!(out.counts.get("issues"), Some(&1));
        assert_eq!(out.counts.get("prs"), Some(&1));
        assert_eq!(out.counts.get("stars"), Some(&1));
        assert_eq!(out.counts.get("gists"), Some(&1));
        assert_eq!(out.counts.get("tasks"), Some(&1), "one assigned issue → one created task");

        // Raw firehose files exist and partition correctly.
        assert!(v.root().join("developer/github/commits/2026-05.jsonl").exists());
        assert!(v.root().join("developer/github/commits/2026-06.jsonl").exists());
        assert!(v.root().join("developer/github/issues/2026-05.jsonl").exists());
        assert!(v.root().join("developer/github/prs/2026-04.jsonl").exists());
        assert!(v.root().join("developer/github/stars.jsonl").exists());
        assert!(v.root().join("developer/github/gists.jsonl").exists());

        // Cursors advanced to the maxima.
        let state = v.read_github_sync();
        assert_eq!(state.commits_since.as_deref(), Some("2026-06-02T00:00:00Z"));
        assert_eq!(state.issues_updated.as_deref(), Some("2026-05-02T00:00:00Z"));
        assert_eq!(state.prs_updated.as_deref(), Some("2026-04-11T00:00:00Z"));
        assert!(state.updated.is_some());

        // Re-pull with identical data: no new raw rows (upsert), tasks no-op.
        let again = pull_with(&v, &api_replay(&v)).unwrap();
        assert_eq!(again.counts.get("commits").copied().unwrap_or(0), 0);
    }

    /// Rebuild a mock that replays the same data (for the idempotency re-run).
    fn api_replay(_v: &Vault) -> MockApi {
        let api = MockApi::new("octocat");
        api.page("/user/repos", vec![repo_json("octocat/repo", "2026-05-21T00:00:00Z")], None);
        api.page(
            "/repos/octocat/repo/commits",
            vec![commit_null_author(), commit_with_author("def456", "2026-06-02T00:00:00Z")],
            None,
        );
        api.page("involves%3Aoctocat%20is%3Aissue", vec![issue_json(1, "I_1", "2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z", "open")], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![pr_json(7, "PR_7")], None);
        api.page("assignee%3Aoctocat", vec![assigned_issue(9, "I_9", "octocat")], None);
        api.page("/user/starred", vec![star_json("R_1", "rust-lang/rust", "2026-01-01T00:00:00Z")], None);
        api.page("/gists", vec![gist_json("G_1")], None);
        api
    }

    #[test]
    fn multi_page_link_drain_strands_nothing() {
        let v = temp_vault("paging");
        let api = MockApi::new("octocat");
        api.page("/user/repos", vec![repo_json("octocat/repo", "2026-05-21T00:00:00Z")], None);
        // Two pages of commits, joined by a next URL.
        api.pages_seq(
            "/repos/octocat/repo/commits",
            vec![
                Page {
                    items: vec![commit_with_author("p1a", "2026-05-01T00:00:00Z")],
                    next: Some("https://api.github.com/repos/octocat/repo/commits?page=2".into()),
                },
                Page {
                    items: vec![commit_with_author("p2a", "2026-05-02T00:00:00Z")],
                    next: None,
                },
            ],
        );
        // No issues/prs/stars/gists/tasks for this test.
        api.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api.page("assignee%3Aoctocat", vec![], None);
        api.page("/user/starred", vec![], None);
        api.page("/gists", vec![], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("commits"), Some(&2), "both pages drained, nothing stranded");
        let body = std::fs::read_to_string(v.root().join("developer/github/commits/2026-05.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 2);
    }

    #[test]
    fn rate_limited_endpoint_leaves_cursor_unadvanced() {
        let v = temp_vault("ratelimit");
        let api = MockApi::new("octocat");
        // repos page fine, but commits endpoint is rate-limited.
        api.page("/user/repos", vec![repo_json("octocat/repo", "2026-05-21T00:00:00Z")], None);
        api.rate_limit("/repos/octocat/repo/commits");
        api.page("involves%3Aoctocat%20is%3Aissue", vec![issue_json(1, "I_1", "2026-05-01T00:00:00Z", "2026-05-02T00:00:00Z", "open")], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api.page("assignee%3Aoctocat", vec![], None);
        api.page("/user/starred", vec![], None);
        api.page("/gists", vec![], None);

        let out = pull_with(&v, &api).unwrap();
        // Commits rate-limited → not counted, cursor stays None; issues still ran.
        assert_eq!(out.counts.get("commits").copied().unwrap_or(0), 0);
        assert_eq!(out.counts.get("issues"), Some(&1));
        let state = v.read_github_sync();
        assert_eq!(state.commits_since, None, "rate-limited commits cursor unadvanced");
        assert_eq!(state.issues_updated.as_deref(), Some("2026-05-02T00:00:00Z"));
    }

    // --- THE dual-write + fate tests ------------------------------------

    #[test]
    fn assigned_issue_lands_in_both_firehose_and_task_store() {
        let v = temp_vault("dualwrite");
        let api = MockApi::new("octocat");
        api.page("/user/repos", vec![], None); // no repos → no commits
        // The SAME assigned issue node "I_DUAL" appears in the involves-issue
        // firehose search AND the assignee tasks search.
        let dual = assigned_issue(42, "I_DUAL", "octocat");
        api.page("involves%3Aoctocat%20is%3Aissue", vec![dual.clone()], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api.page("assignee%3Aoctocat", vec![dual.clone()], None);
        api.page("/user/starred", vec![], None);
        api.page("/gists", vec![], None);

        pull_with(&v, &api).unwrap();

        // 1) firehose: developer/github/issues/2026-06.jsonl carries node I_DUAL.
        let fire = std::fs::read_to_string(v.root().join("developer/github/issues/2026-06.jsonl")).unwrap();
        assert!(fire.contains("I_DUAL"), "assigned issue in the issues firehose: {fire}");

        // 2) tasks: tasks/github/tasks.jsonl carries the same issue as a Task.
        let tasks = std::fs::read_to_string(v.root().join("tasks/github/tasks.jsonl")).unwrap();
        assert!(tasks.contains("I_DUAL"), "assigned issue in the task store: {tasks}");
        assert!(tasks.contains("\"source\":\"github\""));
        assert!(tasks.contains("\"project\":\"octocat/repo\""));
        // Times converted to local in the task layer (a +/-offset or Z form).
        let t = v.load_tasks_snapshot("github").unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].id, "I_DUAL");
        assert_eq!(t[0].tags, vec!["task"]);
        assert_eq!(t[0].extra.get("number").and_then(Value::as_i64), Some(42));
        assert_eq!(t[0].status, "open");
    }

    #[test]
    fn assigned_issue_that_closes_logs_a_completed_event_with_closed_time() {
        let v = temp_vault("fate");
        // First sync: one assigned open issue.
        let api1 = MockApi::new("octocat");
        api1.page("/user/repos", vec![], None);
        api1.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api1.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api1.page("assignee%3Aoctocat", vec![assigned_issue(50, "I_50", "octocat")], None);
        api1.page("/user/starred", vec![], None);
        api1.page("/gists", vec![], None);
        pull_with(&v, &api1).unwrap();
        assert_eq!(v.load_tasks_snapshot("github").unwrap().len(), 1);

        // Second sync: the issue vanished from the assigned-open set. The fate
        // lookup returns it CLOSED with a closed_at → a completed event.
        let api2 = MockApi::new("octocat");
        api2.page("/user/repos", vec![], None);
        api2.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api2.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api2.page("assignee%3Aoctocat", vec![], None); // no longer assigned-open
        api2.page("/user/starred", vec![], None);
        api2.page("/gists", vec![], None);
        api2.single(
            "/repos/octocat/repo/issues/50",
            serde_json::json!({ "state": "closed", "closed_at": "2026-06-09T15:30:00Z" }),
        );

        pull_with(&v, &api2).unwrap();

        // Snapshot now empty; a completed event landed with the closed time.
        assert!(v.load_tasks_snapshot("github").unwrap().is_empty());
        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completed: Vec<_> = events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completed.len(), 1, "the closed issue logged a completion");
        assert_eq!(completed[0].task.id, "I_50");
        // closed_at converted to local; same instant as the UTC closed time.
        assert_eq!(
            DateTime::parse_from_rfc3339(&completed[0].time).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-09T15:30:00Z").unwrap().timestamp(),
        );
        assert!(v.root().join("tasks/github/events/2026-06.jsonl").exists());
    }

    #[test]
    fn assigned_issue_still_open_but_unassigned_is_deleted_not_completed() {
        let v = temp_vault("unassign");
        let api1 = MockApi::new("octocat");
        api1.page("/user/repos", vec![], None);
        api1.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api1.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api1.page("assignee%3Aoctocat", vec![assigned_issue(60, "I_60", "octocat")], None);
        api1.page("/user/starred", vec![], None);
        api1.page("/gists", vec![], None);
        pull_with(&v, &api1).unwrap();

        let api2 = MockApi::new("octocat");
        api2.page("/user/repos", vec![], None);
        api2.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api2.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api2.page("assignee%3Aoctocat", vec![], None);
        api2.page("/user/starred", vec![], None);
        api2.page("/gists", vec![], None);
        // Still OPEN (just unassigned) → Deleted, not Completed.
        api2.single(
            "/repos/octocat/repo/issues/60",
            serde_json::json!({ "state": "open", "closed_at": null }),
        );
        pull_with(&v, &api2).unwrap();

        // Deletions are stamped at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "I_60"));
        assert!(!events.iter().any(|e| e.kind == "completed" && e.task.id == "I_60"));
    }

    // --- connection tests -----------------------------------------------

    #[test]
    fn connection_exposes_token_paste_and_forget() {
        assert!(CONNECTION.method("token-paste").is_some());
        let v = temp_vault("conn");
        // Store a token directly (def_connect needs the network for /user).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "github_pat_x".into(),
                refresh_token: None,
                token_type: Some("Bearer".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        let mut s = v.read_github_sync();
        s.login = Some("octocat".into());
        v.write_github_sync(&s).unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "octocat");
        assert_eq!(status.accounts[0].key, "github");

        def_disconnect(&v, "github").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
        // The PAT is gone from the secret store.
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
        // Empty object (brand-new cursor).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.login.is_none());
        assert!(empty.commits_since.is_none());
        // A partial cursor that only has a login still deserializes.
        let partial: SyncState = serde_json::from_str(r#"{"login":"octocat"}"#).unwrap();
        assert_eq!(partial.login.as_deref(), Some("octocat"));
        assert!(partial.issues_updated.is_none());
    }

    #[test]
    fn urlencode_handles_search_specials() {
        assert_eq!(urlencode("involves:octocat is:issue"), "involves%3Aoctocat%20is%3Aissue");
        assert_eq!(urlencode("updated:>=2026-01-01T00:00:00Z"), "updated%3A%3E%3D2026-01-01T00%3A00%3A00Z");
    }

    // --- FIX 1: stars Accept media type + degraded map_star --------------

    /// A BARE `/user/starred` repo object — what GitHub returns when the
    /// request DOESN'T carry `Accept: application/vnd.github.star+json` (no
    /// `repo` wrapper, no `starred_at`).
    fn star_bare(node: &str, full: &str) -> Value {
        serde_json::json!({
            "id": 99,
            "node_id": node,
            "full_name": full,
            "html_url": format!("https://github.com/{full}")
        })
    }

    #[test]
    fn starred_request_sends_star_media_type() {
        let v = temp_vault("star-accept");
        let api = MockApi::new("octocat");
        api.page("/user/repos", vec![], None);
        api.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api.page("assignee%3Aoctocat", vec![], None);
        api.page("/user/starred", vec![star_json("R_1", "rust-lang/rust", "2026-01-01T00:00:00Z")], None);
        api.page("/gists", vec![], None);

        pull_with(&v, &api).unwrap();

        // The starred call MUST carry the star media type; a regression here
        // would make the real API drop every star (bare array, no `repo`).
        assert_eq!(
            api.accept_for("/user/starred").as_deref(),
            Some("application/vnd.github.star+json"),
            "the /user/starred request must request the star media type"
        );
        // Sanity: an ordinary list call (repos) keeps the default Accept.
        assert_eq!(api.accept_for("/user/repos"), None, "non-star calls use default Accept");
    }

    #[test]
    fn map_star_captures_wrapped_and_degrades_on_bare() {
        // WRAPPED (star+json): starred_at + repo fields captured.
        let wrapped = map_star(star_json("R_1", "octocat/hello", "2026-02-03T04:05:06Z")).unwrap();
        assert_eq!(wrapped.guid, "R_1");
        assert_eq!(wrapped.starred_at.as_deref(), Some("2026-02-03T04:05:06Z"));
        assert_eq!(wrapped.repo_full_name, "octocat/hello");
        assert_eq!(wrapped.repo_node_id.as_deref(), Some("R_1"));
        assert_eq!(wrapped.repo_html_url.as_deref(), Some("https://github.com/octocat/hello"));

        // BARE (header regression): still yields a row (degraded — no
        // starred_at) rather than dropping the whole capability.
        let bare = map_star(star_bare("R_2", "rust-lang/rust"))
            .expect("bare star object must still map to a row");
        assert_eq!(bare.guid, "R_2");
        assert_eq!(bare.repo_full_name, "rust-lang/rust");
        assert_eq!(bare.starred_at, None, "no starred_at on the bare shape (degraded)");
    }

    // --- FIX 2: commits cursor tracks committer date --------------------

    /// A commit whose COMMITTER date is later than its AUTHOR date (a rebase
    /// or amend). `since=` filters on committer date, so the cursor must follow
    /// it — even though the row partitions by author date.
    fn commit_committer_after_author(sha: &str, authored: &str, committed: &str) -> Value {
        serde_json::json!({
            "sha": sha,
            "node_id": format!("C_{sha}"),
            "commit": {
                "message": "rebased commit",
                "author": { "name": "Jane Dev", "email": "jane@example.com", "date": authored },
                "committer": { "name": "Jane Dev", "email": "jane@example.com", "date": committed }
            },
            "author": { "login": "octocat", "id": 1 },
            "html_url": format!("https://github.com/octocat/repo/commit/{sha}")
        })
    }

    #[test]
    fn commits_cursor_advances_to_committer_date_not_author_date() {
        let v = temp_vault("commit-cursor");
        let api = MockApi::new("octocat");
        api.page("/user/repos", vec![repo_json("octocat/repo", "2026-05-25T00:00:00Z")], None);
        // author date 2026-05-10, committer date 2026-05-20 (a rebase).
        api.page(
            "/repos/octocat/repo/commits",
            vec![commit_committer_after_author("rb1", "2026-05-10T00:00:00Z", "2026-05-20T00:00:00Z")],
            None,
        );
        api.page("involves%3Aoctocat%20is%3Aissue", vec![], None);
        api.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api.page("assignee%3Aoctocat", vec![], None);
        api.page("/user/starred", vec![], None);
        api.page("/gists", vec![], None);

        pull_with(&v, &api).unwrap();

        let state = v.read_github_sync();
        assert_eq!(
            state.commits_since.as_deref(),
            Some("2026-05-20T00:00:00Z"),
            "cursor must follow committer date (what since= filters on), not author date"
        );
        // And the row still partitions by AUTHOR date (the 05 month).
        assert!(
            v.root().join("developer/github/commits/2026-05.jsonl").exists(),
            "partitioned by author month"
        );
    }

    // --- FIX 3: search asc sort + max-processed cursor, no tail skip -----

    #[test]
    fn search_uses_asc_sort_and_advances_to_max_processed_then_continues() {
        let v = temp_vault("search-cap");
        let api = MockApi::new("octocat");
        api.page("/user/repos", vec![], None);
        // A TRUNCATED issue result: two asc-ordered pages, then the API caps
        // (no further `next`) even though more matches exist above the max
        // returned. With asc order the un-returned tail is the NEWEST — safe.
        api.pages_seq(
            "involves%3Aoctocat%20is%3Aissue",
            vec![
                Page {
                    items: vec![
                        issue_json(1, "I_1", "2026-01-01T00:00:00Z", "2026-05-01T00:00:00Z", "open"),
                        issue_json(2, "I_2", "2026-01-02T00:00:00Z", "2026-05-02T00:00:00Z", "open"),
                    ],
                    next: Some("https://api.github.com/search/issues?q=involves%3Aoctocat%20is%3Aissue&page=2".into()),
                },
                Page {
                    items: vec![
                        issue_json(3, "I_3", "2026-01-03T00:00:00Z", "2026-05-03T00:00:00Z", "open"),
                    ],
                    next: None, // capped here; newer rows truncated away
                },
            ],
        );
        api.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api.page("assignee%3Aoctocat", vec![], None);
        api.page("/user/starred", vec![], None);
        api.page("/gists", vec![], None);

        pull_with(&v, &api).unwrap();

        // The issue search request carries asc ordering (what makes the cap
        // safe) — assert on the recorded request path.
        assert!(
            api.requests
                .borrow()
                .iter()
                .any(|(p, _)| p.contains("involves%3Aoctocat%20is%3Aissue")
                    && p.contains("sort=updated")
                    && p.contains("order=asc")),
            "issue search must sort updated asc so truncation drops the newest tail"
        );
        // Cursor lands on the MAX processed updated_at (the prefix top).
        let state = v.read_github_sync();
        assert_eq!(
            state.issues_updated.as_deref(),
            Some("2026-05-03T00:00:00Z"),
            "cursor = max updated_at actually processed"
        );

        // A follow-up sync continues from the cursor: the next query carries
        // `updated:>=<cursor>` and picks up the previously-truncated tail
        // (here a newer issue) without skipping anything.
        let api2 = MockApi::new("octocat");
        api2.page("/user/repos", vec![], None);
        api2.page(
            "involves%3Aoctocat%20is%3Aissue",
            vec![issue_json(9, "I_9", "2026-01-09T00:00:00Z", "2026-05-09T00:00:00Z", "open")],
            None,
        );
        api2.page("involves%3Aoctocat%20is%3Apr", vec![], None);
        api2.page("assignee%3Aoctocat", vec![], None);
        api2.page("/user/starred", vec![], None);
        api2.page("/gists", vec![], None);

        pull_with(&v, &api2).unwrap();

        // The second issue query carried the cursor lower bound.
        let cursor_enc = urlencode("updated:>=2026-05-03T00:00:00Z");
        assert!(
            api2.requests
                .borrow()
                .iter()
                .any(|(p, _)| p.contains("involves%3Aoctocat%20is%3Aissue") && p.contains(&cursor_enc)),
            "follow-up search continues from the cursor (updated:>=<cursor>)"
        );
        // And the cursor advanced again — forward progress, nothing skipped.
        let state2 = v.read_github_sync();
        assert_eq!(state2.issues_updated.as_deref(), Some("2026-05-09T00:00:00Z"));
    }
}
