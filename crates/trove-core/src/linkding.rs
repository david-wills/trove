//! Linkding — self-hosted bookmark manager; periodic cloud sync via the REST
//! API into the already-bound [`crate::reading`] contract.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/linkding.md.
//!
//! A **Periodic** cloud pull — two endpoints drained per sync:
//!
//! - `GET {instance}/api/bookmarks/?limit=100&offset=N` — active bookmarks;
//!   each bookmark becomes a [`crate::reading::Item`] under
//!   `reading/linkding/YYYY-MM.jsonl` (`guid` = bookmark `id`; `ts` =
//!   `date_added`; `description` → `excerpt`; `notes` → `extra.notes`;
//!   `tag_names` → `tags`; state = `"saved"`).
//! - `GET {instance}/api/bookmarks/archived/?limit=100&offset=N` — archived
//!   bookmarks (separate endpoint; `is_archived` is always `true` here);
//!   mapped identically but with state = `"archived"`.
//! - `added_since` cursor: once the full history is drained, subsequent syncs
//!   pass `date_added__gt={last_added}` on both endpoints to skip bookmarks
//!   already collected. The writer is append-only (guids are not re-written on
//!   edits), so the cursor correctly tracks new additions only.
//!
//! Auth: `Authorization: Token {api_token}` header. The user supplies both the
//! instance URL and their API token as a composite `{url}|{token}` string
//! (pipe separator is safe since URLs never contain `|`).
//!
//! Two layers per bookmark:
//!
//! - **Raw:** `reading/linkding/raw/YYYY-MM.jsonl` — full-fidelity API
//!   objects, unconditional.
//! - **Contract:** `reading/linkding/YYYY-MM.jsonl` — normalized
//!   [`reading::Item`] rows, deduped by `guid` (= integer `id`).
//!
//! ## API field names (GET /api/bookmarks/ and /api/bookmarks/archived/ — linkding.link/api/)
//!
//! ```text
//! {
//!   "id":                   42,             // stable integer id
//!   "url":                  "https://…",
//!   "title":                "Article title",
//!   "description":          "A snippet or note",
//!   "notes":                "Markdown notes",
//!   "is_archived":          false,          // true when from /archived/ endpoint
//!   "unread":               true,
//!   "shared":               false,
//!   "tag_names":            ["rust", "local-first"],
//!   "date_added":           "2024-02-18T22:15:00.000000Z",
//!   "date_modified":        "2024-02-18T22:15:00.000000Z",
//!   "web_archive_snapshot_url": "https://…",
//!   "favicon_url":          "https://…",
//!   "preview_image_url":    "https://…"
//! }
//! ```
//!
//! List response wrapper:
//! `{"count":N,"next":"…","previous":"…","results":[…]}`
//!
//! Cursor: `.trove/linkding-sync.json` holds:
//! - `last_added` — `date_added` of the most recently added bookmark seen in
//!   the drain; passed as `date_added__gt` on subsequent syncs.
//! - `updated` — wall-clock time of the last successful sync.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Item;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer item stream.
const DIR: &str = "reading/linkding";
/// Full-fidelity raw API objects.
const RAW_DIR: &str = "reading/linkding/raw";

/// Non-secret rebuildable cursor. Deleting it forces a full re-drain.
const SYNC_FILE: &str = ".trove/linkding-sync.json";

/// Service id under `.trove/sync/` where the composite `url|token` is stored.
const SERVICE: &str = "linkding";

/// Items per page (Linkding default max is 100).
const PER_PAGE: u64 = 100;

