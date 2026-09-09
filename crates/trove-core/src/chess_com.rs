//! Chess.com — online chess game archive with embedded PGN via the
//! official keyless Published-Data API (`api.chess.com/pub/player/…`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/chess-com.md.
//!
//! A **Periodic** cloud pull. `gaming/` is **raw-only** in the taxonomy —
//! there is NO write-time contract. This collector writes full-fidelity raw
//! JSONL and touches no `DOMAINS` struct or spec. Two streams:
//!
//! - **games** — one row per game at
//!   `gaming/chess-com/YYYY-MM.jsonl`, partitioned by the archive month
//!   (the API's native partition — a 1:1 fit). Full fidelity: white/black
//!   player objects (username/rating/result/@id), accuracies (when present),
//!   url, fen, pgn, start_time, end_time, time_control, rules, eco,
//!   tournament, match. PGN is stored verbatim for replay — never parsed at
//!   write time.
//! - **stats** — the latest stats snapshot at
//!   `gaming/chess-com/stats.jsonl`, **rewritten whole** each pull
//!   (current-state — ratings/win-loss by mode).
//!
//! The API returns an array of all games in a given calendar month:
//!   `GET https://api.chess.com/pub/player/{username}/games/{YYYY}/{MM}`
//! The archives list endpoint enumerates available months for backfill:
//!   `GET https://api.chess.com/pub/player/{username}/games/archives`
//! Stats snapshot:
//!   `GET https://api.chess.com/pub/player/{username}/stats`
//!
//! No auth; public data only; sequential requests are unlimited per the
//! published rate-limit notes. A polite 300 ms delay between requests keeps
//! the collector well below any threshold.
//!
//! The username is stored as the connection's single pasted field
//! (TokenPaste), exactly like [`crate::listenbrainz`] and
//! [`crate::boardgamegeek`]: the username rides in the `access_token` slot
//! of a never-expiring [`crate::sync::oauth::TokenSet`] under
//! `.trove/sync/chess-com.json`. The month cursor (last fully-fetched month)
//! lives in a rebuildable, non-secret cursor at
//! `.trove/chess-com-sync.json`.
//!
//! JSON shapes confirmed against the Chess.com Published-Data API
//! documentation (chess.com/news/view/published-data-api): games array
//! wrapper is `{"games": [...]}`, each game has `white`/`black` player
//! objects `{username, rating, result, "@id"}`, optional `accuracies`
//! `{white, black}`, and scalar fields `url`, `fen`, `pgn`, `start_time`
//! (unix timestamp), `end_time` (unix timestamp), `time_control`, `rules`,
//! `eco`, `tournament`, `match`. The stats endpoint returns a JSON object
//! keyed by mode (e.g. `chess_bullet`, `chess_blitz`, `chess_rapid`,
//! `chess_daily`, `tactics`, `puzzle_rush`) — stored verbatim.

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

/// Month-partitioned game stream directory.
const GAMES_DIR: &str = "gaming/chess-com";
/// Stats snapshot file (rewritten whole each pull).
const STATS_REL: &str = "gaming/chess-com/stats.jsonl";
/// Non-secret rebuildable cursor (not under `.trove/sync/` — that's for
/// 0600 secrets). Deleting it re-fetches the whole history on the next sync.
const SYNC_FILE: &str = ".trove/chess-com-sync.json";
/// The service id under `.trove/sync/` where the username is stored.
const SERVICE: &str = "chess-com";

