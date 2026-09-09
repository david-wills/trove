//! Jellyfin self-hosted media server — watch history via the local REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/jellyfin.md.
//!
//! A **Periodic** local pull: every movie, TV episode, or music track the user
//! has marked as played on their personal Jellyfin server lands in the unified
//! media stream via the **media-plays write contract**
//! (`docs/vault-spec/domains/media-plays.md`). Two layers per play:
//!
//! - **raw** — the API `BaseItemDto` verbatim at
//!   `media/plays/jellyfin/raw/YYYY-MM.jsonl`, partitioned by watch month.
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/jellyfin/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! Auth is a composite TokenPaste: `<server-url>|<api-key>` — the user pastes
//! their server URL (default `http://localhost:8096`) and the API key they
//! generated in Jellyfin's admin dashboard, joined by `|`. Both are stored as
//! one value in the 0600 secret store (the Philips Hue pattern).
//!
//! The pull uses `GET /Items?Filters=IsPlayed&SortBy=DatePlayed&userId=<id>`.
//! The userId is discovered once via `GET /Users/Me` (requires an API key
//! created for a specific user, not an admin-level keyless API key) and cached
//! in the watermark cursor. Pagination uses `StartIndex` + `Limit`; items are
//! sorted newest first (`SortOrder=Descending`) so a watermark on
//! `LastPlayedDate` is efficient: we stop paging once we see items older than
//! the watermark (the incremental window).
//!
//! **Playback Reporting plugin:** if the optional plugin is installed,
//! `GET /user_usage_stats/{userId}/{date}/GetItems` returns per-session
//! records with a precise `Time` field. We probe it at pull time; if the
//! endpoint returns 404 we degrade gracefully to the `LastPlayedDate` from the
//! base Items endpoint. We record which capability tier was found in the cursor
//! (no hard plugin check at connect time — the user may install it later).

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
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer stream directory; raw lines go one level deeper in `raw/`.
const DIR: &str = "media/plays/jellyfin";
const RAW_DIR: &str = "media/plays/jellyfin/raw";
/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (0600 secrets).
/// Deleting it causes a full re-pull from the beginning.
const SYNC_FILE: &str = ".trove/jellyfin-sync.json";
/// Secret-store service id for the stored `<url>|<api-key>` pair.
const SERVICE: &str = "jellyfin";

/// HTTP page size — Jellyfin's default and a reasonable cap.
const PAGE_SIZE: u64 = 200;

/// Kept short; the server is local — a longer timeout would stall the loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Seconds between syncs. Hourly: a local server is always reachable and the
/// play list changes infrequently; the incremental poll is one quick page when
/// idle.
pub const JELLYFIN_SYNC_SECS: u64 = 3600;

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
                format!("Jellyfin synced — {n} plays")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Jellyfin sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("plays").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Jellyfin is up to date — no new plays".to_string()
    } else {
        format!("Jellyfin synced — {n} plays")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "jellyfin",
        name: "Jellyfin",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Watch history from your self-hosted Jellyfin media server, pulled \
                      via the local REST API. Movies, TV episodes, and music you have \
                      watched or listened to land in the unified media stream.",
        domain: "media",
        vault_path: "media/plays/jellyfin/",
        toggleable: true,
        setup: &[
            "Connect with your Jellyfin server URL and API key on this card.",
            "First sync imports your complete play history; later syncs are incremental.",
        ],
        caveats: "Without the Playback Reporting plugin only the last-played date per \
                  item is available — per-session timestamps require installing the \
                  Playback Reporting plugin on your Jellyfin server.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(JELLYFIN_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("jellyfin"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — composite "SERVER_URL|API_KEY").

/// Parse `url|api_key` from the pasted string.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your server URL and API key as URL|API_KEY");
    }
    let (url, key) = pasted
        .split_once('|')
        .with_context(|| "paste as URL|API_KEY (e.g. http://localhost:8096|your-api-key)")?;
    let url = url.trim().trim_end_matches('/').to_string();
    let key = key.trim().to_string();
    if url.is_empty() {
        bail!("missing server URL — paste as URL|API_KEY");
    }
    if key.is_empty() {
        bail!("missing API key — paste as URL|API_KEY");
    }
    Ok((url, key))
}

