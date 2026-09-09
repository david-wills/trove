//! Steam — Valve's PC gaming platform. Syncs owned games, playtime, and
//! achievements via the official Web API (api.steampowered.com).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/steam.md
//!
//! A **Periodic** cloud pull. `gaming/` is **raw-only** in the taxonomy — there
//! is NO write-time contract. This collector writes full-fidelity raw JSONL and
//! touches no `DOMAINS` struct or spec. Two streams:
//!
//! - **library** — one snapshot per pull at
//!   `gaming/steam/library/YYYY-MM.jsonl`, partitioned by the LOCAL month of
//!   the poll, one JSON line per owned game (appid, name, playtime_forever,
//!   playtime_2weeks?, img_icon_url?, img_logo_url?). Full fidelity; extra API
//!   fields overflow verbatim. The same appid appears once per monthly
//!   partition file; repeated polls within the same month UPSERT (overwrite)
//!   the row so the snapshot is always current for that month.
//!
//! - **achievements** — per-game unlock events at
//!   `gaming/steam/achievements/YYYY-MM.jsonl`, partitioned by the UNLOCK
//!   month (Unix `unlocktime`). guid = `appid:apiname` — stable across
//!   re-polls (never reassigned). Only already-unlocked achievements (`achieved
//!   == 1`) are written; a re-pull of the same appid dedupes on guid.
//!
//! Auth: a free Steam Web API key from steamcommunity.com/dev (any domain
//! string accepted) plus the user's SteamID64 or vanity URL handle. The
//! composite credential is stored as `KEY STEAMID64` (space-separated) in the
//! `access_token` slot of a never-expiring [`crate::sync::oauth::TokenSet`]
//! under `.trove/sync/steam.json`. At connect time a vanity handle is
//! resolved to a SteamID64 via `ISteamUser/ResolveVanityURL/v1` and the
//! numeric id is what is stored, so pulls never have to re-resolve.
//!
//! Watermark: a rebuildable cursor at `.trove/steam-sync.json` (non-secret).
//! Stores the last-library-snapshot time. Achievements are deduped on
//! guid (appid:apiname) across all partition files.
//!
//! No rate limit is officially published; practical cap ~100k req/day (well
//! above any personal use). A 200 ms inter-request throttle is polite.
//!
//! JSON shapes confirmed against the TeamFortress wiki
//! (wiki.teamfortress.com/wiki/WebAPI/GetOwnedGames,
//! wiki.teamfortress.com/wiki/WebAPI/GetPlayerAchievements,
//! wiki.teamfortress.com/wiki/WebAPI/ResolveVanityURL):
//!
//! `GetOwnedGames` response: `{"response": {"game_count": N, "games": [...]}}`.
//! Game object: `appid` (u32), `name` (string, only when include_appinfo=1),
//! `playtime_forever` (u32, minutes), `playtime_2weeks` (u32, optional),
//! `img_icon_url` (string, optional), `img_logo_url` (string, optional),
//! `has_community_visible_stats` (bool, optional).
//!
//! `GetPlayerAchievements` response: `{"playerstats": {"steamID": "...",
//! "gameName": "...", "achievements": [...], "success": true/false}}`.
//! Achievement object: `apiname` (string), `achieved` (0/1 integer),
//! `unlocktime` (u64, Unix seconds — 0 when not unlocked), `name` (string,
//! localized), `description` (string, localized).
//!
//! `ResolveVanityURL` response: `{"response": {"steamid": "...", "success": 1}}`.
//! success=42 means no match; success=1 means found.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Month-partitioned library snapshots.
const LIBRARY_DIR: &str = "gaming/steam/library";
/// Month-partitioned achievement unlocks.
const ACH_DIR: &str = "gaming/steam/achievements";
/// Non-secret rebuildable cursor.
const SYNC_FILE: &str = ".trove/steam-sync.json";
/// Secret store service id for the composite credential (KEY STEAMID64).
const SERVICE: &str = "steam";

const API_BASE: &str = "https://api.steampowered.com";
/// Politeness delay between API requests (~5 req/s).
const REQ_INTERVAL: Duration = Duration::from_millis(200);
/// Per-request HTTP timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between Periodic syncs — 8 h (3 times/day as the brief specifies).
pub const STEAM_SYNC_SECS: u64 = 8 * 3600;
/// Max appids whose achievements we fetch PER PULL (recently-played first).
/// A large library (1000+ games) avoids hammering every appid in one run.
const MAX_ACH_APPIDS_PER_PULL: usize = 50;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(LIBRARY_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(ACH_DIR)))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let lib = out.counts.get("library").copied().unwrap_or(0);
            let ach = out.counts.get("achievements").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(lib > 0 || ach > 0, || {
                format!("steam synced — {lib} library games, {ach} new achievements")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "steam sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let lib = out.counts.get("library").copied().unwrap_or(0);
    let ach = out.counts.get("achievements").copied().unwrap_or(0);
    let headline = if lib == 0 && ach == 0 {
        "Steam is up to date — no new library or achievements".to_string()
    } else {
        format!("Steam synced — {lib} library games, {ach} new achievements")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "steam",
        name: "Steam",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Steam library — owned games, cumulative playtime, \
                      and achievements — via Valve's official Web API. \
                      Requires a free API key and your SteamID64 or vanity URL.",
        domain: "gaming",
        vault_path: "gaming/steam/",
        toggleable: true,
        setup: &[
            "Get a free API key at steamcommunity.com/dev (any domain string accepted).",
            "Paste <API key> <SteamID64 or vanity URL> in the connect field.",
            "Your Steam profile must be public, or your own API key reads your private data.",
        ],
        caveats: "Playtime is cumulative — Steam has no per-session event log, so \
                  anything before the first sync is a single lifetime total; deltas \
                  between polls approximate sessions. Achievement fetches are \
                  rate-limited to 50 games per pull for large libraries.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(STEAM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("steam"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — composite: API key + SteamID64).

/// Parse `"KEY STEAMID64_OR_VANITY"` (space-separated).
fn parse_credential(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("paste your Steam API key and SteamID64 (space-separated), \
               e.g.: ABCDEF1234 76561197960287930");
    }
    let (key, id_or_vanity) = pasted
        .split_once(|c: char| c.is_ascii_whitespace())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "format: <API key> <SteamID64 or vanity URL handle>, \
                 e.g.: ABCDEF1234 76561197960287930"
            )
        })?;
    let key = key.trim().to_string();
    let id_or_vanity = id_or_vanity.trim().to_string();
    if key.is_empty() {
        bail!("API key is empty");
    }
    if id_or_vanity.is_empty() {
        bail!("SteamID64 or vanity URL is empty");
    }
    Ok((key, id_or_vanity))
}

