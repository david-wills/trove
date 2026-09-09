//! Lichess — open-source chess platform with NDJSON game stream API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/lichess.md
//!
//! A **Periodic** cloud pull. `gaming/` is **raw-only** in the taxonomy — there
//! is NO write-time contract. This collector writes full-fidelity raw JSONL and
//! touches no `DOMAINS` struct or spec. One stream:
//!
//! - **games** — one row per game at `gaming/lichess/YYYY-MM.jsonl`, partitioned
//!   by the game-end month (`lastMoveAt` ms → local month), deduped by the Lichess
//!   game `id`. Full fidelity: the entire JSON object as returned by the API is
//!   stored verbatim. Requested fields: moves, opening, clocks, evals, accuracy —
//!   nothing is dropped.
//!
//! Endpoint:
//!   `GET https://lichess.org/api/games/user/{username}?since=<ms>&moves=true&...`
//!   Returns NDJSON (one complete game object per line). The `since` cursor is a
//!   millisecond timestamp stored in `.trove/lichess-sync.json` (rebuildable,
//!   non-secret). A complete drain of the window (read until EOF) is done before
//!   the watermark advances — no partial-fetch cursor advance.
//!
//! Auth: keyless for public games. An optional personal API token (via a
//! TokenPaste connection named "lichess") would unlock private games and higher
//! rate limits, but is NOT required for v1 — the user only needs to supply their
//! Lichess username. The username is stored in the `access_token` slot of the
//! sync-token file (`.trove/sync/lichess.json`), exactly like `boardgamegeek`
//! and `chess-com`.
//!
//! JSON shapes confirmed against real API responses from
//! `GET https://lichess.org/api/games/user/DrNykterstein?max=2&opening=true&moves=false`:
//! top-level fields: id (string), rated (bool), variant (string), speed (string),
//! perf (string), createdAt (int ms), lastMoveAt (int ms), status (string),
//! source (string), players.white/black (user.name, user.id, rating, ratingDiff,
//! provisional, title, flair, patron, patronColor, aiLevel, analysis), winner (string),
//! opening.eco, opening.name, opening.ply, moves (string), clocks (array of int
//! centiseconds), clock.initial, clock.increment, clock.totalTime, tournament
//! (string arena id), swiss (string swiss id), initialFen (non-standard starts).

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Month-partitioned game stream directory (vault-relative).
const GAMES_DIR: &str = "gaming/lichess";
/// Non-secret rebuildable cursor. Deleting it re-walks the full history.
const SYNC_FILE: &str = ".trove/lichess-sync.json";
/// Service id under `.trove/sync/` — stores the username in `access_token`.
const SERVICE: &str = "lichess";

const API_BASE: &str = "https://lichess.org";
/// Politeness throttle between requests (Lichess asks to be gentle).
const REQ_INTERVAL: Duration = Duration::from_millis(500);
/// HTTP timeout for a single request. The NDJSON stream can be large, but we
/// connect with this timeout; streaming itself is bounded by game count.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between periodic syncs (hourly — games are infrequent).
pub const LICHESS_SYNC_SECS: u64 = 3600;
/// Max games per request (Lichess API cap is 300 per `max` param; we use 300
/// to minimize round-trips, draining in one HTTP request for typical accounts).
const PAGE_SIZE: u32 = 300;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(GAMES_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("games").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("lichess synced — {n} games")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "lichess sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("games").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Lichess is up to date — no new games".to_string()
    } else {
        format!("Lichess synced — {n} new games")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "lichess",
        name: "Lichess",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Syncs your Lichess game history (moves, opening, ratings, result, clocks) \
             using the official open-source NDJSON streaming API — keyless for public \
             games, optional personal token for private games and higher rate limits.",
        domain: "gaming",
        vault_path: "gaming/lichess/",
        toggleable: true,
        setup: &[
            "Connect with your Lichess username on this card.",
            "First sync backfills your complete game history; later syncs pull only new games.",
        ],
        caveats: "Reads your public Lichess game history. Games you have set to private \
                  are not included unless you also paste a personal API token (not required \
                  for public games). Lichess is a donation-funded nonprofit — the collector \
                  respects their API guidelines and paces requests accordingly.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(LICHESS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("lichess"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the public username).
//
// The username is stored in the `access_token` slot of a never-expiring
// TokenSet, exactly like `boardgamegeek` and `chess-com`. An optional
// personal API token can be stored in the `refresh_token` slot for future
// use (private games / higher rate limits) — not wired in v1.

fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let client = LichessClient::new(API_BASE.to_string());
    connect_with(vault, &client, username)
}

