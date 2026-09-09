//! RetroAchievements — retro-game achievement unlock history and completion
//! progress via the official REST API (`api.retroachievements.org`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/retroachievements.md
//!
//! A **Periodic** cloud pull. `gaming/` is **raw-only** in the taxonomy —
//! there is no write-time contract for achievement unlocks — so this collector
//! writes full-fidelity raw JSONL and touches no `DOMAINS` struct or spec.
//! Two streams:
//!
//! - **unlocks** — one row per achievement earned at
//!   `gaming/retroachievements/YYYY-MM.jsonl`, partitioned by the unlock
//!   month, deduped by `guid` = `"{AchievementID}_{Date_epoch}"`.
//!   Full fidelity: all API fields preserved — Date, HardcoreMode,
//!   AchievementID, Title, Description, BadgeName, Points, TrueRatio, Type,
//!   Author, AuthorULID, GameTitle, GameIcon, GameID, ConsoleName,
//!   CumulScore, BadgeURL, GameURL.
//! - **progress** — a current-state snapshot rewritten whole each pull at
//!   `gaming/retroachievements/progress.jsonl` (one row per game the user
//!   has played, covering completion counts + mastery award kind).
//!
//! The incremental poll uses `API_GetAchievementsEarnedBetween.php` with a
//! Unix-timestamp cursor persisted in `.trove/retroachievements-sync.json`.
//! The first sync passes `from=0` to backfill the full unlock history.
//!
//! Auth: two credentials from retroachievements.org/settings — the **Web API
//! Key** (y param) and the **account username** (u param, also used as z).
//! The user pastes `username:apikey` as a single field; the module splits on
//! the first `:`, stores the key in `access_token` and the username in `scope`
//! of a never-expiring TokenSet under `.trove/sync/retroachievements.json`.
//! The RA v1 REST API sends `y=<key>&u=<username>&z=<username>`; sending the
//! key as the username (u) would query a nonexistent user, returning empty [].
//!
//! JSON shapes confirmed from the official API docs at
//! `api-docs.retroachievements.org`:
//! - `API_GetAchievementsEarnedBetween` → array of objects with PascalCase
//!   fields: Date, HardcoreMode, AchievementID, Title, Description,
//!   BadgeName, Points, TrueRatio, Type, Author, AuthorULID, GameTitle,
//!   GameIcon, GameID, ConsoleName, CumulScore, BadgeURL, GameURL.
//! - `API_GetUserCompletionProgress` → object with `Count`, `Total`,
//!   `Results` array of {GameID, Title, ImageIcon, ConsoleID, ConsoleName,
//!   MaxPossible, NumAwarded, NumAwardedHardcore, MostRecentAwardedDate,
//!   HighestAwardKind, HighestAwardDate}.

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
// Paths and constants.

/// Month-partitioned unlock stream directory.
const UNLOCKS_DIR: &str = "gaming/retroachievements";
/// Current-state progress snapshot (rewritten whole each pull).
const PROGRESS_REL: &str = "gaming/retroachievements/progress.jsonl";
/// Non-secret rebuildable cursor — not under `.trove/sync/` (that's secrets).
const SYNC_FILE: &str = ".trove/retroachievements-sync.json";
/// Service id for the 0600 secret store (API key slot).
const SERVICE: &str = "retroachievements";