/// Encode a `(key, steamid64)` pair for the secret store.
fn encode_credential(key: &str, steamid64: &str) -> String {
    format!("{key} {steamid64}")
}

/// Decode a stored credential back to `(key, steamid64)`.
fn decode_credential(stored: &str) -> Result<(String, String)> {
    let stored = stored.trim();
    let (key, id) = stored
        .split_once(|c: char| c.is_ascii_whitespace())
        .ok_or_else(|| anyhow::anyhow!("stored Steam credential is malformed — reconnect"))?;
    Ok((key.trim().to_string(), id.trim().to_string()))
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let client = SteamClient::new(API_BASE.to_string());
    connect_with(vault, &client, pasted)
}

fn connect_with(vault: &Vault, client: &impl SteamApi, pasted: &str) -> Result<()> {
    let (key, id_or_vanity) = parse_credential(pasted)?;
    // Resolve vanity URL to SteamID64 if not already numeric.
    let steamid64 = if id_or_vanity.chars().all(|c| c.is_ascii_digit()) {
        id_or_vanity.clone()
    } else {
        match client.resolve_vanity_url(&key, &id_or_vanity)
            .map_err(|e| anyhow::anyhow!("vanity URL resolution failed: {e}"))?
        {
            Some(id) => id,
            None => bail!(
                "Steam could not resolve vanity URL {:?} — check the handle and try again",
                id_or_vanity
            ),
        }
    };
    // Verify the key works and the profile is readable with a cheap probe.
    // NOTE: Steam returns HTTP 200 + `{"response":{}}` (empty, no `games` key)
    // for a private profile when the key does not belong to the profile owner —
    // NOT 401. So we must also check for an empty/absent games array and warn
    // the user, because every subsequent sync would silently return 0 results.
    match client.get_recently_played(&key, &steamid64) {
        Ok(body) => {
            // Parse and check whether the response actually contains data.
            // A private profile returns {} or {"response":{}} with no `games`.
            let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let has_games = v["response"]["games"]
                .as_array()
                .map(|a| !a.is_empty())
                .unwrap_or(false);
            if !has_games {
                // Could be a private profile or simply no recent activity.
                // Warn but still store — the library pull (GetOwnedGames) will
                // surface the real error; not all valid accounts have recent games.
                eprintln!(
                    "steam: connect probe returned no recent games — if syncs return \
                     empty results, ensure the profile is set to Public in Steam \
                     Privacy Settings, or use your own API key to access a private profile"
                );
            }
        }
        Err(FetchError::Unauthorized) => bail!(
            "Steam API key is invalid or the profile is not accessible — \
             check the key at steamcommunity.com/dev and ensure the profile is public \
             (or that you are using your own key to access a private profile)"
        ),
        Err(e) => {
            // Transient/other: store the credential anyway; the pull will retry.
            eprintln!("steam: connect probe failed ({}): storing credential anyway", e);
        }
    }
    vault.save_sync_token(
        SERVICE,
        &crate::sync::oauth::TokenSet {
            access_token: encode_credential(&key, &steamid64),
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
        let stored = token.access_token;
        let label = if let Ok((_, steamid)) = decode_credential(&stored) {
            format!("SteamID64: {steamid}")
        } else {
            "Steam (reconnect to update)".to_string()
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
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "steam",
    display_name: "Steam",
    methods: &[ConnectMethod::TokenPaste {
        label: "API key and SteamID64",
        help: "Paste your Steam Web API key and your SteamID64 (or vanity URL), \
               separated by a space. Get a free API key at steamcommunity.com/dev \
               (any domain string is accepted). Your SteamID64 is the long number \
               in your profile URL (steamcommunity.com/profiles/<ID>). \
               Your profile must be public, or use your own API key for a private profile.",
        placeholder: "ABCDEF1234ABCDEF1234ABCDEF1234AB 76561197960287930",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["steam"],
    setup: &[
        "Get a free API key at steamcommunity.com/dev (any domain string works).",
        "Find your SteamID64 at steamidfinder.com or in your Steam profile URL.",
        "Paste: <API key> <SteamID64> in the connect field.",
        "Your profile must be public unless you're using your own API key.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful library snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    library_synced: Option<String>,
    /// RFC3339 local time of the last successful sync (any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_steam_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_steam_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    /// HTTP 401 / 403 — bad key or private profile.
    Unauthorized,
    /// The game has no stats schema (GetPlayerAchievements returns success:false
    /// or HTTP 400). Skip silently.
    NoStats,
    /// Transient / other.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (invalid key or private profile)"),
            FetchError::NoStats => write!(f, "game has no achievement stats"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FetchError {}

/// Injectable API surface — production wraps ureq, tests use stubs.
trait SteamApi {
    /// `IPlayerService/GetOwnedGames/v1` with `include_appinfo=1`.
    fn get_owned_games(&self, key: &str, steamid: &str) -> Result<String, FetchError>;
    /// `IPlayerService/GetRecentlyPlayedGames/v1`.
    fn get_recently_played(&self, key: &str, steamid: &str) -> Result<String, FetchError>;
    /// `ISteamUserStats/GetPlayerAchievements/v1?appid=X`.
    fn get_player_achievements(
        &self,
        key: &str,
        steamid: &str,
        appid: u32,
    ) -> Result<String, FetchError>;
    /// `ISteamUser/ResolveVanityURL/v1?vanityurl=X` → SteamID64 or None.
    fn resolve_vanity_url(&self, key: &str, vanity: &str) -> Result<Option<String>, FetchError>;
}

struct SteamClient {
    base: String,
}

impl SteamClient {
    fn new(base: String) -> Self {
        SteamClient { base }
    }

    fn get_json(&self, url: &str) -> Result<String, FetchError> {
        match ureq::get(url).timeout(HTTP_TIMEOUT).call() {
            Ok(resp) => resp
                .into_string()
                .map_err(|e| FetchError::Other(format!("reading body: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(400, _)) => Err(FetchError::NoStats),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let snippet = &body[..body.len().min(300)];
                Err(FetchError::Other(format!("HTTP {code}: {snippet}")))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl SteamApi for SteamClient {
    fn get_owned_games(&self, key: &str, steamid: &str) -> Result<String, FetchError> {
        let url = format!(
            "{}/IPlayerService/GetOwnedGames/v1/?key={key}&steamid={steamid}\
             &include_appinfo=1&include_played_free_games=1&format=json",
            self.base
        );
        self.get_json(&url)
    }

    fn get_recently_played(&self, key: &str, steamid: &str) -> Result<String, FetchError> {
        let url = format!(
            "{}/IPlayerService/GetRecentlyPlayedGames/v1/?key={key}&steamid={steamid}\
             &count=0&format=json",
            self.base
        );
        self.get_json(&url)
    }

    fn get_player_achievements(
        &self,
        key: &str,
        steamid: &str,
        appid: u32,
    ) -> Result<String, FetchError> {
        // `&l=english` is required to receive the `name` and `description`
        // fields. Without it Steam omits them entirely (returns only
        // apiname/achieved/unlocktime), even though the response is still
        // HTTP 200. See Steam Web API docs / TF2 wiki GetPlayerAchievements.
        let url = format!(
            "{}/ISteamUserStats/GetPlayerAchievements/v1/?key={key}&steamid={steamid}\
             &appid={appid}&l=english&format=json",
            self.base
        );
        match self.get_json(&url) {
            Ok(body) => {
                // Steam returns HTTP 200 + `{"playerstats":{"success":false}}`
                // when the game has no achievements / stats schema.
                if body.contains("\"success\":false") || body.contains("\"success\": false") {
                    return Err(FetchError::NoStats);
                }
                Ok(body)
            }
            Err(e) => Err(e),
        }
    }

    fn resolve_vanity_url(&self, key: &str, vanity: &str) -> Result<Option<String>, FetchError> {
        let url = format!(
            "{}/ISteamUser/ResolveVanityURL/v1/?key={key}&vanityurl={vanity}&format=json",
            self.base
        );
        let body = self.get_json(&url)?;
        // `{"response":{"steamid":"...","success":1}}` or
        // `{"response":{"success":42,"message":"No match"}}`
        let v: Value =
            serde_json::from_str(&body).map_err(|e| FetchError::Other(e.to_string()))?;
        let resp = &v["response"];
        let success = resp["success"].as_u64().unwrap_or(0);
        if success == 1 {
            Ok(resp["steamid"].as_str().map(str::to_string))
        } else {
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// A raw library game row (one line per game in the monthly snapshot file).
#[derive(Serialize, Deserialize, Debug, Clone)]
struct LibraryGame {
    /// The game's unique Steam appid.
    appid: u32,
    /// Game name (only present when `include_appinfo=1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Total playtime in minutes (cumulative since account creation).
    playtime_forever: u32,
    /// Playtime in the last two weeks, in minutes. Only present when > 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    playtime_2weeks: Option<u32>,
    /// Icon image URL suffix (combine with Steam CDN base to form a URL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    img_icon_url: Option<String>,
    /// Logo image URL suffix (legacy field, may be absent on newer games).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    img_logo_url: Option<String>,
    /// Whether the game exposes stats/achievements via the Web API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    has_community_visible_stats: Option<bool>,
    /// Any fields the API adds in future that we don't explicitly map.
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Parse a `GetOwnedGames` or `GetRecentlyPlayedGames` JSON response body.
/// Returns the list of games in `response.games`, each with `month_key`
/// injected. Unknown fields overflow into `extra`. Returns empty on error.
fn parse_library(body: &str, month_key: &str) -> Vec<LibraryGame> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let games = match v["response"]["games"].as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    // month_key is used by the caller for partitioning (choosing which
    // snapshot file to read/write); parse_library itself does not need it.
    let _ = month_key;
    games
        .iter()
        .filter_map(|g| serde_json::from_value::<LibraryGame>(g.clone()).ok())
        .collect()
}

/// One row in the achievements stream.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct AchievementRow {
    /// `appid:apiname` — the stable, globally-unique guid for dedupe/upsert.
    guid: String,
    /// Steam appid this achievement belongs to.
    appid: u32,
    /// Internal API name (e.g. `"WIN_10_GAMES"`).
    apiname: String,
    /// 1 = unlocked, 0 = locked (we only store unlocked ones).
    achieved: u8,
    /// Unix timestamp when unlocked (0 when not unlocked — never stored).
    unlocktime: u64,
    /// RFC 3339 ISO string of the unlock time (Local), for human readability
    /// and as the stream partition key.
    ts: String,
    /// Localized achievement title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Localized achievement description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Any future fields from the API overflow here.
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// Parse a `GetPlayerAchievements` JSON response for `appid`. Returns only
/// UNLOCKED achievements (`achieved == 1` and `unlocktime > 0`). Skips
/// any entry missing `apiname`. Extra fields flow into `extra`.
fn parse_achievements(body: &str, appid: u32) -> Vec<AchievementRow> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let achievements = match v["playerstats"]["achievements"].as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    achievements
        .iter()
        .filter_map(|a| {
            let apiname = a["apiname"].as_str().filter(|s| !s.is_empty())?.to_string();
            let achieved = a["achieved"].as_u64().unwrap_or(0) as u8;
            if achieved != 1 {
                return None; // locked — skip
            }
            let unlocktime = a["unlocktime"].as_u64().unwrap_or(0);
            if unlocktime == 0 {
                return None; // achieved but no timestamp — skip
            }
            let ts = Utc
                .timestamp_opt(unlocktime as i64, 0)
                .single()
                .map(|dt| DateTime::<Local>::from(dt).to_rfc3339())
                .unwrap_or_else(|| format!("{unlocktime}"));

            let name = a["name"].as_str().map(str::to_string);
            let description = a["description"].as_str().map(str::to_string);

            // Extra fields: everything not explicitly mapped.
            let mut extra = Map::new();
            if let Value::Object(ref m) = a {
                for (k, val) in m {
                    if !matches!(
                        k.as_str(),
                        "apiname" | "achieved" | "unlocktime" | "name" | "description"
                    ) {
                        extra.insert(k.clone(), val.clone());
                    }
                }
            }

            Some(AchievementRow {
                guid: format!("{appid}:{apiname}"),
                appid,
                apiname,
                achieved,
                unlocktime,
                ts,
                name,
                description,
                extra,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Steam is not connected — add your API key and SteamID64 \
                 in the Integrations tab"
            )
        })?;
    let (key, steamid) = decode_credential(&token.access_token)?;
    let client = SteamClient::new(API_BASE.to_string());
    pull_with(vault, &client, &key, &steamid)
}

fn pull_with(
    vault: &Vault,
    client: &impl SteamApi,
    key: &str,
    steamid: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_steam_sync();

    // --- Library snapshot. ---
    let lib_count = sync_library(vault, client, key, steamid)?;
    state.library_synced = Some(Local::now().to_rfc3339());
    thread::sleep(REQ_INTERVAL);

    // --- Achievements (lazily, recently-played first). ---
    let ach_count = sync_achievements(vault, client, key, steamid)?;

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_steam_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{lib_count} library games, {ach_count} new achievements"),
        counts: BTreeMap::from([("library", lib_count), ("achievements", ach_count)]),
    })
}

/// Fetch the owned-games library and upsert into the monthly snapshot.
/// Returns the number of games written (always the full library count on a
/// successful fetch — it is a current-state snapshot, not a delta).
fn sync_library(
    vault: &Vault,
    client: &impl SteamApi,
    key: &str,
    steamid: &str,
) -> Result<u64> {
    let body = client
        .get_owned_games(key, steamid)
        .map_err(|e| anyhow::anyhow!("fetching owned games: {e}"))?;
    let month_key = Local::now().format("%Y-%m").to_string();
    let games = parse_library(&body, &month_key);

    if games.is_empty() {
        return Ok(0);
    }

    // Upsert: read existing rows for THIS month, overwrite with latest.
    // Build a map of appid → existing row (if any) from this month's file.
    let stream = vault.stream(LIBRARY_DIR, Partition::Month);
    let existing: HashMap<u32, LibraryGame> = stream
        .read::<LibraryGame>(&month_key)
        .unwrap_or_default()
        .into_iter()
        .map(|g| (g.appid, g))
        .collect();

    // Merge: for each fetched game, overwrite any existing row.
    let mut merged: HashMap<u32, LibraryGame> = existing;
    for g in &games {
        merged.insert(g.appid, g.clone());
    }

    // Rewrite the month snapshot atomically.
    let mut rows: Vec<LibraryGame> = merged.into_values().collect();
    rows.sort_by_key(|g| g.appid);
    vault.write_snapshot(&format!("{LIBRARY_DIR}/{month_key}.jsonl"), &rows)?;

    Ok(games.len() as u64)
}

/// Fetch achievements for up to `MAX_ACH_APPIDS_PER_PULL` appids.
/// Recently-played games are fetched first (so new unlocks surface quickly).
/// Returns count of NEW achievement rows written.
fn sync_achievements(
    vault: &Vault,
    client: &impl SteamApi,
    key: &str,
    steamid: &str,
) -> Result<u64> {
    // Build the candidate appid list: recently-played first, then all-owned.
    let mut appids: Vec<u32> = Vec::new();
    {
        if let Ok(recent_body) = client.get_recently_played(key, steamid) {
            let recent_games = parse_library(&recent_body, "");
            for g in recent_games {
                if g.has_community_visible_stats.unwrap_or(false) {
                    appids.push(g.appid);
                }
            }
        }
    }
    thread::sleep(REQ_INTERVAL);
    {
        if let Ok(owned_body) = client.get_owned_games(key, steamid) {
            let owned_games = parse_library(&owned_body, "");
            for g in owned_games {
                if g.has_community_visible_stats.unwrap_or(false)
                    && !appids.contains(&g.appid)
                {
                    appids.push(g.appid);
                }
            }
        }
    }
    thread::sleep(REQ_INTERVAL);

    // Load all existing achievement guids across all partitions.
    let ach_stream = vault.stream(ACH_DIR, Partition::Month);
    let mut seen_guids: HashSet<String> = HashSet::new();
    for part_key in ach_stream.partitions()? {
        for row in ach_stream.read::<AchievementRow>(&part_key)? {
            seen_guids.insert(row.guid);
        }
    }

    let mut total_new: u64 = 0;
    let candidate_appids: Vec<u32> =
        appids.into_iter().take(MAX_ACH_APPIDS_PER_PULL).collect();

    for (i, &appid) in candidate_appids.iter().enumerate() {
        if i > 0 {
            thread::sleep(REQ_INTERVAL);
        }
        let body = match client.get_player_achievements(key, steamid, appid) {
            Ok(b) => b,
            // No achievements / stats schema: skip silently.
            Err(FetchError::NoStats) => continue,
            Err(FetchError::Unauthorized) => {
                bail!("Steam API unauthorized — reconnect in the Integrations tab")
            }
            Err(FetchError::Other(e)) => {
                eprintln!("steam: achievements for appid {appid} failed: {e}");
                continue;
            }
        };
        let rows = parse_achievements(&body, appid);
        let new_rows: Vec<AchievementRow> =
            rows.into_iter().filter(|r| !seen_guids.contains(&r.guid)).collect();
        if !new_rows.is_empty() {
            ach_stream.append(&new_rows, |r| r.ts.as_str())?;
            for r in &new_rows {
                seen_guids.insert(r.guid.clone());
            }
            total_new += new_rows.len() as u64;
        }
    }

    Ok(total_new)
}

// ---------------------------------------------------------------------------
// Tests — pure, fixture-based, no network, unique temp dirs.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-steam-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures — shapes confirmed against TeamFortress wiki + documented API shapes.

    /// GetOwnedGames response: 3 games.
    /// api.steampowered.com/IPlayerService/GetOwnedGames/v1?include_appinfo=1
    /// Fields: appid, name, playtime_forever (minutes), playtime_2weeks (minutes,
    /// optional), img_icon_url, img_logo_url, has_community_visible_stats.
    fn owned_games_body() -> String {
        r#"{"response":{"game_count":3,"games":[
          {"appid":440,"name":"Team Fortress 2","playtime_forever":1234,"playtime_2weeks":60,
           "img_icon_url":"e3f595a92552da3d664ad00277fad2107345f743",
           "img_logo_url":"07385eb55b5ba974aebbe74d3c6f7da08f84a22",
           "has_community_visible_stats":true},
          {"appid":730,"name":"Counter-Strike 2","playtime_forever":500,
           "img_icon_url":"69f7ebe2735c366c65c0b33dae00e12dc40edbe8",
           "has_community_visible_stats":true},
          {"appid":271590,"name":"Grand Theft Auto V","playtime_forever":4200,
           "img_icon_url":"4ef58bfee4f84de50ba2d4f51d60a8daa1e9b10",
           "has_community_visible_stats":false}
        ]}}"#.to_string()
    }

    /// Library body with playtime_2weeks absent (game not recently played).
    fn owned_games_no_2weeks() -> String {
        r#"{"response":{"game_count":1,"games":[
          {"appid":570,"name":"Dota 2","playtime_forever":999,
           "img_icon_url":"abcdef12","has_community_visible_stats":true}
        ]}}"#.to_string()
    }

    /// Recently-played response (subset of owned, same shape).
    fn recently_played_body() -> String {
        r#"{"response":{"total_count":1,"games":[
          {"appid":440,"name":"Team Fortress 2","playtime_forever":1234,"playtime_2weeks":60,
           "img_icon_url":"e3f595a92552da3d664ad00277fad2107345f743",
           "has_community_visible_stats":true}
        ]}}"#.to_string()
    }

    /// GetPlayerAchievements response WITHOUT `&l=` (no language param).
    /// The real Steam API omits `name` and `description` entirely in this case —
    /// only apiname/achieved/unlocktime are present. This is the shape the
    /// parser must tolerate gracefully (fields parse as None).
    fn achievements_tf2_no_lang() -> String {
        r#"{"playerstats":{"steamID":"76561197960287930","gameName":"Team Fortress 2","achievements":[
          {"apiname":"TF_PLAY_GAME_EVERYCLASS","achieved":1,"unlocktime":1609459200},
          {"apiname":"TF_MEDIC_MILESTONE_1","achieved":1,"unlocktime":1612137600},
          {"apiname":"TF_PLAY_GAME_FRIENDONLY","achieved":0,"unlocktime":0}
        ],"success":true}}"#.to_string()
    }

    /// GetPlayerAchievements response WITH `&l=english` (as the production URL
    /// now sends). Steam returns `name` and `description` only when `l=` is
    /// supplied. 2 unlocked, 1 locked.
    fn achievements_tf2() -> String {
        r#"{"playerstats":{"steamID":"76561197960287930","gameName":"Team Fortress 2","achievements":[
          {"apiname":"TF_PLAY_GAME_EVERYCLASS","achieved":1,"unlocktime":1609459200,
           "name":"Head of the Class","description":"Play a complete round with every class."},
          {"apiname":"TF_MEDIC_MILESTONE_1","achieved":1,"unlocktime":1612137600,
           "name":"Bedside Manner","description":"Kill 10 enemies with the overdose."},
          {"apiname":"TF_PLAY_GAME_FRIENDONLY","achieved":0,"unlocktime":0,
           "name":"You Only Shiv Twice","description":"Kill 2 enemies in a row with your knife."}
        ],"success":true}}"#.to_string()
    }

    /// GetPlayerAchievements response for a game with no stats (success:false).
    fn achievements_no_stats() -> String {
        r#"{"playerstats":{"steamID":"76561197960287930","gameName":"No-Ach Game","success":false}}"#
            .to_string()
    }

    /// ResolveVanityURL — success.
    fn vanity_ok() -> String {
        r#"{"response":{"steamid":"76561197960287930","success":1}}"#.to_string()
    }

    /// ResolveVanityURL — no match (success=42 per Steam API spec).
    fn vanity_no_match() -> String {
        r#"{"response":{"success":42,"message":"No match"}}"#.to_string()
    }

    // ---------------------------------------------------------------------------
    // Parsing tests.

    #[test]
    fn parses_library_full_fidelity() {
        let games = parse_library(&owned_games_body(), "2024-01");
        assert_eq!(games.len(), 3);
        let tf2 = &games[0];
        assert_eq!(tf2.appid, 440);
        assert_eq!(tf2.name.as_deref(), Some("Team Fortress 2"));
        assert_eq!(tf2.playtime_forever, 1234);
        assert_eq!(tf2.playtime_2weeks, Some(60));
        assert_eq!(
            tf2.img_icon_url.as_deref(),
            Some("e3f595a92552da3d664ad00277fad2107345f743")
        );
        assert_eq!(
            tf2.img_logo_url.as_deref(),
            Some("07385eb55b5ba974aebbe74d3c6f7da08f84a22")
        );
        assert_eq!(tf2.has_community_visible_stats, Some(true));

        let gta = &games[2];
        assert_eq!(gta.appid, 271590);
        assert_eq!(gta.has_community_visible_stats, Some(false));
    }

    #[test]
    fn parses_library_absent_optional_fields() {
        let games = parse_library(&owned_games_no_2weeks(), "2024-02");
        assert_eq!(games.len(), 1);
        assert_eq!(games[0].playtime_2weeks, None);
        assert_eq!(games[0].img_logo_url, None);
    }

    #[test]
    fn parses_library_empty_response() {
        assert!(parse_library(r#"{"response":{}}"#, "2024-01").is_empty());
        assert!(parse_library("not json", "2024-01").is_empty());
        assert!(parse_library(r#"{"response":{"games":[]}}"#, "2024-01").is_empty());
    }

    #[test]
    fn parses_achievements_unlocked_only() {
        let rows = parse_achievements(&achievements_tf2(), 440);
        // Only 2 unlocked (achieved==1 && unlocktime>0); the locked one is dropped.
        assert_eq!(rows.len(), 2);
        let r0 = &rows[0];
        assert_eq!(r0.guid, "440:TF_PLAY_GAME_EVERYCLASS");
        assert_eq!(r0.appid, 440);
        assert_eq!(r0.apiname, "TF_PLAY_GAME_EVERYCLASS");
        assert_eq!(r0.achieved, 1);
        assert_eq!(r0.unlocktime, 1609459200);
        assert_eq!(r0.name.as_deref(), Some("Head of the Class"));
        // ts should be a valid RFC3339 string for the 2021-01-01 UTC unlock
        // (may render as 2020-12-31 in UTC-offset timezones).
        assert!(
            r0.ts.contains("2020") || r0.ts.contains("2021"),
            "expected 2020 or 2021 in ts for unlocktime=1609459200, got: {}",
            r0.ts
        );

        let r1 = &rows[1];
        assert_eq!(r1.guid, "440:TF_MEDIC_MILESTONE_1");
        assert_eq!(r1.unlocktime, 1612137600);
    }

    #[test]
    fn parses_achievements_no_stats_body_returns_empty() {
        // success:false body — parse_achievements returns empty.
        let rows = parse_achievements(&achievements_no_stats(), 570);
        assert!(rows.is_empty(), "expected empty, got {} rows", rows.len());
    }

    #[test]
    fn parses_achievements_locked_row_excluded() {
        let rows = parse_achievements(&achievements_tf2(), 440);
        let locked: Vec<_> = rows.iter().filter(|r| r.achieved == 0).collect();
        assert!(locked.is_empty(), "locked achievements must not be stored");
    }

    #[test]
    fn parses_achievements_empty_body() {
        assert!(parse_achievements("", 440).is_empty());
        assert!(parse_achievements("{}", 440).is_empty());
    }

    /// Confirm that a response WITHOUT `&l=` (no `name`/`description` fields)
    /// correctly produces `name=None` and `description=None`. This guards
    /// against a regression where the URL dropped `&l=english` and the fields
    /// silently vanished in production while the fixture-based tests still
    /// passed (the "self-consistent-but-wrong fixture" failure mode).
    #[test]
    fn parses_achievements_no_lang_name_is_none() {
        let rows = parse_achievements(&achievements_tf2_no_lang(), 440);
        assert_eq!(rows.len(), 2, "still parses 2 unlocked achievements without lang param");
        for row in &rows {
            assert!(
                row.name.is_none(),
                "name must be None when l= omitted; got {:?} for {}",
                row.name,
                row.apiname
            );
            assert!(
                row.description.is_none(),
                "description must be None when l= omitted; got {:?} for {}",
                row.description,
                row.apiname
            );
        }
    }

    #[test]
    fn parse_vanity_ok_fields() {
        let v: Value = serde_json::from_str(&vanity_ok()).unwrap();
        assert_eq!(v["response"]["success"].as_u64(), Some(1));
        assert_eq!(v["response"]["steamid"].as_str(), Some("76561197960287930"));
    }

    #[test]
    fn parse_vanity_no_match_fields() {
        let v: Value = serde_json::from_str(&vanity_no_match()).unwrap();
        assert_eq!(v["response"]["success"].as_u64(), Some(42));
        assert!(v["response"].get("steamid").map_or(true, |s| s.is_null()));
    }

    // ---------------------------------------------------------------------------
    // Credential helpers.

    #[test]
    fn credential_round_trip() {
        let enc = encode_credential("MYKEY1234", "76561197960287930");
        let (k, id) = decode_credential(&enc).unwrap();
        assert_eq!(k, "MYKEY1234");
        assert_eq!(id, "76561197960287930");
    }

    #[test]
    fn parse_credential_space_separated() {
        let (k, id) = parse_credential("MYKEY 76561197960287930").unwrap();
        assert_eq!(k, "MYKEY");
        assert_eq!(id, "76561197960287930");
    }

    #[test]
    fn parse_credential_vanity_handle() {
        let (k, id) = parse_credential("MYKEY gaben").unwrap();
        assert_eq!(k, "MYKEY");
        assert_eq!(id, "gaben");
    }

    #[test]
    fn parse_credential_empty_rejected() {
        assert!(parse_credential("").is_err());
        assert!(parse_credential("KEYONLY").is_err());
        assert!(parse_credential("  ").is_err());
    }

    // ---------------------------------------------------------------------------
    // Stub SteamApi for integration-like tests (no network).

    struct StubSteamApi {
        owned: String,
        recent: String,
        achievements: HashMap<u32, String>,
        vanity_body: Option<String>,
    }

    impl StubSteamApi {
        fn new() -> Self {
            let mut achievements = HashMap::new();
            achievements.insert(440u32, achievements_tf2());
            StubSteamApi {
                owned: owned_games_body(),
                recent: recently_played_body(),
                achievements,
                vanity_body: Some(vanity_ok()),
            }
        }
    }

    impl SteamApi for StubSteamApi {
        fn get_owned_games(&self, _key: &str, _id: &str) -> Result<String, FetchError> {
            Ok(self.owned.clone())
        }
        fn get_recently_played(&self, _key: &str, _id: &str) -> Result<String, FetchError> {
            Ok(self.recent.clone())
        }
        fn get_player_achievements(
            &self,
            _key: &str,
            _id: &str,
            appid: u32,
        ) -> Result<String, FetchError> {
            match self.achievements.get(&appid) {
                Some(body) => {
                    if body.contains("\"success\":false") {
                        Err(FetchError::NoStats)
                    } else {
                        Ok(body.clone())
                    }
                }
                None => Err(FetchError::NoStats),
            }
        }
        fn resolve_vanity_url(
            &self,
            _key: &str,
            _vanity: &str,
        ) -> Result<Option<String>, FetchError> {
            match &self.vanity_body {
                Some(body) => {
                    let v: Value = serde_json::from_str(body).unwrap();
                    let s = v["response"]["success"].as_u64().unwrap_or(0);
                    if s == 1 {
                        Ok(v["response"]["steamid"].as_str().map(str::to_string))
                    } else {
                        Ok(None)
                    }
                }
                None => Ok(None),
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Integration-level store tests.

    fn store_credential(vault: &Vault) {
        vault
            .save_sync_token(
                SERVICE,
                &crate::sync::oauth::TokenSet {
                    access_token: encode_credential("MYKEY", "76561197960287930"),
                    refresh_token: None,
                    token_type: None,
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn connect_stores_credential() {
        let vault = temp_vault("connect");
        let stub = StubSteamApi::new();
        // 76561197960287930 is all-digit so no vanity resolution needed.
        connect_with(&vault, &stub, "MYKEY 76561197960287930").unwrap();
        let token = vault.load_sync_token(SERVICE).unwrap().unwrap();
        let (k, id) = decode_credential(&token.access_token).unwrap();
        assert_eq!(k, "MYKEY");
        assert_eq!(id, "76561197960287930");
    }

    #[test]
    fn connect_resolves_vanity() {
        let vault = temp_vault("vanity");
        let stub = StubSteamApi::new();
        // "gaben" is NOT all-digit → triggers vanity resolution → stub returns 76561197960287930.
        connect_with(&vault, &stub, "MYKEY gaben").unwrap();
        let token = vault.load_sync_token(SERVICE).unwrap().unwrap();
        let (_, id) = decode_credential(&token.access_token).unwrap();
        assert_eq!(id, "76561197960287930");
    }

    #[test]
    fn connect_vanity_not_found_errors() {
        let vault = temp_vault("vanity_miss");
        let mut stub = StubSteamApi::new();
        stub.vanity_body = Some(vanity_no_match());
        let err = connect_with(&vault, &stub, "MYKEY unknownuser").unwrap_err();
        assert!(
            err.to_string().contains("resolve vanity URL"),
            "expected 'resolve vanity URL' in error, got: {err}"
        );
    }

    #[test]
    fn pull_writes_library_and_achievements() {
        let vault = temp_vault("pull_basic");
        store_credential(&vault);
        let stub = StubSteamApi::new();

        let out = pull_with(&vault, &stub, "MYKEY", "76561197960287930").unwrap();
        // Library: 3 games (all from owned_games_body).
        assert_eq!(out.counts.get("library").copied().unwrap_or(0), 3);
        // Achievements: 2 unlocked from TF2 (appid 440); cs2 and gta have NoStats.
        assert_eq!(out.counts.get("achievements").copied().unwrap_or(0), 2);

        // Verify library file on disk.
        let month = Local::now().format("%Y-%m").to_string();
        let lib_path = vault.root().join(LIBRARY_DIR).join(format!("{month}.jsonl"));
        assert!(lib_path.exists(), "library snapshot should exist");
        let lib_content = std::fs::read_to_string(&lib_path).unwrap();
        assert!(lib_content.contains("Team Fortress 2"));
        assert!(lib_content.contains("Counter-Strike 2"));

        // Verify achievement files on disk.
        let ach_dir = vault.root().join(ACH_DIR);
        let ach_files: Vec<_> = std::fs::read_dir(&ach_dir).unwrap().flatten().collect();
        assert!(!ach_files.is_empty(), "achievement files should be written");
    }

    #[test]
    fn pull_dedupes_achievements_on_re_pull() {
        let vault = temp_vault("pull_dedup");
        store_credential(&vault);
        let stub = StubSteamApi::new();

        // First pull.
        let out1 = pull_with(&vault, &stub, "MYKEY", "76561197960287930").unwrap();
        assert_eq!(out1.counts.get("achievements").copied().unwrap_or(0), 2);

        // Second pull — achievements already stored, count should be 0 new.
        let out2 = pull_with(&vault, &stub, "MYKEY", "76561197960287930").unwrap();
        assert_eq!(
            out2.counts.get("achievements").copied().unwrap_or(0),
            0,
            "second pull must not re-write existing achievements"
        );
    }

    #[test]
    fn pull_without_connection_errors() {
        let vault = temp_vault("no_cred");
        let err = pull(&vault).unwrap_err();
        assert!(
            err.to_string().contains("not connected"),
            "expected 'not connected' error, got: {err}"
        );
    }

    #[test]
    fn disconnect_removes_token() {
        let vault = temp_vault("disconnect");
        store_credential(&vault);
        def_disconnect(&vault, "steam").unwrap();
        assert!(vault.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn status_shows_steamid_label() {
        let vault = temp_vault("status");
        store_credential(&vault);
        let status = def_status(&vault).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert!(
            status.accounts[0].label.contains("76561197960287930"),
            "label should include steamid: {}",
            status.accounts[0].label
        );
    }

    #[test]
    fn library_upserts_within_same_month() {
        let vault = temp_vault("upsert");
        store_credential(&vault);
        let stub = StubSteamApi::new();

        // First pull.
        pull_with(&vault, &stub, "MYKEY", "76561197960287930").unwrap();
        // Second pull — same month → upsert, not double-append.
        pull_with(&vault, &stub, "MYKEY", "76561197960287930").unwrap();

        let month = Local::now().format("%Y-%m").to_string();
        let lib_path = vault.root().join(LIBRARY_DIR).join(format!("{month}.jsonl"));
        let content = std::fs::read_to_string(&lib_path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            3,
            "library should have exactly 3 games after 2 pulls, got {}",
            lines.len()
        );
    }

    #[test]
    fn sync_state_written_and_readable() {
        let vault = temp_vault("sync_state");
        store_credential(&vault);
        let stub = StubSteamApi::new();
        pull_with(&vault, &stub, "MYKEY", "76561197960287930").unwrap();
        let state = vault.read_steam_sync();
        assert!(state.library_synced.is_some(), "library_synced should be set after pull");
        assert!(state.updated.is_some(), "updated should be set after pull");
    }
}