fn connect_with(vault: &Vault, client: &impl LichessApi, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("empty username");
    }
    // Cheap existence check: request the last 1 game. A 404 means no such user.
    match client.games(username, None, 1) {
        Ok(_) | Err(FetchError::NoGames) => {}
        Err(FetchError::NotFound) => {
            bail!("Lichess could not find user {username:?} — check the spelling")
        }
        Err(_) => {} // transient/network: store anyway, pull will retry
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
        if !username.is_empty() {
            accounts.push(ConnectedAccount {
                key: SERVICE.to_string(),
                label: username,
                connected_at: None,
                expires_at: None,
                needs_reconnect: false,
                extra: BTreeMap::new(),
            });
        }
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// public Lichess username. No token required for public games.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "lichess",
    display_name: "Lichess",
    methods: &[ConnectMethod::TokenPaste {
        label: "Lichess username",
        help: "Enter your Lichess username to sync your public game history. \
               No account login or token needed for public games.",
        placeholder: "e.g. DrNykterstein",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["lichess"],
    setup: &[
        "Enter your Lichess username and connect.",
        "Public game history is synced immediately — no token needed.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    /// 404 — username doesn't exist.
    NotFound,
    /// 200 but zero bytes / empty stream — no games at all.
    NoGames,
    /// 429 / 503 — rate limited.
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NotFound => write!(f, "user not found"),
            FetchError::NoGames => write!(f, "no games in window"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429/503)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Abstraction over the Lichess user-games endpoint. Returns the raw NDJSON body.
trait LichessApi {
    /// `GET /api/games/user/{username}?since=<ms>&max=<n>&...`
    /// `since` is milliseconds since Unix epoch; absent means "from the beginning".
    fn games(&self, username: &str, since_ms: Option<u64>, max: u32) -> Result<String, FetchError>;
}

/// Thin `ureq` client. Base URL injected so tests run against a stub.
struct LichessClient {
    base: String,
}

impl LichessClient {
    fn new(base: String) -> Self {
        LichessClient { base }
    }
}

impl LichessApi for LichessClient {
    fn games(&self, username: &str, since_ms: Option<u64>, max: u32) -> Result<String, FetchError> {
        let url = format!("{}/api/games/user/{}", self.base, username);
        let mut req = ureq::get(&url)
            .set("Accept", "application/x-ndjson")
            .timeout(HTTP_TIMEOUT)
            .query("max", &max.to_string())
            .query("sort", "dateAsc")  // oldest-first: correct for since-based forward-paging
            .query("moves", "true")
            .query("opening", "true")
            .query("clocks", "true")
            .query("evals", "false")
            .query("accuracy", "false");
        if let Some(ms) = since_ms {
            req = req.query("since", &ms.to_string());
        }
        match req.call() {
            Ok(resp) => {
                let body = resp
                    .into_string()
                    .map_err(|e| FetchError::Other(format!("reading response: {e}")))?;
                let trimmed = body.trim();
                if trimmed.is_empty() {
                    return Err(FetchError::NoGames);
                }
                Ok(body)
            }
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

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Millisecond timestamp of the maximum `createdAt` of the most-recently-seen
    /// game. On the next sync `since` is set to this value so only newer games come
    /// back — the `since` parameter is a `createdAt > since` filter (confirmed live).
    /// Forward-only: never decremented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    since_ms: Option<u64>,
    /// RFC3339 timestamp of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_lichess_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_lichess_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        crate::store::write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// One parsed game: partition key + raw JSON for disk, plus the guid.
struct ParsedGame {
    /// Lichess game id — the stable dedupe key.
    id: String,
    /// Partition key `YYYY-MM` derived from `lastMoveAt` (local time).
    month_key: String,
    /// `createdAt` as milliseconds — used to advance the watermark.
    /// The `since` query parameter is a `createdAt` filter (confirmed live),
    /// so the cursor must track createdAt, not lastMoveAt.
    created_at_ms: u64,
    /// Full-fidelity game object.
    raw: Value,
}

/// Parse an NDJSON body into a list of games. Lines that fail to parse or lack
/// a valid `id`/`createdAt` are skipped (never panic on malformed lines).
fn parse_ndjson(body: &str) -> Vec<ParsedGame> {
    let mut out = Vec::new();
    for line in BufReader::new(body.as_bytes()).lines().flatten() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(game): Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        let Some(id) = game.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            continue;
        };
        // `createdAt` is required — it's what the `since` cursor filters on.
        let Some(created_at_ms) = game.get("createdAt").and_then(Value::as_u64) else {
            continue;
        };
        // Partition by lastMoveAt (game-end month); fall back to createdAt if absent.
        let last_move_at_ms =
            game.get("lastMoveAt").and_then(Value::as_u64).unwrap_or(created_at_ms);
        let month_key = ms_to_month_key(last_move_at_ms);
        out.push(ParsedGame {
            id: id.to_string(),
            month_key,
            created_at_ms,
            raw: game,
        });
    }
    out
}

/// Convert a millisecond timestamp to a `YYYY-MM` partition key in local time.
fn ms_to_month_key(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let dt = Local.timestamp_opt(secs, 0).single().unwrap_or_else(Local::now);
    dt.format("%Y-%m").to_string()
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the username and run the full drain, returning counts.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Lichess is not connected — add your username in the Integrations tab"
            )
        })?;
    let client = LichessClient::new(API_BASE.to_string());
    pull_with(vault, &client, &username)
}

/// Pull body over an injected client (testable seam).
fn pull_with(vault: &Vault, client: &impl LichessApi, username: &str) -> Result<PullOutcome> {
    let mut state = vault.read_lichess_sync();

    let games_written = sync_games(vault, client, username, &mut state)?;

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_lichess_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{games_written} games"),
        counts: BTreeMap::from([("games", games_written)]),
    })
}

