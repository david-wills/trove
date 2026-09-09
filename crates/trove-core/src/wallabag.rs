//! Wallabag — self-hosted (and wallabag.it) read-later service; periodic cloud
//! sync via the OAuth2 REST API into the bound [`crate::reading`] contract.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/wallabag.md.
//!
//! A **Periodic** cloud pull — `GET {instance}/api/entries.{_format}` is the
//! sole entries endpoint. Each entry becomes a [`crate::reading::Item`] under
//! `reading/wallabag/YYYY-MM.jsonl` (`guid` = entry `id`; `ts` = `created_at`).
//! Annotations (highlights) on each entry become [`crate::reading::Highlight`]
//! rows under `reading/wallabag/highlights/YYYY-MM.jsonl`.
//!
//! Two layers:
//! - **Raw:** `reading/wallabag/raw/YYYY-MM.jsonl` — full-fidelity API entry
//!   objects (including `content`, `annotations`, etc.), unconditional.
//! - **Contract:** `reading/wallabag/YYYY-MM.jsonl` — normalized
//!   [`reading::Item`] rows, deduped by `guid`.
//!
//! ## Auth: OAuth2 password grant
//!
//! Wallabag does *not* implement a browser redirect flow — it uses OAuth2
//! `password` grant: the user pastes their instance URL, OAuth2 client id,
//! client secret, username, and password as a pipe-delimited composite string
//! `{url}|{client_id}|{client_secret}|{username}|{password}`. On connect we
//! exchange this for an access + refresh token (stored via `save_sync_token`)
//! and discard the plaintext credentials. The run hook refreshes the token on
//! expiry; a refresh failure surface as a reconnect prompt.
//!
//! ## API field names (GET /api/entries.json — authoritative: entry entity with
//! `entries_for_user` serialization group):
//!
//! ```text
//! {
//!   "id":              42,
//!   "uid":             "6a34…",          // public UID (optional)
//!   "title":           "Article Title",
//!   "url":             "https://…",
//!   "is_archived":     0,                // 0/1 integer
//!   "is_starred":      0,                // 0/1 integer
//!   "content":         "<article HTML>",
//!   "created_at":      "2024-02-18T22:15:00+00:00",
//!   "updated_at":      "2024-02-18T22:15:00+00:00",
//!   "published_at":    "2024-02-01T00:00:00+00:00",
//!   "published_by":    ["Author Name"],
//!   "reading_time":    5,                // minutes (integer)
//!   "domain_name":     "example.com",
//!   "preview_picture": "https://…",
//!   "language":        "en",
//!   "tags":            [{"id": 1, "label": "rust", "slug": "t:rust"}],
//!   "annotations":     [
//!       {"id": 7, "text": "The quote", "quote": "The full quote text",
//!        "created_at": "…", "updated_at": "…",
//!        "ranges": [{"start": "…", "startOffset": 0, "end": "…", "endOffset": 3}]}
//!   ]
//! }
//! ```
//!
//! List response wrapper (Hateoas):
//! ```text
//! {"page": 1, "limit": 30, "pages": 5, "total": 120,
//!  "_embedded": {"items": [...]},
//!  "_links": {"self": {…}, "first": {…}, "last": {…}}}
//! ```
//!
//! ## Cursor
//!
//! `since` is a Unix timestamp (integer). We pass it as a query param and
//! fetch all pages until `page == pages`. The cursor is persisted in
//! `.trove/wallabag-sync.json` and only advanced after a full successful drain.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
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

/// Contract-layer item stream.
const DIR: &str = "reading/wallabag";
/// Highlights from annotations.
const HIGHLIGHTS_DIR: &str = "reading/wallabag/highlights";
/// Full-fidelity raw API objects.
const RAW_DIR: &str = "reading/wallabag/raw";

/// Non-secret rebuildable cursor. Deleting it forces a full re-drain.
const SYNC_FILE: &str = ".trove/wallabag-sync.json";

/// Service id under `.trove/sync/` where the OAuth token is stored (secret).
const SERVICE: &str = "wallabag";

/// Items per page (Wallabag supports up to 100).
const PER_PAGE: u64 = 100;

