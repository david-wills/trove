//! Simkl — TV, anime, and movie watch tracking via the OAuth API (the Trakt
//! alternative, with the best anime ID mapping). Brief: docs/integrations/simkl.md.
//!
//! A **Periodic** cloud pull (M5): every movie, episode, and anime episode
//! Simkl has recorded as watched lands in the unified media stream via the
//! **media-plays write contract** (`docs/vault-spec/domains/media-plays.md`).
//! Two layers per source, exactly like [`crate::trakt`] / [`crate::lastfm`]:
//!
//! - **raw** — the API watched-list item verbatim at
//!   `media/plays/simkl/raw/YYYY-MM.jsonl`, partitioned by the watch's month
//!   (full id/season fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] per watch at
//!   `media/plays/simkl/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! Unlike Trakt's flat `/sync/history` event stream, Simkl's
//! `GET /sync/all-items/` returns the user's whole watched-*list* as
//! `{movies:[…], shows:[…], anime:[…]}`. Each entry carries a wrapper-level
//! `last_watched_at`; with `extended=full&episode_watched_at=yes` shows and
//! anime also carry per-episode `seasons[].episodes[].watched_at`. We map:
//! - **movie** → one [`MediaItem`] at the movie's `last_watched_at`.
//! - **show / anime** → one [`MediaItem`] *per watched episode* (its own
//!   `watched_at`); when no per-episode time is present, one [`MediaItem`] at
//!   the series `last_watched_at` (a degraded but honest fallback).
//! Items with no watch evidence at all (plan-to-watch / hold, `last_watched_at`
//! null, no episode times) are skipped — they aren't plays.
//!
//! The first sync backfills the whole list (no `date_from`); later syncs pass
//! `date_from=<watermark>` (the API returns only items changed since). The
//! watermark is the max `watched_at` ever written, kept in a rebuildable cursor
//! at `.trove/simkl-sync.json` (non-secret state).
//!
//! Auth is OAuth 2.0 with PKCE as a **public client** — Simkl issues no client
//! secret and the token *never expires* (response is
//! `{access_token, token_type:"bearer", scope:"public"}`, no refresh token), so
//! there is no refresh path: only an explicit reconnect (or a server-revoked
//! token, caught as a 401) replaces the token. Every request carries the
//! `simkl-api-key` (the app's client id) header plus `Authorization: Bearer`.

use std::collections::{BTreeMap, HashSet};
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
const DIR: &str = "media/plays/simkl";
const RAW_DIR: &str = "media/plays/simkl/raw";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for
/// 0600 secrets); deleting it re-walks the whole watched list on the next sync.
const SYNC_FILE: &str = ".trove/simkl-sync.json";

/// The OAuth service slug — the `.trove/sync/` key for the app creds + token.
const SERVICE: &str = "simkl";

const API_BASE: &str = "https://api.simkl.org";
/// Kept generous so the (potentially multi-MB) full backfill can complete, but
/// bounded so a hung connection can't stall the watcher owner loop forever.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs in the watcher loop. Hourly — like the other media
/// scrobble hubs: plays trickle in and the incremental poll is one cheap
/// `date_from` request when idle.
pub const SIMKL_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Provider + connection.

/// The Simkl OAuth provider. PKCE on; `basic_auth` off. Simkl is a **public
/// client** — `default_client_secret: None`, so the shared [`oauth`] machinery
/// sends `client_id` + `code_verifier` (and no secret) on the token request.
/// No granular scopes (Simkl grants `public` on consent). Redirect port 38578
/// (38573–38577 are taken by ticktick/oura/google/trakt/microsoft).
pub static SIMKL: Provider = Provider {
    service: SERVICE,
    display_name: "Simkl",
    auth_url: "https://simkl.com/oauth/authorize",
    token_url: "https://api.simkl.org/oauth/token",
    scopes: "",
    redirect_port: 38578,
    use_pkce: true,
    basic_auth: false,
    // Public client + PKCE: a baked client id alone is enough to "just log in"
    // (there is no secret to bake). Empty default → BYO (register a free app).
    default_client_id: option_env!("TROVE_SIMKL_CLIENT_ID"),
    default_client_secret: None,
    extra_auth_params: &[],
};

/// Registered in [`crate::integrations::CONNECTIONS`]. Single-login (no
/// per-account keying): re-connecting replaces the saved token.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "simkl",
    display_name: "Simkl",
    methods: &[ConnectMethod::OAuth {
        provider: &SIMKL,
        multi_account: false,
        run: connect_oauth,
    }],
    status: connect_status,
    disconnect,
    auto_pull: &["simkl"],
    setup: &[
        "Register a free app at simkl.com/settings/developer (any name).",
        "Set its Redirect URI to http://localhost:38578/callback — must match exactly.",
        "Paste the app's Client ID here (Simkl is a public client — no secret). It's saved, so every future connect is just a login.",
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
            .or_else(|| SIMKL.default_credentials())
            .context("no Simkl app credentials — register an app and enter the Client ID once in the Integrations tab")?,
    };
    let flow = OauthFlow::start(&SIMKL, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(SERVICE, &token)?;
    Ok(token)
}