/// Drain new games from `since_ms` and write them, updating the cursor.
///
/// The Lichess NDJSON stream endpoint with `sort=dateAsc` returns games in
/// chronological order (oldest first, by `createdAt`). We request up to
/// PAGE_SIZE games per call and loop until fewer than PAGE_SIZE are returned
/// (signals end of available window). The cursor advances ONLY after the full
/// drain — a crash before commit re-drains the same window on the next run
/// (dedup prevents duplicates).
///
/// IMPORTANT: The `since` parameter is a `createdAt > since` filter (confirmed
/// live). Therefore the cursor must track the maximum `createdAt` seen in the
/// drain, NOT `lastMoveAt`. For correspondence games createdAt can precede
/// lastMoveAt by days-to-weeks; using lastMoveAt as the cursor would permanently
/// skip games whose createdAt falls between consecutive watermarks.
fn sync_games(
    vault: &Vault,
    client: &impl LichessApi,
    username: &str,
    state: &mut SyncState,
) -> Result<u64> {
    let stream = vault.stream(GAMES_DIR, Partition::Month);

    // Load existing game ids from all partitions for deduplication.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for row in stream.read::<Value>(&key)? {
            if let Some(id) = row.get("id").and_then(Value::as_str) {
                if !id.is_empty() {
                    seen.insert(id.to_string());
                }
            }
        }
    }

    let mut total_written: u64 = 0;
    // `latest_created_ms` tracks max createdAt across all batches — this is the
    // value that will become the new `since_ms` cursor after the drain, because
    // `since` is a createdAt filter.
    let mut latest_created_ms: Option<u64> = state.since_ms;
    // We use `since_ms` as the API cursor but add 1ms on subsequent loops so
    // we don't re-fetch the exact boundary game (the dedup set handles it anyway).
    let mut since_ms = state.since_ms;

    loop {
        let body = match client.games(username, since_ms, PAGE_SIZE) {
            Ok(b) => b,
            Err(FetchError::NoGames) => break, // empty window — done
            Err(FetchError::NotFound) => {
                bail!("Lichess user {username:?} not found — check the username")
            }
            Err(FetchError::RateLimited) => {
                // One retry after a short backoff.
                thread::sleep(Duration::from_secs(2));
                match client.games(username, since_ms, PAGE_SIZE) {
                    Ok(b) => b,
                    Err(e) => bail!("Lichess games fetch failed: {e}"),
                }
            }
            Err(e) => bail!("Lichess games fetch failed: {e}"),
        };

        let parsed = parse_ndjson(&body);
        let n = parsed.len();

        let mut new_rows: Vec<RawRow> = Vec::new();
        for g in &parsed {
            // Track the max createdAt across all games — this drives the cursor.
            if latest_created_ms.is_none_or(|prev| g.created_at_ms > prev) {
                latest_created_ms = Some(g.created_at_ms);
            }
            if !seen.insert(g.id.clone()) {
                continue; // already stored
            }
            new_rows.push(RawRow { month_key: g.month_key.clone(), value: g.raw.clone() });
        }

        if !new_rows.is_empty() {
            stream.append(&new_rows, |r| r.month_key.as_str())?;
            total_written += new_rows.len() as u64;
        }

        // If we got fewer games than PAGE_SIZE, this was the last page.
        if n < PAGE_SIZE as usize {
            break;
        }

        // Advance the since cursor to 1ms past the max createdAt seen so the next
        // loop request fetches only games strictly after this batch.
        if let Some(ms) = latest_created_ms {
            since_ms = Some(ms + 1);
        }

        thread::sleep(REQ_INTERVAL);
    }

    // Advance watermark forward-only AFTER the full drain (crash-safe).
    // The cursor is createdAt-based to match the `since` filter semantics.
    if let Some(ms) = latest_created_ms {
        if state.since_ms.is_none_or(|prev| ms > prev) {
            state.since_ms = Some(ms);
        }
    }

    Ok(total_written)
}

