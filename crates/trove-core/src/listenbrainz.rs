//! ListenBrainz — the open scrobble hub backed by MusicBrainz.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/listenbrainz.md.
//!
//! A **Periodic** cloud pull (M5): every listen ListenBrainz has recorded for
//! a public profile lands in the unified media stream via the **media-plays
//! write contract** (`docs/vault-spec/domains/media-plays.md`). Two layers per
//! play, exactly like [`crate::lastfm`]:
//!
//! - **raw** — the API listen object verbatim at
//!   `media/plays/listenbrainz/raw/YYYY-MM.jsonl`, partitioned by the listen's
//!   month (full `track_metadata` fidelity, unconditional).
//! - **contract** — one normalized [`MediaItem`] at
//!   `media/plays/listenbrainz/YYYY-MM.jsonl`, deduped by `guid`.
//!
//! `GET /1/user/{user}/listens` returns newest-first, paginated by timestamp
//! (`count`, `min_ts`, `max_ts` — the API rejects `min_ts` and `max_ts`
//! together). Every sync walks `max_ts` backward from the newest page and
//! stops at the watermark, bounding the bottom CLIENT-SIDE: the first sync
//! (no watermark) drains the whole history, and later syncs drain the whole
//! gap of listens above the watermark — pages all the way down rather than
//! fetching only the newest page (which would strand older un-fetched listens
//! in a large gap). The watermark is the max `listened_at` ever written, kept
//! in a rebuildable cursor at `.trove/listenbrainz-sync.json` (non-secret
//! state, beside the vault's other `.trove/` indexes, not under
//! `.trove/sync/`).
//!
//! Auth is just a public username — public-profile reads are **keyless**.
//! The username is the connection's single pasted field (TokenPaste), stored
//! in the `access_token` slot of a never-expiring [`crate::sync::oauth::TokenSet`]
//! under `.trove/sync/listenbrainz.json`. Because no key is needed, the
//! username *can* be verified at connect time with a one-listen probe.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Result};
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
const DIR: &str = "media/plays/listenbrainz";
const RAW_DIR: &str = "media/plays/listenbrainz/raw";
/// Non-secret rebuildable cursor — *not* under `.trove/sync/` (that's for
/// 0600 secrets); deleting it re-walks the whole history on the next sync.
const SYNC_FILE: &str = ".trove/listenbrainz-sync.json";

/// The service id under `.trove/sync/` where the username is stored (reusing
/// the secret store's `service-token.json` slot, exactly like Last.fm).
const SERVICE: &str = "listenbrainz";

