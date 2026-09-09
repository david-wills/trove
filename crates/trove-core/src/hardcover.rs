//! Hardcover reading library and reviews via the official GraphQL API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/hardcover.md.
//!
//! A **Periodic** cloud pull (hourly) over the Hardcover GraphQL API at
//! `api.hardcover.app/v1/graphql`, writing into the bound [`crate::reading`]
//! contract — the same domain as Readwise, Raindrop, and Pinboard.
//!
//! Two contract layers, both deduped by `guid`:
//!
//! - **Items** under `reading/hardcover/YYYY-MM.jsonl` — one [`reading::Item`]
//!   per `user_books` entry. `ts` = `date_added` (when the book joined the
//!   library); `state` maps the Hardcover status_id (1=saved/Want-to-Read,
//!   2=saved/Currently-Reading, 3=read, 4=saved/Paused, 5=archived/DNF,
//!   6=archived/Ignored); `progress` = pages-read as an integer percent of
//!   total-pages (when both are known); `read_at` = `last_read_date`.
//!   `guid` = `"hc-ub-{user_books.id}"`.
//!
//! - **Highlights** under `reading/hardcover/highlights/YYYY-MM.jsonl` — one
//!   [`reading::Highlight`] per `user_books` entry that has a written review.
//!   `ts` = `reviewed_at`; review text in `text`; book `title` + `author`
//!   inline. `guid` = `"hc-review-{user_books.id}"`.
//!
//! **Raw layer** (unconditional, full fidelity) under
//! `reading/hardcover/raw/YYYY-MM.jsonl` — the GraphQL `user_books` objects
//! verbatim, partitioned by `date_added` month.
//!
//! ## Cursor
//!
//! `.trove/hardcover-sync.json` (non-secret, rebuildable) tracks the max
//! `updated_at` across all synced `user_books` rows. Incremental polls send
//! `updated_at: { _gt: "<watermark>" }` to fetch only changed entries; a first
//! sync (no watermark) fetches everything. The watermark advances only after a
//! full drain so a crash re-drains.
//!
//! ## Auth
//!
//! Personal API token from hardcover.app/account/api. Stored under
//! `.trove/sync/` (0600) via [`crate::sync::oauth::TokenSet`] exactly like the
//! Todoist/Readwise token. Sent as `Authorization: Bearer <token>` on every
//! GraphQL POST. Verified at connect time with a lightweight `{ me { id } }`
//! probe.
//!
//! ## Rate limits
//!
//! Hardcover enforces 60 requests/min. An hourly incremental poll is one
//! request when idle; a first-sync backfill paginates with 100-item pages
//! (well under budget).
//!
//! ## `me` returns an array
//!
//! The Hardcover API wraps the current user as `[users!]!` — index `me[0]`.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::reading::{Highlight, Item};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer items stream; highlights and raw nest alongside.
const DIR: &str = "reading/hardcover";
const HIGHLIGHTS_DIR: &str = "reading/hardcover/highlights";
const RAW_DIR: &str = "reading/hardcover/raw";
/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (0600 secrets).
const SYNC_FILE: &str = ".trove/hardcover-sync.json";
/// Service id under `.trove/sync/` where the API token is stored.
const SERVICE: &str = "hardcover";

const API_BASE: &str = "https://api.hardcover.app/v1/graphql";
/// Conservative timeout — GraphQL responses are small.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Seconds between syncs. Hourly — reading events trickle in slowly.
pub const HARDCOVER_SYNC_SECS: u64 = 3600;
/// Items fetched per GraphQL request.
const PAGE_SIZE: i64 = 100;

// ---------------------------------------------------------------------------
// Status_id → reading contract state.

