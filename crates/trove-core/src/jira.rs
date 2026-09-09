//! Jira Cloud — Atlassian issue tracker via the REST v3 API.
//!
//! Pulls issues **assigned to or reported by** the authenticated user into the
//! bound [`crate::tasks`] contract. Two destinations are written in one pass:
//!
//! - **tasks contract** under `tasks/jira/` — snapshot + event stream via
//!   [`crate::tasks::apply_tasks_sync`], exactly like the Todoist/Asana/Linear
//!   legs. `guid` = the stable Jira issue key (e.g. `"MYPROJ-42"`).
//! - **raw firehose** under `tasks/jira/raw/YYYY-MM.jsonl` — the API issue
//!   objects at full fidelity (description, labels, comments, all `fields`),
//!   partitioned by `fields.created` month, upserted by issue `key`.
//!
//! # Auth
//!
//! HTTP Basic auth = `<email>:<api-token>`, base64-encoded per Atlassian docs.
//! The connection accepts a three-line paste (domain / email / token); the
//! credential is stored in `.trove/sync/jira` (0600) as:
//!   - `access_token` = API token (the secret)
//!   - `token_type`   = site domain (e.g. `mycompany.atlassian.net`)
//!   - `scope`        = Atlassian account email
//!
//! API token from: https://id.atlassian.com/manage-profile/security/api-tokens
//!
//! # API
//!
//! Jira Cloud REST v3: `GET https://{domain}/rest/api/3/search/jql?jql=…`.
//! The legacy `/rest/api/3/search` endpoint was deprecated 2024-10-31 and
//! fully removed as of 2025-10-31; this module targets the replacement.
//! Response envelope: `{ "issues": [...], "isLast": bool, "nextPageToken": str|null }`.
//! Each issue has: `id` (numeric string), `key` (e.g. "PROJ-42"), `fields` (object).
//! Key fields (confirmed from Atlassian REST v3 OpenAPI spec):
//!   `fields.summary`, `fields.status.name`, `fields.status.statusCategory.key`,
//!   `fields.assignee.displayName/emailAddress`, `fields.reporter.displayName/emailAddress`,
//!   `fields.priority.name`, `fields.issuetype.name`,
//!   `fields.project.key/name`, `fields.created`, `fields.updated`,
//!   `fields.duedate` (YYYY-MM-DD or null), `fields.labels[]`,
//!   `fields.description` (ADF or null), `fields.comment.comments[]`.
//! Pagination: token-based via `nextPageToken` query param; drain until the
//! response has `isLast: true` or `nextPageToken` is null/absent.
//!
//! # JQL queries
//!
//! Two JQL queries per pull, merged by key (deduplicated):
//! 1. `assignee = currentUser() ORDER BY updated ASC` (or with `updated > "{since}"`)
//! 2. `reporter = currentUser() ORDER BY updated ASC` (or with `updated > "{since}"`)
//! On the first sync (no cursor) the `updated >` clause is omitted so all
//! issues are fetched. The watermark advances to the max `fields.updated` seen.
//!
//! # Fate / completion
//!
//! An issue that disappears from the open set (status category changed from
//! `todo`/`in-progress` to `done`) is discovered via the watermark window:
//! `assignee = currentUser() AND statusCategory = Done AND updated > "{since}"`.
//! A vanished key in that set → Completed; absent from it (but gone from open)
//! → Deleted; the completed lookup failed → Unknown carry-forward.
//!
//! # Cursor
//!
//! `.trove/jira-sync.json` (non-secret, rebuildable) holds the last successful
//! watermark as an ISO-8601 string.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/jira.md.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
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

/// Source id: folder name under `tasks/`, every row's `source` field.
const SOURCE: &str = "jira";

/// Raw firehose (full-fidelity API issue objects).
const RAW_DIR: &str = "tasks/jira/raw";

/// Non-secret rebuildable watermark file. Not under `.trove/sync/` (0600).
const SYNC_FILE: &str = ".trove/jira-sync.json";

/// Secret store key (0600) for the composite credential.
const SERVICE: &str = "jira";

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Every 15 min, matching the other task sources.
pub const JIRA_SYNC_SECS: u64 = 900;

/// Max page size the Jira Cloud search API accepts.
const PAGE_SIZE: u64 = 100;

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
                    "jira synced — {} open, {} completed, {} deleted",
                    c("open"),
                    c("completed"),
                    c("deleted"),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "jira sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Jira synced — {} open issues, {} completed, {} deleted",
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
        id: "jira",
        name: "Jira",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls issues assigned to or reported by you from Jira Cloud every \
                      15 minutes using the REST v3 API. Requires your site domain, \
                      Atlassian email, and a personal API token.",
        domain: "tasks",
        vault_path: "tasks/jira/",
        toggleable: true,
        setup: &[
            "Connect with your Jira site domain, email, and API token on this card.",
            "Syncs all open issues assigned to or reported by you, plus completions.",
        ],
        caveats: "Targets Jira Cloud. Jira Server and Data Center use different endpoints \
                  and are out of scope for v1.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(JIRA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("jira"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Credential helpers.

/// The three components extracted from the pasted credential block.
struct Credential {
    domain: String,
    email: String,
    api_token: String,
}

/// Parse a pasted Jira credential of the form:
/// ```text
/// mycompany.atlassian.net
/// me@example.com
/// api-token-here
/// ```
/// Lines are trimmed; blank lines are skipped. Optionally strips `https://` and
/// trailing `/` from the domain line. Returns `None` when fewer than 3
/// non-empty lines are present.
fn parse_pasted_credential(pasted: &str) -> Option<Credential> {
    let parts: Vec<&str> =
        pasted.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    let domain = parts[0]
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    Some(Credential {
        domain,
        email: parts[1].to_string(),
        api_token: parts[2].to_string(),
    })
}

/// Load the stored credential from the secret store. Returns `None` when not
/// connected. The layout mirrors the CalDAV composite: `access_token` = API
/// token, `token_type` = domain, `scope` = email.
fn load_credential(vault: &Vault) -> Result<Option<Credential>> {
    let Some(ts) = vault.load_sync_token(SERVICE)? else {
        return Ok(None);
    };
    let api_token = ts.access_token;
    let domain = ts.token_type.unwrap_or_default();
    let email = ts.scope.unwrap_or_default();
    if api_token.is_empty() || domain.is_empty() || email.is_empty() {
        return Ok(None);
    }
    Ok(Some(Credential { domain, email, api_token }))
}

