//! Inoreader — cloud RSS reader with a Google-Reader-style API, pulled into the
//! bound [`crate::reading`] contract. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/inoreader.md.
//!
//! A **Periodic** cloud pull over the Inoreader Reader API v0:
//!
//! - `GET /reader/api/0/stream/contents/user/-/state/com.google/reading-list`
//!   — the user's full All-items stream, with an `ot` (older than) seconds
//!   cursor and `continuation` pagination. Up to 100 items per page; follow
//!   `continuation` until absent. Each item becomes a [`crate::reading::Item`]
//!   filed under `reading/inoreader/YYYY-MM.jsonl` (month of `ts`) plus a
//!   verbatim raw line under `reading/inoreader/raw/YYYY-MM.jsonl`.
//!
//! The persisted cursor lives in `.trove/inoreader-sync.json` (non-secret,
//! rebuildable). The watermark is `ot` = max `crawlTimeMsec/1000` (epoch
//! seconds) across the drain; advanced only *after* a complete drain.
//!
//! ## Auth
//!
//! OAuth 2.0 (`read` scope). The brief described a "personal token" path but
//! the official developer docs confirm no personal token exists — only OAuth 2.0
//! and the deprecated ClientLogin. Bearer token in `Authorization` header.
//! Access tokens expire; refresh token used for silent renewal.
//!
//! Requires an Inoreader Pro plan for API access.
//!
//! ## API field names (from inoreader.com/developers/stream-contents)
//!
//! Top-level stream response:
//! ```text
//! { "items": [...], "continuation": "gmMZgKmmqI4U" }
//! ```
//!
//! Item fields:
//! ```text
//! {
//!   "id":            "tag:google.com,2005:reader/item/000000693c3bc0c",  // stable guid
//!   "crawlTimeMsec": "1618211779000",    // fetch time, epoch milliseconds (string)
//!   "timestampUsec": "1618211779000000", // same, microseconds (string)
//!   "published":     1617969599,         // publisher publish date, epoch seconds (int)
//!   "title":         "...",
//!   "author":        "...",
//!   "canonical":     [{"href":"..."}],   // article URL
//!   "alternate":     [{"href":"...","type":"text/html"}],
//!   "origin": {
//!     "streamId":    "feed/...",
//!     "title":       "...",  // feed/publication title -> Item.feed
//!     "htmlUrl":     "..."   // publisher home URL -> Item.site
//!   },
//!   "summary":       {"content":"<html>..."},
//!   "categories": [          // state flags and user labels
//!     "user/-/state/com.google/read",
//!     "user/-/state/com.google/starred",
//!     "user/-/label/Tech"
//!   ]
//! }
//! ```

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Item;
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "inoreader";
const SERVICE: &str = "inoreader";

const DIR: &str = "reading/inoreader";
const RAW_DIR: &str = "reading/inoreader/raw";

/// Non-secret rebuildable cursor -- NOT under `.trove/sync/`.
const SYNC_FILE: &str = ".trove/inoreader-sync.json";

const API_BASE: &str = "https://www.inoreader.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Items per page (Inoreader max is 100).
const PAGE_SIZE: u32 = 100;

/// The reading-list stream id: all articles from all subscribed feeds.
const READING_LIST_STREAM: &str = "user/-/state/com.google/reading-list";

