//! Navidrome / Subsonic — self-hosted music server; Subsonic REST API poll.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/navidrome.md.
//!
//! A **Periodic** pull: every song that is currently playing (or recently
//! played) on the user's Navidrome, Airsonic-Advanced, Funkwhale, or any
//! Subsonic-compatible server lands in the unified media stream via the
//! **media-plays write contract** (`docs/vault-spec/domains/media-plays.md`).
//! Two layers per play:
//!
//! - **raw** — the `getNowPlaying` entry verbatim at
//!   `media/plays/navidrome/raw/YYYY-MM.jsonl`, partitioned by the play's month
//!   (full fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/navidrome/YYYY-MM.jsonl`, deduped by `guid` and duration-window.
//!
//! The Subsonic protocol's `getNowPlaying` returns the track currently playing
//! (or recently played, in the last ~5 minutes) for each logged-in user. We
//! poll every 5 minutes, convert the snapshot to a play row, and dedupe so that
//! repeated polls of the same continuing play produce exactly one row.
//!
//! **Timestamp semantics:** `ts` is the poll observation time (RFC3339 local).
//! The Subsonic `minutesAgo` field is defined as "last update" (the Navidrome
//! play tracker resets it on every now-playing report while a track plays), not
//! "elapsed since play started" — so `now - minutesAgo` is NOT a stable
//! play-start estimate and is NOT used here.
//!
//! **Dedup strategy:** guid = `navidrome-<songId>-<poll_window>` where
//! `poll_window = now.timestamp() / NAVIDROME_SYNC_SECS` anchors to the
//! 5-minute poll cadence. Additionally, `write_rows` skips a new row when an
//! existing row for the same `song_id` was observed within the past
//! `duration_secs` of the candidate — this ensures a long track observed over
//! multiple poll windows produces exactly one row (treated as a continuing play).
//!
//! Auth is a composite TokenPaste: `<server-url>|<username>|<password>` — the
//! user pastes their server URL, their Subsonic username, and their password,
//! joined by `|`. The pull uses Subsonic salted-token auth (md5(password +
//! salt)) rather than the legacy plaintext `p=` parameter.
//!
//! No rate limits apply (it's the user's own server); we poll every 5 minutes
//! (300 s) which is deliberately polite.

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
const DIR: &str = "media/plays/navidrome";
const RAW_DIR: &str = "media/plays/navidrome/raw";
/// Non-secret rebuildable cursor — not under `.trove/sync/` (0600 secrets).
const SYNC_FILE: &str = ".trove/navidrome-sync.json";
/// Secret-store service id for the stored `<url>|<username>|<password>` triple.
const SERVICE: &str = "navidrome";