const API_BASE: &str = "https://api.chess.com";
/// Politeness throttle between sequential archive fetches (~3 req/s).
const REQ_INTERVAL: Duration = Duration::from_millis(300);
/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Seconds between syncs. Hourly: games trickle in and the delta is cheap.
pub const CHESS_COM_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(GAMES_DIR))
        .or_else(|| crate::registry::file_mtime(&vault.root().join(STATS_REL)))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let games = out.counts.get("games").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(games > 0, || {
                format!("chess.com synced — {games} games")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "chess.com sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let games = out.counts.get("games").copied().unwrap_or(0);
    let headline = if games == 0 {
        "Chess.com is up to date — no new games".to_string()
    } else {
        format!("Chess.com synced — {games} new games")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "chess-com",
        name: "Chess.com",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls your Chess.com game archive — monthly PGN bundles stored \
             as-is for replay — using the official keyless public API. \
             Stores full game records including moves, ratings, time control, \
             opening (ECO), and accuracy scores when available.",
        domain: "gaming",
        vault_path: "gaming/chess-com/",
        toggleable: true,
        setup: &[
            "Connect with your public Chess.com username on this card.",
            "First sync backfills your whole game history; later syncs fetch only new months.",
        ],
        caveats: "Reads your public Chess.com profile — your games are publicly \
                  accessible via the Published-Data API (no account login needed). \
                  Accuracy scores are a Chess.com proprietary metric and are only \
                  present on games where Chess.com computed them.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(CHESS_COM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("chess-com"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the public username).

fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let client = ChessComClient::new(API_BASE.to_string());
    connect_with(vault, &client, username)
}

fn connect_with(vault: &Vault, client: &impl ChessComApi, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("empty username");
    }
    // Keyless verification: fetch the archives list as a cheap probe.
    // A 404 or an empty response means the user doesn't exist or isn't public.
    match client.archives(username) {
        Ok(_) => {}
        Err(FetchError::NotFound) => {
            bail!(
                "Chess.com could not find user {username:?} — check the spelling \
                 (the profile must be public)"
            )
        }
        Err(_) => {} // transient/other: store anyway, the pull will retry
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

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let username = token.access_token;
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: username,
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
    id: "chess-com",
    display_name: "Chess.com",
    methods: &[ConnectMethod::TokenPaste {
        label: "Chess.com username",
        help: "Enter your public Chess.com username — your game history is read \
               from your public profile (no account login or token needed).",
        placeholder: "e.g. hikaru",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["chess-com"],
    setup: &[
        "Enter your public Chess.com username and connect.",
        "Game history is read from your public profile; no login or token is needed.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    /// 404 / username doesn't exist.
    NotFound,
    /// 429 / 503 — rate limited.
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NotFound => write!(f, "user not found or profile not public"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/503)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The three API endpoints this collector uses. Tests implement this against
/// fixtures; production hits the real API.
trait ChessComApi {
    /// `GET /pub/player/{username}/games/archives` — list of monthly archive URLs.
    fn archives(&self, username: &str) -> Result<ArchivesResp, FetchError>;
    /// `GET /pub/player/{username}/games/{year}/{month}` — all games for a month.
    fn games(&self, username: &str, year: u32, month: u32) -> Result<GamesResp, FetchError>;
    /// `GET /pub/player/{username}/stats` — current ratings/win-loss snapshot.
    fn stats(&self, username: &str) -> Result<Value, FetchError>;
}

/// Thin `ureq` client. Base URL injected for testability.
struct ChessComClient {
    base: String,
}

impl ChessComClient {
    fn new(base: String) -> Self {
        ChessComClient { base }
    }

    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, FetchError> {
        let url = format!("{}{}", self.base, path);
        match ureq::get(&url).timeout(HTTP_TIMEOUT).call() {
            Ok(resp) => resp
                .into_json::<T>()
                .map_err(|e| FetchError::Other(format!("json decode: {e}"))),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
            Err(ureq::Error::Status(429 | 503, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl ChessComApi for ChessComClient {
    fn archives(&self, username: &str) -> Result<ArchivesResp, FetchError> {
        self.get_json(&format!("/pub/player/{username}/games/archives"))
    }

    fn games(&self, username: &str, year: u32, month: u32) -> Result<GamesResp, FetchError> {
        self.get_json(&format!("/pub/player/{username}/games/{year}/{month:02}"))
    }

    fn stats(&self, username: &str) -> Result<Value, FetchError> {
        self.get_json(&format!("/pub/player/{username}/stats"))
    }
}

// ---------------------------------------------------------------------------
// API response shapes — minimal typed wrappers; raw fidelity stored via
// `serde_json::Value` so no API field is dropped.

/// `GET /pub/player/{username}/games/archives`
#[derive(Debug, Deserialize)]
struct ArchivesResp {
    /// Array of URLs like "https://api.chess.com/pub/player/x/games/2023/04"
    archives: Vec<String>,
}

/// `GET /pub/player/{username}/games/{year}/{month}`
#[derive(Debug, Deserialize)]
struct GamesResp {
    games: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The latest month we have fully fetched in "YYYY-MM" format.
    /// On the next sync we re-fetch this month (incremental update for the
    /// current month) and any newer months that have appeared in the archives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_month: Option<String>,
    /// RFC3339 timestamp of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_chess_com_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_chess_com_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — extract the YYYY-MM partition key from an archive URL.

/// Parse a year+month pair from a Chess.com archive URL such as
/// "https://api.chess.com/pub/player/hikaru/games/2023/04". Returns
/// `Some(("2023", "04"))` or `None` for an unrecognised format.
fn parse_archive_url(url: &str) -> Option<(u32, u32)> {
    // The URL always ends with "/{year}/{month}" (zero-padded month).
    let mut parts = url.rsplit('/');
    let month: u32 = parts.next()?.parse().ok()?;
    let year: u32 = parts.next()?.parse().ok()?;
    if year < 2000 || year > 2100 || month < 1 || month > 12 {
        return None;
    }
    Some((year, month))
}

/// Format (year, month) as the vault partition key `YYYY-MM`.
fn month_key(year: u32, month: u32) -> String {
    format!("{year:04}-{month:02}")
}

/// Extract a `guid` from a game: the `url` field is a stable per-game
/// URL like "https://www.chess.com/game/live/12345678". Fall back to the
/// stable `uuid` field if the API ever adds one; if neither exists, build
/// a synthetic key from white+black+end_time (stable for the same game).
fn game_guid(game: &Value) -> Option<String> {
    if let Some(url) = game.get("url").and_then(Value::as_str) {
        if !url.is_empty() {
            return Some(url.to_string());
        }
    }
    if let Some(uuid) = game.get("uuid").and_then(Value::as_str) {
        if !uuid.is_empty() {
            return Some(uuid.to_string());
        }
    }
    // Synthetic stable key from end_time + white + black (a game's identity
    // is determined by its participants and finish time).
    let end_time = game.get("end_time").and_then(Value::as_u64)?;
    let white = game
        .get("white")
        .and_then(|w| w.get("username"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let black = game
        .get("black")
        .and_then(|w| w.get("username"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if white.is_empty() && black.is_empty() {
        return None;
    }
    Some(format!("{end_time}-{white}-{black}"))
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the username, fetch archive list, and sync games + stats.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Chess.com is not connected — add your username in the Integrations tab"
            )
        })?;
    let client = ChessComClient::new(API_BASE.to_string());
    pull_with(vault, &client, &username)
}

fn pull_with(vault: &Vault, client: &impl ChessComApi, username: &str) -> Result<PullOutcome> {
    let mut state = vault.read_chess_com_sync();

    let games_written = sync_games(vault, client, username, &mut state)?;

    thread::sleep(REQ_INTERVAL);

    // Stats snapshot: best-effort — if it fails, keep the games and move on.
    let _ = sync_stats(vault, client, username);

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_chess_com_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{games_written} games"),
        counts: BTreeMap::from([("games", games_written)]),
    })
}

/// Enumerate the available archive months and fetch any that are new or the
/// current (most-recent) month which needs a re-fetch for new games.
fn sync_games(
    vault: &Vault,
    client: &impl ChessComApi,
    username: &str,
    state: &mut SyncState,
) -> Result<u64> {
    let archives_resp = match client.archives(username) {
        Ok(r) => r,
        Err(FetchError::NotFound) => {
            bail!("Chess.com user {username:?} not found — check the username")
        }
        Err(FetchError::RateLimited) => {
            // One retry after a short delay.
            thread::sleep(Duration::from_secs(2));
            match client.archives(username) {
                Ok(r) => r,
                Err(e) => bail!("Chess.com archives fetch failed: {e}"),
            }
        }
        Err(e) => bail!("Chess.com archives fetch failed: {e}"),
    };

    let stream = vault.stream(GAMES_DIR, Partition::Month);

    // Load existing guids from all partitions (dedupe is by game URL/uuid).
    // This is a startup cost but keeps the incremental case efficient: after
    // the first sync the cursor skips all old months.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for row in stream.read::<Value>(&key)? {
            if let Some(guid) = game_guid(&row) {
                seen.insert(guid);
            }
        }
    }

    let mut total_written: u64 = 0;
    let mut latest_month: Option<String> = state.last_month.clone();

    let n = archives_resp.archives.len();
    for (i, url) in archives_resp.archives.iter().enumerate() {
        let (year, month) = match parse_archive_url(url) {
            Some(ym) => ym,
            None => continue,
        };
        let key = month_key(year, month);

        // Skip months we have already fully fetched, EXCEPT:
        //   - the current/last archive month (accumulates all month)
        //   - the previous last_month (the "boundary" month): if a game was
        //     played in month M after the last sync but before M+1 appeared,
        //     the month was still the "current" month at write time and is now
        //     `< newest` — we must re-fetch it once more to capture those late
        //     games. We re-fetch every month >= one-before-newest that falls
        //     >= last_month. The seen-HashSet dedup makes the re-fetch free.
        let is_last = i == n - 1;
        // The month before the newest archive (may be the same as last_month
        // at a boundary rollover). Everything strictly before last_month is
        // already fully closed and can be skipped.
        if let Some(ref lm) = state.last_month {
            if key.as_str() < lm.as_str() {
                // Strictly older than the watermark: fully closed, skip.
                continue;
            }
            // key >= lm: always re-fetch. This covers:
            //   - key == lm: the boundary month that may have late games.
            //   - key > lm: new months never fetched before.
            // The seen-HashSet makes re-fetching an already-complete month
            // effectively free (0 new rows written, no duplicates).
        }

        thread::sleep(REQ_INTERVAL);

        let games_resp = match client.games(username, year, month) {
            Ok(r) => r,
            Err(FetchError::NotFound) => continue, // month has no games
            Err(FetchError::RateLimited) => {
                thread::sleep(Duration::from_secs(2));
                match client.games(username, year, month) {
                    Ok(r) => r,
                    Err(e) => bail!("Chess.com games fetch failed for {key}: {e}"),
                }
            }
            Err(e) => bail!("Chess.com games fetch failed for {key}: {e}"),
        };

        // Write new games into the month partition, deduped by guid.
        let mut new_rows: Vec<RawRow> = Vec::new();
        for game in &games_resp.games {
            let guid = match game_guid(game) {
                Some(g) => g,
                None => continue, // can't dedupe — skip
            };
            if !seen.insert(guid.clone()) {
                continue; // already stored
            }
            new_rows.push(RawRow { month_key: key.clone(), value: game.clone() });
        }

        if !new_rows.is_empty() {
            stream.append(&new_rows, |r| r.month_key.as_str())?;
            total_written += new_rows.len() as u64;
        }

        // Advance the watermark forward-only (string compare is correct for
        // YYYY-MM). Only advance past a month once we have fetched it
        // successfully; re-fetching the current month (is_last) does not
        // advance past it — the next sync will re-fetch it again.
        if !is_last {
            if latest_month.as_deref().is_none_or(|lm| key.as_str() > lm) {
                latest_month = Some(key);
            }
        } else {
            // Mark the current month as the last_month so the next sync
            // knows to re-fetch it (and not skip it as "already done").
            if latest_month.as_deref().is_none_or(|lm| key.as_str() >= lm) {
                latest_month = Some(key);
            }
        }
    }

    state.last_month = latest_month;
    Ok(total_written)
}

/// Fetch and snapshot the player stats. Best-effort — the caller ignores
/// errors so the games stream is always kept even if stats fails.
fn sync_stats(vault: &Vault, client: &impl ChessComApi, username: &str) -> Result<()> {
    let stats = match client.stats(username) {
        Ok(v) => v,
        Err(FetchError::NotFound) => return Ok(()),
        Err(e) => bail!("Chess.com stats fetch failed: {e}"),
    };
    vault.write_snapshot(STATS_REL, &[stats])
}

/// A raw game value carrying the month partition key (only for filing). Only
/// `value` is serialized to disk — flattened, so the line is the raw API
/// object.
#[derive(Serialize)]
struct RawRow {
    #[serde(skip)]
    month_key: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-chess-com-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures synthesized from the confirmed Chess.com PubAPI shapes ---
    // (chess.com/news/view/published-data-api).

    fn archives_resp() -> ArchivesResp {
        ArchivesResp {
            archives: vec![
                "https://api.chess.com/pub/player/testuser/games/2023/01".to_string(),
                "https://api.chess.com/pub/player/testuser/games/2023/02".to_string(),
            ],
        }
    }

    /// One game fixture: the real field names from the PubAPI docs.
    fn game1() -> Value {
        serde_json::json!({
            "url": "https://www.chess.com/game/live/11111111",
            "pgn": "[Event \"Live Chess\"]\n1. e4 e5 2. Nf3 Nc6 *",
            "time_control": "600",
            "end_time": 1672531200_u64,
            "rated": true,
            "accuracies": {"white": 87.5, "black": 82.1},
            "tcn": "mCZRnDZJ",
            "uuid": "abc-123",
            "initial_setup": "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
            "fen": "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1",
            "time_class": "rapid",
            "rules": "chess",
            "eco": "https://www.chess.com/openings/Kings-Pawn-Opening",
            "white": {
                "rating": 1500,
                "result": "win",
                "@id": "https://api.chess.com/pub/player/playerone",
                "username": "PlayerOne"
            },
            "black": {
                "rating": 1480,
                "result": "checkmated",
                "@id": "https://api.chess.com/pub/player/playertwo",
                "username": "PlayerTwo"
            }
        })
    }

    /// A second game with no accuracies (Chess.com doesn't always compute them).
    fn game2() -> Value {
        serde_json::json!({
            "url": "https://www.chess.com/game/live/22222222",
            "pgn": "[Event \"Live Chess\"]\n1. d4 d5 *",
            "time_control": "300",
            "end_time": 1672617600_u64,
            "rated": true,
            "fen": "rnbqkbnr/pppppppp/8/8/3P4/8/PPP1PPPP/RNBQKBNR b KQkq d3 0 1",
            "time_class": "blitz",
            "rules": "chess",
            "white": {
                "rating": 1520,
                "result": "resigned",
                "@id": "https://api.chess.com/pub/player/playertwo",
                "username": "PlayerTwo"
            },
            "black": {
                "rating": 1490,
                "result": "win",
                "@id": "https://api.chess.com/pub/player/playerone",
                "username": "PlayerOne"
            }
        })
    }

    fn stats_resp() -> Value {
        serde_json::json!({
            "chess_rapid": {
                "last": {"date": 1672531200_u64, "rating": 1500, "rd": 45},
                "best": {"date": 1672531200_u64, "rating": 1650, "game": "https://www.chess.com/game/live/12345"},
                "record": {"win": 120, "loss": 85, "draw": 22, "time_per_move": 30, "timeout_percent": 1.2}
            },
            "chess_blitz": {
                "last": {"date": 1672531200_u64, "rating": 1350, "rd": 60},
                "record": {"win": 200, "loss": 180, "draw": 40}
            },
            "tactics": {
                "highest": {"rating": 1800, "date": 1672531200_u64},
                "lowest": {"rating": 1200, "date": 1650000000_u64}
            },
            "puzzle_rush": {
                "best": {"total_attempts": 50, "score": 42}
            }
        })
    }

    // --- Stub client ---

    struct StubClient {
        archives: Option<Result<ArchivesResp, FetchError>>,
        games: BTreeMap<(u32, u32), Result<GamesResp, FetchError>>,
        stats: Option<Result<Value, FetchError>>,
    }

    impl StubClient {
        fn ok(archives: ArchivesResp, month_games: Vec<((u32, u32), Vec<Value>)>) -> Self {
            let mut games = BTreeMap::new();
            for ((y, m), gs) in month_games {
                games.insert((y, m), Ok(GamesResp { games: gs }));
            }
            StubClient {
                archives: Some(Ok(archives)),
                games,
                stats: Some(Ok(stats_resp())),
            }
        }
    }

    impl ChessComApi for StubClient {
        fn archives(&self, _: &str) -> Result<ArchivesResp, FetchError> {
            match &self.archives {
                Some(Ok(r)) => Ok(ArchivesResp { archives: r.archives.clone() }),
                Some(Err(FetchError::NotFound)) => Err(FetchError::NotFound),
                _ => Err(FetchError::Other("stub: no archives".to_string())),
            }
        }

        fn games(&self, _: &str, year: u32, month: u32) -> Result<GamesResp, FetchError> {
            match self.games.get(&(year, month)) {
                Some(Ok(r)) => Ok(GamesResp { games: r.games.clone() }),
                Some(Err(FetchError::NotFound)) => Err(FetchError::NotFound),
                Some(Err(e)) => Err(FetchError::Other(e.to_string())),
                None => Err(FetchError::NotFound),
            }
        }

        fn stats(&self, _: &str) -> Result<Value, FetchError> {
            match &self.stats {
                Some(Ok(v)) => Ok(v.clone()),
                _ => Err(FetchError::Other("stub: no stats".to_string())),
            }
        }
    }

    // --- Parsing ---

    #[test]
    fn parse_archive_url_nominal() {
        let (y, m) =
            parse_archive_url("https://api.chess.com/pub/player/hikaru/games/2023/04").unwrap();
        assert_eq!((y, m), (2023, 4));
    }

    #[test]
    fn parse_archive_url_zero_padded() {
        let (y, m) =
            parse_archive_url("https://api.chess.com/pub/player/hikaru/games/2023/01").unwrap();
        assert_eq!((y, m), (2023, 1));
    }

    #[test]
    fn parse_archive_url_bad_format() {
        assert!(parse_archive_url("not-a-url").is_none());
        assert!(parse_archive_url("https://example.com/foo").is_none());
    }

    #[test]
    fn month_key_zero_pads() {
        assert_eq!(month_key(2023, 4), "2023-04");
        assert_eq!(month_key(2024, 12), "2024-12");
    }

    #[test]
    fn game_guid_prefers_url() {
        let g = game1();
        assert_eq!(game_guid(&g).unwrap(), "https://www.chess.com/game/live/11111111");
    }

    #[test]
    fn game_guid_fallback_to_uuid() {
        let mut g = game1();
        g.as_object_mut().unwrap().remove("url");
        let guid = game_guid(&g).unwrap();
        assert_eq!(guid, "abc-123");
    }

    #[test]
    fn game_guid_synthetic_key() {
        let g = serde_json::json!({
            "end_time": 1672531200_u64,
            "white": {"username": "Alice"},
            "black": {"username": "Bob"}
        });
        let guid = game_guid(&g).unwrap();
        assert!(guid.contains("Alice"));
        assert!(guid.contains("Bob"));
        assert!(guid.starts_with("1672531200-"));
    }

    // --- Pull logic ---

    #[test]
    fn pull_writes_games_and_advances_cursor() {
        let vault = temp_vault("pull_basic");
        let client = StubClient::ok(
            archives_resp(),
            vec![
                ((2023, 1), vec![game1()]),
                ((2023, 2), vec![game2()]),
            ],
        );
        let mut state = SyncState::default();
        let written = sync_games(&vault, &client, "testuser", &mut state).unwrap();
        assert_eq!(written, 2);
        // Cursor advances to the last month.
        assert_eq!(state.last_month.as_deref(), Some("2023-02"));

        // Games are partitioned correctly.
        let stream = vault.stream(GAMES_DIR, Partition::Month);
        let jan: Vec<Value> = stream.read("2023-01").unwrap();
        let feb: Vec<Value> = stream.read("2023-02").unwrap();
        assert_eq!(jan.len(), 1, "one game in January");
        assert_eq!(feb.len(), 1, "one game in February");

        // Full fidelity: PGN and player objects preserved.
        assert!(jan[0]["pgn"].as_str().unwrap().contains("1. e4"));
        assert_eq!(jan[0]["white"]["username"], "PlayerOne");
        assert_eq!(jan[0]["accuracies"]["white"], 87.5_f64);
        // game2 has no accuracies — field absent.
        assert!(feb[0].get("accuracies").is_none());
    }

    #[test]
    fn pull_dedupes_on_re_run() {
        let vault = temp_vault("pull_dedupe");
        let client = StubClient::ok(
            archives_resp(),
            vec![
                ((2023, 1), vec![game1()]),
                ((2023, 2), vec![game2()]),
            ],
        );
        // First run.
        let mut state = SyncState::default();
        let first = sync_games(&vault, &client, "testuser", &mut state).unwrap();
        assert_eq!(first, 2);

        // Second run with the same data — nothing new.
        let second = sync_games(&vault, &client, "testuser", &mut state).unwrap();
        assert_eq!(second, 0, "no new games on re-run");

        let stream = vault.stream(GAMES_DIR, Partition::Month);
        let jan: Vec<Value> = stream.read("2023-01").unwrap();
        assert_eq!(jan.len(), 1, "still only one game — no duplicate");
    }

    #[test]
    fn pull_skips_completed_months() {
        let vault = temp_vault("pull_cursor_skip");
        // Archives: Jan + Feb; cursor already has Jan done.
        let mut state = SyncState { last_month: Some("2023-01".to_string()), updated: None };

        let client = StubClient::ok(
            archives_resp(),
            vec![
                // Jan already done — would write a duplicate if fetched.
                ((2023, 1), vec![game1()]),
                ((2023, 2), vec![game2()]),
            ],
        );

        // Pre-populate Jan so we can detect if it gets duplicated.
        {
            let stream = vault.stream(GAMES_DIR, Partition::Month);
            let row = RawRow { month_key: "2023-01".to_string(), value: game1() };
            stream.append(&[row], |r| r.month_key.as_str()).unwrap();
        }

        let written = sync_games(&vault, &client, "testuser", &mut state).unwrap();
        // Jan == last_month so it's re-fetched (boundary re-fetch), but
        // game1 is already in the seen-set so 0 new rows from Jan.
        // Feb is the last archive and is new: game2 is written.
        assert_eq!(written, 1, "only Feb game is new");

        let stream = vault.stream(GAMES_DIR, Partition::Month);
        let jan: Vec<Value> = stream.read("2023-01").unwrap();
        assert_eq!(jan.len(), 1, "Jan still has exactly one game (no duplicate via seen-set)");
    }

    /// Regression test for the month-rollover boundary defect: games played in
    /// the last-synced month M after that sync are captured when M+1 appears.
    ///
    /// SYNC1: archives = [2023-06], game A written, cursor -> 2023-06.
    /// Between syncs: game B is played in June (month still open).
    /// SYNC2: archives = [2023-06, 2023-07]. The old code skipped 2023-06
    /// (key == last_month && !is_last) so game B was lost forever.
    /// The fix: always re-fetch >= last_month; seen-set prevents duplicates.
    #[test]
    fn pull_recaptures_late_boundary_games_on_month_rollover() {
        let vault = temp_vault("boundary_rollover");

        // SYNC1: only June in archives, game A played.
        let archives_june_only = ArchivesResp {
            archives: vec!["https://api.chess.com/pub/player/testuser/games/2023/06".to_string()],
        };
        let game_a = serde_json::json!({
            "url": "https://www.chess.com/game/live/9990001",
            "pgn": "[Event \"Live Chess\"]\n1. e4 e5 *",
            "time_control": "600",
            "end_time": 1685577600_u64,
            "rated": true,
            "fen": "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1",
            "time_class": "rapid",
            "rules": "chess",
            "white": {"rating": 1500, "result": "win",
                      "@id": "https://api.chess.com/pub/player/alice", "username": "Alice"},
            "black": {"rating": 1480, "result": "checkmated",
                      "@id": "https://api.chess.com/pub/player/bob", "username": "Bob"}
        });
        let client1 = StubClient::ok(
            archives_june_only,
            vec![((2023, 6), vec![game_a.clone()])],
        );
        let mut state = SyncState::default();
        let w1 = sync_games(&vault, &client1, "testuser", &mut state).unwrap();
        assert_eq!(w1, 1, "SYNC1: game A written");
        assert_eq!(state.last_month.as_deref(), Some("2023-06"));

        // Between syncs: game B played in June (month was still open).
        // SYNC2: June now has both A+B; July has appeared.
        let game_b = serde_json::json!({
            "url": "https://www.chess.com/game/live/9990002",
            "pgn": "[Event \"Live Chess\"]\n1. d4 d5 *",
            "time_control": "300",
            "end_time": 1685664000_u64,
            "rated": true,
            "fen": "rnbqkbnr/pppppppp/8/8/3P4/8/PPP1PPPP/RNBQKBNR b KQkq d3 0 1",
            "time_class": "blitz",
            "rules": "chess",
            "white": {"rating": 1510, "result": "resigned",
                      "@id": "https://api.chess.com/pub/player/bob", "username": "Bob"},
            "black": {"rating": 1490, "result": "win",
                      "@id": "https://api.chess.com/pub/player/alice", "username": "Alice"}
        });
        let game_july = serde_json::json!({
            "url": "https://www.chess.com/game/live/9990003",
            "pgn": "[Event \"Live Chess\"]\n1. c4 e5 *",
            "time_control": "600",
            "end_time": 1688256000_u64,
            "rated": true,
            "fen": "rnbqkbnr/pppp1ppp/8/4p3/2P5/8/PP1PPPPP/RNBQKBNR w KQkq e6 0 2",
            "time_class": "rapid",
            "rules": "chess",
            "white": {"rating": 1520, "result": "win",
                      "@id": "https://api.chess.com/pub/player/alice", "username": "Alice"},
            "black": {"rating": 1500, "result": "checkmated",
                      "@id": "https://api.chess.com/pub/player/bob", "username": "Bob"}
        });
        let archives_june_july = ArchivesResp {
            archives: vec![
                "https://api.chess.com/pub/player/testuser/games/2023/06".to_string(),
                "https://api.chess.com/pub/player/testuser/games/2023/07".to_string(),
            ],
        };
        let client2 = StubClient::ok(
            archives_june_july,
            vec![
                ((2023, 6), vec![game_a.clone(), game_b.clone()]),
                ((2023, 7), vec![game_july.clone()]),
            ],
        );
        let w2 = sync_games(&vault, &client2, "testuser", &mut state).unwrap();
        // game_a is already seen (deduped); game_b is new in June; game_july is new.
        assert_eq!(w2, 2, "SYNC2: game B (late-June) + July game written");

        let stream = vault.stream(GAMES_DIR, Partition::Month);
        let june: Vec<Value> = stream.read("2023-06").unwrap();
        assert_eq!(june.len(), 2, "June has both game A and late game B");
        let july: Vec<Value> = stream.read("2023-07").unwrap();
        assert_eq!(july.len(), 1, "July has the July game");
    }

    #[test]
    fn stats_snapshot_written() {
        let vault = temp_vault("stats_snapshot");
        let client = StubClient::ok(archives_resp(), vec![]);
        sync_stats(&vault, &client, "testuser").unwrap();
        let rows: Vec<Value> = vault.read_snapshot(STATS_REL).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0]["chess_rapid"]["record"]["win"].as_u64().is_some());
        assert!(rows[0]["tactics"]["highest"]["rating"].as_u64().is_some());
    }

    #[test]
    fn connect_rejects_empty_username() {
        let vault = temp_vault("connect_empty");
        let client = StubClient::ok(archives_resp(), vec![]);
        let r = connect_with(&vault, &client, "  ");
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("empty username"));
    }

    #[test]
    fn connect_stores_username_on_success() {
        let vault = temp_vault("connect_store");
        let client = StubClient::ok(archives_resp(), vec![]);
        connect_with(&vault, &client, "hikaru").unwrap();
        let token = vault.load_sync_token(SERVICE).unwrap().unwrap();
        assert_eq!(token.access_token, "hikaru");
    }

    #[test]
    fn connect_rejects_not_found_user() {
        let vault = temp_vault("connect_not_found");
        let client = StubClient {
            archives: Some(Err(FetchError::NotFound)),
            games: BTreeMap::new(),
            stats: None,
        };
        let r = connect_with(&vault, &client, "nobody");
        assert!(r.is_err());
        // The error says "could not find user" when the API returns 404.
        assert!(r.unwrap_err().to_string().contains("could not find user"));
    }

    #[test]
    fn full_pull_with_roundtrip() {
        let vault = temp_vault("full_pull");
        // Store a username.
        let token = crate::sync::oauth::TokenSet {
            access_token: "testuser".to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        };
        vault.save_sync_token(SERVICE, &token).unwrap();

        let client = StubClient::ok(
            archives_resp(),
            vec![
                ((2023, 1), vec![game1()]),
                ((2023, 2), vec![game2()]),
            ],
        );
        let out = pull_with(&vault, &client, "testuser").unwrap();
        assert_eq!(*out.counts.get("games").unwrap(), 2);

        // Cursor written.
        let state = vault.read_chess_com_sync();
        assert_eq!(state.last_month.as_deref(), Some("2023-02"));
        assert!(state.updated.is_some());
    }
}