// ---------------------------------------------------------------------------
// Connection.

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let Some(cred) = parse_pasted_credential(pasted) else {
        bail!(
            "Paste three lines: your Jira site domain, then your Atlassian email, \
             then your API token.\n\
             Example:\n  mycompany.atlassian.net\n  me@example.com\n  \
             ATATxxxxxxxxxxxxxxxx"
        );
    };
    if cred.domain.is_empty() || cred.email.is_empty() || cred.api_token.is_empty() {
        bail!("All three lines (domain, email, API token) are required.");
    }
    // Validate with a lightweight call to /rest/api/3/myself.
    let client =
        JiraClient::new(cred.domain.clone(), cred.email.clone(), cred.api_token.clone());
    match client.get("/rest/api/3/myself") {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Jira rejected the credentials (401) — check your email address, API token, \
             and site domain. Generate a token at: \
             https://id.atlassian.com/manage-profile/security/api-tokens"
        ),
        Err(e) => bail!("Jira /myself check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: cred.api_token,
            refresh_token: None,
            token_type: Some(cred.domain),
            scope: Some(cred.email),
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(ts) = vault.load_sync_token(SERVICE)? {
        let domain = ts.token_type.as_deref().unwrap_or("unknown domain");
        let email = ts.scope.as_deref().unwrap_or("unknown email");
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: format!("{email} @ {domain}"),
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
    id: "jira",
    display_name: "Jira",
    methods: &[ConnectMethod::TokenPaste {
        label: "Jira site domain, email, and API token",
        help: "Paste three lines: your site domain (e.g. mycompany.atlassian.net), \
               your Atlassian email, and your personal API token from \
               id.atlassian.com/manage-profile/security/api-tokens.",
        placeholder: "mycompany.atlassian.net\nme@example.com\nATATxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["jira"],
    setup: &[
        "Go to id.atlassian.com → Security → API tokens and create a new token.",
        "Copy your Jira site domain (e.g. mycompany.atlassian.net).",
        "Paste all three values here — they are stored locally, never leave your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page of search results from `/rest/api/3/search/jql`.
struct Page {
    issues: Vec<Value>,
    /// Continuation token for the next page; absent when this is the last page.
    next_page_token: Option<String>,
    /// True when the API indicates there are no further pages.
    is_last: bool,
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

/// The endpoints the pull needs. A trait so tests drive the logic offline.
trait JiraApi {
    /// `GET https://{domain}{path}`. Returns a parsed JSON object.
    fn get(&self, path: &str) -> Result<Value, FetchError>;

    /// `GET /rest/api/3/search/jql?jql=…&nextPageToken=…&maxResults=…`
    fn search(&self, jql: &str, next_page_token: Option<&str>, max_results: u64) -> Result<Page, FetchError>;
}

/// Thin real client; base URL = `https://{domain}`.
struct JiraClient {
    domain: String,
    email: String,
    api_token: String,
}

impl JiraClient {
    fn new(domain: String, email: String, api_token: String) -> Self {
        JiraClient { domain, email, api_token }
    }

    /// HTTP Basic auth header value (email:token, base64-encoded).
    fn auth_header(&self) -> String {
        let raw = format!("{}:{}", self.email, self.api_token);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
        )
    }

    fn base_url(&self) -> String {
        format!("https://{}", self.domain)
    }

    fn handle_resp(
        resp: std::result::Result<ureq::Response, ureq::Error>,
    ) -> Result<Value, FetchError> {
        match resp {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parse error: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Unauthorized),
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

impl JiraApi for JiraClient {
    fn get(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{path}", self.base_url());
        Self::handle_resp(
            ureq::get(&url)
                .timeout(HTTP_TIMEOUT)
                .set("Authorization", &self.auth_header())
                .set("Accept", "application/json")
                .call(),
        )
    }

    fn search(&self, jql: &str, next_page_token: Option<&str>, max_results: u64) -> Result<Page, FetchError> {
        let url = format!("{}/rest/api/3/search/jql", self.base_url());
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &self.auth_header())
            .set("Accept", "application/json")
            .query("jql", jql)
            .query("maxResults", &max_results.to_string())
            .query("fields", ISSUE_FIELDS);
        if let Some(token) = next_page_token {
            req = req.query("nextPageToken", token);
        }
        let v = Self::handle_resp(req.call())?;
        Ok(parse_search_page(v))
    }
}

/// Fields requested on every search call. Full fidelity for the raw layer;
/// the contract mapping picks only what it needs.
const ISSUE_FIELDS: &str =
    "id,key,summary,status,assignee,reporter,priority,issuetype,project,\
     created,updated,duedate,labels,description,comment";

/// Parse a `/rest/api/3/search/jql` response envelope into a [`Page`].
///
/// The real shape is `{ "issues": [...], "isLast": bool, "nextPageToken": str|null }`.
/// Neither `total` nor `startAt` is present on this endpoint; pagination is
/// driven solely by `isLast` and `nextPageToken`.
fn parse_search_page(v: Value) -> Page {
    let issues = v
        .get("issues")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // `isLast` is true when there are no further pages. Treat absent as true
    // (safe default: stop rather than spin on a malformed response).
    let is_last = v.get("isLast").and_then(Value::as_bool).unwrap_or(true);
    // `nextPageToken` is a string when more pages exist; null or absent on the last page.
    let next_page_token = v
        .get("nextPageToken")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Page { issues, next_page_token, is_last }
}

/// Drain all pages of a JQL query (token pagination via `nextPageToken`), merging
/// results into a `HashMap<key, Value>` deduplicated by issue key.
///
/// Exits when `isLast == true` or the response carries no `nextPageToken`.
/// Never reads `total` — the new endpoint does not return it.
fn drain_jql(
    api: &impl JiraApi,
    jql: &str,
    out: &mut HashMap<String, Value>,
) -> Result<(), FetchError> {
    let mut next_token: Option<String> = None;
    loop {
        let page = api.search(jql, next_token.as_deref(), PAGE_SIZE)?;
        let is_last = page.is_last;
        let next = page.next_page_token.clone();
        for issue in page.issues {
            if let Some(key) = issue.get("key").and_then(Value::as_str) {
                out.entry(key.to_string()).or_insert(issue);
            }
        }
        if is_last || next.is_none() {
            break;
        }
        next_token = next;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max `fields.updated` seen (ISO8601). The JQL `updated >` lower bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_updated: Option<String>,
}

impl Vault {
    fn read_jira_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_jira_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row.

/// One raw API issue object in `tasks/jira/raw/YYYY-MM.jsonl`.
/// The on-disk line is the verbatim API object (no synthetic keys added).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawIssue {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawIssue {
    /// The stable issue key (dedup key + guid), e.g. `"PROJ-42"`.
    fn key(&self) -> String {
        self.fields
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    }

    /// `fields.created` (ISO8601) — the partition key.
    fn created(&self) -> &str {
        self.fields
            .get("fields")
            .and_then(|f| f.get("created"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

fn raw_issue(value: &Value) -> Option<RawIssue> {
    let obj = value.as_object()?;
    let key = obj.get("key").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let _ = key;
    obj.get("fields")
        .and_then(|f| f.get("created"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    Some(RawIssue { fields: obj.clone() })
}

// ---------------------------------------------------------------------------
// Raw upsert-into-partition (todoist/asana/github idiom).

fn upsert_raw(vault: &Vault, rows: Vec<RawIssue>) -> Result<u64> {
    use crate::store::Partition;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<RawIssue>> = BTreeMap::new();
    for r in rows {
        let created = r.created().to_string();
        let key = Partition::Month
            .key(&created)
            .with_context(|| format!("jira: raw issue created {created:?} has no month"))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<RawIssue> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> = existing
            .iter()
            .enumerate()
            .map(|(i, r)| (r.key(), i))
            .collect();
        for r in fresh {
            match idx.get(&r.key()).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(r.key(), existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| {
            a.created().cmp(b.created()).then_with(|| a.key().cmp(&b.key()))
        });
        vault.write_snapshot(&format!("{RAW_DIR}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// Pure mapping.

/// Pull a string field from an object value.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Nested `fields` → string field accessor.
fn fields_str(issue: &Value, key: &str) -> Option<String> {
    issue.get("fields").and_then(|f| str_opt(f, key))
}

/// `fields.{parent}.name` → string.
fn fields_nested_name(issue: &Value, parent: &str) -> Option<String> {
    issue
        .get("fields")?
        .get(parent)?
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `fields.status.statusCategory.key` — "new" | "indeterminate" | "done".
fn status_category(issue: &Value) -> String {
    issue
        .get("fields")
        .and_then(|f| f.get("status"))
        .and_then(|s| s.get("statusCategory"))
        .and_then(|sc| sc.get("key"))
        .and_then(Value::as_str)
        .unwrap_or("new")
        .to_string()
}

/// Map Jira priority name → task contract scale (0 none, 1 low, 3 medium, 5 high).
fn map_priority(name: Option<&str>) -> i64 {
    match name.unwrap_or("") {
        "Lowest" | "Low" | "Minor" | "Trivial" => 1,
        "Medium" | "Normal" | "Moderate" => 3,
        "High" | "Major" | "High Priority" => 5,
        "Highest" | "Critical" | "Blocker" | "Urgent" => 5,
        _ => 0,
    }
}

/// ISO8601 timestamp → RFC3339 local. Passes through on parse failure.
///
/// Jira returns timestamps as `"2026-06-10T14:00:00.000+0000"` (no colon in
/// timezone offset, milliseconds). We normalize to RFC3339 before parsing.
fn to_local(s: &str) -> String {
    // First try strict RFC3339.
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return t.with_timezone(&Local).to_rfc3339();
    }
    // Jira format "2026-06-10T14:00:00.000+0000" → insert colon before last 2 digits.
    // Strip milliseconds, add colon to offset: "2026-06-10T14:00:00+00:00".
    let normalized = normalize_jira_ts(s);
    DateTime::parse_from_rfc3339(&normalized)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Normalize a Jira timestamp `"YYYY-MM-DDTHH:MM:SS.mmm+HHMM"` to strict
/// RFC3339 `"YYYY-MM-DDTHH:MM:SS+HH:MM"`. Returns the input unchanged when
/// it doesn't match the expected shape.
fn normalize_jira_ts(s: &str) -> String {
    // Must have at least "YYYY-MM-DDTHH:MM:SS" (19 chars).
    if s.len() < 19 {
        return s.to_string();
    }
    // Find the sign (+/-) after the time portion.
    let after_time = &s[19..];
    let sign_pos = after_time.find('+').or_else(|| after_time.rfind('-'));
    let Some(sign_idx) = sign_pos else {
        return s.to_string();
    };
    // Base: strip milliseconds if present (the `.mmm` before the sign).
    let base = &s[..19]; // "YYYY-MM-DDTHH:MM:SS"
    let offset_raw = &after_time[sign_idx..]; // "+0000" or "-0700"
    if offset_raw.len() == 5 && !offset_raw.contains(':') {
        // "+HHMM" → "+HH:MM"
        let sign = &offset_raw[..1];
        let hh = &offset_raw[1..3];
        let mm = &offset_raw[3..5];
        return format!("{base}{sign}{hh}:{mm}");
    }
    s.to_string()
}

/// JQL datetime format: `"YYYY-MM-DD HH:MM"` (Jira requires this exact form for
/// `updated > "…"` comparisons, not ISO8601).
///
/// Jira timestamps may be `"2026-06-10T14:00:00.000+0000"` (no colon in offset,
/// milliseconds included). We try RFC3339 first, then a manual prefix extraction.
fn to_jql_datetime(s: &str) -> String {
    // Try strict RFC3339 parse first (e.g. "2026-06-10T07:00:00-07:00").
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return t.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M").to_string();
    }
    // Jira often returns "2026-06-10T14:00:00.000+0000" — normalize to
    // RFC3339 by inserting the colon in the timezone offset if needed.
    if s.len() >= 19 {
        // Take the date and time prefix (first 16 chars = "YYYY-MM-DDTHH:MM")
        // and replace the 'T' separator with a space for JQL.
        let prefix: String = s.chars().take(16).collect();
        if prefix.len() == 16 && prefix.contains('T') {
            return prefix.replace('T', " ");
        }
    }
    // Last resort: return the first 16 chars as-is.
    s.chars().take(16).collect()
}

/// A raw API issue object → a normalized [`Task`]. Returns `None` when the
/// issue has no key or no summary.
fn task_from_issue(issue: &Value) -> Option<Task> {
    let key = issue.get("key").and_then(Value::as_str)?.to_string();
    let summary = fields_str(issue, "summary")?;

    let status_name =
        fields_nested_name(issue, "status").unwrap_or_else(|| "Open".into());
    let cat = status_category(issue);
    let status = if cat == "done" { "done".into() } else { "open".into() };

    let project = issue
        .get("fields")
        .and_then(|f| f.get("project"))
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();

    let project_key = issue
        .get("fields")
        .and_then(|f| f.get("project"))
        .and_then(|p| p.get("key"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();

    let priority_name = fields_nested_name(issue, "priority");
    let priority = map_priority(priority_name.as_deref());

    // duedate is a plain date string (YYYY-MM-DD) or null.
    let due = fields_str(issue, "duedate");

    let labels: Vec<String> = issue
        .get("fields")
        .and_then(|f| f.get("labels"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect()
        })
        .unwrap_or_default();

    let created = fields_str(issue, "created").map(|s| to_local(&s));
    let modified = fields_str(issue, "updated").map(|s| to_local(&s));

    // Reporter, issuetype, assignee email → extra.
    let mut extra: Map<String, Value> = Map::new();
    if let Some(reporter) = issue.get("fields").and_then(|f| f.get("reporter")) {
        if let Some(name) = reporter.get("displayName").and_then(Value::as_str) {
            extra.insert("reporter_name".into(), Value::from(name));
        }
        if let Some(email) = reporter.get("emailAddress").and_then(Value::as_str) {
            extra.insert("reporter_email".into(), Value::from(email));
        }
    }
    if let Some(issuetype) = fields_nested_name(issue, "issuetype") {
        extra.insert("issuetype".into(), Value::from(issuetype));
    }
    extra.insert("status_name".into(), Value::from(status_name));
    extra.insert("status_category".into(), Value::from(cat));
    if !project_key.is_empty() {
        extra.insert("project_key".into(), Value::from(project_key));
    }
    if let Some(p) = &priority_name {
        extra.insert("priority_name".into(), Value::from(p.clone()));
    }

    Some(Task {
        source: SOURCE.into(),
        id: key,
        title: summary,
        project,
        notes: String::new(), // description is ADF — keep in extra/raw
        status,
        priority,
        due,
        start: None,
        all_day: true, // duedate is always a plain date (no time component)
        recurrence: None,
        tags: labels,
        subtasks: Vec::new(),
        created,
        modified,
        completed: None,
        extra,
    })
}

/// Build a [`ProjectInfo`] list from the unique projects seen across issues.
fn projects_from_issues(issues: &HashMap<String, Value>) -> Vec<ProjectInfo> {
    let mut seen: HashMap<String, String> = HashMap::new();
    for issue in issues.values() {
        if let Some(proj) = issue.get("fields").and_then(|f| f.get("project")) {
            let id =
                proj.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let name =
                proj.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            if !id.is_empty() && !name.is_empty() {
                seen.insert(id, name);
            }
        }
    }
    seen.into_iter().map(|(id, name)| ProjectInfo { id, name }).collect()
}

/// Resolve the `updated` watermark from a set of issues (the max `fields.updated`).
fn max_updated(issues: &HashMap<String, Value>) -> Option<String> {
    issues
        .values()
        .filter_map(|v| {
            v.get("fields").and_then(|f| f.get("updated")).and_then(Value::as_str)
        })
        .max()
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let cred = load_credential(vault)?
        .context("Jira is not connected — add your credentials in the Integrations tab")?;
    let client = JiraClient::new(cred.domain, cred.email, cred.api_token);
    pull_with(vault, &client, Local::now())
}

fn pull_with(vault: &Vault, api: &impl JiraApi, now: DateTime<Local>) -> Result<PullOutcome> {
    let mut state = vault.read_jira_sync();

    // Build the `updated >` clause from the watermark, if any.
    let since_clause = state
        .last_updated
        .as_deref()
        .map(to_jql_datetime)
        .map(|d| format!(" AND updated > \"{d}\""));

    // --- Drain open issues (assigned + reported), merged by key -------------
    let mut open_issues: HashMap<String, Value> = HashMap::new();
    {
        let jql_assigned = format!(
            "assignee = currentUser(){} ORDER BY updated ASC",
            since_clause.as_deref().unwrap_or("")
        );
        drain_jql(api, &jql_assigned, &mut open_issues).map_err(fetch_err)?;
    }
    {
        let jql_reported = format!(
            "reporter = currentUser(){} ORDER BY updated ASC",
            since_clause.as_deref().unwrap_or("")
        );
        drain_jql(api, &jql_reported, &mut open_issues).map_err(fetch_err)?;
    }

    // Filter to only OPEN issues for the contract snapshot.
    let open_map: HashMap<String, Value> = open_issues
        .iter()
        .filter(|(_, v)| status_category(v) != "done")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // --- Raw firehose: upsert ALL issues (open + done seen in this window) --
    let raw_rows: Vec<RawIssue> = open_issues.values().filter_map(raw_issue).collect();
    let raw_new = upsert_raw(vault, raw_rows)?;

    // --- Projects list (for apply_tasks_sync) --------------------------------
    let projects = projects_from_issues(&open_issues);

    // --- Normalize open issues → Task list -----------------------------------
    let fresh: Vec<Task> = open_map.values().filter_map(task_from_issue).collect();

    // --- Completed issues for the fate closure (assignee + statusCategory=Done) --
    // Fetch only if we have a prior watermark (first sync has nothing to diff).
    let completed_map: Option<HashMap<String, String>> =
        if let Some(since) = state.last_updated.as_deref().map(to_jql_datetime) {
            let jql_done = format!(
                "assignee = currentUser() AND statusCategory = Done AND updated > \"{since}\""
            );
            let mut done_issues: HashMap<String, Value> = HashMap::new();
            match drain_jql(api, &jql_done, &mut done_issues) {
                Ok(()) => {
                    // Also pick up reporter side.
                    let jql_done_r = format!(
                        "reporter = currentUser() AND statusCategory = Done AND updated > \"{since}\""
                    );
                    let _ = drain_jql(api, &jql_done_r, &mut done_issues);
                    let map: HashMap<String, String> = done_issues
                        .iter()
                        .map(|(key, v)| {
                            let ts = v
                                .get("fields")
                                .and_then(|f| f.get("updated"))
                                .and_then(Value::as_str)
                                .map(to_local)
                                .unwrap_or_default();
                            (key.clone(), ts)
                        })
                        .collect();
                    Some(map)
                }
                Err(_) => None, // carry forward on error
            }
        } else {
            Some(HashMap::new()) // first sync: no prior tasks → nothing to diff
        };

    // --- Diff into the bound tasks contract -----------------------------------
    let stats = vault
        .apply_tasks_sync(SOURCE, &projects, fresh, |t| {
            jira_fate(t, completed_map.as_ref())
        })
        .context("jira: applying task sync")?;

    // Advance the watermark to the max updated seen in this window.
    if let Some(new_wm) = max_updated(&open_issues) {
        state.last_updated = Some(new_wm);
    }
    // Always advance the cursor (even on an empty sync) so next run uses a
    // narrow window. If open_issues was empty, the watermark stays as-is.
    vault.write_jira_sync(&state)?;

    // now param used for the CollectOutcome timestamp (satisfies the borrow).
    let _ = now;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("open", stats.open);
    counts.insert("completed", stats.completed);
    counts.insert("deleted", stats.deleted);
    counts.insert("created", stats.created);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!("{} open Jira issues", stats.open),
        counts,
    })
}

fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Jira rejected the credentials (401) — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Jira fetch failed: {other}"),
    }
}

/// Resolve the fate of an issue that disappeared from the open set.
fn jira_fate(task: &Task, completed: Option<&HashMap<String, String>>) -> TaskFate {
    match completed {
        Some(map) => match map.get(&task.id) {
            Some(when) if !when.is_empty() => TaskFate::Completed(Some(when.clone())),
            Some(_) => TaskFate::Completed(None),
            None => TaskFate::Deleted,
        },
        None => TaskFate::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-jira-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn now() -> DateTime<Local> {
        DateTime::parse_from_rfc3339("2026-06-14T12:00:00-07:00")
            .unwrap()
            .with_timezone(&Local)
    }

    // ---- Fixtures (Jira Cloud REST v3 shapes) ------------------------------

    /// An open, high-priority issue assigned to current user.
    fn issue_open(key: &str, project_key: &str, project_name: &str) -> Value {
        serde_json::json!({
            "id": "10001",
            "key": key,
            "fields": {
                "summary": "Implement OAuth flow",
                "status": {
                    "name": "In Progress",
                    "statusCategory": { "key": "indeterminate", "name": "In Progress" }
                },
                "assignee": {
                    "displayName": "Alice Dev",
                    "emailAddress": "alice@example.com",
                    "accountId": "abc123"
                },
                "reporter": {
                    "displayName": "Bob PM",
                    "emailAddress": "bob@example.com",
                    "accountId": "def456"
                },
                "priority": { "name": "High", "id": "2" },
                "issuetype": { "name": "Story", "id": "10001" },
                "project": {
                    "id": "10000",
                    "key": project_key,
                    "name": project_name
                },
                "created": "2026-06-01T08:00:00.000+0000",
                "updated": "2026-06-10T14:00:00.000+0000",
                "duedate": "2026-06-20",
                "labels": ["backend", "auth"],
                "description": null,
                "comment": { "comments": [], "total": 0 }
            }
        })
    }

    /// A done issue (statusCategory.key = "done").
    fn issue_done(key: &str, project_key: &str, project_name: &str) -> Value {
        serde_json::json!({
            "id": "10002",
            "key": key,
            "fields": {
                "summary": "Write unit tests",
                "status": {
                    "name": "Done",
                    "statusCategory": { "key": "done", "name": "Done" }
                },
                "assignee": {
                    "displayName": "Alice Dev",
                    "emailAddress": "alice@example.com",
                    "accountId": "abc123"
                },
                "reporter": {
                    "displayName": "Alice Dev",
                    "emailAddress": "alice@example.com",
                    "accountId": "abc123"
                },
                "priority": { "name": "Medium", "id": "3" },
                "issuetype": { "name": "Task", "id": "10002" },
                "project": {
                    "id": "10000",
                    "key": project_key,
                    "name": project_name
                },
                "created": "2026-06-01T09:00:00.000+0000",
                "updated": "2026-06-12T10:00:00.000+0000",
                "duedate": null,
                "labels": [],
                "description": null,
                "comment": { "comments": [], "total": 0 }
            }
        })
    }

    /// An issue with no due date and low priority.
    fn issue_no_due(key: &str) -> Value {
        serde_json::json!({
            "id": "10003",
            "key": key,
            "fields": {
                "summary": "Update README",
                "status": {
                    "name": "To Do",
                    "statusCategory": { "key": "new", "name": "To Do" }
                },
                "assignee": null,
                "reporter": {
                    "displayName": "Carol",
                    "emailAddress": "carol@example.com",
                    "accountId": "ghi789"
                },
                "priority": { "name": "Low", "id": "4" },
                "issuetype": { "name": "Task", "id": "10002" },
                "project": { "id": "10001", "key": "DOCS", "name": "Documentation" },
                "created": "2026-06-02T10:00:00.000+0000",
                "updated": "2026-06-02T10:00:00.000+0000",
                "duedate": null,
                "labels": ["docs"],
                "description": null,
                "comment": { "comments": [], "total": 0 }
            }
        })
    }

    // ---- Mock API ----------------------------------------------------------

    struct MockApi {
        /// Maps a JQL keyword to a queue of page batches (Vec<Value> per page).
        /// Each batch in the queue is one page; multiple batches simulate multi-page results.
        pages: RefCell<Vec<(String, VecDeque<Vec<Value>>)>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi { pages: RefCell::new(Vec::new()) }
        }

        /// Register a single-page result for queries containing `keyword`.
        fn register_jql(&self, keyword: &str, issues: Vec<Value>) {
            self.pages
                .borrow_mut()
                .push((keyword.to_string(), VecDeque::from(vec![issues])));
        }

        /// Register multiple pages (simulates nextPageToken pagination).
        fn register_jql_pages(&self, keyword: &str, pages: Vec<Vec<Value>>) {
            self.pages
                .borrow_mut()
                .push((keyword.to_string(), VecDeque::from(pages)));
        }
    }

    impl JiraApi for MockApi {
        fn get(&self, _path: &str) -> Result<Value, FetchError> {
            Ok(serde_json::json!({"accountId": "abc123", "displayName": "Test User"}))
        }

        /// Simulates `/rest/api/3/search/jql` token pagination.
        /// Each `register_jql` call enqueues one page. When more pages remain
        /// in the queue, `is_last` is false and `next_page_token` is set.
        fn search(&self, jql: &str, _next_page_token: Option<&str>, _max_results: u64) -> Result<Page, FetchError> {
            let mut pages = self.pages.borrow_mut();
            for (keyword, queue) in pages.iter_mut() {
                if jql.contains(keyword.as_str()) {
                    if let Some(issues) = queue.pop_front() {
                        let more = !queue.is_empty();
                        return Ok(Page {
                            issues,
                            is_last: !more,
                            next_page_token: if more {
                                Some("mock-next-token".to_string())
                            } else {
                                None
                            },
                        });
                    }
                }
            }
            Ok(Page { issues: vec![], is_last: true, next_page_token: None })
        }
    }

    // ---- Pure mapping tests ------------------------------------------------

    #[test]
    fn maps_open_issue_fields_correctly() {
        let issue = issue_open("PROJ-1", "PROJ", "My Project");
        let task = task_from_issue(&issue).unwrap();

        assert_eq!(task.source, "jira");
        assert_eq!(task.id, "PROJ-1", "key is the guid");
        assert_eq!(task.title, "Implement OAuth flow");
        assert_eq!(task.project, "My Project");
        assert_eq!(task.status, "open");
        assert_eq!(task.priority, 5, "High → 5");
        assert_eq!(task.due.as_deref(), Some("2026-06-20"));
        assert!(task.all_day, "duedate is always all-day");
        assert_eq!(task.tags, vec!["backend", "auth"]);
        assert!(task.created.is_some());
        assert!(task.modified.is_some());

        assert_eq!(
            task.extra.get("issuetype").and_then(Value::as_str),
            Some("Story")
        );
        assert_eq!(
            task.extra.get("status_name").and_then(Value::as_str),
            Some("In Progress")
        );
        assert_eq!(
            task.extra.get("status_category").and_then(Value::as_str),
            Some("indeterminate")
        );
        assert_eq!(
            task.extra.get("project_key").and_then(Value::as_str),
            Some("PROJ")
        );
        assert_eq!(
            task.extra.get("reporter_name").and_then(Value::as_str),
            Some("Bob PM")
        );
        assert_eq!(
            task.extra.get("priority_name").and_then(Value::as_str),
            Some("High")
        );
    }

    #[test]
    fn done_issue_maps_status_to_done() {
        let issue = issue_done("PROJ-2", "PROJ", "My Project");
        let task = task_from_issue(&issue).unwrap();
        assert_eq!(task.status, "done");
        assert_eq!(task.priority, 3, "Medium → 3");
    }

    #[test]
    fn no_due_date_issue_maps_due_to_none() {
        let task = task_from_issue(&issue_no_due("DOCS-1")).unwrap();
        assert!(task.due.is_none());
        assert_eq!(task.priority, 1, "Low → 1");
        assert_eq!(task.tags, vec!["docs"]);
    }

    #[test]
    fn priority_mapping_covers_all_levels() {
        assert_eq!(map_priority(Some("Highest")), 5);
        assert_eq!(map_priority(Some("Critical")), 5);
        assert_eq!(map_priority(Some("Blocker")), 5);
        assert_eq!(map_priority(Some("High")), 5);
        assert_eq!(map_priority(Some("Major")), 5);
        assert_eq!(map_priority(Some("Medium")), 3);
        assert_eq!(map_priority(Some("Normal")), 3);
        assert_eq!(map_priority(Some("Low")), 1);
        assert_eq!(map_priority(Some("Minor")), 1);
        assert_eq!(map_priority(Some("Lowest")), 1);
        assert_eq!(map_priority(Some("Trivial")), 1);
        assert_eq!(map_priority(None), 0);
        assert_eq!(map_priority(Some("Unknown")), 0);
    }

    #[test]
    fn status_category_extraction() {
        assert_eq!(status_category(&issue_open("X-1", "X", "X")), "indeterminate");
        assert_eq!(status_category(&issue_done("X-2", "X", "X")), "done");
        assert_eq!(status_category(&issue_no_due("X-3")), "new");
    }

    /// Fixture modelled on the real `/rest/api/3/search/jql` response envelope
    /// (Atlassian OpenAPI spec, SearchAndReconcileResults schema): has `isLast`
    /// and `nextPageToken`; NO `total` or `startAt`.
    #[test]
    fn parse_search_page_handles_envelope_not_last() {
        // Mid-pagination response: more pages remain.
        let v = serde_json::json!({
            "issues": [{"key": "A-1"}, {"key": "A-2"}],
            "isLast": false,
            "nextPageToken": "eyJzdGFydCI6MTAwfQ=="
        });
        let page = parse_search_page(v);
        assert_eq!(page.issues.len(), 2);
        assert!(!page.is_last);
        assert_eq!(
            page.next_page_token.as_deref(),
            Some("eyJzdGFydCI6MTAwfQ==")
        );
    }

    #[test]
    fn parse_search_page_handles_envelope_last_page() {
        // Last page: `isLast=true`, no `nextPageToken` key.
        let v = serde_json::json!({
            "issues": [{"key": "A-3"}],
            "isLast": true
        });
        let page = parse_search_page(v);
        assert_eq!(page.issues.len(), 1);
        assert!(page.is_last);
        assert!(page.next_page_token.is_none());
    }

    #[test]
    fn parse_search_page_empty_response() {
        // Completely empty JSON object — treat as last page (no more data).
        let page = parse_search_page(serde_json::json!({}));
        assert!(page.issues.is_empty());
        assert!(page.is_last, "absent isLast defaults to true (stop, not spin)");
        assert!(page.next_page_token.is_none());
    }

    // ---- Credential parsing tests ------------------------------------------

    #[test]
    fn parses_three_line_credential() {
        let c = parse_pasted_credential(
            "myco.atlassian.net\nme@example.com\nATATsecrettoken",
        )
        .unwrap();
        assert_eq!(c.domain, "myco.atlassian.net");
        assert_eq!(c.email, "me@example.com");
        assert_eq!(c.api_token, "ATATsecrettoken");
    }

    #[test]
    fn strips_https_prefix_from_domain() {
        let c = parse_pasted_credential(
            "https://myco.atlassian.net/\nme@example.com\ntoken",
        )
        .unwrap();
        assert_eq!(c.domain, "myco.atlassian.net");
    }

    #[test]
    fn rejects_incomplete_credential() {
        assert!(parse_pasted_credential("myco.atlassian.net\nme@example.com").is_none());
        assert!(parse_pasted_credential("only one line").is_none());
        assert!(parse_pasted_credential("").is_none());
    }

    #[test]
    fn ignores_blank_lines_in_credential() {
        let c = parse_pasted_credential(
            "\nmyco.atlassian.net\n\nme@example.com\n\ntoken\n",
        )
        .unwrap();
        assert_eq!(c.domain, "myco.atlassian.net");
        assert_eq!(c.api_token, "token");
    }

    // ---- JQL datetime formatting -------------------------------------------

    #[test]
    fn to_jql_datetime_handles_jira_format() {
        // Jira timestamps: "YYYY-MM-DDTHH:MM:SS.mmm+HHMM" (no colon in offset)
        let dt = to_jql_datetime("2026-06-10T14:00:00.000+0000");
        assert_eq!(dt, "2026-06-10 14:00");
    }

    #[test]
    fn to_jql_datetime_normalizes_rfc3339_offset_to_utc() {
        // Strict RFC3339 with non-UTC offset.
        let dt = to_jql_datetime("2026-06-10T07:00:00-07:00");
        assert_eq!(dt, "2026-06-10 14:00");
    }

    #[test]
    fn normalize_jira_ts_inserts_colon_in_offset() {
        let n = normalize_jira_ts("2026-06-10T14:00:00.000+0000");
        assert_eq!(n, "2026-06-10T14:00:00+00:00");
        let n2 = normalize_jira_ts("2026-06-10T07:00:00.000-0700");
        assert_eq!(n2, "2026-06-10T07:00:00-07:00");
    }

    // ---- Cursor tests ------------------------------------------------------

    #[test]
    fn cursor_back_compat_empty_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_updated.is_none());
        let with = serde_json::from_str::<SyncState>(
            r#"{"last_updated":"2026-06-01T00:00:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!(
            with.last_updated.as_deref(),
            Some("2026-06-01T00:00:00+00:00")
        );
    }

    // ---- Full pull tests ---------------------------------------------------

    #[test]
    fn full_pull_writes_snapshot_and_raw_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();
        api.register_jql("assignee", vec![issue_open("PROJ-1", "PROJ", "My Project")]);
        api.register_jql("reporter", vec![issue_no_due("DOCS-1")]);
        // No Done queries needed on first sync.

        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("open"), Some(&2));
        assert_eq!(out.counts.get("created"), Some(&2));
        assert_eq!(out.counts.get("raw"), Some(&2));

        // Contract snapshot written.
        let snap = v.load_tasks_snapshot(SOURCE).unwrap();
        assert_eq!(snap.len(), 2);
        let proj1 = snap.iter().find(|t| t.id == "PROJ-1").unwrap();
        assert_eq!(proj1.title, "Implement OAuth flow");
        assert_eq!(proj1.project, "My Project");
        assert_eq!(proj1.priority, 5);

        // Raw firehose partitioned by created month.
        assert!(v.root().join("tasks/jira/raw/2026-06.jsonl").exists());
        let raw =
            std::fs::read_to_string(v.root().join("tasks/jira/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("PROJ-1"));
        assert!(raw.contains("DOCS-1"));

        // Cursor advanced; API token must not appear.
        let state = v.read_jira_sync();
        assert!(state.last_updated.is_some());
        let cursor_body =
            std::fs::read_to_string(v.root().join(".trove/jira-sync.json")).unwrap();
        assert!(!cursor_body.contains("secret") && !cursor_body.contains("ATAT"));
    }

    #[test]
    fn resync_dedupes_raw_by_key() {
        let v = temp_vault("rawdedup");
        let api1 = MockApi::new();
        api1.register_jql("assignee", vec![issue_open("PROJ-1", "PROJ", "Proj")]);
        api1.register_jql("reporter", vec![]);
        pull_with(&v, &api1, now()).unwrap();

        let api2 = MockApi::new();
        api2.register_jql("assignee", vec![issue_open("PROJ-1", "PROJ", "Proj")]);
        api2.register_jql("reporter", vec![]);
        api2.register_jql("Done", vec![]);
        api2.register_jql("statusCategory", vec![]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("raw"), Some(&0), "no new raw on re-sync");
        let raw =
            std::fs::read_to_string(v.root().join("tasks/jira/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "exactly one line");
    }

    #[test]
    fn issue_completed_logs_completion_event() {
        let v = temp_vault("fate-done");
        let api1 = MockApi::new();
        api1.register_jql("assignee", vec![issue_open("PROJ-1", "PROJ", "Proj")]);
        api1.register_jql("reporter", vec![]);
        pull_with(&v, &api1, now()).unwrap();
        assert_eq!(v.load_tasks_snapshot(SOURCE).unwrap().len(), 1);

        // Sync 2: issue gone from open; shows up in Done (statusCategory) query.
        let api2 = MockApi::new();
        api2.register_jql("assignee", vec![]); // no longer open in assignee
        api2.register_jql("reporter", vec![]);
        api2.register_jql("statusCategory", vec![issue_done("PROJ-1", "PROJ", "Proj")]);
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("completed"), Some(&1));
        assert!(v.load_tasks_snapshot(SOURCE).unwrap().is_empty());

        let events = v.task_events("2026-06-01", "2026-06-30").unwrap();
        let completed: Vec<_> =
            events.iter().filter(|e| e.kind == "completed").collect();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].task.id, "PROJ-1");
    }

    #[test]
    fn issue_deleted_when_gone_and_not_done() {
        let v = temp_vault("fate-delete");
        let api1 = MockApi::new();
        api1.register_jql("assignee", vec![issue_open("PROJ-1", "PROJ", "Proj")]);
        api1.register_jql("reporter", vec![]);
        pull_with(&v, &api1, now()).unwrap();

        let api2 = MockApi::new();
        api2.register_jql("assignee", vec![]);
        api2.register_jql("reporter", vec![]);
        api2.register_jql("statusCategory", vec![]); // not in done list → deleted
        let out = pull_with(&v, &api2, now()).unwrap();
        assert_eq!(out.counts.get("deleted"), Some(&1));
        // Deletions are stamped at sync time → the window tracks the real clock.
        let today = chrono::Local::now();
        let from = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let to = (today + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
        let events = v.task_events(&from, &to).unwrap();
        assert!(events.iter().any(|e| e.kind == "deleted" && e.task.id == "PROJ-1"));
    }

    #[test]
    fn fate_resolution_matrix() {
        let make_task = |id: &str| Task {
            source: SOURCE.into(),
            id: id.into(),
            title: "t".into(),
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
        let mut map = HashMap::new();
        map.insert("done".to_string(), "2026-06-13T16:30:00-07:00".to_string());
        map.insert("done_no_ts".to_string(), String::new());

        match jira_fate(&make_task("done"), Some(&map)) {
            TaskFate::Completed(Some(w)) => assert!(w.starts_with("2026-06-13")),
            _ => panic!("expected Completed(time)"),
        }
        assert!(matches!(
            jira_fate(&make_task("done_no_ts"), Some(&map)),
            TaskFate::Completed(None)
        ));
        assert!(matches!(jira_fate(&make_task("ghost"), Some(&map)), TaskFate::Deleted));
        assert!(matches!(jira_fate(&make_task("done"), None), TaskFate::Unknown));
    }

    /// Verify that drain_jql follows nextPageToken across multiple pages and
    /// does not stop early after the first page. This is the core regression
    /// test for the "silent multi-page strand" defect.
    #[test]
    fn drain_jql_follows_next_page_token_across_pages() {
        let v = temp_vault("multipage");
        let api = MockApi::new();
        // Simulate three pages of "assignee" results; reporter returns nothing.
        api.register_jql_pages("assignee", vec![
            vec![issue_open("PROJ-1", "PROJ", "My Project")],
            vec![issue_open("PROJ-2", "PROJ", "My Project")],
            vec![issue_open("PROJ-3", "PROJ", "My Project")],
        ]);
        api.register_jql("reporter", vec![]);

        let out = pull_with(&v, &api, now()).unwrap();
        // All three pages must have been drained.
        assert_eq!(
            out.counts.get("open"),
            Some(&3),
            "all 3 pages drained via nextPageToken"
        );
        assert_eq!(out.counts.get("created"), Some(&3));
        // Raw layer must contain all three keys.
        let raw =
            std::fs::read_to_string(v.root().join("tasks/jira/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("PROJ-1"));
        assert!(raw.contains("PROJ-2"));
        assert!(raw.contains("PROJ-3"));
    }

    #[test]
    fn dedup_across_assigned_and_reported() {
        let v = temp_vault("dedup-overlap");
        let api = MockApi::new();
        api.register_jql("assignee", vec![issue_open("PROJ-1", "PROJ", "Proj")]);
        api.register_jql("reporter", vec![issue_open("PROJ-1", "PROJ", "Proj")]);
        // same key from both queries — deduplicated
        let out = pull_with(&v, &api, now()).unwrap();
        assert_eq!(out.counts.get("open"), Some(&1), "dedup by key");
    }

    #[test]
    fn raw_issue_roundtrips_full_fidelity() {
        let v = issue_open("PROJ-1", "PROJ", "My Project");
        let r = raw_issue(&v).unwrap();
        assert_eq!(r.key(), "PROJ-1");
        assert_eq!(r.created(), "2026-06-01T08:00:00.000+0000");
        let line = serde_json::to_string(&r).unwrap();
        assert!(!line.contains("\"jira_key\""), "no synthetic keys");
        let back: RawIssue = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn connection_stores_token_without_leaking_api_token_to_cursor() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "ATAT_super_secret_token".into(),
                refresh_token: None,
                token_type: Some("myco.atlassian.net".into()),
                scope: Some("me@example.com".into()),
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert!(status.accounts[0].label.contains("me@example.com"));
        assert!(status.accounts[0].label.contains("myco.atlassian.net"));

        // Cursor must not expose the API token.
        v.write_jira_sync(&SyncState {
            last_updated: Some("2026-06-10T14:00:00+00:00".into()),
        })
        .unwrap();
        let cursor =
            std::fs::read_to_string(v.root().join(".trove/jira-sync.json")).unwrap();
        assert!(
            !cursor.contains("ATAT_super_secret_token"),
            "API token must not appear in cursor"
        );

        def_disconnect(&v, "jira").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            if let Ok(entries) = std::fs::read_dir(&sync_dir) {
                for entry in entries.flatten() {
                    let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                    if body.contains("myco.atlassian.net") {
                        let mode =
                            entry.path().metadata().unwrap().permissions().mode() & 0o777;
                        assert_eq!(mode, 0o600, "credential file must be 0600");
                    }
                }
            }
        }
    }

    #[test]
    fn pull_without_connection_returns_clear_error() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "got: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "jira");
    }
}