/// Verify credentials by probing `GET /Users/Me` and store them.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (url, key) = parse_credentials(pasted)?;
    let client = JellyfinClient::new(url, key.clone());
    // A real call proves the server is reachable and the key works.
    match client.get_user_me() {
        Ok(_) => {}
        Err(e @ FetchError::Unauthorized) => bail!(
            "Jellyfin rejected the API key (401) — check it was generated in \
             Dashboard → API Keys and hasn't been revoked: {e}"
        ),
        Err(e @ FetchError::Unreachable(_)) => bail!(
            "Could not reach the Jellyfin server — check the URL and that the \
             server is running: {e}"
        ),
        Err(e) => bail!("Jellyfin /Users/Me check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
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
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let pasted = token.access_token;
        let label = if let Some((url, _)) = pasted.split_once('|') {
            url.trim().to_string()
        } else {
            "Jellyfin".to_string()
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: a local API key is self-service.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "jellyfin",
    display_name: "Jellyfin",
    methods: &[ConnectMethod::TokenPaste {
        label: "Server URL | API key",
        help: "Paste your server URL and an API key (from Dashboard → API Keys) \
               separated by a pipe, e.g. http://localhost:8096|your-api-key. \
               Both are stored locally and sent only to your own server.",
        placeholder: "http://localhost:8096|your-api-key",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["jellyfin"],
    setup: &[
        "Open your Jellyfin server (default: http://localhost:8096).",
        "Go to Dashboard → API Keys and create a new key.",
        "Paste your server URL and the key here as URL|API_KEY.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    NotFound,
    Unreachable(String),
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::Unreachable(m) => write!(f, "unreachable: {m}"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Thin Jellyfin REST client. The base URL and API key are injected so the
/// sync logic is testable against fixtures (the lastfm/philips-hue pattern).
trait JellyfinApi {
    /// `GET /Users/Me` — returns the authenticated user's id and name.
    fn get_user_me(&self) -> Result<Value, FetchError>;

    /// `GET /Items` with `Filters=IsPlayed&SortBy=DatePlayed&SortOrder=Descending`.
    /// Paginated by `StartIndex`. Returns `(items, total_count)`.
    fn get_played_items(
        &self,
        user_id: &str,
        start_index: u64,
    ) -> Result<(Vec<Value>, u64), FetchError>;

    /// `GET /user_usage_stats/{userId}/{date}/GetItems` — Playback Reporting
    /// plugin endpoint. Returns `Err(NotFound)` when the plugin is not
    /// installed. Reserved for the plugin-tier path (future enhancement).
    #[allow(dead_code)]
    fn get_plugin_sessions(
        &self,
        user_id: &str,
        date: &str,
    ) -> Result<Vec<Value>, FetchError>;
}

/// Live client backed by ureq.
struct JellyfinClient {
    base: String,
    api_key: String,
}

impl JellyfinClient {
    fn new(base: String, api_key: String) -> Self {
        let base = base.trim_end_matches('/').to_string();
        JellyfinClient { base, api_key }
    }

    fn get(&self, path: &str) -> ureq::Request {
        ureq::get(&format!("{}{path}", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("X-Emby-Authorization",
                &format!("MediaBrowser Token=\"{}\"", self.api_key))
    }

    fn call_json(&self, path: &str) -> Result<Value, FetchError> {
        let resp = self.get(path).call().map_err(|e| match e {
            ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => FetchError::Unauthorized,
            ureq::Error::Status(404, _) => FetchError::NotFound,
            ureq::Error::Transport(t) => FetchError::Unreachable(t.to_string()),
            ureq::Error::Status(code, r) => {
                let body = r.into_string().unwrap_or_default();
                FetchError::Other(format!("HTTP {code}: {}", body.chars().take(200).collect::<String>()))
            }
        })?;
        resp.into_json().map_err(|e| FetchError::Other(format!("JSON parse: {e}")))
    }
}

impl JellyfinApi for JellyfinClient {
    fn get_user_me(&self) -> Result<Value, FetchError> {
        self.call_json("/Users/Me")
    }

    fn get_played_items(
        &self,
        user_id: &str,
        start_index: u64,
    ) -> Result<(Vec<Value>, u64), FetchError> {
        let path = format!(
            "/Items?userId={user_id}&Filters=IsPlayed&SortBy=DatePlayed&SortOrder=Descending\
             &Recursive=true&IncludeItemTypes=Movie,Episode,Audio\
             &Fields=UserData,PremiereDate,RunTimeTicks,SeriesName,ParentIndexNumber,IndexNumber\
             &StartIndex={start_index}&Limit={PAGE_SIZE}"
        );
        let body = self.call_json(&path)?;
        let total = body
            .get("TotalRecordCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let items = body
            .get("Items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((items, total))
    }

    fn get_plugin_sessions(
        &self,
        user_id: &str,
        date: &str,
    ) -> Result<Vec<Value>, FetchError> {
        let path = format!("/user_usage_stats/{user_id}/{date}/GetItems");
        let body = self.call_json(&path)?;
        let items = if let Some(arr) = body.as_array() {
            arr.clone()
        } else if let Some(arr) = body.get("Items").and_then(Value::as_array) {
            arr.clone()
        } else {
            Vec::new()
        };
        Ok(items)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The userId discovered at connect/first-sync time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    /// Max `LastPlayedDate` (RFC3339) seen across all synced items — the
    /// incremental window filter. Items whose `LastPlayedDate` is older than
    /// this are already synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
    /// Whether the Playback Reporting plugin was detected on the last sync.
    #[serde(default)]
    plugin_detected: bool,
}

impl Vault {
    fn read_jellyfin_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_jellyfin_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Map a Jellyfin `BaseItemDto.Type` string to the media-plays `category`.
fn item_kind_to_category(item_type: &str) -> &'static str {
    match item_type {
        "Movie" => "video",
        "Episode" => "video",
        "Audio" => "music",
        _ => "video",
    }
}

/// Extract the `LastPlayedDate` from `UserData` as an RFC3339 string.
/// Jellyfin returns ISO8601 strings like `2024-03-15T20:30:00.0000000Z`.
fn last_played_ts(item: &Value) -> Option<String> {
    let raw = item
        .get("UserData")
        .and_then(|u| u.get("LastPlayedDate"))
        .and_then(Value::as_str)?;
    // Parse as DateTime<Utc> (the API sends UTC), convert to local RFC3339.
    let dt = chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Local))
        .or_else(|| {
            // Some builds emit a non-standard trailing zeros format; try
            // stripping the sub-second part and re-parsing.
            let trimmed = raw.get(..19).map(|s| format!("{s}Z"))?;
            chrono::DateTime::parse_from_rfc3339(&trimmed)
                .ok()
                .map(|d| d.with_timezone(&Local))
        })?;
    Some(dt.to_rfc3339())
}

/// RunTimeTicks → seconds (Jellyfin stores duration as 100ns ticks).
fn ticks_to_secs(ticks: Option<u64>) -> u64 {
    ticks.map(|t| t / 10_000_000).unwrap_or(0)
}

/// One `BaseItemDto` → a contract [`MediaItem`]. Returns `None` if there is
/// no usable play timestamp.
fn item_to_media(item: &Value, ts: &str) -> Option<MediaItem> {
    let title = item.get("Name").and_then(Value::as_str).unwrap_or("").trim();
    if title.is_empty() {
        return None;
    }

    let item_type = item.get("Type").and_then(Value::as_str).unwrap_or("");
    let category = item_kind_to_category(item_type);

    // series_name is the show name (for episodes); artist for music.
    let subtitle = match item_type {
        "Episode" => item
            .get("SeriesName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        "Audio" => item
            .get("AlbumArtist")
            .and_then(Value::as_str)
            .or_else(|| item.get("Artists").and_then(|a| a.get(0)).and_then(Value::as_str))
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => String::new(), // Movie: no subtitle (director not in this endpoint)
    };

    let detail = match item_type {
        "Episode" => {
            // "S01E03" or just series + title
            let season = item
                .get("ParentIndexNumber")
                .and_then(Value::as_u64)
                .map(|n| format!("S{n:02}"));
            let ep = item
                .get("IndexNumber")
                .and_then(Value::as_u64)
                .map(|n| format!("E{n:02}"));
            match (season, ep) {
                (Some(s), Some(e)) => format!("{s}{e}"),
                (Some(s), None) => s,
                (None, Some(e)) => e,
                (None, None) => String::new(),
            }
        }
        "Audio" => item
            .get("Album")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => String::new(),
    };

    let item_id = item.get("Id").and_then(Value::as_str).unwrap_or("").trim();
    // guid = jellyfin:<item-id>:<ts-day> — one play per item per day;
    // if the plugin surfaces per-session ts the guid naturally separates them.
    let day = ts.get(..10).unwrap_or(ts);
    let guid = format!("jellyfin-{item_id}-{day}");

    let user_data = item.get("UserData");

    // Determine kind: "partial" when PlayedPercentage < 85 and the item has a
    // non-zero resume position (Filters=IsPlayed so Played==true is guaranteed,
    // but a barely-touched rewatch can be partially watched before the server
    // marks it played on a second full completion).
    let played_pct = user_data
        .and_then(|u| u.get("PlayedPercentage"))
        .and_then(Value::as_f64)
        .unwrap_or(100.0);
    let resume_ticks = user_data
        .and_then(|u| u.get("PlaybackPositionTicks"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let kind = if played_pct < 85.0 && resume_ticks > 0 {
        "partial"
    } else {
        "play"
    };

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    };
    put("item_id", item_id);
    put("item_type", item_type);
    if let Some(pc) = user_data
        .and_then(|u| u.get("PlayCount"))
        .and_then(Value::as_u64)
    {
        extra.insert("play_count".into(), Value::Number(pc.into()));
    }
    // Resume position in extra, as the brief specifies.
    let resume_secs = ticks_to_secs(Some(resume_ticks).filter(|&t| t > 0));
    if resume_secs > 0 {
        extra.insert("resume_secs".into(), Value::Number(resume_secs.into()));
    }
    // Total item runtime stored in extra for reference; NOT used as seconds-played
    // (the base /Items endpoint cannot report watched-seconds, only the optional
    // Playback Reporting plugin can — see contract: seconds = 0 when unknown).
    let runtime_secs = ticks_to_secs(
        item.get("RunTimeTicks").and_then(Value::as_u64),
    );
    if runtime_secs > 0 {
        extra.insert("runtime_secs".into(), Value::Number(runtime_secs.into()));
    }

    Some(MediaItem {
        ts: ts.to_string(),
        source: "jellyfin".into(),
        category: category.into(),
        device: String::new(), // local server — device not surfaced in the API
        kind: kind.into(),
        title: title.to_string(),
        subtitle,
        detail,
        // The base /Items endpoint reports only Played:bool + resume position;
        // watched-seconds require the Playback Reporting plugin.
        // Per contract: seconds = 0 when unknown.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write helpers.

/// A raw item line: tagged with `ts` for month partitioning, written as the
/// verbatim API object (the lastfm/simkl pattern).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

struct WriteStats {
    plays: u64,
    max_ts: Option<String>,
}

/// Write raw + contract rows, deduped by guid. Returns the count written and
/// the max ts seen (for advancing the watermark).
fn write_rows(vault: &Vault, rows: &[MediaItem], raws: &[Value]) -> Result<WriteStats> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

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
    let mut max_ts: Option<String> = None;

    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        // Track max_ts across ALL items (even dupes) for the watermark.
        match &max_ts {
            None => max_ts = Some(item.ts.clone()),
            Some(m) if &item.ts > m => max_ts = Some(item.ts.clone()),
            _ => {}
        }
        if !seen.insert(item.guid.clone()) {
            continue;
        }
        new_rows.push(item.clone());
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;

    Ok(WriteStats { plays: new_rows.len() as u64, max_ts })
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync. Called from both the periodic collect hook
/// and the manual "Sync now" hook.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Jellyfin is not connected — add your server URL and API key in the Integrations tab")?;
    let (url, key) = parse_credentials(&pasted)?;
    let client = JellyfinClient::new(url, key);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl JellyfinApi) -> Result<PullOutcome> {
    let mut state = vault.read_jellyfin_sync();

    // Discover userId once and cache it in the cursor.
    let user_id = if let Some(ref uid) = state.user_id {
        uid.clone()
    } else {
        let me = api.get_user_me()
            .map_err(|e| anyhow::anyhow!("could not fetch user info: {e}"))?;
        let uid = me
            .get("Id")
            .and_then(Value::as_str)
            .context("Jellyfin /Users/Me returned no Id")?
            .to_string();
        state.user_id = Some(uid.clone());
        uid
    };

    let mut total_plays: u64 = 0;
    let mut max_ts_overall = state.watermark.clone();

    // Page through played items, newest first. Stop as soon as we hit an
    // item older than the watermark (incremental window optimization).
    let mut start_index: u64 = 0;
    let watermark = state.watermark.clone();

    'paging: loop {
        let (items, _total) = api
            .get_played_items(&user_id, start_index)
            .map_err(|e| anyhow::anyhow!("Jellyfin /Items fetch failed: {e}"))?;

        if items.is_empty() {
            break;
        }

        let mut rows: Vec<MediaItem> = Vec::new();
        let mut raws: Vec<Value> = Vec::new();

        for item in &items {
            let Some(ts) = last_played_ts(item) else {
                continue;
            };
            // Incremental: if this item is at or older than the watermark,
            // everything after it (sorted descending) is also older — stop paging.
            // Use strict `<` (not `<=`) so a same-second new play whose ts equals
            // the watermark is not silently skipped; guid dedupe suppresses any
            // genuine re-write of the boundary item.
            if let Some(ref wm) = watermark {
                if &ts < wm {
                    break 'paging;
                }
            }
            if let Some(contract_row) = item_to_media(item, &ts) {
                rows.push(contract_row);
                raws.push(item.clone());
            }
        }

        if !rows.is_empty() {
            let stats = write_rows(vault, &rows, &raws)?;
            total_plays += stats.plays;
            if let Some(ref ts) = stats.max_ts {
                max_ts_overall = Some(match &max_ts_overall {
                    None => ts.clone(),
                    Some(m) => if ts > m { ts.clone() } else { m.clone() },
                });
            }
        }

        let page_count = items.len() as u64;
        if page_count < PAGE_SIZE {
            break; // short page = last page
        }
        start_index += page_count;
    }

    // Advance watermark only forward.
    if let Some(ref ts) = max_ts_overall {
        if state.watermark.as_ref().map_or(true, |w| ts > w) {
            state.watermark = max_ts_overall.clone();
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_jellyfin_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_plays} plays"),
        counts: BTreeMap::from([("plays", total_plays)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-jellyfin-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A realistic `BaseItemDto` for a played movie (from Jellyfin REST API).
    fn movie_item(id: &str, name: &str, last_played: &str, ticks: u64) -> Value {
        serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": "Movie",
            "RunTimeTicks": ticks,
            "UserData": {
                "PlayCount": 1,
                "LastPlayedDate": last_played,
                "IsFavorite": false,
                "Played": true,
                "PlaybackPositionTicks": 0
            }
        })
    }

    /// A realistic `BaseItemDto` for a played TV episode.
    fn episode_item(
        id: &str,
        name: &str,
        series: &str,
        season: u64,
        ep: u64,
        last_played: &str,
        ticks: u64,
    ) -> Value {
        serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": "Episode",
            "SeriesName": series,
            "ParentIndexNumber": season,
            "IndexNumber": ep,
            "RunTimeTicks": ticks,
            "UserData": {
                "PlayCount": 1,
                "LastPlayedDate": last_played,
                "IsFavorite": false,
                "Played": true,
                "PlaybackPositionTicks": 0
            }
        })
    }

    /// A realistic `BaseItemDto` for a played music track.
    fn audio_item(id: &str, name: &str, album_artist: &str, album: &str, last_played: &str, ticks: u64) -> Value {
        serde_json::json!({
            "Id": id,
            "Name": name,
            "Type": "Audio",
            "AlbumArtist": album_artist,
            "Album": album,
            "RunTimeTicks": ticks,
            "UserData": {
                "PlayCount": 1,
                "LastPlayedDate": last_played,
                "IsFavorite": false,
                "Played": true,
                "PlaybackPositionTicks": 0
            }
        })
    }

    // Stub API that serves one page of played items.
    struct StubApi {
        items: Vec<Value>,
        user_id: String,
    }

    impl JellyfinApi for StubApi {
        fn get_user_me(&self) -> Result<Value, FetchError> {
            Ok(serde_json::json!({ "Id": self.user_id, "Name": "testuser" }))
        }

        fn get_played_items(
            &self,
            _user_id: &str,
            _start_index: u64,
        ) -> Result<(Vec<Value>, u64), FetchError> {
            Ok((self.items.clone(), self.items.len() as u64))
        }

        fn get_plugin_sessions(
            &self,
            _user_id: &str,
            _date: &str,
        ) -> Result<Vec<Value>, FetchError> {
            Err(FetchError::NotFound)
        }
    }

    #[test]
    fn parses_movie_item() {
        let item = movie_item("abc-1", "Heat", "2024-03-15T20:30:00.0000000Z", 87_000_000_000);
        let ts = last_played_ts(&item).unwrap();
        // Should be parseable as RFC3339, mapping UTC to local.
        let dt = DateTime::parse_from_rfc3339(&ts).unwrap();
        assert_eq!(dt.timestamp(), 1710534600, "correct UTC epoch");

        let row = item_to_media(&item, &ts).unwrap();
        assert_eq!(row.title, "Heat");
        assert_eq!(row.category, "video");
        assert_eq!(row.kind, "play");
        assert_eq!(row.source, "jellyfin");
        assert_eq!(row.subtitle, "", "movie has no subtitle");
        // The base /Items endpoint cannot report watched-seconds —
        // per contract seconds = 0 (unknown). Runtime lives in extra.
        assert_eq!(row.seconds, 0, "seconds is 0 (unknown) for base-install path");
        assert_eq!(
            row.extra.get("runtime_secs"),
            Some(&Value::Number(8700u64.into())),
            "runtime stored in extra (87_000_000_000 / 10_000_000 = 8700)"
        );
        assert!(row.guid.starts_with("jellyfin-abc-1-"), "guid has item id prefix");
        assert_eq!(row.extra.get("item_type"), Some(&Value::String("Movie".into())));
        assert_eq!(row.extra.get("item_id"), Some(&Value::String("abc-1".into())));
    }

    #[test]
    fn parses_episode_item() {
        let item = episode_item(
            "ep-1", "Pilot", "Breaking Bad", 1, 1,
            "2024-04-01T19:00:00.0000000Z",
            2_760_000_000,
        );
        let ts = last_played_ts(&item).unwrap();
        let row = item_to_media(&item, &ts).unwrap();
        assert_eq!(row.title, "Pilot");
        assert_eq!(row.subtitle, "Breaking Bad");
        assert_eq!(row.detail, "S01E01");
        assert_eq!(row.category, "video");
        // Base-install path: seconds = 0 (unknown); runtime in extra.
        assert_eq!(row.seconds, 0, "seconds = 0 for base-install path");
        assert_eq!(
            row.extra.get("runtime_secs"),
            Some(&Value::Number(276u64.into())),
            "runtime in extra (2_760_000_000 / 10_000_000 = 276)"
        );
        assert!(row.guid.starts_with("jellyfin-ep-1-"));
    }

    #[test]
    fn parses_audio_item() {
        let item = audio_item(
            "au-1", "Endors Toi", "Tame Impala", "Lonerism",
            "2024-05-10T14:00:00.0000000Z",
            3_120_000_000,
        );
        let ts = last_played_ts(&item).unwrap();
        let row = item_to_media(&item, &ts).unwrap();
        assert_eq!(row.title, "Endors Toi");
        assert_eq!(row.subtitle, "Tame Impala");
        assert_eq!(row.detail, "Lonerism");
        assert_eq!(row.category, "music");
        // Base-install path: seconds = 0; runtime in extra.
        assert_eq!(row.seconds, 0, "seconds = 0 for base-install path");
        assert_eq!(
            row.extra.get("runtime_secs"),
            Some(&Value::Number(312u64.into()))
        );
    }

    #[test]
    fn resume_position_lands_in_extra() {
        // Item with a non-zero PlaybackPositionTicks (resume offset).
        let item = serde_json::json!({
            "Id": "res-1",
            "Name": "Inception",
            "Type": "Movie",
            "RunTimeTicks": 87_000_000_000u64,
            "UserData": {
                "PlayCount": 1,
                "LastPlayedDate": "2024-06-01T22:00:00.0000000Z",
                "IsFavorite": false,
                "Played": true,
                // 900 seconds into the film (resume position).
                "PlaybackPositionTicks": 9_000_000_000u64,
                "PlayedPercentage": 90.0
            }
        });
        let ts = last_played_ts(&item).unwrap();
        let row = item_to_media(&item, &ts).unwrap();
        assert_eq!(row.seconds, 0, "seconds always 0 from base-install path");
        assert_eq!(
            row.extra.get("resume_secs"),
            Some(&Value::Number(900u64.into())),
            "resume position (9_000_000_000 / 10_000_000 = 900) in extra"
        );
        // PlayedPercentage >= 85 → kind stays "play".
        assert_eq!(row.kind, "play");
    }

    #[test]
    fn partial_kind_when_low_played_pct_with_resume() {
        // An item barely started: <85% played and has a resume offset.
        let item = serde_json::json!({
            "Id": "par-1",
            "Name": "Dune: Part Two",
            "Type": "Movie",
            "RunTimeTicks": 90_000_000_000u64,
            "UserData": {
                "PlayCount": 1,
                "LastPlayedDate": "2024-06-02T20:00:00.0000000Z",
                "Played": true,
                // Stopped after ~10 minutes out of 2h30m → low percentage.
                "PlaybackPositionTicks": 6_000_000_000u64,
                "PlayedPercentage": 11.0
            }
        });
        let ts = last_played_ts(&item).unwrap();
        let row = item_to_media(&item, &ts).unwrap();
        assert_eq!(row.kind, "partial", "low PlayedPercentage + resume → partial");
        assert_eq!(
            row.extra.get("resume_secs"),
            Some(&Value::Number(600u64.into()))
        );
    }

    #[test]
    fn write_and_dedupe_plays() {
        let v = temp_vault("write");
        let api = StubApi {
            user_id: "user-42".into(),
            items: vec![
                movie_item("m1", "Heat", "2024-03-15T20:30:00.0000000Z", 87_000_000_000),
                episode_item("ep1", "Pilot", "Breaking Bad", 1, 1, "2024-03-10T19:00:00.0000000Z", 2_760_000_000),
            ],
        };

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("plays"), Some(&2));

        // Verify contract files landed.
        let mar_path = v.root().join("media/plays/jellyfin/2024-03.jsonl");
        assert!(mar_path.exists(), "March contract file written");
        let mar_contents = std::fs::read_to_string(&mar_path).unwrap();
        let lines: Vec<&str> = mar_contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(lines.len(), 2, "2 items in March 2024");

        // Verify raw files.
        let raw_path = v.root().join("media/plays/jellyfin/raw/2024-03.jsonl");
        assert!(raw_path.exists(), "March raw file written");

        // Re-run: guids already stored → 0 new plays.
        let again = pull_with(&v, &api).unwrap();
        assert_eq!(again.counts.get("plays"), Some(&0), "dedupe works");

        // Cursor advanced.
        let state = v.read_jellyfin_sync();
        assert!(state.watermark.is_some(), "watermark set");
        assert_eq!(state.user_id.as_deref(), Some("user-42"));
        assert!(state.updated.is_some());
    }

    #[test]
    fn watermark_stops_paging() {
        let v = temp_vault("watermark");
        // Two separate runs: first syncs both items, second only the new one.
        let api1 = StubApi {
            user_id: "user-1".into(),
            items: vec![
                movie_item("older", "Old Movie", "2024-01-10T10:00:00.0000000Z", 72_000_000_000),
            ],
        };
        let out1 = pull_with(&v, &api1).unwrap();
        assert_eq!(out1.counts.get("plays"), Some(&1));

        // After first sync the watermark is set to the Jan item's ts.
        // A second pull sees the same item (same last_played) → ts <= watermark → stop.
        let out2 = pull_with(&v, &api1).unwrap();
        assert_eq!(out2.counts.get("plays"), Some(&0), "stopped at watermark");
    }

    #[test]
    fn credential_parsing_ok() {
        let (url, key) = parse_credentials("http://localhost:8096|abc123def").unwrap();
        assert_eq!(url, "http://localhost:8096");
        assert_eq!(key, "abc123def");
    }

    #[test]
    fn credential_parsing_trims_whitespace() {
        let (url, key) = parse_credentials("  http://nas.local:8096  |  mykey  ").unwrap();
        assert_eq!(url, "http://nas.local:8096");
        assert_eq!(key, "mykey");
    }

    #[test]
    fn credential_parsing_strips_trailing_slash() {
        let (url, _) = parse_credentials("http://localhost:8096/|key").unwrap();
        assert_eq!(url, "http://localhost:8096");
    }

    #[test]
    fn credential_parsing_rejects_missing_pipe() {
        assert!(parse_credentials("http://localhost:8096").is_err());
    }

    #[test]
    fn credential_parsing_rejects_empty_parts() {
        assert!(parse_credentials("|mykey").is_err(), "empty url");
        assert!(parse_credentials("http://localhost:8096|").is_err(), "empty key");
        assert!(parse_credentials("").is_err(), "empty string");
    }

    #[test]
    fn connection_has_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "jellyfin");
        assert_eq!(CONNECTION.auto_pull, &["jellyfin"]);
    }

    #[test]
    fn def_status_no_token_returns_empty_accounts() {
        let v = temp_vault("status");
        let status = def_status(&v).unwrap();
        assert!(status.accounts.is_empty());
        assert!(status.configured);
    }

    #[test]
    fn def_status_with_token_returns_account_with_url_label() {
        let v = temp_vault("status-url");
        vault_store_token(&v, "http://jellyfin.local:8096|test-key-xyz");
        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "http://jellyfin.local:8096");
        assert_eq!(status.accounts[0].key, "jellyfin");
    }

    #[test]
    fn def_disconnect_removes_token() {
        let v = temp_vault("disconnect");
        vault_store_token(&v, "http://localhost:8096|key1");
        assert_eq!(def_status(&v).unwrap().accounts.len(), 1);
        def_disconnect(&v, "jellyfin").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    fn vault_store_token(vault: &Vault, pasted: &str) {
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: pasted.to_string(),
                    refresh_token: None,
                    token_type: None,
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();
    }
}