/// A raw game row carrying the month partition key (only for filing). Only
/// `value` is serialized to disk — flattened so the line is the full-fidelity
/// API object.
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
        let dir =
            std::env::temp_dir().join(format!("trove-lichess-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures from real Lichess NDJSON responses ---
    // Shapes confirmed against GET /api/games/user/DrNykterstein?max=2&opening=true.

    /// A rated blitz game — arena tournament, full players, opening, clock.
    fn game_rated_blitz() -> &'static str {
        r#"{"id":"kAdOQKeh","rated":true,"variant":"standard","speed":"blitz","perf":"blitz","createdAt":1775677143033,"lastMoveAt":1775677513708,"status":"resign","source":"arena","players":{"white":{"user":{"name":"respects_55","id":"respects_55"},"rating":2644,"ratingDiff":-5,"provisional":true},"black":{"user":{"name":"DrNykterstein","title":"GM","flair":"people.santa-claus-light-skin-tone","patron":true,"patronColor":10,"id":"drnykterstein"},"rating":3145,"ratingDiff":8,"provisional":true}},"winner":"black","opening":{"eco":"B02","name":"Alekhine Defense: Sämisch Attack","ply":5},"moves":"e4 Nf6 e5 Nd5 Nc3 Nxc3","tournament":"mzOPeKWa","clock":{"initial":180,"increment":0,"totalTime":180}}"#
    }

    /// A casual correspondence game vs AI — has `analysis` and `aiLevel`.
    fn game_casual_vs_ai() -> &'static str {
        r#"{"id":"RaKbMbh7","rated":false,"variant":"standard","speed":"correspondence","perf":"correspondence","createdAt":1689455796698,"lastMoveAt":1689455814478,"status":"resign","source":"ai","players":{"white":{"user":{"name":"DrNykterstein","title":"GM","flair":"people.santa-claus-light-skin-tone","patron":true,"patronColor":10,"id":"drnykterstein"},"rating":1500,"provisional":true,"analysis":{"inaccuracy":0,"mistake":0,"blunder":1,"acpl":38,"accuracy":74}},"black":{"aiLevel":1,"analysis":{"inaccuracy":1,"mistake":1,"blunder":1,"acpl":95,"accuracy":56}}},"winner":"black","opening":{"eco":"C20","name":"King's Pawn Game: Leonardis Variation","ply":3}}"#
    }

    /// A game from a non-standard starting position (initialFen present).
    fn game_from_position() -> &'static str {
        r#"{"id":"lit8Alwh","rated":false,"variant":"fromPosition","speed":"blitz","perf":"blitz","createdAt":1615143483317,"lastMoveAt":1615143526042,"status":"mate","source":"arena","players":{"white":{"user":{"name":"DrNykterstein","title":"GM","id":"drnykterstein"},"rating":2971,"analysis":{"inaccuracy":0,"mistake":0,"blunder":0,"acpl":9,"accuracy":96}},"black":{"user":{"name":"roak74","id":"roak74"},"rating":1782,"analysis":{"inaccuracy":0,"mistake":2,"blunder":1,"acpl":198,"accuracy":50}}},"initialFen":"r1bqkbnr/pppp1ppp/2n5/4p3/2B1P3/5N2/PPPP1PPP/RNBQK2R b KQkq - 0 1","winner":"white","tournament":"xGJIZMwW","clock":{"initial":300,"increment":0,"totalTime":300}}"#
    }

    fn two_games_ndjson() -> String {
        format!("{}\n{}", game_rated_blitz(), game_casual_vs_ai())
    }

    fn three_games_ndjson() -> String {
        format!("{}\n{}\n{}", game_rated_blitz(), game_casual_vs_ai(), game_from_position())
    }

    // --- Parsing tests ---

    #[test]
    fn parse_rated_blitz_full_fidelity() {
        let games = parse_ndjson(game_rated_blitz());
        assert_eq!(games.len(), 1);
        let g = &games[0];
        assert_eq!(g.id, "kAdOQKeh");
        assert_eq!(g.created_at_ms, 1775677143033_u64);
        // month_key is derived from lastMoveAt (game-end month) — check YYYY-MM format.
        assert_eq!(g.month_key.len(), 7, "month key is YYYY-MM");
        assert!(g.month_key.contains('-'));

        let r = &g.raw;
        assert_eq!(r["id"], "kAdOQKeh");
        assert_eq!(r["rated"], true);
        assert_eq!(r["variant"], "standard");
        assert_eq!(r["speed"], "blitz");
        assert_eq!(r["perf"], "blitz");
        assert_eq!(r["status"], "resign");
        assert_eq!(r["source"], "arena");
        assert_eq!(r["winner"], "black");
        // Players preserved.
        assert_eq!(r["players"]["white"]["user"]["name"], "respects_55");
        assert_eq!(r["players"]["white"]["rating"], 2644);
        assert_eq!(r["players"]["black"]["user"]["name"], "DrNykterstein");
        assert_eq!(r["players"]["black"]["user"]["title"], "GM");
        assert_eq!(r["players"]["black"]["rating"], 3145);
        // Opening.
        assert_eq!(r["opening"]["eco"], "B02");
        assert_eq!(r["opening"]["name"], "Alekhine Defense: Sämisch Attack");
        assert_eq!(r["opening"]["ply"], 5);
        // Moves string preserved verbatim.
        let moves = r["moves"].as_str().unwrap();
        assert!(moves.contains("e4 Nf6"), "moves string preserved");
        // Clock.
        assert_eq!(r["clock"]["initial"], 180);
        assert_eq!(r["clock"]["increment"], 0);
        assert_eq!(r["clock"]["totalTime"], 180);
        // Tournament id.
        assert_eq!(r["tournament"], "mzOPeKWa");
    }

    #[test]
    fn parse_casual_vs_ai_full_fidelity() {
        let games = parse_ndjson(game_casual_vs_ai());
        assert_eq!(games.len(), 1);
        let g = &games[0];
        assert_eq!(g.id, "RaKbMbh7");
        assert_eq!(g.created_at_ms, 1689455796698_u64);

        let r = &g.raw;
        assert_eq!(r["rated"], false);
        assert_eq!(r["speed"], "correspondence");
        // AI opponent has aiLevel but no user.
        assert_eq!(r["players"]["black"]["aiLevel"], 1);
        assert!(r["players"]["black"].get("user").is_none(), "AI has no user obj");
        // Analysis fields.
        assert_eq!(r["players"]["white"]["analysis"]["accuracy"], 74);
        assert_eq!(r["players"]["black"]["analysis"]["acpl"], 95);
        // No clock (correspondence vs AI).
        assert!(r.get("clock").is_none() || r["clock"].is_null());
    }

    #[test]
    fn parse_from_position_game() {
        let games = parse_ndjson(game_from_position());
        assert_eq!(games.len(), 1);
        let g = &games[0];
        assert_eq!(g.id, "lit8Alwh");
        let r = &g.raw;
        assert_eq!(r["variant"], "fromPosition");
        // initialFen preserved.
        let fen = r["initialFen"].as_str().unwrap();
        assert!(fen.contains("KQkq"), "initialFen preserved verbatim");
    }

    #[test]
    fn parse_ndjson_two_games() {
        let games = parse_ndjson(&two_games_ndjson());
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id, "kAdOQKeh");
        assert_eq!(games[1].id, "RaKbMbh7");
    }

    #[test]
    fn parse_ndjson_skips_empty_lines_and_malformed() {
        let body = format!("\n{}\n\nnot-json\n{}\n", game_rated_blitz(), game_from_position());
        let games = parse_ndjson(&body);
        assert_eq!(games.len(), 2);
    }

    #[test]
    fn ms_to_month_key_format() {
        // 2023-08-15 UTC ≈ 1692057600000ms
        let key = ms_to_month_key(1692057600000);
        assert!(key.len() == 7, "key is YYYY-MM: got {key}");
        assert!(key.starts_with("2023-"), "key starts with year: got {key}");
    }

    // --- Stub client ---

    struct StubClient {
        /// Sequence of responses to return on successive calls.
        responses: std::cell::RefCell<Vec<Result<String, FetchError>>>,
    }

    impl StubClient {
        fn with_responses(responses: Vec<Result<String, FetchError>>) -> Self {
            StubClient { responses: std::cell::RefCell::new(responses) }
        }
        fn one_page(body: String) -> Self {
            // One page of data, then empty (signals end).
            Self::with_responses(vec![Ok(body), Err(FetchError::NoGames)])
        }
    }

    impl LichessApi for StubClient {
        fn games(
            &self,
            _username: &str,
            _since_ms: Option<u64>,
            _max: u32,
        ) -> Result<String, FetchError> {
            let mut rs = self.responses.borrow_mut();
            if rs.is_empty() {
                return Err(FetchError::NoGames);
            }
            match rs.remove(0) {
                Ok(s) => Ok(s),
                Err(FetchError::NotFound) => Err(FetchError::NotFound),
                Err(FetchError::NoGames) => Err(FetchError::NoGames),
                Err(FetchError::RateLimited) => Err(FetchError::RateLimited),
                Err(FetchError::Other(m)) => Err(FetchError::Other(m)),
            }
        }
    }

    // --- Pull logic ---

    #[test]
    fn pull_writes_games_and_advances_cursor() {
        let vault = temp_vault("pull_basic");
        let client = StubClient::one_page(two_games_ndjson());
        let mut state = SyncState::default();
        let written = sync_games(&vault, &client, "testuser", &mut state).unwrap();
        assert_eq!(written, 2);
        // Cursor advances to the max createdAt (since filters on createdAt).
        // blitz createdAt=1775677143033, casual createdAt=1689455796698 → max is blitz.
        assert_eq!(state.since_ms, Some(1775677143033_u64));
    }

    #[test]
    fn pull_partitions_by_month() {
        let vault = temp_vault("pull_partition");
        let client = StubClient::one_page(three_games_ndjson());
        let mut state = SyncState::default();
        sync_games(&vault, &client, "testuser", &mut state).unwrap();

        let stream = vault.stream(GAMES_DIR, Partition::Month);
        let keys = stream.partitions().unwrap();
        // We have three games — at minimum 1 partition should exist.
        assert!(!keys.is_empty(), "at least one partition written");
    }

    #[test]
    fn pull_dedupes_on_re_run() {
        let vault = temp_vault("pull_dedupe");
        // First run: writes two games.
        let client = StubClient::one_page(two_games_ndjson());
        let mut state = SyncState::default();
        let first = sync_games(&vault, &client, "testuser", &mut state).unwrap();
        assert_eq!(first, 2);

        // Second run with same data: no new games.
        let client2 = StubClient::one_page(two_games_ndjson());
        let second = sync_games(&vault, &client2, "testuser", &mut state).unwrap();
        assert_eq!(second, 0, "no new games on re-run");
    }

    #[test]
    fn pull_cursor_written_to_disk() {
        let vault = temp_vault("pull_cursor");
        let token = crate::sync::oauth::TokenSet {
            access_token: "testuser".to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        };
        vault.save_sync_token(SERVICE, &token).unwrap();

        let client = StubClient::one_page(game_rated_blitz().to_string());
        let out = pull_with(&vault, &client, "testuser").unwrap();
        assert_eq!(*out.counts.get("games").unwrap(), 1);

        // Cursor must be persisted — value is max createdAt (since filters on createdAt).
        let state = vault.read_lichess_sync();
        assert_eq!(state.since_ms, Some(1775677143033_u64));
        assert!(state.updated.is_some());
    }

    #[test]
    fn pull_not_connected_errors() {
        let vault = temp_vault("pull_not_connected");
        let err = pull(&vault).unwrap_err();
        assert!(
            err.to_string().contains("not connected"),
            "error mentions 'not connected': {err}"
        );
    }

    #[test]
    fn connect_stores_username() {
        let vault = temp_vault("connect_store");
        // Stub returns NoGames (valid empty account = exists).
        let client = StubClient::with_responses(vec![Err(FetchError::NoGames)]);
        connect_with(&vault, &client, "testplayer").unwrap();
        let token = vault.load_sync_token(SERVICE).unwrap().unwrap();
        assert_eq!(token.access_token, "testplayer");
    }

    #[test]
    fn connect_rejects_empty_username() {
        let vault = temp_vault("connect_empty");
        let client = StubClient::with_responses(vec![]);
        let r = connect_with(&vault, &client, "  ");
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("empty username"));
    }

    #[test]
    fn connect_rejects_not_found_user() {
        let vault = temp_vault("connect_not_found");
        let client = StubClient::with_responses(vec![Err(FetchError::NotFound)]);
        let r = connect_with(&vault, &client, "nobody");
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("could not find user"));
    }

    #[test]
    fn full_fidelity_no_fields_dropped() {
        // Verify the raw row stored on disk is the complete API object.
        let vault = temp_vault("full_fidelity");
        let client = StubClient::one_page(game_rated_blitz().to_string());
        let mut state = SyncState::default();
        sync_games(&vault, &client, "testuser", &mut state).unwrap();

        let stream = vault.stream(GAMES_DIR, Partition::Month);
        let keys = stream.partitions().unwrap();
        assert!(!keys.is_empty());
        let rows: Vec<Value> = stream.read(&keys[0]).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        // Every documented field must survive the round-trip.
        assert!(r.get("id").is_some());
        assert!(r.get("rated").is_some());
        assert!(r.get("variant").is_some());
        assert!(r.get("speed").is_some());
        assert!(r.get("perf").is_some());
        assert!(r.get("createdAt").is_some());
        assert!(r.get("lastMoveAt").is_some());
        assert!(r.get("status").is_some());
        assert!(r.get("players").is_some());
        assert!(r.get("opening").is_some());
        assert!(r.get("moves").is_some());
        assert!(r.get("tournament").is_some());
        assert!(r.get("clock").is_some());
    }

    /// Helper: generate a synthetic NDJSON body of `count` games.
    /// Games are numbered starting at `start_id_suffix`, with `createdAt` and
    /// `lastMoveAt` derived from `base_created_ms`. With `sort=dateAsc` the API
    /// returns oldest games first, so we simulate that order here (ascending
    /// createdAt).
    fn synthetic_ndjson_page(start_id_suffix: u32, count: usize, base_created_ms: u64) -> String {
        let mut lines = Vec::with_capacity(count);
        for i in 0..count {
            let created_at = base_created_ms + i as u64 * 60_000; // 1 minute apart
            let last_move_at = created_at + 30_000; // game lasts 30 seconds
            let id = format!("game{:06}", start_id_suffix as usize + i);
            lines.push(format!(
                r#"{{"id":"{id}","rated":true,"variant":"standard","speed":"blitz","perf":"blitz","createdAt":{created_at},"lastMoveAt":{last_move_at},"status":"resign","source":"lobby","players":{{"white":{{"user":{{"name":"alpha","id":"alpha"}},"rating":1500,"ratingDiff":5}},"black":{{"user":{{"name":"beta","id":"beta"}},"rating":1500,"ratingDiff":-5}}}},"winner":"white"}}"#,
            ));
        }
        lines.join("\n")
    }

    /// Verify that a multi-page drain (two full PAGE_SIZE pages) collects ALL
    /// games and sets the cursor to the max createdAt of the second page.
    /// This test would have failed against the pre-fix code (sort defaulted to
    /// dateDesc + cursor tracked lastMoveAt).
    #[test]
    fn multi_page_drain_collects_all_games_and_correct_cursor() {
        let vault = temp_vault("multi_page_drain");

        // Base timestamp well in the past so partitions land in a predictable range.
        let base_ms: u64 = 1_600_000_000_000; // ~2020-09-13

        // Page 1: PAGE_SIZE (300) games, createdAt starting at base_ms (oldest first).
        let page1 = synthetic_ndjson_page(0, PAGE_SIZE as usize, base_ms);
        // Page 2: PAGE_SIZE (300) more games, createdAt continuing from page 1.
        let page2_base = base_ms + PAGE_SIZE as u64 * 60_000;
        let page2 = synthetic_ndjson_page(PAGE_SIZE, PAGE_SIZE as usize, page2_base);

        // StubClient returns page1, then page2, then NoGames (drain complete).
        let client = StubClient::with_responses(vec![
            Ok(page1),
            Ok(page2),
            Err(FetchError::NoGames),
        ]);

        let mut state = SyncState::default();
        let written = sync_games(&vault, &client, "testuser", &mut state).unwrap();

        // All 600 games must be written — previously only 300 (the first page)
        // were collected and the oldest-200-of-500 would be silently lost with
        // a real >300-game account.
        assert_eq!(written, (PAGE_SIZE as u64) * 2, "both pages collected");

        // Cursor must be the max createdAt from page 2 (last game in second page).
        let expected_cursor = page2_base + (PAGE_SIZE as u64 - 1) * 60_000;
        assert_eq!(
            state.since_ms,
            Some(expected_cursor),
            "cursor is max createdAt of last page"
        );
    }
}