/// Subsonic API version implemented by this client (stable since Navidrome 0.29).
const API_VERSION: &str = "1.16.1";
/// Client name sent in every request — uniquely identifies Trove.
const CLIENT_NAME: &str = "trove";
/// HTTP timeout: the server is local — short keeps the watcher loop responsive.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Seconds between syncs (5 minutes). getNowPlaying reflects plays from the
/// last ~5 minutes, so polling at this cadence catches every completed play.
pub const NAVIDROME_SYNC_SECS: u64 = 300;

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
                format!("Navidrome synced — {n} plays")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Navidrome sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("plays").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Navidrome is up to date — no new plays".to_string()
    } else {
        format!("Navidrome synced — {n} plays")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "navidrome",
        name: "Navidrome / Subsonic",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Play history from your self-hosted Navidrome, Airsonic-Advanced, \
                      Funkwhale, or any Subsonic-compatible music server. Works with \
                      any server that speaks the Subsonic REST API (v1.16.1+).",
        domain: "media",
        vault_path: "media/plays/navidrome/",
        toggleable: true,
        setup: &[
            "Connect with your Subsonic server URL, username, and password on this card.",
            "Trove polls your server every 5 minutes to capture completed plays.",
        ],
        caveats: "If you also scrobble this server to Last.fm or ListenBrainz, \
                  the same plays will appear from both sources — guids prevent \
                  duplicates within this source, but cross-source deduplication \
                  is a read-time concern. \
                  Works with any Subsonic-compatible server: Navidrome, \
                  Airsonic-Advanced, Funkwhale, Ampache.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(NAVIDROME_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("navidrome"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — composite "SERVER_URL|USERNAME|PASSWORD").

/// Parse `url|username|password` from the pasted string.
fn parse_credentials(pasted: &str) -> Result<(String, String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste as SERVER_URL|USERNAME|PASSWORD");
    }
    let (url_part, rest) = pasted
        .split_once('|')
        .with_context(|| {
            "paste as SERVER_URL|USERNAME|PASSWORD \
             (e.g. http://localhost:4533|alice|s3cr3t)"
        })?;
    let (user_part, pass_part) = rest
        .split_once('|')
        .with_context(|| {
            "paste as SERVER_URL|USERNAME|PASSWORD \
             (e.g. http://localhost:4533|alice|s3cr3t)"
        })?;
    let url = url_part.trim().trim_end_matches('/').to_string();
    let username = user_part.trim().to_string();
    let password = pass_part.trim().to_string();
    if url.is_empty() {
        bail!("missing server URL — paste as SERVER_URL|USERNAME|PASSWORD");
    }
    if username.is_empty() {
        bail!("missing username — paste as SERVER_URL|USERNAME|PASSWORD");
    }
    if password.is_empty() {
        bail!("missing password — paste as SERVER_URL|USERNAME|PASSWORD");
    }
    Ok((url, username, password))
}

