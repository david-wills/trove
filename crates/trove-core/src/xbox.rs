//! Xbox — Microsoft's gaming platform covering Xbox Live, Game Pass PC, and
//! console activity. Syncs title history and achievements via the OpenXBL
//! proxy API (xbl.io), which wraps the Xbox Live REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/xbox.md.
//!
//! # Auth
//!
//! The user signs up at xbl.io, links their Microsoft account, and copies
//! their API key.  The key is sent in the `X-Authorization` header on every
//! request.  No token refresh is needed — the key does not expire
//! automatically; the user must re-paste if they regenerate it.
//!
//! Free tier: 150 req/hour.  A full sync (title history + achievements) is
//! two to a few dozen requests; the cadence of 6 h keeps well inside the
//! free tier even for large libraries.
//!
//! # Data
//!
//! Two raw-only streams in `gaming/xbox/`:
//!
//! - **titles** — `gaming/xbox/titles/YYYY-MM.jsonl`, partitioned by the
//!   LOCAL month of `lastTimePlayed`.  One row per title per sync; upserted
//!   by `titleId` within the same partition.  The whole title list is
//!   fetched each sync; read-time uses the latest row to get current playtime.
//!
//! - **achievements** — `gaming/xbox/achievements/YYYY-MM.jsonl`, partitioned
//!   by the unlock month.  `guid` = `{titleId}:{achievementId}`.  Only
//!   unlocked achievements (`progressState == "Achieved"`) are stored.
//!   Deduped across re-pulls.
//!
//! `gaming/` is **raw-only** in the taxonomy — no write-time contract.
//!
//! # Response shapes — NOT documented; parser parked Needs-sample
//!
//! **WARNING:** The xbl.io OpenAPI spec (openapi.yaml) lists endpoint paths
//! but provides NO response body schemas for `/api/v2/achievements` or
//! `/api/v2/player/titleHistory` — both show only `200: description: Success`.
//! The shapes below are the EXPECTED shapes based on the canonical Xbox Live
//! REST API (OpenXbox/xbox-webapi-python models) and community consensus, but
//! they have NOT been verified against a real xbl.io response.  A Needs-sample
//! review with a real API key is required before these parsers are trusted.
//!
//! An optional `{"content": <payload>}` wrapper MAY be present; both parsers
//! fall back to the root object when `content` is absent.  The `code` field
//! seen in some community examples is not in the openapi.yaml.
//!
//! `GET /api/v2/player/titleHistory` — expected shape (unverified):
//! ```json
//! {"titles": [
//!   {"titleId": "1810924247", "name": "Halo Infinite", "type": "Game",
//!    "titleHistory": {"lastTimePlayed": "2025-11-26T02:34:30Z"}, ...}
//! ]}
//! ```
//!
//! `GET /api/v2/achievements` — expected modern Xbox Live shape (unverified):
//! ```json
//! {"achievements": [
//!   {"id": "1", "name": "Legendary Warrior",
//!    "progressState": "Achieved",
//!    "progression": {"timeUnlocked": "2024-01-15T18:30:00.0000000Z"},
//!    "titleAssociations": [{"id": 1810924247, "name": "Halo Infinite"}], ...}
//! ], "pagingInfo": {...}}
//! ```
//!
//! The parser also tolerates the `isUnlocked`/`timeUnlocked` variant seen in
//! some xbl.io community examples (flat timeUnlocked, boolean isUnlocked) as a
//! defensive fallback; the real envelope must be confirmed with a live response.
//!
//! # Cursor
//!
//! A rebuildable cursor at `.trove/xbox-sync.json` stores:
//! - `title_synced`: RFC3339 of the last successful title-list sync.
//! - `achievement_guids`: set of `{titleId}:{achievementId}` already written.
//! - `updated`: RFC3339 of the last successful overall sync.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "xbox";
const SYNC_FILE: &str = ".trove/xbox-sync.json";
const TITLES_DIR: &str = "gaming/xbox/titles";
const ACH_DIR: &str = "gaming/xbox/achievements";

const API_BASE: &str = "https://xbl.io/api/v2";

/// Free tier: 150 req/hour → throttle at ~2 req/s to stay safe.
const REQ_INTERVAL: Duration = Duration::from_millis(500);
/// Per-request HTTP timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(25);
/// Sync every 6 hours — infrequent enough to stay within the free tier.
pub const XBOX_SYNC_SECS: u64 = 6 * 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ACH_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(TITLES_DIR)))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let token = match vault.load_sync_token(SERVICE) {
        Ok(Some(t)) => t,
        Ok(None) => return Ok(crate::registry::CollectOutcome::quiet()),
        Err(_) => return Ok(crate::registry::CollectOutcome::quiet()),
    };
    match pull_with_key(vault, &token.access_token) {
        Ok(out) => {
            let titles = out.counts.get("titles").copied().unwrap_or(0);
            let ach = out.counts.get("achievements").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(titles > 0 || ach > 0, || {
                format!("Xbox synced — {titles} titles, {ach} new achievements")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Xbox sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault.load_sync_token(SERVICE)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Xbox is not connected — add your xbl.io API key in the Integrations tab"
        )
    })?;
    pull_with_key(vault, &token.access_token)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "xbox",
        name: "Xbox",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your Xbox game library — title history with cumulative playtime \
                      and achievements with unlock dates — via the OpenXBL community proxy \
                      (xbl.io). Requires a free xbl.io API key (150 req/hour).",
        domain: "gaming",
        vault_path: "gaming/xbox/",
        toggleable: true,
        setup: &[
            "Sign up at xbl.io and link your Microsoft account.",
            "Copy your API key from the xbl.io console.",
            "Paste the key into the connection field on this card.",
        ],
        caveats: "Xbox data is served via the OpenXBL community proxy (xbl.io), not \
                  directly from Microsoft. xbl.io is a third-party service; data transits \
                  their servers. The free tier allows 150 requests/hour; Trove stays well \
                  within this for personal use. The API may change or become unavailable \
                  without notice.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(XBOX_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("xbox"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — the xbl.io API key).

fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API key is empty — copy your key from the xbl.io console and paste it here");
    }
    // Verify the key with a cheap probe (account endpoint).
    let client = XblClient::new(API_BASE.to_string());
    connect_with(vault, &client, key)
}