/// Seconds between periodic syncs. Every 30 minutes -- articles flow
/// continuously; the `ot`-cursor fetch is lightweight when idle.
pub const INOREADER_SYNC_SECS: u64 = 1_800;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static INOREADER: Provider = Provider {
    service: SERVICE,
    display_name: "Inoreader",
    auth_url: "https://www.inoreader.com/oauth2/auth",
    token_url: "https://www.inoreader.com/oauth2/token",
    // read scope: read-only access to the user's feeds and articles.
    scopes: "read",
    // Assigned unique production redirect port for inoreader (INDEX #225).
    redirect_port: 38805,
    use_pkce: false,
    // Inoreader uses client_id/secret in the form body (not HTTP Basic).
    basic_auth: false,
    default_client_id: option_env!("TROVE_INOREADER_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_INOREADER_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("articles").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("inoreader synced -- {n} articles")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("inoreader sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("articles").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Inoreader synced -- {n} articles"),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. The &DEF line already
/// exists -- do not add another.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "inoreader",
        name: "Inoreader",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Inoreader feed subscriptions, read articles, and starred items \
                      into the unified reading store via the Google-Reader-style API. First sync \
                      backfills everything; later syncs fetch only what's new.",
        domain: "reading",
        vault_path: "reading/inoreader/",
        toggleable: true,
        setup: &[
            "Connect your Inoreader account on this card.",
            "API access requires an Inoreader Pro plan (~$90/yr or $9.99/mo).",
            "First sync backfills all articles; later syncs are incremental.",
        ],
        caveats: "API access requires an Inoreader Pro subscription; free accounts cannot use \
                  the API (OPML feed export is available free via Settings -> Import/Export, but \
                  article history requires Pro).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(INOREADER_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("inoreader"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(INOREADER.service)?.is_some()
        || INOREADER.default_credentials().is_some();
    let accounts = match vault.load_sync_token(INOREADER.service)? {
        Some(token) => vec![ConnectedAccount {
            key: INOREADER.service.to_string(),
            label: INOREADER.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Only flag reconnect when token expired AND no refresh token.
            needs_reconnect: token.expired() && token.refresh_token.is_none(),
            extra: BTreeMap::new(),
        }],
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`]. One line required:
/// `&crate::inoreader::CONNECTION,`
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "inoreader",
    display_name: "Inoreader",
    methods: &[ConnectMethod::OAuth {
        provider: &INOREADER,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["inoreader"],
    setup: &[
        "Sign in at www.inoreader.com/developers and register an OAuth app (type: Web).",
        "Set the redirect URI to http://localhost:38805/callback -- must match exactly.",
        "Paste the app's Client ID and Client Secret here. An Inoreader Pro plan is required \
         for API access.",
    ],
};

/// Exchange the authorization code for a token and store it.
fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = creds
        .or_else(|| INOREADER.default_credentials())
        .context(
            "No Inoreader app credentials -- register an OAuth app at \
             www.inoreader.com/developers and paste the Client ID and Secret",
        )?;
    let flow = OauthFlow::start(&INOREADER, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, std::time::Duration::from_secs(120))?;
    vault.save_sync_app(INOREADER.service, &creds)?;
    vault.save_sync_token(INOREADER.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// HTTP layer -- injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Forbidden,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Forbidden => write!(f, "forbidden (HTTP 403)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// One page: items array plus continuation token (absent on last page).
struct Page {
    items: Vec<Value>,
    continuation: Option<String>,
}

/// Trait so tests drive the mapping/persist logic with fixtures, never the
/// network.
trait InoreaderApi {
    /// One page of stream contents. `ot` = epoch seconds lower bound (crawl
    /// time); `continuation` = opaque pagination token from a prior response.
    fn stream_page(
        &self,
        stream_id: &str,
        ot: Option<u64>,
        continuation: Option<&str>,
    ) -> Result<Page, FetchError>;
}

/// Thin HTTP client; base URL injected for testability.
struct InoreaderClient {
    base: String,
    token: String,
}

impl InoreaderClient {
    fn new(base: String, token: String) -> Self {
        InoreaderClient { base, token }
    }
}

impl InoreaderApi for InoreaderClient {
    fn stream_page(
        &self,
        stream_id: &str,
        ot: Option<u64>,
        continuation: Option<&str>,
    ) -> Result<Page, FetchError> {
        let url = format!(
            "{}/reader/api/0/stream/contents/{}",
            self.base,
            urlencoded(stream_id)
        );
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .query("n", &PAGE_SIZE.to_string())
            .query("output", "json");
        if let Some(ts) = ot {
            req = req.query("ot", &ts.to_string());
        }
        if let Some(c) = continuation {
            req = req.query("c", c);
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing stream: {e}")))?;
                Ok(parse_stream_page(v))
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
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

/// Minimal percent-encoding for the stream id in the URL path.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '/' => out.push(c),
            ':' => out.push_str("%3A"),
            ',' => out.push_str("%2C"),
            _ => {
                for b in c.to_string().as_bytes() {
                    out.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    out
}

/// Extract `items` + `continuation` from a stream/contents response.
fn parse_stream_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let items = o.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let continuation = o
                .get("continuation")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Page { items, continuation }
        }
        _ => Page { items: Vec::new(), continuation: None },
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max `crawlTimeMsec / 1000` seen across the drain -- the `ot` lower bound
    /// for the next poll. `None` on the first sync (backfill from beginning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ot: Option<u64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_inoreader_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_inoreader_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row -- the full API item object, tagged with the partition ts.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A top-level string field, trimmed; `""` when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Insert `k`->`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// `crawlTimeMsec` (a string containing epoch ms) -> epoch milliseconds as u64.
/// Returns `None` when absent, unparseable, or zero.
fn crawl_time_ms(item: &Value) -> Option<u64> {
    let s = item.get("crawlTimeMsec").and_then(Value::as_str).unwrap_or("");
    let ms: u64 = s.trim().parse().ok()?;
    (ms > 0).then_some(ms)
}

/// Epoch ms -> RFC3339 local time. Returns `None` when ms is 0.
fn epoch_ms_to_local(ms: u64) -> Option<String> {
    if ms == 0 {
        return None;
    }
    let secs = (ms / 1_000) as i64;
    let nanos = ((ms % 1_000) * 1_000_000) as u32;
    let dt = Utc.timestamp_opt(secs, nanos).single()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

/// Epoch seconds -> RFC3339 local time. Returns `None` when secs <= 0.
fn epoch_secs_to_local(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    let dt = Utc.timestamp_opt(secs, 0).single()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

/// Inoreader item -> contract [`Item`]. Returns `None` when the item has no `id`
/// (can't deduplicate) or no usable timestamp (can't partition).
///
/// Field mapping (from inoreader.com/developers/stream-contents):
/// - `id` -> `guid` (the stable `tag:google.com,...` long id)
/// - `crawlTimeMsec` (string epoch ms) -> `ts` (local); fallback to `published`
///   (epoch seconds). `crawlTimeMsec` is preferred -- it's when the article
///   entered the user's stream, the most identity-bearing time.
/// - `canonical[0].href` -> `url`; fallback to `alternate[0].href`
/// - `origin.title` -> `feed`
/// - `origin.htmlUrl` stripped to host -> `site`
/// - `title`, `author` -> contract fields of the same name
/// - `summary.content` -> `excerpt` (HTML snippet; stored as-is)
/// - `categories` -> `state` + `tags`
///   - contains `.../state/com.google/starred` -> `state = "favorite"`
///   - contains `.../state/com.google/read` (and no starred) -> `state = "read"`
///   - otherwise -> `state = "saved"`
///   - contains `.../label/<name>` -> extracted into `tags`
pub(crate) fn item_from(item: &Value) -> Option<Item> {
    let guid = str_field(item, "id");
    if guid.is_empty() {
        return None;
    }

    // ts: prefer crawlTimeMsec (ms string), fall back to published (int secs).
    let (ts, crawl_ms_raw) = {
        let cms = crawl_time_ms(item);
        let ts_str = cms
            .and_then(epoch_ms_to_local)
            .or_else(|| {
                item.get("published")
                    .and_then(Value::as_i64)
                    .and_then(epoch_secs_to_local)
            })?;
        (ts_str, cms)
    };
    // Ensure partition key derivable.
    Partition::Month.key(&ts)?;

    // URL: canonical[0].href preferred, then alternate (prefer text/html).
    let url = {
        let canonical = item
            .get("canonical")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|o| o.get("href"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !canonical.is_empty() {
            canonical
        } else {
            item.get("alternate")
                .and_then(Value::as_array)
                .and_then(|a| {
                    a.iter()
                        .find(|x| {
                            x.get("type").and_then(Value::as_str) == Some("text/html")
                        })
                        .or_else(|| a.first())
                })
                .and_then(|x| x.get("href"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        }
    };

    // Feed and site from origin.
    let feed = item
        .get("origin")
        .and_then(|o| o.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let site = {
        let html_url = item
            .get("origin")
            .and_then(|o| o.get("htmlUrl"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let without_scheme = html_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        without_scheme.split('/').next().unwrap_or("").trim().to_string()
    };

    // Excerpt from summary.content.
    let excerpt = item
        .get("summary")
        .and_then(|s| s.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // State and tags from the categories array.
    let (state, tags) = categories_to_state_and_tags(
        item.get("categories").and_then(Value::as_array),
    );

    // extra: source-specific fields not in contract columns.
    let mut extra = Map::new();
    if let Some(origin_stream_id) =
        item.get("origin").and_then(|o| o.get("streamId")).and_then(Value::as_str)
    {
        put_str(&mut extra, "origin_stream_id", origin_stream_id);
    }
    if let Some(cms) = crawl_ms_raw {
        extra.insert("crawl_time_ms".into(), Value::from(cms));
    }
    if let Some(pub_secs) = item.get("published").and_then(Value::as_i64) {
        if pub_secs > 0 {
            extra.insert("published_secs".into(), Value::from(pub_secs));
        }
    }

    Some(Item {
        ts,
        source: SOURCE.into(),
        guid,
        url,
        title: str_field(item, "title"),
        author: str_field(item, "author"),
        site,
        feed,
        excerpt,
        tags,
        state,
        progress: None,
        read_at: String::new(),
        extra,
    })
}

/// Parse the `categories` array into a (state, tags) pair.
///
/// - Any category ending in `/state/com.google/starred` -> `state = "favorite"` (wins).
/// - Any category ending in `/state/com.google/read` (and no starred) -> `state = "read"`.
/// - Otherwise -> `state = "saved"`.
/// - Any category matching `/label/<name>` -> tag.
pub(crate) fn categories_to_state_and_tags(
    cats: Option<&Vec<Value>>,
) -> (String, Vec<String>) {
    let mut is_read = false;
    let mut is_starred = false;
    let mut tags: Vec<String> = Vec::new();

    if let Some(arr) = cats {
        for cat in arr {
            let s = cat.as_str().unwrap_or("").trim();
            if s.is_empty() {
                continue;
            }
            if s.ends_with("/state/com.google/starred") {
                is_starred = true;
            } else if s.ends_with("/state/com.google/read") {
                is_read = true;
            } else if let Some(label) = extract_label(s) {
                if !label.is_empty() {
                    tags.push(label);
                }
            }
        }
    }

    let state = if is_starred {
        "favorite"
    } else if is_read {
        "read"
    } else {
        "saved"
    }
    .to_string();

    (state, tags)
}

/// Extract the label name from a `.../label/<name>` category string.
fn extract_label(cat: &str) -> Option<String> {
    let label_prefix = "/label/";
    let pos = cat.rfind(label_prefix)?;
    let name = cat[pos + label_prefix.len()..].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Max `crawlTimeMsec` epoch-ms across a slice of item objects.
fn max_crawl_ms(items: &[Value]) -> Option<u64> {
    items.iter().filter_map(crawl_time_ms).max()
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

fn write_items(vault: &Vault, rows: Vec<(Item, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids for deduplication.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (item, raw_val) in rows {
        if item.guid.is_empty() || !seen.insert(item.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val });
        new_items.push(item);
    }

    contract.append(&new_items, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_items.len() as u64)
}

// ---------------------------------------------------------------------------
// Drain + pull.

fn drain_stream(
    api: &impl InoreaderApi,
    stream_id: &str,
    ot: Option<u64>,
) -> Result<Vec<Value>, FetchError> {
    let mut all = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let page = api.stream_page(stream_id, ot, continuation.as_deref())?;
        all.extend(page.items);
        match page.continuation {
            Some(c) => continuation = Some(c),
            None => break,
        }
    }
    Ok(all)
}

/// Resolve and (if needed) silently refresh the stored OAuth token.
fn resolve_token(vault: &Vault) -> Result<String> {
    let mut token = vault
        .load_sync_token(SERVICE)?
        .context("Inoreader is not connected -- log in from the Integrations tab")?;
    if token.expired() {
        let creds = vault
            .load_sync_app(SERVICE)?
            .or_else(|| INOREADER.default_credentials())
            .context(
                "Inoreader token expired and no app credentials found -- reconnect from \
                 the Integrations tab",
            )?;
        token = oauth::refresh_token(&INOREADER, &creds, &token)
            .context("Inoreader token refresh failed -- reconnect from the Integrations tab")?;
        vault.save_sync_token(SERVICE, &token)?;
    }
    Ok(token.access_token)
}

/// Resolve the token and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = resolve_token(vault)?;
    let client = InoreaderClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl InoreaderApi) -> Result<PullOutcome> {
    let mut state = vault.read_inoreader_sync();
    let ot_secs = state.ot;

    let items = drain_stream(api, READING_LIST_STREAM, ot_secs).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Inoreader rejected the token (401) -- reconnect from the Integrations tab"
        ),
        FetchError::Forbidden => anyhow::anyhow!(
            "Inoreader returned 403 -- API access requires an Inoreader Pro plan"
        ),
        other => anyhow::anyhow!("Inoreader stream fetch failed: {other}"),
    })?;

    // Watermark: max crawlTimeMsec / 1000 (the API `ot` parameter is seconds).
    let new_ot = max_crawl_ms(&items).map(|ms| ms / 1_000);

    let rows: Vec<(Item, Value)> = items
        .iter()
        .filter_map(|it| item_from(it).map(|row| (row, it.clone())))
        .collect();

    let written = write_items(vault, rows)?;

    // Advance watermark only after the full drain succeeds, and only forward.
    if let Some(ot) = new_ot {
        if state.ot.is_none_or(|cur| ot > cur) {
            state.ot = Some(ot);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_inoreader_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("articles", written);
    Ok(PullOutcome { headline: format!("{written} articles"), counts })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-inoreader-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures -- modelled on inoreader.com/developers/stream-contents shape.

    /// A fully-featured item: crawlTimeMsec, starred, author, canonical url,
    /// user label tag.
    fn item_starred() -> Value {
        json!({
            "id": "tag:google.com,2005:reader/item/0000000693c3bc0c",
            "crawlTimeMsec": "1749344400000",
            "timestampUsec": "1749344400000000",
            "published": 1749340800i64,
            "title": "Windows and Linux devices are under attack by a new cryptomining worm",
            "author": "Dan Goodin",
            "canonical": [
                { "href": "https://arstechnica.com/?p=1755573" }
            ],
            "alternate": [
                { "href": "https://arstechnica.com/?p=1755573", "type": "text/html" }
            ],
            "summary": {
                "direction": "ltr",
                "content": "<div>Malware uses SSH brute-force attacks...</div>"
            },
            "origin": {
                "streamId": "feed/http://feeds.arstechnica.com/arstechnica/gadgets",
                "title": "Ars Technica Gear & Gadgets",
                "htmlUrl": "http://arstechnica.com/"
            },
            "categories": [
                "user/1005921515/state/com.google/reading-list",
                "user/1005921515/state/com.google/starred",
                "user/1005921515/label/Tech"
            ]
        })
    }

    /// An item that is read (not starred), no label tag, uses alternate URL.
    fn item_read() -> Value {
        json!({
            "id": "tag:google.com,2005:reader/item/0000000693c3bc0d",
            "crawlTimeMsec": "1749085200000",
            "timestampUsec": "1749085200000000",
            "published": 1749081600i64,
            "title": "RSS Is Not Dead",
            "author": "Bob Smith",
            "canonical": [],
            "alternate": [
                { "href": "https://blog.example.org/rss-is-not-dead", "type": "text/html" }
            ],
            "summary": {
                "direction": "ltr",
                "content": "<p>RSS is alive and well.</p>"
            },
            "origin": {
                "streamId": "feed/https://blog.example.org/feed",
                "title": "Example Dev Blog",
                "htmlUrl": "https://blog.example.org/about"
            },
            "categories": [
                "user/1005921515/state/com.google/reading-list",
                "user/1005921515/state/com.google/read"
            ]
        })
    }

    fn item_no_id() -> Value {
        json!({
            "id": "",
            "crawlTimeMsec": "1749344400000",
            "published": 1749340800i64,
            "title": "No Id Item"
        })
    }

    fn item_no_ts() -> Value {
        json!({
            "id": "tag:google.com,2005:reader/item/0000000000000001",
            "crawlTimeMsec": "0",
            "published": 0,
            "title": "No Timestamp Item"
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_starred_item_with_label_tag_and_canonical_url() {
        let it = item_from(&item_starred()).unwrap();
        assert_eq!(it.source, SOURCE);
        assert_eq!(it.guid, "tag:google.com,2005:reader/item/0000000693c3bc0c");
        assert_eq!(it.url, "https://arstechnica.com/?p=1755573");
        assert_eq!(
            it.title,
            "Windows and Linux devices are under attack by a new cryptomining worm"
        );
        assert_eq!(it.author, "Dan Goodin");
        assert_eq!(it.feed, "Ars Technica Gear & Gadgets");
        assert_eq!(it.site, "arstechnica.com");
        assert_eq!(it.excerpt, "<div>Malware uses SSH brute-force attacks...</div>");
        assert_eq!(it.tags, vec!["Tech"]);
        assert_eq!(it.state, "favorite", "starred -> favorite");
        assert!(it.progress.is_none());
        assert!(it.read_at.is_empty());
        let ts_ms = DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp_millis();
        assert_eq!(ts_ms, 1749344400000, "ts from crawlTimeMsec epoch ms");
        assert_eq!(
            it.extra.get("origin_stream_id"),
            Some(&json!("feed/http://feeds.arstechnica.com/arstechnica/gadgets"))
        );
        assert_eq!(it.extra.get("crawl_time_ms"), Some(&json!(1749344400000u64)));
        assert_eq!(it.extra.get("published_secs"), Some(&json!(1749340800i64)));
    }

    #[test]
    fn maps_read_item_with_alternate_url_and_read_state() {
        let it = item_from(&item_read()).unwrap();
        assert_eq!(it.guid, "tag:google.com,2005:reader/item/0000000693c3bc0d");
        assert_eq!(it.url, "https://blog.example.org/rss-is-not-dead");
        assert_eq!(it.state, "read", "read category -> read state");
        assert!(it.tags.is_empty());
        assert_eq!(it.site, "blog.example.org");
        let ts_ms = DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp_millis();
        assert_eq!(ts_ms, 1749085200000, "ts from crawlTimeMsec");
    }

    #[test]
    fn starred_takes_precedence_over_read_when_both_present() {
        let mut it = item_starred();
        it["categories"] = json!([
            "user/1005921515/state/com.google/reading-list",
            "user/1005921515/state/com.google/read",
            "user/1005921515/state/com.google/starred"
        ]);
        let row = item_from(&it).unwrap();
        assert_eq!(row.state, "favorite", "starred wins over read");
    }

    #[test]
    fn unread_item_without_starred_maps_to_saved() {
        let mut it = item_starred();
        it["categories"] = json!(["user/1005921515/state/com.google/reading-list"]);
        let row = item_from(&it).unwrap();
        assert_eq!(row.state, "saved", "no read/starred -> saved");
    }

    #[test]
    fn skips_item_with_no_id() {
        assert!(item_from(&item_no_id()).is_none(), "empty id -> skip");
    }

    #[test]
    fn skips_item_with_no_usable_timestamp() {
        assert!(
            item_from(&item_no_ts()).is_none(),
            "zero crawl + zero published -> skip"
        );
    }

    #[test]
    fn falls_back_to_published_when_crawl_time_is_zero() {
        let mut it = item_read();
        it["crawlTimeMsec"] = json!("0");
        let row = item_from(&it).unwrap();
        let ts_ms = DateTime::parse_from_rfc3339(&row.ts).unwrap().timestamp_millis();
        assert_eq!(ts_ms, 1749081600 * 1000, "fallback to published epoch secs");
    }

    #[test]
    fn categories_to_state_handles_all_variants() {
        let (s, t) = categories_to_state_and_tags(Some(&vec![json!(
            "user/1/state/com.google/reading-list"
        )]));
        assert_eq!(s, "saved");
        assert!(t.is_empty());

        let (s, _) =
            categories_to_state_and_tags(Some(&vec![json!("user/1/state/com.google/read")]));
        assert_eq!(s, "read");

        let (s, _) = categories_to_state_and_tags(Some(&vec![
            json!("user/1/state/com.google/read"),
            json!("user/1/state/com.google/starred"),
        ]));
        assert_eq!(s, "favorite");

        let (_, tags) = categories_to_state_and_tags(Some(&vec![
            json!("user/1/label/Tech"),
            json!("user/1/label/AI"),
        ]));
        let mut sorted = tags.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["AI", "Tech"]);
    }

    #[test]
    fn extract_label_parses_label_categories() {
        assert_eq!(extract_label("user/1005921515/label/Tech"), Some("Tech".into()));
        assert_eq!(extract_label("user/-/label/My Feeds"), Some("My Feeds".into()));
        assert_eq!(extract_label("user/1/state/com.google/read"), None);
        assert_eq!(extract_label("user/1/state/com.google/reading-list"), None);
    }

    #[test]
    fn urlencoded_handles_stream_id_characters() {
        let encoded = urlencoded("user/-/state/com.google/reading-list");
        assert!(encoded.contains("com.google"), "dots pass through");
        assert!(!encoded.contains(':'), "colon must be encoded");
        let tag_encoded = urlencoded("tag:google.com,2005:reader/item/abc");
        assert!(tag_encoded.contains("%3A"), "colon encoded");
        assert!(tag_encoded.contains("%2C"), "comma encoded");
    }

    #[test]
    fn parse_stream_page_reads_items_and_continuation() {
        let p = parse_stream_page(json!({
            "items": [{"id": "a"}, {"id": "b"}],
            "continuation": "tok123"
        }));
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.continuation.as_deref(), Some("tok123"));

        let last = parse_stream_page(json!({"items": [{"id": "c"}]}));
        assert_eq!(last.continuation, None);

        let empty = parse_stream_page(json!({"items": []}));
        assert_eq!(empty.items.len(), 0);
    }

    #[test]
    fn max_crawl_ms_finds_maximum() {
        let items = vec![
            json!({"crawlTimeMsec": "100"}),
            json!({"crawlTimeMsec": "300"}),
            json!({"crawlTimeMsec": "200"}),
        ];
        assert_eq!(max_crawl_ms(&items), Some(300));
        assert_eq!(max_crawl_ms(&[json!({"crawlTimeMsec": "0"})]), None);
        assert_eq!(max_crawl_ms(&[]), None);
    }

    // -----------------------------------------------------------------------
    // Integration: mock API -> pull -> vault.

    struct MockApi {
        pages: RefCell<VecDeque<Result<Page, FetchError>>>,
        ot_log: RefCell<Vec<Option<u64>>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(VecDeque::new()),
                ot_log: RefCell::new(Vec::new()),
            }
        }
        fn page(self, items: Vec<Value>, continuation: Option<&str>) -> Self {
            self.pages.borrow_mut().push_back(Ok(Page {
                items,
                continuation: continuation.map(str::to_string),
            }));
            self
        }
    }

    impl InoreaderApi for MockApi {
        fn stream_page(
            &self,
            _stream_id: &str,
            ot: Option<u64>,
            _cont: Option<&str>,
        ) -> Result<Page, FetchError> {
            self.ot_log.borrow_mut().push(ot);
            self.pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Page { items: vec![], continuation: None }))
        }
    }

    #[test]
    fn full_pull_writes_contract_and_raw_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new().page(vec![item_starred(), item_read()], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("articles"), Some(&2));

        // Contract: partitioned by crawlTimeMsec month. Both items in June 2025.
        let contract =
            std::fs::read_to_string(v.root().join("reading/inoreader/2025-06.jsonl")).unwrap();
        assert_eq!(contract.lines().count(), 2, "two contract rows");
        assert!(contract.contains("\"state\":\"favorite\""));
        assert!(contract.contains("\"state\":\"read\""));
        assert!(contract.contains("\"source\":\"inoreader\""));
        assert!(contract.contains("\"feed\":\"Ars Technica Gear & Gadgets\""));
        assert!(contract.contains("\"tags\":[\"Tech\"]"));

        // Raw: same partition.
        let raw = std::fs::read_to_string(
            v.root().join("reading/inoreader/raw/2025-06.jsonl"),
        )
        .unwrap();
        assert_eq!(raw.lines().count(), 2, "two raw rows");
        assert!(raw.contains("\"timestampUsec\""), "raw keeps API fields");
        assert!(raw.contains("\"crawlTimeMsec\""));

        // Watermark = max crawlTimeMsec / 1000 = 1749344400.
        let state = v.read_inoreader_sync();
        assert_eq!(state.ot, Some(1749344400), "watermark = max crawl ms / 1000");
        assert!(state.updated.is_some());

        // Cursor file has no access token.
        let cursor =
            std::fs::read_to_string(v.root().join(".trove/inoreader-sync.json")).unwrap();
        assert!(!cursor.contains("Bearer"), "token never in cursor file");

        // Second run with same items: guid dedupe, byte-identical files.
        let api2 = MockApi::new().page(vec![item_starred(), item_read()], None);
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("articles"), Some(&0), "re-run dedupes");
        let contract2 =
            std::fs::read_to_string(v.root().join("reading/inoreader/2025-06.jsonl")).unwrap();
        assert_eq!(contract, contract2, "byte-identical after re-run");
    }

    #[test]
    fn pagination_follows_continuation_before_mapping() {
        let v = temp_vault("pagination");
        let api = MockApi::new()
            .page(vec![item_starred()], Some("page2"))
            .page(vec![item_read()], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("articles"), Some(&2), "both pages drained");
        let ot_log = api.ot_log.borrow();
        assert_eq!(ot_log.len(), 2, "two page fetches");
        assert_eq!(ot_log[0], None, "first sync has no ot");
    }

    #[test]
    fn partial_drain_failure_does_not_advance_watermark() {
        let v = temp_vault("partialfail");
        let api = MockApi::new();
        api.pages.borrow_mut().push_back(Ok(Page {
            items: vec![item_starred()],
            continuation: Some("page2".into()),
        }));
        api.pages
            .borrow_mut()
            .push_back(Err(FetchError::Other("boom".into())));

        let err = pull_with(&v, &api).unwrap_err().to_string();
        assert!(err.contains("stream fetch failed"), "error surfaced: {err}");
        let state = v.read_inoreader_sync();
        assert!(state.ot.is_none(), "no partial watermark advance");
    }

    #[test]
    fn watermark_advances_only_forward() {
        let v = temp_vault("watermark");
        let api = MockApi::new().page(vec![item_read()], None);
        pull_with(&v, &api).unwrap();
        let ot_after_first = v.read_inoreader_sync().ot.unwrap();

        // Second pull with older crawlTimeMsec must not regress the watermark.
        let mut older = item_read();
        older["crawlTimeMsec"] = json!("1000");
        older["id"] = json!("tag:google.com,2005:reader/item/older_item");
        let api2 = MockApi::new().page(vec![older], None);
        pull_with(&v, &api2).unwrap();
        assert_eq!(
            v.read_inoreader_sync().ot,
            Some(ot_after_first),
            "watermark must not regress"
        );
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.ot.is_none());
        let partial: SyncState = serde_json::from_str(r#"{"ot":1749344400}"#).unwrap();
        assert_eq!(partial.ot, Some(1749344400));
        assert!(partial.updated.is_none());
    }

    #[test]
    fn provider_uses_assigned_redirect_port() {
        assert_eq!(INOREADER.redirect_port, 38805, "assigned production port");
    }

    #[test]
    fn connection_exposes_oauth_method() {
        assert!(CONNECTION.method("oauth").is_some(), "oauth method registered");
        assert_eq!(CONNECTION.id, "inoreader");
        assert_eq!(DEF.connection, Some("inoreader"));
    }
}
