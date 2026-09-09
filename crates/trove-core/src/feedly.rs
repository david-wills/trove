//! Feedly — cloud RSS reader with a documented v3 API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/feedly.md
//!
//! A **Periodic** cloud pull over two Feedly v3 endpoints:
//!
//! - `GET /v3/profile` — once per sync, to resolve the user's `id` (required
//!   for personal stream IDs).
//! - `GET /v3/streams/contents?streamId=user/UID/category/global.all` — poll
//!   the user's full All-feeds stream with a `newerThan` timestamp cursor
//!   (epoch ms). Up to 100 entries per page; follow the `continuation` token
//!   until absent. Each entry becomes a [`crate::reading::Item`] filed under
//!   `reading/feedly/YYYY-MM.jsonl` (month of `ts`) plus a verbatim raw line
//!   under `reading/feedly/raw/YYYY-MM.jsonl`.
//!
//! The persisted cursor lives in `.trove/feedly-sync.json` (non-secret,
//! rebuildable). The watermark is `newerThan` = max `crawled` epoch across the
//! drain; it is advanced only *after* a complete drain so a crash re-drains.
//!
//! Auth: personal developer token pasted by the user (Feedly Pro/Enterprise
//! required for API access). Stored under `.trove/sync/` (0600) via the secret
//! store; never logged or written to the cursor. The connect card carries a
//! plain-copy note that API access requires a Feedly Pro/Enterprise plan.
//!
//! OPML feed-list import (free-tier fallback) is handled via the generic import
//! box; the raw subscriptions snapshot lands in `reading/feedly/feeds.jsonl`.
//!
//! ## API field names (v3, JSON; from developers.feedly.com/docs/articlejson)
//!
//! Entry objects returned by `/v3/streams/contents`:
//!
//! ```text
//! {
//!   "id":            "…",         // unique immutable entry id → guid
//!   "title":         "…",         // article headline
//!   "author":        "…",         // byline
//!   "published":     1234567890000, // publisher publish date, epoch ms
//!   "crawled":       1234567890000, // when Feedly first fetched it, epoch ms (watermark)
//!   "updated":       1234567890000, // publisher update date, epoch ms (optional)
//!   "unread":        true | false,  // read status
//!   "origin": {
//!     "streamId":    "feed/…",    // the RSS feed id
//!     "title":       "…",         // feed / publication title → Item.feed
//!     "htmlUrl":     "…"          // publisher's home URL → Item.site
//!   },
//!   "alternate": [                // publisher-supplied URLs
//!     { "href": "…", "type": "text/html" }
//!   ],
//!   "summary": {                  // publisher-supplied excerpt/snippet
//!     "content": "…",
//!     "direction": "ltr"
//!   },
//!   "content": {                  // full content when available
//!     "content": "…",
//!     "direction": "ltr"
//!   },
//!   "visual": { "url": "…", … }, // featured image (optional)
//!   "tags": [                     // boards the user saved it to
//!     { "id": "user/…/tag/…", "label": "…" }
//!   ],
//!   "categories": [               // folders the feed lives in
//!     { "id": "user/…/category/…", "label": "…" }
//!   ],
//!   "engagement":    42,          // social share count
//!   "keywords":      ["…"],       // publisher keywords
//!   "canonicalUrl":  "…"          // canonical URL when present (optional)
//! }
//! ```
//!
//! Top-level stream response from `/v3/streams/contents`:
//!
//! ```text
//! {
//!   "id":           "user/…/category/global.all",
//!   "items":        [ … ],   // array of entry objects
//!   "continuation": "…"      // absent on the last page
//! }
//! ```
//!
//! Profile response from `/v3/profile`:
//!
//! ```text
//! { "id": "c805fcbf-3acf-4302-a97e-d82f9d7c897f", "email": "…", … }
//! ```

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
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

/// Contract-layer item stream; raw under `raw/`.
const DIR: &str = "reading/feedly";
const RAW_DIR: &str = "reading/feedly/raw";
// Subscription snapshot path (non-contract raw; OPML imports via generic import
// box land at reading/feedly/feeds.jsonl — no code path yet, noted for the record).

/// Non-secret rebuildable cursor (NOT under `.trove/sync/` — that's 0600
/// secrets). Deleting it forces a full re-poll on the next sync.
const SYNC_FILE: &str = ".trove/feedly-sync.json";

/// Service id under `.trove/sync/` where the pasted token is stored.
const SERVICE: &str = "feedly";