const API_BASE: &str = "https://retroachievements.org";
/// Politeness: ~2 req/sec → 500ms between requests.
const REQ_INTERVAL: Duration = Duration::from_millis(500);
/// Connection timeout kept short so a hung request can't stall the loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between Periodic syncs. Hourly is polite and cheap.
pub const RA_SYNC_SECS: u64 = 3600;
/// Max results per completion-progress page.
const PROGRESS_PAGE_SIZE: u32 = 500;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(UNLOCKS_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("unlocks").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("retroachievements synced — {n} new unlocks")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "retroachievements sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let unlocks = out.counts.get("unlocks").copied().unwrap_or(0);
    let progress = out.counts.get("progress").copied().unwrap_or(0);
    let headline = if unlocks == 0 {
        format!("RetroAchievements is up to date — no new unlocks ({progress} progress rows)")
    } else {
        format!("RetroAchievements synced — {unlocks} new unlocks, {progress} progress rows")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "retroachievements",
        name: "RetroAchievements",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Syncs your RetroAchievements unlock history (achievement id, timestamp, \
                      hardcore flag, points, game/console) and per-game completion progress. \
                      Requires your personal API key from retroachievements.org/settings.",
        domain: "gaming",
        vault_path: "gaming/retroachievements/",
        toggleable: true,
        setup: &[
            "Find your Web API Key at retroachievements.org/settings (\"Keys\" section).",
            "In the connect card, enter your RA username, a colon, then your API key (e.g. MyUsername:aBcDeFgHiJkLmNoPqRsTuVwXyZ123456).",
            "First sync backfills your full unlock history; later syncs fetch only what's new.",
        ],
        caveats: "Reads your complete unlock history via the official RA API. \
                  Rate-limited by RetroAchievements; the hourly poll is well below the threshold. \
                  Progress snapshot shows current completion state; historical unlock events \
                  are in the dated monthly stream.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(RA_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("retroachievements"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = "username:apikey" combined).

/// Split a `"username:apikey"` paste into `(username, api_key)`.
/// Returns `None` if either part is missing or empty. The API key is a
/// 32-char hex string that cannot contain a colon, so splitting on the first
/// `:` is unambiguous.
fn split_credential(pasted: &str) -> Option<(String, String)> {
    let (user, key) = pasted.trim().split_once(':')?;
    let user = user.trim().to_string();
    let key = key.trim().to_string();
    if user.is_empty() || key.is_empty() {
        return None;
    }
    Some((user, key))
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let Some((username, api_key)) = split_credential(pasted) else {
        bail!(
            "expected \"username:apikey\" — paste your RetroAchievements \
             username, a colon, then your Web API Key from \
             retroachievements.org/settings → Keys"
        );
    };
    // Best-effort validation with the REAL username so that a bad key returns
    // Unauthorized (401/403) rather than 200 [] (unknown user). A network blip
    // is tolerated — only an explicit auth rejection is surfaced.
    let client = RaClient::new(API_BASE.to_string(), username.clone());
    if let Err(FetchError::Unauthorized) = client.achievements_between(&api_key, 0, 1) {
        bail!(
            "RetroAchievements rejected the API key — \
             check retroachievements.org/settings → Keys"
        );
    }
    // API key in `access_token`; (non-secret) username in `scope`.
    let token = crate::sync::oauth::TokenSet {
        access_token: api_key,
        refresh_token: None,
        token_type: None,
        scope: Some(username),
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // Username is in `scope`; fall back to a generic label if missing
        // (handles tokens stored before this field was introduced).
        let label = token
            .scope
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "RetroAchievements".to_string());
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

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste
/// `username:apikey` — your RA username, a colon, then your Web API Key.
/// No app registration needed; the key is personal to each RA account.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "retroachievements",
    display_name: "RetroAchievements",
    methods: &[ConnectMethod::TokenPaste {
        label: "RetroAchievements username:apikey",
        help: "Enter your RetroAchievements username, a colon, then your Web API Key from \
               retroachievements.org/settings → \"Keys\" section. \
               Example: MyUsername:aBcDeFgHiJkLmNoPqRsTuVwXyZ123456. \
               The key gives read-only access to your own achievement history.",
        placeholder: "MyUsername:aBcDeFgHiJkLmNoPqRsTuVwXyZ123456",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["retroachievements"],
    setup: &[
        "Go to retroachievements.org/settings and scroll to the \"Keys\" section.",
        "Copy your Web API Key.",
        "In the box below, type your RetroAchievements username, then a colon (:), then paste the key. Example: MyUsername:aBcDeFgHiJkLmNoPqRsTuVwXyZ123456.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized — check your API key"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Injectable API surface. Production calls the real RA REST API; tests use
/// a deterministic stub with pre-loaded fixture responses.
trait RaApi {
    /// `API_GetAchievementsEarnedBetween` for [from_epoch, to_epoch).
    /// Returns the raw JSON Value (an array).
    fn achievements_between(
        &self,
        api_key: &str,
        from_epoch: i64,
        to_epoch: i64,
    ) -> Result<Value, FetchError>;

    /// `API_GetUserCompletionProgress` — one page (offset + count).
    fn completion_progress(
        &self,
        api_key: &str,
        offset: u32,
        count: u32,
    ) -> Result<Value, FetchError>;
}

/// Thin production client. Base URL is injected so tests run without hitting
/// the network. The RA v1 API uses three query params: `z` (calling username),
/// `y` (API key), and `u` (target username — same as the caller when pulling
/// personal data). Username and key are two distinct values; the key must NOT
/// be used in place of the username or the API returns 200 [] (unknown user).
struct RaClient {
    base: String,
    username: String,
}

impl RaClient {
    fn new(base: String, username: String) -> Self {
        RaClient { base, username }
    }

    fn handle_response(result: Result<ureq::Response, ureq::Error>) -> Result<Value, FetchError> {
        match result {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("JSON decode: {e}"))),
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

impl RaApi for RaClient {
    fn achievements_between(
        &self,
        api_key: &str,
        from_epoch: i64,
        to_epoch: i64,
    ) -> Result<Value, FetchError> {
        let result = ureq::get(&format!(
            "{}/API/API_GetAchievementsEarnedBetween.php",
            self.base
        ))
        .timeout(HTTP_TIMEOUT)
        .query("z", &self.username)
        .query("y", api_key)
        .query("u", &self.username)
        .query("f", &from_epoch.to_string())
        .query("t", &to_epoch.to_string())
        .call();
        Self::handle_response(result)
    }

    fn completion_progress(
        &self,
        api_key: &str,
        offset: u32,
        count: u32,
    ) -> Result<Value, FetchError> {
        let result = ureq::get(&format!(
            "{}/API/API_GetUserCompletionProgress.php",
            self.base
        ))
        .timeout(HTTP_TIMEOUT)
        .query("z", &self.username)
        .query("y", api_key)
        .query("u", &self.username)
        .query("o", &offset.to_string())
        .query("c", &count.to_string())
        .call();
        Self::handle_response(result)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Unix seconds of the latest unlock epoch written. The next pull passes
    /// `from = watermark_ts + 1`. None = first sync (passes `from = 0`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark_ts: Option<i64>,
    /// RFC3339 local time of the last successful progress snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    progress_synced: Option<String>,
    /// RFC3339 local time of the last successful sync (any stream).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ra_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ra_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Build the stable `guid` for an unlock row:
/// `"{AchievementID}_{UnixEpoch}"`.
fn make_unlock_guid(row: &Value) -> Option<String> {
    let aid = row.get("AchievementID")?.as_u64()?;
    let date_str = row.get("Date")?.as_str()?;
    let epoch = parse_ra_date(date_str)?;
    Some(format!("{aid}_{epoch}"))
}

/// Parse `"YYYY-MM-DD HH:MM:SS"` (UTC) → Unix epoch seconds.
/// Returns None on a malformed string.
fn parse_ra_date(s: &str) -> Option<i64> {
    use chrono::{NaiveDateTime, TimeZone, Utc};
    let ndt = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S").ok()?;
    Some(Utc.from_utc_datetime(&ndt).timestamp())
}

/// The `"YYYY-MM"` partition key from a `"YYYY-MM-DD HH:MM:SS"` date string.
fn month_key(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() >= 7 {
        Some(s[..7].to_string())
    } else {
        None
    }
}

/// An unlock row ready for the JSONL stream. `ts` drives partitioning
/// (not serialized); `raw` is the full-fidelity object with `guid` injected.
#[derive(Serialize)]
struct UnlockRow {
    #[serde(skip)]
    ts: String, // "YYYY-MM" month key
    #[serde(flatten)]
    raw: Value,
}

/// Parse the raw JSON array from `API_GetAchievementsEarnedBetween`.
/// Returns parsed rows (with `guid` injected) and the max epoch seen.
fn parse_unlocks(json: &Value) -> (Vec<UnlockRow>, Option<i64>) {
    let arr = match json.as_array() {
        Some(a) => a,
        None => return (Vec::new(), None),
    };
    let mut rows = Vec::new();
    let mut max_epoch: Option<i64> = None;

    for raw in arr {
        let date_str = match raw.get("Date").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        let epoch = match parse_ra_date(date_str) {
            Some(e) => e,
            None => continue,
        };
        let ts = match month_key(date_str) {
            Some(k) => k,
            None => continue,
        };
        let guid = match make_unlock_guid(raw) {
            Some(g) => g,
            None => continue,
        };
        // Inject guid into a clone of the raw row before writing.
        let mut obj = raw.clone();
        if let Some(m) = obj.as_object_mut() {
            m.insert("guid".to_string(), Value::String(guid));
        }
        if max_epoch.is_none_or(|mx| epoch > mx) {
            max_epoch = Some(epoch);
        }
        rows.push(UnlockRow { ts, raw: obj });
    }
    (rows, max_epoch)
}

// ---------------------------------------------------------------------------
// The pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "RetroAchievements is not connected — paste your username:apikey \
                 in the Integrations tab"
            )
        })?;
    let api_key = token.access_token;
    if api_key.trim().is_empty() {
        anyhow::bail!(
            "RetroAchievements API key is missing — reconnect in the Integrations tab"
        );
    }
    // Username is stored in `scope`. If somehow absent (pre-fix token), fall
    // back to an empty string so the request fails with 200 [] rather than
    // panicking — the user will see zero unlocks and can reconnect.
    let username = token.scope.unwrap_or_default();
    let client = RaClient::new(API_BASE.to_string(), username);
    pull_with(vault, &client, &api_key)
}

fn pull_with(vault: &Vault, client: &impl RaApi, api_key: &str) -> Result<PullOutcome> {
    let mut state = vault.read_ra_sync();

    // --- Unlocks: incremental window from watermark → now. ---
    let unlocks_written = sync_unlocks(vault, client, api_key, &mut state)?;
    thread::sleep(REQ_INTERVAL);

    // --- Progress: current-state snapshot (paginated). ---
    let progress_count = match sync_progress(vault, client, api_key) {
        Ok(n) => {
            state.progress_synced = Some(Local::now().to_rfc3339());
            n
        }
        Err(e) => {
            // Keep unlocks; log the progress failure gracefully.
            eprintln!("[retroachievements] progress snapshot skipped: {e}");
            0
        }
    };

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ra_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{unlocks_written} unlocks, {progress_count} progress rows"),
        counts: BTreeMap::from([
            ("unlocks", unlocks_written),
            ("progress", progress_count),
        ]),
    })
}

/// Fetch all new unlocks since the watermark and write to month-partitioned
/// JSONL. Advances the watermark to the latest epoch seen. Cursor is NOT
/// advanced if the write set would be empty from a parse miss (guards against
/// data loss on a bad API response).
fn sync_unlocks(
    vault: &Vault,
    client: &impl RaApi,
    api_key: &str,
    state: &mut SyncState,
) -> Result<u64> {
    let stream = vault.stream(UNLOCKS_DIR, Partition::Month);

    // Load existing guids for deduplication across all partitions.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for row in stream.read::<Value>(&key)? {
            if let Some(g) = row.get("guid").and_then(|v| v.as_str()) {
                seen.insert(g.to_string());
            }
        }
    }

    let from = state.watermark_ts.map(|ts| ts + 1).unwrap_or(0);
    let to = chrono::Utc::now().timestamp();

    let json = match client.achievements_between(api_key, from, to) {
        Ok(v) => v,
        Err(FetchError::Unauthorized) => {
            bail!(
                "RetroAchievements API key rejected — reconnect in the Integrations tab"
            )
        }
        Err(e) => bail!("RetroAchievements unlocks fetch failed: {e}"),
    };

    let (parsed, max_epoch) = parse_unlocks(&json);

    // Buffer new rows, skip already-seen guids.
    let mut new_rows: Vec<UnlockRow> = Vec::new();
    for row in parsed {
        let guid = match row.raw.get("guid").and_then(|v| v.as_str()) {
            Some(g) => g.to_string(),
            None => continue,
        };
        if !seen.insert(guid) {
            continue; // already stored
        }
        new_rows.push(row);
    }

    let written = new_rows.len() as u64;
    if !new_rows.is_empty() {
        // Verify all partition keys are valid before touching disk.
        let all_valid = new_rows.iter().all(|r| Partition::Month.key(&r.ts).is_some());
        if !all_valid {
            bail!(
                "RetroAchievements: one or more unlock rows has an unparseable date \
                 — cursor NOT advanced"
            );
        }
        stream.append(&new_rows, |r| &r.ts)?;
    }

    // Advance watermark to the latest epoch (forward-only). If response was
    // genuinely empty, max_epoch is None and cursor stays where it was.
    if let Some(epoch) = max_epoch {
        if state.watermark_ts.is_none_or(|w| epoch > w) {
            state.watermark_ts = Some(epoch);
        }
    }

    Ok(written)
}

/// Fetch all completion progress pages (drain until short/empty page) and
/// write a current-state snapshot. Rewritten whole each pull.
fn sync_progress(vault: &Vault, client: &impl RaApi, api_key: &str) -> Result<u64> {
    let mut all_rows: Vec<Value> = Vec::new();
    let mut offset: u32 = 0;

    loop {
        let json = match client.completion_progress(api_key, offset, PROGRESS_PAGE_SIZE) {
            Ok(v) => v,
            Err(FetchError::Unauthorized) => bail!("RetroAchievements API key rejected"),
            Err(e) => bail!("RetroAchievements progress fetch failed: {e}"),
        };

        let results = match json.get("Results").and_then(|v| v.as_array()) {
            Some(r) => r,
            None => break, // unexpected shape — stop gracefully
        };

        let page_len = results.len() as u32;
        all_rows.extend(results.iter().cloned());

        if page_len < PROGRESS_PAGE_SIZE {
            break; // last page (short or empty)
        }
        offset += page_len;
        thread::sleep(REQ_INTERVAL);
    }

    let total = all_rows.len() as u64;
    vault.write_snapshot(PROGRESS_REL, &all_rows)?;
    Ok(total)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-ra-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixtures confirmed from official API docs (api-docs.retroachievements.org).

    /// Two unlock rows from `API_GetAchievementsEarnedBetween` in different
    /// months — exercises month partitioning. PascalCase fields match the
    /// documented API response exactly.
    fn fixture_unlocks_two_months() -> Value {
        json!([
            {
                "Date": "2023-08-03 22:41:48",
                "HardcoreMode": 1,
                "AchievementID": 175333,
                "Title": "Solo Adventurer",
                "Description": "Solo defeat Golden Beetles at 2nd block",
                "BadgeName": "228985",
                "Points": 10,
                "TrueRatio": 25,
                "Type": "missable",
                "Author": "Altomar",
                "AuthorULID": "00003EMFWR7XB8SDPEHB3K56ZQ",
                "GameTitle": "Persona 3 Portable",
                "GameIcon": "/Images/065205.png",
                "GameID": 3164,
                "ConsoleName": "PlayStation Portable",
                "CumulScore": 10,
                "BadgeURL": "/Badge/228985.png",
                "GameURL": "/game/3164"
            },
            {
                "Date": "2023-09-15 10:05:00",
                "HardcoreMode": 0,
                "AchievementID": 200001,
                "Title": "First Steps",
                "Description": "Complete the tutorial",
                "BadgeName": "300001",
                "Points": 5,
                "TrueRatio": 5,
                "Type": null,
                "Author": "Devuser",
                "AuthorULID": "00000AAAAAAAAAAAAAAAAAAAAAA",
                "GameTitle": "Super Mario Bros.",
                "GameIcon": "/Images/001001.png",
                "GameID": 1,
                "ConsoleName": "NES",
                "CumulScore": 15,
                "BadgeURL": "/Badge/300001.png",
                "GameURL": "/game/1"
            }
        ])
    }

    /// A single unlock row (for dedup and field-fidelity tests).
    fn fixture_unlocks_one() -> Value {
        json!([
            {
                "Date": "2023-08-03 22:41:48",
                "HardcoreMode": 1,
                "AchievementID": 175333,
                "Title": "Solo Adventurer",
                "Description": "Solo defeat Golden Beetles at 2nd block",
                "BadgeName": "228985",
                "Points": 10,
                "TrueRatio": 25,
                "Type": "missable",
                "Author": "Altomar",
                "AuthorULID": "00003EMFWR7XB8SDPEHB3K56ZQ",
                "GameTitle": "Persona 3 Portable",
                "GameIcon": "/Images/065205.png",
                "GameID": 3164,
                "ConsoleName": "PlayStation Portable",
                "CumulScore": 10,
                "BadgeURL": "/Badge/228985.png",
                "GameURL": "/game/3164"
            }
        ])
    }

    /// An empty response (no new unlocks).
    fn fixture_empty() -> Value {
        json!([])
    }

    /// A progress snapshot response from `API_GetUserCompletionProgress`.
    fn fixture_progress() -> Value {
        json!({
            "Count": 2,
            "Total": 2,
            "Results": [
                {
                    "GameID": 3164,
                    "Title": "Persona 3 Portable",
                    "ImageIcon": "/Images/065205.png",
                    "ConsoleID": 41,
                    "ConsoleName": "PlayStation Portable",
                    "MaxPossible": 58,
                    "NumAwarded": 3,
                    "NumAwardedHardcore": 2,
                    "MostRecentAwardedDate": "2023-08-03T22:41:48+00:00",
                    "HighestAwardKind": null,
                    "HighestAwardDate": null
                },
                {
                    "GameID": 1,
                    "Title": "Super Mario Bros.",
                    "ImageIcon": "/Images/001001.png",
                    "ConsoleID": 7,
                    "ConsoleName": "NES",
                    "MaxPossible": 40,
                    "NumAwarded": 1,
                    "NumAwardedHardcore": 0,
                    "MostRecentAwardedDate": "2023-09-15T10:05:00+00:00",
                    "HighestAwardKind": null,
                    "HighestAwardDate": null
                }
            ]
        })
    }

    // ---------------------------------------------------------------------------
    // Stub client.

    struct StubClient {
        unlock_responses: RefCell<Vec<Value>>,
        progress_responses: RefCell<Vec<Value>>,
    }

    impl StubClient {
        fn new(unlock_responses: Vec<Value>, progress_responses: Vec<Value>) -> Self {
            StubClient {
                unlock_responses: RefCell::new(unlock_responses),
                progress_responses: RefCell::new(progress_responses),
            }
        }
    }

    impl RaApi for StubClient {
        fn achievements_between(
            &self,
            _api_key: &str,
            _from: i64,
            _to: i64,
        ) -> Result<Value, FetchError> {
            let mut q = self.unlock_responses.borrow_mut();
            if q.is_empty() { Ok(json!([])) } else { Ok(q.remove(0)) }
        }

        fn completion_progress(
            &self,
            _api_key: &str,
            _offset: u32,
            _count: u32,
        ) -> Result<Value, FetchError> {
            let mut q = self.progress_responses.borrow_mut();
            if q.is_empty() {
                Ok(json!({ "Count": 0, "Total": 0, "Results": [] }))
            } else {
                Ok(q.remove(0))
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Unit: parsing helpers.

    #[test]
    fn parse_ra_date_valid() {
        // 2023-08-03 22:41:48 UTC = 1691102508
        assert_eq!(parse_ra_date("2023-08-03 22:41:48"), Some(1691102508));
    }

    #[test]
    fn parse_ra_date_invalid_returns_none() {
        assert!(parse_ra_date("").is_none());
        assert!(parse_ra_date("not-a-date").is_none());
        assert!(parse_ra_date("2023-08-03").is_none()); // missing time
    }

    #[test]
    fn month_key_from_date_string() {
        assert_eq!(month_key("2023-08-03 22:41:48"), Some("2023-08".to_string()));
        assert_eq!(month_key("2023-09-15 10:05:00"), Some("2023-09".to_string()));
        assert_eq!(month_key("2023"), None);
    }

    #[test]
    fn unlock_guid_format() {
        let row = json!({
            "Date": "2023-08-03 22:41:48",
            "AchievementID": 175333_u64
        });
        assert_eq!(make_unlock_guid(&row).unwrap(), "175333_1691102508");
    }

    // ---------------------------------------------------------------------------
    // Unit: parse_unlocks.

    #[test]
    fn parse_unlocks_two_months() {
        let (rows, max_epoch) = parse_unlocks(&fixture_unlocks_two_months());
        assert_eq!(rows.len(), 2);

        // August row.
        let r0 = &rows[0];
        assert_eq!(r0.ts, "2023-08");
        assert_eq!(r0.raw["AchievementID"], 175333);
        assert_eq!(r0.raw["HardcoreMode"], 1);
        assert_eq!(r0.raw["GameID"], 3164);
        assert_eq!(r0.raw["ConsoleName"], "PlayStation Portable");
        assert_eq!(r0.raw["guid"], "175333_1691102508");

        // September row.
        let r1 = &rows[1];
        assert_eq!(r1.ts, "2023-09");
        assert_eq!(r1.raw["AchievementID"], 200001);
        assert_eq!(r1.raw["HardcoreMode"], 0);

        // max_epoch from September.
        let epoch_sep = parse_ra_date("2023-09-15 10:05:00").unwrap();
        assert_eq!(max_epoch, Some(epoch_sep));
    }

    #[test]
    fn parse_unlocks_empty_array() {
        let (rows, max_epoch) = parse_unlocks(&fixture_empty());
        assert!(rows.is_empty());
        assert!(max_epoch.is_none());
    }

    #[test]
    fn parse_unlocks_full_fidelity() {
        let (rows, _) = parse_unlocks(&fixture_unlocks_one());
        assert_eq!(rows.len(), 1);
        let r = &rows[0].raw;
        // All documented fields must be preserved.
        assert_eq!(r["Title"], "Solo Adventurer");
        assert_eq!(r["Description"], "Solo defeat Golden Beetles at 2nd block");
        assert_eq!(r["BadgeName"], "228985");
        assert_eq!(r["Points"], 10);
        assert_eq!(r["TrueRatio"], 25);
        assert_eq!(r["Author"], "Altomar");
        assert_eq!(r["AuthorULID"], "00003EMFWR7XB8SDPEHB3K56ZQ");
        assert_eq!(r["BadgeURL"], "/Badge/228985.png");
        assert_eq!(r["GameURL"], "/game/3164");
        assert_eq!(r["GameTitle"], "Persona 3 Portable");
        assert_eq!(r["CumulScore"], 10);
        assert_eq!(r["GameIcon"], "/Images/065205.png");
        assert_eq!(r["Type"], "missable");
    }

    // ---------------------------------------------------------------------------
    // Integration: pull_with.

    #[test]
    fn pull_writes_two_partitions_and_progress() {
        let vault = temp_vault("two-months");
        let stub = StubClient::new(
            vec![fixture_unlocks_two_months()],
            vec![fixture_progress()],
        );
        let out = pull_with(&vault, &stub, "testkey").unwrap();
        assert_eq!(out.counts["unlocks"], 2);
        assert_eq!(out.counts["progress"], 2);

        // Two unlock partitions written.
        let stream = vault.stream(UNLOCKS_DIR, Partition::Month);
        let partitions = stream.partitions().unwrap();
        assert!(partitions.contains(&"2023-08".to_string()), "Aug partition missing");
        assert!(partitions.contains(&"2023-09".to_string()), "Sep partition missing");

        // Progress snapshot has 2 rows.
        let progress: Vec<Value> = vault.read_snapshot(PROGRESS_REL).unwrap_or_default();
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[0]["GameID"], 3164);
        assert_eq!(progress[1]["GameID"], 1);

        // Watermark advanced to the September epoch.
        let state = vault.read_ra_sync();
        let epoch_sep = parse_ra_date("2023-09-15 10:05:00").unwrap();
        assert_eq!(state.watermark_ts, Some(epoch_sep));
        assert!(state.progress_synced.is_some());
        assert!(state.updated.is_some());
    }

    #[test]
    fn pull_deduplicates_on_resync() {
        let vault = temp_vault("dedup");

        let stub1 = StubClient::new(vec![fixture_unlocks_one()], vec![fixture_progress()]);
        let out1 = pull_with(&vault, &stub1, "testkey").unwrap();
        assert_eq!(out1.counts["unlocks"], 1);

        // Second pull returns the same achievement → deduped to 0 new.
        let stub2 = StubClient::new(vec![fixture_unlocks_one()], vec![fixture_progress()]);
        let out2 = pull_with(&vault, &stub2, "testkey").unwrap();
        assert_eq!(out2.counts["unlocks"], 0);

        // Only 1 row on disk.
        let stream = vault.stream(UNLOCKS_DIR, Partition::Month);
        let aug: Vec<Value> = stream.read("2023-08").unwrap();
        assert_eq!(aug.len(), 1);
    }

    #[test]
    fn pull_empty_does_not_advance_watermark() {
        let vault = temp_vault("empty-watermark");
        let stub = StubClient::new(vec![fixture_empty()], vec![fixture_progress()]);
        let out = pull_with(&vault, &stub, "testkey").unwrap();
        assert_eq!(out.counts["unlocks"], 0);
        // Cursor stays None (no max_epoch from empty response).
        let state = vault.read_ra_sync();
        assert!(state.watermark_ts.is_none());
    }

    #[test]
    fn pull_first_sync_sets_watermark_and_timestamps() {
        let vault = temp_vault("first-sync");
        let stub = StubClient::new(
            vec![fixture_unlocks_two_months()],
            vec![fixture_progress()],
        );
        let out = pull_with(&vault, &stub, "testkey").unwrap();
        assert_eq!(out.counts["unlocks"], 2);
        let state = vault.read_ra_sync();
        assert!(state.watermark_ts.is_some());
        assert!(state.progress_synced.is_some());
        assert!(state.updated.is_some());
    }

    // ---------------------------------------------------------------------------
    // Unit: split_credential (username:apikey combined paste).

    #[test]
    fn split_credential_parses_user_and_key() {
        // Normal case: username + 32-char hex key.
        assert_eq!(
            split_credential("  MyUser : aBcDeFgHiJkLmNoPqRsTuVwXyZ123456  "),
            Some(("MyUser".to_string(), "aBcDeFgHiJkLmNoPqRsTuVwXyZ123456".to_string()))
        );
    }

    #[test]
    fn split_credential_rejects_no_colon() {
        assert_eq!(split_credential("onlyusername"), None);
        assert_eq!(split_credential("  "), None);
    }

    #[test]
    fn split_credential_rejects_empty_halves() {
        assert_eq!(split_credential(":onlykey"), None);  // missing username
        assert_eq!(split_credential("onlyuser:"), None); // missing key
        assert_eq!(split_credential(":"), None);         // both missing
    }

    #[test]
    fn split_credential_splits_on_first_colon_only() {
        // Key could theoretically contain a colon in an edge case.
        assert_eq!(
            split_credential("User:key:extra"),
            Some(("User".to_string(), "key:extra".to_string()))
        );
    }

    // ---------------------------------------------------------------------------
    // Integration: connection round-trip via TokenSet.

    #[test]
    fn token_roundtrip_username_in_scope() {
        let vault = temp_vault("token-roundtrip");
        // Simulate what def_connect stores (without the network probe).
        let token = crate::sync::oauth::TokenSet {
            access_token: "testapikey123".to_string(),
            refresh_token: None,
            token_type: None,
            scope: Some("TestPlayer".to_string()),
            expires_at: None,
        };
        vault.save_sync_token(SERVICE, &token).unwrap();

        // Verify pull() reads back the same username and key.
        let loaded = vault.load_sync_token(SERVICE).unwrap().unwrap();
        assert_eq!(loaded.access_token, "testapikey123");
        assert_eq!(loaded.scope.as_deref(), Some("TestPlayer"));
    }
}