/// `configured` mirrors the other OAuth connections: app credentials saved or
/// compiled in, so connecting is just a login. At most one account. Simkl
/// tokens never expire and carry no refresh token, so an account is never shown
/// as needing reconnect on its own — only an explicit disconnect or a server
/// 401 (a revoked token, dropped at pull time) forces a fresh login.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured =
        vault.load_sync_app(SERVICE)?.is_some() || SIMKL.default_credentials().is_some();
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
                label: SIMKL.display_name.to_string(),
                connected_at: None,
                // Simkl tokens are non-expiring; `expires_at` is normally None.
                expires_at: token.expires_at,
                // No expiry, no refresh token → never a passive reconnect
                // signal. A revoked token surfaces as a 401 at pull time.
                needs_reconnect: false,
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
// Token / credentials. Simkl tokens never expire and carry no refresh token,
// so (unlike trakt/google) there is NO refresh path — just load the saved
// token; a revoked one is caught as a 401 at fetch time.

/// Resolve the app credentials (saved → compiled-in), needed to read the client
/// id for the `simkl-api-key` header.
fn resolve_creds(vault: &Vault) -> Result<AppCredentials> {
    vault
        .load_sync_app(SERVICE)?
        .or_else(|| SIMKL.default_credentials())
        .context("no Simkl app credentials — reconnect from the Integrations tab")
}

