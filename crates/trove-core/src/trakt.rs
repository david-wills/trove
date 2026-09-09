//! Trakt.tv — cloud movie/TV watch history via the OAuth API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/trakt.md.
//!
//! A **Periodic** cloud pull (M5): every play Trakt has recorded — a watch, a
//! scrobble, or a checkin, fed from Plex/Infuse/the apps and the Trakt site —
//! lands in the unified media stream via the **media-plays write contract**
//! (`docs/vault-spec/domains/media-plays.md`). Two layers per play, exactly
//! like [`crate::lastfm`] / [`crate::listenbrainz`]:
//!
//! - **raw** — the API history item verbatim at
//!   `media/plays/trakt/raw/YYYY-MM.jsonl`, partitioned by the play's month
//!   (full id/show fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/trakt/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! `GET /sync/history` returns watched items NEWEST-first, paginated by the
//! response headers `X-Pagination-Page` / `X-Pagination-Page-Count`. The first
//! sync backfills the whole history (page 1 → page count, no `start_at`); later
//! syncs pass `start_at=<watermark ISO>` and page through the new window. The
//! watermark is the max `watched_at` ever written, kept in a rebuildable cursor
//! at `.trove/trakt-sync.json` (non-secret state, beside the vault's other
//! `.trove/` indexes, not under `.trove/sync/`).
//!
//! Auth is OAuth 2.0 with PKCE — the user registers a free Trakt app (or a
//! build bakes one in) and logs in. Trakt issues a refresh token, so an
//! expired access token is *refreshed* in place (the [`crate::sync::google`]
//! precedent, single-account); only a refresh failure forces a reconnect.
//! Every request carries the `trakt-api-key` (the app's client id) header;
//! authenticated ones also `Authorization: Bearer <access_token>`.

use std::collections::{BTreeMap, HashSet};
use std::thread;
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
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

/// Contract-layer stream directory; raw lines go one level deeper in `raw/`.
const DIR: &str = "media/plays/trakt";
const RAW_DIR: &str = "media/plays/trakt/raw";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for
/// 0600 secrets); deleting it re-walks the whole history on the next sync.
const SYNC_FILE: &str = ".trove/trakt-sync.json";

/// The OAuth service slug — the `.trove/sync/` key for the app creds + token.
const SERVICE: &str = "trakt";

const API_BASE: &str = "https://api.trakt.tv";
/// Trakt allows up to 100 history items per page; 100 keeps each backfill
/// request light while minimizing round-trips.
const PAGE_SIZE: u32 = 100;
/// Small inter-request delay so a backfill stays well under any rate limit.
const REQ_INTERVAL: Duration = Duration::from_millis(250);
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Seconds between syncs in the watcher loop. Hourly — like the other media
/// scrobble hubs: plays trickle in and the incremental poll is one cheap
/// request when idle.
pub const TRAKT_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Provider + connection.

/// The Trakt OAuth provider. PKCE on; `basic_auth` off — Trakt takes
/// client_id + client_secret in the token-request *body* (which the shared
/// [`oauth`] form-POST machinery handles for `basic_auth: false`). No granular
/// scopes (Trakt grants the whole account on consent). Redirect port 38576
/// (38573/74/75 are taken by other connections).
pub static TRAKT: Provider = Provider {
    service: SERVICE,
    display_name: "Trakt",
    auth_url: "https://trakt.tv/oauth/authorize",
    token_url: "https://api.trakt.tv/oauth/token",
    scopes: "",
    redirect_port: 38576,
    use_pkce: true,
    basic_auth: false,
    // Bake credentials in at build time for a zero-setup "just log in"
    // experience: TROVE_TRAKT_CLIENT_ID / TROVE_TRAKT_CLIENT_SECRET.
    default_client_id: option_env!("TROVE_TRAKT_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_TRAKT_CLIENT_SECRET"),
    extra_auth_params: &[],
};

/// Registered in [`crate::integrations::CONNECTIONS`]. Single-login (no
/// per-account keying): re-connecting replaces the saved token.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "trakt",
    display_name: "Trakt",
    methods: &[ConnectMethod::OAuth {
        provider: &TRAKT,
        multi_account: false,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect: disconnect,
    auto_pull: &["trakt"],
    setup: &[
        "Register a free app at trakt.tv/oauth/applications/new (any name).",
        "Set its Redirect URI to http://localhost:38576/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved, so every future connect is just a login.",
    ],
};

/// [`ConnectMethod::OAuth`] adapter: forward to [`connect`] (which owns the
/// explicit → saved → compiled-in credential resolution) and drop the returned
/// token — callers re-read state through the status hook.
fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

/// Interactive connect: opens the consent page in the browser, waits for the
/// redirect, saves the token. Blocking — callers off the main thread only.
///
/// App credentials resolve in order: explicitly passed (first-time setup,
/// saved for next time) → previously saved → compiled-in defaults. After the
/// first connect, "reconnect" is therefore just a login.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(SERVICE, &c)?;
            c
        }
        None => vault
            .load_sync_app(SERVICE)?
            .or_else(|| TRAKT.default_credentials())
            .context("no Trakt app credentials — register an app and enter them once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&TRAKT, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(SERVICE, &token)?;
    Ok(token)
}