/// Verify credentials by probing `ping` and store them.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (url, username, password) = parse_credentials(pasted)?;
    let client = SubsonicClient::new(url, username.clone(), password);
    match client.ping() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "The server rejected the credentials (401 / wrong-user or wrong-password) — \
             check your username and password"
        ),
        Err(FetchError::SubsonicError(code, msg)) => bail!(
            "Subsonic error {code}: {msg} — check your username and password"
        ),
        Err(FetchError::Unreachable(e)) => bail!(
            "Could not reach the server — check the URL and that the server is running: {e}"
        ),
        Err(FetchError::Other(e)) => bail!("Server check failed: {e}"),
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
        // Display the server URL as the account label.
        let label = if let Some((url, _)) = pasted.split_once('|') {
            url.trim().to_string()
        } else {
            "Navidrome server".to_string()
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
    id: "navidrome",
    display_name: "Navidrome / Subsonic",
    methods: &[ConnectMethod::TokenPaste {
        label: "Server URL | Username | Password",
        help: "Paste your Subsonic server URL, your username, and your password separated \
               by pipes, e.g. http://localhost:4533|alice|s3cr3t. \
               Works with Navidrome, Airsonic-Advanced, Funkwhale, and Ampache. \
               Credentials are stored locally and sent only to your own server.",
        placeholder: "http://localhost:4533|username|password",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["navidrome"],
    setup: &[
        "Open your Navidrome / Subsonic server (default: http://localhost:4533).",
        "Find your username and password in the server settings.",
        "Paste your server URL, username, and password here as URL|USERNAME|PASSWORD.",
    ],
};

// ---------------------------------------------------------------------------
// Subsonic salted-token auth.

/// Compute Subsonic salted-token auth params: `&t=<md5>&s=<salt>`.
/// Using a fixed salt per pull avoids per-request entropy overhead; the salt
/// is only security-critical in HTTPS contexts (which the user's server likely
/// uses). A random 8-char hex salt per connection attempt is sufficient for
/// the challenge-response scheme.
fn auth_params(password: &str, salt: &str) -> String {
    // md5(password + salt) as lowercase hex — the documented algorithm.
    let raw = format!("{password}{salt}");
    let digest = md5_hex(raw.as_bytes());
    format!("t={digest}&s={salt}")
}

/// Compute MD5 hex from bytes using the `md5` crate (pure Rust, RustCrypto).
/// The Subsonic auth protocol specifies exactly this algorithm:
/// `t = md5(password + salt)` as lowercase hex.
fn md5_hex(data: &[u8]) -> String {
    let digest = md5::compute(data);
    digest.0.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for tests.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Unreachable(String),
    SubsonicError(i64, String),
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::Unreachable(m) => write!(f, "unreachable: {m}"),
            FetchError::SubsonicError(c, m) => write!(f, "Subsonic error {c}: {m}"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Trait over the Subsonic endpoints we need — injectable so tests run offline.
trait SubsonicApi {
    /// `GET /rest/ping` — verifies the credentials work.
    fn ping(&self) -> Result<(), FetchError>;

    /// `GET /rest/getNowPlaying` — one entry per user currently/recently playing.
    fn get_now_playing(&self) -> Result<Vec<Value>, FetchError>;
}

/// Live client backed by ureq + salted-token auth.
struct SubsonicClient {
    base: String,
    username: String,
    password: String,
}

impl SubsonicClient {
    fn new(base: String, username: String, password: String) -> Self {
        let base = base.trim_end_matches('/').to_string();
        SubsonicClient { base, username, password }
    }

    /// Build an authenticated Subsonic API URL for the given endpoint.
    fn url(&self, endpoint: &str) -> String {
        // Use a deterministic short salt for this client instance (not
        // security-critical for LAN/localhost traffic; fine for HTTPS too).
        let salt = "trvs0001";
        let auth = auth_params(&self.password, salt);
        format!(
            "{}/rest/{endpoint}?u={}&{auth}&v={API_VERSION}&c={CLIENT_NAME}&f=json",
            self.base, self.username
        )
    }

    fn call(&self, endpoint: &str) -> Result<Value, FetchError> {
        let url = self.url(endpoint);
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                    FetchError::Unauthorized
                }
                ureq::Error::Transport(t) => FetchError::Unreachable(t.to_string()),
                ureq::Error::Status(code, r) => {
                    let body = r.into_string().unwrap_or_default();
                    FetchError::Other(format!(
                        "HTTP {code}: {}",
                        body.chars().take(200).collect::<String>()
                    ))
                }
            })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| FetchError::Other(format!("JSON parse: {e}")))?;

        // Subsonic wraps all responses in `subsonic-response`; errors have
        // a numeric `error.code` and `error.message`.
        let root = body
            .get("subsonic-response")
            .ok_or_else(|| FetchError::Other("missing subsonic-response wrapper".into()))?;

        let status = root.get("status").and_then(Value::as_str).unwrap_or("");
        if status != "ok" {
            let code = root
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let msg = root
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            return if code == 40 || code == 41 {
                Err(FetchError::Unauthorized)
            } else {
                Err(FetchError::SubsonicError(code, msg))
            };
        }
        Ok(root.clone())
    }
}

impl SubsonicApi for SubsonicClient {
    fn ping(&self) -> Result<(), FetchError> {
        self.call("ping").map(|_| ())
    }

