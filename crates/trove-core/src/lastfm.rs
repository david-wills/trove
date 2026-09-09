//! Last.fm scrobble history via the official AudioScrobbler API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/lastfm.md.
//!
//! A **Periodic** cloud pull (M5): every scrobble Last.fm has ever recorded
//! lands in the unified media stream via the **media-plays write contract**
//! (`docs/vault-spec/domains/media-plays.md`). Two layers per play:
//!
//! - **raw** — the API track object verbatim at
//!   `media/plays/lastfm/raw/YYYY-MM.jsonl`, partitioned by the play's month
//!   (full fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/lastfm/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! `user.getRecentTracks` returns newest-first, paginated (200/page). The
//! first sync backfills the whole history (page 1 → `totalPages`); later
//! syncs pass `from=<watermark+1>` so only new scrobbles come back. The
//! watermark is the max scrobble `uts` ever written, kept in a rebuildable
//! cursor at `.trove/lastfm-sync.json` (non-secret state, so it sits beside
//! the vault's other `.trove/` indexes, not under `.trove/sync/`).
//!
//! Auth is a username plus an api_key. The username is the connection's
//! single pasted field (TokenPaste); the api_key is read from
//! `TROVE_LASTFM_API_KEY` (env → compiled-in [`BAKED_API_KEY`]) — every call
//! needs one, there is no keyless path. Reads come from the *public* profile;
//! private-profile reads (a session key) are out of scope for v1.

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
use crate::vault::Vault;

/// Contract-layer stream directory; raw lines go one level deeper in `raw/`.
const DIR: &str = "media/plays/lastfm";
const RAW_DIR: &str = "media/plays/lastfm/raw";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for
/// 0600 secrets); deleting it re-walks the whole history on the next sync.
const SYNC_FILE: &str = ".trove/lastfm-sync.json";

/// The service id under `.trove/sync/` where the username is stored (reusing
/// the secret store's `service-token.json` slot, exactly like Oura's PAT).
const SERVICE: &str = "lastfm";

/// Compiled-in API key default. Empty by default — set `TROVE_LASTFM_API_KEY`
/// at build time to bake one in, or at runtime to provide one. There is no
/// keyless path: every Last.fm call requires an api_key.
const BAKED_API_KEY: &str = "";

const API_BASE: &str = "https://ws.audioscrobbler.com";
/// Last.fm allows 200 tracks per page.
const PAGE_SIZE: u32 = 200;
/// ~5 req/s ceiling → 200ms between page fetches.
const REQ_INTERVAL: Duration = Duration::from_millis(200);
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Seconds between syncs in the watcher loop. Hourly: scrobbles trickle in
/// and the incremental `from=` poll is one cheap request when idle.
pub const LASTFM_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull the manual "Sync now" runs, but it never
// errors the loop — a missing key/username or a network blip is just a quiet
// no-op until the next tick.
fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("scrobbles").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("last.fm synced — {n} scrobbles")
            }))
        }
        // Not connected / no key / transient network: stay silent, retry next
        // tick. A real bug still surfaces in the log via the message.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "last.fm sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected, no key) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let scrobbles = out.counts.get("scrobbles").copied().unwrap_or(0);
    let headline = if scrobbles == 0 {
        "Last.fm is up to date — no new scrobbles".to_string()
    } else {
        format!("Last.fm synced — {scrobbles} scrobbles")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "lastfm",
        name: "Last.fm",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Your complete Last.fm scrobble history, pulled via \
                      ws.audioscrobbler.com into the unified media stream. \
                      First sync backfills your whole history; later syncs \
                      fetch only what's new.",
        domain: "media",
        vault_path: "media/plays/lastfm/",
        toggleable: true,
        setup: &[
            "Connect with your public Last.fm username on this card.",
            "First sync backfills your entire scrobble history (paginated); later syncs are incremental.",
        ],
        caveats: "Reads your public Last.fm profile — private profiles aren't supported yet. \
                  Last.fm records play *events*, not durations, so every scrobble has seconds=0. \
                  The now-playing track (if any) is skipped — it isn't a completed scrobble. \
                  Rate-limited to ~5 requests per second; a large history backfills over many pages.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(LASTFM_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("lastfm"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the public username).

/// Store the pasted username under `.trove/sync/lastfm.json` (0600), modeled
/// on Oura's PAT store: the username rides in the `access_token` slot of a
/// never-expiring [`TokenSet`], so the secret-store mechanics are shared and
/// `status`/`disconnect` are trivial. If an api_key is provisioned we *may*
/// do a one-track verification fetch; with no key we store the username
/// anyway (live validation deferred — there is no key in CI/dev).
fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("empty username");
    }
    // Best-effort validation only when a key is available; never required.
    if let Some(key) = resolve_api_key() {
        let client = LastfmClient::new(API_BASE.to_string(), key);
        // A single-page probe. A clear error is surfaced to the connect UI;
        // network/other errors don't block storing the username (the user may
        // be briefly offline) — only an explicit "no such user" would, and
        // Last.fm returns that as an error body we treat as Other below.
        if let Err(FetchError::Other(msg)) = client.recent_tracks(username, 1, 1, None) {
            if msg.contains("User not found") || msg.contains("\"error\":6") {
                bail!("Last.fm could not find user {username:?} — check the spelling");
            }
        }
    }
    let token = crate::sync::oauth::TokenSet {
        access_token: username.to_string(),
        refresh_token: None,
        token_type: None,
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &token)
}