/// HTTP timeout per request. Self-hosted instances can be LAN-slow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs. Hourly is plenty for bookmarks.
pub const LINKDING_SYNC_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("bookmarks").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("linkding synced — {n} bookmarks")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "linkding sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("bookmarks").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Linkding is up to date — no new bookmarks".to_string()
    } else {
        format!("Linkding synced — {n} bookmarks")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "linkding",
        name: "Linkding",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs bookmarks and tags from your self-hosted Linkding instance via its REST \
                      API. Paste your instance URL and API token to connect.",
        domain: "reading",
        vault_path: "reading/linkding/",
        toggleable: true,
        setup: &[
            "Open your Linkding instance and go to Settings → Integrations.",
            "Copy the REST API token shown on that page.",
            "Paste your instance URL and token here as: https://your-instance.example.com|your-token",
        ],
        caveats: "Linkding must be reachable from this machine. Many instances are LAN-only (HTTP \
                  is accepted — HTTPS is not required for self-hosted targets).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(LINKDING_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("linkding"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = composite "url|token" string).
//
// A pipe `|` is chosen as the separator: it never appears in URLs (RFC 3986)
// and never in Linkding's hex API tokens.

/// Parse the composite `url|token` credential pasted by the user.
fn parse_credential(raw: &str) -> Result<(String, String)> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!(
            "enter your Linkding instance URL and API token separated by a pipe, \
             e.g. https://your-instance.example.com|your-token"
        );
    }
    let (url, token) = raw.split_once('|').ok_or_else(|| {
        anyhow::anyhow!(
            "expected url|token separated by a pipe — \
             e.g. https://your-instance.example.com|your-token"
        )
    })?;
    let url = url.trim().trim_end_matches('/');
    let token = token.trim();
    if url.is_empty() {
        bail!("instance URL is empty — provide the base URL of your Linkding instance");
    }
    if token.is_empty() {
        bail!("token is empty — copy the REST API token from Linkding Settings → Integrations");
    }
    Ok((url.to_string(), token.to_string()))
}

fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let (url, token) = parse_credential(raw)?;

    // Light verify: fetch one bookmark to confirm the URL+token combination works.
    let client = LinkdingClient::new(url.clone(), token.clone());
    match client.get_page(0, 1, false, None) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Linkding rejected the token — copy it fresh from Settings → Integrations on your \
             Linkding instance"
        ),
        Err(FetchError::Other(msg)) => bail!("Linkding connection check failed: {msg}"),
    }

    // Store as "url|token" so the pull fn can recover both.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: format!("{url}|{token}"),
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
    if let Some(ts) = vault.load_sync_token(SERVICE)? {
        // Display the instance URL as the human-readable label.
        let label = ts
            .access_token
            .split_once('|')
            .map(|(u, _)| u.to_string())
            .unwrap_or_else(|| "Linkding".to_string());
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
    id: "linkding",
    display_name: "Linkding",
    methods: &[ConnectMethod::TokenPaste {
        label: "Linkding instance URL and API token",
        help: "Paste your Linkding instance base URL and API token separated by a pipe: \
               https://your-instance.example.com|your-token. \
               Find the token in Settings → Integrations on your Linkding instance. \
               HTTP is accepted for local instances.",
        placeholder: "https://your-instance.example.com|your-api-token",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["linkding"],
    setup: &[
        "Open your Linkding instance in a browser.",
        "Go to Settings → Integrations to find your REST API token.",
        "Copy the token, then paste here as: https://your-instance.example.com|your-token",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable for tests.

/// One page of bookmarks from `GET /api/bookmarks/`.
struct Page {
    items: Vec<Value>,
    /// True when there is no next page (the API's `next` field is null/absent).
    is_last: bool,
}

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The endpoint the pull needs — a trait so tests run fully offline.
trait LinkdingApi {
    /// Fetch one page of bookmarks.
    ///
    /// When `archived` is `false`, hits `GET /api/bookmarks/` (active only).
    /// When `archived` is `true`, hits `GET /api/bookmarks/archived/`.
    ///
    /// `added_since` is passed as `date_added__gt` when present (ISO 8601 UTC
    /// with microseconds, strictly-after semantics per the Linkding source).
    fn get_page(
        &self,
        offset: u64,
        limit: u64,
        archived: bool,
        added_since: Option<&str>,
    ) -> Result<Page, FetchError>;
}

/// Thin HTTP client. Base URL is injected so tests can swap it for a mock.
struct LinkdingClient {
    base: String,
    token: String,
}

impl LinkdingClient {
    fn new(base: String, token: String) -> Self {
        LinkdingClient { base, token }
    }
}

impl LinkdingApi for LinkdingClient {
    fn get_page(
        &self,
        offset: u64,
        limit: u64,
        archived: bool,
        added_since: Option<&str>,
    ) -> Result<Page, FetchError> {
        // Active bookmarks: /api/bookmarks/   Archived: /api/bookmarks/archived/
        let path = if archived { "api/bookmarks/archived/" } else { "api/bookmarks/" };
        let url = format!("{}/{}", self.base, path);
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Token {}", self.token))
            .query("limit", &limit.to_string())
            .query("offset", &offset.to_string());

        // date_added__gt: strictly-after semantics (bookmarks added after the
        // cursor). Captures only new additions, which matches the append-only
        // write path (no guid updates ever overwrite an existing row).
        if let Some(since) = added_since {
            req = req.query("date_added__gt", since);
        }

        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(parse_page(v))
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(format!("I/O error: {e}"))),
        }
    }
}

/// Parse the Linkding paginated response:
/// `{"count":N,"next":"…"|null,"previous":"…"|null,"results":[…]}`
fn parse_page(v: Value) -> Page {
    let items = v
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // `next` is null or absent when this is the last page.
    let is_last = v.get("next").map_or(true, Value::is_null);
    Page { items, is_last }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// `date_added` of the most recently added bookmark seen in the last
    /// drain, in the format the API returns (ISO 8601 UTC with microseconds).
    /// Passed as `date_added__gt` on the next sync to skip already-collected
    /// bookmarks.  Old cursors may carry `last_modified` (pre-fix field name)
    /// — those are silently ignored and a full re-drain happens once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_added: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_linkding_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_linkding_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row.

/// Full-fidelity API bookmark object. The `ts` field drives partition filing
/// but is not serialized (the object already carries `date_added`).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// A top-level string field, trimmed; "" when missing/null/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// RFC3339-ish timestamp (Linkding delivers ISO 8601 UTC with microseconds,
/// e.g. `"2024-02-18T22:15:00.000000Z"`) → RFC3339 local.
/// Unparseable/empty values pass through verbatim.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Insert `k` → `v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// Map one Linkding API bookmark object to a contract [`Item`].
///
/// Returns `None` when the bookmark lacks a usable `id` (the dedup guid) or
/// a usable `date_added` timestamp (needed to partition the row by month).
///
/// Field mapping (official API — linkding.link/api/):
/// - `id`            → `guid`     (integer id, stringified)
/// - `url`           → `url`
/// - `title`         → `title`
/// - `description`   → `excerpt`  (the user's snippet/description)
/// - `notes`         → `extra.notes` (Markdown notes field)
/// - `tag_names`     → `tags`     (already a string array)
/// - `date_added`    → `ts`       (ISO 8601 UTC → local RFC3339)
/// - `is_archived`   → `state`    (`"archived"` or `"saved"`)
/// - `unread`        → `state = "saved"` override when true + extra flag
/// - `shared`        → `extra.shared`
/// - `date_modified` → `extra.date_modified`
/// - `web_archive_snapshot_url`, `favicon_url`, `preview_image_url` → `extra`
pub fn bookmark_to_item(v: &Value) -> Option<Item> {
    // guid: stable integer `id`.
    let guid = match v.get("id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return None,
    };

    // ts: `date_added` → local RFC3339.
    let raw_ts = str_field(v, "date_added");
    if raw_ts.is_empty() {
        return None;
    }
    let ts = to_local(&raw_ts);
    // Must be partitionable (YYYY-MM prefix).
    Partition::Month.key(&ts)?;

    let url = str_field(v, "url");
    let title = str_field(v, "title");
    let excerpt = str_field(v, "description");

    // tags: `tag_names` is already a JSON string array.
    let tags: Vec<String> = v
        .get("tag_names")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    // state: archived beats all; unread keeps "saved"; shared/default → "saved".
    let is_archived = v.get("is_archived").and_then(Value::as_bool) == Some(true);
    let state = if is_archived { "archived" } else { "saved" }.to_string();

    let mut extra = Map::new();
    // notes (Markdown notes, separate from description).
    put_str(&mut extra, "notes", &str_field(v, "notes"));
    // shared flag.
    if let Some(shared) = v.get("shared").and_then(Value::as_bool) {
        extra.insert("shared".into(), Value::Bool(shared));
    }
    // unread flag.
    if let Some(unread) = v.get("unread").and_then(Value::as_bool) {
        extra.insert("unread".into(), Value::Bool(unread));
    }
    // date_modified — kept for cursor tracking (raw form, not localized).
    put_str(&mut extra, "date_modified", &str_field(v, "date_modified"));
    // Snapshot/preview metadata.
    put_str(
        &mut extra,
        "web_archive_snapshot_url",
        &str_field(v, "web_archive_snapshot_url"),
    );
    put_str(&mut extra, "favicon_url", &str_field(v, "favicon_url"));
    put_str(
        &mut extra,
        "preview_image_url",
        &str_field(v, "preview_image_url"),
    );

    Some(Item {
        ts,
        source: "linkding".into(),
        guid,
        url,
        title,
        author: String::new(),
        site: String::new(),
        feed: String::new(),
        excerpt,
        tags,
        state,
        progress: None,
        read_at: String::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

fn write_bookmarks(vault: &Vault, rows: Vec<(Item, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Collect guids already stored so re-pulls are idempotent.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<Item> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (item, raw_val) in rows {
        if item.guid.is_empty() || !seen.insert(item.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val });
        new_rows.push(item);
    }

    contract.append(&new_rows, |r| r.ts.as_str())?;
    raw.append(&new_raws, |r| r.ts.as_str())?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the stored credential and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let credential = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Linkding is not connected — add your instance URL and API token in the Integrations tab")?;

    let (url, token) = parse_credential(&credential)?;
    let client = LinkdingClient::new(url, token);
    pull_with(vault, &client)
}

/// Drain one endpoint (active or archived) into `all_rows`.
///
/// Tracks the newest `date_added` seen among successfully-mapped items so the
/// cursor never advances past an item that was silently dropped due to a
/// mapping failure.
fn drain_endpoint(
    api: &impl LinkdingApi,
    archived: bool,
    added_since: Option<&str>,
    all_rows: &mut Vec<(Item, Value)>,
    new_last_added: &mut Option<String>,
) -> Result<()> {
    let mut offset: u64 = 0;
    let endpoint = if archived { "/api/bookmarks/archived/" } else { "/api/bookmarks/" };

    loop {
        let page = api
            .get_page(offset, PER_PAGE, archived, added_since)
            .map_err(|e| anyhow::anyhow!("Linkding fetch failed ({endpoint}): {e}"))?;

        if page.items.is_empty() {
            break;
        }

        for raw_val in &page.items {
            if let Some(item) = bookmark_to_item(raw_val) {
                // Only advance the cursor for items that successfully mapped
                // (defect 3 fix): a bookmark that fails parsing is never
                // silently skipped past.
                let da = str_field(raw_val, "date_added");
                if !da.is_empty() {
                    let is_newer = new_last_added
                        .as_deref()
                        .map_or(true, |cur| da.as_str() > cur);
                    if is_newer {
                        *new_last_added = Some(da);
                    }
                }
                all_rows.push((item, raw_val.clone()));
            }
        }

        if page.is_last {
            break;
        }

        offset += PER_PAGE;
    }
    Ok(())
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl LinkdingApi) -> Result<PullOutcome> {
    let mut state = vault.read_linkding_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // On incremental syncs pass the cursor as `date_added__gt`; on first run
    // (no cursor) drain the full history.  The writer is append-only so
    // `date_added` is the right watermark (edits don't produce new rows).
    let added_since = state.last_added.as_deref();

    // Track the newest `date_added` of successfully-mapped items across both
    // endpoints so we can advance the cursor after a successful full drain.
    let mut new_last_added: Option<String> = state.last_added.clone();

    let mut all_rows: Vec<(Item, Value)> = Vec::new();

    // Drain active bookmarks (GET /api/bookmarks/).
    drain_endpoint(api, false, added_since, &mut all_rows, &mut new_last_added)?;

    // Drain archived bookmarks (GET /api/bookmarks/archived/).
    // These live at a separate endpoint — the active endpoint never returns
    // is_archived=true, so skipping this call means archived bookmarks are
    // never collected.
    drain_endpoint(api, true, added_since, &mut all_rows, &mut new_last_added)?;

    // --- Persist ---
    let written = write_bookmarks(vault, all_rows)?;
    counts.insert("bookmarks", written);

    // Advance the cursor only after the full drain (crash safety rule).
    if new_last_added != state.last_added {
        state.last_added = new_last_added;
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_linkding_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("Linkding synced — {written} bookmarks"),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-linkding-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — from official API docs (linkding.link/api/).
    //
    // GET /api/bookmarks/ response:
    //   {"count":N,"next":…|null,"previous":…|null,"results":[…]}
    //
    // Bookmark object fields:
    //   id, url, title, description, notes, is_archived, unread, shared,
    //   tag_names, date_added, date_modified,
    //   web_archive_snapshot_url, favicon_url, preview_image_url

    fn bookmark_tagged() -> Value {
        json!({
            "id": 42,
            "url": "https://blog.example.org/rust-local-first",
            "title": "Local-First Software in Rust",
            "description": "A survey of local-first Rust libraries.",
            "notes": "Great overview — bookmark for later.",
            "is_archived": false,
            "unread": true,
            "shared": false,
            "tag_names": ["rust", "local-first"],
            "date_added": "2024-02-18T22:15:00.000000Z",
            "date_modified": "2024-02-18T22:15:00.000000Z",
            "web_archive_snapshot_url": "",
            "favicon_url": "https://blog.example.org/favicon.ico",
            "preview_image_url": ""
        })
    }

    fn bookmark_archived() -> Value {
        json!({
            "id": 99,
            "url": "https://example.com/archived-page",
            "title": "An Archived Page",
            "description": "",
            "notes": "",
            "is_archived": true,
            "unread": false,
            "shared": true,
            "tag_names": ["archive"],
            "date_added": "2023-11-05T10:00:00.000000Z",
            "date_modified": "2024-01-20T08:30:00.000000Z",
            "web_archive_snapshot_url": "https://web.archive.org/save/https://example.com/archived-page",
            "favicon_url": "",
            "preview_image_url": ""
        })
    }

    fn bookmark_no_tags() -> Value {
        json!({
            "id": 7,
            "url": "https://example.net/bare",
            "title": "",
            "description": "",
            "notes": "",
            "is_archived": false,
            "unread": false,
            "shared": false,
            "tag_names": [],
            "date_added": "2026-06-15T14:00:00.000000Z",
            "date_modified": "2026-06-15T14:00:00.000000Z",
            "web_archive_snapshot_url": "",
            "favicon_url": "",
            "preview_image_url": ""
        })
    }

    fn page_response(results: Vec<Value>, has_next: bool) -> Value {
        json!({
            "count": results.len(),
            "next": if has_next { json!("https://instance/api/bookmarks/?limit=100&offset=100") } else { json!(null) },
            "previous": json!(null),
            "results": results
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_tagged_bookmark_to_item() {
        let b = bookmark_tagged();
        let item = bookmark_to_item(&b).unwrap();

        assert_eq!(item.source, "linkding");
        assert_eq!(item.guid, "42", "guid = id (stringified integer)");
        assert_eq!(item.url, "https://blog.example.org/rust-local-first");
        assert_eq!(item.title, "Local-First Software in Rust");
        assert_eq!(item.excerpt, "A survey of local-first Rust libraries.");
        assert_eq!(item.tags, vec!["rust", "local-first"]);
        assert_eq!(item.state, "saved", "active bookmark → saved");
        // ts is the local-tz rendering of 2024-02-18T22:15:00Z
        let epoch = DateTime::parse_from_rfc3339(&item.ts).unwrap().timestamp();
        let expected =
            DateTime::parse_from_rfc3339("2024-02-18T22:15:00Z").unwrap().timestamp();
        assert_eq!(epoch, expected, "ts preserves the UTC instant");
        // extra fields
        assert_eq!(
            item.extra.get("notes"),
            Some(&json!("Great overview — bookmark for later."))
        );
        assert_eq!(item.extra.get("shared"), Some(&json!(false)));
        assert_eq!(item.extra.get("unread"), Some(&json!(true)));
        assert_eq!(
            item.extra.get("favicon_url"),
            Some(&json!("https://blog.example.org/favicon.ico"))
        );
    }

    #[test]
    fn maps_archived_bookmark_to_state_archived() {
        let b = bookmark_archived();
        let item = bookmark_to_item(&b).unwrap();

        assert_eq!(item.guid, "99");
        assert_eq!(item.state, "archived", "is_archived=true → state=archived");
        assert_eq!(item.tags, vec!["archive"]);
        assert_eq!(item.extra.get("shared"), Some(&json!(true)));
        assert_eq!(
            item.extra.get("web_archive_snapshot_url"),
            Some(&json!("https://web.archive.org/save/https://example.com/archived-page"))
        );
    }

    #[test]
    fn maps_bare_bookmark_no_tags_no_title() {
        let b = bookmark_no_tags();
        let item = bookmark_to_item(&b).unwrap();

        assert_eq!(item.guid, "7");
        assert!(item.tags.is_empty(), "empty tag_names → empty tags");
        assert!(item.title.is_empty(), "empty title passes through");
        assert_eq!(item.state, "saved");
    }

    #[test]
    fn omit_empty_fields_on_serialize() {
        let b = bookmark_no_tags();
        let item = bookmark_to_item(&b).unwrap();
        let v = serde_json::to_value(&item).unwrap();

        // Required fields always present.
        assert!(v.get("ts").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("guid").is_some());
        // state="saved" is non-empty so it IS serialized.
        assert_eq!(v.get("state").and_then(|s| s.as_str()), Some("saved"));
        // Empty-omit fields absent.
        assert!(v.get("tags").is_none(), "empty tags omitted");
        assert!(v.get("excerpt").is_none(), "empty excerpt omitted");
        assert!(v.get("author").is_none());
        assert!(v.get("progress").is_none());
        assert!(v.get("read_at").is_none());
    }

    #[test]
    fn rejects_bookmark_with_missing_id() {
        let mut b = bookmark_tagged();
        b.as_object_mut().unwrap().remove("id");
        assert!(bookmark_to_item(&b).is_none(), "no id → None");
    }

    #[test]
    fn rejects_bookmark_with_missing_date_added() {
        let mut b = bookmark_tagged();
        b.as_object_mut().unwrap().remove("date_added");
        assert!(bookmark_to_item(&b).is_none(), "no date_added → None");
    }

    #[test]
    fn partitions_by_local_month_of_ts() {
        let b = bookmark_tagged();
        let item = bookmark_to_item(&b).unwrap();
        let month_key = Partition::Month.key(&item.ts).unwrap();
        assert_eq!(month_key.len(), 7, "YYYY-MM format");
        assert!(month_key.starts_with("202"), "plausible year prefix");
    }

    // -----------------------------------------------------------------------
    // Credential parsing.

    #[test]
    fn parse_credential_splits_on_first_pipe() {
        let (url, token) =
            parse_credential("https://my.instance.com|abc123def456").unwrap();
        assert_eq!(url, "https://my.instance.com");
        assert_eq!(token, "abc123def456");
    }

    #[test]
    fn parse_credential_strips_trailing_slash() {
        let (url, _) =
            parse_credential("https://my.instance.com/|abc123def456").unwrap();
        assert_eq!(url, "https://my.instance.com");
    }

    #[test]
    fn parse_credential_rejects_missing_pipe() {
        assert!(parse_credential("https://my.instance.com").is_err());
    }

    #[test]
    fn parse_credential_rejects_empty() {
        assert!(parse_credential("").is_err());
    }

    #[test]
    fn parse_credential_rejects_empty_url() {
        assert!(parse_credential("|abc123").is_err());
    }

    #[test]
    fn parse_credential_rejects_empty_token() {
        assert!(parse_credential("https://example.com|").is_err());
    }

    // -----------------------------------------------------------------------
    // Page parsing.

    #[test]
    fn parse_page_detects_last_page_when_next_is_null() {
        let resp = page_response(vec![bookmark_tagged()], false);
        let page = parse_page(resp);
        assert!(page.is_last, "next=null → is_last");
        assert_eq!(page.items.len(), 1);
    }

    #[test]
    fn parse_page_detects_non_last_page_when_next_is_present() {
        let resp = page_response(vec![bookmark_tagged()], true);
        let page = parse_page(resp);
        assert!(!page.is_last, "next=url → not last");
    }

    #[test]
    fn parse_page_empty_results_is_last() {
        let resp = page_response(vec![], false);
        let page = parse_page(resp);
        assert!(page.is_last, "empty results → last (no next)");
        assert!(page.items.is_empty());
    }

    // -----------------------------------------------------------------------
    // Mock API.
    //
    // `MockApi` holds separate page lists for the active and archived
    // endpoints so integration tests can exercise both.  `active_pages` drives
    // GET /api/bookmarks/; `archived_pages` drives GET /api/bookmarks/archived/.

    struct MockApi {
        active_pages: Vec<Vec<Value>>,
        archived_pages: Vec<Vec<Value>>,
    }

    impl MockApi {
        /// Only active bookmarks; archived endpoint returns empty.
        fn single(items: Vec<Value>) -> Self {
            MockApi { active_pages: vec![items], archived_pages: vec![] }
        }
        /// Paginated active bookmarks; archived endpoint returns empty.
        fn multi(pages: Vec<Vec<Value>>) -> Self {
            MockApi { active_pages: pages, archived_pages: vec![] }
        }
        /// Active bookmarks on one page + archived bookmarks on one page.
        fn with_archived(active: Vec<Value>, archived: Vec<Value>) -> Self {
            MockApi {
                active_pages: vec![active],
                archived_pages: vec![archived],
            }
        }
    }

    impl LinkdingApi for MockApi {
        fn get_page(
            &self,
            offset: u64,
            limit: u64,
            archived: bool,
            _added_since: Option<&str>,
        ) -> Result<Page, FetchError> {
            let pages = if archived { &self.archived_pages } else { &self.active_pages };
            let page_idx = (offset / limit) as usize;
            if page_idx >= pages.len() {
                return Ok(Page { items: vec![], is_last: true });
            }
            let items = pages[page_idx].clone();
            let is_last = page_idx + 1 >= pages.len();
            Ok(Page { items, is_last })
        }
    }

    // -----------------------------------------------------------------------
    // Integration tests.

    #[test]
    fn full_pull_writes_raw_and_contract_and_advances_cursor() {
        let v = temp_vault("fullpull");
        // Active bookmarks go to the active endpoint; archived go to /archived/.
        let api = MockApi::with_archived(
            vec![bookmark_tagged(), bookmark_no_tags()],
            vec![bookmark_archived()],
        );

        let out = pull_with(&v, &api).unwrap();
        // 2 active + 1 archived = 3 total
        assert_eq!(out.counts.get("bookmarks"), Some(&3));

        // Contract files partitioned by local month.
        let contract_dir = v.root().join("reading/linkding");
        let jsonl_files: Vec<_> = std::fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().to_string();
                if name.ends_with(".jsonl") { Some(name) } else { None }
            })
            .collect();
        // 3 bookmarks across 3 different months (2023-11, 2024-02, 2026-06)
        assert_eq!(jsonl_files.len(), 3, "3 partition files: {jsonl_files:?}");

        // Raw layer exists.
        let raw_dir = v.root().join("reading/linkding/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") { Some(n) } else { None }
            })
            .collect();
        assert!(!raw_files.is_empty(), "raw files written: {raw_files:?}");

        // Raw retains all source fields.
        let raw_content: String = raw_files
            .iter()
            .map(|f| {
                std::fs::read_to_string(v.root().join("reading/linkding/raw").join(f)).unwrap()
            })
            .collect();
        assert!(raw_content.contains("\"is_archived\""), "raw keeps is_archived");
        assert!(raw_content.contains("\"tag_names\""), "raw keeps tag_names");

        // Cursor advanced (last_added tracks the newest date_added seen).
        let state = v.read_linkding_sync();
        assert!(state.last_added.is_some(), "cursor set after successful drain");
        assert!(state.updated.is_some());

        // Token NOT in cursor file.
        let cursor_text =
            std::fs::read_to_string(v.root().join(".trove/linkding-sync.json")).unwrap();
        assert!(!cursor_text.contains("abc123"), "token never in cursor file");
        // Old field name must not appear in cursor (renamed to last_added).
        assert!(
            !cursor_text.contains("\"last_modified\""),
            "old cursor field name absent: {cursor_text}"
        );
    }

    #[test]
    fn archived_bookmarks_are_collected_from_archived_endpoint() {
        let v = temp_vault("archived-endpoint");
        // If we only called /api/bookmarks/, the archived bookmark (id=99)
        // would never appear.  The pull must drain /api/bookmarks/archived/ too.
        let api = MockApi::with_archived(
            vec![bookmark_tagged()],            // active endpoint
            vec![bookmark_archived()],           // archived endpoint
        );
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("bookmarks"), Some(&2), "active + archived both collected");

        // Verify the archived item is present in the contract layer.
        let contract_dir = v.root().join("reading/linkding");
        let all_jsonl: String = std::fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| {
                let e = e.unwrap();
                if e.file_name().to_string_lossy().ends_with(".jsonl") {
                    Some(std::fs::read_to_string(e.path()).unwrap())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            all_jsonl.contains("\"archived\""),
            "archived state present in contract layer: {all_jsonl}"
        );
        assert!(
            all_jsonl.contains("\"99\"") || all_jsonl.contains("\"id\":99"),
            "archived bookmark id=99 in vault"
        );
    }

    #[test]
    fn second_pull_deduplicates_existing_guids() {
        let v = temp_vault("dedup");
        let api1 = MockApi::single(vec![bookmark_tagged()]);
        let first = pull_with(&v, &api1).unwrap();
        assert_eq!(first.counts.get("bookmarks"), Some(&1));

        // Second pull returns the same bookmark (from active endpoint) plus
        // an archived one from the archived endpoint — only the new one lands.
        let api2 = MockApi::with_archived(vec![bookmark_tagged()], vec![bookmark_archived()]);
        let second = pull_with(&v, &api2).unwrap();
        assert_eq!(
            second.counts.get("bookmarks"),
            Some(&1),
            "existing guid deduplicated, only new one counted"
        );
    }

    #[test]
    fn multi_page_drain_collects_all_bookmarks() {
        let v = temp_vault("multipage");
        // Two active pages; archived endpoint empty.
        let api = MockApi::multi(vec![
            vec![bookmark_tagged()],
            vec![bookmark_no_tags()],
        ]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("bookmarks"), Some(&2), "all pages drained");
    }

    #[test]
    fn empty_result_set_writes_zero_and_updates_cursor_time() {
        let v = temp_vault("empty");
        let api = MockApi::single(vec![]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("bookmarks"), Some(&0));
        // updated is set even on a zero-bookmark pull.
        let state = v.read_linkding_sync();
        assert!(state.updated.is_some(), "updated always set after successful pull");
    }

    #[test]
    fn pull_without_token_returns_clear_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        // Empty cursor deserializes cleanly (first-run state).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_added.is_none());
        assert!(empty.updated.is_none());

        // New field name.
        let partial: SyncState =
            serde_json::from_str(r#"{"last_added":"2024-02-18T22:15:00.000000Z"}"#).unwrap();
        assert_eq!(
            partial.last_added.as_deref(),
            Some("2024-02-18T22:15:00.000000Z")
        );
        assert!(partial.updated.is_none());

        // Old cursor (pre-fix with last_modified field) gracefully ignored:
        // serde skips unknown fields by default, so a full re-drain occurs
        // instead of using a stale date_modified watermark as date_added__gt.
        let old_cursor: SyncState =
            serde_json::from_str(r#"{"last_modified":"2024-02-18T22:15:00.000000Z"}"#).unwrap();
        assert!(
            old_cursor.last_added.is_none(),
            "old last_modified field not loaded as last_added — triggers re-drain"
        );
    }

    #[test]
    fn connection_def_is_token_paste_and_references_linkding() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "linkding");
        assert_eq!(DEF.connection, Some("linkding"));
        assert!(DEF.pull.is_some());
        assert!(DEF.last_data.is_some());
        assert_eq!(DEF.meta.domain, "reading");
    }

    #[test]
    fn stored_credential_shows_url_as_label_in_status() {
        let v = temp_vault("status");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "https://my.linkding.local|secret-token".into(),
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
        assert_eq!(
            status.accounts[0].label,
            "https://my.linkding.local",
            "instance URL shown as label, token hidden"
        );
        assert_eq!(status.accounts[0].key, "linkding");

        def_disconnect(&v, "linkding").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }
}
