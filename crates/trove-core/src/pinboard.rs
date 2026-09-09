//! Pinboard — bookmarking service; periodic full-archive sync via the v1 API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/pinboard.md.
//!
//! A **Periodic** cloud pull: `GET /v1/posts/all` returns the full bookmark
//! archive in one call (JSON). A `/v1/posts/update` guard short-circuits the
//! fetch when nothing changed. Rate limit is 1 req/3 s — respected via a
//! minimum sleep between the update-check and the full fetch.
//!
//! Two layers per bookmark:
//!
//! - **raw** — the API post object verbatim under
//!   `reading/pinboard/raw/YYYY-MM.jsonl`, partitioned by save-time month
//!   (full fidelity, unconditional).
//! - **contract** — one normalized [`crate::reading::Item`] under
//!   `reading/pinboard/YYYY-MM.jsonl`, deduped by `guid` (= `hash`).
//!
//! Auth is the `user:TOKEN` API token from pinboard.in/settings/password,
//! pasted via [`ConnectMethod::TokenPaste`] and stored under `.trove/sync/`
//! (0600). It rides as a query param (`auth_token=user:TOKEN`), never logged,
//! never written to the cursor or any non-secret file.
//!
//! ## API field names (v1, JSON; authoritative reference: pinboard.in/api/)
//!
//! A post object from `/v1/posts/all?format=json`:
//!
//! ```text
//! {
//!   "href":        "https://…",       // URL of the saved page
//!   "description": "title text",      // user's title for the bookmark
//!   "extended":    "note text",       // longer description / notes
//!   "meta":        "abc123",          // version hash (changes on update)
//!   "hash":        "a1b2c3d4e5f6",   // stable md5 of the URL — our guid
//!   "time":        "2024-02-18T22:15:00Z",  // ISO8601 UTC save time
//!   "shared":      "yes" | "no",     // public/private
//!   "toread":      "yes" | "no",     // in "mark to read" queue
//!   "tags":        "tag1 tag2 tag3"  // space-separated tag string
//! }
//! ```
//!
//! `/v1/posts/update` returns `{"update_time":"2024-02-18T22:15:00Z"}`.

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

/// Contract-layer item stream; raw under `raw/`.
const DIR: &str = "reading/pinboard";
const RAW_DIR: &str = "reading/pinboard/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it forces a full re-pull on the next sync.
const SYNC_FILE: &str = ".trove/pinboard-sync.json";

/// Service id under `.trove/sync/` where the pasted `user:TOKEN` is stored.
const SERVICE: &str = "pinboard";

/// API base.
const API_BASE: &str = "https://api.pinboard.in/v1";

/// Pinboard rate limit: 1 request per 3 seconds minimum. We sleep between
/// the update-check and the full fetch to respect this.
const RATE_LIMIT_SLEEP: Duration = Duration::from_secs(3);