/// API base.
const API_BASE: &str = "https://cloud.feedly.com";

/// Maximum entries per page (Feedly cap: 100).
const PAGE_SIZE: u32 = 100;

/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs. Every 30 minutes — articles flow continuously but
/// the Pro watermark fetch is lightweight when idle.
pub const FEEDLY_SYNC_SECS: u64 = 1_800;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("articles").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("feedly synced — {n} articles")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("feedly sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("articles").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Feedly synced — {n} articles"),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. The &DEF line already
/// exists — do not add another.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "feedly",
        name: "Feedly",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Feedly feed subscriptions, read articles, and saved items into \
                      the unified reading store via the official API. First sync backfills \
                      everything; later syncs fetch only what's new.",
        domain: "reading",
        vault_path: "reading/feedly/",
        toggleable: true,
        setup: &[
            "Connect with your Feedly developer token on this card.",
            "First sync backfills all articles; later syncs are incremental.",
            "API access requires a Feedly Pro or Enterprise plan.",
        ],
        caveats: "API access requires a Feedly Pro or Enterprise subscription; free accounts \
                  can export their feed list as OPML only (use the import box instead).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(FEEDLY_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("feedly"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste: paste the personal developer token.

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Feedly developer token from feedly.com/i/team/api");
    }
    // Verify with GET /v3/profile (401 → bad token, 403 → no API entitlement).
    let client = FeedlyClient::new(API_BASE.to_string(), token.to_string());
    match client.profile() {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Feedly rejected the token (401) — copy it fresh from feedly.com/i/team/api"
        ),
        Err(FetchError::Forbidden) => bail!(
            "Feedly returned 403 — API access requires a Feedly Pro or Enterprise plan"
        ),
        Err(e) => bail!("Feedly auth check failed: {e}"),
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
            label: "Feedly".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. A new line is required:
/// `&crate::feedly::CONNECTION,`
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "feedly",
    display_name: "Feedly",
    methods: &[ConnectMethod::TokenPaste {
        label: "Feedly developer token",
        help: "Paste your Feedly developer token from feedly.com/i/team/api — it is stored \
               locally and only ever sent to cloud.feedly.com. API access requires a Feedly \
               Pro or Enterprise plan; free accounts cannot use the API.",
        placeholder: "A0aa1bBb-1a1a-1a1a-1a1a-1a1a1a1a1a1a",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["feedly"],
    setup: &[
        "Open feedly.com/i/team/api while signed in to Feedly Pro or Enterprise.",
        "Click 'New API Token' and copy the token (it displays only once).",
        "Paste it here — it is stored locally, never sent anywhere but Feedly.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors.
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

/// One page: the items array plus the continuation cursor for the next page
/// (absent on the last page).
struct Page {
    items: Vec<Value>,
    continuation: Option<String>,
}

/// Trait so tests drive the mapping/persist logic with fixtures, never the network.
trait FeedlyApi {
    /// `GET /v3/profile` → the user's profile JSON (`id` field is the user id).
    fn profile(&self) -> Result<Value, FetchError>;

    /// `GET /v3/streams/contents` for the given `stream_id`, with optional
    /// `newer_than` (epoch ms) cursor and pagination `continuation`.
    fn stream_page(
        &self,
        stream_id: &str,
        newer_than: Option<u64>,
        continuation: Option<&str>,
    ) -> Result<Page, FetchError>;
}

/// Thin HTTP client. Base URL is injected for testability.
struct FeedlyClient {
    base: String,
    token: String,
}

impl FeedlyClient {
    fn new(base: String, token: String) -> Self {
        FeedlyClient { base, token }
    }
}

impl FeedlyApi for FeedlyClient {
    fn profile(&self) -> Result<Value, FetchError> {
        let url = format!("{}/v3/profile", self.base);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .call()
        {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing profile: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Forbidden),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }

    fn stream_page(
        &self,
        stream_id: &str,
        newer_than: Option<u64>,
        continuation: Option<&str>,
    ) -> Result<Page, FetchError> {
        let url = format!("{}/v3/streams/contents", self.base);
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .query("streamId", stream_id)
            .query("count", &PAGE_SIZE.to_string());
        if let Some(nt) = newer_than {
            req = req.query("newerThan", &nt.to_string());
        }
        if let Some(c) = continuation {
            req = req.query("continuation", c);
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

/// Extract `items` + `continuation` from a `/v3/streams/contents` response.
fn parse_stream_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let items =
                o.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
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
    /// Max `crawled` epoch-ms watermark across the drained articles. On the
    /// next sync we pass this as `newerThan`. `None` on the first sync
    /// (backfill from the beginning).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    newer_than: Option<u64>,
    /// Cached user id from `/v3/profile`. Stored so we don't re-fetch the
    /// profile on every sync (the id never changes). `None` until first sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_feedly_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_feedly_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row — the full API entry object, tagged with the partition ts.

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

/// Epoch milliseconds → RFC3339 local time. Returns `None` when missing or
/// zero (1970-01-01, likely a missing field the API serialized as 0).
fn epoch_ms_to_local(v: &Value, key: &str) -> Option<String> {
    let ms = v.get(key).and_then(Value::as_i64)?;
    if ms <= 0 {
        return None;
    }
    let secs = ms / 1_000;
    let nanos = ((ms % 1_000) * 1_000_000) as u32;
    let dt = Utc.timestamp_opt(secs, nanos).single()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

/// Insert `k`→`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// A Feedly entry → a contract [`Item`]. `None` when the entry has no `id`
/// (can't dedupe) or no usable timestamp (can't partition).
fn item_from_entry(e: &Value) -> Option<Item> {
    let guid = str_field(e, "id");
    if guid.is_empty() {
        return None;
    }

    // Detect global.saved board membership (the Feedly "Saved for Later" feature).
    // Tags whose id ends with "/tag/global.saved" are the built-in Saved board.
    // Other tags are arbitrary user boards (e.g. "Reading", "Recipes").
    let has_global_saved = e
        .get("tags")
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter().any(|t| {
                t.get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.ends_with("/tag/global.saved"))
                    .unwrap_or(false)
            })
        });

    // ts: for items saved to the global.saved board, prefer `actionTimestamp`
    // (when the user saved it — more identity-bearing than the publish date).
    // Fall back to `published`, then `crawled`. All are epoch ms.
    let ts = if has_global_saved {
        epoch_ms_to_local(e, "actionTimestamp")
            .or_else(|| epoch_ms_to_local(e, "published"))
            .or_else(|| epoch_ms_to_local(e, "crawled"))
    } else {
        epoch_ms_to_local(e, "published")
            .or_else(|| epoch_ms_to_local(e, "crawled"))
    }?;
    // Ensure the month partition key can be derived (guards against a bad ts).
    Partition::Month.key(&ts)?;

    // URL: `canonicalUrl` if present, else the first `alternate` with
    // type=text/html, falling back to alternate[0] if none is typed html.
    let url = {
        let canonical = str_field(e, "canonicalUrl");
        if !canonical.is_empty() {
            canonical
        } else {
            let alts = e.get("alternate").and_then(Value::as_array);
            // Prefer the first entry whose type is text/html (per Feedly docs,
            // alternate entries can carry non-html types).
            let href = alts
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
                .trim();
            href.to_string()
        }
    };

    // Feed/publisher from `origin`.
    let feed = e
        .get("origin")
        .and_then(|o| o.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Site (publisher home URL → domain). Use htmlUrl from origin, stripped to
    // host only for the `site` field.
    let site = {
        let html_url = e
            .get("origin")
            .and_then(|o| o.get("htmlUrl"))
            .and_then(Value::as_str)
            .unwrap_or("");
        // Very lightweight domain extraction (no full URL parse needed).
        // Strip scheme and trailing path: "https://blog.example.com/foo" → "blog.example.com".
        let without_scheme = html_url
            .trim_start_matches("https://")
            .trim_start_matches("http://");
        without_scheme.split('/').next().unwrap_or("").trim().to_string()
    };

    // Excerpt: first non-empty of `summary.content` or `content.content`.
    let excerpt = {
        let summary = e
            .get("summary")
            .and_then(|s| s.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !summary.is_empty() {
            summary
        } else {
            e.get("content")
                .and_then(|c| c.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        }
    };

    // Tags: the boards/tags the user saved this article to.
    let tags: Vec<String> = e
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let label = t.get("label").and_then(Value::as_str).unwrap_or("").trim();
                    (!label.is_empty()).then(|| label.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    // State: global.saved board membership → "favorite" (Feedly's Saved-for-Later
    // feature uses the special global.saved board, not arbitrary user boards).
    // Other board membership does NOT override the unread flag — a boarded article
    // that is also read should be "read", not "favorite".
    // unread=false → "read"; unread=true (or missing) → "saved".
    let unread = e.get("unread").and_then(Value::as_bool).unwrap_or(true);
    let state = if has_global_saved {
        "favorite"
    } else if !unread {
        "read"
    } else {
        "saved"
    }
    .to_string();

    // read_at: Feedly's stream-contents entry carries no per-article read
    // timestamp (read time lives only in the markers API, not in the entry
    // payload). The `crawled` field is a server fetch time unrelated to when
    // the user read the article. Leave read_at empty; state="read" already
    // records that the article was read without inventing a time.
    let read_at = String::new();

    // extra: source-specific fields that aren't contract columns.
    let mut extra = Map::new();
    // Original feed stream id, useful for cross-referencing.
    if let Some(stream_id) =
        e.get("origin").and_then(|o| o.get("streamId")).and_then(Value::as_str)
    {
        put_str(&mut extra, "origin_stream_id", stream_id);
    }
    if let Some(eng) = e.get("engagement").and_then(Value::as_i64) {
        extra.insert("engagement".into(), Value::from(eng));
    }
    // keywords from the publisher (if any).
    if let Some(kw) = e.get("keywords").and_then(Value::as_array) {
        let words: Vec<Value> = kw
            .iter()
            .filter_map(|v| {
                let s = v.as_str()?.trim();
                (!s.is_empty()).then(|| Value::String(s.to_string()))
            })
            .collect();
        if !words.is_empty() {
            extra.insert("keywords".into(), Value::Array(words));
        }
    }
    // crawled timestamp in raw epoch ms for watermarking transparency.
    if let Some(c) = e.get("crawled").and_then(Value::as_i64) {
        extra.insert("crawled_ms".into(), Value::from(c));
    }

    Some(Item {
        ts,
        source: "feedly".into(),
        guid,
        url,
        title: str_field(e, "title"),
        author: str_field(e, "author"),
        site,
        feed,
        excerpt,
        tags,
        state,
        progress: None, // Feedly doesn't report per-article read progress
        read_at,
        extra,
    })
}

/// Max `crawled` epoch-ms across a slice of entry objects. `None` when no
/// entry has a positive `crawled` value.
fn max_crawled(entries: &[Value]) -> Option<u64> {
    entries
        .iter()
        .filter_map(|e| e.get("crawled").and_then(Value::as_i64))
        .filter(|&ms| ms > 0)
        .max()
        .map(|ms| ms as u64)
}

// ---------------------------------------------------------------------------
// Write helpers — raw + contract, deduped by guid.

/// Append new contract + raw rows for the drain, deduped by guid. Returns the
/// number of new contract rows written.
fn write_items(
    vault: &Vault,
    rows: Vec<(Item, Value)>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids for deduplication (re-runnable: overlap never
    // duplicates, following the lastfm/pinboard/readwise pattern).
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

/// Drain every page from the stream, following `continuation` until absent.
/// The whole drain fails without advancing any watermark — a crash re-drains.
fn drain_stream(
    api: &impl FeedlyApi,
    stream_id: &str,
    newer_than: Option<u64>,
) -> Result<Vec<Value>, FetchError> {
    let mut all = Vec::new();
    let mut continuation: Option<String> = None;
    loop {
        let page = api.stream_page(stream_id, newer_than, continuation.as_deref())?;
        all.extend(page.items);
        match page.continuation {
            Some(c) => continuation = Some(c),
            None => break,
        }
    }
    Ok(all)
}

/// Resolve the token and sync. Missing token → quiet skip on the periodic
/// path (mirrors todoist/lastfm/pinboard); clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Feedly is not connected — add your developer token in the Integrations tab")?;
    let client = FeedlyClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl FeedlyApi) -> Result<PullOutcome> {
    let mut state = vault.read_feedly_sync();

    // Resolve (or cache) the user's id — needed for the personal stream id.
    let user_id = if let Some(uid) = &state.user_id {
        uid.clone()
    } else {
        let profile = api
            .profile()
            .map_err(|e| match e {
                FetchError::Unauthorized => anyhow::anyhow!(
                    "Feedly rejected the token (401) — reconnect from the Integrations tab"
                ),
                FetchError::Forbidden => anyhow::anyhow!(
                    "Feedly returned 403 — API access requires a Feedly Pro or Enterprise plan"
                ),
                other => anyhow::anyhow!("Feedly profile fetch failed: {other}"),
            })?;
        let uid = str_field(&profile, "id");
        if uid.is_empty() {
            bail!("Feedly profile response missing 'id' field");
        }
        uid
    };

    // The user's All-feeds stream — includes every article from every
    // subscribed feed. The `newerThan` cursor limits to articles crawled
    // after the last successful sync.
    let stream_id = format!("user/{user_id}/category/global.all");

    // Drain the full window before mapping or advancing the watermark.
    let entries = drain_stream(api, &stream_id, state.newer_than).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Feedly rejected the token (401) on stream fetch — reconnect from the Integrations tab"
        ),
        FetchError::Forbidden => anyhow::anyhow!(
            "Feedly returned 403 on stream fetch — check that API access is enabled for your plan"
        ),
        other => anyhow::anyhow!("Feedly stream fetch failed: {other}"),
    })?;

    // Advance the watermark candidate: max `crawled` across the whole drain.
    let crawled_max = max_crawled(&entries);

    // Map entries → (contract row, raw value) pairs.
    let rows: Vec<(Item, Value)> = entries
        .iter()
        .filter_map(|e| item_from_entry(e).map(|it| (it, e.clone())))
        .collect();

    let written = write_items(vault, rows)?;

    // Advance watermark and cache user_id only after the full drain succeeds.
    // Advance only forward (a re-pull of an older window must not regress).
    if let Some(cmax) = crawled_max {
        if state.newer_than.is_none_or(|cur| cmax > cur) {
            state.newer_than = Some(cmax);
        }
    }
    state.user_id = Some(user_id);
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_feedly_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("articles", written);
    Ok(PullOutcome {
        headline: format!("{written} articles"),
        counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-feedly-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures — modelled on the developers.feedly.com/docs/articlejson shape.

    /// A fully-featured article entry: saved to a board ("Reading"), published
    /// with an author, crawled at a different time, explicit canonicalUrl.
    fn entry_saved() -> Value {
        json!({
            "id": "u1KxEsXN5qEhEmFOfCGlwQ_0",
            "title": "A Deep Dive into Local-First Software",
            "author": "Jane Roe",
            "published": 1749340800000i64,   // 2025-06-08T00:00:00Z
            "crawled":   1749344400000i64,   // 2025-06-08T01:00:00Z
            "updated":   null,
            "unread": true,
            "canonicalUrl": "https://example.com/local-first",
            "alternate": [
                { "href": "https://example.com/local-first", "type": "text/html" }
            ],
            "origin": {
                "streamId": "feed/https://example.com/rss",
                "title": "Example Engineering Blog",
                "htmlUrl": "https://example.com/"
            },
            "summary": {
                "content": "The cloud is just someone else's computer.",
                "direction": "ltr"
            },
            "tags": [
                { "id": "user/abc123/tag/Reading", "label": "Reading" }
            ],
            "categories": [
                { "id": "user/abc123/category/Tech", "label": "Tech" }
            ],
            "engagement": 142,
            "keywords": ["local-first", "distributed systems"]
        })
    }

    /// An article with no canonicalUrl (must fall back to alternate[0].href),
    /// unread=false (state="read"), no tags (not saved to any board).
    fn entry_read() -> Value {
        json!({
            "id": "rss_abc_xyz_456",
            "title": "RSS Is Not Dead",
            "author": "Bob Smith",
            "published": 1749081600000i64,   // 2025-06-05T00:00:00Z
            "crawled":   1749085200000i64,   // 2025-06-05T01:00:00Z
            "unread": false,
            "alternate": [
                { "href": "https://blog.example.org/rss-is-not-dead", "type": "text/html" }
            ],
            "origin": {
                "streamId": "feed/https://blog.example.org/feed",
                "title": "Example Dev Blog",
                "htmlUrl": "https://blog.example.org/about"
            },
            "summary": {
                "content": "RSS is alive and well.",
                "direction": "ltr"
            },
            "tags": [],
            "engagement": 88
        })
    }

    /// An article with neither `published` nor `crawled` — must be skipped
    /// (can't partition without a usable timestamp).
    fn entry_no_timestamp() -> Value {
        json!({
            "id": "no_ts_article",
            "title": "No Timestamp Article",
            "published": 0,
            "crawled": 0
        })
    }

    /// An article with no `id` — must be skipped (can't deduplicate).
    fn entry_no_id() -> Value {
        json!({
            "id": "",
            "title": "No Id Article",
            "published": 1749340800000i64
        })
    }

    /// An article saved to Feedly's built-in "Saved for Later" board
    /// (global.saved). This is the ONLY board that maps to state="favorite".
    /// Also carries an actionTimestamp (when the user saved it) which should
    /// become the `ts` in preference to `published`.
    fn entry_global_saved() -> Value {
        json!({
            "id": "global_saved_article_1",
            "title": "Article Saved For Later",
            "author": "Alice",
            "published":       1749340800000i64,   // 2025-06-08T00:00:00Z
            "crawled":         1749344400000i64,   // 2025-06-08T01:00:00Z
            "actionTimestamp": 1749348000000i64,   // 2025-06-08T02:00:00Z (save time)
            "unread": true,
            "canonicalUrl": "https://example.com/save-this",
            "alternate": [
                { "href": "https://example.com/save-this", "type": "text/html" }
            ],
            "origin": {
                "streamId": "feed/https://example.com/rss",
                "title": "Example Blog",
                "htmlUrl": "https://example.com/"
            },
            "summary": { "content": "Worth reading later.", "direction": "ltr" },
            "tags": [
                { "id": "user/abc123/tag/global.saved", "label": "Saved" }
            ],
            "engagement": 10
        })
    }

    // ---------------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_saved_entry_with_tags_and_canonical_url() {
        // entry_saved has a generic "Reading" board (NOT global.saved) and unread=true.
        // State must be "saved" — generic board membership does NOT imply "favorite".
        // Only the global.saved board (Feedly's Saved-for-Later) maps to "favorite".
        let e = entry_saved();
        let it = item_from_entry(&e).unwrap();
        assert_eq!(it.source, "feedly");
        assert_eq!(it.guid, "u1KxEsXN5qEhEmFOfCGlwQ_0");
        assert_eq!(it.url, "https://example.com/local-first");
        assert_eq!(it.title, "A Deep Dive into Local-First Software");
        assert_eq!(it.author, "Jane Roe");
        assert_eq!(it.site, "example.com");
        assert_eq!(it.feed, "Example Engineering Blog");
        assert_eq!(it.excerpt, "The cloud is just someone else's computer.");
        assert_eq!(it.tags, vec!["Reading"], "board labels extracted as tags");
        assert_eq!(it.state, "saved", "generic board + unread=true → saved (not favorite)");
        assert!(it.read_at.is_empty(), "Feedly stream carries no per-article read time");
        // ts = published, in local time.
        let ts_ms =
            DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp_millis();
        assert_eq!(ts_ms, 1749340800000, "ts from published epoch ms");
        // extra carries engagement and keywords.
        assert_eq!(it.extra.get("engagement"), Some(&json!(142)));
        let kw = it.extra.get("keywords").and_then(Value::as_array).unwrap();
        assert_eq!(kw.len(), 2);
        assert_eq!(it.extra.get("origin_stream_id"), Some(&json!("feed/https://example.com/rss")));
    }

    #[test]
    fn global_saved_board_maps_to_favorite_and_uses_action_timestamp() {
        // The global.saved board (Feedly's Saved-for-Later) is the ONLY board that
        // should produce state="favorite". actionTimestamp is preferred for ts.
        let e = entry_global_saved();
        let it = item_from_entry(&e).unwrap();
        assert_eq!(it.state, "favorite", "global.saved board → favorite");
        assert!(it.read_at.is_empty(), "Feedly stream carries no per-article read time");
        // ts = actionTimestamp (save time), preferred over published for saved items.
        let ts_ms = DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp_millis();
        assert_eq!(ts_ms, 1749348000000, "ts from actionTimestamp (save time)");
        // tags still carry the board label.
        assert_eq!(it.tags, vec!["Saved"]);
    }

    #[test]
    fn boarded_and_read_article_is_read_not_favorite() {
        // An article in a generic board AND unread=false should be "read",
        // not "favorite". The has_tags short-circuit bug caused the read-state
        // to be lost in the original code.
        let mut e = entry_saved();
        e["unread"] = json!(false);
        let it = item_from_entry(&e).unwrap();
        assert_eq!(it.state, "read", "generic board + unread=false → read");
        assert!(it.read_at.is_empty(), "Feedly stream carries no per-article read time");
    }

    #[test]
    fn maps_read_entry_falls_back_to_alternate_url_and_state_is_read() {
        let e = entry_read();
        let it = item_from_entry(&e).unwrap();
        assert_eq!(it.guid, "rss_abc_xyz_456");
        assert_eq!(it.url, "https://blog.example.org/rss-is-not-dead");
        assert_eq!(it.site, "blog.example.org");
        assert_eq!(it.state, "read", "unread=false → read");
        // Feedly's stream-contents entry has no per-article read timestamp —
        // `crawled` is a server fetch time, not the user's read time. read_at
        // is intentionally empty; state="read" captures the fact of being read.
        assert!(it.read_at.is_empty(), "Feedly stream carries no per-article read time");
        assert!(it.tags.is_empty(), "no boards → no tags");
        assert!(it.progress.is_none(), "Feedly has no per-article progress");
    }

    #[test]
    fn skips_entry_with_no_usable_timestamp() {
        assert!(item_from_entry(&entry_no_timestamp()).is_none());
    }

    #[test]
    fn skips_entry_with_no_id() {
        assert!(item_from_entry(&entry_no_id()).is_none());
    }

    #[test]
    fn falls_back_to_crawled_when_published_is_zero() {
        let mut e = entry_saved();
        e["published"] = json!(0);
        // crawled = 1749344400000 → usable timestamp.
        let it = item_from_entry(&e).unwrap();
        let ts_ms =
            DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp_millis();
        assert_eq!(ts_ms, 1749344400000, "falls back to crawled");
    }

    #[test]
    fn epoch_ms_to_local_rejects_zero_and_negative() {
        let zero = json!({"crawled": 0i64});
        assert!(epoch_ms_to_local(&zero, "crawled").is_none());
        let neg = json!({"crawled": -1i64});
        assert!(epoch_ms_to_local(&neg, "crawled").is_none());
        let missing = json!({});
        assert!(epoch_ms_to_local(&missing, "crawled").is_none());
    }

    #[test]
    fn max_crawled_picks_maximum() {
        let entries = vec![
            json!({"crawled": 1000i64}),
            json!({"crawled": 3000i64}),
            json!({"crawled": 2000i64}),
            json!({"crawled": 0i64}),    // skipped: zero
            json!({"crawled": -1i64}),   // skipped: negative
        ];
        assert_eq!(max_crawled(&entries), Some(3000));
        assert_eq!(max_crawled(&[]), None);
    }

    #[test]
    fn parse_stream_page_extracts_items_and_continuation() {
        let resp = json!({
            "id": "user/abc/category/global.all",
            "items": [{"id": "a"}, {"id": "b"}],
            "continuation": "next_cursor_123"
        });
        let p = parse_stream_page(resp);
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.continuation.as_deref(), Some("next_cursor_123"));

        let last = parse_stream_page(json!({"items": [{"id": "c"}]}));
        assert_eq!(last.continuation, None, "missing continuation → last page");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.newer_than.is_none());
        assert!(empty.user_id.is_none());
        // An older cursor that only carried newer_than deserializes cleanly.
        let partial: SyncState =
            serde_json::from_str(r#"{"newer_than": 1749344400000}"#).unwrap();
        assert_eq!(partial.newer_than, Some(1749344400000u64));
        assert!(partial.user_id.is_none());
    }

    // ---------------------------------------------------------------------------
    // Mock API + integration test.

    struct MockApi {
        profile_resp: Result<Value, ()>,
        stream_pages: RefCell<VecDeque<Page>>,
        stream_calls: RefCell<Vec<(String, Option<u64>)>>,
    }

    impl MockApi {
        fn with_profile(uid: &str) -> Self {
            MockApi {
                profile_resp: Ok(json!({"id": uid, "email": "u@example.com"})),
                stream_pages: RefCell::new(VecDeque::new()),
                stream_calls: RefCell::new(Vec::new()),
            }
        }
        fn push_page(self, items: Vec<Value>, continuation: Option<&str>) -> Self {
            self.stream_pages.borrow_mut().push_back(Page {
                items,
                continuation: continuation.map(str::to_string),
            });
            self
        }
    }

    impl FeedlyApi for MockApi {
        fn profile(&self) -> Result<Value, FetchError> {
            self.profile_resp
                .as_ref()
                .map(|v| v.clone())
                .map_err(|_| FetchError::Unauthorized)
        }

        fn stream_page(
            &self,
            stream_id: &str,
            newer_than: Option<u64>,
            _continuation: Option<&str>,
        ) -> Result<Page, FetchError> {
            self.stream_calls.borrow_mut().push((stream_id.to_string(), newer_than));
            Ok(self
                .stream_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(Page { items: vec![], continuation: None }))
        }
    }

    #[test]
    fn full_pull_writes_contract_and_raw_dedupes_and_advances_watermark() {
        let v = temp_vault("fullpull");
        // Two pages; second has no continuation (last page).
        let api = MockApi::with_profile("user-abc")
            .push_page(vec![entry_saved()], Some("cursor_p2"))
            .push_page(vec![entry_read()], None);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("articles"), Some(&2));

        // Contract rows: entry_saved → 2025-06, entry_read → 2025-06.
        let items_jun = std::fs::read_to_string(
            v.root().join("reading/feedly/2025-06.jsonl"),
        )
        .unwrap();
        assert_eq!(items_jun.lines().count(), 2);
        // entry_saved: generic "Reading" board + unread=true → "saved" (not "favorite").
        // entry_read: unread=false → "read".
        assert!(items_jun.contains("\"state\":\"saved\""));
        assert!(items_jun.contains("\"state\":\"read\""));

        // Raw layer mirrors the same partition.
        let raw_jun = std::fs::read_to_string(
            v.root().join("reading/feedly/raw/2025-06.jsonl"),
        )
        .unwrap();
        assert!(raw_jun.contains("\"engagement\":142"), "raw keeps full API object");
        assert!(raw_jun.contains("\"engagement\":88"));

        // Cursor: newer_than = max crawled across both entries.
        // entry_saved.crawled = 1749344400000 (2025-06-08T01:00:00Z)
        // entry_read.crawled  = 1749085200000 (2025-06-05T01:00:00Z)
        // → max = 1749344400000.
        let state = v.read_feedly_sync();
        assert_eq!(state.newer_than, Some(1749344400000u64));
        assert_eq!(state.user_id.as_deref(), Some("user-abc"));
        assert!(state.updated.is_some());

        // Token never in the cursor file.
        let cursor = std::fs::read_to_string(v.root().join(".trove/feedly-sync.json")).unwrap();
        assert!(!cursor.contains("Bearer"), "token never in cursor");

        // Re-run with same entries → 0 new (guid dedupe).
        let api2 = MockApi::with_profile("user-abc")
            .push_page(vec![entry_saved(), entry_read()], None);
        let again = pull_with(&v, &api2).unwrap();
        assert_eq!(again.counts.get("articles"), Some(&0));
        let items_after = std::fs::read_to_string(
            v.root().join("reading/feedly/2025-06.jsonl"),
        )
        .unwrap();
        assert_eq!(items_after, items_jun, "file byte-identical after re-run");
    }

    #[test]
    fn user_id_cached_across_syncs() {
        let v = temp_vault("uid_cache");
        // First sync: profile is fetched.
        let api1 = MockApi::with_profile("user-xyz").push_page(vec![entry_saved()], None);
        pull_with(&v, &api1).unwrap();
        let state = v.read_feedly_sync();
        assert_eq!(state.user_id.as_deref(), Some("user-xyz"));

        // Second sync: profile API not needed (user_id cached in cursor).
        // If it were called, MockApi would need a real profile resp — but the
        // user_id is in the cursor so pull_with reads it from there directly.
        let api2 = MockApi::with_profile("user-xyz").push_page(vec![], None);
        pull_with(&v, &api2).unwrap();
        // Stream was called with the cached user-id based stream path.
        let calls = api2.stream_calls.borrow();
        assert!(calls[0].0.contains("user-xyz"), "stream called with cached user id");
    }

    #[test]
    fn skipped_entries_do_not_advance_count() {
        let v = temp_vault("skip_entries");
        let api = MockApi::with_profile("u1").push_page(
            vec![entry_no_timestamp(), entry_no_id(), entry_saved()],
            None,
        );
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("articles"), Some(&1), "only valid entries counted");
    }

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "feedly_secret_token".into(),
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
        assert_eq!(status.accounts[0].label, "Feedly");

        // Token must NOT appear in any non-secret (cursor) file.
        v.write_feedly_sync(&SyncState {
            newer_than: Some(1749344400000),
            user_id: Some("u1".into()),
            updated: Some("2025-06-08T01:00:00-07:00".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/feedly-sync.json")).unwrap();
        assert!(!cursor.contains("feedly_secret_token"), "token never in cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("feedly_secret_token") {
                    found = true;
                    let mode =
                        entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
            assert!(found, "token stored under .trove/sync");
        }

        def_disconnect(&v, "feedly").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_token_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err(), "empty token rejected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method_and_def_references_connection() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "feedly");
        assert_eq!(DEF.connection, Some("feedly"));
    }
}
