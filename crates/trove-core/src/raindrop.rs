//! Raindrop.io — bookmark and read-later manager; periodic cloud sync via REST
//! API into the already-bound [`crate::reading`] contract. Catalogued in the
//! Phase 2 pass; brief: docs/integrations/raindrop.md.
//!
//! A **Periodic** cloud pull over one endpoint, one personal API token:
//!
//! - `GET https://api.raindrop.io/rest/v1/raindrops/0?perpage=50&page=N`
//!   (collection 0 = all raindrops). Each raindrop becomes a
//!   [`crate::reading::Item`] under `reading/raindrop/YYYY-MM.jsonl` (`guid` =
//!   the raindrop `id`; `ts` = `created`, ISO → local; `link` → `url`;
//!   `excerpt` → `excerpt`; `note` → `excerpt` when excerpt is absent; `tags`
//!   straight over; collection name + cover into `extra`). `state` = `"saved"`.
//!
//! Two layers:
//!
//! - **Raw:** `reading/raindrop/raw/YYYY-MM.jsonl` — full-fidelity API objects,
//!   partitioned by `created` month, unconditional.
//! - **Contract:** `reading/raindrop/YYYY-MM.jsonl` — normalized
//!   [`reading::Item`] rows, deduped by `id`.
//!
//! ## Cursor
//!
//! `.trove/raindrop-sync.json` (non-secret, rebuildable) holds:
//! - `last_update` — the RFC3339 max `lastUpdate` seen across the drain;
//!   drives an `on lastUpdate` cursor so a re-sync stops once all items seen
//!   have `lastUpdate ≤ cursor` (page until a short page, then check every
//!   item's `lastUpdate` ≤ cursor to stop early on an incremental run).
//! - `updated` — wall-clock time of the last successful sync.
//!
//! The cursor advances only after the full drain; a crash re-drains.
//!
//! ## API field names (from official docs — developer.raindrop.io/v1/raindrops)
//!
//! - `_id` — stable integer id (the `guid`; this is the real field name — NOT `id`)
//! - `link` — the saved URL (NOT `url`)
//! - `title`, `excerpt`, `note`, `domain`, `type`, `cover`, `important`
//! - `tags` — `["tag1","tag2"]`
//! - `created`, `lastUpdate` — ISO 8601 timestamps
//! - `collection` — `{"$id": N, "title": "..."}`  (title may be absent on all)
//!
//! Response wrapper: `{"result":true,"items":[...],"count":N}`
//!
//! Auth: `Authorization: Bearer TOKEN`.

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

/// Contract-layer item stream; raw full-fidelity objects under `raw/`.
const DIR: &str = "reading/raindrop";
const RAW_DIR: &str = "reading/raindrop/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-fetches from the beginning.
const SYNC_FILE: &str = ".trove/raindrop-sync.json";

/// Service id under `.trove/sync/` where the pasted token is stored.
const SERVICE: &str = "raindrop";