/// HTTP timeout per request. Pinboard is in maintenance mode and can be slow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// Seconds between syncs. Daily is plenty for bookmarks; an incremental pull
/// reduces to one cheap /posts/update call when nothing changed.
pub const PINBOARD_SYNC_SECS: u64 = 86_400;

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
                format!("pinboard synced — {n} bookmarks")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "pinboard sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("bookmarks").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Pinboard is up to date — no new bookmarks".to_string()
    } else {
        format!("Pinboard synced — {n} bookmarks")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "pinboard",
        name: "Pinboard",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync your Pinboard bookmarks and tags. Pinboard's simple token API \
                      returns your full archive; paste your API token to connect.",
        domain: "reading",
        vault_path: "reading/pinboard/",
        toggleable: true,
        setup: &[
            "Open pinboard.in/settings/password while signed in.",
            "Copy the API token (format: username:hex-token).",
            "Paste the whole token here — it's stored locally.",
        ],
        caveats: "Pinboard is in maintenance mode with declining reliability; the API enforces a \
                  1-request-per-3-seconds rate limit. Syncing pulls your full archive each time \
                  (no incremental endpoint), short-circuited by a cheap /posts/update check when \
                  nothing changed. An XML/JSON export is the fallback if the service becomes unavailable.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(PINBOARD_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("pinboard"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the full `user:TOKEN` string from settings/password).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!(
            "empty token — paste your Pinboard API token from pinboard.in/settings/password \
             (format: username:hex-token)"
        );
    }
    // Basic shape check: should contain exactly one colon with non-empty parts.
    let colon_pos = token.find(':').context(
        "token must be in username:hex-token format from pinboard.in/settings/password",
    )?;
    if colon_pos == 0 || colon_pos == token.len() - 1 {
        bail!("token must be in username:hex-token format from pinboard.in/settings/password");
    }
    // Live verify: /posts/update is a lightweight, single-field endpoint.
    let client = PinboardClient::new(token.to_string());
    match client.posts_update() {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Pinboard rejected the token (403/401) — copy it fresh from pinboard.in/settings/password"
        ),
        Err(FetchError::ServiceDegraded(_msg)) => {
            // Service is flaky — store the token anyway; the user can retry later.
        }
        Err(FetchError::Other(msg)) => bail!("Pinboard auth check failed: {msg}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: token.to_string(),
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
        // The username is the prefix before the first `:`.
        let label = ts
            .access_token
            .split(':')
            .next()
            .unwrap_or("pinboard")
            .to_string();
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
    id: "pinboard",
    display_name: "Pinboard",
    methods: &[ConnectMethod::TokenPaste {
        label: "Pinboard API token",
        help: "Paste the full API token from pinboard.in/settings/password — it looks like \
               username:hex-token and is stored locally, never sent anywhere but Pinboard.",
        placeholder: "username:abc123def456…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["pinboard"],
    setup: &[
        "Open pinboard.in/settings/password while signed in to Pinboard.",
        "Copy the API token (format: username:hex-token).",
        "Paste the whole token string here.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    /// Timeout, 5xx, or other transient failure — Pinboard is in maintenance
    /// mode and degraded responses are expected. Treated as honest "try later",
    /// not as a Trove error.
    ServiceDegraded(String),
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::ServiceDegraded(m) => write!(f, "service degraded: {m}"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The two endpoints the pull needs. A trait so tests drive mapping/persist
/// logic with fixtures, never the network.
trait PinboardApi {
    /// `GET /v1/posts/update` → the last-change timestamp.
    fn posts_update(&self) -> Result<String, FetchError>;
    /// `GET /v1/posts/all` → all bookmarks as a JSON array.
    fn posts_all(&self) -> Result<Vec<Value>, FetchError>;
}

/// Thin live client; the token is the full `user:TOKEN` string.
struct PinboardClient {
    token: String,
}

impl PinboardClient {
    fn new(token: String) -> Self {
        PinboardClient { token }
    }

    fn get_json(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{API_BASE}{path}");
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .query("auth_token", &self.token)
            .query("format", "json")
            .call()
        {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing JSON: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code, resp)) if code >= 500 => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::ServiceDegraded(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::ServiceDegraded(format!("I/O error (timeout?): {e}"))),
        }
    }
}

impl PinboardApi for PinboardClient {
    fn posts_update(&self) -> Result<String, FetchError> {
        let v = self.get_json("/posts/update")?;
        Ok(v.get("update_time")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    fn posts_all(&self) -> Result<Vec<Value>, FetchError> {
        let v = self.get_json("/posts/all")?;
        match v {
            Value::Array(a) => Ok(a),
            _ => Err(FetchError::Other(
                "/posts/all returned a non-array response".into(),
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// `update_time` from `/posts/update` at the last successful full-archive
    /// pull. If it matches the next check we skip the fetch entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_update_time: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_pinboard_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_pinboard_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row.

/// The API post object verbatim; only `ts` (the contract ts for partitioning)
/// is skipped from serialization — the on-disk line is the raw API object.
#[derive(Serialize)]
struct RawPost {
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

/// An RFC3339-ish timestamp (Pinboard delivers `2024-02-18T22:15:00Z`)
/// → RFC3339 local. Unparseable/empty values pass through verbatim.
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

/// Map one Pinboard API post object to a contract [`Item`].
///
/// Returns `None` when the post lacks a usable `hash` (the guid / dedupe key)
/// or a usable `time` (needed to partition the row by month).
///
/// API field → contract field:
/// - `href`        → `url`
/// - `description` → `title`
/// - `extended`    → `excerpt`  (the bookmark's own note/description)
/// - `hash`        → `guid`     (stable md5 of the href — the dedupe key)
/// - `time`        → `ts`       (ISO8601 UTC → local RFC3339)
/// - `tags`        → `tags`     (space-separated → `Vec<String>`)
/// - all bookmarks → `state` = `"saved"` (every stored Pinboard bookmark is a
///   saved/default item). `toread` "yes"/"no" rides in `extra` as a secondary flag.
/// - `shared`, `meta` → `extra`
fn post_to_item(post: &Value) -> Option<Item> {
    let hash = str_field(post, "hash");
    if hash.is_empty() {
        return None;
    }
    let raw_ts = str_field(post, "time");
    if raw_ts.is_empty() {
        return None;
    }
    let ts = to_local(&raw_ts);
    // Must yield a month partition; otherwise the row can't be filed.
    Partition::Month.key(&ts)?;

    let tags_raw = str_field(post, "tags");
    let tags: Vec<String> = if tags_raw.is_empty() {
        Vec::new()
    } else {
        tags_raw
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };

    // Every stored Pinboard bookmark IS a saved/default item in the contract's
    // vocabulary (state="saved" = "default, unread"). The toread flag is a
    // secondary bit — preserved verbatim in `extra` — but does NOT gate whether
    // the bookmark is "saved": the entire archive is saved items.
    let toread = str_field(post, "toread");
    let state = "saved".to_string();

    let mut extra = Map::new();
    put_str(&mut extra, "shared", &str_field(post, "shared"));
    put_str(&mut extra, "toread", &toread);
    put_str(&mut extra, "meta", &str_field(post, "meta"));

    Some(Item {
        ts,
        source: "pinboard".into(),
        guid: hash,
        url: str_field(post, "href"),
        title: str_field(post, "description"),
        author: String::new(),
        site: String::new(),
        feed: String::new(),
        excerpt: str_field(post, "extended"),
        tags,
        state,
        progress: None,
        read_at: String::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

/// Append new contract + raw rows for this pull, deduped by guid against what
/// is already on disk. Returns the number of new contract rows written.
fn write_posts(vault: &Vault, posts: Vec<(Item, Value)>) -> Result<u64> {
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

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_raws: Vec<RawPost> = Vec::new();
    for (item, raw_val) in posts {
        if item.guid.is_empty() || !seen.insert(item.guid.clone()) {
            continue; // no id or already stored
        }
        new_raws.push(RawPost { ts: item.ts.clone(), value: raw_val });
        new_items.push(item);
    }

    contract.append(&new_items, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_items.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the stored token and sync. Missing token → clear error (mirrors
/// the todoist/lastfm pattern).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Pinboard is not connected — add your API token in the Integrations tab")?;
    let client = PinboardClient::new(token);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl PinboardApi) -> Result<PullOutcome> {
    let mut state = vault.read_pinboard_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- Update-check short-circuit ----------------------------------------
    // /posts/update is a single cheap call; if update_time is unchanged since
    // the last successful pull, skip the full fetch entirely.
    let update_time = match api.posts_update() {
        Ok(t) => t,
        Err(FetchError::ServiceDegraded(msg)) => {
            // Service is flaky: count zero new bookmarks and report via headline.
            counts.insert("bookmarks", 0);
            return Ok(PullOutcome {
                headline: format!("Pinboard sync skipped — service degraded: {msg}"),
                counts,
            });
        }
        Err(FetchError::Unauthorized) => {
            bail!("Pinboard rejected the token (401/403) — reconnect from the Integrations tab")
        }
        Err(FetchError::Other(msg)) => bail!("Pinboard /posts/update failed: {msg}"),
    };

    if !update_time.is_empty()
        && state.last_update_time.as_deref() == Some(update_time.as_str())
    {
        counts.insert("bookmarks", 0);
        return Ok(PullOutcome {
            headline: "Pinboard is up to date — nothing changed".to_string(),
            counts,
        });
    }

    // Respect the 1 req/3 s rate limit before the full fetch.
    std::thread::sleep(RATE_LIMIT_SLEEP);

    // --- Full archive fetch ------------------------------------------------
    let posts_raw = match api.posts_all() {
        Ok(p) => p,
        Err(FetchError::ServiceDegraded(msg)) => {
            counts.insert("bookmarks", 0);
            return Ok(PullOutcome {
                headline: format!("Pinboard sync skipped — service degraded on /posts/all: {msg}"),
                counts,
            });
        }
        Err(FetchError::Unauthorized) => {
            bail!("Pinboard rejected the token (401/403) on /posts/all — reconnect")
        }
        Err(FetchError::Other(msg)) => bail!("Pinboard /posts/all failed: {msg}"),
    };

    // Map each post to (contract_item, raw_value).
    let pairs: Vec<(Item, Value)> = posts_raw
        .into_iter()
        .filter_map(|raw| {
            let item = post_to_item(&raw)?;
            Some((item, raw))
        })
        .collect();

    let written = write_posts(vault, pairs)?;
    counts.insert("bookmarks", written);

    // Advance the cursor ONLY after a full successful drain (the crash rule).
    if !update_time.is_empty() {
        state.last_update_time = Some(update_time);
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_pinboard_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("Pinboard synced — {written} bookmarks"),
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
            .join(format!("trove-pinboard-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — documented API response shape (pinboard.in/api/).
    //
    // /v1/posts/all returns a JSON array; each element:
    //   href, description, extended, meta, hash, time, shared, toread, tags
    //
    // /v1/posts/update returns:
    //   {"update_time": "2024-02-18T22:15:00Z"}

    fn post_tagged() -> Value {
        json!({
            "href": "https://news.example.net/old-but-gold",
            "description": "Old But Gold",
            "extended": "A classic reference worth keeping.",
            "meta": "cef1e4f8a80c26a3a63acfe285c85234",
            "hash": "a1b2c3d4e5f6789012345678abcdef01",
            "time": "2024-02-18T22:15:00Z",
            "shared": "no",
            "toread": "no",
            "tags": "archive reference"
        })
    }

    fn post_toread() -> Value {
        json!({
            "href": "https://blog.example.org/future-read",
            "description": "Something to Read Later",
            "extended": "",
            "meta": "deadbeef00001111aaaabbbbccccdddd",
            "hash": "b2c3d4e5f60123456789abcdef012345",
            "time": "2024-06-10T09:30:00Z",
            "shared": "yes",
            "toread": "yes",
            "tags": "reading todo"
        })
    }

    fn post_no_tags() -> Value {
        json!({
            "href": "https://example.com/bare",
            "description": "Bare Bookmark",
            "extended": "",
            "meta": "00000000000000000000000000000000",
            "hash": "c3d4e5f6012345678912345678901234",
            "time": "2026-01-05T14:00:00Z",
            "shared": "yes",
            "toread": "no",
            "tags": ""
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_tagged_post_to_contract_item() {
        let p = post_tagged();
        let item = post_to_item(&p).unwrap();

        assert_eq!(item.source, "pinboard");
        assert_eq!(item.guid, "a1b2c3d4e5f6789012345678abcdef01", "guid = hash");
        assert_eq!(item.url, "https://news.example.net/old-but-gold", "url = href");
        assert_eq!(item.title, "Old But Gold", "title = description");
        assert_eq!(item.excerpt, "A classic reference worth keeping.", "excerpt = extended");
        assert_eq!(item.tags, vec!["archive", "reference"], "space-separated tags split");
        // Every Pinboard bookmark is state="saved" (default/unread in the reading contract).
        // toread flag rides in extra, but doesn't gate whether the item is "saved".
        assert_eq!(item.state, "saved", "all bookmarks get state=saved: {:?}", item.state);
        // ts is the local-tz rendering of 2024-02-18T22:15:00Z
        let ts_epoch = DateTime::parse_from_rfc3339(&item.ts).unwrap().timestamp();
        let expected = DateTime::parse_from_rfc3339("2024-02-18T22:15:00Z").unwrap().timestamp();
        assert_eq!(ts_epoch, expected, "ts preserves the instant");
        // extra: shared/toread/meta
        assert_eq!(item.extra.get("shared"), Some(&json!("no")));
        assert_eq!(item.extra.get("toread"), Some(&json!("no")));
        assert_eq!(item.extra.get("meta"), Some(&json!("cef1e4f8a80c26a3a63acfe285c85234")));
    }

    #[test]
    fn maps_toread_post_to_state_saved() {
        let p = post_toread();
        let item = post_to_item(&p).unwrap();
        assert_eq!(item.state, "saved", "toread=yes → state=saved");
        assert_eq!(item.tags, vec!["reading", "todo"]);
        assert_eq!(item.extra.get("shared"), Some(&json!("yes")));
    }

    #[test]
    fn maps_post_with_no_tags_and_no_extended() {
        let p = post_no_tags();
        let item = post_to_item(&p).unwrap();
        assert!(item.tags.is_empty(), "empty tag string → empty tags vec");
        assert!(item.excerpt.is_empty(), "empty extended → empty excerpt");
    }

    #[test]
    fn omit_empty_fields_on_serialize() {
        let p = post_no_tags();
        let item = post_to_item(&p).unwrap();
        let v = serde_json::to_value(&item).unwrap();
        // Required fields always present.
        assert!(v.get("ts").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("guid").is_some());
        // url and title present (non-empty).
        assert!(v.get("url").is_some());
        assert!(v.get("title").is_some());
        // Empty-omit fields absent.
        assert!(v.get("tags").is_none(), "empty tags omitted");
        assert!(v.get("excerpt").is_none(), "empty excerpt omitted");
        assert!(v.get("author").is_none());
        // state="saved" is non-empty so it IS serialized.
        assert_eq!(v.get("state").and_then(|s| s.as_str()), Some("saved"), "state=saved present");
        assert!(v.get("progress").is_none());
        assert!(v.get("read_at").is_none());
    }

    #[test]
    fn rejects_post_with_missing_hash() {
        let mut p = post_tagged();
        p.as_object_mut().unwrap().remove("hash");
        assert!(post_to_item(&p).is_none(), "no hash → None");
    }

    #[test]
    fn rejects_post_with_missing_time() {
        let mut p = post_tagged();
        p.as_object_mut().unwrap().remove("time");
        assert!(post_to_item(&p).is_none(), "no time → None");
    }

    #[test]
    fn partitions_by_local_month_of_ts() {
        // 2024-02-18T22:15:00Z → contract lands in a 2024-02 partition file
        // (or 2024-01 if local TZ is UTC-8; the month of the *local* ts wins).
        let p = post_tagged();
        let item = post_to_item(&p).unwrap();
        let month_key = Partition::Month.key(&item.ts).unwrap();
        // Exactly 7 chars "YYYY-MM".
        assert_eq!(month_key.len(), 7);
        assert!(month_key.starts_with("202"));
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        update_result: Result<String, String>,
        all_result: Result<Vec<Value>, String>,
    }

    impl MockApi {
        fn ok(update_time: &str, posts: Vec<Value>) -> Self {
            MockApi {
                update_result: Ok(update_time.to_string()),
                all_result: Ok(posts),
            }
        }
        fn degraded(msg: &str) -> Self {
            MockApi {
                update_result: Err(msg.to_string()),
                all_result: Err(msg.to_string()),
            }
        }
    }

    impl PinboardApi for MockApi {
        fn posts_update(&self) -> Result<String, FetchError> {
            match &self.update_result {
                Ok(t) => Ok(t.clone()),
                Err(m) => Err(FetchError::ServiceDegraded(m.clone())),
            }
        }
        fn posts_all(&self) -> Result<Vec<Value>, FetchError> {
            match &self.all_result {
                Ok(v) => Ok(v.clone()),
                Err(m) => Err(FetchError::ServiceDegraded(m.clone())),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Integration tests.

    #[test]
    fn full_pull_writes_raw_and_contract_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::ok(
            "2024-06-10T09:30:00Z",
            vec![post_tagged(), post_toread(), post_no_tags()],
        );

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("bookmarks"), Some(&3));

        // Contract rows partitioned by local month.
        // post_tagged → 2024-02 (or nearby), post_toread → 2024-06,
        // post_no_tags → 2026-01.
        let feb = v.root().join("reading/pinboard");
        let files: Vec<_> = std::fs::read_dir(&feb)
            .unwrap()
            .filter_map(|e| {
                let e = e.unwrap();
                let name = e.file_name().to_string_lossy().to_string();
                // Only .jsonl files at the top level (not in raw/).
                if name.ends_with(".jsonl") { Some(name) } else { None }
            })
            .collect();
        assert_eq!(files.len(), 3, "3 posts across 3 different months: {files:?}");

        // Raw layer exists.
        let raw_dir = v.root().join("reading/pinboard/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") { Some(n) } else { None }
            })
            .collect();
        assert!(!raw_files.is_empty(), "raw files written: {raw_files:?}");

        // Raw retains source fields contract drops.
        let raw_content: String = raw_files
            .iter()
            .map(|f| std::fs::read_to_string(v.root().join("reading/pinboard/raw").join(f)).unwrap())
            .collect();
        assert!(raw_content.contains("\"meta\""), "raw keeps meta field");
        assert!(raw_content.contains("\"hash\""), "raw keeps hash field");

        // Cursor advanced.
        let state = v.read_pinboard_sync();
        assert_eq!(
            state.last_update_time.as_deref(),
            Some("2024-06-10T09:30:00Z"),
            "cursor updated"
        );
        assert!(state.updated.is_some());

        // Token NOT in cursor file.
        let cursor_text =
            std::fs::read_to_string(v.root().join(".trove/pinboard-sync.json")).unwrap();
        assert!(!cursor_text.contains("user:"), "token never in the cursor");
    }

    #[test]
    fn second_pull_same_update_time_is_noop() {
        let v = temp_vault("noop");
        let api = MockApi::ok("2024-06-10T09:30:00Z", vec![post_tagged()]);
        let first = pull_with(&v, &api).unwrap();
        assert_eq!(first.counts.get("bookmarks"), Some(&1));

        // Second pull: same update_time → skipped entirely.
        let api2 = MockApi::ok("2024-06-10T09:30:00Z", vec![post_tagged()]);
        let second = pull_with(&v, &api2).unwrap();
        assert_eq!(second.counts.get("bookmarks"), Some(&0), "short-circuit: no re-fetch");
        // The headline reflects the skip.
        assert!(
            second.headline.contains("up to date") || second.headline.contains("nothing changed"),
            "headline: {}",
            second.headline
        );
    }

    #[test]
    fn idempotent_repull_with_new_update_time_dedupes_existing_guids() {
        let v = temp_vault("dedup");
        let api1 = MockApi::ok("2024-06-10T09:30:00Z", vec![post_tagged()]);
        let first = pull_with(&v, &api1).unwrap();
        assert_eq!(first.counts.get("bookmarks"), Some(&1));

        // New update_time (a second bookmark added) but same posts on disk.
        let api2 =
            MockApi::ok("2024-07-01T00:00:00Z", vec![post_tagged(), post_toread()]);
        let second = pull_with(&v, &api2).unwrap();
        assert_eq!(
            second.counts.get("bookmarks"),
            Some(&1),
            "only the new bookmark counted, tagged duplicate skipped"
        );
    }

    #[test]
    fn degraded_service_on_update_check_is_not_an_error() {
        let v = temp_vault("degraded-update");
        let api = MockApi::degraded("connection timeout");
        // Must NOT return Err; returns Ok with 0 bookmarks + degraded headline.
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("bookmarks"), Some(&0));
        assert!(
            out.headline.contains("degraded") || out.headline.contains("skipped"),
            "headline: {}",
            out.headline
        );
        // Cursor not advanced (no successful sync).
        assert!(v.read_pinboard_sync().last_update_time.is_none());
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_update_time.is_none());
        assert!(empty.updated.is_none());

        let partial: SyncState =
            serde_json::from_str(r#"{"last_update_time":"2024-01-01T00:00:00Z"}"#).unwrap();
        assert_eq!(
            partial.last_update_time.as_deref(),
            Some("2024-01-01T00:00:00Z")
        );
        assert!(partial.updated.is_none());
    }

    #[test]
    fn connection_def_is_token_paste_and_references_pinboard() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "pinboard");
        assert_eq!(DEF.connection, Some("pinboard"));
        assert!(DEF.pull.is_some());
        assert!(DEF.last_data.is_some());
        assert_eq!(DEF.meta.domain, "reading");
    }

    #[test]
    fn pull_without_token_returns_clear_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn stored_token_shows_username_in_status() {
        let v = temp_vault("status");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "alice:abc123def456".into(),
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
        assert_eq!(status.accounts[0].label, "alice", "username extracted from token");
        assert_eq!(status.accounts[0].key, "pinboard");

        def_disconnect(&v, "pinboard").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }
}