    fn get_now_playing(&self) -> Result<Vec<Value>, FetchError> {
        let root = self.call("getNowPlaying")?;
        // `nowPlaying` may be absent when nobody is playing.
        let entries: Vec<Value> = match root
            .get("nowPlaying")
            .and_then(|n| n.get("entry"))
        {
            Some(Value::Array(a)) => a.clone(),
            Some(obj @ Value::Object(_)) => vec![obj.clone()],
            _ => Vec::new(),
        };
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
    /// Per-song last-observed Unix epoch (seconds).  Persisted across polls so
    /// the duration-window dedup in `write_rows` can suppress duplicate rows
    /// even when the suppression itself would not write a row (and therefore
    /// cannot advance the stored observation time via the contract file alone).
    /// Map key: song_id string.  Serde-compatible with older cursors that lack
    /// this field (defaults to empty map).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    song_last_seen: std::collections::BTreeMap<String, i64>,
}

impl Vault {
    fn read_navidrome_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_navidrome_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Convert a `getNowPlaying` entry to a [`MediaItem`]. Returns `None` if the
/// entry has no usable song id or title.
///
/// `ts` is set to `now` (the poll observation time), not `now - minutesAgo`.
/// The OpenSubsonic spec defines `minutesAgo` as "last update" — Navidrome's
/// play tracker resets it on every now-playing report while a track plays, so
/// it is not a stable measure of elapsed time since the play started and cannot
/// reliably reconstruct the play-start time.
///
/// The guid is `navidrome-<songId>-<poll_window>` where
/// `poll_window = now.timestamp() / NAVIDROME_SYNC_SECS`, anchoring dedup to
/// the 5-minute poll cadence.  An additional duration-window check in
/// `write_rows` suppresses duplicate rows when the same track continues across
/// multiple poll windows (see that function's doc).
fn entry_to_media(entry: &Value, now: DateTime<Local>) -> Option<MediaItem> {
    let song_id = entry.get("id").and_then(Value::as_str).unwrap_or("").trim();
    if song_id.is_empty() {
        return None;
    }
    let title = entry.get("title").and_then(Value::as_str).unwrap_or("").trim();
    if title.is_empty() {
        return None;
    }

    let artist = entry.get("artist").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let album = entry.get("album").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let duration_secs = entry.get("duration").and_then(Value::as_u64).unwrap_or(0);

    // ts = observation time (honest: "we saw this play at this moment").
    // minutesAgo is NOT subtracted: it is "last update" per the OpenSubsonic
    // spec, not "elapsed since play started", and drifts across polls.
    let ts = now.to_rfc3339();

    // Anchor the guid to the 5-minute poll cadence window, not to minutesAgo.
    // This gives the same guid for any call within the same 300s window so
    // within-poll dedup is exact; the duration-window check in write_rows
    // handles cross-window dedup for long-running plays.
    let poll_window = now.timestamp() / (NAVIDROME_SYNC_SECS as i64);
    let guid = format!("navidrome-{song_id}-{poll_window}");

    let username = entry.get("username").and_then(Value::as_str).unwrap_or("").trim();
    let player_name = entry.get("playerName").and_then(Value::as_str).unwrap_or("").trim();

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.is_empty() {
            extra.insert(k.into(), Value::String(v.into()));
        }
    };
    put("song_id", song_id);
    put("username", username);
    put("player_name", player_name);
    if duration_secs > 0 {
        extra.insert("duration_secs".into(), Value::Number(duration_secs.into()));
    }
    if let Some(bit_rate) = entry.get("bitRate").and_then(Value::as_u64) {
        extra.insert("bit_rate".into(), Value::Number(bit_rate.into()));
    }

    Some(MediaItem {
        ts,
        source: "navidrome".into(),
        category: "music".into(),
        device: String::new(),
        kind: "play".into(),
        title: title.to_string(),
        subtitle: artist,
        detail: album,
        // The Subsonic API returns duration but not seconds-played for ongoing
        // plays — an honest unknown per the spec.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write helpers.

/// A raw entry line: tagged with `ts` for month partitioning, written as the
/// verbatim API object (the lastfm/jellyfin pattern).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Write raw + contract rows, deduped by guid and by duration window.
///
/// Two levels of dedup:
///
/// 1. **Guid dedup** — `seen_guids` set of stored guids, unchanged across
///    polls of the same 5-min window (fast path).
///
/// 2. **Duration-window dedup** — for each candidate, check whether the same
///    `song_id` was observed within the past `duration_secs`.  If so, the
///    candidate is a continuation of the same play and is skipped.  This
///    prevents duplicate rows when a long track spans multiple 5-min poll
///    windows and therefore receives a fresh guid on each poll.
///
/// `state.song_last_seen` (a persisted map of `song_id → Unix epoch`) is the
/// authoritative source for the last observation time.  It is updated here
/// (both when writing and when suppressing) so that consecutive suppressed
/// polls still roll the window forward.  The caller is responsible for
/// persisting the updated state to disk.
///
/// Returns the count of genuinely new rows written.
fn write_rows(vault: &Vault, rows: &[MediaItem], raws: &[Value], state: &mut SyncState) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Collect existing guids for fast dedup.
    let mut seen_guids: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for it in contract.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                seen_guids.insert(it.guid.clone());
            }
        }
    }

