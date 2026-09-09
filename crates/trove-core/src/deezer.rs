//! Deezer recent-play history via the official developer API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/deezer.md.
//!
//! A **Periodic** cloud pull (hourly): every track Deezer has recorded in the
//! recent-plays history lands in the unified media stream via the
//! **media-plays write contract** (`docs/vault-spec/domains/media-plays.md`).
//! Two layers per play:
//!
//! - **raw** — the API track object verbatim at
//!   `media/plays/deezer/raw/YYYY-MM.jsonl`, partitioned by the play's month
//!   (full fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/deezer/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! `GET /user/me/history` returns recent plays (up to the `limit` count),
//! newest first. There is no server-side `since` filter, so every poll fetches
//! the full window and the guid dedupe drops already-seen plays. The watermark
//! advances to the max `timestamp` seen, so the contract stream grows forward
//! from connect time (no lifetime backfill exists — the API returns recent plays
//! only; Deezer support says full-history timestamp recovery is in development).
//!
//! **Auth**: Deezer uses a custom OAuth 2.0 variant at `connect.deezer.com`:
//! the consent URL uses `app_id` + `perms` (not `client_id` + `scope`), and
//! the token exchange returns a URL-encoded query string
//! (`access_token=...&expires=...`) rather than JSON. A loopback listener on
//! port 38789 catches the redirect; we parse the custom token response manually.
//! Deezer does NOT support PKCE. The token can expire (the `expires` field in
//! seconds); when the stored token has expired the card shows reconnect (no
//! server-side refresh endpoint exists in the documented API).

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::{AppCredentials, TokenSet};
use crate::vault::Vault;