/// `configured` mirrors the other OAuth connections: app credentials saved or
/// compiled in, so connecting is just a login. At most one account. Because
/// Trakt issues refresh tokens, an expired access token is *not* a reconnect
/// signal here — it will be refreshed silently on the next pull; only a token
/// the store has already invalidated (none) is shown as needing reconnect.
/// Trakt access tokens last ~3 months, so a fresh login is rarely needed.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured =
        vault.load_sync_app(SERVICE)?.is_some() || TRAKT.default_credentials().is_some();
    let accounts = match vault.load_sync_token(SERVICE)? {
        Some(token) => {
            let mut extra = BTreeMap::new();
            if let Some(last_sync) = vault
                .sync_status()?
                .into_iter()
                .find(|s| s.service == SERVICE)
                .and_then(|s| s.last_sync)
            {
                extra.insert("last_sync", last_sync);
            }
            vec![ConnectedAccount {
                key: SERVICE.to_string(),
                label: TRAKT.display_name.to_string(),
                // Not stored anywhere — the token file's mtime would lie after
                // a re-save (refresh rewrites it).
                connected_at: None,
                expires_at: token.expires_at,
                // A live refresh token means expiry is self-healing; the only
                // reconnect signal is a token with no refresh token at all
                // (shouldn't happen — Trakt always issues one).
                needs_reconnect: token.expired() && token.refresh_token.is_none(),
                extra,
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

/// Forget the token; app credentials are kept so reconnecting is just a login.
/// Single-account, so the key is ignored.
fn disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

// ---------------------------------------------------------------------------
// Token freshness: refresh in place (Trakt issues refresh tokens), modeled on
// [`crate::sync::google::fresh_token`] but single-account.

/// Resolve the app credentials (saved → compiled-in), needed both to read the
/// client id for the `trakt-api-key` header and to refresh the token.
fn resolve_creds(vault: &Vault) -> Result<AppCredentials> {
    vault
        .load_sync_app(SERVICE)?
        .or_else(|| TRAKT.default_credentials())
        .context("no Trakt app credentials — reconnect from the Integrations tab")
}

/// The live access token, refreshed first if it is (about to be) expired. Trakt
/// DOES issue refresh tokens, so an expired token is refreshed and re-saved in
/// place; only a refresh *failure* bails with a reconnect message (the token is
/// dropped so the hub clearly shows a disconnected account, mirroring how
/// Google flags `needs_reconnect`). Returns the fresh token.
fn fresh_token(vault: &Vault, creds: &AppCredentials) -> Result<TokenSet> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Trakt is not connected — connect from the Integrations tab")?;
    if !token.expired() {
        return Ok(token);
    }
    match oauth::refresh_token(&TRAKT, creds, &token) {
        Ok(new) => {
            vault.save_sync_token(SERVICE, &new)?;
            Ok(new)
        }
        Err(e) => {
            // The refresh token is dead (revoked / too old). Drop the token so
            // the connection card shows a clean "reconnect" rather than a stale
            // account that keeps failing.
            vault.delete_sync_token(SERVICE)?;
            Err(e).context("Trakt token refresh failed — reconnect from the Integrations tab")
        }
    }
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull the manual "Sync now" runs, but it never errors
// the loop — a not-connected state or a network blip is just a quiet no-op
// until the next tick.
fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("plays").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("trakt synced — {n} plays")
            }))
        }
        // Not connected / transient network: stay silent, retry next tick. A
        // real bug still surfaces in the log via the message.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "trakt sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected, refresh failed) to the