/// HTTP timeout per request. Self-hosted instances may be LAN-slow.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs — hourly for a personal read-later archive.
pub const WALLABAG_SYNC_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("entries").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("wallabag synced — {n} entries")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "wallabag sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let e = out.counts.get("entries").copied().unwrap_or(0);
    let h = out.counts.get("highlights").copied().unwrap_or(0);
    let headline = if e == 0 && h == 0 {
        "Wallabag is up to date — no new entries".to_string()
    } else {
        format!("Wallabag synced — {e} entries, {h} highlights")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "wallabag",
        name: "Wallabag",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Sync saved articles, annotations, and tags from your self-hosted Wallabag \
                      instance or wallabag.it. Paste your instance URL plus the OAuth2 client id \
                      and secret to connect.",
        domain: "reading",
        vault_path: "reading/wallabag/",
        toggleable: true,
        setup: &[
            "Open your Wallabag instance → Settings → API clients management → Create a new client.",
            "Copy your Client ID and Client Secret from the created client.",
            "Paste the five values separated by pipe characters: \
             {instance_url}|{client_id}|{client_secret}|{username}|{password}",
            "Example: https://app.wallabag.it|1_myapp|secret|alice|pass123",
            "First sync backfills all entries; later syncs fetch only what was added or updated.",
        ],
        caveats: "Supports both wallabag.it accounts and self-hosted instances. The instance URL \
                  is user-supplied and stored alongside the OAuth token — never hardcoded. \
                  Article full text is stored in the raw layer only (not in the contract rows).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WALLABAG_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("wallabag"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste composite):
// "{instance_url}|{client_id}|{client_secret}|{username}|{password}"
//
// On connect: exchange for an OAuth2 password-grant token, persist the token
// set + the base URL + client creds in the secret store. The plaintext
// password is stored only long enough to do the exchange (it never hits disk).
// Subsequent syncs use the stored access + refresh token pair.

/// The secret store holds a JSON blob containing the token + the base URL +
/// the client creds needed for refresh. We piggyback everything on the
/// `access_token` field of `TokenSet` by encoding as `{base_url}\n{token_json}`.
/// The client id + secret ride in `scope` as `{client_id}:{client_secret}` — a
/// safe choice since neither contains a colon in practice; if they do the user
/// needs to check their client setup.
///
/// On disk shape (inside TokenSet.access_token):
/// `{base_url}` — the base URL without trailing slash.
/// TokenSet.refresh_token — the OAuth refresh token.
/// TokenSet.scope — `{client_id}:{client_secret}`.
/// TokenSet.expires_at — Unix seconds of the access token expiry.

/// Verify + connect: exchange the composite paste for a real token pair.
fn def_connect(vault: &Vault, composite: &str) -> Result<()> {
    let (base_url, client_id, client_secret, username, password) =
        split_composite(composite.trim())?;
    let client = WallabagClient::new(base_url.clone());
    let raw = client
        .token_password(&client_id, &client_secret, &username, &password)
        .map_err(|e| anyhow::anyhow!("Wallabag auth failed: {e}"))?;
    persist_token(vault, &base_url, &client_id, &client_secret, raw)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(ts) = vault.load_sync_token(SERVICE)? {
        let base_url = ts.access_token.as_str();
        let label = base_url
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .split('/')
            .next()
            .unwrap_or("wallabag")
            .to_string();
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: ts.expires_at,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
/// The composite paste format is:
/// `{instance_url}|{client_id}|{client_secret}|{username}|{password}`
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "wallabag",
    display_name: "Wallabag",
    methods: &[ConnectMethod::TokenPaste {
        label: "Wallabag credentials (5 fields, pipe-separated)",
        help: "Create an API client in your Wallabag instance settings, then paste five values \
               separated by | : instance URL, client ID, client secret, your username, and your \
               password. Example: https://app.wallabag.it|1_myapp|s3cret|alice|pass123. Your \
               credentials are exchanged for an OAuth token and never stored in plain text.",
        placeholder: "https://app.wallabag.it|1_myapp|secret|username|password",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["wallabag"],
    setup: &[
        "Open your Wallabag instance → Settings → API clients management.",
        "Click 'Create a new client' and copy the Client ID and Client Secret.",
        "Paste the five pipe-separated values: \
         {instance_url}|{client_id}|{client_secret}|{username}|{password}",
    ],
};

// ---------------------------------------------------------------------------
// Credentials parsing.

/// Split `{url}|{client_id}|{client_secret}|{username}|{password}`.
/// Returns (base_url, client_id, client_secret, username, password).
fn split_composite(s: &str) -> Result<(String, String, String, String, String)> {
    let parts: Vec<&str> = s.splitn(5, '|').map(str::trim).collect();
    if parts.len() != 5 {
        bail!(
            "expected five pipe-separated fields: \
             {{instance_url}}|{{client_id}}|{{client_secret}}|{{username}}|{{password}} \
             — got {} part(s)",
            parts.len()
        );
    }
    let [url, cid, csec, user, pass] = parts.as_slice() else {
        unreachable!()
    };
    if url.is_empty() {
        bail!("instance URL must not be empty");
    }
    if cid.is_empty() {
        bail!("client_id must not be empty");
    }
    if csec.is_empty() {
        bail!("client_secret must not be empty");
    }
    if user.is_empty() {
        bail!("username must not be empty");
    }
    if pass.is_empty() {
        bail!("password must not be empty");
    }
    let base_url = url.trim_end_matches('/').to_string();
    Ok((base_url, cid.to_string(), csec.to_string(), user.to_string(), pass.to_string()))
}

/// Persist the base URL + client creds + token set to the secret store.
/// Encoding:
/// - `access_token` field: the base URL (the non-secret routing coordinate)
/// - `refresh_token` field: the OAuth refresh token
/// - `scope` field: `{client_id}:{client_secret}` (for refresh calls)
/// - `expires_at`: Unix seconds of access token expiry from `expires_in`
fn persist_token(
    vault: &Vault,
    base_url: &str,
    client_id: &str,
    client_secret: &str,
    raw: OAuthTokenResponse,
) -> Result<()> {
    let expires_at = raw.expires_in.map(|s| {
        (Utc::now().timestamp() as u64).saturating_add(s)
    });
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: base_url.to_string(),
            refresh_token: Some(raw.refresh_token.unwrap_or_default()),
            token_type: raw.token_type,
            scope: Some(format!("{client_id}:{client_secret}")),
            expires_at,
        },
    )
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run offline.

/// The minimal OAuth token response from Wallabag's `/oauth/v2/token`.
#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
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

/// One page of `/api/entries.json`.
struct Page {
    items: Vec<Value>,
    current_page: u64,
    total_pages: u64,
}

/// The endpoints the pull needs. Injectable so tests drive the logic offline.
trait WallabagApi {
    /// `POST /oauth/v2/token` password grant → token response JSON.
    fn token_password(
        &self,
        client_id: &str,
        client_secret: &str,
        username: &str,
        password: &str,
    ) -> Result<OAuthTokenResponse, FetchError>;

    /// `POST /oauth/v2/token` refresh_token grant → token response JSON.
    fn token_refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<OAuthTokenResponse, FetchError>;

    /// `GET /api/entries.json?since={since}&page={page}&perPage={per_page}`.
    fn entries_page(
        &self,
        bearer: &str,
        since: u64,
        page: u64,
        per_page: u64,
    ) -> Result<Page, FetchError>;
}

/// Thin live client.
struct WallabagClient {
    base: String,
}

impl WallabagClient {
    fn new(base: String) -> Self {
        WallabagClient { base }
    }

    fn post_form<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<T, FetchError> {
        let url = format!("{}{path}", self.base);
        let body: String = params
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
            .collect::<Vec<_>>()
            .join("&");
        match ureq::post(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
        {
            Ok(resp) => resp
                .into_json::<T>()
                .map_err(|e| FetchError::Other(format!("parsing token response: {e}"))),
            Err(ureq::Error::Status(401 | 400, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(format!("POST {path}: {e}"))),
        }
    }
}

impl WallabagApi for WallabagClient {
    fn token_password(
        &self,
        client_id: &str,
        client_secret: &str,
        username: &str,
        password: &str,
    ) -> Result<OAuthTokenResponse, FetchError> {
        self.post_form(
            "/oauth/v2/token",
            &[
                ("grant_type", "password"),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("username", username),
                ("password", password),
            ],
        )
    }

    fn token_refresh(
        &self,
        client_id: &str,
        client_secret: &str,
        refresh_token: &str,
    ) -> Result<OAuthTokenResponse, FetchError> {
        self.post_form(
            "/oauth/v2/token",
            &[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("refresh_token", refresh_token),
            ],
        )
    }

    fn entries_page(
        &self,
        bearer: &str,
        since: u64,
        page: u64,
        per_page: u64,
    ) -> Result<Page, FetchError> {
        let url = format!("{}/api/entries.json", self.base);
        let since_str = since.to_string();
        let page_str = page.to_string();
        let per_page_str = per_page.to_string();
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {bearer}"))
            .query("since", &since_str)
            .query("page", &page_str)
            .query("perPage", &per_page_str)
            .query("order", "asc")
            .call()
        {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing entries response: {e}")))?;
                Ok(parse_entries_page(v))
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(format!("GET /api/entries.json: {e}"))),
        }
    }
}

/// Percent-encode form values (very minimal — encodes space, |, & etc.).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Parse `{"page":N, "pages":M, "_embedded": {"items": [...]}}`.
fn parse_entries_page(v: Value) -> Page {
    let current_page = v.get("page").and_then(Value::as_u64).unwrap_or(1);
    let total_pages = v.get("pages").and_then(Value::as_u64).unwrap_or(1);
    let items = v
        .get("_embedded")
        .and_then(|e| e.get("items"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Page { items, current_page, total_pages }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Unix timestamp of the most recently synced entry's `updated_at`.
    /// Passed as the `since` parameter on the next pull to fetch only
    /// new/updated entries. `0` on a first-time sync (full backfill).
    #[serde(default)]
    since: u64,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_wallabag_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_wallabag_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Token management.

/// Resolved credentials extracted from the stored secret.
struct Credentials {
    base_url: String,
    client_id: String,
    client_secret: String,
    access_token: String,
    refresh_token: String,
    expires_at: Option<u64>,
}

/// Load stored credentials. Returns an error when not connected.
fn load_credentials(vault: &Vault) -> Result<Credentials> {
    let ts = vault
        .load_sync_token(SERVICE)?
        .context("Wallabag is not connected — paste your credentials in the Integrations tab")?;
    let base_url = ts.access_token.clone();
    let scope = ts.scope.as_deref().unwrap_or("");
    let (client_id, client_secret) = scope.split_once(':').unwrap_or(("", ""));
    // The live access token is stored separately under the service key during
    // the pull — or we derive it via refresh. For the initial design we store
    // the access token in a companion non-secret cursor field and use refresh
    // on demand. However, to keep things simple and secure, we store the bearer
    // token in a second TokenSet under `{SERVICE}-bearer`.
    let bearer_ts = vault.load_sync_token(&format!("{SERVICE}-bearer"))?;
    let access_token = bearer_ts.as_ref().map(|t| t.access_token.as_str()).unwrap_or("").to_string();
    let refresh_token = ts.refresh_token.clone().unwrap_or_default();
    Ok(Credentials {
        base_url,
        client_id: client_id.to_string(),
        client_secret: client_secret.to_string(),
        access_token,
        refresh_token,
        expires_at: ts.expires_at,
    })
}

/// Persist the bearer access token + updated expiry into a companion secret
/// entry so it survives across runs and can be refreshed independently of
/// the long-lived credentials.
fn save_bearer(vault: &Vault, bearer: &str, raw: &OAuthTokenResponse) -> Result<()> {
    let expires_at = raw.expires_in.map(|s| {
        (Utc::now().timestamp() as u64).saturating_add(s)
    });
    vault.save_sync_token(
        &format!("{SERVICE}-bearer"),
        &TokenSet {
            access_token: bearer.to_string(),
            refresh_token: None,
            token_type: raw.token_type.clone(),
            scope: None,
            expires_at,
        },
    )
}

/// Returns a valid bearer token, refreshing if necessary.
fn ensure_bearer(vault: &Vault, creds: &mut Credentials) -> Result<String, anyhow::Error> {
    // If we already have a non-expired bearer token, return it.
    let now = Utc::now().timestamp() as u64;
    if !creds.access_token.is_empty()
        && creds.expires_at.map_or(true, |exp| exp > now.saturating_add(60))
    {
        return Ok(creds.access_token.clone());
    }

    // Attempt a refresh.
    if !creds.refresh_token.is_empty() && !creds.client_id.is_empty() {
        let client = WallabagClient::new(creds.base_url.clone());
        match client.token_refresh(&creds.client_id, &creds.client_secret, &creds.refresh_token) {
            Ok(raw) => {
                let bearer = raw.access_token.clone();
                // Persist the new bearer token.
                save_bearer(vault, &bearer, &raw)?;
                // Update the refresh token + expiry in the main credential slot.
                let expires_at = raw.expires_in.map(|s| now.saturating_add(s));
                vault.save_sync_token(
                    SERVICE,
                    &TokenSet {
                        access_token: creds.base_url.clone(),
                        refresh_token: raw.refresh_token.clone().or_else(|| {
                            // Keep the old refresh token if the server didn't return a new one.
                            Some(creds.refresh_token.clone())
                        }),
                        token_type: raw.token_type.clone(),
                        scope: Some(format!("{}:{}", creds.client_id, creds.client_secret)),
                        expires_at,
                    },
                )?;
                creds.access_token = bearer.clone();
                creds.expires_at = expires_at;
                return Ok(bearer);
            }
            Err(FetchError::Unauthorized) => {
                bail!(
                    "Wallabag refresh token was rejected (401) — reconnect from the Integrations tab"
                );
            }
            Err(FetchError::Other(msg)) => {
                bail!("Wallabag token refresh failed: {msg}");
            }
        }
    }

    bail!(
        "Wallabag access token has expired and no refresh token is available — \
         reconnect from the Integrations tab"
    )
}

// ---------------------------------------------------------------------------
// Raw row.

/// The API entry object verbatim; only `ts` (the contract ts for partitioning)
/// is skipped from serialization — the on-disk line is the raw API object.
#[derive(Serialize)]
struct RawEntry {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// A top-level string field, trimmed; "" when missing/null/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// An RFC3339-ish timestamp → RFC3339 local. Unparseable values pass through.
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

/// Wallabag tags: `[{"id": 1, "label": "rust", "slug": "t:rust"}]` → names.
fn tag_labels(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|t| {
                let label = str_field(t, "label");
                (!label.is_empty()).then_some(label)
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `is_archived`/`is_starred` fields come as 0/1 integers or booleans.
fn bool_field(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

/// Map one Wallabag API entry object to a contract [`Item`]. Returns `None`
/// when the entry has no usable `id` (can't dedup) or no usable `created_at`
/// (can't partition).
fn entry_to_item(entry: &Value) -> Option<Item> {
    let id = entry.get("id")
        .and_then(|v| match v {
            Value::Number(n) => Some(n.to_string()),
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        })?;

    let raw_ts = str_field(entry, "created_at");
    if raw_ts.is_empty() {
        return None;
    }
    let ts = to_local(&raw_ts);
    // Must yield a month partition; otherwise the row can't be filed.
    Partition::Month.key(&ts)?;

    let is_archived = bool_field(entry, "is_archived");
    let is_starred = bool_field(entry, "is_starred");
    let state = if is_starred {
        "favorite"
    } else if is_archived {
        "archived"
    } else {
        "saved"
    };

    let read_at = str_field(entry, "archived_at");
    let read_at = if !read_at.is_empty() { to_local(&read_at) } else { String::new() };

    // reading_time is in minutes (integer); no unit conversion needed.
    let reading_time = entry.get("reading_time").and_then(Value::as_i64);

    // published_by: array of author strings → join.
    let author = match entry.get("published_by") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|a| a.as_str())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    };

    let mut extra = Map::new();
    put_str(&mut extra, "uid", &str_field(entry, "uid"));
    put_str(&mut extra, "preview_picture", &str_field(entry, "preview_picture"));
    put_str(&mut extra, "language", &str_field(entry, "language"));
    if let Some(rt) = reading_time {
        if rt > 0 {
            extra.insert("reading_time_min".into(), Value::from(rt));
        }
    }
    put_str(&mut extra, "published_at", &str_field(entry, "published_at"));
    // is_starred preserved so users can query it independently.
    if is_starred {
        extra.insert("is_starred".into(), Value::Bool(true));
    }
    if is_archived {
        extra.insert("is_archived".into(), Value::Bool(true));
    }

    Some(Item {
        ts,
        source: "wallabag".into(),
        guid: id,
        url: str_field(entry, "url"),
        title: str_field(entry, "title"),
        author,
        site: str_field(entry, "domain_name"),
        feed: String::new(),
        excerpt: String::new(), // content is full HTML; not suitable as excerpt
        tags: tag_labels(entry.get("tags")),
        state: state.to_string(),
        progress: None, // Wallabag doesn't expose reading progress
        read_at,
        extra,
    })
}

/// Map a Wallabag annotation on `entry` to a contract [`Highlight`].
/// Returns `None` when the annotation has no usable id or timestamp.
fn annotation_to_highlight(ann: &Value, entry: &Value) -> Option<Highlight> {
    let id = ann.get("id")
        .and_then(|v| match v {
            Value::Number(n) => Some(format!("wallabag-ann-{n}")),
            Value::String(s) if !s.is_empty() => Some(format!("wallabag-ann-{s}")),
            _ => None,
        })?;

    let raw_ts = str_field(ann, "created_at");
    if raw_ts.is_empty() {
        return None;
    }
    let ts = to_local(&raw_ts);
    Partition::Month.key(&ts)?;

    let entry_id = entry.get("id").map(|v| v.to_string()).unwrap_or_default();
    let mut extra = Map::new();
    put_str(&mut extra, "entry_id", &entry_id);
    // Ranges are source-specific position data → extra.
    if let Some(ranges) = ann.get("ranges") {
        if !matches!(ranges, Value::Null) {
            extra.insert("ranges".into(), ranges.clone());
        }
    }

    Some(Highlight {
        ts,
        source: "wallabag".into(),
        guid: id,
        // `text` is the user's note/comment; `quote` is the selected passage.
        text: str_field(ann, "quote"),
        note: str_field(ann, "text"),
        title: str_field(entry, "title"),
        author: String::new(),
        url: str_field(entry, "url"),
        location: String::new(),
        color: String::new(),
        tags: Vec::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

/// Append new entry contract + raw rows for this pull, deduped by guid.
/// Also extracts and appends annotation highlights.
/// Returns `(entries_written, highlights_written)`.
fn write_entries(
    vault: &Vault,
    entries: Vec<(Item, Vec<Highlight>, Value)>,
) -> Result<(u64, u64)> {
    let contract = vault.stream(DIR, Partition::Month);
    let highlights_stream = vault.stream(HIGHLIGHTS_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // Collect existing guids to make the write idempotent.
    let mut seen_items: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen_items.insert(g);
            }
        }
    }
    let mut seen_highlights: HashSet<String> = HashSet::new();
    for key in highlights_stream.partitions()? {
        for v in highlights_stream.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen_highlights.insert(g);
            }
        }
    }

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_highlights: Vec<Highlight> = Vec::new();
    let mut new_raws: Vec<RawEntry> = Vec::new();

    for (item, highlights, raw_val) in entries {
        let guid = item.guid.clone();
        if guid.is_empty() {
            continue;
        }
        // Always process highlights regardless of whether the parent entry is
        // new — Wallabag's `since` filter delivers re-fetched entries when
        // their updated_at bumped (e.g. a highlight was added to an existing
        // article). Dedup solely by the highlight's own guid set.
        for hl in highlights {
            if !hl.guid.is_empty() && seen_highlights.insert(hl.guid.clone()) {
                new_highlights.push(hl);
            }
        }
        if seen_items.insert(guid.clone()) {
            // New entry: write contract row and raw row.
            // Known raw-fidelity limitation: if an existing entry is mutated
            // (re-tag / archive / re-parse) its updated_at bumps and the
            // server re-delivers it via `since`, but we skip both the contract
            // and raw rows here (append-only). The contract layer is
            // intentionally append-only. The raw layer silently carries a
            // stale full-text/state snapshot; raw is rebuildable so this is
            // acceptable for now.
            new_raws.push(RawEntry { ts: item.ts.clone(), value: raw_val });
            new_items.push(item);
        }
    }

    contract.append(&new_items, |i| &i.ts)?;
    highlights_stream.append(&new_highlights, |h| &h.ts)?;
    raw_stream.append(&new_raws, |r| &r.ts)?;

    Ok((new_items.len() as u64, new_highlights.len() as u64))
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve stored credentials, refresh the bearer if needed, and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let mut creds = load_credentials(vault)?;
    let bearer = ensure_bearer(vault, &mut creds)?;
    let client = WallabagClient::new(creds.base_url.clone());
    pull_with(vault, &client, &bearer)
}

/// The pull body over an injected API + bearer — the testable seam.
fn pull_with(vault: &Vault, api: &impl WallabagApi, bearer: &str) -> Result<PullOutcome> {
    let mut state = vault.read_wallabag_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Drain all pages starting at `since`. The `since` parameter instructs
    // Wallabag to return entries with `updated_at > since` (strict >,
    // confirmed in EntryRepository.php:315; Unix timestamp).
    // We start from page 1 and follow until page == pages.
    let since = state.since;
    let mut all_entries: Vec<Value> = Vec::new();
    let mut max_updated: u64 = since;

    let first_page = api
        .entries_page(bearer, since, 1, PER_PAGE)
        .map_err(|e| match e {
            FetchError::Unauthorized => anyhow::anyhow!(
                "Wallabag rejected the access token (401) — reconnect from the Integrations tab"
            ),
            FetchError::Other(msg) => anyhow::anyhow!("Wallabag entries fetch failed: {msg}"),
        })?;

    let total_pages = first_page.total_pages.max(1);
    all_entries.extend(first_page.items);

    for page in 2..=total_pages {
        let p = api
            .entries_page(bearer, since, page, PER_PAGE)
            .map_err(|e| match e {
                FetchError::Unauthorized => anyhow::anyhow!(
                    "Wallabag rejected the access token (401) — reconnect from the Integrations tab"
                ),
                FetchError::Other(msg) => {
                    anyhow::anyhow!("Wallabag entries page {page} fetch failed: {msg}")
                }
            })?;
        all_entries.extend(p.items);
    }

    // Map entries to (Item, highlights, raw_value) triples.
    let mut mapped: Vec<(Item, Vec<Highlight>, Value)> = Vec::new();
    for entry in &all_entries {
        // Track max updated_at for cursor advancement.
        let ua = str_field(entry, "updated_at");
        if !ua.is_empty() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(&ua) {
                let epoch = dt.timestamp() as u64;
                if epoch > max_updated {
                    max_updated = epoch;
                }
            }
        }

        let item = match entry_to_item(entry) {
            Some(it) => it,
            None => continue, // unparseable entry — skip without crashing
        };

        // Extract annotations as highlights.
        let highlights: Vec<Highlight> = match entry.get("annotations") {
            Some(Value::Array(anns)) => anns
                .iter()
                .filter_map(|ann| annotation_to_highlight(ann, entry))
                .collect(),
            _ => Vec::new(),
        };

        mapped.push((item, highlights, entry.clone()));
    }

    let (entries_written, highlights_written) = write_entries(vault, mapped)?;
    counts.insert("entries", entries_written);
    counts.insert("highlights", highlights_written);

    // Advance the watermark only after the full successful drain.
    // Do not advance backwards (since is monotonic).
    // Subtract 1 second from max_updated before persisting: the server uses
    // strict `updated_at > :since`, so an entry edited in the same wall-clock
    // second as max_updated but after the drain's read would be excluded
    // forever. The guid dedup set absorbs the harmless boundary re-fetch.
    if max_updated > state.since {
        state.since = max_updated.saturating_sub(1);
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_wallabag_sync(&state)?;

    let e = counts.get("entries").copied().unwrap_or(0);
    let h = counts.get("highlights").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("{e} entries, {h} highlights"),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-wallabag-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — documented API response shapes (confirmed from
    // src/Wallabag/CoreBundle/Entity/Entry.php serialization groups).

    /// A typical Wallabag entry: starred article with tags and annotations.
    fn entry_starred() -> Value {
        json!({
            "id": 42,
            "uid": "6a34abcd",
            "url": "https://example.com/a-local-first-future",
            "title": "A Local-First Future",
            "is_archived": 0,
            "is_starred": 1,
            "content": "<p>Why local data beats the cloud.</p>",
            "created_at": "2024-02-18T22:15:00+00:00",
            "updated_at": "2024-02-18T22:20:00+00:00",
            "published_at": "2024-02-01T00:00:00+00:00",
            "published_by": ["Jane Roe"],
            "reading_time": 7,
            "domain_name": "example.com",
            "preview_picture": "https://example.com/cover.jpg",
            "language": "en",
            "tags": [
                {"id": 1, "label": "local-first", "slug": "t:local-first"},
                {"id": 2, "label": "software", "slug": "t:software"}
            ],
            "annotations": [
                {
                    "id": 7,
                    "text": "This is my note on the quote",
                    "quote": "Why local data beats the cloud.",
                    "created_at": "2024-02-19T08:00:00+00:00",
                    "updated_at": "2024-02-19T08:00:00+00:00",
                    "ranges": [{"start": "/p[1]", "startOffset": 0, "end": "/p[1]", "endOffset": 31}]
                }
            ]
        })
    }

    /// An archived entry with no tags or annotations.
    fn entry_archived() -> Value {
        json!({
            "id": 99,
            "uid": null,
            "url": "https://blog.example.org/read-and-done",
            "title": "Read and Done",
            "is_archived": 1,
            "is_starred": 0,
            "content": "<p>This was worth reading.</p>",
            "created_at": "2024-06-10T09:30:00+00:00",
            "updated_at": "2024-06-11T12:00:00+00:00",
            "archived_at": "2024-06-11T12:00:00+00:00",
            "published_at": null,
            "published_by": [],
            "reading_time": 3,
            "domain_name": "blog.example.org",
            "preview_picture": null,
            "language": "fr",
            "tags": [],
            "annotations": []
        })
    }

    /// A minimal entry (only required fields). Tests graceful handling of
    /// missing/null optional fields.
    fn entry_minimal() -> Value {
        json!({
            "id": 1,
            "url": "https://bare.example.net/page",
            "title": "Bare Entry",
            "is_archived": 0,
            "is_starred": 0,
            "created_at": "2026-01-05T14:00:00+00:00",
            "updated_at": "2026-01-05T14:00:00+00:00",
            "tags": [],
            "annotations": []
        })
    }

    /// A paginated entries response (page 1 of 2).
    fn entries_page_1(item: Value) -> Value {
        json!({
            "page": 1,
            "limit": 100,
            "pages": 2,
            "total": 101,
            "_links": {
                "self": {"href": "/api/entries?page=1"},
                "first": {"href": "/api/entries?page=1"},
                "last": {"href": "/api/entries?page=2"}
            },
            "_embedded": {
                "items": [item]
            }
        })
    }

    /// A paginated entries response (last page).
    fn entries_page_last(item: Value) -> Value {
        json!({
            "page": 2,
            "limit": 100,
            "pages": 2,
            "total": 101,
            "_links": {
                "self": {"href": "/api/entries?page=2"},
                "first": {"href": "/api/entries?page=1"},
                "last": {"href": "/api/entries?page=2"}
            },
            "_embedded": {
                "items": [item]
            }
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_starred_entry_to_contract_item_with_tags_and_extra() {
        let e = entry_starred();
        let item = entry_to_item(&e).unwrap();

        assert_eq!(item.source, "wallabag");
        assert_eq!(item.guid, "42", "guid is the entry id as string");
        assert_eq!(item.url, "https://example.com/a-local-first-future");
        assert_eq!(item.title, "A Local-First Future");
        assert_eq!(item.author, "Jane Roe", "published_by[0] → author");
        assert_eq!(item.site, "example.com", "domain_name → site");
        assert_eq!(item.state, "favorite", "is_starred=1 → state=favorite");
        assert!(item.excerpt.is_empty(), "content not mapped to excerpt");

        let ts_epoch = DateTime::parse_from_rfc3339(&item.ts).unwrap().timestamp();
        let expected = DateTime::parse_from_rfc3339("2024-02-18T22:15:00+00:00").unwrap().timestamp();
        assert_eq!(ts_epoch, expected, "ts = created_at converted to local");

        let mut tags = item.tags.clone();
        tags.sort();
        assert_eq!(tags, vec!["local-first", "software"], "tags extracted from label");

        assert_eq!(item.extra.get("reading_time_min"), Some(&json!(7)));
        assert_eq!(item.extra.get("is_starred"), Some(&json!(true)));
        assert_eq!(item.extra.get("language"), Some(&json!("en")));
        assert_eq!(item.extra.get("uid"), Some(&json!("6a34abcd")));
        assert_eq!(
            item.extra.get("preview_picture"),
            Some(&json!("https://example.com/cover.jpg"))
        );
        // is_archived not in extra when false.
        assert!(item.extra.get("is_archived").is_none());
    }

    #[test]
    fn maps_archived_entry_correctly() {
        let e = entry_archived();
        let item = entry_to_item(&e).unwrap();
        assert_eq!(item.guid, "99");
        assert_eq!(item.state, "archived", "is_archived=1, is_starred=0 → archived");
        assert_eq!(item.author, "", "empty published_by → empty author");
        assert!(item.tags.is_empty());
        assert_eq!(item.extra.get("is_archived"), Some(&json!(true)));
        // uid is null → not in extra.
        assert!(item.extra.get("uid").is_none());
        // read_at from archived_at.
        let ra_epoch = DateTime::parse_from_rfc3339(&item.read_at).unwrap().timestamp();
        let expected = DateTime::parse_from_rfc3339("2024-06-11T12:00:00+00:00").unwrap().timestamp();
        assert_eq!(ra_epoch, expected, "archived_at → read_at");
    }

    #[test]
    fn maps_minimal_entry_without_optional_fields() {
        let e = entry_minimal();
        let item = entry_to_item(&e).unwrap();
        assert_eq!(item.guid, "1");
        assert_eq!(item.state, "saved");
        assert!(item.author.is_empty());
        assert!(item.site.is_empty());
        assert!(item.tags.is_empty());
        assert!(item.extra.is_empty(), "extra should be empty for minimal entry: {:?}", item.extra);
    }

    #[test]
    fn entry_to_item_rejects_missing_id_and_missing_created_at() {
        let mut e = entry_minimal();
        e.as_object_mut().unwrap().remove("id");
        assert!(entry_to_item(&e).is_none(), "no id → None");

        let mut e2 = entry_minimal();
        e2.as_object_mut().unwrap().remove("created_at");
        assert!(entry_to_item(&e2).is_none(), "no created_at → None");
    }

    #[test]
    fn maps_annotation_to_highlight() {
        let e = entry_starred();
        let ann = &e["annotations"][0];
        let hl = annotation_to_highlight(ann, &e).unwrap();

        assert_eq!(hl.source, "wallabag");
        assert_eq!(hl.guid, "wallabag-ann-7");
        assert_eq!(hl.text, "Why local data beats the cloud.", "quote → text");
        assert_eq!(hl.note, "This is my note on the quote", "text → note");
        assert_eq!(hl.title, "A Local-First Future", "entry title inline");
        assert_eq!(hl.url, "https://example.com/a-local-first-future", "entry url inline");
        assert_eq!(hl.extra.get("entry_id"), Some(&json!("42")));
        assert!(hl.extra.get("ranges").is_some(), "ranges preserved in extra");

        let ts_epoch = DateTime::parse_from_rfc3339(&hl.ts).unwrap().timestamp();
        let expected = DateTime::parse_from_rfc3339("2024-02-19T08:00:00+00:00").unwrap().timestamp();
        assert_eq!(ts_epoch, expected, "ts = annotation created_at");
    }

    #[test]
    fn annotation_rejects_missing_id_and_created_at() {
        let e = entry_starred();
        let mut ann = e["annotations"][0].clone();
        ann.as_object_mut().unwrap().remove("id");
        assert!(annotation_to_highlight(&ann, &e).is_none(), "no id → None");

        let mut ann2 = e["annotations"][0].clone();
        ann2.as_object_mut().unwrap().remove("created_at");
        assert!(annotation_to_highlight(&ann2, &e).is_none(), "no created_at → None");
    }

    #[test]
    fn tag_labels_handles_array_of_tag_objects_and_empty() {
        let tags = json!([
            {"id": 1, "label": "rust", "slug": "t:rust"},
            {"id": 2, "label": "wasm", "slug": "t:wasm"}
        ]);
        let names = tag_labels(Some(&tags));
        assert_eq!(names, vec!["rust", "wasm"]);
        assert!(tag_labels(Some(&json!([]))).is_empty());
        assert!(tag_labels(None).is_empty());
    }

    #[test]
    fn bool_field_handles_integer_and_bool_variants() {
        let v = json!({"a": 1, "b": 0, "c": true, "d": false});
        assert!(bool_field(&v, "a"), "1 → true");
        assert!(!bool_field(&v, "b"), "0 → false");
        assert!(bool_field(&v, "c"), "true → true");
        assert!(!bool_field(&v, "d"), "false → false");
        assert!(!bool_field(&v, "missing"), "absent → false");
    }

    #[test]
    fn parse_entries_page_reads_embedded_items_and_pagination() {
        let raw = entries_page_1(entry_starred());
        let page = parse_entries_page(raw);
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.current_page, 1);
        assert_eq!(page.total_pages, 2);
    }

    #[test]
    fn split_composite_parses_five_fields_and_rejects_malformed() {
        let r = split_composite("https://app.wallabag.it|1_myapp|s3cret|alice|pass123").unwrap();
        assert_eq!(r.0, "https://app.wallabag.it");
        assert_eq!(r.1, "1_myapp");
        assert_eq!(r.2, "s3cret");
        assert_eq!(r.3, "alice");
        assert_eq!(r.4, "pass123");

        // Trailing slash stripped from URL.
        let r2 = split_composite("https://example.com/|cid|csec|user|pw").unwrap();
        assert_eq!(r2.0, "https://example.com", "trailing slash stripped");

        // Whitespace trimmed from each field.
        let r3 = split_composite("  https://a.b  |  cid  |  csec  |  u  |  p  ").unwrap();
        assert_eq!(r3.0, "https://a.b");
        assert_eq!(r3.3, "u");

        // Not enough fields.
        assert!(split_composite("url|cid|csec").is_err(), "3 fields → error");
        // Empty sub-field.
        assert!(split_composite("|cid|csec|user|pw").is_err(), "empty url → error");
        assert!(split_composite("url||csec|user|pw").is_err(), "empty client_id → error");
        assert!(split_composite("url|cid|csec|user|").is_err(), "empty password → error");
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        pages: RefCell<Vec<Value>>,
    }

    impl MockApi {
        fn with_pages(pages: Vec<Value>) -> Self {
            MockApi { pages: RefCell::new(pages) }
        }
        fn single(item: Value) -> Self {
            Self::with_pages(vec![json!({
                "page": 1, "limit": 100, "pages": 1, "total": 1,
                "_links": {},
                "_embedded": {"items": [item]}
            })])
        }
        fn empty() -> Self {
            Self::with_pages(vec![json!({
                "page": 1, "limit": 100, "pages": 1, "total": 0,
                "_links": {},
                "_embedded": {"items": []}
            })])
        }
    }

    impl WallabagApi for MockApi {
        fn token_password(&self, _cid: &str, _csec: &str, _u: &str, _p: &str)
            -> Result<OAuthTokenResponse, FetchError>
        {
            Ok(OAuthTokenResponse {
                access_token: "tok123".into(),
                refresh_token: Some("ref456".into()),
                expires_in: Some(3600),
                token_type: Some("Bearer".into()),
            })
        }
        fn token_refresh(&self, _cid: &str, _csec: &str, _rt: &str)
            -> Result<OAuthTokenResponse, FetchError>
        {
            Ok(OAuthTokenResponse {
                access_token: "tok_refreshed".into(),
                refresh_token: Some("ref_new".into()),
                expires_in: Some(3600),
                token_type: Some("Bearer".into()),
            })
        }
        fn entries_page(&self, _bearer: &str, _since: u64, page: u64, _per_page: u64)
            -> Result<Page, FetchError>
        {
            let pages = self.pages.borrow();
            let idx = (page as usize).saturating_sub(1);
            let v = pages.get(idx).cloned().unwrap_or_else(|| json!({
                "page": page, "limit": 100, "pages": 1, "total": 0,
                "_links": {}, "_embedded": {"items": []}
            }));
            Ok(parse_entries_page(v))
        }
    }

    // -----------------------------------------------------------------------
    // Integration tests.

    #[test]
    fn full_pull_writes_contract_raw_and_highlights_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::single(entry_starred());

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("entries"), Some(&1));
        assert_eq!(out.counts.get("highlights"), Some(&1), "one annotation → one highlight");

        // Contract row on disk.
        let items = std::fs::read_to_string(
            v.root().join("reading/wallabag/2024-02.jsonl")
        ).unwrap();
        assert_eq!(items.lines().count(), 1);
        assert!(items.contains("\"guid\":\"42\""));
        assert!(items.contains("\"state\":\"favorite\""));

        // Highlights under highlights/.
        let hl = std::fs::read_to_string(
            v.root().join("reading/wallabag/highlights/2024-02.jsonl")
        ).unwrap();
        assert_eq!(hl.lines().count(), 1);
        assert!(hl.contains("\"guid\":\"wallabag-ann-7\""));
        assert!(hl.contains("Why local data beats the cloud."));

        // Raw layer exists with full content.
        let raw_dir = v.root().join("reading/wallabag/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                n.ends_with(".jsonl").then_some(n)
            })
            .collect();
        assert!(!raw_files.is_empty(), "raw files written");
        let raw_content = std::fs::read_to_string(
            v.root().join("reading/wallabag/raw").join(&raw_files[0])
        ).unwrap();
        assert!(raw_content.contains("\"content\""), "raw keeps content field");
        assert!(raw_content.contains("\"annotations\""), "raw keeps annotations");

        // Cursor advanced to updated_at of entry_starred minus 1 second.
        // We persist max_updated.saturating_sub(1) so same-second edits on
        // the boundary are re-fetched (guid dedup absorbs the re-fetch).
        let state = v.read_wallabag_sync();
        let max_updated = DateTime::parse_from_rfc3339("2024-02-18T22:20:00+00:00")
            .unwrap().timestamp() as u64;
        assert_eq!(state.since, max_updated.saturating_sub(1),
                   "cursor = max updated_at - 1s (same-second boundary safety)");
        assert!(state.updated.is_some());
    }

    #[test]
    fn second_pull_dedupes_existing_entries() {
        let v = temp_vault("dedup");
        let api1 = MockApi::single(entry_starred());
        let out1 = pull_with(&v, &api1, "tok").unwrap();
        assert_eq!(out1.counts.get("entries"), Some(&1));

        // Re-pull with the same entry → guid dedup → 0 new.
        let api2 = MockApi::single(entry_starred());
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(out2.counts.get("entries"), Some(&0), "duplicate entry skipped");
        assert_eq!(out2.counts.get("highlights"), Some(&0), "duplicate highlight skipped");
    }

    #[test]
    fn new_highlight_on_existing_entry_is_written_on_resync() {
        // Regression test for the bug where highlights nested inside the
        // new-entry gate were silently dropped when re-syncing an already-
        // stored entry that had a new annotation added since the last pull.
        let v = temp_vault("hl-resync");

        // First pull: entry with no annotations.
        let entry_no_ann = json!({
            "id": 42,
            "url": "https://example.com/a-local-first-future",
            "title": "A Local-First Future",
            "is_archived": 0,
            "is_starred": 0,
            "created_at": "2024-02-18T22:15:00+00:00",
            "updated_at": "2024-02-18T22:15:00+00:00",
            "tags": [],
            "annotations": []
        });
        let out1 = pull_with(&v, &MockApi::single(entry_no_ann), "tok").unwrap();
        assert_eq!(out1.counts.get("entries"), Some(&1), "entry stored on first pull");
        assert_eq!(out1.counts.get("highlights"), Some(&0), "no highlights yet");

        // Second pull: same entry (guid 42 already seen) but now it carries a
        // NEW annotation (guid wallabag-ann-99) that was added between pulls.
        let entry_with_ann = json!({
            "id": 42,
            "url": "https://example.com/a-local-first-future",
            "title": "A Local-First Future",
            "is_archived": 0,
            "is_starred": 0,
            "created_at": "2024-02-18T22:15:00+00:00",
            "updated_at": "2024-02-18T22:30:00+00:00",
            "tags": [],
            "annotations": [
                {
                    "id": 99,
                    "text": "My note",
                    "quote": "local data beats the cloud",
                    "created_at": "2024-02-18T22:25:00+00:00",
                    "updated_at": "2024-02-18T22:25:00+00:00",
                    "ranges": []
                }
            ]
        });
        let out2 = pull_with(&v, &MockApi::single(entry_with_ann), "tok").unwrap();
        assert_eq!(out2.counts.get("entries"), Some(&0),
                   "entry guid already seen — no new contract row");
        assert_eq!(out2.counts.get("highlights"), Some(&1),
                   "new highlight on existing entry MUST be written");

        // Verify it landed on disk.
        let hl_dir = v.root().join("reading/wallabag/highlights");
        let hl_files: Vec<_> = std::fs::read_dir(&hl_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                n.ends_with(".jsonl").then_some(n)
            })
            .collect();
        assert!(!hl_files.is_empty(), "highlights dir must contain files");
        let total_lines: usize = hl_files.iter().map(|f| {
            std::fs::read_to_string(v.root().join("reading/wallabag/highlights").join(f))
                .unwrap().lines().count()
        }).sum();
        assert_eq!(total_lines, 1, "exactly one highlight row on disk");

        let hl_content = std::fs::read_to_string(
            v.root().join("reading/wallabag/highlights").join(&hl_files[0])
        ).unwrap();
        assert!(hl_content.contains("\"wallabag-ann-99\""),
                "correct highlight guid persisted");
    }

    #[test]
    fn pulls_multiple_pages_in_sequence() {
        let v = temp_vault("multipage");
        let api = MockApi::with_pages(vec![
            entries_page_1(entry_starred()),
            entries_page_last(entry_archived()),
        ]);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("entries"), Some(&2), "2 entries across 2 pages");
    }

    #[test]
    fn empty_pull_is_noop() {
        let v = temp_vault("empty");
        let api = MockApi::empty();
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("entries"), Some(&0));
        // Cursor updated timestamp set but since stays 0.
        let state = v.read_wallabag_sync();
        assert_eq!(state.since, 0, "no entries → since stays 0");
        assert!(state.updated.is_some(), "updated timestamp set even on empty pull");
    }

    #[test]
    fn omit_empty_fields_on_item_serialize() {
        let e = entry_minimal();
        let item = entry_to_item(&e).unwrap();
        let v = serde_json::to_value(&item).unwrap();
        // Required fields always present.
        assert!(v.get("ts").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("guid").is_some());
        // Empty-omit fields absent.
        assert!(v.get("tags").is_none(), "empty tags omitted");
        assert!(v.get("excerpt").is_none(), "empty excerpt omitted");
        assert!(v.get("author").is_none(), "empty author omitted");
        assert!(v.get("site").is_none(), "empty site omitted");
        assert!(v.get("extra").is_none(), "empty extra omitted");
        assert!(v.get("progress").is_none(), "no progress omitted");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.since, 0, "missing since defaults to 0");
        assert!(empty.updated.is_none());

        let with_since: SyncState =
            serde_json::from_str(r#"{"since": 1708296900}"#).unwrap();
        assert_eq!(with_since.since, 1708296900);
    }

    #[test]
    fn connection_def_is_token_paste_and_references_wallabag() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "wallabag");
        assert_eq!(DEF.connection, Some("wallabag"));
        assert!(DEF.pull.is_some());
        assert!(DEF.last_data.is_some());
        assert_eq!(DEF.meta.domain, "reading");
    }

    #[test]
    fn urlencoding_encodes_special_chars() {
        assert_eq!(urlencoding("hello world"), "hello%20world");
        assert_eq!(urlencoding("pass|word"), "pass%7Cword");
        assert_eq!(urlencoding("a&b=c"), "a%26b%3Dc");
        assert_eq!(urlencoding("simple"), "simple");
    }
}