/// The saved access token (Simkl tokens don't expire, so no refresh step).
fn current_token(vault: &Vault) -> Result<TokenSet> {
    vault
        .load_sync_token(SERVICE)?
        .context("Simkl is not connected — connect from the Integrations tab")
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull the manual "Sync now" runs, but it never errors
// the loop — a not-connected state or a network blip is just a quiet no-op
// until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("plays").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("simkl synced — {n} plays")
            }))
        }
        // Not connected / transient network: stay silent, retry next tick. A
        // real bug still surfaces in the log via the message.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "simkl sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected, network) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let plays = out.counts.get("plays").copied().unwrap_or(0);
    let headline = if plays == 0 {
        "Simkl is up to date — no new plays".to_string()
    } else {
        format!("Simkl synced — {plays} plays")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "simkl",
        name: "Simkl",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Sync your Simkl watch history — movies, TV episodes, and anime — into the \
             unified media stream. Simkl carries the best anime ID mapping (MAL, AniDB) \
             and cross-links to IMDB, TMDB, and TVDB. First sync backfills your whole \
             watched list; later syncs fetch only what's new.",
        domain: "media",
        vault_path: "media/plays/simkl/",
        toggleable: true,
        setup: &[
            "Connect your Simkl account on this card (register a free Simkl app first — see the connect steps).",
            "First sync backfills your entire watched list; later syncs are incremental.",
        ],
        caveats: "Simkl records watch *events*, not durations, so every play has seconds=0. \
                  Shows and anime expand to one row per watched episode (episode-level \
                  timestamps); when Simkl only knows a series-level last-watched time, that \
                  series contributes a single row. IDs cross-reference Simkl, IMDB, TMDB, \
                  TVDB, MAL, and AniDB. Connecting uses OAuth (public client, no secret).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SIMKL_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("simkl"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

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

/// The `GET /sync/all-items/` watched-list call. Tests implement this against
/// fixtures; production hits the real API. Returns the whole
/// `{movies, shows, anime}` body (one request — the watched list isn't paged
/// for `date_from` sync; the changed window comes back in full).
trait Watched {
    /// `date_from` is the incremental lower bound (ISO8601 UTC; the API returns
    /// only items changed at/after it), `None` on a first-run backfill.
    fn all_items(&self, access_token: &str, date_from: Option<&str>) -> Result<Value, FetchError>;
}

/// Thin client. The base URL is injected so the sync logic stays testable
/// against a local stub. Carries the app's client id for the mandatory
/// `simkl-api-key` header.
struct SimklClient {
    base: String,
    client_id: String,
}

impl SimklClient {
    fn new(base: String, client_id: String) -> Self {
        SimklClient { base, client_id }
    }
}

impl Watched for SimklClient {
    fn all_items(&self, access_token: &str, date_from: Option<&str>) -> Result<Value, FetchError> {
        // No `type` segment → all of movies+shows+anime in one call.
        // `extended=full` + `episode_watched_at=yes` give per-episode
        // timestamps (the per-episode rows depend on them).
        let mut req = ureq::get(&format!("{}/sync/all-items/", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/json")
            .set("simkl-api-key", &self.client_id)
            .set("Authorization", &format!("Bearer {access_token}"))
            .query("extended", "full")
            .query("episode_watched_at", "yes");
        if let Some(date_from) = date_from {
            req = req.query("date_from", date_from);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
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
    /// Max `watched_at` (ISO8601 UTC, as returned by Simkl) ever written. The
    /// next incremental poll asks for `date_from = watermark`; the guid dedupe
    /// drops the boundary items the inclusive filter re-includes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_simkl_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_simkl_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// One emitted play plus the raw item it came from. A multi-episode show's raw
/// wrapper is attached to each of its episode rows; [`write_rows`] dedupes the
/// raw line so it's stored once.
struct ParsedPlay {
    item: MediaItem,
    raw: Value,
}

/// Parse the whole `{movies, shows, anime}` watched-list body into contract
/// rows + their raw items. Movies → one row; shows/anime → one row per watched
/// episode (per-episode `watched_at`), falling back to one row at the series
/// `last_watched_at` when no episode time is present. Entries with no watch
/// evidence (null `last_watched_at`, no episode times) are skipped. A `null`
/// body (empty watchlist) yields nothing.
fn parse_watched(body: &Value) -> (Vec<MediaItem>, Vec<Value>) {
    let mut plays: Vec<ParsedPlay> = Vec::new();
    for it in array_at(body, "movies") {
        if let Some(p) = movie_play(it) {
            plays.push(p);
        }
    }
    // Shows and anime share the wrapper shape (media object under `show`); the
    // `anime` arm additionally harvests anime ids (mal/anidb) and `anime_type`.
    for it in array_at(body, "shows") {
        episode_plays(it, false, &mut plays);
    }
    for it in array_at(body, "anime") {
        episode_plays(it, true, &mut plays);
    }
    let mut rows = Vec::with_capacity(plays.len());
    let mut raws = Vec::with_capacity(plays.len());
    for p in plays {
        rows.push(p.item);
        raws.push(p.raw);
    }
    (rows, raws)
}

/// A movie watched-list entry → one [`MediaItem`] at its `last_watched_at`.
/// `None` if it has no watch time (not actually watched) or no title.
fn movie_play(it: &Value) -> Option<ParsedPlay> {
    let watched_at = watch_time(it)?;
    let movie = it.get("movie")?;
    let title = str_at(movie, "title");
    if title.is_empty() {
        return None;
    }
    let simkl_id = id_at(movie, "simkl");
    let year = movie.get("year").and_then(Value::as_i64);
    // Subtitle is "(Year)" or "" — movies have no series grouping key.
    let subtitle = match year {
        Some(y) => format!("({y})"),
        None => String::new(),
    };

    let mut extra = Map::new();
    {
        let mut put = id_putter(&mut extra);
        put("status", &str_at(it, "status"));
        put("simkl_id", &simkl_id);
        put("imdb_id", &str_at_ids(movie, "imdb"));
        put("tmdb_id", &id_at(movie, "tmdb"));
    }

    let item = media_item(&watched_at, title, subtitle, String::new(), &simkl_id, extra)?;
    Some(ParsedPlay { item, raw: it.clone() })
}

/// A show/anime watched-list entry → one [`MediaItem`] per watched episode
/// (its own `watched_at`), or a single series row at `last_watched_at` when no
/// per-episode time is available. Skipped entirely when there's no watch
/// evidence. Appends onto `out`.
fn episode_plays(it: &Value, is_anime: bool, out: &mut Vec<ParsedPlay>) {
    let Some(show) = it.get("show") else { return };
    let show_title = str_at(show, "title");
    let show_simkl_id = id_at(show, "simkl");

    // Show-level ids → every episode row carries them for series grouping.
    let show_ids = |extra: &mut Map<String, Value>| {
        let mut put = id_putter(extra);
        put("status", &str_at(it, "status"));
        put("show_simkl_id", &show_simkl_id);
        put("show_imdb_id", &str_at_ids(show, "imdb"));
        put("show_tmdb_id", &id_at(show, "tmdb"));
        put("show_tvdb_id", &id_at(show, "tvdb"));
        if is_anime {
            // Anime's strength: MAL + AniDB ids.
            put("mal_id", &id_at(show, "mal"));
            put("anidb_id", &id_at(show, "anidb"));
            put("anime_type", &str_at(it, "anime_type"));
        }
    };

    // Per-episode rows — present only with episode_watched_at=yes. The real
    // `/sync/all-items` returns episodes as `{number, watched_at}` only: there
    // is NO per-episode `simkl` id (all ids live at the show wrapper level), so
    // an episode's guid is always the show id + SxxExx detail (+ watched_at).
    let mut emitted = 0usize;
    for (sidx, season) in array_at(it, "seasons").iter().enumerate() {
        let snum = season.get("number").and_then(Value::as_i64);
        for ep in array_at(season, "episodes") {
            let Some(watched_at) = watch_time(ep) else {
                continue; // episode not watched (or no per-episode time)
            };
            let enum_ = ep.get("number").and_then(Value::as_i64);
            // The SxxExx label. When `season.number` is absent (undocumented /
            // degenerate — real all-items always carries it), fall back to the
            // season's array index so two number-less seasons sharing an episode
            // number + watched_at can't collapse to one guid (silent data loss).
            let detail = match (snum, enum_) {
                (Some(s), Some(e)) => format!("S{s:02}E{e:02}"),
                (None, Some(e)) => format!("S#{sidx:02}E{e:02}"),
                (Some(s), None) => format!("S{s:02}"),
                (None, None) => format!("S#{sidx:02}"),
            };
            // Episode title: the SxxExx label (Simkl's watched list carries no
            // per-episode titles), falling back to the show title.
            let title = if !detail.is_empty() { detail.clone() } else { show_title.clone() };
            // guid keys on the show id + SxxExx — unique per episode, stable
            // across re-pulls (episodes carry no id of their own).
            let guid_key = format!("{show_simkl_id}-{detail}");
            let mut extra = Map::new();
            show_ids(&mut extra);
            {
                let mut put = id_putter(&mut extra);
                if let Some(s) = snum {
                    put("season", &s.to_string());
                }
                if let Some(e) = enum_ {
                    put("number", &e.to_string());
                }
            }
            if let Some(item) =
                media_item(&watched_at, title, show_title.clone(), detail, &guid_key, extra)
            {
                out.push(ParsedPlay { item, raw: it.clone() });
                emitted += 1;
            }
        }
    }
    if emitted > 0 {
        return;
    }

    // No per-episode watched time: fall back to one series-level row at
    // `last_watched_at` (still an honest "watched this series then"), but only
    // when the series actually has a watch time and a title.
    let Some(watched_at) = watch_time(it) else {
        return;
    };
    if show_title.is_empty() {
        return;
    }
    let mut extra = Map::new();
    show_ids(&mut extra);
    // The detail carries the last episode label Simkl knows ("E148"/"S01E02").
    let detail = str_at(it, "last_watched");
    // Series-level guid: show id + a "-series" tag (no episode id here).
    let guid_key = format!("{show_simkl_id}-series");
    if let Some(item) =
        media_item(&watched_at, show_title.clone(), String::new(), detail, &guid_key, extra)
    {
        out.push(ParsedPlay { item, raw: it.clone() });
    }
}

/// Build a [`MediaItem`] from a UTC `watched_at`, rendering the local-offset
/// `ts` and the `simkl-<key>-<watched_at>` guid. `None` if the time doesn't
/// parse.
fn media_item(
    watched_at: &str,
    title: String,
    subtitle: String,
    detail: String,
    guid_key: &str,
    extra: Map<String, Value>,
) -> Option<MediaItem> {
    // watched_at is ISO8601 UTC → RFC3339 with the local offset (same instant),
    // via the shared local conversion (no hand-rolled tz math).
    let ts = DateTime::parse_from_rfc3339(watched_at)
        .ok()?
        .with_timezone(&Local)
        .to_rfc3339();
    Some(MediaItem {
        ts,
        source: "simkl".into(),
        category: "video".into(),
        device: String::new(),
        kind: "play".into(),
        title,
        subtitle,
        detail,
        // Simkl records the fact of a watch, not its duration — an honest 0.
        seconds: 0,
        favicon: String::new(),
        // guid_key + watched_at: a re-watch of the same item at a later time is
        // a distinct play; a re-pull of the same watch dedupes.
        guid: format!("simkl-{guid_key}-{watched_at}"),
        extra,
    })
}

/// A closure that inserts a trimmed non-empty string into `extra` (missing ids
/// are omitted, not stored empty).
fn id_putter(extra: &mut Map<String, Value>) -> impl FnMut(&str, &str) + '_ {
    move |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    }
}

/// The watch timestamp of a watched-list entry or episode: `watched_at` if
/// present, else `last_watched_at` (series wrapper). `None` when missing, empty,
/// or JSON null.
fn watch_time(v: &Value) -> Option<String> {
    for key in ["watched_at", "last_watched_at"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            let s = s.trim();
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// An array field of an object as a slice; empty when missing or not an array.
fn array_at<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
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

/// A numeric-or-string id under `obj.ids.<key>`, stringified; "" when missing.
/// Simkl is inconsistent — `simkl`/`tmdb` may be a number or a quoted string,
/// `mal`/`anidb` are usually quoted strings — so both forms are tolerated.
fn id_at(v: &Value, key: &str) -> String {
    match v.get("ids").and_then(|ids| ids.get(key)) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.trim().to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Outcome of writing one batch of parsed plays.
struct WriteStats {
    plays: u64,
    /// Max `watched_at` (ISO8601) seen — drives the forward-only watermark
    /// advance. ISO8601 UTC strings sort lexically, so the string max is the
    /// latest instant.
    max_watched_at: Option<String>,
}

/// A raw watched-list item carrying the contract ts purely so the
/// month-partition writer files it under the watch's month. Only `value` is
/// serialized — flattened, so the raw line is the API object verbatim.
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

/// The watermark candidate from a raw entry: the latest watch time anywhere in
/// it (the wrapper `last_watched_at` is the series max, but defensively scan
/// the episodes too so a stray newer per-episode time still advances the
/// cursor).
fn raw_max_watched_at(raw: &Value) -> Option<String> {
    let mut max: Option<String> = watch_time(raw);
    for season in array_at(raw, "seasons") {
        for ep in array_at(season, "episodes") {
            if let Some(w) = watch_time(ep) {
                max = max_iso(max, w);
            }
        }
    }
    max
}

/// Write raw + contract rows, deduped by guid against what's already on disk,
/// and report the count written plus the max `watched_at` seen. Raw lines
/// partition by the same month as their contract row (the play's local month).
fn write_rows(vault: &Vault, rows: &[MediaItem], raws: &[Value]) -> Result<WriteStats> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids — re-runnable: a re-pull of overlapping items never
    // duplicates. (The store appends; dedupe is the domain's job.)
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
    // A multi-episode show shares one raw wrapper across its episode rows;
    // write that raw line once per distinct wrapper in this batch.
    let mut raw_written: HashSet<String> = HashSet::new();
    let mut max_watched_at: Option<String> = None;
    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        // Watermark tracks the API's UTC `watched_at`, not the local-offset
        // contract ts, so the `date_from` we send back is in the API's frame.
        // (Computed for every parsed row, even dedup-skipped ones, so a re-pull
        // still re-confirms the high-water mark.)
        if let Some(w) = raw_max_watched_at(raw_val) {
            max_watched_at = max_iso(max_watched_at, w);
        }
        if !seen.insert(item.guid.clone()) {
            continue; // already stored
        }
        new_rows.push(item.clone());
        // Dedupe the raw wrapper so a show with N watched episodes doesn't write
        // its raw object N times. Key on a stable signature (media simkl id +
        // series watch time).
        let raw_sig = format!(
            "{}-{}",
            raw_val
                .get("show")
                .or_else(|| raw_val.get("movie"))
                .map(|m| id_at(m, "simkl"))
                .unwrap_or_default(),
            watch_time(raw_val).unwrap_or_default()
        );
        if raw_written.insert(raw_sig) {
            new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
        }
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;

    Ok(WriteStats { plays: new_rows.len() as u64, max_watched_at })
}

/// Resolve credentials + the saved token and sync. First run (no watermark):
/// backfill the whole watched list (no `date_from`). Otherwise: incremental
/// from `date_from = watermark`. Returns a [`PullOutcome`] with a `plays` count.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let creds = resolve_creds(vault)?;
    let token = current_token(vault)?;
    let client = SimklClient::new(API_BASE.to_string(), creds.client_id.clone());
    pull_with(vault, &client, &token.access_token)
}

/// The pull body over an injected fetcher — the testable seam.
///
/// One `GET /sync/all-items/` returns the whole changed window as
/// `{movies, shows, anime}` (the watched list isn't paged for `date_from`
/// sync). We parse it fully, write raw + contract deduped by guid, then advance
/// the watermark to the max `watched_at` seen — forward-only, only after the
/// write succeeds (so a failure mid-way re-pulls from the same `date_from`; the
/// guid dedupe skips what already landed). A `null` body (empty watchlist) is a
/// clean no-op.
fn pull_with(vault: &Vault, client: &impl Watched, access_token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_simkl_sync();
    let date_from = state.watermark.clone();

    let body = match client.all_items(access_token, date_from.as_deref()) {
        Ok(b) => b,
        Err(e @ FetchError::Unauthorized) => {
            // A revoked/invalid token. Drop it so the card shows reconnect.
            vault.delete_sync_token(SERVICE)?;
            bail!("Simkl rejected the request: {e} — reconnect from the Integrations tab");
        }
        Err(FetchError::RateLimited) => bail!("Simkl rate limited — try again later"),
        Err(e) => bail!("Simkl fetch failed: {e}"),
    };

    let (rows, raws) = parse_watched(&body);
    let stats = write_rows(vault, &rows, &raws)?;

    // Advance the watermark to the max watched_at seen (forward-only; ISO8601
    // UTC strings sort lexically so string comparison is instant comparison).
    if let Some(w) = stats.max_watched_at {
        if state.watermark.as_deref().is_none_or(|cur| w.as_str() > cur) {
            state.watermark = Some(w);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_simkl_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{} plays", stats.plays),
        counts: BTreeMap::from([("plays", stats.plays)]),
    })
}

// TODO(simkl ratings/watchlist): curation snapshots — GET /sync/ratings and the
// plan-to-watch/hold/dropped watchlist statuses → overwrite (replace-on-sync)
// media/simkl/ratings.jsonl + media/simkl/watchlist.jsonl (one full-fidelity
// item per line, NOT media-plays, no contract). Authenticated (bearer + the
// simkl-api-key header). Deferred to keep the history → media-plays path
// bulletproof within budget; see the return note.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-simkl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A real `GET /sync/all-items/` body (`extended=full&episode_watched_at=yes`):
    /// one MOVIE, one SHOW with two watched episodes, one ANIME with one watched
    /// episode (exercising the mal/anidb id `extra` fields). Shapes follow the
    /// documented Simkl API (api.simkl.org / the SIMKL/API apiary.apib): all ids
    /// (simkl/imdb/tmdb/tvdb/mal/anidb) live at the show/movie wrapper level —
    /// episodes carry ONLY `{number, watched_at}`, never a per-episode id.
    fn sample_body() -> Value {
        serde_json::json!({
            "movies": [
                {
                    "last_watched_at": "2026-06-10T20:00:00Z",
                    "status": "completed",
                    "movie": {
                        "title": "The Matrix",
                        "year": 1999,
                        "ids": { "simkl": 481, "imdb": "tt0133093", "tmdb": 603 }
                    }
                }
            ],
            "shows": [
                {
                    "last_watched_at": "2026-05-30T03:15:00Z",
                    "status": "watching",
                    "last_watched": "S01E02",
                    "show": {
                        "title": "Game of Thrones",
                        "year": 2011,
                        "ids": { "simkl": 1390, "tvdb": "121361", "imdb": "tt0944947", "tmdb": "1399" }
                    },
                    "seasons": [
                        {
                            "number": 1,
                            "episodes": [
                                { "number": 1, "watched_at": "2026-05-29T02:00:00Z" },
                                { "number": 2, "watched_at": "2026-05-30T03:15:00Z" }
                            ]
                        }
                    ]
                }
            ],
            "anime": [
                {
                    "last_watched_at": "2026-04-01T12:00:00Z",
                    "status": "watching",
                    "anime_type": "tv",
                    "last_watched": "S01E01",
                    "show": {
                        "title": "Attack on Titan",
                        "year": 2013,
                        "ids": { "simkl": 39687, "mal": "16498", "tvdb": 267440, "anidb": "9541", "imdb": "tt2560140" }
                    },
                    "seasons": [
                        {
                            "number": 1,
                            "episodes": [
                                { "number": 1, "watched_at": "2026-04-01T12:00:00Z" }
                            ]
                        }
                    ]
                }
            ]
        })
    }

    #[test]
    fn parses_movie_show_and_anime_items() {
        let (rows, raws) = parse_watched(&sample_body());
        // 1 movie + 2 show episodes + 1 anime episode = 4 contract rows.
        assert_eq!(rows.len(), 4);
        assert_eq!(raws.len(), 4);

        // MOVIE: title = movie title, subtitle = "(year)", no detail.
        let movie = rows.iter().find(|r| r.title == "The Matrix").unwrap();
        assert_eq!(movie.subtitle, "(1999)");
        assert_eq!(movie.detail, "", "movies carry no SxxExx detail");
        assert_eq!(movie.source, "simkl");
        assert_eq!(movie.category, "video");
        assert_eq!(movie.kind, "play");
        assert_eq!(movie.seconds, 0, "events not durations");
        assert_eq!(movie.guid, "simkl-481-2026-06-10T20:00:00Z");
        assert_eq!(
            DateTime::parse_from_rfc3339(&movie.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T20:00:00Z").unwrap().timestamp()
        );
        assert_eq!(movie.extra.get("simkl_id"), Some(&Value::String("481".into())));
        assert_eq!(movie.extra.get("imdb_id"), Some(&Value::String("tt0133093".into())));
        assert_eq!(movie.extra.get("tmdb_id"), Some(&Value::String("603".into())));
        assert_eq!(movie.extra.get("status"), Some(&Value::String("completed".into())));

        // SHOW episodes: title = SxxExx, subtitle = show title, detail = SxxExx.
        let ep2 = rows
            .iter()
            .find(|r| r.detail == "S01E02" && r.subtitle == "Game of Thrones")
            .unwrap();
        assert_eq!(ep2.title, "S01E02");
        assert_eq!(ep2.subtitle, "Game of Thrones");
        // Real episodes carry NO per-episode simkl id, so the guid is the SHOW
        // id + SxxExx detail + watched_at — the production fallback path.
        assert_eq!(
            ep2.guid, "simkl-1390-S01E02-2026-05-30T03:15:00Z",
            "guid keys on show id + SxxExx (episodes have no id of their own)"
        );
        assert_eq!(ep2.extra.get("show_simkl_id"), Some(&Value::String("1390".into())));
        assert_eq!(ep2.extra.get("show_imdb_id"), Some(&Value::String("tt0944947".into())));
        assert_eq!(ep2.extra.get("show_tvdb_id"), Some(&Value::String("121361".into())));
        assert!(
            ep2.extra.get("episode_simkl_id").is_none(),
            "no per-episode id in real all-items data"
        );
        assert_eq!(ep2.extra.get("season"), Some(&Value::String("1".into())));
        assert_eq!(ep2.extra.get("number"), Some(&Value::String("2".into())));
        assert!(ep2.extra.get("mal_id").is_none(), "a show carries no anime ids");

        let ep1 = rows
            .iter()
            .find(|r| r.detail == "S01E01" && r.subtitle == "Game of Thrones")
            .unwrap();
        assert_eq!(ep1.guid, "simkl-1390-S01E01-2026-05-29T02:00:00Z");
        // Distinct episodes (different SxxExx) get DISTINCT stable guids.
        assert_ne!(ep1.guid, ep2.guid, "different episodes → different guids");

        // ANIME episode: anime-specific ids (mal, anidb) ride in extra.
        let anime = rows.iter().find(|r| r.subtitle == "Attack on Titan").unwrap();
        assert_eq!(anime.title, "S01E01");
        assert_eq!(anime.detail, "S01E01");
        assert_eq!(anime.category, "video");
        assert_eq!(anime.guid, "simkl-39687-S01E01-2026-04-01T12:00:00Z");
        assert_eq!(anime.extra.get("show_simkl_id"), Some(&Value::String("39687".into())));
        assert_eq!(anime.extra.get("mal_id"), Some(&Value::String("16498".into())));
        assert_eq!(anime.extra.get("anidb_id"), Some(&Value::String("9541".into())));
        assert_eq!(anime.extra.get("anime_type"), Some(&Value::String("tv".into())));
        assert_eq!(anime.extra.get("show_imdb_id"), Some(&Value::String("tt2560140".into())));
    }

    #[test]
    fn series_without_episode_times_falls_back_to_one_row() {
        // A show with only a series-level last_watched_at (no episode_watched_at
        // data) → exactly one row at that time, not zero and not per-episode.
        let body = serde_json::json!({
            "shows": [
                {
                    "last_watched_at": "2026-06-01T00:00:00Z",
                    "status": "completed",
                    "last_watched": "E148",
                    "show": { "title": "Hunter x Hunter", "year": 2011, "ids": { "simkl": 40398 } },
                    "seasons": [
                        { "number": 1, "episodes": [ { "number": 1 }, { "number": 2 } ] }
                    ]
                }
            ]
        });
        let (rows, raws) = parse_watched(&body);
        assert_eq!(rows.len(), 1, "no per-episode time → single series row");
        assert_eq!(raws.len(), 1);
        assert_eq!(rows[0].title, "Hunter x Hunter");
        assert_eq!(rows[0].subtitle, "", "series row has no subtitle");
        assert_eq!(rows[0].detail, "E148", "detail carries the last-watched label");
        assert_eq!(rows[0].guid, "simkl-40398-series-2026-06-01T00:00:00Z");
    }

    #[test]
    fn plan_to_watch_with_no_watch_time_is_skipped() {
        // status plantowatch / hold with last_watched_at null → not a play.
        let body = serde_json::json!({
            "shows": [
                {
                    "last_watched_at": null,
                    "status": "plantowatch",
                    "show": { "title": "Emerald City", "year": 2017, "ids": { "simkl": 583436 } }
                }
            ],
            "movies": [
                {
                    "last_watched_at": null,
                    "status": "plantowatch",
                    "movie": { "title": "Dune Part Three", "ids": { "simkl": 99999 } }
                }
            ]
        });
        let (rows, _) = parse_watched(&body);
        assert!(rows.is_empty(), "unwatched plan-to-watch entries emit nothing");
    }

    #[test]
    fn null_season_numbers_do_not_collide_guids() {
        // Defensive: real all-items always carries season.number, but if two
        // seasons both have `number: null` and each has an episode `number: 1`
        // at the SAME watched_at, a season-number-less detail would collapse to
        // one guid → silent data loss. The season's array index discriminates,
        // so the two distinct episodes get two distinct guids (two rows).
        let body = serde_json::json!({
            "shows": [
                {
                    "last_watched_at": "2026-06-01T00:00:00Z",
                    "status": "watching",
                    "show": { "title": "Degenerate Show", "ids": { "simkl": 7777 } },
                    "seasons": [
                        { "number": null, "episodes": [ { "number": 1, "watched_at": "2026-06-01T00:00:00Z" } ] },
                        { "number": null, "episodes": [ { "number": 1, "watched_at": "2026-06-01T00:00:00Z" } ] }
                    ]
                }
            ]
        });
        let (rows, _) = parse_watched(&body);
        assert_eq!(rows.len(), 2, "two distinct episodes → two rows, no collapse");
        let guids: HashSet<&str> = rows.iter().map(|r| r.guid.as_str()).collect();
        assert_eq!(guids.len(), 2, "distinct seasons must not collide to one guid");
    }

    /// A one-body-then-empty fetcher: returns the fixture on call 1, an empty
    /// watchlist after. Enough for the store/cursor + re-run dedupe test.
    struct StubClient {
        body: Value,
        calls: std::cell::RefCell<u32>,
    }
    impl StubClient {
        fn new(body: Value) -> Self {
            StubClient { body, calls: std::cell::RefCell::new(0) }
        }
    }
    impl Watched for StubClient {
        fn all_items(
            &self,
            _access_token: &str,
            _date_from: Option<&str>,
        ) -> Result<Value, FetchError> {
            let mut c = self.calls.borrow_mut();
            *c += 1;
            if *c == 1 {
                Ok(self.body.clone())
            } else {
                Ok(serde_json::json!({ "movies": [], "shows": [], "anime": [] }))
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
        let client = StubClient::new(sample_body());

        let out = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&4));

        // Contract layer, partitioned by each play's LOCAL month.
        let stream = v.stream(DIR, Partition::Month);
        let mut on_disk: Vec<MediaItem> = Vec::new();
        for key in stream.partitions().unwrap() {
            for it in stream.read::<MediaItem>(&key).unwrap() {
                assert_eq!(Partition::Month.key(&it.ts), Some(key.as_str()));
                on_disk.push(it);
            }
        }
        assert_eq!(on_disk.len(), 4, "four contract rows persisted");

        // Raw layer mirrors the partitioning under raw/, verbatim objects. The
        // show with two episodes contributes ONE raw line (deduped) → 3 raws.
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
                    assert_eq!(it["movie"]["ids"]["imdb"].as_str(), Some("tt0133093"));
                }
            }
        }
        assert_eq!(raw_count, 3, "movie + show(once) + anime raw wrappers");
        assert!(saw_matrix, "raw kept the full movie object verbatim");

        // Cursor advanced to the max watched_at written (the newest, the movie).
        let state = v.read_simkl_sync();
        assert_eq!(state.watermark.as_deref(), Some("2026-06-10T20:00:00Z"));
        assert!(state.updated.is_some());

        // Re-run with the same input → guid dedupe, no duplicate contract rows.
        let client2 = StubClient::new(sample_body());
        let again = pull_with(&v, &client2, "tok").unwrap();
        assert_eq!(again.counts.get("plays"), Some(&0), "all guids already stored");
        let mut after = 0usize;
        for key in stream.partitions().unwrap() {
            after += stream.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(after, 4, "no duplicate rows after re-run");

        // And the unified media stream sees the movie via the contract arm.
        let day = v.media_timeline(&local_day("2026-06-10T20:00:00Z")).unwrap();
        let matrix = day.iter().find(|i| i.title == "The Matrix");
        assert!(matrix.is_some(), "the movie joins the unified stream on its local day");
        assert_eq!(matrix.unwrap().source, "simkl");
        assert_eq!(matrix.unwrap().category, "video");
    }

    #[test]
    fn incremental_window_advances_cursor_to_max_watched_at() {
        // Seed a watermark (incremental path → date_from sent). The whole
        // returned window must be drained and the cursor advanced to the newest
        // watch across movies/shows/anime.
        let v = temp_vault("gap");
        v.write_simkl_sync(&SyncState {
            watermark: Some("2026-01-01T00:00:00Z".into()),
            updated: Some("2026-01-01T00:00:00+00:00".into()),
        })
        .unwrap();

        let client = StubClient::new(sample_body());
        let out = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&4), "the whole window drains");

        let stream = v.stream(DIR, Partition::Month);
        let mut on_disk = 0usize;
        for key in stream.partitions().unwrap() {
            on_disk += stream.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(on_disk, 4, "all rows persisted, no gaps");

        // Newest watch in the fixture is the movie at 2026-06-10.
        let state = v.read_simkl_sync();
        assert_eq!(state.watermark.as_deref(), Some("2026-06-10T20:00:00Z"));

        // Idempotent: a second pull finds nothing new (guid dedupe + cursor).
        let again = pull_with(&v, &client, "tok").unwrap();
        assert_eq!(again.counts.get("plays"), Some(&0), "no re-write past the boundary");
    }

    #[test]
    fn null_body_is_a_clean_noop() {
        // Simkl returns `null` when the user's watchlist is empty.
        struct NullClient;
        impl Watched for NullClient {
            fn all_items(&self, _t: &str, _d: Option<&str>) -> Result<Value, FetchError> {
                Ok(Value::Null)
            }
        }
        let v = temp_vault("null");
        let out = pull_with(&v, &NullClient, "tok").unwrap();
        assert_eq!(out.counts.get("plays"), Some(&0));
        // Cursor still gets an `updated` stamp; watermark stays None.
        let state = v.read_simkl_sync();
        assert!(state.watermark.is_none());
        assert!(state.updated.is_some());
    }

    // -----------------------------------------------------------------------
    // Connection / token tests — all offline (no network, no real creds).

    fn token(expires_at: Option<u64>) -> TokenSet {
        TokenSet {
            access_token: "acc".into(),
            // Simkl issues NO refresh token.
            refresh_token: None,
            token_type: Some("bearer".into()),
            scope: Some("public".into()),
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
        let baked = option_env!("TROVE_SIMKL_CLIENT_ID").is_some();
        assert_eq!(s.configured, baked);

        // Saving an app id (no secret — public client) makes it "configured".
        v.save_sync_app(SERVICE, &AppCredentials { client_id: "cid".into(), client_secret: None })
            .unwrap();
        // A non-expiring token maps to exactly one account, never reconnect.
        v.save_sync_token(SERVICE, &token(None)).unwrap();
        let s = connect_status(&v).unwrap();
        assert!(s.configured);
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "simkl");
        assert_eq!(a.label, "Simkl");
        assert!(!a.needs_reconnect, "non-expiring token never flags reconnect");
    }

    #[test]
    fn disconnect_forgets_token_keeps_app_creds() {
        let v = temp_vault("disconnect");
        v.save_sync_app(SERVICE, &AppCredentials { client_id: "cid".into(), client_secret: None })
            .unwrap();
        v.save_sync_token(SERVICE, &token(None)).unwrap();
        disconnect(&v, "simkl").unwrap();
        assert!(connect_status(&v).unwrap().accounts.is_empty());
        assert!(connect_status(&v).unwrap().configured, "app creds survive disconnect");
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
        assert!(v.load_sync_app(SERVICE).unwrap().is_some());
    }

    #[test]
    fn current_token_without_connection_is_a_clean_error() {
        let v = temp_vault("token-unconnected");
        let err = current_token(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
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
            serde_json::from_str(r#"{"watermark":"2026-06-10T20:00:00Z"}"#).unwrap();
        assert_eq!(old.watermark.as_deref(), Some("2026-06-10T20:00:00Z"));
        assert!(old.updated.is_none());
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermark.is_none() && empty.updated.is_none());
    }

    // -----------------------------------------------------------------------
    // OAuth: public client (PKCE, NO secret). Mirrors trakt's authorize-URL
    // test but asserts the secret is absent and the verifier/challenge present.

    #[test]
    fn authorize_url_is_public_pkce_no_secret() {
        // The compiled-in default carries no secret (public client).
        assert!(SIMKL.default_client_secret.is_none(), "public client: no baked secret");
        assert_eq!(SIMKL.redirect_port, 38578);
        assert!(SIMKL.use_pkce, "PKCE on");
        assert!(!SIMKL.basic_auth, "creds go in the body, not Basic auth");

        let creds = AppCredentials { client_id: "my-client".into(), client_secret: None };
        // Ephemeral bind: never bind the real 38578 in tests (parallel-safe).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let flow = OauthFlow::start_with_listener(&SIMKL, &creds, listener).unwrap();
        let url = flow.authorize_url();
        assert!(url.starts_with("https://simkl.com/oauth/authorize?"));
        assert!(url.contains("client_id=my-client"));
        // PKCE challenge present; no secret ever appears in the authorize URL.
        assert!(url.contains("code_challenge_method=S256"), "PKCE challenge present");
        assert!(!url.contains("client_secret"), "no secret in a public-client flow");
        // The fixed 38578 callback rides in the URL (urlencoded).
        assert!(url.contains("38578"));
    }
}