// user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let plays = out.counts.get("plays").copied().unwrap_or(0);
    let headline = if plays == 0 {
        "Trakt is up to date — no new plays".to_string()
    } else {
        format!("Trakt synced — {plays} plays")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "trakt",
        name: "Trakt",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Sync your full movie and TV watch history from Trakt into the \
             unified media stream. A universal scrobble hub — Plex, Infuse, and \
             many players feed here automatically. First sync backfills your \
             whole history; later syncs fetch only what's new.",
        domain: "media",
        vault_path: "media/plays/trakt/",
        toggleable: true,
        setup: &[
            "Connect your Trakt account on this card (register a free Trakt app first — see the connect steps).",
            "First sync backfills your entire watch history (paginated); later syncs are incremental.",
        ],
        caveats: "Free and VIP accounts are capped at 100k history items; older plays beyond that \
                  cap aren't returned by the API. Trakt records watch *events*, not durations, so \
                  every play has seconds=0. IDs cross-reference Trakt, IMDB, TMDB, and TVDB. \
                  Connecting uses OAuth (a public-profile fallback isn't supported yet).",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(TRAKT_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("trakt"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// One page of history plus the pagination header Trakt returns.
struct HistoryPage {
    items: Vec<Value>,
    /// `X-Pagination-Page-Count` — total pages for the current query. 0/absent
    /// is treated as "this is the only page" by the caller.
    page_count: u32,
}

/// Status-level fetch errors. 401 (token rejected) and 429 (rate limited) want
/// distinct handling; everything else is a message.
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

/// One page of `GET /sync/history`. Tests implement this against fixtures;
/// production hits the real API.
trait History {
    /// `start_at` is the incremental lower bound (ISO8601 UTC; the API filters
    /// to plays at/after it), `None` on a first-run backfill. `page` is 1-based.
    fn history(
        &self,
        access_token: &str,
        page: u32,
        limit: u32,
        start_at: Option<&str>,
    ) -> Result<HistoryPage, FetchError>;
}

/// Thin client. The base URL is injected so the sync logic stays testable
/// against a local stub (the `lastfm.rs` / `listenbrainz.rs` pattern). Carries
/// the app's client id for the mandatory `trakt-api-key` header.
struct TraktClient {
    base: String,
    client_id: String,
}

impl TraktClient {
    fn new(base: String, client_id: String) -> Self {
        TraktClient { base, client_id }
    }
}

impl History for TraktClient {
    fn history(
        &self,
        access_token: &str,
        page: u32,
        limit: u32,
        start_at: Option<&str>,
    ) -> Result<HistoryPage, FetchError> {
        let mut req = ureq::get(&format!("{}/sync/history", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/json")
            .set("trakt-api-version", "2")
            .set("trakt-api-key", &self.client_id)
            .set("Authorization", &format!("Bearer {access_token}"))
            .query("page", &page.to_string())
            .query("limit", &limit.to_string());
        if let Some(start_at) = start_at {
            req = req.query("start_at", start_at);
        }
        match req.call() {
            Ok(resp) => {
                let page_count = resp
                    .header("X-Pagination-Page-Count")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(0);
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                let items = v.as_array().cloned().unwrap_or_default();
                Ok(HistoryPage { items, page_count })
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
    /// Max `watched_at` (ISO8601 UTC, as returned by Trakt) ever written. The
    /// next incremental poll asks for `start_at = watermark`; the guid dedupe
    /// drops the boundary item that the inclusive filter re-includes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_trakt_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_trakt_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Parse one history page (a JSON array) into (contract rows, raw items). Each
/// entry maps to one [`MediaItem`]; entries without `id`/`watched_at` or a
/// resolvable title (not a real play) are skipped from both layers.
fn parse_history(items: &[Value]) -> (Vec<MediaItem>, Vec<Value>) {
    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for it in items {
        let Some(item) = history_item(it) else {
            continue;
        };
        rows.push(item);
        raws.push(it.clone());
    }
    (rows, raws)
}

/// One history entry → a media-plays contract row. `None` if it lacks the
/// fields a play must have (`id`, `watched_at`, a resolvable title).
///
/// Movies and episodes share `id`/`watched_at`/`action` but differ in shape:
/// - movie:   `movie:{title,year,ids:{trakt,slug,imdb,tmdb}}`
/// - episode: `episode:{season,number,title,ids:{trakt,tvdb,imdb,tmdb}}` AND
///            `show:{title,year,ids:{trakt,slug,tvdb,imdb,tmdb}}`
fn history_item(it: &Value) -> Option<MediaItem> {
    let id = it.get("id").and_then(Value::as_i64)?;
    let watched_at = it.get("watched_at").and_then(Value::as_str)?.trim();
    if watched_at.is_empty() {
        return None;
    }
    let item_type = it.get("type").and_then(Value::as_str).unwrap_or("");
    let action = it.get("action").and_then(Value::as_str).unwrap_or("");

    // watched_at is ISO8601 UTC → RFC3339 with the local offset. Parse to an
    // instant first, then render via the same local helper every collector uses
    // (no hand-rolled tz math).
    let ts = DateTime::parse_from_rfc3339(watched_at)
        .ok()?
        .with_timezone(&Local)
        .to_rfc3339();

    let guid = format!("trakt-{id}");

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    };
    put("action", action);

    // Contract fields differ by type. Movies: title + "(year)" subtitle, no
    // detail. Episodes: episode title + show subtitle + "SxxExx" detail.
    let (title, subtitle, detail) = match item_type {
        "episode" => {
            let episode = it.get("episode")?;
            let show = it.get("show");
            let ep_title = str_at(episode, "title");
            let show_title = show.map(|s| str_at(s, "title")).unwrap_or_default();
            let season = episode.get("season").and_then(Value::as_i64);
            let number = episode.get("number").and_then(Value::as_i64);
            let detail = match (season, number) {
                (Some(s), Some(n)) => format!("S{s:02}E{n:02}"),
                _ => String::new(),
            };
            // Episode-level ids.
            put("trakt_id", &id_at(episode, "trakt"));
            put("imdb_id", &str_at_ids(episode, "imdb"));
            put("tmdb_id", &id_at(episode, "tmdb"));
            put("tvdb_id", &id_at(episode, "tvdb"));
            // Show-level ids — useful for grouping a series across episodes.
            if let Some(show) = show {
                put("show_trakt_id", &id_at(show, "trakt"));
                put("show_imdb_id", &str_at_ids(show, "imdb"));
            }
            if let Some(s) = season {
                put("season", &s.to_string());
            }
            if let Some(n) = number {
                put("number", &n.to_string());
            }
            // Title falls back to the SxxExx (or show title) when the episode
            // has no title (Trakt occasionally omits it for fresh airings).
            let title = if !ep_title.is_empty() {
                ep_title
            } else if !detail.is_empty() {
                detail.clone()
            } else {
                show_title.clone()
            };
            if title.is_empty() && show_title.is_empty() {
                return None;
            }
            (title, show_title, detail)
        }
        _ => {
            // Treat anything else as a movie (type "movie"); the `movie` object
            // is the authority. Bail if it's missing (not a parseable play).
            let movie = it.get("movie")?;
            let m_title = str_at(movie, "title");
            if m_title.is_empty() {
                return None;
            }
            let year = movie.get("year").and_then(Value::as_i64);
            // Subtitle = "Title (Year)" — the chart grouping key — or just the
            // title when the year is unknown.
            let subtitle = match year {
                Some(y) => format!("{m_title} ({y})"),
                None => m_title.clone(),
            };
            put("trakt_id", &id_at(movie, "trakt"));
            put("imdb_id", &str_at_ids(movie, "imdb"));
            put("tmdb_id", &id_at(movie, "tmdb"));
            put("tvdb_id", &id_at(movie, "tvdb"));
            // detail omitted for movies (per the contract).
            (m_title, subtitle, String::new())
        }
    };

    Some(MediaItem {
        ts,
        source: "trakt".into(),
        category: "video".into(),
        device: String::new(),
        kind: "play".into(),
        title,
        subtitle,
        detail,
        // Trakt records events, not durations — an honest unknown.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

/// A string field of an object, trimmed; "" when missing or non-string.
fn str_at(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// A string id under `obj.ids.<key>` (e.g. the imdb id, "tt0133093"), trimmed.
fn str_at_ids(v: &Value, key: &str) -> String {
    v.get("ids")
        .and_then(|ids| ids.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// A numeric id under `obj.ids.<key>` (trakt/tmdb/tvdb are integers),
/// stringified; "" when missing. Tolerates a string id too, defensively.
fn id_at(v: &Value, key: &str) -> String {
    match v.get("ids").and_then(|ids| ids.get(key)) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

/// The `watched_at` (ISO8601 string) of a raw history item. Used to compute the
/// watermark and partition the raw line.
fn watched_at_of(it: &Value) -> Option<String> {
    it.get("watched_at")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// The pull.

/// Outcome of writing one batch of parsed plays.
struct WriteStats {
    plays: u64,
    /// Max `watched_at` (ISO8601) written — drives the forward-only watermark
    /// advance. ISO8601 UTC strings sort lexically, so the string max is the
    /// latest instant.
    max_watched_at: Option<String>,
}

/// A raw history item carrying the contract ts purely so the month-partition
/// writer files it under the play's month. Only `value` is serialized to disk —
/// flattened, so the raw line is the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Pick the lexically-greater of an accumulator and a candidate ISO8601 string.
fn max_iso(acc: Option<String>, cand: String) -> Option<String> {
    match acc {
        Some(m) if m >= cand => Some(m),
        _ => Some(cand),
    }
}

/// Write raw + contract rows, deduped by guid against what's already on disk,
/// and report the count written plus the max `watched_at` seen. Raw lines
/// partition by the same month as their contract row (the play's local month).
fn write_rows(vault: &Vault, rows: &[MediaItem], raws: &[Value]) -> Result<WriteStats> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids — re-runnable: a re-pull of overlapping pages never
    // duplicates. (The store appends; dedupe is the domain's job — the
    // letterboxd/lastfm pattern.)
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
    let mut max_watched_at: Option<String> = None;
    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        // The watermark tracks the API's `watched_at` (UTC ISO), not the
        // local-offset contract ts, so the max compares like-for-like and the
        // `start_at` we send back is in the frame the API expects.
        if let Some(w) = watched_at_of(raw_val) {
            max_watched_at = max_iso(max_watched_at, w);
        }
        if !seen.insert(item.guid.clone()) {
            continue; // already stored
        }
        new_rows.push(item.clone());
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;

    Ok(WriteStats { plays: new_rows.len() as u64, max_watched_at })
}

/// Resolve credentials + a fresh token and sync. First run (no watermark):
/// backfill page 1 → page count, no `start_at`. Otherwise: incremental from
/// `start_at = watermark`, paging through the window. Returns a generic
/// [`PullOutcome`] with a `plays` count.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let creds = resolve_creds(vault)?;
    let token = fresh_token(vault, &creds)?;
    let client = TraktClient::new(API_BASE.to_string(), creds.client_id.clone());
    pull_with(vault, &client, &token.access_token)
}

/// One page fetch with a single rate-limit back-off-and-retry. Mirrors the
/// lastfm/listenbrainz 429 handling.
fn fetch_page(
    client: &impl History,
    access_token: &str,
    page: u32,
    start_at: Option<&str>,
) -> Result<HistoryPage> {
    match client.history(access_token, page, PAGE_SIZE, start_at) {
        Ok(b) => Ok(b),
        Err(e @ FetchError::Unauthorized) => bail!("Trakt rejected the request: {e}"),
        Err(FetchError::RateLimited) => {
            // Back off once and retry the same page. The watermark only advances
            // after the loop fully drains, so the next tick re-drains the gap
            // from the same watermark (guid dedupe skips what already landed) —
            // no plays are lost.
            thread::sleep(Duration::from_secs(2));
            client
                .history(access_token, page, PAGE_SIZE, start_at)
                .map_err(|e| anyhow::anyhow!("Trakt rate limited: {e}"))
        }
        Err(e) => bail!("Trakt fetch failed: {e}"),
    }
}

/// The pull body over an injected fetcher — the testable seam.
///
/// History comes back NEWEST-first; pagination is by the `X-Pagination-Page` /
/// `X-Pagination-Page-Count` headers. We drain ALL pages of the current window
/// (page 1 → page count) BEFORE advancing the watermark — the listenbrainz
/// incremental bug was advancing after a single page and stranding the rest of
/// the gap, so the whole window is consumed here. The guid dedupe in
/// [`write_rows`] is the backstop for the watermark-boundary item that an
/// inclusive `start_at` re-includes.
fn pull_with(vault: &Vault, client: &impl History, access_token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_trakt_sync();
    let start_at = state.watermark.clone();

    let mut total_written: u64 = 0;
    let mut max_overall = state.watermark.clone();

    let mut page: u32 = 1;
    loop {
        let body = fetch_page(client, access_token, page, start_at.as_deref())?;
        let (rows, raws) = parse_history(&body.items);
        let stats = write_rows(vault, &rows, &raws)?;
        total_written += stats.plays;
        if let Some(w) = stats.max_watched_at {
            max_overall = max_iso(max_overall, w);
        }

        // Drain the whole window: stop only when we've covered every page the
        // header reports (or the page came back empty — a guard against a
        // missing/zero header so a malformed response can't loop forever).
        let page_count = body.page_count.max(1);
        if page >= page_count || body.items.is_empty() {
            break;
        }
        page += 1;
        thread::sleep(REQ_INTERVAL);
    }

    // Advance the watermark to the max watched_at seen (forward-only; ISO8601
    // UTC strings sort lexically so string comparison is instant comparison).
    if let Some(w) = max_overall {
        if state.watermark.as_deref().is_none_or(|cur| w.as_str() > cur) {
            state.watermark = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_trakt_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_written} plays"),
        counts: BTreeMap::from([("plays", total_written)]),
    })
}

// TODO(trakt P2): curation snapshots — GET /sync/ratings, /sync/watchlist,
// /sync/collection/movies + /collection/shows → overwrite (replace-on-sync)
// media/trakt/ratings.jsonl, media/trakt/watchlist.jsonl,
// media/trakt/collection.jsonl (one full-fidelity item per line, NOT
// media-plays, no contract). Authenticated (bearer + trakt headers). Deferred
// to keep P1 (history → media-plays + OAuth + refresh) bulletproof within
// budget; see the return note.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-trakt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A real `GET /sync/history` page (a JSON array): one MOVIE item and one
    /// EPISODE item, newest-first, spanning two months. Shapes follow the
    /// documented Trakt v2 API.
    fn sample_page() -> Vec<Value> {
        serde_json::from_value(serde_json::json!([
            {
                "id": 982910,
                "watched_at": "2026-06-10T20:00:00.000Z",
                "action": "scrobble",
                "type": "movie",
                "movie": {
                    "title": "The Matrix",
                    "year": 1999,
                    "ids": { "trakt": 481, "slug": "the-matrix-1999", "imdb": "tt0133093", "tmdb": 603 }
                }
            },
            {
                "id": 982909,
                "watched_at": "2026-05-30T03:15:00.000Z",
                "action": "watch",
                "type": "episode",
                "episode": {
                    "season": 1,
                    "number": 2,
                    "title": "The Kingsroad",
                    "ids": { "trakt": 73641, "tvdb": 3254641, "imdb": "tt1668746", "tmdb": 63057 }
                },
                "show": {
                    "title": "Game of Thrones",
                    "year": 2011,
                    "ids": { "trakt": 1390, "slug": "game-of-thrones", "tvdb": 121361, "imdb": "tt0944947", "tmdb": 1399 }
                }
            }
        ]))
        .unwrap()
    }

    #[test]
    fn parses_movie_and_episode_items() {
        let (rows, raws) = parse_history(&sample_page());
        assert_eq!(rows.len(), 2);
        assert_eq!(raws.len(), 2);

        // MOVIE: title = movie title, subtitle = "Title (Year)", no detail.
        let movie = &rows[0];
        assert_eq!(movie.title, "The Matrix");
        assert_eq!(movie.subtitle, "The Matrix (1999)");
        assert_eq!(movie.detail, "", "movies carry no SxxExx detail");
        assert_eq!(movie.source, "trakt");
        assert_eq!(movie.category, "video");
        assert_eq!(movie.kind, "play");
        assert_eq!(movie.seconds, 0, "events not durations");
        assert_eq!(movie.guid, "trakt-982910");
        // watched_at → ts carrying the local offset (the same instant).
        assert_eq!(
            DateTime::parse_from_rfc3339(&movie.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T20:00:00.000Z").unwrap().timestamp()
        );
        assert_eq!(movie.extra.get("action"), Some(&Value::String("scrobble".into())));
        assert_eq!(movie.extra.get("trakt_id"), Some(&Value::String("481".into())));
        assert_eq!(movie.extra.get("imdb_id"), Some(&Value::String("tt0133093".into())));
        assert_eq!(movie.extra.get("tmdb_id"), Some(&Value::String("603".into())));
        assert!(movie.extra.get("tvdb_id").is_none(), "movie has no tvdb id");
        assert!(movie.extra.get("season").is_none(), "no episode fields on a movie");

        // EPISODE: title = episode title, subtitle = show title, detail = SxxExx.
        let ep = &rows[1];
        assert_eq!(ep.title, "The Kingsroad");
        assert_eq!(ep.subtitle, "Game of Thrones");
        assert_eq!(ep.detail, "S01E02");
        assert_eq!(ep.source, "trakt");
        assert_eq!(ep.category, "video");
        assert_eq!(ep.kind, "play");
        assert_eq!(ep.seconds, 0);
        assert_eq!(ep.guid, "trakt-982909");
        assert_eq!(ep.extra.get("action"), Some(&Value::String("watch".into())));
        // Episode-level ids.
        assert_eq!(ep.extra.get("trakt_id"), Some(&Value::String("73641".into())));
        assert_eq!(ep.extra.get("imdb_id"), Some(&Value::String("tt1668746".into())));
        assert_eq!(ep.extra.get("tmdb_id"), Some(&Value::String("63057".into())));
        assert_eq!(ep.extra.get("tvdb_id"), Some(&Value::String("3254641".into())));
        // Show-level ids for series grouping.
        assert_eq!(ep.extra.get("show_trakt_id"), Some(&Value::String("1390".into())));
        assert_eq!(ep.extra.get("show_imdb_id"), Some(&Value::String("tt0944947".into())));
        // Season + number as flat strings.
        assert_eq!(ep.extra.get("season"), Some(&Value::String("1".into())));
        assert_eq!(ep.extra.get("number"), Some(&Value::String("2".into())));
    }

    #[test]
    fn movie_without_year_uses_bare_title_subtitle() {
        let page: Vec<Value> = serde_json::from_value(serde_json::json!([
            {
                "id": 1,
                "watched_at": "2026-06-01T00:00:00.000Z",
                "action": "checkin",
                "type": "movie",
                "movie": { "title": "Untitled Doc", "ids": { "trakt": 9 } }
            }
        ]))
        .unwrap();
        let (rows, _) = parse_history(&page);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "Untitled Doc");
        assert_eq!(rows[0].subtitle, "Untitled Doc", "no year → bare title");
        assert_eq!(rows[0].detail, "");
        assert_eq!(rows[0].extra.get("trakt_id"), Some(&Value::String("9".into())));
        assert!(rows[0].extra.get("imdb_id").is_none(), "missing ids omitted, not empty");
    }

    /// A one-page-then-empty fetcher honoring the page-count header: returns the
    /// fixed page on call 1 (page_count=1), an empty page after. Enough for the
    /// store/cursor + re-run dedupe test.
    struct StubClient {
        page: Vec<Value>,
        calls: std::cell::RefCell<u32>,
    }
    impl StubClient {
        fn new(page: Vec<Value>) -> Self {
            StubClient { page, calls: std::cell::RefCell::new(0) }
        }
    }
    impl History for StubClient {
        fn history(
            &self,
            _access_token: &str,
            _page: u32,
            _limit: u32,
            _start_at: Option<&str>,
        ) -> Result<HistoryPage, FetchError> {
            let mut c = self.calls.borrow_mut();
            *c += 1;
            if *c == 1 {
                Ok(HistoryPage { items: self.page.clone(), page_count: 1 })
            } else {
                Ok(HistoryPage { items: Vec::new(), page_count: 1 })
            }
        }
    }

    /// The local calendar day of an ISO8601 UTC instant — so timeline/partition
    /// assertions stay timezone-agnostic (CI may run in any zone).
    fn local_day(iso_utc: &str) -> String {
        DateTime::parse_from_rfc3339(iso_utc)
            .unwrap()
            .with_timezone(&Local)
            .format("%Y-%m-%d")
            .to_string()
    }

    #[test]
    fn writes_partitioned_layers_dedupes_and_advances_cursor() {
        let v = temp_vault("store");
        let client = StubClient::new(sample_page());

        let out = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&2));

        // Contract layer, partitioned by the play's LOCAL month. Assert each row
        // sits in the file named by its own ts month rather than hard-coding a
        // zone (the fixture instants are mid-day/early-morning UTC).
        let stream = v.stream(DIR, Partition::Month);
        let mut on_disk: Vec<MediaItem> = Vec::new();
        for key in stream.partitions().unwrap() {
            for it in stream.read::<MediaItem>(&key).unwrap() {
                assert_eq!(Partition::Month.key(&it.ts), Some(key.as_str()));
                on_disk.push(it);
            }
        }
        assert_eq!(on_disk.len(), 2, "two contract rows persisted");

        // Raw layer mirrors the partitioning under raw/, verbatim objects.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0usize;
        let mut saw_matrix = false;
        for key in raw.partitions().unwrap() {
            for it in raw.read::<Value>(&key).unwrap() {
                raw_count += 1;
                if it.get("movie").and_then(|m| m.get("title")).and_then(Value::as_str)
                    == Some("The Matrix")
                {
                    saw_matrix = true;
                    // Raw is the API object verbatim — ids preserved.
                    assert_eq!(it["movie"]["ids"]["imdb"].as_str(), Some("tt0133093"));
                }
            }
        }
        assert_eq!(raw_count, 2);
        assert!(saw_matrix, "raw kept the full movie object verbatim");

        // Cursor advanced to the max watched_at written (the newest, the movie).
        let state = v.read_trakt_sync();
        assert_eq!(state.watermark.as_deref(), Some("2026-06-10T20:00:00.000Z"));
        assert!(state.updated.is_some());

        // Re-run with the same input → guid dedupe, no duplicate contract rows.
        let client2 = StubClient::new(sample_page());
        let again = pull_with(&v, &client2, "tok").unwrap();
        assert_eq!(again.counts.get("plays"), Some(&0), "all guids already stored");
        let mut after = 0usize;
        for key in stream.partitions().unwrap() {
            after += stream.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(after, 2, "no duplicate rows after re-run");

        // And the unified media stream sees the play via the contract arm.
        let day = v.media_timeline(&local_day("2026-06-10T20:00:00.000Z")).unwrap();
        let matrix = day.iter().find(|i| i.title == "The Matrix");
        assert!(matrix.is_some(), "the movie joins the unified stream on its local day");
        assert_eq!(matrix.unwrap().source, "trakt");
        assert_eq!(matrix.unwrap().category, "video");
    }

    /// A fetcher that serves a pool of plays NEWEST-first in pages of
    /// [`PAGE_SIZE`], reporting the real page count — so a window larger than
    /// one page is actually paged through, exactly like the API. Ignores
    /// `start_at` (the test seeds the watermark to prove the WHOLE window
    /// drains, guarding the listenbrainz single-page bug).
    struct PagingStubClient {
        /// All items, newest-first.
        all_desc: Vec<Value>,
    }
    impl PagingStubClient {
        /// `count` plays at one-minute intervals ending at `2026-06-15T00:00Z`,
        /// newest-first. Distinct ids and timestamps so each lands as a unique
        /// guid; staying within a single day keeps the dates valid.
        fn new(count: usize) -> Self {
            let base = DateTime::parse_from_rfc3339("2026-06-15T00:00:00.000Z").unwrap();
            let mut all: Vec<Value> = (0..count)
                .map(|i| {
                    // i=0 is the newest (id = count), older as i grows.
                    let id = (count - i) as i64;
                    let watched = base - chrono::Duration::minutes(i as i64);
                    let iso = watched.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
                    serde_json::json!({
                        "id": id,
                        "watched_at": iso,
                        "action": "scrobble",
                        "type": "movie",
                        "movie": { "title": format!("Movie {id}"), "year": 2020, "ids": { "trakt": id } }
                    })
                })
                .collect();
            // Newest-first by construction; sort explicitly for clarity.
            all.sort_by(|a, b| {
                b["watched_at"].as_str().unwrap().cmp(a["watched_at"].as_str().unwrap())
            });
            PagingStubClient { all_desc: all }
        }
    }
    impl History for PagingStubClient {
        fn history(
            &self,
            _access_token: &str,
            page: u32,
            limit: u32,
            _start_at: Option<&str>,
        ) -> Result<HistoryPage, FetchError> {
            let limit = limit as usize;
            let page_count = self.all_desc.len().div_ceil(limit).max(1) as u32;
            let start = (page as usize - 1) * limit;
            let items =
                self.all_desc.iter().skip(start).take(limit).cloned().collect::<Vec<_>>();
            Ok(HistoryPage { items, page_count })
        }
    }

    #[test]
    fn incremental_drains_a_multi_page_window_without_stranding_plays() {
        // Regression guard for the listenbrainz single-page defect: with a
        // watermark already set, the pull must drain EVERY page of the window,
        // not just page 1. 250 plays = 2.5 pages of 100.
        let v = temp_vault("gap");
        // Seed a watermark so this is the INCREMENTAL path (start_at sent).
        v.write_trakt_sync(&SyncState {
            watermark: Some("2026-06-01T00:00:00.000Z".into()),
            updated: Some("2026-06-01T00:00:00+00:00".into()),
        })
        .unwrap();

        let client = PagingStubClient::new(250);
        let out = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(
            out.counts.get("plays"),
            Some(&250),
            "the whole window must drain across all pages, not just the first"
        );

        // Count contract rows actually on disk across every partition.
        let stream = v.stream(DIR, Partition::Month);
        let mut on_disk = 0usize;
        for key in stream.partitions().unwrap() {
            on_disk += stream.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(on_disk, 250, "250 contract rows persisted, no gaps");

        // Watermark advanced forward-only to the newest play (the lexical max).
        let newest = client
            .all_desc
            .iter()
            .map(|v| v["watched_at"].as_str().unwrap())
            .max()
            .unwrap();
        let state = v.read_trakt_sync();
        assert_eq!(state.watermark.as_deref(), Some(newest));

        // Idempotent: a second pull finds nothing new (guid dedupe + watermark).
        let again = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(again.counts.get("plays"), Some(&0), "no re-write past the boundary");
    }

    // -----------------------------------------------------------------------
    // Connection / token tests — all offline (no network, no real creds).

    fn token(expires_at: Option<u64>, refresh: Option<&str>) -> TokenSet {
        TokenSet {
            access_token: "acc".into(),
            refresh_token: refresh.map(str::to_string),
            token_type: Some("bearer".into()),
            scope: Some(String::new()),
            expires_at,
        }
    }

    #[test]
    fn oauth_method_exposed_and_status_maps_token_to_one_account() {
        assert!(CONNECTION.method("oauth").is_some(), "OAuth method exposed");
        let v = temp_vault("status");
        // No token → no accounts; configured depends only on baked creds here.
        let s = connect_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        let baked = option_env!("TROVE_TRAKT_CLIENT_ID").is_some();
        assert_eq!(s.configured, baked);

        // Saving app creds makes it "configured" regardless of bake.
        v.save_sync_app(
            SERVICE,
            &AppCredentials { client_id: "cid".into(), client_secret: Some("secret".into()) },
        )
        .unwrap();
        // A live (far-future) token maps to exactly one account.
        v.save_sync_token(SERVICE, &token(Some(1_900_000_000), Some("ref"))).unwrap();
        let s = connect_status(&v).unwrap();
        assert!(s.configured);
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "trakt");
        assert_eq!(a.label, "Trakt");
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect, "live token, no reconnect");
    }

    #[test]
    fn expired_token_with_refresh_token_does_not_flag_reconnect() {
        // Trakt issues refresh tokens, so an expired access token is
        // self-healing — the status must NOT show reconnect (it refreshes on
        // the next pull). Contrast ticktick, which has no refresh token.
        let v = temp_vault("expired-refreshable");
        v.save_sync_token(SERVICE, &token(Some(1_000), Some("ref"))).unwrap();
        let s = connect_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert!(!s.accounts[0].needs_reconnect, "refreshable expiry isn't a reconnect");
    }

    #[test]
    fn disconnect_forgets_token_keeps_app_creds() {
        let v = temp_vault("disconnect");
        v.save_sync_app(
            SERVICE,
            &AppCredentials { client_id: "cid".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
        v.save_sync_token(SERVICE, &token(Some(1_900_000_000), Some("ref"))).unwrap();
        disconnect(&v, "trakt").unwrap();
        assert!(connect_status(&v).unwrap().accounts.is_empty());
        assert!(connect_status(&v).unwrap().configured, "app creds survive disconnect");
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
        assert!(v.load_sync_app(SERVICE).unwrap().is_some());
    }

    #[test]
    fn fresh_token_returns_live_token_unchanged() {
        let v = temp_vault("fresh-live");
        let creds = AppCredentials { client_id: "cid".into(), client_secret: Some("s".into()) };
        v.save_sync_token(SERVICE, &token(Some(1_900_000_000), Some("ref"))).unwrap();
        let t = fresh_token(&v, &creds).unwrap();
        assert_eq!(t.access_token, "acc", "live token passed through, no refresh");
        // …and the saved token is untouched (no spurious refresh write).
        assert!(v.load_sync_token(SERVICE).unwrap().is_some());
    }

    #[test]
    fn fresh_token_without_connection_is_a_clean_error() {
        let v = temp_vault("fresh-unconnected");
        let creds = AppCredentials { client_id: "cid".into(), client_secret: Some("s".into()) };
        let err = fresh_token(&v, &creds).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    // The pull's refresh goes through `oauth::refresh_token`, which we don't
    // stub (no touching oauth.rs). So we exercise the refresh *decision* via
    // `fresh_token` with an EXPIRED token whose refresh fails offline (no
    // network → an Err): the failure path must drop the token and bail
    // "reconnect". The success path is covered structurally — a live token is
    // never refreshed (`fresh_token_returns_live_token_unchanged`) — plus the
    // google::fresh_token precedent for the save-on-success half.
    #[test]
    fn expired_token_refresh_failure_drops_token_and_bails_reconnect() {
        let v = temp_vault("refresh-fail");
        let creds = AppCredentials { client_id: "cid".into(), client_secret: Some("s".into()) };
        // Expired token WITH a refresh token → fresh_token attempts a refresh;
        // offline that errors, so it must drop the token and bail.
        v.save_sync_token(SERVICE, &token(Some(1_000), Some("dead-refresh"))).unwrap();
        let err = fresh_token(&v, &creds).unwrap_err().to_string();
        assert!(err.contains("reconnect"), "refresh failure bails reconnect: {err}");
        assert!(
            v.load_sync_token(SERVICE).unwrap().is_none(),
            "failed refresh drops the dead token so the card shows reconnect"
        );
    }

    #[test]
    fn pull_without_connection_is_a_clean_error() {
        let v = temp_vault("pull-unconnected");
        // No app creds and (in CI) no baked creds → clear error, no panic. If
        // creds ARE baked, the error is instead "not connected" (no token).
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("app credentials") || err.contains("not connected"),
            "clear error, no panic: {err}"
        );
    }

    #[test]
    fn sync_state_back_compat() {
        // An older cursor line (watermark only, no `updated`) must still
        // deserialize — and a bare `{}` (fresh) too.
        let old: SyncState =
            serde_json::from_str(r#"{"watermark":"2026-06-10T20:00:00.000Z"}"#).unwrap();
        assert_eq!(old.watermark.as_deref(), Some("2026-06-10T20:00:00.000Z"));
        assert!(old.updated.is_none());
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermark.is_none() && empty.updated.is_none());
    }
}