    let mut new_rows: Vec<MediaItem> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();

    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        // Level 1: guid-based dedup (same poll window).
        if !seen_guids.insert(item.guid.clone()) {
            continue;
        }

        // Level 2: duration-window dedup (same play continuing into a new poll window).
        //
        // If the same song was observed within the past `duration_secs`, the new
        // observation is a continuation of that play, not a fresh listen.
        // `state.song_last_seen` is persisted across pull calls so this check
        // works even when no row is written for the suppressed observation.
        // We always advance the stored time — whether writing or suppressing —
        // so consecutive polls roll the window forward correctly.
        let duration_secs = item
            .extra
            .get("duration_secs")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if duration_secs > 0 {
            if let Some(Value::String(sid)) = item.extra.get("song_id") {
                if let Ok(parsed) = DateTime::parse_from_rfc3339(&item.ts) {
                    let candidate_epoch = parsed.timestamp();
                    let suppress = if let Some(&prev_epoch) = state.song_last_seen.get(sid.as_str()) {
                        // Previous observation within this track's duration → same play.
                        let gap = candidate_epoch - prev_epoch;
                        gap >= 0 && gap < duration_secs as i64
                    } else {
                        false
                    };
                    // Advance the persisted observation time regardless of suppress.
                    let entry = state.song_last_seen.entry(sid.clone()).or_insert(i64::MIN);
                    if candidate_epoch > *entry {
                        *entry = candidate_epoch;
                    }
                    if suppress {
                        continue; // same play, skip
                    }
                }
            }
        }

        new_rows.push(item.clone());
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;

    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context(
            "Navidrome is not connected — add your server URL, username, and password \
             in the Integrations tab",
        )?;
    let (url, username, password) = parse_credentials(&pasted)?;
    let client = SubsonicClient::new(url, username, password);
    pull_with(vault, &client, Local::now())
}

