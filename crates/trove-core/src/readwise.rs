//! Readwise + Readwise Reader — the reading hub, pulled into the bound
//! [`crate::reading`] contract. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/readwise.md. **First collector in the `reading` domain** —
//! this build binds the contract (see `crate::reading` / `crate::contracts`).
//!
//! A **Periodic** cloud pull over two endpoints, one account token:
//!
//! - `GET https://readwise.io/api/v2/export/` → books + their nested
//!   highlights. Each highlight becomes a [`crate::reading::Highlight`] under
//!   `reading/readwise/highlights/YYYY-MM.jsonl` (`guid` = the Readwise
//!   highlight id; `ts` = `highlighted_at`, ISO → local; book `title`/`author`
//!   carried inline; `category`/`highlighted_at`/source ids into `extra`).
//! - `GET https://readwise.io/api/v3/list/` → Reader documents. Each becomes a
//!   [`crate::reading::Item`] under `reading/readwise/YYYY-MM.jsonl` (`guid` =
//!   the document id; `ts` = `saved_at`, ISO → local; `reading_progress` 0..1 →
//!   an **integer percent 0–100**; `location` → coarse `state`).
//!
//! Two layers per endpoint: the **raw** API object verbatim under
//! `reading/readwise/raw/YYYY-MM.jsonl` (full fidelity, unconditional), and the
//! normalized **contract** rows, deduped by `guid`.
//!
//! Both endpoints take an `updatedAfter` ISO cursor; we persist a watermark per
//! endpoint in `.trove/readwise-sync.json` (non-secret, rebuildable) and only
//! advance it after a full drain, so a crash re-drains rather than skips. Both
//! paginate with `nextPageCursor` (request param `pageCursor`); we follow the
//! cursor until it's null. Rate limit on both is 20/min — fine for an
//! incremental personal pull.
//!
//! ## Token-count ambiguity (resolved)
//!
//! The brief flagged that the research notes disagree on whether Reader (v3)
//! needs a *separate* token from Readwise (v2). Both endpoints document the same
//! `Authorization: Token XXX` scheme against the same account, so we try the one
//! pasted token for both. If v3 rejects it (401), we fall back to a second token
//! — `TROVE_READWISE_READER_TOKEN` (env → baked, empty default) — for the Reader
//! leg only; if that's absent the highlights leg still succeeds and the Reader
//! leg is a clean skip. The single pasted token is the only connect field.
//!
//! Auth is a secret: pasted via [`ConnectMethod::TokenPaste`], stored under
//! `.trove/sync/` (0600), verified at connect with a real `GET /api/v2/auth/`,
//! and never logged or written to the cursor or any non-secret file.

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

/// Contract-layer item stream; highlights nest under `highlights/`, raw under
/// `raw/`.
const DIR: &str = "reading/readwise";
const HIGHLIGHTS_DIR: &str = "reading/readwise/highlights";
const RAW_DIR: &str = "reading/readwise/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-asks both endpoints from the beginning.
const SYNC_FILE: &str = ".trove/readwise-sync.json";

/// Service id under `.trove/sync/` where the pasted token is stored (the
/// GitHub/Todoist-PAT slot: the token rides a never-expiring [`TokenSet`]).
const SERVICE: &str = "readwise";

/// Compiled-in *Reader* token default, used only if the primary token is
/// rejected by v3. Empty by default — set `TROVE_READWISE_READER_TOKEN` to bake
/// one in, or rely on the single pasted token serving both endpoints.
const BAKED_READER_TOKEN: &str = "";