const API_BASE: &str = "https://api.raindrop.io";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Items per page — Raindrop max is 50.
const PER_PAGE: u64 = 50;
/// Collection 0 = all bookmarks.
const COLLECTION_ALL: &str = "0";
/// Seconds between syncs in the watcher loop. Hourly is fine for bookmarks.
pub const RAINDROP_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!("raindrop synced — {} bookmarks", c("items"))
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "raindrop sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("Raindrop.io synced — {} bookmarks", c("items")),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "raindrop",
        name: "Raindrop.io",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your Raindrop.io bookmarks, collections, and tags into the unified \
                      reading store via the official REST API. First sync backfills everything; \
                      later syncs fetch only what changed.",
        domain: "reading",
        vault_path: "reading/raindrop/",
        toggleable: true,
        setup: &[
            "Open app.raindrop.io/settings/integrations while signed in to Raindrop.",
            "Under 'For Developers', click 'Create test token' to generate a personal API token.",
            "Copy the token and paste it here -- it's stored locally and never sent anywhere but \
             Raindrop.",
        ],
        caveats: "The personal test token from the integrations settings page covers all read \
                  operations on the free tier. Some backup/export features are Pro-only, but the \
                  API read path used here is not gated.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(RAINDROP_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("raindrop"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a Raindrop personal API token, a SECRET).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!(
            "empty token — paste your Raindrop personal API token from \
             app.raindrop.io/settings/integrations (Create test token)"
        );
    }
    // Verify with a lightweight call: fetch one raindrop from collection 0.
    // A 401 means the token is wrong; any other error is network/transient.
    let client = RaindropClient::new(API_BASE.to_string(), token.to_string());
    match client.get_page(0, 1) {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Raindrop rejected the token (401) — copy it fresh from \
             app.raindrop.io/settings/integrations"
        ),
        Err(e) => bail!("Raindrop auth check failed: {e}"),
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
            label: "Raindrop.io".to_string(),
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
    id: "raindrop",
    display_name: "Raindrop.io",
    methods: &[ConnectMethod::TokenPaste {
        label: "Raindrop personal API token",
        help: "Paste your Raindrop personal test token from app.raindrop.io/settings/integrations \
               (under 'For Developers' -> 'Create test token'). Stored locally, never sent anywhere \
               but Raindrop.",
        placeholder: "eyJhbGciOiJIUzI1NiIs…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["raindrop"],
    setup: &[
        "Open app.raindrop.io/settings/integrations while signed in.",
        "Under 'For Developers', click 'Create test token'.",
        "Copy the token and paste it here.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable for tests.

/// One page of raindrop items.
struct Page {
    items: Vec<Value>,
    /// True when this page has fewer items than `perpage` — the last page.
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
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The endpoints the pull needs. A trait so tests run fully offline.
trait RaindropApi {
    /// One page of `GET /rest/v1/raindrops/0`. `page` is 0-based.
    fn get_page(&self, page: u64, per_page: u64) -> Result<Page, FetchError>;
}

/// Thin HTTP client. Base URL is injected so tests can swap it.
struct RaindropClient {
    base: String,
    token: String,
}

impl RaindropClient {
    fn new(base: String, token: String) -> Self {
        RaindropClient { base, token }
    }
}

impl RaindropApi for RaindropClient {
    fn get_page(&self, page: u64, per_page: u64) -> Result<Page, FetchError> {
        let url = format!(
            "{}/rest/v1/raindrops/{}",
            self.base, COLLECTION_ALL
        );
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .query("perpage", &per_page.to_string())
            .query("page", &page.to_string())
            .call();

        match resp {
            Ok(r) => {
                let v: Value = r
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok(parse_page(v, per_page))
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
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

/// Parse `{"result":true,"items":[…],"count":N}` into a [`Page`].
/// `is_last` when the items array is shorter than `per_page` (the Raindrop API
/// does not provide a next-page cursor; a short page signals the last page).
fn parse_page(v: Value, per_page: u64) -> Page {
    let items = v
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let is_last = (items.len() as u64) < per_page;
    Page { items, is_last }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max `lastUpdate` seen across the drain — the cursor. RFC3339.
    /// When set, we stop paging once every item on a page has
    /// `lastUpdate ≤ last_update` (items are returned newest-first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_raindrop_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_raindrop_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity API object). Only `value` is serialized.

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
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable/empty values pass
/// through verbatim (the readwise pattern).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Insert `k`→`v` into `extra` only when `v` is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// A Raindrop item → a contract [`Item`]. Returns `None` when the raindrop
/// has no id (can't dedup) or no usable `created` timestamp (can't partition).
///
/// Field mapping (official API docs — developer.raindrop.io/v1/raindrops):
/// - `_id` (stable integer) → `guid`
/// - `link` → `url`
/// - `title` → `title`
/// - `excerpt` → `excerpt` (description/snippet from the page)
/// - `note` → appended to `excerpt` when excerpt is empty; carried in `extra.note`
/// - `tags` (`["tag1","tag2"]`) → `tags`
/// - `created` (ISO) → `ts` (local)
/// - `lastUpdate` (ISO) → `extra.lastUpdate` (for cursor; local)
/// - `domain` → `site`
/// - `collection.title` → `extra.collection`
/// - `cover` → `extra.cover`
/// - `type` → `extra.type`
/// - `important` (bool) → `state = "favorite"` when true, else `"saved"`
fn item_from(v: &Value) -> Option<Item> {
    // guid: the stable `_id` field (integer in the API — note the underscore prefix).
    let guid = match v.get("_id") {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return None,
    };

    // ts: `created` → local RFC3339.
    let raw_created = str_field(v, "created");
    if raw_created.is_empty() {
        return None;
    }
    let ts = to_local(&raw_created);
    // Must be partitionable (YYYY-MM prefix).
    Partition::Month.key(&ts)?;

    let url = str_field(v, "link");
    let title = str_field(v, "title");
    let excerpt_raw = str_field(v, "excerpt");
    let note_raw = str_field(v, "note");
    // The contract `excerpt` field is the snippet/description; the Raindrop
    // `note` is the user's own free-text note. Use excerpt when present; fall
    // back to the note so a pure-note bookmark still surfaces text.
    let excerpt = if !excerpt_raw.is_empty() { excerpt_raw } else { note_raw.clone() };

    let site = str_field(v, "domain");

    // tags: Raindrop returns a plain string array.
    let tags: Vec<String> = v
        .get("tags")
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

    // state: `important` (bool) → "favorite"; otherwise "saved".
    let state = if v.get("important").and_then(Value::as_bool) == Some(true) {
        "favorite"
    } else {
        "saved"
    }
    .to_string();

    let mut extra = Map::new();
    // lastUpdate for cursor accounting (local time).
    let last_update = str_field(v, "lastUpdate");
    if !last_update.is_empty() {
        put_str(&mut extra, "lastUpdate", &to_local(&last_update));
    }
    // Collection name (title) and id. The $id field is always present; the
    // title is documented but may be absent on nested/private collections.
    // Carrying $id ensures the collection is identifiable even without a title.
    if let Some(col) = v.get("collection") {
        let col_title = str_field(col, "title");
        put_str(&mut extra, "collection", &col_title);
        // $id is an integer; store as a JSON number string for easy display.
        if let Some(cid) = col.get("$id").and_then(Value::as_i64) {
            put_str(&mut extra, "collection_id", &cid.to_string());
        }
    }
    // Cover image.
    put_str(&mut extra, "cover", &str_field(v, "cover"));
    // Content type (link/article/image/video/document/audio).
    put_str(&mut extra, "type", &str_field(v, "type"));
    // The user's note goes in extra as well for full fidelity.
    put_str(&mut extra, "note", &note_raw);

    Some(Item {
        ts,
        source: "raindrop".into(),
        guid,
        url,
        title,
        author: String::new(),
        site,
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

fn write_layer(
    vault: &Vault,
    rows: Vec<(Item, Value)>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids — re-runnable: a re-pull of an overlapping window never
    // duplicates (the readwise/letterboxd/lastfm pattern).
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
    for (row, raw_val) in rows {
        if row.guid.is_empty() || !seen.insert(row.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: row.ts.clone(), value: raw_val });
        new_rows.push(row);
    }

    contract.append(&new_rows, |r| r.ts.as_str())?;
    raw.append(&new_raws, |r| r.ts.as_str())?;
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
        .context(
            "Raindrop is not connected — add your personal API token in the Integrations tab",
        )?;
    let client = RaindropClient::new(API_BASE.to_string(), token);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl RaindropApi) -> Result<PullOutcome> {
    let mut state = vault.read_raindrop_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Drain every page, newest-first. The Raindrop API does not provide a
    // cursor; we page until a short page (< perpage items). On an incremental
    // sync we stop early once every item on a page has `lastUpdate ≤ cursor`.
    let mut all_items: Vec<Value> = Vec::new();
    let mut page: u64 = 0;
    let prior_cursor = state.last_update.clone();

    'pages: loop {
        let p = api
            .get_page(page, PER_PAGE)
            .map_err(|e| match e {
                FetchError::Unauthorized => anyhow::anyhow!(
                    "Raindrop rejected the token (401) — reconnect from the Integrations tab"
                ),
                FetchError::Other(m) => anyhow::anyhow!("Raindrop fetch failed (page {page}): {m}"),
            })?;

        let is_last = p.is_last;
        // On an incremental sync: if every item on this page has
        // lastUpdate ≤ prior cursor, all remaining pages are also old — stop.
        if let Some(ref cursor) = prior_cursor {
            let all_old = p.items.iter().all(|item| {
                let lu = str_field(item, "lastUpdate");
                !lu.is_empty() && lu.as_str() <= cursor.as_str()
            });
            if all_old && !p.items.is_empty() {
                // These items are already stored — don't re-add them.
                break 'pages;
            }
        }

        all_items.extend(p.items);
        if is_last {
            break;
        }
        page += 1;
    }

    // Watermark: max `lastUpdate` across all drained items (UTC string compare
    // is valid for the source's own same-offset ISO stamps). Advance only after
    // the full drain — a crash re-drains rather than skips.
    let new_watermark = all_items
        .iter()
        .filter_map(|v| {
            let s = str_field(v, "lastUpdate");
            (!s.is_empty()).then_some(s)
        })
        .max();

    // Map and write.
    let rows: Vec<(Item, Value)> = all_items
        .into_iter()
        .filter_map(|v| item_from(&v).map(|it| (it, v)))
        .collect();
    let written = write_layer(vault, rows)?;
    counts.insert("items", written);

    // Advance watermark only after the full drain and successful write.
    if let Some(w) = new_watermark {
        if state.last_update.as_deref().is_none_or(|cur| w.as_str() > cur) {
            state.last_update = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_raindrop_sync(&state)?;

    let i = counts.get("items").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("{i} bookmarks"),
        counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-raindrop-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---  fixtures (documented API response shape) -------------------------
    //
    // Modeled on the official Raindrop.io REST API documentation:
    // https://developer.raindrop.io/v1/raindrops/single
    // Fields: id (integer), link, title, excerpt, note, tags (string[]),
    //         created (ISO), lastUpdate (ISO), collection ({$id, title}),
    //         cover, domain, type, important (bool).

    /// A fully-featured bookmark with tags, note, collection, cover, type.
    fn raindrop_full() -> Value {
        json!({
            "_id": 1029384,
            "link": "https://example.com/a-deep-dive",
            "title": "A Deep Dive into Local-First Software",
            "excerpt": "The cloud is just someone else's computer.",
            "note": "Read this again after building Trove.",
            "tags": ["software", "local-first"],
            "created": "2026-06-10T21:03:00Z",
            "lastUpdate": "2026-06-10T21:10:00Z",
            "collection": {"$id": 7, "title": "Reading"},
            "cover": "https://example.com/cover.jpg",
            "domain": "example.com",
            "type": "article",
            "important": false,
            "order": 0,
            "media": [{"link": "https://example.com/cover.jpg", "type": "image"}]
        })
    }

    /// A minimal bookmark: only _id, link, and created (the three required).
    fn raindrop_minimal() -> Value {
        json!({
            "_id": 9999,
            "link": "https://minimal.example.org/",
            "title": "",
            "excerpt": "",
            "note": "",
            "tags": [],
            "created": "2026-05-01T08:00:00Z",
            "lastUpdate": "2026-05-01T08:00:00Z",
            "collection": {"$id": -1},
            "cover": "",
            "domain": "",
            "type": "link",
            "important": false
        })
    }

    /// A "starred" (important=true) bookmark — maps to state "favorite".
    fn raindrop_starred() -> Value {
        json!({
            "_id": 42,
            "link": "https://starred.example.com/",
            "title": "Starred Bookmark",
            "excerpt": "",
            "note": "A user note.",
            "tags": ["reference"],
            "created": "2026-06-01T12:00:00Z",
            "lastUpdate": "2026-06-02T09:00:00Z",
            "collection": {"$id": 3, "title": "Saved"},
            "cover": "",
            "domain": "starred.example.com",
            "type": "link",
            "important": true
        })
    }

    // --- pure mapping tests -----------------------------------------------

    #[test]
    fn maps_full_raindrop_to_item() {
        let v = raindrop_full();
        let it = item_from(&v).unwrap();
        assert_eq!(it.source, "raindrop");
        assert_eq!(it.guid, "1029384", "guid is the integer id as string");
        assert_eq!(it.url, "https://example.com/a-deep-dive");
        assert_eq!(it.title, "A Deep Dive into Local-First Software");
        assert_eq!(it.excerpt, "The cloud is just someone else's computer.", "excerpt from excerpt field");
        assert_eq!(it.site, "example.com");
        assert_eq!(it.tags, vec!["software", "local-first"]);
        assert_eq!(it.state, "saved", "important=false → saved");
        // ts = created, converted to local (same instant as the UTC source).
        assert_eq!(
            DateTime::parse_from_rfc3339(&it.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T21:03:00.000Z").unwrap().timestamp(),
        );
        // extra carries collection, cover, type, note, lastUpdate.
        assert_eq!(it.extra.get("collection"), Some(&json!("Reading")));
        assert_eq!(it.extra.get("cover"), Some(&json!("https://example.com/cover.jpg")));
        assert_eq!(it.extra.get("type"), Some(&json!("article")));
        assert_eq!(it.extra.get("note"), Some(&json!("Read this again after building Trove.")));
        // lastUpdate stored in extra (local time — same instant as UTC source).
        let lu = it.extra.get("lastUpdate").and_then(Value::as_str).unwrap();
        assert_eq!(
            DateTime::parse_from_rfc3339(lu).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T21:10:00.000Z").unwrap().timestamp(),
            "lastUpdate converted to local and stored in extra",
        );
    }

    #[test]
    fn maps_minimal_raindrop_omits_empty_fields() {
        let v = raindrop_minimal();
        let it = item_from(&v).unwrap();
        assert_eq!(it.guid, "9999");
        assert_eq!(it.url, "https://minimal.example.org/");
        assert!(it.title.is_empty());
        assert!(it.excerpt.is_empty());
        assert!(it.tags.is_empty());
        assert_eq!(it.state, "saved");
        assert!(it.site.is_empty());
        // Serialized form drops empty strings and empty vecs.
        let s = serde_json::to_value(&it).unwrap();
        assert!(s.get("title").is_none(), "empty title omitted");
        assert!(s.get("excerpt").is_none());
        assert!(s.get("tags").is_none());
        assert!(s.get("site").is_none());
        // collection with no title doesn't inject an empty string into extra.
        assert!(
            it.extra.get("collection").is_none()
                || it.extra.get("collection").and_then(Value::as_str) != Some(""),
            "empty collection title must not appear in extra"
        );
    }

    #[test]
    fn starred_bookmark_maps_to_favorite_state_and_note_fallback() {
        let v = raindrop_starred();
        let it = item_from(&v).unwrap();
        assert_eq!(it.state, "favorite", "important=true → favorite");
        // excerpt falls back to the note when the excerpt field is empty.
        assert_eq!(it.excerpt, "A user note.", "note fallback when excerpt is empty");
        assert_eq!(it.extra.get("note"), Some(&json!("A user note.")));
        assert_eq!(it.extra.get("collection"), Some(&json!("Saved")));
    }

    #[test]
    fn item_from_rejects_missing_id_and_missing_created() {
        let no_id = json!({"link": "https://x.example/", "created": "2026-06-01T00:00:00Z"});
        assert!(item_from(&no_id).is_none(), "no _id → None");
        let no_created = json!({"_id": 1, "link": "https://x.example/"});
        assert!(item_from(&no_created).is_none(), "no created → None");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_update.is_none());
        assert!(empty.updated.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_update":"2026-06-10T21:10:00.000Z"}"#).unwrap();
        assert_eq!(partial.last_update.as_deref(), Some("2026-06-10T21:10:00.000Z"));
        assert!(partial.updated.is_none());
    }

    #[test]
    fn parse_page_short_page_is_last() {
        // A page with fewer items than per_page → is_last.
        let v = json!({"result": true, "items": [raindrop_full()], "count": 1});
        let p = parse_page(v, 50);
        assert!(p.is_last, "1 item < 50 perpage → last page");
        assert_eq!(p.items.len(), 1);

        // A full page (50 items) is NOT last.
        let full_items: Vec<Value> = (0..50)
            .map(|i| json!({"_id": i, "link": format!("https://x.example/{i}"), "created": "2026-06-01T00:00:00Z", "lastUpdate": "2026-06-01T00:00:00Z"}))
            .collect();
        let v2 = json!({"result": true, "items": full_items, "count": 50});
        let p2 = parse_page(v2, 50);
        assert!(!p2.is_last, "50 items == 50 perpage → not last");
    }

    // --- scripted mock API ------------------------------------------------

    struct MockApi {
        pages: std::cell::RefCell<std::collections::VecDeque<Result<Page, FetchError>>>,
    }

    impl MockApi {
        fn with_page(items: Vec<Value>, is_last: bool) -> Self {
            let mut q = std::collections::VecDeque::new();
            q.push_back(Ok(Page { items, is_last }));
            MockApi { pages: std::cell::RefCell::new(q) }
        }
        fn push_page(&self, items: Vec<Value>, is_last: bool) {
            self.pages.borrow_mut().push_back(Ok(Page { items, is_last }));
        }
    }

    impl RaindropApi for MockApi {
        fn get_page(&self, _page: u64, _per: u64) -> Result<Page, FetchError> {
            self.pages
                .borrow_mut()
                .pop_front()
                .unwrap_or(Ok(Page { items: vec![], is_last: true }))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::with_page(vec![raindrop_full(), raindrop_starred(), raindrop_minimal()], true);

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("items"), Some(&3));

        // Contract stream, partitioned by created month.
        // raindrop_full and raindrop_starred → June; raindrop_minimal → May.
        let jun = std::fs::read_to_string(v.root().join("reading/raindrop/2026-06.jsonl")).unwrap();
        let may = std::fs::read_to_string(v.root().join("reading/raindrop/2026-05.jsonl")).unwrap();
        assert_eq!(jun.lines().count(), 2, "two June bookmarks");
        assert_eq!(may.lines().count(), 1, "one May bookmark");
        assert!(jun.contains("\"guid\":\"1029384\""));
        assert!(jun.contains("\"state\":\"favorite\""), "starred item");

        // Raw layer mirrors partitioning under raw/.
        let raw_jun = std::fs::read_to_string(v.root().join("reading/raindrop/raw/2026-06.jsonl")).unwrap();
        assert!(raw_jun.contains("\"media\""), "raw keeps fields the contract drops");
        assert!(raw_jun.contains("\"order\""), "raw keeps order field");

        // Watermark advanced to the max lastUpdate seen across the drain.
        let state = v.read_raindrop_sync();
        // Max of "2026-06-10T21:10:00Z", "2026-06-02T09:00:00Z", "2026-05-01T08:00:00Z"
        assert_eq!(
            state.last_update.as_deref(),
            Some("2026-06-10T21:10:00Z"),
            "watermark = max lastUpdate"
        );
        assert!(state.updated.is_some());

        // The cursor file must NOT contain the token.
        let cursor = std::fs::read_to_string(v.root().join(".trove/raindrop-sync.json")).unwrap();
        assert!(!cursor.contains("eyJ"), "token never in cursor");

        // Re-run with the same input → guid dedupe, no new rows, file unchanged.
        let api2 = MockApi::with_page(vec![raindrop_full(), raindrop_starred(), raindrop_minimal()], true);
        let again = pull_with(&v, &api2).unwrap();
        assert_eq!(again.counts.get("items"), Some(&0), "dedupe: nothing new");
        let jun2 = std::fs::read_to_string(v.root().join("reading/raindrop/2026-06.jsonl")).unwrap();
        assert_eq!(jun, jun2, "file byte-identical after re-run");
    }

    #[test]
    fn multi_page_pull_drains_all_pages() {
        let v = temp_vault("multipage");
        let api = MockApi::with_page(vec![raindrop_full()], false);  // page 0, not last
        api.push_page(vec![raindrop_minimal()], true);               // page 1, last

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("items"), Some(&2), "both pages drained");
    }

    #[test]
    fn incremental_stops_at_cursor() {
        // Simulate a re-sync where the cursor equals all items' lastUpdate.
        // The incremental check should stop early and write 0 new rows.
        let v = temp_vault("incremental");

        // First sync — establish the watermark.
        let api1 = MockApi::with_page(vec![raindrop_full()], true);
        pull_with(&v, &api1).unwrap();
        let state = v.read_raindrop_sync();
        assert!(state.last_update.is_some());

        // Second sync — all items have lastUpdate ≤ cursor → stop early.
        let api2 = MockApi::with_page(vec![raindrop_full()], false); // not last
        api2.push_page(vec![raindrop_starred()], true);              // would be page 2

        // raindrop_full's lastUpdate = "2026-06-10T21:10:00Z" which equals
        // the stored cursor. The page check sees all_old=true and breaks.
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("items"), Some(&0), "incremental: 0 new rows when all old");
    }

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        use crate::sync::oauth::TokenSet;
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "rd_secret_xyz".into(),
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
        assert_eq!(status.accounts[0].label, "Raindrop.io");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("rd_secret_xyz") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
            assert!(found, "the token was stored under .trove/sync");
        }

        def_disconnect(&v, "raindrop").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_token_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "raindrop");
        assert_eq!(DEF.connection, Some("raindrop"));
    }
}