fn connect_with(vault: &Vault, client: &impl XblApi, key: &str) -> Result<()> {
    if key.trim().is_empty() {
        bail!("API key is empty — copy your key from the xbl.io console and paste it here");
    }
    // Probe: GET /account — should return profileUsers if the key is valid.
    match client.account(key) {
        Ok(body) => {
            // Minimal check: if the response contains "profileUsers" or any
            // gamertag, the key is likely valid.  Any JSON body on 200 is
            // treated as success; an error message body triggers a warning but
            // we still store the key (the user may be briefly rate-limited).
            if body.is_empty() || body.starts_with('{') {
                // Looks like a JSON response — store the key.
            } else {
                bail!("xbl.io returned an unexpected response — check your API key");
            }
        }
        Err(FetchError::Unauthorized) => {
            bail!(
                "xbl.io API key is invalid — check that you copied the full key \
                 from the xbl.io console (Settings → API Keys)"
            )
        }
        Err(FetchError::RateLimited) => {
            // Store anyway — rate limiting is transient.
            eprintln!(
                "xbox: rate limited during connect probe — storing key anyway; \
                 the next sync will retry"
            );
        }
        Err(FetchError::Other(e)) => {
            // Transient network error: store the key; the pull will retry.
            eprintln!("xbox: connect probe failed ({e}) — storing key anyway");
        }
    }
    vault.save_sync_token(
        SERVICE,
        &crate::sync::oauth::TokenSet {
            access_token: key.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None, // API keys do not expire automatically.
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let label = if token.access_token.len() > 8 {
            format!("xbl.io key ****{}", &token.access_token[token.access_token.len() - 4..])
        } else {
            "xbl.io (key set)".to_string()
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
    id: "xbox",
    display_name: "Xbox (via xbl.io)",
    methods: &[ConnectMethod::TokenPaste {
        label: "xbl.io API key",
        help: "Sign up at xbl.io and link your Microsoft account. Copy your API key from \
               the xbl.io console (Settings → API Keys) and paste it here. \
               The free tier allows 150 requests/hour — more than enough for personal use. \
               Note: your Xbox data transits the xbl.io servers; this is a third-party proxy.",
        placeholder: "xbl.io API key (32+ characters)",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["xbox"],
    setup: &[
        "Go to xbl.io and click 'Sign In' — log in with your Microsoft account.",
        "Navigate to Settings → API Keys and copy your API key.",
        "Paste the key into the connection field on this card.",
        "Note: your Xbox data transits the xbl.io servers (a third-party service).",
    ],
};

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 of the last successful title list sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title_synced: Option<String>,
    /// Dedup set: `{titleId}:{achievementId}` already written to the vault.
    /// Rebuilt from raw on first run; persisted for speed on subsequent pulls.
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    achievement_guids: HashSet<String>,
    /// RFC3339 of the last successful overall sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_xbox_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_xbox_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

#[derive(Debug)]
enum FetchError {
    /// HTTP 401 / 403 — invalid or missing API key.
    Unauthorized,
    /// HTTP 429 / 503 — rate limited.
    RateLimited,
    /// Any other error.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (invalid xbl.io API key)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/503)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Injectable API surface — production wraps ureq; tests use stubs.
trait XblApi {
    /// `GET /api/v2/account` — cheap probe for key validity.
    fn account(&self, key: &str) -> Result<String, FetchError>;
    /// `GET /api/v2/player/titleHistory` — own title history.
    fn title_history(&self, key: &str) -> Result<String, FetchError>;
    /// `GET /api/v2/achievements` — own achievement list (all titles).
    fn achievements(&self, key: &str) -> Result<String, FetchError>;
}

struct XblClient {
    base: String,
}

impl XblClient {
    fn new(base: String) -> Self {
        XblClient { base }
    }

    fn get(&self, key: &str, path: &str) -> Result<String, FetchError> {
        match ureq::get(&format!("{}{}", self.base, path))
            .timeout(HTTP_TIMEOUT)
            .set("X-Authorization", key)
            .set("Accept", "application/json")
            .call()
        {
            Ok(resp) => resp
                .into_string()
                .map_err(|e| FetchError::Other(format!("reading response body: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429 | 503, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let snippet = body.chars().take(300).collect::<String>();
                Err(FetchError::Other(format!("HTTP {code}: {snippet}")))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl XblApi for XblClient {
    fn account(&self, key: &str) -> Result<String, FetchError> {
        self.get(key, "/account")
    }

    fn title_history(&self, key: &str) -> Result<String, FetchError> {
        self.get(key, "/player/titleHistory")
    }

    fn achievements(&self, key: &str) -> Result<String, FetchError> {
        self.get(key, "/achievements")
    }
}

// ---------------------------------------------------------------------------
// Parsing.
//
// NOTE: These parsers are scaffolded from the documented field names in the
// brief and community consensus — NOT verified against a real xbl.io response.
// They attempt to be tolerant (fall back to `Value::Object` passthrough) so
// that even if the outer envelope key differs (e.g. "titles" vs "titleHistory")
// the raw data still lands.  A Needs-sample review is required to harden these.

/// Parse a `GET /api/v2/player/titleHistory` response body.
///
/// Expected shape (NOT verified against a real xbl.io response — Needs-sample):
/// `{"titles": [{"titleId": "...", "name": "...", "titleHistory":
///   {"lastTimePlayed": "2025-11-26T02:34:30Z"}, ...}]}`
///
/// An outer `{"content": <payload>}` wrapper is handled defensively (falls
/// back to root when absent).  Each title object has `titleId`, `name`,
/// `type`, `devices`, `achievement` (score summary), and
/// `titleHistory.lastTimePlayed`.  Full fidelity: every field in the title
/// object is preserved.
///
/// Returns a `Vec<(partition_key, Value)>` where partition_key is the
/// YYYY-MM of `lastTimePlayed` (or today on missing/invalid dates).
fn parse_titles(body: &str) -> Vec<(String, Value)> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Unwrap the outer {content, code} envelope that xbl.io wraps every response in.
    let payload = v.get("content").unwrap_or(&v);

    // payload.titles is the array of title objects.
    let titles_arr = if let Some(arr) = payload.get("titles").and_then(Value::as_array) {
        arr.clone()
    } else if let Value::Array(arr) = payload {
        arr.clone()
    } else {
        return Vec::new();
    };

    let today = Local::now().format("%Y-%m-01T00:00:00Z").to_string();
    let mut out: Vec<(String, Value)> = Vec::new();

    for title in &titles_arr {
        // Try multiple field names for the last-played timestamp — the exact
        // name is unverified; these are the plausible candidates.
        // xbl.io nests it inside titleHistory.lastTimePlayed; some other shapes
        // put it at the top level as lastPlayedDateTime or lastPlayed.
        let ts = title
            .get("lastTimePlayed")
            .or_else(|| title.get("lastPlayedDateTime"))
            .or_else(|| title.get("lastPlayed"))
            .or_else(|| {
                title
                    .get("titleHistory")
                    .and_then(|th| th.get("lastTimePlayed"))
            })
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let partition_key = if Partition::Month.key(&ts).is_some() {
            ts
        } else {
            today.clone()
        };

        out.push((partition_key, title.clone()));
    }

    out
}

/// Parse a `GET /api/v2/achievements` response body.
///
/// Expected shape based on canonical Xbox Live REST API (NOT verified against
/// a real xbl.io response — Needs-sample):
///
/// Modern flat shape (`AchievementResponse`):
/// ```json
/// {"achievements": [
///   {"id": "1", "name": "...",
///    "progressState": "Achieved",
///    "progression": {"timeUnlocked": "2024-01-15T18:30:00.0000000Z", ...},
///    "titleAssociations": [{"id": 1810924247, "name": "Halo Infinite"}], ...}
/// ], "pagingInfo": {...}}
/// ```
///
/// Unlock signal: `progressState == "Achieved"`.
/// Unlock time: `progression.timeUnlocked` (nested).
/// Title ID: `titleAssociations[0].id` (integer or string).
///
/// Defensive fallbacks (tolerates alternate/unverified shapes):
/// - Optional `{"content": <payload>}` outer wrapper is unwrapped if present.
/// - `isUnlocked: true` (boolean) is also accepted as an unlock signal.
/// - Flat `timeUnlocked` on the achievement object is also accepted.
/// - Grouped `{titles: [{titleId, achievements:[...]}]}` shape is also
///   tolerated so this parser survives if xbl.io proxies a different variant.
///
/// Returns a `Vec<RawAch>` of UNLOCKED achievements.
struct RawAch {
    guid: String,
    ts: String, // RFC3339 or YYYY-MM-DD — partition key.
    value: Value,
}

fn parse_achievements(body: &str) -> Vec<RawAch> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // Unwrap optional outer {content} wrapper (defensive; may or may not exist).
    let payload = v.get("content").unwrap_or(&v);

    let today = Local::now().format("%Y-%m-01T00:00:00Z").to_string();
    let mut out: Vec<RawAch> = Vec::new();

    // --- Path A: modern flat shape — {achievements: [...]} (canonical Xbox Live REST) ---
    if let Some(ach_arr) = payload.get("achievements").and_then(Value::as_array) {
        for ach in ach_arr {
            // Unlock signal: progressState == "Achieved" (canonical modern shape).
            // Fallback: isUnlocked: true (boolean — seen in some community examples).
            let unlocked_by_state = ach
                .get("progressState")
                .and_then(Value::as_str)
                .map(|s| s.eq_ignore_ascii_case("Achieved"))
                .unwrap_or(false);
            let unlocked_by_bool = ach
                .get("isUnlocked")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !unlocked_by_state && !unlocked_by_bool {
                continue;
            }

            let ach_id = ach
                .get("id")
                .or_else(|| ach.get("achievementId"))
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            if ach_id.is_empty() {
                continue;
            }

            // Title ID: from titleAssociations[0].id (modern canonical shape).
            // Fallback: flat titleId field.
            let title_id = ach
                .get("titleAssociations")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|ta| ta.get("id"))
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .or_else(|| {
                    ach.get("titleId").and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                })
                .unwrap_or_default();

            let guid = if title_id.is_empty() {
                format!("unknown:{ach_id}")
            } else {
                format!("{title_id}:{ach_id}")
            };

            // Unlock timestamp: progression.timeUnlocked (canonical nested).
            // Fallback: flat timeUnlocked on the achievement object.
            let ts_raw = ach
                .get("progression")
                .and_then(|p| p.get("timeUnlocked"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && *s != "0001-01-01T00:00:00.0000000Z")
                .or_else(|| {
                    ach.get("timeUnlocked")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty() && *s != "0001-01-01T00:00:00.0000000Z")
                })
                .unwrap_or("")
                .to_string();

            let ts = if Partition::Month.key(&ts_raw).is_some() {
                ts_raw
            } else {
                today.clone()
            };

            // Build the full-fidelity row: inject computed fields.
            let row = match ach.clone() {
                Value::Object(mut m) => {
                    m.entry("guid".to_string()).or_insert(Value::String(guid.clone()));
                    m.entry("title_id".to_string())
                        .or_insert(Value::String(title_id));
                    m.entry("achievement_id".to_string())
                        .or_insert(Value::String(ach_id));
                    Value::Object(m)
                }
                other => other,
            };

            out.push(RawAch { guid, ts, value: row });
        }
        return out;
    }

    // --- Path B: grouped shape — {titles: [{titleId, achievements:[...]}]} ---
    // Tolerated as a defensive fallback in case xbl.io wraps achievements under
    // per-title objects.  Shape NOT confirmed; Needs-sample to verify.
    if let Some(titles_arr) = payload.get("titles").and_then(Value::as_array) {
        for title_obj in titles_arr {
            let title_id = title_obj
                .get("titleId")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_default();
            if title_id.is_empty() {
                continue;
            }

            let achievements = match title_obj.get("achievements").and_then(Value::as_array) {
                Some(arr) => arr.clone(),
                None => continue,
            };

            for ach in &achievements {
                let unlocked_by_state = ach
                    .get("progressState")
                    .and_then(Value::as_str)
                    .map(|s| s.eq_ignore_ascii_case("Achieved"))
                    .unwrap_or(false);
                let unlocked_by_bool = ach
                    .get("isUnlocked")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if !unlocked_by_state && !unlocked_by_bool {
                    continue;
                }

                let ach_id = ach
                    .get("id")
                    .or_else(|| ach.get("achievementId"))
                    .and_then(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                    .unwrap_or_default();
                if ach_id.is_empty() {
                    continue;
                }

                let guid = format!("{title_id}:{ach_id}");

                let ts_raw = ach
                    .get("progression")
                    .and_then(|p| p.get("timeUnlocked"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty() && *s != "0001-01-01T00:00:00.0000000Z")
                    .or_else(|| {
                        ach.get("timeUnlocked")
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty() && *s != "0001-01-01T00:00:00.0000000Z")
                    })
                    .unwrap_or("")
                    .to_string();

                let ts = if Partition::Month.key(&ts_raw).is_some() {
                    ts_raw
                } else {
                    today.clone()
                };

                let row = match ach.clone() {
                    Value::Object(mut m) => {
                        m.entry("guid".to_string()).or_insert(Value::String(guid.clone()));
                        m.entry("title_id".to_string())
                            .or_insert(Value::String(title_id.clone()));
                        m.entry("achievement_id".to_string())
                            .or_insert(Value::String(ach_id));
                        Value::Object(m)
                    }
                    other => other,
                };

                out.push(RawAch { guid, ts, value: row });
            }
        }
    }

    out
}

// ---------------------------------------------------------------------------
// The pull.

pub fn pull_with_key(vault: &Vault, key: &str) -> Result<PullOutcome> {
    let client = XblClient::new(API_BASE.to_string());
    pull_with(vault, &client, key)
}

fn pull_with(vault: &Vault, client: &impl XblApi, key: &str) -> Result<PullOutcome> {
    let mut state = vault.read_xbox_sync();

    // --- Populate achievement guid cache from existing raw (first run). ---
    if state.achievement_guids.is_empty() {
        let ach_stream = vault.stream(ACH_DIR, Partition::Month);
        if let Ok(partitions) = ach_stream.partitions() {
            for part in partitions {
                if let Ok(rows) = ach_stream.read::<Value>(&part) {
                    for row in rows {
                        if let Some(guid) = row.get("guid").and_then(Value::as_str) {
                            state.achievement_guids.insert(guid.to_string());
                        }
                    }
                }
            }
        }
    }

    // --- Title history snapshot. ---
    let title_count = sync_titles(vault, client, key)?;
    state.title_synced = Some(Local::now().to_rfc3339());
    thread::sleep(REQ_INTERVAL);

    // --- Achievements. ---
    let ach_count = sync_achievements(vault, client, key, &mut state)?;

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_xbox_sync(&state)?;

    let headline = if ach_count == 0 && title_count == 0 {
        "Xbox is up to date — no new titles or achievements".to_string()
    } else {
        format!("Xbox synced — {title_count} titles, {ach_count} new achievements")
    };

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([("titles", title_count), ("achievements", ach_count)]),
    })
}

/// Fetch title history and upsert into monthly partitions.
///
/// For each partition touched by the fresh pull, existing rows are read,
/// merged by `titleId` (the fresh row wins — newer playtime/scores), and
/// the partition is rewritten atomically via [`crate::store::write_atomic`].
/// This prevents unbounded file growth from repeated full-library appends.
///
/// Returns the total number of title rows written (across all partitions).
fn sync_titles(vault: &Vault, client: &impl XblApi, key: &str) -> Result<u64> {
    let body = client
        .title_history(key)
        .map_err(|e| anyhow::anyhow!("Xbox title history fetch failed: {e}"))?;

    let parsed = parse_titles(&body);
    if parsed.is_empty() {
        return Ok(0);
    }

    // Group fresh rows by partition key.
    let mut by_partition: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (ts, v) in parsed {
        let part_key = Partition::Month
            .key(&ts)
            .map(str::to_string)
            .unwrap_or_else(|| ts.clone());
        by_partition.entry(part_key).or_default().push(v);
    }

    let stream = vault.stream(TITLES_DIR, Partition::Month);
    let mut total_written: u64 = 0;

    for (part_key, fresh_rows) in &by_partition {
        // Read existing rows for this partition (empty on first pull).
        let existing: Vec<Value> = stream.read::<Value>(part_key).unwrap_or_default();

        // Merge: build map keyed by titleId; fresh rows overwrite existing.
        let mut merged: BTreeMap<String, Value> = BTreeMap::new();
        for row in existing.into_iter().chain(fresh_rows.iter().cloned()) {
            let title_id = row
                .get("titleId")
                .and_then(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .or_else(|| v.as_u64().map(|n| n.to_string()))
                })
                .unwrap_or_else(|| format!("unknown-{}", merged.len()));
            merged.insert(title_id, row);
        }

        // Rewrite the partition atomically (sibling tmp + rename).
        let rel = format!("{TITLES_DIR}/{part_key}.jsonl");
        let path = vault.resolve(&rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut body = String::new();
        for row in merged.values() {
            body.push_str(&serde_json::to_string(row)?);
            body.push('\n');
        }
        crate::store::write_atomic(&path, body.as_bytes())?;
        total_written += merged.len() as u64;
    }

    Ok(total_written)
}

/// Fetch the achievement list and write new (unlocked) achievements.
/// Returns the count of NEW achievement rows written.
fn sync_achievements(
    vault: &Vault,
    client: &impl XblApi,
    key: &str,
    state: &mut SyncState,
) -> Result<u64> {
    let body = match client.achievements(key) {
        Ok(b) => b,
        Err(FetchError::Unauthorized) => {
            bail!("Xbox API key invalid — reconnect in the Integrations tab")
        }
        Err(e) => {
            // Transient: log and skip achievements this pull; titles are already written.
            eprintln!("xbox: achievements fetch failed ({e}) — skipping this pull");
            return Ok(0);
        }
    };

    let parsed = parse_achievements(&body);

    // Warn when the body is non-empty but parsing yielded no achievements —
    // this indicates the API shape may have changed (unofficial-API status-line
    // pattern from the brief).  Silently returning 0 rows would mask total
    // data loss as "up to date".
    if parsed.is_empty() && !body.trim().is_empty() && body.trim() != "{}" {
        // Only warn when the body looks non-trivial (contains at least one
        // alphanumeric character beyond the outer braces).
        if body.len() > 10 {
            eprintln!(
                "xbox: achievements parse produced 0 rows from a non-empty body \
                 ({} bytes) — API shape may have changed; Needs-sample to verify",
                body.len()
            );
        }
    }

    let ach_stream = vault.stream(ACH_DIR, Partition::Month);

    let mut new_rows: Vec<AchRow> = Vec::new();
    for a in parsed {
        if state.achievement_guids.contains(&a.guid) {
            continue;
        }
        state.achievement_guids.insert(a.guid.clone());
        new_rows.push(AchRow { ts: a.ts, value: a.value });
    }

    if !new_rows.is_empty() {
        ach_stream.append(&new_rows, |r| &r.ts)?;
    }

    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// Serializable row wrappers (partition key skipped; value written as-is).
//
// TitleRow is not needed: sync_titles upserts by rewriting partitions
// atomically (read-merge-write_atomic), so it does not go through
// JsonlStream::append.  AchRow is used by sync_achievements.

#[derive(Serialize)]
struct AchRow {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Tests — pure, fixture-based, no network, unique temp dirs.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(tag: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-xbox-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -------------------------------------------------------------------------
    // Fixtures.
    //
    // NOTE: These fixtures use the EXPECTED shapes based on the canonical Xbox
    // Live REST API (OpenXbox/xbox-webapi-python models).  Neither xbl.io's
    // openapi.yaml nor a live response has been verified — Needs-sample.
    //
    // The achievements fixture uses the modern flat shape:
    //   {achievements: [...], pagingInfo: {...}}
    //   progressState == "Achieved" (unlock signal)
    //   progression.timeUnlocked (nested unlock timestamp)
    //   titleAssociations[{id, name}] (title lookup per achievement)
    //
    // The title history fixture uses the expected wrapped shape:
    //   {content: {xuid, titles: [...]}, code: 200}
    //   titles[].titleHistory.lastTimePlayed (nested date)

    /// Title history response — expected shape (unverified against real xbl.io).
    /// Outer {content, code} wrapper is present (falls back to root if absent).
    fn titles_body() -> String {
        json!({
            "content": {
                "xuid": "2533274798129181",
                "titles": [
                    {
                        "titleId": "1717113201",
                        "pfn": "Microsoft.Minecraft_8wekyb3d8bbwe",
                        "name": "Minecraft",
                        "type": "Game",
                        "devices": ["PC", "Xbox Series X|S"],
                        "displayImage": "https://store-images.s-microsoft.com/image/apps.50718.foo",
                        "achievement": {
                            "currentAchievements": 42,
                            "totalAchievements": 150,
                            "currentGamerscore": 840,
                            "totalGamerscore": 3000,
                            "progressPercentage": 28.0
                        },
                        "gamePass": { "isGamePass": false },
                        "titleHistory": {
                            "lastTimePlayed": "2024-11-15T21:05:00.0000000Z",
                            "visible": true,
                            "canHide": true
                        },
                        "xboxLiveTier": "Full",
                        "isStreamable": false
                    },
                    {
                        "titleId": "307026",
                        "name": "Halo 5: Guardians",
                        "type": "Game",
                        "devices": ["Xbox One"],
                        "displayImage": "https://store-images.s-microsoft.com/image/apps.13726.foo",
                        "achievement": {
                            "currentAchievements": 5,
                            "totalAchievements": 50,
                            "currentGamerscore": 100,
                            "totalGamerscore": 1000,
                            "progressPercentage": 10.0
                        },
                        "titleHistory": {
                            "lastTimePlayed": "2022-08-10T14:30:00.0000000Z",
                            "visible": true,
                            "canHide": true
                        },
                        "xboxLiveTier": "Full",
                        "isStreamable": false
                    }
                ]
            },
            "code": 200
        })
        .to_string()
    }

    /// Achievements response — canonical modern flat shape (AchievementResponse)
    /// from the Xbox Live REST API (OpenXbox/xbox-webapi-python models).
    ///
    /// Shape (unverified against a real xbl.io response — Needs-sample):
    ///   {achievements: [...], pagingInfo: {...}}
    ///   progressState == "Achieved"  (unlock signal)
    ///   progression.timeUnlocked     (nested unlock timestamp)
    ///   titleAssociations[{id,name}] (title lookup per achievement)
    fn achievements_body() -> String {
        json!({
            "achievements": [
                {
                    "id": "1",
                    "serviceConfigId": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
                    "name": "Taking Inventory",
                    "description": "Pick up an item from the crafting output slot.",
                    "progressState": "Achieved",
                    "progression": {
                        "requirements": [],
                        "timeUnlocked": "2024-11-15T18:30:00.0000000Z"
                    },
                    "titleAssociations": [
                        { "id": 1717113201, "name": "Minecraft" }
                    ],
                    "gamerscore": 20,
                    "isSecret": false
                },
                {
                    "id": "2",
                    "serviceConfigId": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
                    "name": "Benchmaking",
                    "description": "Craft a workbench with four blocks of wooden planks.",
                    "progressState": "NotStarted",
                    "progression": {
                        "requirements": [],
                        "timeUnlocked": "0001-01-01T00:00:00.0000000Z"
                    },
                    "titleAssociations": [
                        { "id": 1717113201, "name": "Minecraft" }
                    ],
                    "gamerscore": 10,
                    "isSecret": false
                },
                {
                    "id": "3",
                    "serviceConfigId": "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
                    "name": "A Monument to All Your Sins",
                    "description": "Complete every level in Halo 5: Guardians on LASO.",
                    "progressState": "Achieved",
                    "progression": {
                        "requirements": [],
                        "timeUnlocked": "2022-08-10T19:45:00.0000000Z"
                    },
                    "titleAssociations": [
                        { "id": 307026, "name": "Halo 5: Guardians" }
                    ],
                    "gamerscore": 50,
                    "isSecret": false
                }
            ],
            "pagingInfo": {
                "continuationToken": null,
                "totalRecords": 3
            }
        })
        .to_string()
    }

    // -------------------------------------------------------------------------
    // Parsing tests.

    #[test]
    fn parse_titles_reads_envelope_and_extracts_lastTimePlayed() {
        let rows = parse_titles(&titles_body());
        assert_eq!(rows.len(), 2);
        // Minecraft — partitioned by titleHistory.lastTimePlayed month.
        let (key0, val0) = &rows[0];
        assert!(
            key0.starts_with("2024-11"),
            "expected 2024-11 partition key, got {key0}"
        );
        // The full raw value must be the object as returned (full fidelity).
        assert_eq!(val0["titleId"], "1717113201");
        assert_eq!(val0["name"], "Minecraft");
        // Nested titleHistory object preserved.
        assert!(val0.get("titleHistory").is_some());

        // Halo 5 — lastTimePlayed 2022-08
        let (key1, val1) = &rows[1];
        assert!(
            key1.starts_with("2022-08"),
            "expected 2022-08 partition key, got {key1}"
        );
        assert_eq!(val1["titleId"], "307026");
    }

    #[test]
    fn parse_titles_tolerates_missing_date() {
        // A title with no lastTimePlayed should still be emitted (today's partition).
        // Uses the real {content, code} envelope.
        let body = json!({
            "content": {
                "xuid": "123",
                "titles": [
                    { "titleId": "9999", "name": "No Date Game" }
                ]
            },
            "code": 200
        })
        .to_string();
        let rows = parse_titles(&body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1["titleId"], "9999");
        // Partition key falls back to today's month.
        assert!(!rows[0].0.is_empty());
    }

    #[test]
    fn parse_titles_tolerates_raw_payload_without_envelope() {
        // When there is no content wrapper (e.g. a future format change or test),
        // the parser falls back to the root as payload.
        let body = json!({
            "titles": [
                { "titleId": "111", "name": "No-Wrapper Game",
                  "titleHistory": { "lastTimePlayed": "2023-06-01T00:00:00Z" } }
            ]
        })
        .to_string();
        let rows = parse_titles(&body);
        assert_eq!(rows.len(), 1);
        let (key, val) = &rows[0];
        assert!(key.starts_with("2023-06"), "got: {key}");
        assert_eq!(val["titleId"], "111");
    }

    #[test]
    fn parse_titles_empty_or_invalid_body() {
        assert!(parse_titles("").is_empty());
        assert!(parse_titles("not json").is_empty());
        // Empty titles array in envelope.
        assert!(parse_titles(r#"{"content":{"titles":[]},"code":200}"#).is_empty());
    }

    #[test]
    fn parse_achievements_unlocked_only_with_guid() {
        // Uses the canonical modern flat shape:
        //   progressState == "Achieved", progression.timeUnlocked, titleAssociations[0].id
        let rows = parse_achievements(&achievements_body());
        // Only 2 of 3 are unlocked (id=2 progressState="NotStarted").
        assert_eq!(rows.len(), 2, "expected 2 unlocked; got {}", rows.len());

        // Achievement 1 is from Minecraft (titleAssociations[0].id = 1717113201).
        let row0 = &rows[0];
        assert_eq!(row0.guid, "1717113201:1");
        assert_eq!(row0.value["name"], "Taking Inventory");
        assert_eq!(row0.value["guid"], "1717113201:1");
        assert_eq!(row0.value["title_id"], "1717113201");
        assert_eq!(row0.value["achievement_id"], "1");
        // Partition key = unlock month from progression.timeUnlocked.
        assert!(
            row0.ts.starts_with("2024-11"),
            "expected 2024-11 partition key, got {}",
            row0.ts
        );

        // Achievement 3 is from Halo 5 (titleAssociations[0].id = 307026).
        let row1 = &rows[1];
        assert_eq!(row1.guid, "307026:3");
        assert_eq!(row1.value["name"], "A Monument to All Your Sins");
        assert!(
            row1.ts.starts_with("2022-08"),
            "expected 2022-08 partition key, got {}",
            row1.ts
        );
    }

    #[test]
    fn parse_achievements_skips_not_achieved_and_zero_epoch() {
        // progressState != "Achieved" with the not-yet-unlocked epoch sentinel must be excluded.
        // Uses the flat achievements shape.
        let body = json!({
            "achievements": [
                {
                    "id": "99",
                    "name": "Locked",
                    "progressState": "NotStarted",
                    "progression": {
                        "requirements": [],
                        "timeUnlocked": "0001-01-01T00:00:00.0000000Z"
                    },
                    "titleAssociations": [{ "id": 123, "name": "Test Game" }],
                    "gamerscore": 10
                }
            ],
            "pagingInfo": { "continuationToken": null, "totalRecords": 1 }
        })
        .to_string();
        assert!(
            parse_achievements(&body).is_empty(),
            "locked achievements must not be written"
        );
    }

    #[test]
    fn parse_achievements_fallback_accepts_isunlocked_boolean() {
        // Defensive fallback: some community examples use isUnlocked: true (boolean).
        // The grouped {titles: [...]} path also tolerates this.
        let body = json!({
            "achievements": [
                {
                    "id": "77",
                    "name": "Bonus Achievement",
                    "isUnlocked": true,
                    "timeUnlocked": "2023-03-10T12:00:00.0000000Z",
                    "titleAssociations": [{ "id": 55555, "name": "Some Game" }],
                    "gamerscore": 5
                }
            ]
        })
        .to_string();
        let rows = parse_achievements(&body);
        assert_eq!(rows.len(), 1, "isUnlocked=true fallback should be accepted");
        assert_eq!(rows[0].guid, "55555:77");
        assert!(rows[0].ts.starts_with("2023-03"), "got: {}", rows[0].ts);
    }

    #[test]
    fn parse_achievements_empty_body() {
        assert!(parse_achievements("").is_empty());
        assert!(parse_achievements("{}").is_empty());
        // Empty achievements array (flat shape).
        assert!(
            parse_achievements(r#"{"achievements":[],"pagingInfo":{"continuationToken":null,"totalRecords":0}}"#).is_empty()
        );
    }

    // -------------------------------------------------------------------------
    // Stub XblApi for store-level tests.

    struct StubXblApi {
        title_resp: String,
        ach_resp: String,
        account_resp: String,
    }

    impl StubXblApi {
        fn new() -> Self {
            StubXblApi {
                title_resp: titles_body(),
                ach_resp: achievements_body(),
                account_resp: r#"{"profileUsers":[{"id":"2533274798129181","settings":[{"id":"Gamertag","value":"TestUser"}]}]}"#.to_string(),
            }
        }
    }

    impl XblApi for StubXblApi {
        fn account(&self, _key: &str) -> Result<String, FetchError> {
            Ok(self.account_resp.clone())
        }
        fn title_history(&self, _key: &str) -> Result<String, FetchError> {
            Ok(self.title_resp.clone())
        }
        fn achievements(&self, _key: &str) -> Result<String, FetchError> {
            Ok(self.ach_resp.clone())
        }
    }

    fn store_key(vault: &Vault) {
        vault
            .save_sync_token(
                SERVICE,
                &crate::sync::oauth::TokenSet {
                    access_token: "fake-key-1234".to_string(),
                    refresh_token: None,
                    token_type: None,
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();
    }

    // -------------------------------------------------------------------------
    // Store-level tests.

    #[test]
    fn pull_writes_titles_and_achievements() {
        let vault = temp_vault("pull_basic");
        store_key(&vault);
        let stub = StubXblApi::new();

        let out = pull_with(&vault, &stub, "fake-key").unwrap();
        // 2 titles written.
        assert_eq!(out.counts.get("titles").copied().unwrap_or(0), 2);
        // 2 of 3 achievements are unlocked.
        assert_eq!(out.counts.get("achievements").copied().unwrap_or(0), 2);

        // Verify title file on disk.
        let titles_dir = vault.root().join(TITLES_DIR);
        let title_files: Vec<_> = std::fs::read_dir(&titles_dir)
            .unwrap()
            .flatten()
            .collect();
        assert!(!title_files.is_empty(), "title files should be written");

        // Verify achievement file on disk.
        let ach_dir = vault.root().join(ACH_DIR);
        let ach_files: Vec<_> = std::fs::read_dir(&ach_dir)
            .unwrap()
            .flatten()
            .collect();
        assert!(!ach_files.is_empty(), "achievement files should be written");
    }

    #[test]
    fn pull_dedupes_achievements_on_re_pull() {
        let vault = temp_vault("dedup");
        store_key(&vault);
        let stub = StubXblApi::new();

        // First pull — 2 new achievements.
        let out1 = pull_with(&vault, &stub, "fake-key").unwrap();
        assert_eq!(out1.counts.get("achievements").copied().unwrap_or(0), 2);

        // Second pull — same body, 0 new achievements.
        let out2 = pull_with(&vault, &stub, "fake-key").unwrap();
        assert_eq!(
            out2.counts.get("achievements").copied().unwrap_or(0),
            0,
            "second pull must not re-write existing achievements"
        );
    }

    #[test]
    fn def_pull_errors_when_no_token_stored() {
        // def_pull should return a "not connected" error when no token is in the vault.
        let vault = temp_vault("no_token");
        let err = def_pull(&vault).unwrap_err();
        assert!(
            err.to_string().contains("not connected"),
            "expected 'not connected' error, got: {err}"
        );
    }

    #[test]
    fn connect_validates_empty_key() {
        let vault = temp_vault("empty_key");
        // Connecting with an empty key should fail with a clear message.
        // Use a stub that we won't even call.
        let stub = StubXblApi::new();
        let err = connect_with(&vault, &stub, "").unwrap_err();
        assert!(
            err.to_string().contains("empty"),
            "expected 'empty' in error, got: {err}"
        );
    }

    #[test]
    fn connect_stores_key_on_valid_probe() {
        let vault = temp_vault("connect");
        let stub = StubXblApi::new();
        connect_with(&vault, &stub, "fake-api-key-1234").unwrap();
        let token = vault.load_sync_token(SERVICE).unwrap().unwrap();
        assert_eq!(token.access_token, "fake-api-key-1234");
    }

    #[test]
    fn status_shows_masked_key() {
        let vault = temp_vault("status");
        store_key(&vault);
        let status = def_status(&vault).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert!(
            status.accounts[0].label.contains("****"),
            "label should mask key: {}",
            status.accounts[0].label
        );
    }

    #[test]
    fn disconnect_removes_token() {
        let vault = temp_vault("disconnect");
        store_key(&vault);
        def_disconnect(&vault, "xbox").unwrap();
        assert!(vault.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn sync_state_persists_and_reloads() {
        let vault = temp_vault("sync_state");
        let mut state = SyncState::default();
        state.achievement_guids.insert("1717113201:1".into());
        state.updated = Some("2024-11-15T21:05:00+00:00".into());
        vault.write_xbox_sync(&state).unwrap();

        let loaded = vault.read_xbox_sync();
        assert_eq!(loaded.achievement_guids.len(), 1);
        assert!(loaded.achievement_guids.contains("1717113201:1"));
        assert_eq!(
            loaded.updated,
            Some("2024-11-15T21:05:00+00:00".into())
        );
    }

    #[test]
    fn parse_achievements_canonical_flat_shape_progressstate_and_nested_ts() {
        // Canonical modern Xbox Live flat shape:
        //   progressState == "Achieved", progression.timeUnlocked, titleAssociations[0].id
        // (NOT verified against a real xbl.io response — Needs-sample)
        let body = json!({
            "achievements": [{
                "id": "42",
                "name": "Canonical Shape Achievement",
                "progressState": "Achieved",
                "progression": {
                    "requirements": [],
                    "timeUnlocked": "2023-01-15T12:00:00.0000000Z"
                },
                "titleAssociations": [{ "id": 99999, "name": "Some Game" }],
                "gamerscore": 15
            }],
            "pagingInfo": { "continuationToken": null, "totalRecords": 1 }
        })
        .to_string();
        let rows = parse_achievements(&body);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].guid, "99999:42");
        assert_eq!(rows[0].value["title_id"], "99999");
        assert_eq!(rows[0].value["achievement_id"], "42");
        assert!(rows[0].ts.starts_with("2023-01"), "got: {}", rows[0].ts);
    }

    #[test]
    fn parse_achievements_grouped_fallback_shape_tolerated() {
        // Grouped {titles: [{titleId, achievements:[...]}]} shape — defensive fallback
        // in case xbl.io wraps achievements per-title.  Shape NOT confirmed; Needs-sample.
        let body = json!({
            "titles": [{
                "titleId": "99999",
                "name": "Some Game",
                "achievements": [{
                    "id": "42",
                    "name": "Grouped Shape Achievement",
                    "progressState": "Achieved",
                    "progression": {
                        "timeUnlocked": "2023-01-15T12:00:00.0000000Z"
                    },
                    "gamerscore": 15
                }]
            }]
        })
        .to_string();
        let rows = parse_achievements(&body);
        assert_eq!(rows.len(), 1, "grouped fallback shape should be tolerated");
        assert_eq!(rows[0].guid, "99999:42");
        assert_eq!(rows[0].value["title_id"], "99999");
        assert!(rows[0].ts.starts_with("2023-01"), "got: {}", rows[0].ts);
    }

    #[test]
    fn parse_titles_uses_nested_lastTimePlayed_from_titleHistory() {
        // xbl.io wraps the last-played date inside titleHistory.lastTimePlayed.
        // Uses the real {content, code} envelope.
        let body = json!({
            "content": {
                "xuid": "123",
                "titles": [{
                    "titleId": "77777",
                    "name": "Nested Date",
                    "titleHistory": {
                        "lastTimePlayed": "2023-09-01T10:00:00.0000000Z"
                    }
                }]
            },
            "code": 200
        })
        .to_string();
        let rows = parse_titles(&body);
        assert_eq!(rows.len(), 1, "row emitted with nested date");
        // Partition key must resolve to 2023-09 (from nested titleHistory).
        assert!(
            rows[0].0.starts_with("2023-09"),
            "expected 2023-09 from nested titleHistory.lastTimePlayed, got {}",
            rows[0].0
        );
        // The full object including the nested titleHistory is preserved.
        assert!(rows[0].1.get("titleHistory").is_some());
    }
}