const API_BASE: &str = "https://api.listenbrainz.org";
/// ListenBrainz allows up to 1000 listens per request; 100 is a polite page
/// that keeps each request light during a backfill walk.
const PAGE_SIZE: u32 = 100;
/// Small inter-request delay so a backfill stays well under any rate limit.
const REQ_INTERVAL: Duration = Duration::from_millis(250);
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Seconds between syncs in the watcher loop. Hourly: listens trickle in and
/// the incremental poll is one cheap request (the newest page) when idle.
pub const LISTENBRAINZ_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull the manual "Sync now" runs, but it never
// errors the loop — a missing username or a network blip is just a quiet
// no-op until the next tick.
fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("listens").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("listenbrainz synced — {n} listens")
            }))
        }
        // Not connected / transient network: stay silent, retry next tick. A
        // real bug still surfaces in the log via the message.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "listenbrainz sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let listens = out.counts.get("listens").copied().unwrap_or(0);
    let headline = if listens == 0 {
        "ListenBrainz is up to date — no new listens".to_string()
    } else {
        format!("ListenBrainz synced — {listens} listens")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "listenbrainz",
        name: "ListenBrainz",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Syncs your listening history from ListenBrainz, the open scrobble \
             hub backed by MusicBrainz, into the unified media stream. First \
             sync backfills your whole history; later syncs fetch only what's \
             new. Listens carry MusicBrainz GUIDs for cross-source linking.",
        domain: "media",
        vault_path: "media/plays/listenbrainz/",
        toggleable: true,
        setup: &[
            "Connect with your public ListenBrainz username on this card.",
            "First sync backfills your entire listening history (paginated); later syncs are incremental.",
        ],
        caveats: "Reads your public ListenBrainz profile — no account or token needed. \
                  ListenBrainz records play *events*, not durations, so every listen has seconds=0. \
                  MusicBrainz IDs (recording/artist/release MBIDs) are present only on listens the \
                  server has matched to MusicBrainz; unmatched listens carry only the title, artist, \
                  and the MessyBrainz recording_msid.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(LISTENBRAINZ_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("listenbrainz"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the public username).

/// Store the pasted username under `.trove/sync/listenbrainz.json` (0600),
/// modeled on Last.fm: the username rides in the `access_token` slot of a
/// never-expiring [`crate::sync::oauth::TokenSet`], so the secret-store
/// mechanics are shared and `status`/`disconnect` are trivial. Reads are
/// keyless, so we verify the profile exists with a one-listen probe; a clear
/// error is surfaced to the connect UI when the user doesn't exist.
fn def_connect(vault: &Vault, username: &str) -> Result<()> {
    let client = ListenBrainzClient::new(API_BASE.to_string());
    connect_with(vault, &client, username)
}

/// The connect body over an injected fetcher — the testable seam (tests verify
/// against a stub, never the network).
fn connect_with(vault: &Vault, client: &impl Listens, username: &str) -> Result<()> {
    let username = username.trim();
    if username.is_empty() {
        bail!("empty username");
    }
    // Keyless verification: a single-listen probe. 404 / a clear error means
    // the profile doesn't exist; a network blip doesn't block storing (the
    // user may be briefly offline), only an explicit not-found does.
    match client.listens(username, 1, None, None) {
        Ok(_) => {}
        Err(FetchError::NotFound) => {
            bail!("ListenBrainz could not find user {username:?} — check the spelling")
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

/// Forget the stored username. Synced data stays in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// `configured` is always true: public reads need no app credentials, so
/// connecting is just pasting a username. The connected account, if any, is
/// the stored username.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let username = token.access_token;
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: username,
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // a username never expires
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste a
/// public username. No api_key — public-profile reads are fully keyless.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "listenbrainz",
    display_name: "ListenBrainz",
    methods: &[ConnectMethod::TokenPaste {
        label: "ListenBrainz username",
        help: "Your public ListenBrainz profile name — listening history is read from your public profile (no account or token needed).",
        placeholder: "e.g. rob",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["listenbrainz"],
    setup: &[
        "Enter your public ListenBrainz username and connect.",
        "Listening history is read from your public profile; no account or token is needed.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors. 404 (no such user) and 429 (rate limited) want
/// distinct handling; everything else is a message.
#[derive(Debug)]
enum FetchError {
    NotFound,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NotFound => write!(f, "user not found (HTTP 404)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// One page of `GET /1/user/{user}/listens`. Tests implement this against
/// fixtures; production hits the real API.
trait Listens {
    /// `min_ts` / `max_ts` are the timestamp paging bounds (unix seconds,
    /// both exclusive). At most one is set per call (the API rejects both).
    fn listens(
        &self,
        user: &str,
        count: u32,
        min_ts: Option<i64>,
        max_ts: Option<i64>,
    ) -> Result<Value, FetchError>;
}

/// Thin client. The base URL is injected so the sync logic stays testable
/// against a local stub (the `lastfm.rs` pattern).
struct ListenBrainzClient {
    base: String,
}

impl ListenBrainzClient {
    fn new(base: String) -> Self {
        ListenBrainzClient { base }
    }
}

impl Listens for ListenBrainzClient {
    fn listens(
        &self,
        user: &str,
        count: u32,
        min_ts: Option<i64>,
        max_ts: Option<i64>,
    ) -> Result<Value, FetchError> {
        let mut req = ureq::get(&format!("{}/1/user/{}/listens", self.base, user))
            .timeout(HTTP_TIMEOUT)
            .query("count", &count.to_string());
        // The API rejects min_ts and max_ts together. The pull only ever sends
        // max_ts (downward paging); connect's probe sends neither. Guard anyway
        // by preferring min_ts should a caller ever set both.
        if let Some(min_ts) = min_ts {
            req = req.query("min_ts", &min_ts.to_string());
        } else if let Some(max_ts) = max_ts {
            req = req.query("max_ts", &max_ts.to_string());
        }
        match req.call() {
            Ok(resp) => {
                // Honor the advertised rate-limit headers: if we're nearly out
                // of the window's budget, wait out the reset before returning
                // so the next page doesn't 429.
                let remaining = resp
                    .header("X-RateLimit-Remaining")
                    .and_then(|s| s.trim().parse::<i64>().ok());
                let reset_in = resp
                    .header("X-RateLimit-Reset-In")
                    .and_then(|s| s.trim().parse::<u64>().ok());
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                if matches!(remaining, Some(r) if r <= 1) {
                    // Cap the wait so a bogus header can't stall the loop.
                    thread::sleep(Duration::from_secs(reset_in.unwrap_or(2).min(10)));
                }
                Ok(v)
            }
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
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
    /// Max `listened_at` ever written. The next poll pages `max_ts` backward
    /// and writes every listen strictly above this, stopping at the boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_listenbrainz_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_listenbrainz_sync(&self, state: &SyncState) -> Result<()> {
        let path = self.resolve(SYNC_FILE)?;
        write_json_atomic(&path, state)
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Parse one `listens` response into (contract rows, raw listen objects). The
/// listen array lives at `payload.listens`; each entry maps to one
/// [`MediaItem`]. Defensive about the API's shape:
/// - `mbid_mapping` is present ONLY on server-matched listens — its keys are
///   simply absent on unmatched ones (never written as ""/null).
/// - `track_metadata.release_name` may be missing/empty — omitted from detail.
/// - `recording_msid` is top-level and always present (the guid anchor).
fn parse_listens(body: &Value) -> (Vec<MediaItem>, Vec<Value>) {
    let listens: Vec<Value> = body
        .get("payload")
        .and_then(|p| p.get("listens"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for l in listens {
        let Some(item) = listen_item(&l) else {
            continue;
        };
        rows.push(item);
        raws.push(l);
    }
    (rows, raws)
}

/// One listen object → a media-plays contract row. `None` if it has no
/// `listened_at` (not a real listen) or no track title.
fn listen_item(l: &Value) -> Option<MediaItem> {
    let listened_at = l.get("listened_at").and_then(Value::as_i64)?;
    let meta = l.get("track_metadata");

    let title = meta
        .and_then(|m| m.get("track_name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }
    let artist = meta
        .and_then(|m| m.get("artist_name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let release = meta
        .and_then(|m| m.get("release_name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    let recording_msid = l
        .get("recording_msid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // listened_at is UTC unix seconds → RFC3339 with the local offset (the
    // same epoch→local helper every collector uses; no hand-rolled tz math).
    let ts = DateTime::from_timestamp(listened_at, 0)?
        .with_timezone(&Local)
        .to_rfc3339();

    // recording_msid is always present and unique → a stable guid anchor.
    let guid = format!("lb-{listened_at}-{recording_msid}");

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().into()));
        }
    };

    // mbid_mapping lives under track_metadata and is present ONLY for
    // server-matched listens. When absent, none of these keys are written —
    // the unmatched listen carries no empty mbid columns.
    let mapping = meta.and_then(|m| m.get("mbid_mapping"));
    put(
        "recording_mbid",
        mapping
            .and_then(|m| m.get("recording_mbid"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    put(
        "release_mbid",
        mapping
            .and_then(|m| m.get("release_mbid"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );
    // artist_mbids is an ARRAY of UUID strings; stored as a comma-joined
    // string so the contract row stays flat (the verbatim array is preserved
    // in the raw layer). Empty/absent → omitted.
    let artist_mbids = mapping
        .and_then(|m| m.get("artist_mbids"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    put("artist_mbids", &artist_mbids);

    // recording_msid (top-level, MessyBrainz id) and the submission client are
    // always-available provenance.
    put("recording_msid", &recording_msid);
    put(
        "submission_client",
        meta.and_then(|m| m.get("additional_info"))
            .and_then(|a| a.get("submission_client"))
            .and_then(Value::as_str)
            .unwrap_or(""),
    );

    Some(MediaItem {
        ts,
        source: "listenbrainz".into(),
        category: "music".into(),
        device: String::new(),
        kind: "play".into(),
        title,
        // Grouping key for charts: the artist.
        subtitle: artist,
        detail: release,
        // ListenBrainz records events, not durations — an honest unknown.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

fn listened_at_of(l: &Value) -> Option<i64> {
    l.get("listened_at").and_then(Value::as_i64)
}

/// The `listened_at` (unix seconds) a contract row was built from. `ts` is the
/// RFC3339 rendering of exactly that epoch (see [`listen_item`]), so parsing it
/// back recovers the value — used to bound the pull against the watermark.
fn listened_at_of_item(item: &MediaItem) -> Option<i64> {
    DateTime::parse_from_rfc3339(&item.ts)
        .ok()
        .map(|dt| dt.timestamp())
}

// ---------------------------------------------------------------------------
// The pull.

/// Outcome of writing one batch of parsed listens.
struct WriteStats {
    listens: u64,
    /// Max `listened_at` written — drives the forward-only watermark advance.
    max_ts: Option<i64>,
}

/// A raw listen object carrying the contract ts purely so the month-partition
/// writer files it under the listen's month. Only `value` is serialized to
/// disk — flattened, so the raw line is the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Write raw + contract rows, deduped by guid against what's already on disk,
/// and report the count written plus the max `listened_at` seen. Raw lines
/// partition by the same month as their contract row (the listen's month).
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
    let mut max_ts: Option<i64> = None;
    for (item, raw_val) in rows.iter().zip(raws.iter()) {
        if let Some(t) = listened_at_of(raw_val) {
            max_ts = Some(max_ts.map_or(t, |m| m.max(t)));
        }
        if !seen.insert(item.guid.clone()) {
            continue; // already stored
        }
        new_rows.push(item.clone());
        new_raws.push(RawLine { ts: item.ts.clone(), value: raw_val.clone() });
    }

    contract.append(&new_rows, |i| &i.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;

    Ok(WriteStats { listens: new_rows.len() as u64, max_ts })
}

/// Resolve the username and sync. Pages `max_ts` backward from the newest
/// listen, writing everything above the watermark and stopping once the
/// boundary is reached: a first run (no watermark) drains the whole history,
/// later runs drain the whole gap of new listens. Returns a generic
/// [`PullOutcome`] with a `listens` count.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let username = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("ListenBrainz is not connected — add your username in the Integrations tab")
        })?;
    let client = ListenBrainzClient::new(API_BASE.to_string());
    pull_with(vault, &client, &username)
}

/// One page fetch with a single rate-limit back-off-and-retry. Mirrors
/// lastfm's 429 handling.
fn fetch_page(
    client: &impl Listens,
    user: &str,
    min_ts: Option<i64>,
    max_ts: Option<i64>,
) -> Result<Value> {
    match client.listens(user, PAGE_SIZE, min_ts, max_ts) {
        Ok(b) => Ok(b),
        Err(FetchError::NotFound) => bail!("ListenBrainz could not find user {user:?}"),
        Err(FetchError::RateLimited) => {
            // Back off once and retry the same page. If it still fails the pull
            // bails; since the watermark only advances after the loop fully
            // drains, the next tick re-drains the gap from the same watermark
            // (guid dedupe skips what already landed) — no listens are lost.
            thread::sleep(Duration::from_secs(2));
            client
                .listens(user, PAGE_SIZE, min_ts, max_ts)
                .map_err(|e| anyhow::anyhow!("ListenBrainz rate limited: {e}"))
        }
        Err(e) => bail!("ListenBrainz fetch failed: {e}"),
    }
}

/// The pull body over an injected fetcher — the testable seam.
fn pull_with(vault: &Vault, client: &impl Listens, username: &str) -> Result<PullOutcome> {
    let mut state = vault.read_listenbrainz_sync();

    let mut total_written: u64 = 0;
    let mut max_ts_overall = state.watermark;

    // ONE loop for both modes. The API forbids `min_ts` + `max_ts` together,
    // so we always page DOWNWARD by `max_ts` (newest-first) and bound the
    // bottom CLIENT-SIDE by the watermark. A first run (no watermark, treated
    // as 0) drains to the very beginning; an incremental poll drains the whole
    // gap above the watermark — never just the newest page (which would strand
    // every older un-fetched listen below the next tick's bound → data loss).
    let watermark = state.watermark.unwrap_or(0);
    let mut max_ts: Option<i64> = None; // cursor, unset = newest page, advances backward

    loop {
        let body = fetch_page(client, username, None, max_ts)?;
        let (rows, raws) = parse_listens(&body);
        if rows.is_empty() {
            break; // reached the end of history (or the empty profile)
        }
        // The batch's oldest listen decides the boundary. Computed over the
        // WHOLE batch (before the watermark filter) so the stop test is exact.
        let batch_oldest = rows.iter().filter_map(|r| listened_at_of_item(r)).min();

        // Write only listens strictly newer than the watermark; the guid
        // dedupe in `write_rows` stays as a backstop for overlapping pages.
        let (fresh_rows, fresh_raws): (Vec<MediaItem>, Vec<Value>) = rows
            .into_iter()
            .zip(raws)
            .filter(|(item, _)| listened_at_of_item(item).is_none_or(|t| t > watermark))
            .unzip();
        if !fresh_rows.is_empty() {
            let stats = write_rows(vault, &fresh_rows, &fresh_raws)?;
            total_written += stats.listens;
            if let Some(t) = stats.max_ts {
                max_ts_overall = Some(max_ts_overall.map_or(t, |m| m.max(t)));
            }
        }

        // Stop once we've reached (or crossed) the watermark boundary — the
        // fresh rows from this batch are already written above.
        match batch_oldest {
            Some(oldest) if oldest > watermark => {
                // More gap remains below; page strictly before this batch.
                // `max_ts` is exclusive, so `Some(oldest)` advances the cursor
                // even when a batch shares one timestamp — no non-advancing hang.
                max_ts = Some(oldest);
                thread::sleep(REQ_INTERVAL);
            }
            _ => break, // batch_oldest <= watermark (or no parseable ts): done.
        }
    }

    // Advance the watermark to the max listened_at seen (forward-only).
    if let Some(t) = max_ts_overall {
        if state.watermark.is_none_or(|w| t > w) {
            state.watermark = Some(t);
        }
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_listenbrainz_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_written} listens"),
        counts: BTreeMap::from([("listens", total_written)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-listenbrainz-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A real `listens` response: one MATCHED listen (full mbid_mapping +
    /// release_name + submission_client) and one UNMATCHED listen (no
    /// mbid_mapping, no release_name). Newest-first, spanning two months.
    fn sample_page() -> Value {
        serde_json::json!({
            "payload": {
                "count": 2,
                "latest_listen_ts": 1603186824,
                "oldest_listen_ts": 1599300000,
                "user_id": "rob",
                "listens": [
                    {
                        "listened_at": 1603186824,
                        "recording_msid": "d23f4719-9212-49f0-ad08-ddbfbfc50d6f",
                        "track_metadata": {
                            "track_name": "Never Gonna Give You Up",
                            "artist_name": "Rick Astley",
                            "release_name": "Whenever You Need Somebody",
                            "additional_info": {
                                "submission_client": "Rhythmbox ListenBrainz Plugin",
                                "duration_ms": 222000
                            },
                            "mbid_mapping": {
                                "recording_mbid": "98255a8c-017a-4bc7-8dd6-1fa36124572b",
                                "artist_mbids": [
                                    "db92a151-1ac2-438b-bc43-b82e149ddd50",
                                    "00000000-0000-0000-0000-000000000002"
                                ],
                                "release_mbid": "bf9e91ea-8029-4a04-a26a-224e00a83266"
                            }
                        }
                    },
                    {
                        "listened_at": 1599300000,
                        "recording_msid": "aaaaaaaa-1111-2222-3333-444444444444",
                        "track_metadata": {
                            "track_name": "An Unmatched Demo",
                            "artist_name": "Some Bedroom Artist",
                            "additional_info": {}
                        }
                    }
                ]
            }
        })
    }

    #[test]
    fn parses_matched_and_unmatched_listens() {
        let (rows, raws) = parse_listens(&sample_page());
        assert_eq!(rows.len(), 2);
        assert_eq!(raws.len(), 2);

        // MATCHED listen: full mbid extras + detail.
        let matched = &rows[0];
        assert_eq!(matched.title, "Never Gonna Give You Up");
        assert_eq!(matched.subtitle, "Rick Astley");
        assert_eq!(matched.detail, "Whenever You Need Somebody");
        assert_eq!(matched.source, "listenbrainz");
        assert_eq!(matched.category, "music");
        assert_eq!(matched.kind, "play");
        assert_eq!(matched.seconds, 0, "events not durations");
        assert_eq!(
            matched.guid,
            "lb-1603186824-d23f4719-9212-49f0-ad08-ddbfbfc50d6f"
        );
        // listened_at 1603186824 = 2020-10-20T09:40:24Z; ts carries local offset.
        assert_eq!(
            DateTime::parse_from_rfc3339(&matched.ts).unwrap().timestamp(),
            1603186824
        );
        assert_eq!(
            matched.extra.get("recording_mbid"),
            Some(&Value::String("98255a8c-017a-4bc7-8dd6-1fa36124572b".into()))
        );
        assert_eq!(
            matched.extra.get("release_mbid"),
            Some(&Value::String("bf9e91ea-8029-4a04-a26a-224e00a83266".into()))
        );
        // artist_mbids: the ARRAY, comma-joined into one string.
        assert_eq!(
            matched.extra.get("artist_mbids"),
            Some(&Value::String(
                "db92a151-1ac2-438b-bc43-b82e149ddd50,00000000-0000-0000-0000-000000000002".into()
            ))
        );
        assert_eq!(
            matched.extra.get("recording_msid"),
            Some(&Value::String("d23f4719-9212-49f0-ad08-ddbfbfc50d6f".into()))
        );
        assert_eq!(
            matched.extra.get("submission_client"),
            Some(&Value::String("Rhythmbox ListenBrainz Plugin".into()))
        );

        // UNMATCHED listen: no mbid_mapping → mbid-derived keys OMITTED (not
        // ""/null), no detail; but title/subtitle/ts/guid/seconds=0 all hold.
        let unmatched = &rows[1];
        assert_eq!(unmatched.title, "An Unmatched Demo");
        assert_eq!(unmatched.subtitle, "Some Bedroom Artist");
        assert_eq!(unmatched.detail, "", "no release_name → empty detail omitted on write");
        assert_eq!(unmatched.seconds, 0);
        assert_eq!(
            unmatched.guid,
            "lb-1599300000-aaaaaaaa-1111-2222-3333-444444444444"
        );
        assert!(
            unmatched.extra.get("recording_mbid").is_none(),
            "unmatched: recording_mbid omitted, not empty"
        );
        assert!(unmatched.extra.get("artist_mbids").is_none());
        assert!(unmatched.extra.get("release_mbid").is_none());
        // …but the always-present provenance survives.
        assert_eq!(
            unmatched.extra.get("recording_msid"),
            Some(&Value::String("aaaaaaaa-1111-2222-3333-444444444444".into()))
        );
        assert!(
            unmatched.extra.get("submission_client").is_none(),
            "empty additional_info → no submission_client"
        );
    }

    /// A one-page-then-empty fetcher: returns `page` on the first call, then
    /// an empty listens array (terminates both backfill and incremental).
    struct StubClient {
        page: Value,
        calls: std::cell::RefCell<u32>,
    }
    impl StubClient {
        fn new(page: Value) -> Self {
            StubClient { page, calls: std::cell::RefCell::new(0) }
        }
    }
    impl Listens for StubClient {
        fn listens(
            &self,
            _user: &str,
            _count: u32,
            _min_ts: Option<i64>,
            _max_ts: Option<i64>,
        ) -> Result<Value, FetchError> {
            let mut c = self.calls.borrow_mut();
            *c += 1;
            if *c == 1 {
                Ok(self.page.clone())
            } else {
                Ok(serde_json::json!({ "payload": { "count": 0, "listens": [] } }))
            }
        }
    }

    #[test]
    fn writes_partitioned_layers_dedupes_and_advances_cursor() {
        let v = temp_vault("store");
        let client = StubClient::new(sample_page());

        let out = pull_with(&v, &client, "rob").unwrap();
        assert_eq!(out.counts.get("listens"), Some(&2));

        // Contract layer, partitioned by the listen's month.
        let oct =
            std::fs::read_to_string(v.root().join("media/plays/listenbrainz/2020-10.jsonl")).unwrap();
        assert_eq!(oct.lines().count(), 1, "the matched listen lands in Oct");
        let sep =
            std::fs::read_to_string(v.root().join("media/plays/listenbrainz/2020-09.jsonl")).unwrap();
        assert_eq!(sep.lines().count(), 1, "the unmatched listen lands in Sep");
        // The unmatched contract LINE must not carry empty mbid keys.
        assert!(!sep.contains("recording_mbid"), "no empty mbid key on the unmatched line");
        assert!(!sep.contains("release_mbid"));
        assert!(!sep.contains("artist_mbids"));

        // Raw layer mirrors the partitioning under raw/, verbatim objects.
        let oct_raw =
            std::fs::read_to_string(v.root().join("media/plays/listenbrainz/raw/2020-10.jsonl"))
                .unwrap();
        assert_eq!(oct_raw.lines().count(), 1);
        assert!(oct_raw.contains("\"mbid_mapping\""), "raw keeps the full track_metadata");
        assert!(oct_raw.contains("\"duration_ms\":222000"));
        let sep_raw =
            std::fs::read_to_string(v.root().join("media/plays/listenbrainz/raw/2020-09.jsonl"))
                .unwrap();
        assert!(!sep_raw.contains("mbid_mapping"), "unmatched raw has no mapping either");

        // Cursor advanced to the max listened_at written.
        let state = v.read_listenbrainz_sync();
        assert_eq!(state.watermark, Some(1603186824));
        assert!(state.updated.is_some());

        // Re-run with the same input → guid dedupe, no duplicate contract rows.
        let client2 = StubClient::new(sample_page());
        let again = pull_with(&v, &client2, "rob").unwrap();
        assert_eq!(again.counts.get("listens"), Some(&0), "all guids already stored");
        let oct2 =
            std::fs::read_to_string(v.root().join("media/plays/listenbrainz/2020-10.jsonl")).unwrap();
        assert_eq!(oct, oct2, "contract file byte-identical after re-run");

        // And the unified media stream sees the listen via the contract arm.
        let day = v.media_timeline("2020-10-20").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "listenbrainz");
        assert_eq!(day[0].title, "Never Gonna Give You Up");
        assert_eq!(day[0].category, "music");
    }

    /// A fetcher that honors `max_ts` (exclusive, newest-first), serving a
    /// fixed pool of `listened_at` timestamps in pages of [`PAGE_SIZE`] — so a
    /// gap larger than one page is actually paged down, exactly like the API.
    /// Each timestamp becomes a minimal-but-valid listen with a unique guid.
    struct PagingStubClient {
        /// All `listened_at` values present, newest-first.
        all_desc: Vec<i64>,
    }
    impl PagingStubClient {
        fn new(mut listened_ats: Vec<i64>) -> Self {
            listened_ats.sort_unstable();
            listened_ats.dedup();
            listened_ats.reverse(); // newest-first
            PagingStubClient { all_desc: listened_ats }
        }
        fn listen(ts: i64) -> Value {
            serde_json::json!({
                "listened_at": ts,
                "recording_msid": format!("msid-{ts}"),
                "track_metadata": {
                    "track_name": format!("Track {ts}"),
                    "artist_name": "An Artist"
                }
            })
        }
    }
    impl Listens for PagingStubClient {
        fn listens(
            &self,
            _user: &str,
            count: u32,
            min_ts: Option<i64>,
            max_ts: Option<i64>,
        ) -> Result<Value, FetchError> {
            // The unified pull only ever pages by `max_ts`; assert the API
            // contract (never both bounds) holds.
            assert!(min_ts.is_none(), "unified loop must not send min_ts");
            // newest-first, strictly older than max_ts (exclusive), capped at count.
            let page: Vec<Value> = self
                .all_desc
                .iter()
                .copied()
                .filter(|t| max_ts.is_none_or(|m| *t < m))
                .take(count as usize)
                .map(Self::listen)
                .collect();
            Ok(serde_json::json!({ "payload": { "count": page.len(), "listens": page } }))
        }
    }

    #[test]
    fn incremental_drains_a_multi_page_gap_without_stranding_listens() {
        // Regression for the data-loss defect: with a watermark already set,
        // the OLD incremental path fetched ONE newest-first page and broke,
        // stranding every listen below it. Here T0 is the watermark and T1..T250
        // (all > T0) accumulated since — 250 listens, 2.5 pages of 100.
        let v = temp_vault("gap");
        let t0: i64 = 1_600_000_000; // watermark (UTC: 2020-09-13)
        // Seed the watermark so this is the INCREMENTAL path, not a first run.
        v.write_listenbrainz_sync(&SyncState {
            watermark: Some(t0),
            updated: Some("2020-09-13T00:00:00+00:00".into()),
        })
        .unwrap();

        let newest = t0 + 250; // T250
        let gap: Vec<i64> = (1..=250).map(|i| t0 + i).collect(); // T1..T250
        let client = PagingStubClient::new(gap);

        let out = pull_with(&v, &client, "rob").unwrap();
        // ALL 250 are written — none stranded. (OLD behavior wrote only ~100.)
        assert_eq!(
            out.counts.get("listens"),
            Some(&250),
            "the whole gap must drain in one pull, not just the newest page"
        );

        // Count contract rows actually on disk across every partition.
        let stream = v.stream(DIR, Partition::Month);
        let mut on_disk = 0usize;
        for key in stream.partitions().unwrap() {
            on_disk += stream.read::<MediaItem>(&key).unwrap().len();
        }
        assert_eq!(on_disk, 250, "250 contract rows persisted, no gaps");

        // Watermark advanced forward-only to the newest listen written (T250).
        let state = v.read_listenbrainz_sync();
        assert_eq!(state.watermark, Some(newest));

        // Idempotent: a second pull now finds nothing new (watermark == newest).
        let again = pull_with(&v, &client, "rob").unwrap();
        assert_eq!(again.counts.get("listens"), Some(&0), "no re-fetch past the boundary");
    }

    #[test]
    fn connection_exposes_token_paste_and_disconnect_forgets_username() {
        assert!(CONNECTION.method("token-paste").is_some());
        let v = temp_vault("conn");
        // Verify offline via the stub (no network): a 200 with any payload.
        let client = StubClient::new(sample_page());
        connect_with(&v, &client, "rob").unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.configured, "keyless: always configured");
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "rob");
        assert_eq!(status.accounts[0].key, "listenbrainz");
        assert!(!status.accounts[0].needs_reconnect);
        def_disconnect(&v, "listenbrainz").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    /// A fetcher that always reports the user doesn't exist.
    struct NotFoundClient;
    impl Listens for NotFoundClient {
        fn listens(
            &self,
            _user: &str,
            _count: u32,
            _min_ts: Option<i64>,
            _max_ts: Option<i64>,
        ) -> Result<Value, FetchError> {
            Err(FetchError::NotFound)
        }
    }

    #[test]
    fn empty_username_rejected_unknown_user_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        let client = StubClient::new(sample_page());
        // Empty/whitespace username is rejected before any fetch.
        assert!(connect_with(&v, &client, "   ").is_err());
        // A 404 from the keyless probe is a clear connect-time error.
        let err = connect_with(&v, &NotFoundClient, "ghost").unwrap_err().to_string();
        assert!(err.contains("could not find"), "clear not-found error: {err}");
        // Not connected: pull errors clearly (no api_key concept at all).
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn sync_state_back_compat() {
        // An older cursor line (watermark only, no `updated`) must still
        // deserialize — and a bare `{}` (fresh) too.
        let old: SyncState = serde_json::from_str(r#"{"watermark":123}"#).unwrap();
        assert_eq!(old.watermark, Some(123));
        assert!(old.updated.is_none());
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermark.is_none() && empty.updated.is_none());
    }
}