const API_BASE: &str = "https://readwise.io";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs in the watcher loop. Hourly: highlights/saves trickle
/// in and the incremental `updatedAfter` poll is cheap when idle.
pub const READWISE_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Newest stem across both the item stream and the highlights stream.
    let items = crate::registry::newest_stem(&vault.root().join(DIR));
    let highlights = crate::registry::newest_stem(&vault.root().join(HIGHLIGHTS_DIR));
    [items, highlights].into_iter().flatten().max()
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing token or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("readwise synced — {} highlights, {} documents", c("highlights"), c("items"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("readwise sync skipped: {e}"))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!(
            "Readwise synced — {} highlights, {} Reader documents",
            c("highlights"),
            c("items")
        ),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "readwise",
        name: "Readwise + Readwise Reader",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your highlights (Readwise: Kindle, Apple Books, web articles) and \
                      saved articles (Readwise Reader) into the unified reading store via the \
                      official API. First sync backfills everything; later syncs fetch only \
                      what changed.",
        domain: "reading",
        vault_path: "reading/readwise/",
        toggleable: true,
        setup: &[
            "Connect with your Readwise access token on this card.",
            "First sync backfills all highlights and Reader documents; later syncs are incremental.",
        ],
        caveats: "One token usually covers both Readwise and Reader; if Reader rejects it, set a \
                  second token via TROVE_READWISE_READER_TOKEN (the highlights sync still works \
                  without it). The export and list endpoints are rate-limited to 20 requests per \
                  minute, so a very large library backfills over a few minutes.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(READWISE_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("readwise"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a Readwise access token, a SECRET).

/// Verify the pasted token with `GET /api/v2/auth/` (a 204 on success), then
/// store it (0600). A 401 bails with a clear message; the token is never logged.
fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("empty token — paste your Readwise access token from readwise.io/access_token");
    }
    let client = ReadwiseClient::new(API_BASE.to_string(), token.to_string());
    match client.verify() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Readwise rejected the token (401) — copy it fresh from readwise.io/access_token"
        ),
        Err(e) => bail!("Readwise auth check failed: {e}"),
    }
    // The token goes ONLY through the secret store (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
            refresh_token: None,
            token_type: Some("Token".into()),
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored token. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the token is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Readwise".to_string(),
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // the access token doesn't expire
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: a personal access token is self-service.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// access token. One token serves both endpoints; the optional Reader fallback
/// is provisioned out-of-band via `TROVE_READWISE_READER_TOKEN`, not a connect
/// field.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "readwise",
    display_name: "Readwise + Readwise Reader",
    methods: &[ConnectMethod::TokenPaste {
        label: "Readwise access token",
        help: "Paste your Readwise access token from readwise.io/access_token — it covers both \
               Readwise highlights and Reader documents and is stored locally, never sent anywhere \
               but Readwise.",
        placeholder: "abcd1234efgh5678…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["readwise"],
    setup: &[
        "Open readwise.io/access_token while signed in to Readwise.",
        "Copy your access token.",
        "Paste it here — it's stored locally and used for both Readwise and Reader.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page: the items array plus the cursor for the next page
/// (`nextPageCursor`, null on the last page).
struct Page {
    items: Vec<Value>,
    next_cursor: Option<String>,
}

/// Status-level fetch errors: 401 wants distinct handling (so the v3 leg can
/// try a fallback token / skip cleanly), 429 is transient, everything else is a
/// message.
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

/// The endpoints the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network. `token` lets a single client serve
/// the primary and the Reader-fallback token.
trait ReadwiseApi {
    /// One page of `GET /api/v2/export/`. `updated_after` filters by the ISO
    /// cursor; `cursor` (the prior `nextPageCursor`) fetches the next page.
    fn export_page(
        &self,
        token: &str,
        updated_after: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page, FetchError>;

    /// One page of `GET /api/v3/list/`. Same parameters.
    fn list_page(
        &self,
        token: &str,
        updated_after: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page, FetchError>;
}

/// Thin client; base URL injected (the github/oura/lastfm/todoist pattern). The
/// primary token rides on the client for `verify`; per-call tokens drive the
/// two-token fallback.
struct ReadwiseClient {
    base: String,
    token: String,
}

impl ReadwiseClient {
    fn new(base: String, token: String) -> Self {
        ReadwiseClient { base, token }
    }

    /// `GET /api/v2/auth/` → 204 when the token is valid. Used at connect.
    fn verify(&self) -> Result<(), FetchError> {
        let url = format!("{}/api/v2/auth/", self.base);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Token {}", self.token))
            .call()
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!("HTTP {code}: {}", body.chars().take(200).collect::<String>())))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }

    /// Shared GET → one page, reading `results` + `nextPageCursor` defensively.
    fn get_page(
        &self,
        path: &str,
        token: &str,
        updated_after: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page, FetchError> {
        let mut req = ureq::get(&format!("{}{path}", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Token {token}"));
        if let Some(ua) = updated_after {
            req = req.query("updatedAfter", ua);
        }
        if let Some(c) = cursor {
            req = req.query("pageCursor", c);
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(parse_page(v))
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!("HTTP {code}: {}", body.chars().take(300).collect::<String>())))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl ReadwiseApi for ReadwiseClient {
    fn export_page(
        &self,
        token: &str,
        updated_after: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page, FetchError> {
        self.get_page("/api/v2/export/", token, updated_after, cursor)
    }

    fn list_page(
        &self,
        token: &str,
        updated_after: Option<&str>,
        cursor: Option<&str>,
    ) -> Result<Page, FetchError> {
        self.get_page("/api/v3/list/", token, updated_after, cursor)
    }
}

/// Pull `results` + `nextPageCursor` out of a Readwise list response. Both
/// endpoints wrap items under `results` with a top-level `nextPageCursor`
/// (null on the last page). A bare array is tolerated as a single un-paged page.
fn parse_page(v: Value) -> Page {
    match v {
        Value::Object(o) => {
            let items = o.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
            let next_cursor = o
                .get("nextPageCursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            Page { items, next_cursor }
        }
        Value::Array(a) => Page { items: a, next_cursor: None },
        _ => Page { items: Vec::new(), next_cursor: None },
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max `updated_at` seen across exported highlights' parent books — the
    /// `updatedAfter` lower bound for the next export poll. RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    highlights_updated_after: Option<String>,
    /// Max `updated_at` seen across Reader documents — the `updatedAfter` lower
    /// bound for the next list poll. RFC3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    items_updated_after: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_readwise_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_readwise_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity API object). The on-disk line is the verbatim
// API object (flattened — no synthetic columns), tagged with the contract ts
// purely so the month-partition writer files it under the right month. Only
// `value` is serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable/empty values pass
/// through verbatim rather than being dropped (the github/todoist idiom). The
/// caller only feeds this non-empty, partitionable values.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// First non-empty among the given keys, as a trimmed string.
fn first_time(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .map(|k| str_field(v, k))
        .find(|s| !s.is_empty())
}

/// Insert `k`→`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// A Readwise highlight (nested in a v2 export book) → a contract
/// [`Highlight`]. `book` supplies the inline parent title/author. `None` when
/// the highlight has no id (can't dedup) or no usable timestamp (can't
/// partition).
fn highlight_from(h: &Value, book: &Value) -> Option<Highlight> {
    let id = h.get("id").and_then(value_id)?;
    // ts = the most identity-bearing time: highlighted_at, else created/updated.
    let raw_ts = first_time(h, &["highlighted_at", "created_at", "updated_at"])?;
    // Must yield a month partition; otherwise the row can't be filed.
    Partition::Month.key(&to_local(&raw_ts))?;
    let ts = to_local(&raw_ts);

    let mut extra = Map::new();
    // Book category (books|articles|tweets|podcasts|supplementals) and
    // highlighted_at ride in extra per the contract's worked example, which
    // shows highlighted_at in *local* time (reading.md). Convert to match `ts`.
    put_str(&mut extra, "category", &str_field(book, "category"));
    let highlighted_at = str_field(h, "highlighted_at");
    if !highlighted_at.is_empty() {
        put_str(&mut extra, "highlighted_at", &to_local(&highlighted_at));
    }
    put_str(&mut extra, "source_type", &str_field(book, "source"));
    put_str(&mut extra, "readwise_url", &str_field(h, "readwise_url"));
    // Stable source ids — useful for cross-referencing, not contract columns.
    if let Some(bid) = book.get("user_book_id").and_then(value_id) {
        extra.insert("user_book_id".into(), Value::String(bid));
    }
    if let Some(loc_type) = h.get("location_type").and_then(Value::as_str) {
        put_str(&mut extra, "location_type", loc_type);
    }
    if h.get("is_favorite").and_then(Value::as_bool) == Some(true) {
        extra.insert("is_favorite".into(), Value::Bool(true));
    }

    Some(Highlight {
        ts,
        source: "readwise".into(),
        guid: id,
        text: str_field(h, "text"),
        note: str_field(h, "note"),
        // Prefer the human title; fall back to the API's readable_title.
        title: first_time(book, &["title", "readable_title"]).unwrap_or_default(),
        author: str_field(book, "author"),
        // A web highlight carries its own page url; books carry none.
        url: str_field(h, "url"),
        location: location_str(h),
        color: str_field(h, "color"),
        tags: tag_names(h.get("tags")),
        extra,
    })
}

/// True when a v3 list document is a Reader **highlight or note** — i.e. a child
/// document, not a saved item. Per the Reader API, highlights and notes are
/// returned as documents with a non-null `parent_id` (the id of the parent
/// article/book/highlight) and `category` "highlight"/"note". We key off
/// `parent_id` (the authoritative signal) and also catch the categories
/// defensively, so neither pollutes the saved-and-read item stream.
fn is_reader_child(d: &Value) -> bool {
    let has_parent = !str_field(d, "parent_id").is_empty();
    let cat = str_field(d, "category");
    has_parent || cat == "highlight" || cat == "note"
}

/// A Reader document (v3 list) → a contract [`Item`]. `None` when the document
/// has no id (can't dedup) or no usable timestamp (can't partition).
fn item_from(d: &Value) -> Option<Item> {
    let id = d.get("id").and_then(value_id)?;
    // ts = the most identity-bearing time: saved_at, else updated/created.
    let raw_ts = first_time(d, &["saved_at", "updated_at", "created_at"])?;
    Partition::Month.key(&to_local(&raw_ts))?;
    let ts = to_local(&raw_ts);

    // reading_progress is a 0..1 fraction → integer percent 0..100 (the
    // PHASE3-REVIEW carry-forward). Clamp defensively.
    let progress = d.get("reading_progress").and_then(Value::as_f64).map(|f| {
        let pct = (f * 100.0).round() as i64;
        pct.clamp(0, 100)
    });

    // location (new|later|shortlist|archive|feed) → coarse contract state. The
    // native value is preserved in extra.
    let location = str_field(d, "location");
    let state = match location.as_str() {
        "archive" => "archived",
        "shortlist" => "favorite",
        "new" | "later" | "feed" => "saved",
        _ => "",
    }
    .to_string();

    let read_at = d
        .get("last_opened_at")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(to_local)
        .unwrap_or_default();

    let mut extra = Map::new();
    put_str(&mut extra, "reader_location", &location);
    put_str(&mut extra, "category", &str_field(d, "category"));
    put_str(&mut extra, "reading_time", &str_field(d, "reading_time"));
    put_str(&mut extra, "summary", &str_field(d, "summary"));
    put_str(&mut extra, "notes", &str_field(d, "notes"));
    put_str(&mut extra, "published_date", &str_field(d, "published_date"));
    put_str(&mut extra, "image_url", &str_field(d, "image_url"));
    put_str(&mut extra, "parent_id", &str_field(d, "parent_id"));
    if let Some(wc) = d.get("word_count").and_then(Value::as_i64) {
        extra.insert("word_count".into(), Value::from(wc));
    }
    // source_url is the document's own Reader URL; url below is the saved page.
    put_str(&mut extra, "source_url", &str_field(d, "source_url"));

    Some(Item {
        ts,
        source: "readwise".into(),
        guid: id,
        url: str_field(d, "url"),
        title: str_field(d, "title"),
        author: str_field(d, "author"),
        site: str_field(d, "site_name"),
        feed: String::new(),
        // Reader has no separate excerpt; the summary rides in extra above.
        excerpt: String::new(),
        tags: tag_names(d.get("tags")),
        state,
        progress,
        read_at,
        extra,
    })
}

/// An id field that may be a JSON number or string → a `String`. Readwise ids
/// are integers in v2 and opaque strings in v3 — accept both.
fn value_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A highlight's location → a string. v2 `location` is an integer (Kindle
/// location / page); render it, optionally as a range with `end_location`.
fn location_str(h: &Value) -> String {
    let start = match h.get("location") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return String::new(),
    };
    match h.get("end_location") {
        Some(Value::Number(n)) => format!("{start}-{n}"),
        Some(Value::String(s)) if !s.trim().is_empty() && s.trim() != start => {
            format!("{start}-{}", s.trim())
        }
        _ => start,
    }
}

/// Readwise tags come two ways: v2 highlight tags are `[{id, name}]`; v3
/// document tags are an object keyed by tag name (`{"work": {...}}`). Normalize
/// either (or a plain `["a","b"]`) to a Vec of names.
fn tag_names(tags: Option<&Value>) -> Vec<String> {
    match tags {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|t| match t {
                Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
                Value::Object(_) => {
                    let n = str_field(t, "name");
                    (!n.is_empty()).then_some(n)
                }
                _ => None,
            })
            .collect(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| {
                // Prefer an inner `name`; fall back to the dict key.
                let n = str_field(v, "name");
                if n.is_empty() { k.trim().to_string() } else { n }
            })
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Max RFC3339-ish timestamp across a slice of values at `key` (string compare
/// is valid for the source's own same-offset UTC stamps). `None` when no value
/// has the key.
fn max_updated(values: &[Value], key: &str) -> Option<String> {
    values
        .iter()
        .filter_map(|v| {
            let s = str_field(v, key);
            (!s.is_empty()).then_some(s)
        })
        .max()
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid against what's already on disk.

/// Append new contract + raw rows for one endpoint, deduped by guid. Returns the
/// number of new contract rows written. Raw lines partition by the same month
/// as their contract row.
fn write_layer<T, F>(
    vault: &Vault,
    contract_dir: &str,
    rows: Vec<(T, Value)>,
    guid_of: F,
    ts_of: impl Fn(&T) -> &str,
) -> Result<u64>
where
    T: Serialize,
    F: Fn(&T) -> &str,
{
    let contract = vault.stream(contract_dir, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids in the contract stream — re-runnable: a re-pull of an
    // overlapping window never duplicates (the letterboxd/lastfm pattern).
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<T> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        let guid = guid_of(&row).to_string();
        if guid.is_empty() || !seen.insert(guid) {
            continue; // no id, or already stored
        }
        new_raws.push(RawLine { ts: ts_of(&row).to_string(), value: raw_val });
        new_rows.push(row);
    }

    contract.append(&new_rows, |r| ts_of(r))?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token and sync. Missing token ⇒ a quiet skip on the periodic
/// path (mirror todoist/lastfm), a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Readwise is not connected — add your access token in the Integrations tab")?;
    let client = ReadwiseClient::new(API_BASE.to_string(), token.clone());
    pull_with(vault, &client, &token)
}

/// The Reader-fallback token: env → baked, empty default. `None` when neither.
fn reader_fallback_token() -> Option<String> {
    if let Ok(t) = std::env::var("TROVE_READWISE_READER_TOKEN") {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    let baked = BAKED_READER_TOKEN.trim();
    (!baked.is_empty()).then(|| baked.to_string())
}

/// The pull body over an injected API + primary token — the testable seam.
fn pull_with(vault: &Vault, api: &impl ReadwiseApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_readwise_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- Readwise highlights (v2 export) ---------------------------------
    // Drain every page (follow nextPageCursor) before mapping/advancing — a
    // partial fetch must not move the watermark, so a crash re-drains.
    let books = drain(|cursor| api.export_page(token, state.highlights_updated_after.as_deref(), cursor))
        .map_err(|e| fetch_err("export", e))?;
    // Watermark candidate: the max highlight `updated_at` (falling back to
    // `highlighted_at`) across this drain. The export endpoint's `updatedAfter`
    // filters by the *highlight's* update time — the book object carries no
    // top-level `updated_at` — so the watermark must come from the highlights.
    let mut highlight_rows: Vec<(Highlight, Value)> = Vec::new();
    let mut hl_watermark: Option<String> = None;
    for book in &books {
        if let Some(Value::Array(hls)) = book.get("highlights") {
            for h in hls {
                let when = first_time(h, &["updated_at", "highlighted_at", "created_at"]);
                if let Some(w) = when {
                    hl_watermark = Some(hl_watermark.map_or(w.clone(), |cur| cur.max(w)));
                }
                if let Some(row) = highlight_from(h, book) {
                    highlight_rows.push((row, h.clone()));
                }
            }
        }
    }
    let hl_written = write_layer(vault, HIGHLIGHTS_DIR, highlight_rows, |h| &h.guid, |h| &h.ts)?;
    counts.insert("highlights", hl_written);

    // --- Reader documents (v3 list) --------------------------------------
    // Try the primary token; on 401 fall back to the Reader token if present,
    // else skip the Reader leg cleanly (highlights already succeeded).
    let docs_result = drain(|cursor| api.list_page(token, state.items_updated_after.as_deref(), cursor));
    let docs = match docs_result {
        Ok(docs) => Some(docs),
        Err(FetchError::Unauthorized) => match reader_fallback_token() {
            Some(reader) => Some(
                drain(|cursor| api.list_page(&reader, state.items_updated_after.as_deref(), cursor))
                    .map_err(|e| fetch_err("list", e))?,
            ),
            // No fallback token: the single token doesn't cover Reader. Skip the
            // leg rather than fail the whole pull — the highlights are stored.
            None => None,
        },
        Err(e) => return Err(fetch_err("list", e)),
    };

    let mut item_watermark = None;
    if let Some(docs) = &docs {
        // The watermark tracks the whole drain (incl. child docs we don't store
        // as items) so a re-poll never re-fetches annotations either.
        item_watermark = max_updated(docs, "updated_at");
        // `/api/v3/list/` returns the user's Reader highlights and notes as
        // documents too — they carry a non-null `parent_id` (the parent
        // article/book) and `category` "highlight"/"note". Those are NOT saved
        // items; mapping them into the item stream would pollute the
        // saved-and-read timeline. Skip every child doc: the saved-item stream
        // is top-level documents only (`parent_id` null). Reader highlights
        // already reach the highlight stream via the v2 `/export/` leg, which
        // exports the full Readwise library (Reader highlights sync into it).
        let item_rows: Vec<(Item, Value)> = docs
            .iter()
            .filter(|d| !is_reader_child(d))
            .filter_map(|d| item_from(d).map(|it| (it, d.clone())))
            .collect();
        let items_written = write_layer(vault, DIR, item_rows, |i| &i.guid, |i| &i.ts)?;
        counts.insert("items", items_written);
    } else {
        counts.insert("items", 0);
    }

    // Advance each watermark only after its full drain, and only forward.
    if let Some(w) = hl_watermark {
        if state.highlights_updated_after.as_deref().is_none_or(|cur| w.as_str() > cur) {
            state.highlights_updated_after = Some(w);
        }
    }
    if let Some(w) = item_watermark {
        if state.items_updated_after.as_deref().is_none_or(|cur| w.as_str() > cur) {
            state.items_updated_after = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_readwise_sync(&state)?;

    let h = counts.get("highlights").copied().unwrap_or(0);
    let i = counts.get("items").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("{h} highlights, {i} documents"),
        counts,
    })
}

/// Drain every page of one endpoint, following `nextPageCursor` until null.
/// `fetch(cursor)` performs one request; the whole drain fails (returning the
/// error) without partial-advancing any watermark — the caller advances only on
/// the complete result.
fn drain(
    mut fetch: impl FnMut(Option<&str>) -> Result<Page, FetchError>,
) -> Result<Vec<Value>, FetchError> {
    let mut items = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = fetch(cursor.as_deref())?;
        items.extend(page.items);
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    Ok(items)
}

/// Map a [`FetchError`] at the top of an endpoint into an anyhow error with a
/// clear reconnect message for 401.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Readwise rejected the token (401) on the {endpoint} endpoint — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Readwise rate limited the {endpoint} endpoint (429) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Readwise {endpoint} fetch failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-readwise-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the documented v2 export + v3 list shapes) ------------

    /// A v2 export book with two highlights: one with a note + tags + single
    /// location, one note-less with a location range. Modeled on the documented
    /// readwise.io/api_deets export shape.
    fn export_book() -> Value {
        json!({
            "user_book_id": 17506326,
            "title": "Gravity and Grace",
            "readable_title": "Gravity and Grace",
            "author": "Simone Weil",
            "source": "kindle",
            "cover_image_url": "https://images.example.com/cover.jpg",
            "unique_url": "",
            "category": "books",
            "document_note": "",
            "readwise_url": "https://readwise.io/bookreview/17506326",
            "source_url": null,
            "asin": "B000FC1JAI",
            "highlights": [
                {
                    "id": 884412,
                    "text": "Attention is the rarest and purest form of generosity.",
                    "location": 142,
                    "location_type": "location",
                    "note": "cf. Weil on prayer",
                    "color": "yellow",
                    "highlighted_at": "2026-06-08T20:11:00+00:00",
                    "created_at": "2026-06-08T20:12:00+00:00",
                    "updated_at": "2026-06-08T20:12:00+00:00",
                    "end_location": null,
                    "url": null,
                    "book_id": 17506326,
                    "tags": [{"id": 1, "name": "attention"}, {"id": 2, "name": "ethics"}],
                    "is_favorite": true,
                    "is_discard": false,
                    "readwise_url": "https://readwise.io/open/884412"
                },
                {
                    "id": 884413,
                    "text": "We are what we repeatedly do.",
                    "location": 1099,
                    "location_type": "location",
                    "note": "",
                    "color": "",
                    "highlighted_at": "2026-05-30T09:05:00+00:00",
                    "end_location": 1101,
                    "url": null,
                    "book_id": 17506326,
                    "tags": [],
                    "is_favorite": false,
                    "is_discard": false,
                    "readwise_url": "https://readwise.io/open/884413"
                }
            ]
        })
    }

    /// A v3 Reader document: an archived article, 63% read, with a tag dict.
    fn reader_doc() -> Value {
        json!({
            "id": "01gm6kjzabcd609yepjrmcgz8a",
            "url": "https://example.com/a-deep-dive",
            "source_url": "https://read.readwise.io/read/01gm6kjz",
            "title": "A Deep Dive into Local-First Software",
            "author": "Jane Roe",
            "source": "Reader add from import",
            "category": "article",
            "location": "archive",
            "tags": {"software": {"name": "software", "type": "manual"}, "local-first": {"name": "local-first", "type": "manual"}},
            "site_name": "example.com",
            "word_count": 4200,
            "reading_time": "17 mins",
            "created_at": "2026-06-09T10:00:00+00:00",
            "updated_at": "2026-06-10T18:00:00+00:00",
            "published_date": "2026-06-01",
            "summary": "Why files beat databases for personal data.",
            "image_url": "https://example.com/cover.jpg",
            "notes": "great read",
            "parent_id": null,
            "reading_progress": 0.63,
            "first_opened_at": "2026-06-09T10:05:00+00:00",
            "last_opened_at": "2026-06-10T17:30:00+00:00",
            "saved_at": "2026-06-09T09:59:00+00:00",
            "last_moved_at": "2026-06-10T18:00:00+00:00"
        })
    }

    /// A v3 Reader **highlight** document — a *child* of `reader_doc()`. The
    /// Reader API returns highlights and notes as documents with a non-null
    /// `parent_id` and `category` "highlight"/"note". These are NOT saved items
    /// and must never reach the item stream.
    fn reader_highlight_doc() -> Value {
        json!({
            "id": "01gm6kjzhhhh609yepjrmcgz8z",
            "parent_id": "01gm6kjzabcd609yepjrmcgz8a",
            "category": "highlight",
            "location": "archive",
            "content": "Why files beat databases for personal data.",
            "title": null,
            "author": "Jane Roe",
            "site_name": "example.com",
            "tags": {},
            "reading_progress": 0.0,
            "created_at": "2026-06-10T17:31:00+00:00",
            "updated_at": "2026-06-10T17:31:00+00:00",
            "saved_at": "2026-06-10T17:31:00+00:00",
            "last_moved_at": "2026-06-10T17:31:00+00:00"
        })
    }

    // --- pure mapping tests ----------------------------------------------

    #[test]
    fn maps_highlight_with_note_tags_location_and_inline_parent() {
        let book = export_book();
        let h = &book["highlights"][0];
        let hl = highlight_from(h, &book).unwrap();
        assert_eq!(hl.source, "readwise");
        assert_eq!(hl.guid, "884412", "guid is the highlight id (stable)");
        assert_eq!(hl.text, "Attention is the rarest and purest form of generosity.");
        assert_eq!(hl.note, "cf. Weil on prayer");
        assert_eq!(hl.title, "Gravity and Grace", "parent title inline");
        assert_eq!(hl.author, "Simone Weil", "parent author inline");
        assert_eq!(hl.location, "142", "single location, no range");
        assert_eq!(hl.color, "yellow");
        assert_eq!(hl.tags, vec!["attention", "ethics"], "[{{id,name}}] → names");
        // ts = highlighted_at, converted to local (same instant as the UTC).
        assert_eq!(
            DateTime::parse_from_rfc3339(&hl.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-08T20:11:00+00:00").unwrap().timestamp(),
        );
        assert_eq!(hl.extra.get("category"), Some(&json!("books")));
        // highlighted_at is stored in local time (matching `ts` and the spec
        // example), so assert the instant, not a fixed offset.
        let ha = hl.extra.get("highlighted_at").and_then(Value::as_str).unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(ha).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-08T20:11:00+00:00").unwrap().timestamp(),
            "highlighted_at carries the same instant as the source, in local tz",
        );
        assert_eq!(hl.extra.get("user_book_id"), Some(&json!("17506326")));
        assert_eq!(hl.extra.get("is_favorite"), Some(&json!(true)));
    }

    #[test]
    fn maps_noteless_highlight_with_location_range() {
        let book = export_book();
        let h = &book["highlights"][1];
        let hl = highlight_from(h, &book).unwrap();
        assert_eq!(hl.guid, "884413");
        assert!(hl.note.is_empty(), "note-less highlight omits note");
        assert!(hl.color.is_empty(), "empty color omitted");
        assert_eq!(hl.location, "1099-1101", "location..end_location → a range");
        assert!(hl.tags.is_empty());
        assert!(hl.extra.get("is_favorite").is_none(), "false favorite not stored");
        // Omit-empty: re-serialized form drops note/color/url/is_favorite.
        let re = serde_json::to_value(&hl).unwrap();
        assert!(re.get("note").is_none() && re.get("color").is_none() && re.get("url").is_none());
    }

    #[test]
    fn maps_reader_doc_progress_fraction_to_integer_percent_and_state() {
        let d = reader_doc();
        let it = item_from(&d).unwrap();
        assert_eq!(it.source, "readwise");
        assert_eq!(it.guid, "01gm6kjzabcd609yepjrmcgz8a", "guid is the document id");
        assert_eq!(it.url, "https://example.com/a-deep-dive");
        assert_eq!(it.title, "A Deep Dive into Local-First Software");
        assert_eq!(it.author, "Jane Roe");
        assert_eq!(it.site, "example.com");
        // reading_progress 0.63 → 63 (integer percent, the carry-forward).
        assert_eq!(it.progress, Some(63), "0..1 fraction scaled to 0..100 integer percent");
        // location "archive" → state "archived"; native value preserved.
        assert_eq!(it.state, "archived");
        assert_eq!(it.extra.get("reader_location"), Some(&json!("archive")));
        assert_eq!(it.extra.get("word_count"), Some(&json!(4200)));
        assert_eq!(it.extra.get("reading_time"), Some(&json!("17 mins")));
        // tag dict → names (order not guaranteed from a map).
        let mut tags = it.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["local-first", "software"]);
        // ts = saved_at (local).
        assert_eq!(
            DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-09T09:59:00+00:00").unwrap().timestamp(),
        );
        // read_at = last_opened_at (local).
        assert_eq!(
            DateTime::parse_from_rfc3339(&it.read_at).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T17:30:00+00:00").unwrap().timestamp(),
        );
    }

    #[test]
    fn progress_clamps_and_states_map() {
        let mk = |loc: &str, prog: f64| {
            let mut d = reader_doc();
            d["location"] = json!(loc);
            d["reading_progress"] = json!(prog);
            item_from(&d).unwrap()
        };
        assert_eq!(mk("new", 0.0).state, "saved");
        assert_eq!(mk("later", 0.5).state, "saved");
        assert_eq!(mk("feed", 0.0).state, "saved");
        assert_eq!(mk("shortlist", 1.0).state, "favorite");
        assert_eq!(mk("archive", 1.0).progress, Some(100));
        // Out-of-range fraction clamps to the 0..100 band.
        assert_eq!(mk("new", 1.2).progress, Some(100));
        assert_eq!(mk("new", -0.1).progress, Some(0));
    }

    #[test]
    fn tag_names_handles_object_array_and_strings() {
        assert_eq!(tag_names(Some(&json!([{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]))), vec!["a", "b"]);
        assert_eq!(tag_names(Some(&json!(["x", "y"]))), vec!["x", "y"]);
        let mut dict = tag_names(Some(&json!({"k1": {"name": "n1"}, "k2": {}})));
        dict.sort();
        assert_eq!(dict, vec!["k2", "n1"], "inner name wins; dict key fallback");
        assert!(tag_names(Some(&json!({}))).is_empty());
        assert!(tag_names(None).is_empty());
    }

    #[test]
    fn reader_child_docs_are_not_saved_items() {
        // Top-level article (parent_id null, category "article") → an item.
        assert!(!is_reader_child(&reader_doc()), "a saved article is not a child");
        // A highlight doc (non-null parent_id, category "highlight") → excluded.
        assert!(is_reader_child(&reader_highlight_doc()), "highlight is a child doc");
        // A note doc (caught by category even if parent_id were absent).
        let note = json!({"id": "n1", "category": "note", "parent_id": "p1"});
        assert!(is_reader_child(&note));
        // The parent_id signal alone is authoritative (category-agnostic).
        let orphan = json!({"id": "x", "parent_id": "p9"});
        assert!(is_reader_child(&orphan), "non-null parent_id ⇒ child");
        // An explicit null parent_id with a normal category is a top-level item.
        let article = json!({"id": "a", "parent_id": null, "category": "pdf"});
        assert!(!is_reader_child(&article));
    }

    // --- a scripted mock API ---------------------------------------------

    struct MockApi {
        export_pages: RefCell<VecDeque<Result<Page, FetchError>>>,
        list_pages: RefCell<VecDeque<Result<Page, FetchError>>>,
        list_pages_fallback: RefCell<VecDeque<Result<Page, FetchError>>>,
        export_tokens: RefCell<Vec<String>>,
        list_tokens: RefCell<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                export_pages: RefCell::new(VecDeque::new()),
                list_pages: RefCell::new(VecDeque::new()),
                list_pages_fallback: RefCell::new(VecDeque::new()),
                export_tokens: RefCell::new(Vec::new()),
                list_tokens: RefCell::new(Vec::new()),
            }
        }
        fn export(self, results: Vec<Value>) -> Self {
            self.export_pages.borrow_mut().push_back(Ok(Page { items: results, next_cursor: None }));
            self
        }
        fn list(self, results: Vec<Value>) -> Self {
            self.list_pages.borrow_mut().push_back(Ok(Page { items: results, next_cursor: None }));
            self
        }
    }

    impl ReadwiseApi for MockApi {
        fn export_page(&self, token: &str, _ua: Option<&str>, _cursor: Option<&str>) -> Result<Page, FetchError> {
            self.export_tokens.borrow_mut().push(token.to_string());
            self.export_pages.borrow_mut().pop_front().unwrap_or(Ok(Page { items: vec![], next_cursor: None }))
        }
        fn list_page(&self, token: &str, _ua: Option<&str>, _cursor: Option<&str>) -> Result<Page, FetchError> {
            self.list_tokens.borrow_mut().push(token.to_string());
            // A non-primary token drains the fallback queue.
            let queue = if token == "primary-tok" { &self.list_pages } else { &self.list_pages_fallback };
            queue.borrow_mut().pop_front().unwrap_or(Ok(Page { items: vec![], next_cursor: None }))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_dedupes_and_advances_two_watermarks() {
        let v = temp_vault("fullpull");
        // The list page mixes a saved article with a Reader highlight doc (a
        // child of that article). Only the article is a saved item; the child
        // must NOT pollute the item stream.
        let api = MockApi::new()
            .export(vec![export_book()])
            .list(vec![reader_doc(), reader_highlight_doc()]);

        let out = pull_with(&v, &api, "primary-tok").unwrap();
        assert_eq!(out.counts.get("highlights"), Some(&2));
        assert_eq!(out.counts.get("items"), Some(&1), "the child highlight doc is excluded");

        // Contract item stream, partitioned by saved_at month.
        let items = std::fs::read_to_string(v.root().join("reading/readwise/2026-06.jsonl")).unwrap();
        assert_eq!(items.lines().count(), 1, "only the top-level article, not the child doc");
        // The Reader highlight doc's id never appears as a saved item.
        assert!(
            !items.contains("01gm6kjzhhhh609yepjrmcgz8z"),
            "Reader highlight/note docs must not land in the item stream: {items}"
        );
        assert!(items.contains("\"progress\":63"), "integer percent on disk: {items}");
        assert!(items.contains("\"state\":\"archived\""));

        // Highlights stream under highlights/, partitioned by highlighted_at —
        // June (884412) and May (884413) land in different month files.
        let hl_jun = std::fs::read_to_string(v.root().join("reading/readwise/highlights/2026-06.jsonl")).unwrap();
        let hl_may = std::fs::read_to_string(v.root().join("reading/readwise/highlights/2026-05.jsonl")).unwrap();
        assert_eq!(hl_jun.lines().count(), 1, "884412 → June");
        assert_eq!(hl_may.lines().count(), 1, "884413 → May");
        assert!(hl_jun.contains("\"guid\":\"884412\""));

        // Raw layer mirrors the partitioning under raw/, verbatim API objects.
        let raw_jun = std::fs::read_to_string(v.root().join("reading/readwise/raw/2026-06.jsonl")).unwrap();
        assert!(raw_jun.contains("\"book_id\":17506326"), "raw keeps fields the contract drops");
        assert!(raw_jun.contains("\"reading_progress\":0.63"), "raw keeps the source fraction");

        // Two independent watermarks advanced to each leg's max updated_at.
        let state = v.read_readwise_sync();
        assert_eq!(state.highlights_updated_after.as_deref(), Some("2026-06-08T20:12:00+00:00"));
        assert_eq!(state.items_updated_after.as_deref(), Some("2026-06-10T18:00:00+00:00"));
        assert!(state.updated.is_some());

        // The cursor file carries NO token.
        let cursor = std::fs::read_to_string(v.root().join(".trove/readwise-sync.json")).unwrap();
        assert!(!cursor.contains("primary-tok"), "token never in the cursor");

        // Re-run with the same input → guid dedupe, byte-identical files.
        let again = pull_with(&v, &MockApi::new().export(vec![export_book()]).list(vec![reader_doc()]), "primary-tok").unwrap();
        assert_eq!(again.counts.get("highlights"), Some(&0));
        assert_eq!(again.counts.get("items"), Some(&0));
        let items2 = std::fs::read_to_string(v.root().join("reading/readwise/2026-06.jsonl")).unwrap();
        assert_eq!(items, items2, "item file byte-identical after re-run");
    }

    #[test]
    fn reader_401_without_fallback_skips_reader_but_keeps_highlights() {
        let v = temp_vault("reader401");
        let api = MockApi::new().export(vec![export_book()]);
        api.list_pages.borrow_mut().push_back(Err(FetchError::Unauthorized));

        // No TROVE_READWISE_READER_TOKEN set in this env → Reader leg skips.
        let out = pull_with(&v, &api, "primary-tok").unwrap();
        assert_eq!(out.counts.get("highlights"), Some(&2), "highlights still synced");
        assert_eq!(out.counts.get("items"), Some(&0), "Reader leg skipped, not errored");
        // The highlights watermark advanced; the items watermark stays unset.
        let state = v.read_readwise_sync();
        assert!(state.highlights_updated_after.is_some());
        assert!(state.items_updated_after.is_none(), "no Reader drain → no item watermark");
    }

    #[test]
    fn partial_export_drain_failure_does_not_advance_watermark() {
        let v = temp_vault("partialfail");
        let api = MockApi::new();
        // Page 1 OK with a cursor, page 2 errors mid-drain → the whole export
        // drain fails and the pull errors WITHOUT advancing the watermark.
        api.export_pages.borrow_mut().push_back(Ok(Page {
            items: vec![export_book()],
            next_cursor: Some("CUR2".into()),
        }));
        api.export_pages.borrow_mut().push_back(Err(FetchError::Other("boom".into())));

        let err = pull_with(&v, &api, "primary-tok").unwrap_err().to_string();
        assert!(err.contains("export"), "error names the endpoint: {err}");
        // Nothing committed: no watermark, so the next sync re-drains from scratch.
        let state = v.read_readwise_sync();
        assert!(state.highlights_updated_after.is_none(), "partial drain must not advance");
    }

    #[test]
    fn drain_follows_next_page_cursor_to_completion() {
        let api = MockApi::new();
        api.export_pages.borrow_mut().push_back(Ok(Page { items: vec![json!({"a": 1})], next_cursor: Some("c2".into()) }));
        api.export_pages.borrow_mut().push_back(Ok(Page { items: vec![json!({"a": 2})], next_cursor: None }));
        let all = drain(|c| api.export_page("primary-tok", None, c)).unwrap();
        assert_eq!(all.len(), 2, "both pages drained before returning");
    }

    #[test]
    fn parse_page_reads_results_and_next_cursor_and_bare_array() {
        let p = parse_page(json!({"count": 5, "results": [1, 2], "nextPageCursor": "c1"}));
        assert_eq!(p.items.len(), 2);
        assert_eq!(p.next_cursor.as_deref(), Some("c1"));
        let last = parse_page(json!({"count": 2, "results": [1], "nextPageCursor": null}));
        assert_eq!(last.next_cursor, None);
        let bare = parse_page(json!([1, 2, 3]));
        assert_eq!(bare.items.len(), 3);
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor file deserializes to all-None (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.highlights_updated_after.is_none());
        assert!(empty.items_updated_after.is_none());
        // An older cursor that carried only the highlights watermark still
        // deserializes (additive evolution — prove old lines load).
        let partial: SyncState =
            serde_json::from_str(r#"{"highlights_updated_after":"2026-06-01T00:00:00+00:00"}"#).unwrap();
        assert_eq!(partial.highlights_updated_after.as_deref(), Some("2026-06-01T00:00:00+00:00"));
        assert!(partial.items_updated_after.is_none());
    }

    // --- connection tests -------------------------------------------------

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        // Store directly (def_connect needs the network for /auth/).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "rw_secret_abc".into(),
                refresh_token: None,
                token_type: Some("Token".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Readwise");
        assert_eq!(status.accounts[0].key, "readwise");

        // The token is NOT in any non-secret file (the cursor).
        v.write_readwise_sync(&SyncState {
            highlights_updated_after: Some("2026-06-01T00:00:00+00:00".into()),
            items_updated_after: None,
            updated: Some("2026-06-15T00:00:00-07:00".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/readwise-sync.json")).unwrap();
        assert!(!cursor.contains("rw_secret_abc"), "token never in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("rw_secret_abc") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
            assert!(found, "the token was stored under .trove/sync");
        }

        def_disconnect(&v, "readwise").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_token_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "readwise");
        assert_eq!(DEF.connection, Some("readwise"));
    }
}