/// Contract-layer stream directory; raw lines go one level deeper in `raw/`.
const DIR: &str = "media/plays/deezer";
const RAW_DIR: &str = "media/plays/deezer/raw";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/`.
const SYNC_FILE: &str = ".trove/deezer-sync.json";

/// The service slug for stored app creds + token.
const SERVICE: &str = "deezer";

const API_BASE: &str = "https://api.deezer.com";
/// Authorization endpoint (Deezer-specific; uses `app_id`, not `client_id`).
const AUTH_URL: &str = "https://connect.deezer.com/oauth/auth.php";
/// Token exchange endpoint; returns URL-encoded form, not JSON.
const TOKEN_URL: &str = "https://connect.deezer.com/oauth/access_token.php";
/// The loopback port. Must match the registered redirect URI exactly.
/// Assigned: 38580 + 209 = 38789.
const REDIRECT_PORT: u16 = 38789;

/// Permissions required for history access. `listening_history` is the relevant
/// Deezer perm; `basic_access` covers profile info the status check uses.
const DEEZER_PERMS: &str = "basic_access,listening_history";

/// Items per history page — Deezer allows up to 50; we ask for the max so one
/// request covers the typical recent-play window.
const PAGE_LIMIT: u32 = 50;
/// HTTP timeout, short enough to not stall the collector loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Hourly polling cadence (plays trickle in; incremental dedup is cheap).
pub const DEEZER_SYNC_SECS: u64 = 3600;

// Compiled-in app credentials from build-time env vars. Empty defaults mean
// "BYO" (user registers their own app at developers.deezer.com). Baking
// credentials provides a zero-setup experience.
const BAKED_APP_ID: &str = match option_env!("TROVE_DEEZER_APP_ID") {
    Some(v) => v,
    None => "",
};
const BAKED_APP_SECRET: &str = match option_env!("TROVE_DEEZER_APP_SECRET") {
    Some(v) => v,
    None => "",
};

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("plays").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("deezer synced — {n} plays")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "deezer sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let plays = out.counts.get("plays").copied().unwrap_or(0);
    let headline = if plays == 0 {
        "Deezer is up to date — no new plays".to_string()
    } else {
        format!("Deezer synced — {plays} plays")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "deezer",
        name: "Deezer",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Your recently played tracks from Deezer, pulled via the \
                      official developer API. Popular with EU-based listeners. \
                      Also scrobbles to Last.fm natively — if you already use \
                      the last.fm integration your Deezer listens may already \
                      be covered.",
        domain: "media",
        vault_path: "media/plays/deezer/",
        toggleable: true,
        setup: &[
            "Register a free app at developers.deezer.com (any name).",
            "Set its Redirect URI to http://localhost:38789/callback — must match exactly.",
            "Paste the app ID and secret, then click Connect.",
        ],
        caveats: "The API returns recent plays only; full timestamped history \
                  is still in development per Deezer support. \
                  If you scrobble Deezer to Last.fm, the last.fm integration \
                  already covers your listening history. \
                  The Deezer API does not provide a refresh token — \
                  reconnecting is required when the access token expires.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(DEEZER_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("deezer"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — Deezer-specific OAuth (non-standard: app_id/perms params;
// URL-encoded form token response; no PKCE; no refresh token).

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    // Resolve app credentials: explicit (first-time) → saved → baked.
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(SERVICE, &c)?;
            c
        }
        None => vault
            .load_sync_app(SERVICE)?
            .or_else(|| baked_credentials())
            .context(
                "no Deezer app credentials — register an app at developers.deezer.com \
                 and enter the App ID and Secret in the Integrations tab",
            )?,
    };

    // Bind the loopback listener (ephemeral in tests, fixed port in production).
    let listener = TcpListener::bind(("127.0.0.1", REDIRECT_PORT))
        .with_context(|| format!("binding localhost:{REDIRECT_PORT}"))?;
    let token = deezer_oauth_flow(&creds, listener)?;
    vault.save_sync_token(SERVICE, &token)?;
    Ok(())
}

/// Status: configured = app creds available; at most one account.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured =
        vault.load_sync_app(SERVICE)?.is_some() || baked_credentials().is_some();
    let accounts = match vault.load_sync_token(SERVICE)? {
        Some(token) => {
            // Tokens expire; when the stored token is past its expiry the card
            // should prompt reconnect (no server-side refresh endpoint).
            let needs_reconnect = token.expired();
            let mut extra = BTreeMap::new();
            if needs_reconnect {
                extra.insert(
                    "note",
                    "access token has expired — reconnect from the Integrations tab".to_string(),
                );
            }
            vec![ConnectedAccount {
                key: SERVICE.to_string(),
                label: "Deezer".to_string(),
                connected_at: None,
                expires_at: token.expires_at,
                needs_reconnect,
                extra,
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// The Deezer OAuth provider descriptor. Deezer uses non-standard param names
/// (`app_id` / `perms`) but the loopback redirect and code exchange are driven
/// by the custom [`deezer_oauth_flow`] fn above — this descriptor is kept for
/// registry metadata (redirect_port, display_name, baked defaults).
pub static DEEZER_PROVIDER: crate::sync::oauth::Provider = crate::sync::oauth::Provider {
    service: SERVICE,
    display_name: "Deezer",
    auth_url: AUTH_URL,
    token_url: TOKEN_URL,
    scopes: DEEZER_PERMS,
    redirect_port: REDIRECT_PORT,
    use_pkce: false,
    basic_auth: false,
    default_client_id: option_env!("TROVE_DEEZER_APP_ID"),
    default_client_secret: option_env!("TROVE_DEEZER_APP_SECRET"),
    extra_auth_params: &[],
};

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "deezer",
    display_name: "Deezer",
    methods: &[ConnectMethod::OAuth {
        provider: &DEEZER_PROVIDER,
        multi_account: false,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect,
    auto_pull: &["deezer"],
    setup: &[
        "Register a free app at developers.deezer.com (any name, any description).",
        "Set its Redirect URI to http://localhost:38789/callback — must match exactly.",
        "Paste the app's App ID (a number) and Secret Key here.",
        "Note: Deezer tokens expire and require reconnecting. If you already scrobble to Last.fm, your Deezer history may already be there.",
    ],
};

// ---------------------------------------------------------------------------
// Deezer OAuth flow — custom because Deezer's auth server is non-standard.

/// Run the complete Deezer OAuth authorization-code flow using a loopback
/// redirect. Deezer uses `app_id` (not `client_id`) and `perms` (not `scope`).
/// The token exchange returns `access_token=...&expires=...` as URL-encoded
/// text (not JSON). PKCE is not supported.
fn deezer_oauth_flow(creds: &AppCredentials, listener: TcpListener) -> Result<TokenSet> {
    listener
        .set_nonblocking(true)
        .context("set nonblocking on listener")?;

    // Build the consent URL with Deezer's non-standard parameters.
    let redirect_uri =
        format!("http://localhost:{REDIRECT_PORT}/callback");
    let auth_url = format!(
        "{AUTH_URL}?app_id={}&redirect_uri={}&perms={}",
        urlencode(&creds.client_id),
        urlencode(&redirect_uri),
        urlencode(DEEZER_PERMS),
    );

    crate::sync::oauth::open_browser(&auth_url)?;

    // Wait for the loopback redirect (Deezer redirects to our URI with `code=`).
    let code = wait_for_code(&listener, Duration::from_secs(300))?;

    // Exchange the code for an access token. Deezer's token endpoint returns
    // URL-encoded form data, NOT JSON (e.g. `access_token=abc&expires=3600`).
    let secret = creds.client_secret.as_deref().unwrap_or("");
    let token_req_url = format!(
        "{TOKEN_URL}?app_id={}&secret={}&code={}&output=json",
        urlencode(&creds.client_id),
        urlencode(secret),
        urlencode(&code),
    );
    let resp_text = ureq::get(&token_req_url)
        .timeout(HTTP_TIMEOUT)
        .call()
        .map_err(|e| anyhow::anyhow!("Deezer token exchange failed: {e}"))?
        .into_string()
        .context("reading Deezer token response")?;

    parse_deezer_token(&resp_text)
}

/// Parse Deezer's URL-encoded token response.
/// With `output=json` the response may be JSON; without it's URL-encoded form.
/// We try JSON first, then fall back to URL-encoded parsing.
fn parse_deezer_token(resp: &str) -> Result<TokenSet> {
    let resp = resp.trim();
    // Try JSON first (when output=json is honoured).
    if resp.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(resp) {
            if let Some(token) = v.get("access_token").and_then(Value::as_str) {
                let expires_in = v.get("expires").and_then(Value::as_u64);
                let expires_at = expires_in.map(|s| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        + s
                });
                return Ok(TokenSet {
                    access_token: token.to_string(),
                    refresh_token: None,
                    token_type: Some("bearer".into()),
                    scope: Some(DEEZER_PERMS.into()),
                    expires_at,
                });
            }
            // JSON error body?
            if let Some(e) = v.get("error") {
                bail!("Deezer token error: {e}");
            }
        }
    }

    // URL-encoded fallback: `access_token=abc&expires=3600&token_type=Bearer`.
    let mut access_token = None;
    let mut expires_in: Option<u64> = None;
    for pair in resp.split('&') {
        let mut kv = pair.splitn(2, '=');
        match (kv.next(), kv.next()) {
            (Some("access_token"), Some(v)) => access_token = Some(urldecode(v)),
            (Some("expires"), Some(v)) => expires_in = v.parse::<u64>().ok(),
            _ => {}
        }
    }
    let access_token = access_token.filter(|s| !s.is_empty()).context(
        "Deezer token response missing access_token — check the App ID and Secret",
    )?;
    let expires_at = expires_in.map(|s| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + s
    });
    Ok(TokenSet {
        access_token,
        refresh_token: None,
        token_type: Some("bearer".into()),
        scope: Some(DEEZER_PERMS.into()),
        expires_at,
    })
}

/// Block until the browser redirects back with a `code=` query parameter.
fn wait_for_code(listener: &TcpListener, timeout: Duration) -> Result<String> {
    use std::time::Instant;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .context("read timeout")?;
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = match request.split_whitespace().nth(1) {
                    Some(p) => p,
                    None => continue,
                };
                let query = path.splitn(2, '?').nth(1).unwrap_or("");
                let mut code = None;
                let mut error = None;
                for pair in query.split('&') {
                    let mut kv = pair.splitn(2, '=');
                    match (kv.next(), kv.next()) {
                        (Some("code"), Some(v)) => code = Some(urldecode(v)),
                        (Some("error"), Some(v)) => error = Some(urldecode(v)),
                        _ => {}
                    }
                }
                if let Some(err) = error {
                    let body = page("Connection refused", &format!("Deezer: {err}"));
                    let _ = write_http(&mut stream, 200, &body);
                    bail!("Deezer authorization failed: {err}");
                }
                if let Some(c) = code {
                    let body = page(
                        "Trove is connected",
                        "You can close this tab and return to Trove.",
                    );
                    let _ = write_http(&mut stream, 200, &body);
                    return Ok(c);
                }
                // Not the redirect (favicon, etc.) — serve 404 and keep waiting.
                let _ = write_http(&mut stream, 404, "not found");
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!("timed out waiting for the Deezer OAuth redirect");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e).context("accepting Deezer OAuth redirect"),
        }
    }
}

fn write_http(stream: &mut impl Write, status: u16, body: &str) -> std::io::Result<()> {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:-apple-system,sans-serif;display:grid;place-items:center;\
         height:100vh;margin:0\"><div style=\"text-align:center\"><h1>{title}</h1>\
         <p>{detail}</p></div></body></html>"
    )
}

/// Baked app credentials from build-time env vars.
fn baked_credentials() -> Option<AppCredentials> {
    let id = BAKED_APP_ID.trim();
    let secret = BAKED_APP_SECRET.trim();
    if id.is_empty() {
        return None;
    }
    Some(AppCredentials {
        client_id: id.to_string(),
        client_secret: (!secret.is_empty()).then(|| secret.to_string()),
    })
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for tests.

trait HistoryFetch {
    /// Fetch one page of the history. Returns the raw JSON body.
    fn history(&self, access_token: &str, index: u32, limit: u32) -> Result<Value, FetchError>;
}

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

struct DeezerClient {
    base: String,
}

impl HistoryFetch for DeezerClient {
    fn history(&self, access_token: &str, index: u32, limit: u32) -> Result<Value, FetchError> {
        // Deezer passes the token as a query parameter.
        let req = ureq::get(&format!("{}/user/me/history", self.base))
            .timeout(HTTP_TIMEOUT)
            .query("access_token", access_token)
            .query("index", &index.to_string())
            .query("limit", &limit.to_string());
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                // Deezer returns API errors in the body with HTTP 200.
                if let Some(err) = v.get("error") {
                    let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
                    let msg = err.get("message").and_then(Value::as_str).unwrap_or("");
                    return match code {
                        // 200 = OAuthException (invalid/expired token)
                        200 | 300 => Err(FetchError::Unauthorized),
                        429 => Err(FetchError::RateLimited),
                        _ => Err(FetchError::Other(format!("Deezer API error {code}: {msg}"))),
                    };
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

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max play `timestamp` (Unix seconds) ever written. Used for dedup-boundary
    /// reference (no server-side `since` filter exists).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_deezer_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_deezer_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Parse one page of `GET /user/me/history` into (contract rows, raw objects).
///
/// Each item in `data` is a Deezer track object enriched with a `timestamp`
/// field (Unix seconds of when the track was played). Items with no `timestamp`
/// or no `title` are skipped.
fn parse_history_page(body: &Value) -> (Vec<MediaItem>, Vec<Value>) {
    let items = body
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for item in items {
        let ts_secs = item.get("timestamp").and_then(Value::as_i64);
        let Some(ts_secs) = ts_secs else {
            continue; // no play timestamp → skip
        };
        let Some(media_item) = track_to_media_item(item, ts_secs) else {
            continue;
        };
        rows.push(media_item);
        raws.push(item.clone());
    }
    (rows, raws)
}

/// Convert a Deezer history item (track + timestamp) to a [`MediaItem`].
/// Returns `None` if the title is empty.
fn track_to_media_item(item: &Value, ts_secs: i64) -> Option<MediaItem> {
    let title = str_field(item, "title");
    if title.is_empty() {
        return None;
    }
    let artist_name = item
        .get("artist")
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let album_title = item
        .get("album")
        .and_then(|a| a.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // `duration` is in seconds; Deezer records the play event, not how much
    // was actually played — treat it as seconds=0 (unknown-played, like lastfm).
    // But we store the track duration in extra for reference.
    let duration_secs = item.get("duration").and_then(Value::as_u64).unwrap_or(0);

    let ts = DateTime::from_timestamp(ts_secs, 0)?.with_timezone(&Local).to_rfc3339();

    let track_id = item.get("id").and_then(Value::as_i64).unwrap_or(0);
    // guid = deezer-<track_id>-<timestamp_secs> — stable across re-pulls; a
    // re-play of the same track at a different time is a distinct play.
    let guid = format!("deezer-{track_id}-{ts_secs}");

    let mut extra = Map::new();
    let mut put = |k: &str, v: String| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v));
        }
    };
    put("track_id", track_id.to_string());
    put("isrc", str_field(item, "isrc"));
    put("rank", item.get("rank").and_then(Value::as_i64).unwrap_or(0).to_string());
    if duration_secs > 0 {
        put("track_duration_secs", duration_secs.to_string());
    }
    if let Some(artist) = item.get("artist") {
        if let Some(id) = artist.get("id").and_then(Value::as_i64) {
            put("artist_id", id.to_string());
        }
    }
    if let Some(album) = item.get("album") {
        if let Some(id) = album.get("id").and_then(Value::as_i64) {
            put("album_id", id.to_string());
        }
        put(
            "album_cover",
            album
                .get("cover_medium")
                .or_else(|| album.get("cover"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    put("link", str_field(item, "link"));
    put("preview", str_field(item, "preview"));
    put(
        "explicit",
        item.get("explicit_lyrics").and_then(Value::as_bool).map(|b| b.to_string()).unwrap_or_default(),
    );

    Some(MediaItem {
        ts,
        source: "deezer".into(),
        category: "music".into(),
        device: String::new(),
        kind: "play".into(),
        title,
        subtitle: artist_name,
        detail: album_title,
        // Deezer records the play event, not how much was actually played.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Write layer.

/// A raw history item carrying the contract ts for month-partitioning.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

struct WriteStats {
    plays: u64,
    max_ts: Option<i64>,
}

/// Write raw + contract rows, deduping by guid.
fn write_rows(vault: &Vault, rows: &[MediaItem], raws: &[Value]) -> Result<WriteStats> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids for dedup.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for it in contract.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                seen.insert(it.guid);
            }
        }
    }

    let mut new_rows: Vec<MediaItem> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    let mut max_ts: Option<i64> = None;

    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        // Extract the Unix timestamp from the raw item for the watermark.
        if let Some(t) = raw_val.get("timestamp").and_then(Value::as_i64) {
            max_ts = Some(max_ts.map_or(t, |m: i64| m.max(t)));
        }
        if !seen.insert(item.guid.clone()) {
            continue; // already stored
        }
        new_rows.push(item.clone());
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw_stream.append(&new_raws, |r| &r.ts)?;

    Ok(WriteStats { plays: new_rows.len() as u64, max_ts })
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the saved token and sync history. Returns a [`PullOutcome`] with
/// a `plays` count.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Deezer is not connected — connect from the Integrations tab")?;
    if token.expired() {
        bail!(
            "Deezer access token has expired — reconnect from the Integrations tab \
             (Deezer does not issue refresh tokens)"
        );
    }
    let client = DeezerClient { base: API_BASE.to_string() };
    pull_with(vault, &client, &token.access_token)
}

/// The pull body over an injected fetcher — the testable seam.
///
/// The Deezer history endpoint has no `since` filter — it always returns the
/// most recent N plays. We pull one page (up to `PAGE_LIMIT` items), parse
/// them, write raw+contract deduped by guid, and advance the watermark to the
/// max timestamp seen (forward-only). The guid dedupe is the sole idempotency
/// mechanism.
fn pull_with(vault: &Vault, client: &impl HistoryFetch, access_token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_deezer_sync();

    let body = match client.history(access_token, 0, PAGE_LIMIT) {
        Ok(b) => b,
        Err(e @ FetchError::Unauthorized) => {
            // Token rejected (expired or revoked). Clear it so the card prompts
            // reconnect; the user must authenticate again.
            vault.delete_sync_token(SERVICE)?;
            bail!(
                "Deezer rejected the access token: {e} — reconnect from the Integrations tab"
            );
        }
        Err(FetchError::RateLimited) => bail!("Deezer rate limited — try again later"),
        Err(e) => bail!("Deezer fetch failed: {e}"),
    };

    let (rows, raws) = parse_history_page(&body);
    let stats = write_rows(vault, &rows, &raws)?;

    // Advance the watermark (forward-only).
    if let Some(t) = stats.max_ts {
        if state.watermark.is_none_or(|w| t > w) {
            state.watermark = Some(t);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_deezer_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{} plays", stats.plays),
        counts: BTreeMap::from([("plays", stats.plays)]),
    })
}

// ---------------------------------------------------------------------------
// Shared URL utilities.

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(h) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                16,
            ) {
                out.push(h as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-deezer-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A realistic `GET /user/me/history` response body modelled on the
    /// documented Deezer API track object shape (id, title, duration,
    /// artist{id,name}, album{id,title,cover_medium}, isrc, rank,
    /// explicit_lyrics, preview, link) plus the history-specific `timestamp`
    /// field (Unix seconds of the play). Two plays across two different months.
    fn sample_history() -> Value {
        serde_json::json!({
            "data": [
                {
                    "id": 3135556,
                    "title": "Harder, Better, Faster, Stronger",
                    "title_short": "Harder, Better, Faster, Stronger",
                    "isrc": "GBDUW0000059",
                    "link": "https://www.deezer.com/track/3135556",
                    "duration": 226,
                    "rank": 814839,
                    "explicit_lyrics": false,
                    "preview": "https://cdns-preview-d.dzcdn.net/stream/c-dda49a8f3e00d48bc55b28e17a9b63c8-2.mp3",
                    "timestamp": 1748908800,
                    "artist": {
                        "id": 27,
                        "name": "Daft Punk",
                        "tracklist": "https://api.deezer.com/artist/27/top?limit=50",
                        "type": "artist"
                    },
                    "album": {
                        "id": 302127,
                        "title": "Discovery",
                        "cover": "https://api.deezer.com/album/302127/image",
                        "cover_medium": "https://e-cdns-images.dzcdn.net/images/cover/302127/250x250-000000-80-0-0.jpg",
                        "tracklist": "https://api.deezer.com/album/302127/tracks",
                        "type": "album"
                    },
                    "type": "track"
                },
                {
                    "id": 1234567,
                    "title": "One More Time",
                    "title_short": "One More Time",
                    "isrc": "GBDUW0100001",
                    "link": "https://www.deezer.com/track/1234567",
                    "duration": 320,
                    "rank": 900000,
                    "explicit_lyrics": false,
                    "preview": "https://cdns-preview-d.dzcdn.net/stream/preview.mp3",
                    "timestamp": 1746230400,
                    "artist": {
                        "id": 27,
                        "name": "Daft Punk",
                        "tracklist": "https://api.deezer.com/artist/27/top?limit=50",
                        "type": "artist"
                    },
                    "album": {
                        "id": 302127,
                        "title": "Discovery",
                        "cover": "https://api.deezer.com/album/302127/image",
                        "cover_medium": "https://e-cdns-images.dzcdn.net/images/cover/302127/250x250-000000-80-0-0.jpg",
                        "tracklist": "https://api.deezer.com/album/302127/tracks",
                        "type": "album"
                    },
                    "type": "track"
                }
            ],
            "total": 2,
            "next": "https://api.deezer.com/user/me/history?index=50&limit=50"
        })
    }

    /// An item with no timestamp — should be skipped entirely.
    fn no_timestamp_item() -> Value {
        serde_json::json!({
            "data": [
                {
                    "id": 9999,
                    "title": "No Timestamp Track",
                    "duration": 180,
                    "artist": { "id": 1, "name": "Someone" },
                    "album": { "id": 2, "title": "Something" },
                    "type": "track"
                }
            ],
            "total": 1
        })
    }

    #[test]
    fn parses_history_page_correctly() {
        let (rows, raws) = parse_history_page(&sample_history());
        assert_eq!(rows.len(), 2, "two plays parsed");
        assert_eq!(raws.len(), 2);

        let row = &rows[0];
        assert_eq!(row.title, "Harder, Better, Faster, Stronger");
        assert_eq!(row.subtitle, "Daft Punk");
        assert_eq!(row.detail, "Discovery");
        assert_eq!(row.source, "deezer");
        assert_eq!(row.category, "music");
        assert_eq!(row.kind, "play");
        assert_eq!(row.seconds, 0, "play event, not duration");
        assert_eq!(row.guid, "deezer-3135556-1748908800");
        // Timestamp round-trips through UTC epoch.
        assert_eq!(
            DateTime::parse_from_rfc3339(&row.ts).unwrap().timestamp(),
            1748908800
        );
        // Extra fields present.
        assert_eq!(row.extra.get("track_id"), Some(&Value::String("3135556".into())));
        assert_eq!(row.extra.get("isrc"), Some(&Value::String("GBDUW0000059".into())));
        assert_eq!(row.extra.get("artist_id"), Some(&Value::String("27".into())));
        assert_eq!(row.extra.get("album_id"), Some(&Value::String("302127".into())));
        assert!(row.extra.get("track_duration_secs").is_some());
        assert!(row.extra.get("link").is_some());

        // Second play lands in a different month.
        let row2 = &rows[1];
        assert_eq!(row2.title, "One More Time");
        assert_eq!(row2.guid, "deezer-1234567-1746230400");
    }

    #[test]
    fn skips_items_without_timestamp() {
        let (rows, raws) = parse_history_page(&no_timestamp_item());
        assert!(rows.is_empty(), "item with no timestamp must be skipped");
        assert!(raws.is_empty());
    }

    #[test]
    fn empty_data_array_is_a_noop() {
        let body = serde_json::json!({ "data": [], "total": 0 });
        let (rows, raws) = parse_history_page(&body);
        assert!(rows.is_empty());
        assert!(raws.is_empty());
    }

    struct StubClient {
        body: Value,
    }
    impl HistoryFetch for StubClient {
        fn history(&self, _t: &str, _i: u32, _l: u32) -> Result<Value, FetchError> {
            Ok(self.body.clone())
        }
    }

    #[test]
    fn writes_partitioned_layers_dedupes_and_advances_cursor() {
        let v = temp_vault("store");
        let client = StubClient { body: sample_history() };

        let out = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&2));

        // Contract layer — two different months.
        let stream = v.stream(DIR, Partition::Month);
        let mut on_disk: Vec<MediaItem> = Vec::new();
        for key in stream.partitions().unwrap() {
            for it in stream.read::<MediaItem>(&key).unwrap() {
                assert_eq!(Partition::Month.key(&it.ts), Some(key.as_str()));
                on_disk.push(it);
            }
        }
        assert_eq!(on_disk.len(), 2, "two contract rows persisted");

        // Raw layer mirrors the partitioning under raw/.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0usize;
        let mut saw_daft_punk = false;
        for key in raw.partitions().unwrap() {
            for it in raw.read::<Value>(&key).unwrap() {
                raw_count += 1;
                if it.get("id").and_then(Value::as_i64) == Some(3135556) {
                    saw_daft_punk = true;
                    assert_eq!(it["artist"]["name"].as_str(), Some("Daft Punk"));
                    assert_eq!(it["timestamp"].as_i64(), Some(1748908800));
                }
            }
        }
        assert_eq!(raw_count, 2, "two raw rows");
        assert!(saw_daft_punk, "raw kept the full API object verbatim");

        // Cursor advanced to the max timestamp.
        let state = v.read_deezer_sync();
        assert_eq!(state.watermark, Some(1748908800));
        assert!(state.updated.is_some());

        // Re-run with the same input → guid dedupe, no duplicate rows.
        let client2 = StubClient { body: sample_history() };
        let again = pull_with(&v, &client2, "tok").unwrap();
        assert_eq!(again.counts.get("plays"), Some(&0), "all guids already stored");
        let mut after = 0usize;
        for key in stream.partitions().unwrap() {
            after += stream.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(after, 2, "no duplicate rows after re-run");

        // The unified media stream sees the plays via the contract arm.
        let day = v
            .media_timeline(
                &DateTime::from_timestamp(1748908800, 0)
                    .unwrap()
                    .with_timezone(&Local)
                    .format("%Y-%m-%d")
                    .to_string(),
            )
            .unwrap();
        let daft = day.iter().find(|i| i.title == "Harder, Better, Faster, Stronger");
        assert!(daft.is_some(), "the play joins the unified stream on its local day");
        assert_eq!(daft.unwrap().source, "deezer");
        assert_eq!(daft.unwrap().category, "music");
    }

    #[test]
    fn pull_without_connection_is_a_clean_error() {
        let v = temp_vault("unconnected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("not connected"),
            "clear error, no panic: {err}"
        );
    }

    #[test]
    fn expired_token_is_a_clean_error() {
        let v = temp_vault("expired");
        // Epoch 1 = already expired.
        let token = TokenSet {
            access_token: "tok".into(),
            refresh_token: None,
            token_type: Some("bearer".into()),
            scope: None,
            expires_at: Some(1),
        };
        v.save_sync_token(SERVICE, &token).unwrap();
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("expired") || err.contains("reconnect"),
            "clear error for expired token: {err}"
        );
    }

    #[test]
    fn unauthorized_response_drops_token() {
        struct AuthErrClient;
        impl HistoryFetch for AuthErrClient {
            fn history(&self, _t: &str, _i: u32, _l: u32) -> Result<Value, FetchError> {
                Err(FetchError::Unauthorized)
            }
        }
        let v = temp_vault("auth-err");
        let token = TokenSet {
            access_token: "bad-tok".into(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        };
        v.save_sync_token(SERVICE, &token).unwrap();
        let err = pull_with(&v, &AuthErrClient, "bad-tok").unwrap_err().to_string();
        assert!(err.contains("rejected") || err.contains("reconnect"), "{err}");
        // Token should be dropped so the card prompts reconnect.
        assert!(v.load_sync_token(SERVICE).unwrap().is_none(), "token cleared on 401");
    }

    #[test]
    fn parse_deezer_token_url_encoded() {
        let resp = "access_token=mytoken123&expires=3600&token_type=Bearer";
        let tok = parse_deezer_token(resp).unwrap();
        assert_eq!(tok.access_token, "mytoken123");
        assert!(tok.expires_at.is_some(), "expires_at computed from expires=3600");
        assert!(tok.refresh_token.is_none(), "Deezer issues no refresh token");
    }

    #[test]
    fn parse_deezer_token_json() {
        let resp = r#"{"access_token":"jsontoken","expires":7200}"#;
        let tok = parse_deezer_token(resp).unwrap();
        assert_eq!(tok.access_token, "jsontoken");
        assert!(tok.expires_at.is_some());
    }

    #[test]
    fn parse_deezer_token_missing_access_token_errors() {
        let resp = "expires=3600&token_type=Bearer";
        let err = parse_deezer_token(resp).unwrap_err().to_string();
        assert!(err.contains("access_token"), "clear error: {err}");
    }

    #[test]
    fn sync_state_back_compat() {
        let old: SyncState =
            serde_json::from_str(r#"{"watermark":1748908800}"#).unwrap();
        assert_eq!(old.watermark, Some(1748908800));
        assert!(old.updated.is_none());
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermark.is_none() && empty.updated.is_none());
    }

    #[test]
    fn connection_redirect_port_is_assigned() {
        assert_eq!(REDIRECT_PORT, 38789, "38580 + 209 = 38789");
    }

    #[test]
    fn urlencode_and_urldecode_round_trip() {
        let s = "hello world/foo&bar=baz";
        let encoded = urlencode(s);
        assert!(!encoded.contains(' '), "spaces encoded");
        assert!(!encoded.contains('/'), "slashes encoded");
        let decoded = urldecode(&encoded);
        assert_eq!(decoded, s);
    }

    #[test]
    fn connection_exposes_oauth_method_and_disconnect_forgets_token() {
        let v = temp_vault("conn");
        let token = TokenSet {
            access_token: "acc".into(),
            refresh_token: None,
            token_type: Some("bearer".into()),
            scope: None,
            // Non-expiring for this test.
            expires_at: None,
        };
        v.save_sync_token(SERVICE, &token).unwrap();
        let status = connect_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "deezer");
        assert!(!status.accounts[0].needs_reconnect, "non-expiring token ok");

        disconnect(&v, "deezer").unwrap();
        assert!(connect_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn expired_stored_token_flags_needs_reconnect() {
        let v = temp_vault("expired-status");
        let token = TokenSet {
            access_token: "old".into(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: Some(1), // epoch 1 = definitely expired
        };
        v.save_sync_token(SERVICE, &token).unwrap();
        let status = connect_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert!(status.accounts[0].needs_reconnect, "expired token flags reconnect");
    }
}