/// Forget the stored username. Synced data stays in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// `configured` = an api_key is available (env or baked); without one no pull
/// can run, so the hub should say so. The connected account, if any, is the
/// stored username.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = resolve_api_key().is_some();
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let username = token.access_token;
        let mut extra = BTreeMap::new();
        // Honest signal when the username is stored but no key is provisioned:
        // the pull can't run until a key lands.
        if !configured {
            extra.insert("note", "no API key configured (set TROVE_LASTFM_API_KEY)".to_string());
        }
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: username,
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // a username never expires
            needs_reconnect: false,
            extra,
        });
    }
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste a
/// public username. The api_key is provisioned out-of-band via
/// `TROVE_LASTFM_API_KEY` (no per-user app registration), so it isn't a
/// connect field.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "lastfm",
    display_name: "Last.fm",
    methods: &[ConnectMethod::TokenPaste {
        label: "Last.fm username",
        help: "Your public Last.fm profile name — listening history is read from your public profile. Private profiles aren't supported yet.",
        placeholder: "rj",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["lastfm"],
    setup: &[
        "Enter your public Last.fm username and connect.",
        "Listening history is read from your public profile; private profiles aren't supported yet.",
    ],
};

// ---------------------------------------------------------------------------
// API key resolution: env → baked. None when neither is set (no keyless path).

fn resolve_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("TROVE_LASTFM_API_KEY") {
        let k = k.trim();
        if !k.is_empty() {
            return Some(k.to_string());
        }
    }
    let baked = BAKED_API_KEY.trim();
    (!baked.is_empty()).then(|| baked.to_string())
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors, mirroring [`crate::oura`]'s split: 401/429 want
/// distinct handling, everything else is a message.
#[derive(Debug)]
enum FetchError {
    RateLimited,
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// One page of `user.getRecentTracks`. Tests implement this against fixtures;
/// production hits the real API.
trait RecentTracks {
    /// `from` is an optional lower bound (unix seconds, inclusive) — the
    /// incremental watermark filter.
    fn recent_tracks(
        &self,
        user: &str,
        page: u32,
        limit: u32,
        from: Option<i64>,
    ) -> Result<Value, FetchError>;
}

/// Thin client. The base URL is injected so the sync logic stays testable
/// against a local stub (the `oura.rs` / `tasks.rs` pattern).
struct LastfmClient {
    base: String,
    api_key: String,
}

impl LastfmClient {
    fn new(base: String, api_key: String) -> Self {
        LastfmClient { base, api_key }
    }
}

impl RecentTracks for LastfmClient {
    fn recent_tracks(
        &self,
        user: &str,
        page: u32,
        limit: u32,
        from: Option<i64>,
    ) -> Result<Value, FetchError> {
        let mut req = ureq::get(&format!("{}/2.0/", self.base))
            .timeout(HTTP_TIMEOUT)
            .query("method", "user.getRecentTracks")
            .query("user", user)
            .query("api_key", &self.api_key)
            .query("format", "json")
            .query("extended", "1")
            .query("limit", &limit.to_string())
            .query("page", &page.to_string());
        if let Some(from) = from {
            req = req.query("from", &from.to_string());
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                // Last.fm signals errors with HTTP 200 + an `error` code body.
                if let Some(code) = v.get("error").and_then(Value::as_i64) {
                    let msg = v.get("message").and_then(Value::as_str).unwrap_or("");
                    return match code {
                        29 => Err(FetchError::RateLimited),
                        10 | 26 => Err(FetchError::Unauthorized), // invalid/suspended key
                        _ => Err(FetchError::Other(format!("\"error\":{code} {msg}"))),
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
    /// Max scrobble `uts` ever written. The next incremental poll asks for
    /// `from = watermark + 1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_lastfm_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_lastfm_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// The container `@attr` (all strings in the API).
struct PageMeta {
    total_pages: u32,
}

/// Parse one `user.getRecentTracks` response into (contract rows, raw track
/// objects, page meta). The now-playing row (`@attr.nowplaying == "true"`,
/// no `date`) is skipped entirely — no contract row and no raw row.
///
/// Defensive about the API's JSON quirks:
/// - `recenttracks.track` is an array, but a single object when only one
///   result — both are handled.
/// - artist is `artist.name` (extended=1) or `artist.#text` (plain) — read
///   whichever is present.
/// - album / mbids may be empty strings — omitted from the output.
fn parse_page(body: &Value) -> (Vec<MediaItem>, Vec<Value>, PageMeta) {
    let rt = body.get("recenttracks");
    let total_pages = rt
        .and_then(|r| r.get("@attr"))
        .and_then(|a| a.get("totalPages"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let meta = PageMeta { total_pages };

    let tracks: Vec<Value> = match rt.and_then(|r| r.get("track")) {
        Some(Value::Array(a)) => a.clone(),
        Some(obj @ Value::Object(_)) => vec![obj.clone()],
        _ => Vec::new(),
    };

    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for t in tracks {
        // The now-playing row carries no real scrobble time — skip it.
        let nowplaying = t
            .get("@attr")
            .and_then(|a| a.get("nowplaying"))
            .and_then(Value::as_str)
            == Some("true");
        let uts = t
            .get("date")
            .and_then(|d| d.get("uts"))
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<i64>().ok());
        let (nowplaying, uts) = match (nowplaying, uts) {
            (true, _) | (_, None) => continue, // no uts ⇒ not a completed scrobble
            (false, Some(u)) => (false, u),
        };
        let _ = nowplaying;

        let Some(item) = track_item(&t, uts) else {
            continue;
        };
        rows.push(item);
        raws.push(t);
    }
    (rows, raws, meta)
}

/// One track object → a media-plays contract row. `None` if it has no title.
fn track_item(t: &Value, uts: i64) -> Option<MediaItem> {
    let name = str_field(t, "name");
    if name.is_empty() {
        return None;
    }
    let artist = artist_name(t);
    let album = t
        .get("album")
        .and_then(|a| a.get("#text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // uts is UTC unix seconds → RFC3339 with the local offset (the same
    // epoch→local helper every collector uses; no hand-rolled tz math).
    let ts = DateTime::from_timestamp(uts, 0)?.with_timezone(&Local).to_rfc3339();

    let guid = format!("lastfm-{uts}-{}-{}", slug(&artist), slug(&name));

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    };
    put("track_mbid", &str_field(t, "mbid"));
    put(
        "artist_mbid",
        t.get("artist").and_then(|a| a.get("mbid")).and_then(Value::as_str).unwrap_or(""),
    );
    put(
        "album_mbid",
        t.get("album").and_then(|a| a.get("mbid")).and_then(Value::as_str).unwrap_or(""),
    );
    put("loved", &str_field(t, "loved"));
    put("url", &str_field(t, "url"));

    Some(MediaItem {
        ts,
        source: "lastfm".into(),
        category: "music".into(),
        device: String::new(),
        kind: "play".into(),
        title: name,
        // Grouping key for charts: the artist.
        subtitle: artist,
        detail: album,
        // Last.fm records events, not durations — an honest unknown.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

/// Artist name: `artist.name` (extended=1) preferred, `artist.#text` (plain)
/// fallback.
fn artist_name(t: &Value) -> String {
    let a = t.get("artist");
    let name = a
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty());
    let text = a.and_then(|a| a.get("#text")).and_then(Value::as_str);
    name.or(text).unwrap_or("").trim().to_string()
}

/// A top-level string field, trimmed; "" when missing or non-string.
fn str_field(t: &Value, key: &str) -> String {
    t.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Slugify for the guid: lowercase, runs of non-alphanumeric → single dash,
/// trimmed of leading/trailing dashes.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ---------------------------------------------------------------------------
// The pull.

/// Outcome of writing one batch of parsed pages.
struct WriteStats {
    scrobbles: u64,
    max_uts: Option<i64>,
}

/// Write raw + contract rows, deduped by guid against what's already on disk,
/// and report the count written and the max uts seen. Raw lines partition by
/// the same month as their contract row (the play's `uts`).
fn write_rows(vault: &Vault, rows: &[MediaItem], raws: &[Value]) -> Result<WriteStats> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids — re-runnable: a re-pull of overlapping pages never
    // duplicates. (The store appends; dedupe is the domain's job — letterboxd
    // pattern.)
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for it in contract.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                seen.insert(it.guid);
            }
        }
    }

    let mut new_rows: Vec<MediaItem> = Vec::new();
    // Raw lines, tagged with the contract ts so they partition by play month.
    let mut new_raws: Vec<RawLine> = Vec::new();
    let mut max_uts: Option<i64> = None;
    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        if let Some(u) = uts_of(raw_val) {
            max_uts = Some(max_uts.map_or(u, |m| m.max(u)));
        }
        if !seen.insert(item.guid.clone()) {
            continue; // already stored
        }
        new_rows.push(item.clone());
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;

    Ok(WriteStats { scrobbles: new_rows.len() as u64, max_uts })
}

/// A raw track object carrying the contract ts purely so the month-partition
/// writer files it under the play's month. Only `value` is serialized to disk
/// — flattened, so the raw line is the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

fn uts_of(t: &Value) -> Option<i64> {
    t.get("date")
        .and_then(|d| d.get("uts"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<i64>().ok())
}

/// Resolve credentials and sync. First run (no watermark): backfill page 1 →
/// `totalPages`. Otherwise: incremental from `watermark + 1`. Returns a
/// generic [`PullOutcome`] with a `scrobbles` count.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let api_key = resolve_api_key()
        .context("no Last.fm API key — set TROVE_LASTFM_API_KEY")?;
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .context("Last.fm is not connected — add your username in the Integrations tab")?;
    let client = LastfmClient::new(API_BASE.to_string(), api_key);
    pull_with(vault, &client, &username)
}

/// The pull body over an injected fetcher — the testable seam.
fn pull_with(vault: &Vault, client: &impl RecentTracks, username: &str) -> Result<PullOutcome> {
    let mut state = vault.read_lastfm_sync();
    // Incremental polls fetch only scrobbles strictly newer than the
    // watermark; a first run (no watermark) backfills everything.
    let from = state.watermark.map(|w| w + 1);

    let mut total_written: u64 = 0;
    let mut max_uts_overall = state.watermark;

    // Page 1 gives us totalPages; then walk 2..=totalPages.
    let mut page: u32 = 1;
    loop {
        let body = match client.recent_tracks(username, page, PAGE_SIZE, from) {
            Ok(b) => b,
            Err(e @ FetchError::Unauthorized) => bail!("Last.fm rejected the request: {e}"),
            Err(FetchError::RateLimited) => {
                // Back off once and retry the same page; the watcher will pick
                // up any remaining pages next tick.
                thread::sleep(Duration::from_secs(2));
                match client.recent_tracks(username, page, PAGE_SIZE, from) {
                    Ok(b) => b,
                    Err(e) => bail!("Last.fm rate limited: {e}"),
                }
            }
            Err(e) => bail!("Last.fm fetch failed: {e}"),
        };
        let (rows, raws, meta) = parse_page(&body);
        let stats = write_rows(vault, &rows, &raws)?;
        total_written += stats.scrobbles;
        if let Some(u) = stats.max_uts {
            max_uts_overall = Some(max_uts_overall.map_or(u, |m| m.max(u)));
        }

        let total_pages = meta.total_pages.max(1);
        if page >= total_pages {
            break;
        }
        page += 1;
        thread::sleep(REQ_INTERVAL); // ~5 req/s ceiling
    }

    // Advance the watermark to the max uts written (or seen). Only persist a
    // forward move.
    if let Some(u) = max_uts_overall {
        if state.watermark.is_none_or(|w| u > w) {
            state.watermark = Some(u);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_lastfm_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_written} scrobbles"),
        counts: BTreeMap::from([("scrobbles", total_written)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-lastfm-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A real extended=1 response: one now-playing row (no date) plus three
    /// real scrobbles across two months, modeled on the documented shape.
    fn sample_page() -> Value {
        serde_json::json!({
            "recenttracks": {
                "@attr": {
                    "user": "rj",
                    "page": "1",
                    "perPage": "200",
                    "totalPages": "1",
                    "total": "3"
                },
                "track": [
                    {
                        "@attr": { "nowplaying": "true" },
                        "artist": { "url": "https://www.last.fm/music/Tame+Impala", "mbid": "", "name": "Tame Impala" },
                        "mbid": "08b8e930-fa2d-454d-a659-069df6111a02",
                        "url": "https://www.last.fm/music/Tame+Impala/_/Apocalypse+Dreams",
                        "streamable": "0",
                        "album": { "mbid": "0d2cb66c-3a32-4f81-b977-9231c461c34a", "#text": "Lonerism" },
                        "name": "Apocalypse Dreams",
                        "loved": "0"
                    },
                    {
                        "mbid": "0632aa27-3c1f-4ca9-ae87-e656ce677bfe",
                        "loved": "1",
                        "artist": { "name": "Tame Impala", "mbid": "63aa26c3-d59b-4da4-84ac-716b54f1ef4d" },
                        "album": { "mbid": "0d2cb66c-3a32-4f81-b977-9231c461c34a", "#text": "Lonerism" },
                        "date": { "uts": "1603186824", "#text": "20 Oct 2020, 09:40" },
                        "url": "https://www.last.fm/music/Tame+Impala/_/Endors+Toi",
                        "name": "Endors Toi"
                    },
                    {
                        "mbid": "",
                        "loved": "0",
                        "artist": { "name": "Aphex Twin", "mbid": "" },
                        "album": { "mbid": "", "#text": "" },
                        "date": { "uts": "1603100424", "#text": "19 Oct 2020, 09:40" },
                        "url": "https://www.last.fm/music/Aphex+Twin/_/Xtal",
                        "name": "Xtal"
                    },
                    {
                        "mbid": "aaaa",
                        "loved": "0",
                        "artist": { "name": "Boards of Canada", "mbid": "bbbb" },
                        "album": { "mbid": "cccc", "#text": "Music Has the Right to Children" },
                        "date": { "uts": "1599300000", "#text": "05 Sep 2020, 11:20" },
                        "url": "https://www.last.fm/music/Boards+of+Canada/_/Roygbiv",
                        "name": "Roygbiv"
                    }
                ]
            }
        })
    }

    /// A single-track (non-array) response: `track` is one object, with a date.
    fn single_track_page() -> Value {
        serde_json::json!({
            "recenttracks": {
                "@attr": { "user": "rj", "page": "1", "perPage": "200", "totalPages": "1", "total": "1" },
                "track": {
                    "mbid": "single-mbid",
                    "loved": "0",
                    "artist": { "name": "Solo Artist", "mbid": "" },
                    "album": { "mbid": "", "#text": "Solo Album" },
                    "date": { "uts": "1603186824", "#text": "20 Oct 2020, 09:40" },
                    "url": "https://www.last.fm/music/Solo+Artist/_/Solo+Track",
                    "name": "Solo Track"
                }
            }
        })
    }

    #[test]
    fn parses_extended_response_and_skips_nowplaying() {
        let (rows, raws, meta) = parse_page(&sample_page());
        // 4 tracks in, 1 now-playing skipped → 3 contract rows + 3 raw rows.
        assert_eq!(rows.len(), 3, "now-playing row dropped");
        assert_eq!(raws.len(), 3, "no raw row for now-playing either");
        assert_eq!(meta.total_pages, 1);

        // First real scrobble (Endors Toi, extended artist, loved=1).
        let endors = &rows[0];
        assert_eq!(endors.title, "Endors Toi");
        assert_eq!(endors.subtitle, "Tame Impala");
        assert_eq!(endors.detail, "Lonerism");
        assert_eq!(endors.source, "lastfm");
        assert_eq!(endors.category, "music");
        assert_eq!(endors.kind, "play");
        assert_eq!(endors.seconds, 0, "events not durations");
        assert_eq!(endors.guid, "lastfm-1603186824-tame-impala-endors-toi");
        // uts 1603186824 = 2020-10-20T09:40:24Z; ts carries the local offset.
        assert_eq!(
            DateTime::parse_from_rfc3339(&endors.ts).unwrap().timestamp(),
            1603186824
        );
        assert_eq!(endors.extra.get("loved"), Some(&Value::String("1".into())));
        assert_eq!(
            endors.extra.get("track_mbid"),
            Some(&Value::String("0632aa27-3c1f-4ca9-ae87-e656ce677bfe".into()))
        );
        assert_eq!(
            endors.extra.get("artist_mbid"),
            Some(&Value::String("63aa26c3-d59b-4da4-84ac-716b54f1ef4d".into()))
        );
        assert_eq!(
            endors.extra.get("album_mbid"),
            Some(&Value::String("0d2cb66c-3a32-4f81-b977-9231c461c34a".into()))
        );
        assert_eq!(
            endors.extra.get("url"),
            Some(&Value::String("https://www.last.fm/music/Tame+Impala/_/Endors+Toi".into()))
        );

        // Xtal: empty album + empty mbids must be omitted, not stored as "".
        let xtal = &rows[1];
        assert_eq!(xtal.title, "Xtal");
        assert_eq!(xtal.subtitle, "Aphex Twin");
        assert_eq!(xtal.detail, "", "empty album omitted from detail");
        assert!(xtal.extra.get("track_mbid").is_none(), "empty mbid omitted");
        assert!(xtal.extra.get("artist_mbid").is_none());
        assert!(xtal.extra.get("album_mbid").is_none());
        assert_eq!(xtal.guid, "lastfm-1603100424-aphex-twin-xtal");
    }

    #[test]
    fn parses_single_track_object() {
        let (rows, raws, meta) = parse_page(&single_track_page());
        assert_eq!(rows.len(), 1, "single non-array track parses to one row");
        assert_eq!(raws.len(), 1);
        assert_eq!(meta.total_pages, 1);
        assert_eq!(rows[0].title, "Solo Track");
        assert_eq!(rows[0].subtitle, "Solo Artist");
        assert_eq!(rows[0].detail, "Solo Album");
        assert_eq!(rows[0].guid, "lastfm-1603186824-solo-artist-solo-track");
    }

    /// A one-page fetcher that ignores `from` — enough for the store/cursor
    /// test (the watermark logic is exercised by re-running).
    struct StubClient {
        page: Value,
    }
    impl RecentTracks for StubClient {
        fn recent_tracks(
            &self,
            _user: &str,
            _page: u32,
            _limit: u32,
            _from: Option<i64>,
        ) -> Result<Value, FetchError> {
            Ok(self.page.clone())
        }
    }

    #[test]
    fn writes_partitioned_layers_dedupes_and_advances_cursor() {
        let v = temp_vault("store");
        let client = StubClient { page: sample_page() };

        let out = pull_with(&v, &client, "rj").unwrap();
        assert_eq!(out.counts.get("scrobbles"), Some(&3));

        // Contract layer, partitioned by the play's month.
        let oct = std::fs::read_to_string(v.root().join("media/plays/lastfm/2020-10.jsonl")).unwrap();
        assert_eq!(oct.lines().count(), 2, "Endors Toi + Xtal land in Oct");
        let sep = std::fs::read_to_string(v.root().join("media/plays/lastfm/2020-09.jsonl")).unwrap();
        assert_eq!(sep.lines().count(), 1, "Roygbiv lands in Sep");

        // Raw layer mirrors the partitioning under raw/, verbatim objects.
        let oct_raw =
            std::fs::read_to_string(v.root().join("media/plays/lastfm/raw/2020-10.jsonl")).unwrap();
        assert_eq!(oct_raw.lines().count(), 2);
        assert!(oct_raw.contains("\"uts\":\"1603186824\""), "raw is the API object verbatim");
        assert!(oct_raw.contains("\"#text\":\"20 Oct 2020, 09:40\""));
        assert!(!oct_raw.contains("Apocalypse Dreams"), "now-playing produced no raw row");

        // Cursor advanced to the max uts written.
        let state = v.read_lastfm_sync();
        assert_eq!(state.watermark, Some(1603186824));
        assert!(state.updated.is_some());

        // Re-run with the same input → guid dedupe, no duplicate contract rows.
        let again = pull_with(&v, &client, "rj").unwrap();
        assert_eq!(again.counts.get("scrobbles"), Some(&0), "all guids already stored");
        let oct2 = std::fs::read_to_string(v.root().join("media/plays/lastfm/2020-10.jsonl")).unwrap();
        assert_eq!(oct, oct2, "contract file byte-identical after re-run");

        // And the unified media stream sees the scrobbles via the contract arm.
        let day = v.media_timeline("2020-10-20").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "lastfm");
        assert_eq!(day[0].title, "Endors Toi");
        assert_eq!(day[0].category, "music");
    }

    #[test]
    fn slug_collapses_and_trims() {
        assert_eq!(slug("Tame Impala"), "tame-impala");
        assert_eq!(slug("Sigur Rós"), "sigur-r-s"); // non-ascii → dash
        assert_eq!(slug("  A.B.C!  "), "a-b-c");
        assert_eq!(slug("***"), "");
        assert_eq!(slug("Godspeed You! Black Emperor"), "godspeed-you-black-emperor");
    }

    #[test]
    fn connection_exposes_token_paste_and_disconnect_forgets_username() {
        assert!(CONNECTION.method("token-paste").is_some());
        let v = temp_vault("conn");
        // No api_key in this environment → store the username anyway.
        def_connect(&v, "rj").unwrap();
        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "rj");
        assert_eq!(status.accounts[0].key, "lastfm");
        assert!(!status.accounts[0].needs_reconnect);
        def_disconnect(&v, "lastfm").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_username_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        // Not connected: pull errors clearly regardless of key presence.
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("not connected") || err.contains("API key"),
            "clear error, no panic: {err}"
        );
    }
}