/// The pull body over an injected API and an injected clock — the testable seam.
///
/// `now` is the observation timestamp; passing it explicitly lets tests drive
/// consecutive polls with controlled time values to verify dedup behaviour.
fn pull_with(vault: &Vault, api: &impl SubsonicApi, now: DateTime<Local>) -> Result<PullOutcome> {
    let entries = api
        .get_now_playing()
        .map_err(|e| anyhow::anyhow!("Navidrome getNowPlaying failed: {e}"))?;

    let mut rows: Vec<MediaItem> = Vec::new();
    let mut raws: Vec<Value> = Vec::new();

    for entry in &entries {
        if let Some(item) = entry_to_media(entry, now) {
            rows.push(item);
            raws.push(entry.clone());
        }
    }

    // Load sync state before write_rows so the duration-window dedup can read
    // and update `song_last_seen` (persisted across pull calls).
    let mut state = vault.read_navidrome_sync();

    let new_plays = if rows.is_empty() {
        0
    } else {
        write_rows(vault, &rows, &raws, &mut state)?
    };

    state.updated = Some(now.to_rfc3339());
    vault.write_navidrome_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{new_plays} plays"),
        counts: BTreeMap::from([("plays", new_plays)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-navidrome-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Fixed "now" for deterministic timestamps in tests.
    /// 2026-06-15 12:00:00 local time.
    fn fixed_now() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap()
    }

    /// Fixed "now" advanced by `secs` seconds (simulates a later poll).
    fn fixed_now_plus(secs: i64) -> DateTime<Local> {
        fixed_now() + chrono::Duration::seconds(secs)
    }

    /// A realistic `getNowPlaying` entry from the OpenSubsonic API documentation.
    /// `duration` defaults to 227 s so duration-window dedup has a reference.
    fn sample_entry(song_id: &str, title: &str, artist: &str, album: &str, minutes_ago: u64) -> Value {
        serde_json::json!({
            "id": song_id,
            "parent": "200147046",
            "title": title,
            "isDir": false,
            "isVideo": false,
            "type": "music",
            "albumId": "200147046",
            "album": album,
            "artistId": "100002619",
            "artist": artist,
            "coverArt": song_id,
            "duration": 227,
            "bitRate": 320,
            "bitDepth": 16,
            "samplingRate": 44100,
            "channelCount": 2,
            "track": 7,
            "year": 2020,
            "genre": "Electronic",
            "size": 6341039,
            "suffix": "mp3",
            "contentType": "audio/mpeg",
            "username": "alice",
            "minutesAgo": minutes_ago,
            "playerId": 1,
            "playerName": "NavidromeUI",
            "state": "playing"
        })
    }

    /// Like `sample_entry` but with an explicit `duration` field.
    fn sample_entry_dur(song_id: &str, title: &str, duration: u64) -> Value {
        serde_json::json!({
            "id": song_id,
            "title": title,
            "artist": "Artist",
            "album": "Album",
            "duration": duration,
            "username": "alice",
            "minutesAgo": 0_u64,
            "playerName": "NavidromeUI",
            "state": "playing"
        })
    }

    #[test]
    fn parses_now_playing_entry_to_media_item() {
        let now = fixed_now();
        let entry = sample_entry("300115266", "Take the Home", "Raggedy Angry",
                                 "How I Learned to Love Our Robot Overlords", 2);
        let item = entry_to_media(&entry, now).unwrap();

        assert_eq!(item.title, "Take the Home");
        assert_eq!(item.subtitle, "Raggedy Angry");
        assert_eq!(item.detail, "How I Learned to Love Our Robot Overlords");
        assert_eq!(item.source, "navidrome");
        assert_eq!(item.category, "music");
        assert_eq!(item.kind, "play");
        assert_eq!(item.seconds, 0, "duration unknown for ongoing plays");
        assert!(!item.guid.is_empty(), "guid must be set");
        assert!(item.guid.starts_with("navidrome-300115266-"),
                "guid encodes song id: {}", item.guid);

        // ts is the observation time (== now), NOT now-minutesAgo.
        // minutesAgo is the OpenSubsonic "last update" field, not play-start elapsed.
        let ts_parsed = DateTime::parse_from_rfc3339(&item.ts).unwrap();
        let diff_secs = (ts_parsed.timestamp() - now.timestamp()).abs();
        assert!(diff_secs < 5, "ts should equal the observation time (now), diff={diff_secs}s");

        // Extra fields populated from the API entry.
        assert_eq!(item.extra.get("song_id"), Some(&Value::String("300115266".into())));
        assert_eq!(item.extra.get("username"), Some(&Value::String("alice".into())));
        assert_eq!(item.extra.get("player_name"), Some(&Value::String("NavidromeUI".into())));
        assert_eq!(item.extra.get("duration_secs"), Some(&Value::Number(227.into())));
        assert_eq!(item.extra.get("bit_rate"), Some(&Value::Number(320.into())));
    }

    #[test]
    fn skips_entry_with_no_id_or_title() {
        let now = fixed_now();
        // No id.
        let no_id = serde_json::json!({"title": "Song", "artist": "A", "minutesAgo": 0});
        assert!(entry_to_media(&no_id, now).is_none(), "missing id → None");
        // No title.
        let no_title = serde_json::json!({"id": "123", "artist": "A", "minutesAgo": 0});
        assert!(entry_to_media(&no_title, now).is_none(), "missing title → None");
        // Empty title.
        let empty_title = serde_json::json!({"id": "123", "title": "", "minutesAgo": 0});
        assert!(entry_to_media(&empty_title, now).is_none(), "empty title → None");
    }

    #[test]
    fn same_song_same_poll_window_dedupes() {
        // Two calls with the SAME `now` (same poll window) must produce the same guid,
        // regardless of minutesAgo.  minutesAgo no longer influences the guid.
        let now = fixed_now();
        let e1 = sample_entry("abc", "Track", "Artist", "Album", 1);
        let e2 = sample_entry("abc", "Track", "Artist", "Album", 2);
        let item1 = entry_to_media(&e1, now).unwrap();
        let item2 = entry_to_media(&e2, now).unwrap();
        assert_eq!(item1.guid, item2.guid,
            "same song + same poll window → same guid (minutesAgo ignored)");
    }

    #[test]
    fn different_songs_different_guids() {
        let now = fixed_now();
        let e1 = sample_entry("song1", "Track A", "Artist", "Album", 0);
        let e2 = sample_entry("song2", "Track B", "Artist", "Album", 0);
        let item1 = entry_to_media(&e1, now).unwrap();
        let item2 = entry_to_media(&e2, now).unwrap();
        assert_ne!(item1.guid, item2.guid, "different songs → different guids");
    }

    /// Stub that returns a fixed list of now-playing entries.
    struct StubApi {
        entries: Vec<Value>,
    }
    impl SubsonicApi for StubApi {
        fn ping(&self) -> Result<(), FetchError> {
            Ok(())
        }
        fn get_now_playing(&self) -> Result<Vec<Value>, FetchError> {
            Ok(self.entries.clone())
        }
    }

    #[test]
    fn writes_partitioned_layers_and_dedupes() {
        let v = temp_vault("store");
        let now = fixed_now();
        let api = StubApi {
            entries: vec![
                sample_entry("s1", "Song One", "Artist A", "Album 1", 3),
                sample_entry("s2", "Song Two", "Artist B", "Album 2", 1),
            ],
        };

        let out = pull_with(&v, &api, now).unwrap();
        assert_eq!(out.counts.get("plays"), Some(&2));

        // Contract layer exists.
        let month = "2026-06";
        let contract_path = v.root().join(format!("media/plays/navidrome/{month}.jsonl"));
        assert!(contract_path.exists(), "contract file written");
        let lines: Vec<_> = std::fs::read_to_string(&contract_path).unwrap()
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        // Check both titles appear.
        let titles: HashSet<String> = lines.iter()
            .map(|l| l.get("title").and_then(Value::as_str).unwrap_or("").to_string())
            .collect();
        assert!(titles.contains("Song One"));
        assert!(titles.contains("Song Two"));

        // Raw layer exists.
        let raw_path = v.root().join(format!("media/plays/navidrome/raw/{month}.jsonl"));
        assert!(raw_path.exists(), "raw file written");
        let raw_content = std::fs::read_to_string(&raw_path).unwrap();
        let raw_lines: Vec<_> = raw_content.lines().collect();
        assert_eq!(raw_lines.len(), 2);
        // Raw contains the verbatim API fields.
        assert!(raw_lines[0].contains("NavidromeUI") || raw_lines[1].contains("NavidromeUI"),
                "raw has playerName from API");

        // Second pull with same entries and same `now` (same poll window):
        // guid dedup → 0 new rows.
        let out2 = pull_with(&v, &api, now).unwrap();
        assert_eq!(out2.counts.get("plays"), Some(&0), "re-run same window: guid dedup");
        let len2 = std::fs::read_to_string(&contract_path).unwrap().lines().count();
        assert_eq!(len2, 2, "contract file unchanged after re-run");

        // Cursor was written.
        let state = v.read_navidrome_sync();
        assert!(state.updated.is_some(), "cursor updated after sync");
    }

    /// Regression test for the "unstable-guid / duplicate-flood" bug.
    ///
    /// Simulates FOUR consecutive polls of ONE continuously-playing track
    /// (duration=400s, polls 300s apart so the track is still in-progress at
    /// each poll).  Only ONE contract row must be written — not three duplicates.
    #[test]
    fn two_polls_of_same_continuing_play_write_one_row() {
        let v = temp_vault("cross-poll");

        // Song with duration 400 s (> poll cadence of 300 s) → should still be
        // visible at poll 2 and treated as the same play.
        let entries = vec![sample_entry_dur("longtrack", "Long Track", 400)];
        let api = StubApi { entries };

        // Poll 1 at T=0.
        let now1 = fixed_now();
        let out1 = pull_with(&v, &api, now1).unwrap();
        assert_eq!(out1.counts.get("plays"), Some(&1), "poll 1: first observation written");

        // Poll 2 at T=300 (same track still playing, minutesAgo≈0 per spec).
        let now2 = fixed_now_plus(300);
        let out2 = pull_with(&v, &api, now2).unwrap();
        assert_eq!(out2.counts.get("plays"), Some(&0),
            "poll 2: continuing play suppressed by duration-window dedup");

        // Poll 3 at T=600.
        let now3 = fixed_now_plus(600);
        let out3 = pull_with(&v, &api, now3).unwrap();
        assert_eq!(out3.counts.get("plays"), Some(&0),
            "poll 3: still within duration window — suppressed");

        // Poll 4 at T=900.
        let now4 = fixed_now_plus(900);
        let out4 = pull_with(&v, &api, now4).unwrap();
        assert_eq!(out4.counts.get("plays"), Some(&0),
            "poll 4: still within duration window — suppressed");

        // Exactly one row in the vault.
        let month = "2026-06";
        let contract_path = v.root().join(format!("media/plays/navidrome/{month}.jsonl"));
        let row_count = std::fs::read_to_string(&contract_path).unwrap().lines().count();
        assert_eq!(row_count, 1,
            "one continuous play → exactly 1 contract row, got {row_count}");
    }

    /// Verify that two DISTINCT plays of the same song (after the previous
    /// play's duration window expires) both get written.
    #[test]
    fn two_distinct_replays_both_written() {
        let v = temp_vault("replay");

        // Short track: 60 s.  After 300 s the window has clearly expired.
        let entries = vec![sample_entry_dur("shorttrack", "Short Track", 60)];
        let api = StubApi { entries };

        // Play 1 at T=0.
        let out1 = pull_with(&v, &api, fixed_now()).unwrap();
        assert_eq!(out1.counts.get("plays"), Some(&1), "play 1 written");

        // Play 2 at T=300 (300 s > 60 s duration → previous window expired).
        let out2 = pull_with(&v, &api, fixed_now_plus(300)).unwrap();
        assert_eq!(out2.counts.get("plays"), Some(&1),
            "play 2 (distinct replay, window expired) → written as new row");

        let month = "2026-06";
        let contract_path = v.root().join(format!("media/plays/navidrome/{month}.jsonl"));
        let row_count = std::fs::read_to_string(&contract_path).unwrap().lines().count();
        assert_eq!(row_count, 2, "two distinct plays → 2 rows");
    }

    #[test]
    fn empty_now_playing_is_a_noop() {
        let v = temp_vault("empty");
        let api = StubApi { entries: vec![] };
        let out = pull_with(&v, &api, fixed_now()).unwrap();
        assert_eq!(out.counts.get("plays"), Some(&0));
        // No contract file created.
        assert!(!v.root().join("media/plays/navidrome").exists()
                || std::fs::read_dir(v.root().join("media/plays/navidrome"))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true));
    }

    #[test]
    fn parse_credentials_splits_three_parts() {
        let (url, user, pass) = parse_credentials("http://localhost:4533|alice|s3cr3t").unwrap();
        assert_eq!(url, "http://localhost:4533");
        assert_eq!(user, "alice");
        assert_eq!(pass, "s3cr3t");
    }

    #[test]
    fn parse_credentials_strips_trailing_slash() {
        let (url, _, _) = parse_credentials("http://localhost:4533/|alice|pass").unwrap();
        assert_eq!(url, "http://localhost:4533");
    }

    #[test]
    fn parse_credentials_rejects_short_inputs() {
        assert!(parse_credentials("").is_err());
        assert!(parse_credentials("http://localhost:4533|alice").is_err());
        assert!(parse_credentials("onlyone").is_err());
    }

    #[test]
    fn connection_exposes_token_paste() {
        assert!(CONNECTION.method("token-paste").is_some());
    }

    #[test]
    fn md5_hex_matches_known_vector() {
        // Subsonic auth: md5("sesame" + "c19b2d") from the spec example.
        // Expected from the Subsonic API docs.
        let result = md5_hex(b"sesamec19b2d");
        assert_eq!(result, "26719a1196d2a940705a59634eb18eab");
    }

    #[test]
    fn auth_params_format() {
        let params = auth_params("sesame", "c19b2d");
        assert!(params.starts_with("t=26719a1196d2a940705a59634eb18eab&s=c19b2d"),
                "auth params: {params}");
    }
}