/// Map Hardcover `status_id` to the reading contract `state` string.
/// status_id: 1=Want to Read, 2=Currently Reading, 3=Read, 4=Paused,
/// 5=Did Not Finish, 6=Ignored.
fn status_to_state(status_id: i64) -> &'static str {
    match status_id {
        1 => "saved",    // Want to Read
        2 => "saved",    // Currently Reading (in-progress)
        3 => "read",     // Read
        4 => "saved",    // Paused
        5 => "archived", // Did Not Finish
        6 => "archived", // Ignored
        _ => "saved",
    }
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    let items = crate::registry::newest_stem(&vault.root().join(DIR));
    let highlights = crate::registry::newest_stem(&vault.root().join(HIGHLIGHTS_DIR));
    [items, highlights].into_iter().flatten().max()
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "hardcover synced — {} library entries, {} reviews",
                    c("items"),
                    c("highlights")
                )
            }))
        }
        Err(e) => {
            Ok(crate::registry::CollectOutcome::note(format!("hardcover sync skipped: {e}")))
        }
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Hardcover synced — {} library entries, {} reviews",
            c("items"),
            c("highlights")
        ),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "hardcover",
        name: "Hardcover",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Your Hardcover reading library, statuses, and reviews, pulled via the \
                      official GraphQL API into the unified reading store. First sync backfills \
                      your full library; later syncs fetch only changed entries.",
        domain: "reading",
        vault_path: "reading/hardcover/",
        toggleable: true,
        setup: &[
            "Connect with your Hardcover API token on this card.",
            "First sync backfills your entire library; later syncs are incremental.",
        ],
        caveats: "Rate-limited to 60 requests/min. Reading progress requires both pages-read \
                  and total-pages to be set in Hardcover.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(HARDCOVER_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("hardcover"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a personal API token, a SECRET).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Hardcover API token from hardcover.app/account/api");
    }
    let client = HardcoverClient::new(API_BASE.to_string(), token.to_string());
    match client.verify() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Hardcover rejected the token (401) — check it's your API token from \
             hardcover.app/account/api and hasn't been revoked"
        ),
        Err(e) => bail!("Hardcover API check failed: {e}"),
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
            label: "Hardcover".to_string(),
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
    id: "hardcover",
    display_name: "Hardcover",
    methods: &[ConnectMethod::TokenPaste {
        label: "Hardcover API token",
        help: "Paste your Hardcover API token from hardcover.app/account/api — it's stored \
               locally and never sent anywhere but Hardcover.",
        placeholder: "eyJhbGciOiJIUzI1NiJ9…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["hardcover"],
    setup: &[
        "Open hardcover.app/account/api while signed in to Hardcover.",
        "Copy your API token.",
        "Paste it here — it's stored locally and never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP / GraphQL layer — injectable so tests run fully offline.

/// GraphQL fetch errors: 401 wants a clear reconnect message, 429 is transient.
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

/// One page of `user_books` results plus the next offset (None when exhausted).
struct Page {
    items: Vec<Value>,
    next_offset: Option<i64>,
}

/// Injectable API trait — production POSTs to api.hardcover.app; tests drive
/// from fixtures without touching the network.
trait HardcoverApi {
    /// Fetch one page of `user_books`, optionally filtered by `updated_at_after`
    /// (ISO-8601 UTC for incremental syncs). `offset` for pagination.
    fn user_books(
        &self,
        token: &str,
        updated_at_after: Option<&str>,
        offset: i64,
    ) -> Result<Page, FetchError>;

    /// Lightweight probe: `{ me { id } }`. Returns Ok(()) on success.
    fn probe(&self, token: &str) -> Result<(), FetchError>;
}

/// Thin production client. Base URL injected (testable seam).
struct HardcoverClient {
    base: String,
    token: String,
}

impl HardcoverClient {
    fn new(base: String, token: String) -> Self {
        HardcoverClient { base, token }
    }

    fn verify(&self) -> Result<(), FetchError> {
        self.probe(&self.token)
    }

    /// POST a GraphQL query; return the parsed body or a `FetchError`.
    fn gql_post(
        &self,
        token: &str,
        query: &str,
        variables: Value,
    ) -> Result<Value, FetchError> {
        let body = serde_json::json!({ "query": query, "variables": variables });
        match ureq::post(&self.base)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_json(body)
        {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                // GraphQL surface-level errors: { "errors": [...] }
                if let Some(Value::Array(errs)) = v.get("errors") {
                    if !errs.is_empty() {
                        let msg = errs
                            .first()
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown GraphQL error");
                        return Err(FetchError::Other(msg.to_string()));
                    }
                }
                Ok(v)
            }
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
}

impl HardcoverApi for HardcoverClient {
    fn probe(&self, token: &str) -> Result<(), FetchError> {
        let v = self.gql_post(token, "{ me { id } }", Value::Null)?;
        // me returns [users!]! — check we got at least one user.
        let ok = v
            .get("data")
            .and_then(|d| d.get("me"))
            .and_then(Value::as_array)
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);
        if ok { Ok(()) } else { Err(FetchError::Unauthorized) }
    }

    fn user_books(
        &self,
        token: &str,
        updated_at_after: Option<&str>,
        offset: i64,
    ) -> Result<Page, FetchError> {
        let where_clause = if let Some(ts) = updated_at_after {
            serde_json::json!({ "updated_at": { "_gt": ts } })
        } else {
            Value::Object(Map::new())
        };

        // Query confirmed against hardcoverapp/hardcover-docs schema.graphql and
        // ThorbenWoelk/hardcover.cli client.rs. Field names verified from
        // schema-fields.json and field-descriptions.json in that repo.
        let query = r#"
            query($limit: Int!, $offset: Int!, $where: user_books_bool_exp) {
                me {
                    user_books(
                        limit: $limit,
                        offset: $offset,
                        order_by: { updated_at: asc },
                        where: $where
                    ) {
                        id
                        status_id
                        rating
                        review
                        reviewed_at
                        date_added
                        last_read_date
                        updated_at
                        read_count
                        owned
                        user_book_reads(order_by: { started_at: desc }, limit: 5) {
                            id
                            started_at
                            finished_at
                            progress_pages
                            progress_seconds
                        }
                        book {
                            id
                            title
                            slug
                            pages
                            release_year
                            cached_contributors
                        }
                    }
                }
            }
        "#;

        let variables = serde_json::json!({
            "limit": PAGE_SIZE,
            "offset": offset,
            "where": where_clause,
        });

        let v = self.gql_post(token, query, variables)?;

        // me returns [users!]! — index into me[0].
        let books: Vec<Value> = v
            .get("data")
            .and_then(|d| d.get("me"))
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|u| u.get("user_books"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let next_offset =
            if books.len() as i64 == PAGE_SIZE { Some(offset + PAGE_SIZE) } else { None };
        Ok(Page { items: books, next_offset })
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max `updated_at` across all synced `user_books` rows. Used as the `_gt`
    /// filter on the next incremental poll. ISO-8601 UTC string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_watermark: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_hardcover_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_hardcover_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row: verbatim API object, month-partitioned by ts.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping helpers.

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// RFC3339, naive datetime, or YYYY-MM-DD → RFC3339 local.
///
/// Hardcover's GraphQL schema uses `timestamptz` (UTC offset present) for
/// most fields, but `reviewed_at` is typed as `timestamp` (naive, no offset).
/// Hasura serialises `timestamp` without a timezone offset, e.g.
/// `"2024-03-15T10:00:00"`. We treat naive datetimes as UTC then convert to
/// local. Bare dates (YYYY-MM-DD) are treated as midnight UTC. Unparseable
/// values pass through verbatim so the caller can detect them.
fn to_local(s: &str) -> String {
    // RFC3339 with offset (the common case for timestamptz fields).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Local).to_rfc3339();
    }
    // Naive datetime with sub-seconds (Hasura may include fractional seconds).
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return ndt.and_utc().with_timezone(&Local).to_rfc3339();
    }
    // Naive datetime without sub-seconds (e.g. reviewed_at from Hasura).
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return ndt.and_utc().with_timezone(&Local).to_rfc3339();
    }
    // Bare date — treat as midnight UTC.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(0, 0, 0) {
            return dt.and_utc().with_timezone(&Local).to_rfc3339();
        }
    }
    s.to_string()
}

/// Insert k→v into extra only when v is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// Extract the primary author name from `cached_contributors`.
///
/// The Hardcover GraphQL API returns each contributor element as
/// `{"author": {"name": "..."}, "contribution": "Author"}` — the name is
/// nested two levels deep under `author.name`. This is confirmed by the
/// emgoto Hardcover API guide and the blampe/rreading-glasses + ThorbenWoelk
/// Go/Rust consumers. A flat `{"name": "..."}` fallback is retained for any
/// cached/serialised variants that may have been flattened client-side.
///
/// The field may also be stored as a pre-serialised JSON string, in which
/// case we parse it first and apply the same nested access.
fn author_from_book(book: &Value) -> String {
    fn name_from_contributor(c: &Value) -> Option<&str> {
        // Real API shape: {author: {name: "..."}, contribution: "..."}
        c.get("author")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
            // Flat fallback: {name: "..."} (client-flattened / cached variant)
            .or_else(|| c.get("name").and_then(Value::as_str))
    }

    let cc = book.get("cached_contributors");
    match cc {
        Some(Value::Array(arr)) => arr
            .first()
            .and_then(name_from_contributor)
            .unwrap_or("")
            .trim()
            .to_string(),
        Some(Value::String(s)) => {
            // May be a JSON-encoded array stored as a string.
            if let Ok(parsed) = serde_json::from_str::<Vec<Value>>(s) {
                parsed
                    .first()
                    .and_then(name_from_contributor)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            } else {
                s.trim().to_string()
            }
        }
        _ => String::new(),
    }
}

/// `user_books` entry → a contract [`Item`]. `None` when the entry has no id
/// or no usable `date_added` timestamp (can't partition).
fn item_from(ub: &Value) -> Option<Item> {
    let id = ub.get("id").and_then(|v| match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    })?;

    // ts = date_added (when the book joined the library).
    let raw_ts = str_field(ub, "date_added");
    if raw_ts.is_empty() {
        return None;
    }
    let ts = to_local(&raw_ts);
    // Must yield a month partition key; otherwise the row can't be filed.
    Partition::Month.key(&ts)?;

    let status_id = ub.get("status_id").and_then(Value::as_i64).unwrap_or(0);
    let state = status_to_state(status_id).to_string();

    let book = ub.get("book").unwrap_or(&Value::Null);
    let title = str_field(book, "title");
    let author = author_from_book(book);

    // read_at = last_read_date, if present.
    let read_at_raw = str_field(ub, "last_read_date");
    let read_at =
        if read_at_raw.is_empty() { String::new() } else { to_local(&read_at_raw) };

    // Progress: percentage of total pages read.
    // A finished book (status_id=3) is always 100%; otherwise compute from the
    // most recent user_book_read's progress_pages vs book.pages (both > 0).
    let progress = if status_id == 3 {
        Some(100i64)
    } else {
        let total_pages = book.get("pages").and_then(Value::as_i64).unwrap_or(0);
        let read_pages = ub
            .get("user_book_reads")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .and_then(|r| r.get("progress_pages"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if total_pages > 0 && read_pages > 0 {
            Some(((read_pages as f64 / total_pages as f64) * 100.0).round() as i64)
        } else {
            None
        }
    };

    let mut extra = Map::new();
    put_str(&mut extra, "status_id", &status_id.to_string());
    if let Some(r) = ub.get("rating") {
        if !r.is_null() {
            extra.insert("rating".into(), r.clone());
        }
    }
    if !read_at_raw.is_empty() {
        put_str(&mut extra, "last_read_date", &read_at_raw);
    }
    if let Some(rc) = ub.get("read_count").and_then(Value::as_i64) {
        extra.insert("read_count".into(), Value::from(rc));
    }
    if let Some(o) = ub.get("owned").and_then(Value::as_bool) {
        extra.insert("owned".into(), Value::Bool(o));
    }
    let book_id_s = book
        .get("id")
        .and_then(|v| match v {
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) => Some(s.trim().to_string()),
            _ => None,
        })
        .unwrap_or_default();
    put_str(&mut extra, "book_id", &book_id_s);
    put_str(&mut extra, "slug", &str_field(book, "slug"));
    if let Some(ry) = book.get("release_year").and_then(Value::as_i64) {
        extra.insert("release_year".into(), Value::from(ry));
    }
    if let Some(p) = book.get("pages").and_then(Value::as_i64) {
        extra.insert("pages".into(), Value::from(p));
    }

    Some(Item {
        ts,
        source: "hardcover".into(),
        guid: format!("hc-ub-{id}"),
        url: String::new(),
        title,
        author,
        site: String::new(),
        feed: String::new(),
        excerpt: String::new(),
        tags: Vec::new(),
        state,
        progress,
        read_at,
        extra,
    })
}

/// `user_books` entry with a review → a contract [`Highlight`]. `None` when
/// there is no review text or no `reviewed_at` timestamp.
fn highlight_from_review(ub: &Value) -> Option<Highlight> {
    let review = str_field(ub, "review");
    if review.is_empty() {
        return None;
    }
    let reviewed_at_raw = str_field(ub, "reviewed_at");
    if reviewed_at_raw.is_empty() {
        return None;
    }
    let ts = to_local(&reviewed_at_raw);
    Partition::Month.key(&ts)?;

    let id = ub.get("id").and_then(|v| match v {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        _ => None,
    })?;

    let book = ub.get("book").unwrap_or(&Value::Null);
    let title = str_field(book, "title");
    let author = author_from_book(book);

    let mut extra = Map::new();
    put_str(&mut extra, "reviewed_at", &reviewed_at_raw);
    if let Some(r) = ub.get("rating") {
        if !r.is_null() {
            extra.insert("rating".into(), r.clone());
        }
    }

    Some(Highlight {
        ts,
        source: "hardcover".into(),
        guid: format!("hc-review-{id}"),
        text: review,
        note: String::new(),
        title,
        author,
        url: String::new(),
        location: String::new(),
        color: String::new(),
        tags: Vec::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write helpers (deduped, partitioned).

/// Append new items + raw rows, deduped by guid. Returns count written.
fn write_items(vault: &Vault, pairs: Vec<(Item, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids — guid dedupe prevents duplicates on re-pull.
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
    for (item, raw_val) in pairs {
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

/// Append new highlights (reviews), deduped by guid. Returns count written.
fn write_highlights(vault: &Vault, rows: Vec<Highlight>) -> Result<u64> {
    let stream = vault.stream(HIGHLIGHTS_DIR, Partition::Month);

    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<Highlight> = Vec::new();
    for hl in rows {
        if hl.guid.is_empty() || !seen.insert(hl.guid.clone()) {
            continue;
        }
        new_rows.push(hl);
    }

    stream.append(&new_rows, |h| &h.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Hardcover is not connected — add your API token in the Integrations tab")?;
    let client = HardcoverClient::new(API_BASE.to_string(), token.clone());
    pull_with(vault, &client, &token)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl HardcoverApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_hardcover_sync();

    let mut all_books: Vec<Value> = Vec::new();
    let mut offset: i64 = 0;

    // Drain all pages before writing — crash re-drains rather than advancing
    // the watermark on a partial fetch.
    loop {
        let page = match api.user_books(token, state.updated_at_watermark.as_deref(), offset) {
            Ok(p) => p,
            Err(FetchError::Unauthorized) => {
                bail!("Hardcover rejected the request — reconnect your API token")
            }
            Err(FetchError::RateLimited) => {
                // Back off once and retry; the watcher retries next tick anyway.
                std::thread::sleep(Duration::from_secs(2));
                match api.user_books(token, state.updated_at_watermark.as_deref(), offset) {
                    Ok(p) => p,
                    Err(e) => bail!("Hardcover rate limited: {e}"),
                }
            }
            Err(e) => bail!("Hardcover fetch failed: {e}"),
        };
        let has_more = page.next_offset.is_some();
        all_books.extend(page.items);
        match page.next_offset {
            Some(next) => offset = next,
            None => break,
        }
        // Courtesy delay between pages (~6 req/min, far below the 60/min cap).
        if has_more {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // Map books → contract rows + raw lines.
    let mut item_pairs: Vec<(Item, Value)> = Vec::new();
    let mut review_rows: Vec<Highlight> = Vec::new();
    let mut max_updated: Option<String> = None;

    for ub in &all_books {
        let upd = str_field(ub, "updated_at");
        if !upd.is_empty() {
            max_updated =
                Some(max_updated.map_or(upd.clone(), |cur: String| cur.max(upd)));
        }
        if let Some(item) = item_from(ub) {
            item_pairs.push((item, ub.clone()));
        }
        if let Some(hl) = highlight_from_review(ub) {
            review_rows.push(hl);
        }
    }

    let items_written = write_items(vault, item_pairs)?;
    let highlights_written = write_highlights(vault, review_rows)?;

    // Advance watermark only after a full successful drain.
    if let Some(new_wm) = max_updated {
        if state.updated_at_watermark.as_deref().map_or(true, |cur| new_wm.as_str() > cur) {
            state.updated_at_watermark = Some(new_wm);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_hardcover_sync(&state)?;

    Ok(PullOutcome {
        headline: format!(
            "{items_written} library entries, {highlights_written} reviews"
        ),
        counts: BTreeMap::from([
            ("items", items_written),
            ("highlights", highlights_written),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-hardcover-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A representative `user_books` response (offset=0): two books — one
    /// finished with a review and reading sessions, one want-to-read.
    /// Field names confirmed from hardcoverapp/hardcover-docs schema-fields.json
    /// and ThorbenWoelk/hardcover.cli client.rs query strings.
    fn sample_response(offset: i64) -> Page {
        if offset > 0 {
            return Page { items: vec![], next_offset: None };
        }
        Page {
            items: vec![
                serde_json::json!({
                    "id": 42,
                    "status_id": 3,
                    "rating": 4.5,
                    "review": "A profound meditation on memory and loss.",
                    "reviewed_at": "2024-03-15T10:00:00+00:00",
                    "date_added": "2023-11-01",
                    "last_read_date": "2024-03-10",
                    "updated_at": "2024-03-15T10:00:00+00:00",
                    "read_count": 1,
                    "owned": true,
                    "user_book_reads": [
                        {
                            "id": 101,
                            "started_at": "2023-11-05",
                            "finished_at": "2024-03-10",
                            "progress_pages": 320,
                            "progress_seconds": null
                        }
                    ],
                    "book": {
                        "id": 9001,
                        "title": "The Remains of the Day",
                        "slug": "the-remains-of-the-day",
                        "pages": 320,
                        "release_year": 1989,
                        "cached_contributors": [
                            {"author": {"name": "Kazuo Ishiguro"}, "contribution": "Author"}
                        ]
                    }
                }),
                serde_json::json!({
                    "id": 77,
                    "status_id": 1,
                    "rating": null,
                    "review": "",
                    "reviewed_at": null,
                    "date_added": "2024-06-01",
                    "last_read_date": null,
                    "updated_at": "2024-06-01T08:00:00+00:00",
                    "read_count": 0,
                    "owned": false,
                    "user_book_reads": [],
                    "book": {
                        "id": 9002,
                        "title": "Piranesi",
                        "slug": "piranesi",
                        "pages": 272,
                        "release_year": 2020,
                        "cached_contributors": [
                            {"author": {"name": "Susanna Clarke"}, "contribution": "Author"}
                        ]
                    }
                }),
            ],
            next_offset: None,
        }
    }

    struct StubApi {
        response: fn(i64) -> Page,
    }

    impl HardcoverApi for StubApi {
        fn probe(&self, _token: &str) -> Result<(), FetchError> {
            Ok(())
        }
        fn user_books(
            &self,
            _token: &str,
            _updated_at_after: Option<&str>,
            offset: i64,
        ) -> Result<Page, FetchError> {
            Ok((self.response)(offset))
        }
    }

    #[test]
    fn item_from_finished_book() {
        let ub = &sample_response(0).items[0];
        let item = item_from(ub).expect("should map");
        assert_eq!(item.source, "hardcover");
        assert_eq!(item.guid, "hc-ub-42");
        assert_eq!(item.title, "The Remains of the Day");
        assert_eq!(item.author, "Kazuo Ishiguro");
        assert_eq!(item.state, "read");
        // Finished book → progress = 100.
        assert_eq!(item.progress, Some(100));
        // ts = date_added 2023-11-01 → valid RFC3339.
        DateTime::parse_from_rfc3339(&item.ts).expect("ts must be valid RFC3339");
        // read_at from last_read_date.
        assert!(!item.read_at.is_empty());
        // extra carries status_id, pages, book_id, rating, owned.
        assert_eq!(item.extra.get("status_id"), Some(&Value::String("3".into())));
        assert_eq!(item.extra.get("pages"), Some(&Value::Number(320.into())));
        assert_eq!(item.extra.get("book_id"), Some(&Value::String("9001".into())));
        assert_eq!(item.extra.get("owned"), Some(&Value::Bool(true)));
    }

    #[test]
    fn item_from_want_to_read() {
        let ub = &sample_response(0).items[1];
        let item = item_from(ub).expect("should map");
        assert_eq!(item.guid, "hc-ub-77");
        assert_eq!(item.title, "Piranesi");
        assert_eq!(item.author, "Susanna Clarke");
        assert_eq!(item.state, "saved"); // status_id=1 → saved
        assert_eq!(item.progress, None); // no pages read, not finished
        assert!(item.read_at.is_empty());
    }

    #[test]
    fn highlight_review_maps_correctly() {
        let ub = &sample_response(0).items[0];
        let hl = highlight_from_review(ub).expect("should produce review highlight");
        assert_eq!(hl.source, "hardcover");
        assert_eq!(hl.guid, "hc-review-42");
        assert_eq!(hl.text, "A profound meditation on memory and loss.");
        assert_eq!(hl.title, "The Remains of the Day");
        assert_eq!(hl.author, "Kazuo Ishiguro");
        // ts = reviewed_at 2024-03-15T10:00:00+00:00 → valid RFC3339.
        DateTime::parse_from_rfc3339(&hl.ts).expect("highlight ts must be valid RFC3339");
        assert!(hl.extra.get("rating").is_some());
    }

    #[test]
    fn no_review_yields_no_highlight() {
        let ub = &sample_response(0).items[1]; // want-to-read, no review
        assert!(highlight_from_review(ub).is_none());
    }

    #[test]
    fn full_pull_writes_partitioned_layers_and_advances_watermark() {
        let v = temp_vault("store");
        let api = StubApi { response: sample_response };

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("items"), Some(&2));
        assert_eq!(out.counts.get("highlights"), Some(&1));

        // The exact partition month depends on local timezone (bare dates are
        // treated as midnight UTC), so we scan all JSONL files in the dir.
        let contract_dir = v.root().join("reading/hardcover");
        let all_items: String = std::fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert!(all_items.contains("hc-ub-42"), "book 42 present");
        assert!(all_items.contains("hc-ub-77"), "book 77 present");
        assert_eq!(
            all_items.lines().count(),
            2,
            "exactly two item lines total"
        );

        // Raw under raw/ — verbatim API objects.
        let raw_dir = v.root().join("reading/hardcover/raw");
        let all_raw: String = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert!(all_raw.contains("the-remains-of-the-day"), "raw is verbatim API object");
        assert_eq!(all_raw.lines().count(), 2, "two raw lines total");

        // Review highlight: reviewed_at is an RFC3339 timestamp so it
        // partitions exactly to 2024-03 regardless of timezone.
        let hl_file = v.root().join("reading/hardcover/highlights/2024-03.jsonl");
        let hl = std::fs::read_to_string(&hl_file).unwrap();
        assert_eq!(hl.lines().count(), 1);
        assert!(hl.contains("hc-review-42"));

        // Watermark advanced to the max updated_at seen.
        let state = v.read_hardcover_sync();
        assert!(state.updated_at_watermark.is_some());
        assert!(state.updated.is_some());

        // Re-run with same data → all guids already stored, no new rows.
        let again = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(again.counts.get("items"), Some(&0));
        assert_eq!(again.counts.get("highlights"), Some(&0));

        // Item files unchanged on re-run (idempotent).
        let all_items2: String = std::fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
            .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
            .collect();
        assert_eq!(all_items, all_items2, "idempotent re-run leaves contract files unchanged");
    }

    #[test]
    fn to_local_handles_bare_date_and_rfc3339() {
        let bare = to_local("2024-03-10");
        DateTime::parse_from_rfc3339(&bare).expect("bare date should yield valid RFC3339");
        let rfc = to_local("2024-03-15T10:00:00+00:00");
        DateTime::parse_from_rfc3339(&rfc).expect("RFC3339 round-trip");
    }

    #[test]
    fn author_from_array_and_string_encoding() {
        // Real API shape: nested author.name — the primary path.
        let book_real = serde_json::json!({
            "cached_contributors": [
                {"author": {"name": "Ursula K. Le Guin"}, "contribution": "Author"}
            ]
        });
        assert_eq!(author_from_book(&book_real), "Ursula K. Le Guin");

        // Flat fallback: {name: "..."} — retained for client-flattened variants.
        let book_flat = serde_json::json!({
            "cached_contributors": [{"name": "Ursula K. Le Guin"}]
        });
        assert_eq!(author_from_book(&book_flat), "Ursula K. Le Guin");

        // JSON-string encoding, real nested shape.
        let book_str = serde_json::json!({
            "cached_contributors": "[{\"author\":{\"name\":\"Octavia Butler\"},\"contribution\":\"Author\"}]"
        });
        assert_eq!(author_from_book(&book_str), "Octavia Butler");

        // JSON-string encoding, flat fallback.
        let book_str_flat = serde_json::json!({
            "cached_contributors": "[{\"name\":\"Octavia Butler\"}]"
        });
        assert_eq!(author_from_book(&book_str_flat), "Octavia Butler");

        let book_empty = serde_json::json!({});
        assert_eq!(author_from_book(&book_empty), "");
    }

    #[test]
    fn to_local_handles_naive_datetime() {
        // Naive datetime (no offset) — Hasura serialises reviewed_at as timestamp.
        let naive = to_local("2024-03-15T10:00:00");
        DateTime::parse_from_rfc3339(&naive).expect("naive datetime should yield valid RFC3339");

        // With fractional seconds.
        let naive_frac = to_local("2024-03-15T10:00:00.123456");
        DateTime::parse_from_rfc3339(&naive_frac).expect("naive fractional should yield RFC3339");

        // The month prefix (first 7 chars) must still be parseable for partitioning.
        assert_eq!(&naive[..7], "2024-03");
    }

    #[test]
    fn status_mappings() {
        assert_eq!(status_to_state(1), "saved");    // Want to Read
        assert_eq!(status_to_state(2), "saved");    // Currently Reading
        assert_eq!(status_to_state(3), "read");     // Read
        assert_eq!(status_to_state(4), "saved");    // Paused
        assert_eq!(status_to_state(5), "archived"); // DNF
        assert_eq!(status_to_state(6), "archived"); // Ignored
    }

    #[test]
    fn connection_lifecycle_without_network() {
        assert!(CONNECTION.method("token-paste").is_some());
        let v = temp_vault("conn");
        // Bypass the network probe by writing the token directly.
        let token_set = TokenSet {
            access_token: "test-tok".into(),
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: None,
            expires_at: None,
        };
        v.save_sync_token(SERVICE, &token_set).unwrap();
        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "hardcover");
        def_disconnect(&v, "hardcover").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn pull_without_token_errors_clearly() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }
}
