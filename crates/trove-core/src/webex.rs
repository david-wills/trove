//! Webex (Cisco) meeting transcripts — Periodic OAuth cloud pull.
//!
//! Polls `GET /v1/meetingTranscripts` (Webex REST API) and writes:
//!
//! - **meetings contract** `meetings/webex/YYYY-MM.jsonl`: one [`Meeting`] row
//!   per transcript, upserted by `guid` (the transcript `id` field, which is
//!   stable and unique per transcript). [`crate::meetings::Meeting`] is the
//!   shared contract; we do NOT bind a new one.
//!
//! - **raw firehose** `meetings/webex/raw/YYYY-MM.jsonl`: verbatim transcript
//!   list objects, full fidelity.
//!
//! - **VTT sidecars** `meetings/webex/raw/transcripts/<id>.vtt`: the raw VTT
//!   bytes, written only when the `vttDownloadLink` succeeds. The contract row
//!   sets `transcript_ref` to the sidecar path.
//!
//! **Download links expire** — `vttDownloadLink`/`txtDownloadLink` are never
//! persisted; they are fetched fresh from the list endpoint on each poll and
//! used immediately.
//!
//! ## API shape (confirmed from developer.webex.com example)
//!
//! `GET /v1/meetingTranscripts?from=…&to=…` returns:
//! ```json
//! { "items": [ {
//!     "id": "…",            // stable transcript id — our guid
//!     "startTime": "…",     // ISO 8601 UTC
//!     "meetingId": "…",     // meeting instance id
//!     "meetingTopic": "…",  // meeting title
//!     "siteUrl": "…",       // Webex site domain
//!     "scheduledMeetingId": "…",
//!     "meetingSeriesId": "…",
//!     "hostUserId": "…",
//!     "vttDownloadLink": "…",  // expires — never persist
//!     "txtDownloadLink": "…",  // expires — never persist
//!     "status": "available"
//! } ] }
//! ```
//! Pagination via RFC 5988 `Link: <url>; rel="next"` response header.
//!
//! ## Cursor
//!
//! `.trove/webex-sync.json` holds `from` (date, `YYYY-MM-DD`), the start of
//! the next poll window. Advances after a full drain. A 2-day overlap prevents
//! missing transcripts whose processing lags their meeting end. First sync goes
//! back [`INITIAL_BACKFILL_DAYS`] days.
//!
//! ## Auth / scope
//!
//! OAuth 2.0 with `meeting:read` scope. The user supplies their own integration
//! credentials (BYO Client ID + Secret registered at developer.webex.com); baked
//! credentials can be compiled in via `TROVE_WEBEX_CLIENT_ID` /
//! `TROVE_WEBEX_CLIENT_SECRET` env vars at build time. Webex issues refresh
//! tokens; the pull refreshes silently on expiry (Zoom pattern).
//!
//! 🔒 Default-off: meeting transcripts are conversation content.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::meetings::Meeting;
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SOURCE: &str = "webex";
const SERVICE: &str = "webex";

const CONTRACT_DIR: &str = "meetings/webex";
const RAW_DIR: &str = "meetings/webex/raw";
const TRANSCRIPT_DIR: &str = "meetings/webex/raw/transcripts";

const SYNC_FILE: &str = ".trove/webex-sync.json";

const API_BASE: &str = "https://webexapis.com/v1";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Seconds between periodic syncs (every 30 minutes — transcripts are
/// low-volume; Webex processes them asynchronously, so a shorter window
/// would rarely find new ones).
pub const WEBEX_SYNC_SECS: u64 = 1800;

/// Max items per page when we request a limit (Webex default is 100).
const PAGE_LIMIT: u32 = 100;

/// How many days back to start on a first sync.
const INITIAL_BACKFILL_DAYS: i64 = 365;

/// Overlap: re-cover the last N days on each poll so transcripts that finish
/// processing hours after a meeting ends are not permanently missed.
const CURSOR_OVERLAP_DAYS: i64 = 2;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static WEBEX: Provider = Provider {
    service: SERVICE,
    display_name: "Webex",
    auth_url: "https://webexapis.com/v1/authorize",
    token_url: "https://webexapis.com/v1/access_token",
    // meeting:read — list and download meeting transcripts.
    scopes: "meeting:read",
    // Assigned unique production redirect port for webex (INDEX #274).
    redirect_port: 38854,
    use_pkce: false,
    // Webex expects client_id / client_secret as form body on the token endpoint
    // (not HTTP Basic auth).
    basic_auth: false,
    default_client_id: option_env!("TROVE_WEBEX_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_WEBEX_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured =
        vault.load_sync_app(WEBEX.service)?.is_some() || WEBEX.default_credentials().is_some();
    let accounts = match vault.load_sync_token(WEBEX.service)? {
        Some(token) => vec![ConnectedAccount {
            key: WEBEX.service.to_string(),
            label: WEBEX.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Webex issues refresh tokens; an expired token with a refresh
            // token does NOT need reconnect — the pull refreshes silently.
            needs_reconnect: token.expired() && token.refresh_token.is_none(),
            extra: BTreeMap::new(),
        }],
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "webex",
    display_name: "Webex",
    methods: &[ConnectMethod::OAuth {
        provider: &WEBEX,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["webex"],
    setup: &[
        "Sign in at developer.webex.com and create an Integration (OAuth type).",
        "Add the scope: meeting:read.",
        "Set the OAuth redirect URL to http://localhost:38854/callback — must match exactly.",
        "Paste the integration's Client ID and Client Secret here. They're saved, so every \
         future connect is just a login.",
    ],
};

/// Interactive OAuth connect: opens the consent page, waits for the redirect,
/// saves the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(WEBEX.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(WEBEX.service)?
            .or_else(|| WEBEX.default_credentials())
            .context(
                "no Webex app credentials — register an Integration at developer.webex.com and \
                 enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&WEBEX, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(WEBEX.service, &token)?;
    Ok(token)
}

/// Refresh an expired token silently. On refresh failure, delete the stored
/// token so the user sees a reconnect prompt.
fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(WEBEX.service)?
        .or_else(|| WEBEX.default_credentials())
        .context("Webex token expired and no app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&WEBEX, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(WEBEX.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(WEBEX.service)?;
            bail!("Webex token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct SyncState {
    /// `YYYY-MM-DD` start date of the next poll window (UTC). Advances after
    /// each successful drain. On first sync, set to `INITIAL_BACKFILL_DAYS`
    /// ago.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    /// RFC3339 UTC timestamp of the newest transcript we have stored (high-
    /// water mark for last_data display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_transcript_ts: Option<String>,
}

impl Vault {
    fn read_webex_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_webex_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_webex_sync().last_transcript_ts.filter(|s| !s.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "webex synced — {} meetings, {} transcripts",
                    c("meetings"),
                    c("transcripts"),
                )
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("webex sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub already there).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "webex",
        name: "Webex",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting transcripts are conversation content.
        default_on: false,
        description: "Syncs Webex meeting transcripts into the unified meetings store via \
                      the official Webex REST API (webexapis.com/v1/meetingTranscripts), \
                      every 30 minutes. Both Webex Assistant and Cisco AI Assistant \
                      transcripts are included. Requires a free or paid Webex account \
                      with an OAuth integration.",
        domain: "meetings",
        vault_path: "meetings/webex/",
        toggleable: true,
        setup: &[
            "Connect your Webex account above via OAuth.",
            "Each sync pulls new transcripts; VTT files are stored as sidecar \
             artifacts under meetings/webex/raw/transcripts/.",
        ],
        caveats: "Meeting transcripts are conversation content, so this source is off by \
                  default — turn it on deliberately. VTT download links expire and are \
                  re-fetched on every sync; only available transcripts are downloaded.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(WEBEX_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("webex"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait so tests run fully offline.

/// Status-level fetch errors with enough granularity to surface good messages.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    NotFound,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The API calls the pull needs. A trait so tests drive the logic with
/// fixtures, never the network.
trait WebexApi {
    /// `GET /meetingTranscripts?from=…&to=…&limit=…` — first page.
    /// Returns (json body, Option<next_url> from Link header).
    fn list_transcripts(
        &self,
        token: &str,
        from: &str,
        to: &str,
    ) -> Result<(Value, Option<String>), FetchError>;

    /// GET the given `next_url` (Link header continuation).
    fn list_transcripts_next(
        &self,
        token: &str,
        url: &str,
    ) -> Result<(Value, Option<String>), FetchError>;

    /// Download the VTT transcript bytes from the given URL.
    fn download_vtt(&self, token: &str, url: &str) -> Result<String, FetchError>;
}

/// Live implementation using ureq + rustls.
struct WebexClient;

impl WebexApi for WebexClient {
    fn list_transcripts(
        &self,
        token: &str,
        from: &str,
        to: &str,
    ) -> Result<(Value, Option<String>), FetchError> {
        let url = format!(
            "{API_BASE}/meetingTranscripts?from={from}&to={to}&limit={PAGE_LIMIT}"
        );
        http_get_json_with_next(&url, token)
    }

    fn list_transcripts_next(
        &self,
        token: &str,
        url: &str,
    ) -> Result<(Value, Option<String>), FetchError> {
        http_get_json_with_next(url, token)
    }

    fn download_vtt(&self, token: &str, url: &str) -> Result<String, FetchError> {
        let resp = ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .call();
        match resp {
            Ok(r) => r.into_string().map_err(|e| FetchError::Other(e.to_string())),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(300).collect();
                Err(FetchError::Other(format!("HTTP {code}: {snippet}")))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

/// `GET url` with Bearer auth; returns (json body, Option<next url from Link header>).
fn http_get_json_with_next(
    url: &str,
    bearer: &str,
) -> Result<(Value, Option<String>), FetchError> {
    let resp = ureq::get(url)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Accept", "application/json")
        .call();
    match resp {
        Ok(r) => {
            // Extract the RFC 5988 Link: <url>; rel="next" header if present.
            let next = r.header("Link").and_then(parse_link_next);
            let body: Value =
                r.into_json().map_err(|e| FetchError::Other(format!("parse: {e}")))?;
            Ok((body, next))
        }
        Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
        Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
        Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            Err(FetchError::Other(format!("HTTP {code}: {snippet}")))
        }
        Err(e) => Err(FetchError::Other(e.to_string())),
    }
}

/// Parse a `Link` header for the `rel="next"` URL (RFC 5988).
/// Input example: `<https://webexapis.com/v1/…>; rel="next"`
fn parse_link_next(header: &str) -> Option<String> {
    // The Link header may contain multiple comma-separated entries:
    // `<url1>; rel="next", <url2>; rel="last"`
    for part in header.split(',') {
        let part = part.trim();
        // Each part looks like: `<url>; rel="next"` or `<url>; rel=next`
        if part.contains("rel=\"next\"") || part.contains("rel=next") {
            if let Some(start) = part.find('<') {
                if let Some(end) = part[start..].find('>') {
                    let url = &part[start + 1..start + end];
                    if !url.is_empty() {
                        return Some(url.to_string());
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Raw row (verbatim API transcript object, full fidelity).

/// One raw API transcript object in `meetings/webex/raw/YYYY-MM.jsonl`.
/// Round-trips byte-identically; `id` (dedup key) and `startTime` (partition
/// key) are read back via accessors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawTranscript {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawTranscript {
    fn id(&self) -> String {
        self.fields.get("id").and_then(Value::as_str).unwrap_or("").to_string()
    }

    fn start_time(&self) -> &str {
        self.fields.get("startTime").and_then(Value::as_str).unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Pure mapping helpers.

/// Pull a string field, trimmed; `None` when missing/non-string/empty.
fn str_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Convert an ISO 8601 UTC string to RFC3339 local time. Unparseable values
/// pass through verbatim (the fathom/zoom idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Vault-relative transcript sidecar path.
fn transcript_ref(id: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{id}.vtt")
}

/// Map a Webex `meetingTranscripts` API object → a [`Meeting`] contract row.
/// Returns `None` when the object has no `id` (can't dedup) or no `startTime`
/// (can't partition).
///
/// Field mapping:
/// - `id`           → `guid` (stable transcript id — the dedup key)
/// - `startTime`    → `ts`, `started` (UTC → local RFC3339)
/// - `meetingTopic` → `title`
/// - `meetingId`    → `extra["meetingId"]` (the meeting instance id)
/// - `meetingSeriesId` → `extra["meetingSeriesId"]`
/// - `siteUrl`      → `extra["siteUrl"]`
/// - `hostUserId`   → `extra["hostUserId"]`
/// - `status`       → `extra["status"]`
/// platform is always "webex".
fn meeting_from_transcript(obj: &Value) -> Option<Meeting> {
    let id = str_opt(obj, "id")?;
    let start_raw = str_opt(obj, "startTime")?;
    let ts = to_local(&start_raw);

    let mut m = Meeting::new(SOURCE, &id, ts.clone());
    m.started = ts;
    m.platform = "webex".to_string();

    if let Some(title) = str_opt(obj, "meetingTopic") {
        m.title = title;
    }

    // source-specific fields that don't map to contract columns → extra.
    for &key in &["meetingId", "meetingSeriesId", "scheduledMeetingId", "siteUrl", "hostUserId", "status"] {
        if let Some(val) = str_opt(obj, key) {
            m.extra.insert(key.to_string(), Value::from(val));
        }
    }

    // Note: vttDownloadLink / txtDownloadLink are intentionally NOT stored
    // here — they expire and must be re-fetched from the list endpoint each
    // poll. They are used only transiently during the pull, then discarded.

    Some(m)
}

// ---------------------------------------------------------------------------
// Upsert helpers (same pattern as fathom.rs / zoom.rs).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

fn upsert_raw(vault: &Vault, rows: Vec<RawTranscript>) -> Result<u64> {
    upsert_partition(
        vault,
        RAW_DIR,
        rows,
        |r| r.start_time().to_string(),
        |r| r.id(),
    )
}

fn upsert_partition<T, FT, FG>(
    vault: &Vault,
    dir: &str,
    rows: Vec<T>,
    ts_of: FT,
    guid_of: FG,
) -> Result<u64>
where
    T: Serialize + serde::de::DeserializeOwned,
    FT: Fn(&T) -> String,
    FG: Fn(&T) -> String,
{
    let stream = vault.stream(dir, Partition::Month);
    let mut by_month: BTreeMap<String, Vec<T>> = BTreeMap::new();
    for r in rows {
        let ts = ts_of(&r);
        let key = Partition::Month
            .key(&ts)
            .with_context(|| format!("webex: ts {ts:?} has no month (dir {dir})"))?
            .to_string();
        by_month.entry(key).or_default().push(r);
    }
    let mut new_count = 0u64;
    for (month, fresh) in by_month {
        let mut existing: Vec<T> = stream.read(&month)?;
        let mut idx: HashMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (guid_of(r), i)).collect();
        for r in fresh {
            let g = guid_of(&r);
            match idx.get(&g).copied() {
                Some(i) => existing[i] = r,
                None => {
                    idx.insert(g, existing.len());
                    existing.push(r);
                    new_count += 1;
                }
            }
        }
        existing.sort_by(|a, b| ts_of(a).cmp(&ts_of(b)).then_with(|| guid_of(a).cmp(&guid_of(b))));
        vault.write_snapshot(&format!("{dir}/{month}.jsonl"), &existing)?;
    }
    Ok(new_count)
}

// ---------------------------------------------------------------------------
// Pull entry point.

/// Public entry point — resolve credentials, refresh if needed, then pull.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Webex is not connected — add your credentials in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = WebexClient;
    pull_with(vault, &client, &token.access_token)
}

/// Pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl WebexApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_webex_sync();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    // Compute the poll window. Overlap floor prevents missing late-processed
    // transcripts; first sync uses INITIAL_BACKFILL_DAYS.
    let overlap_floor = {
        let today_dt = chrono::Utc::now().date_naive();
        (today_dt - chrono::Duration::days(CURSOR_OVERLAP_DAYS))
            .format("%Y-%m-%d")
            .to_string()
    };
    let stored_from = state.from.clone().unwrap_or_else(|| {
        let start = chrono::Utc::now() - chrono::Duration::days(INITIAL_BACKFILL_DAYS);
        start.format("%Y-%m-%d").to_string()
    });
    // Use whichever start is earlier: stored cursor or the overlap floor.
    let from = if stored_from <= overlap_floor { stored_from } else { overlap_floor.clone() };

    // Webex `to` date is inclusive; cap to today.
    let to = today.clone();

    // Guard: if from is somehow in the future, nothing to fetch.
    if from > today {
        let mut counts = BTreeMap::new();
        counts.insert("meetings", 0u64);
        counts.insert("transcripts", 0u64);
        counts.insert("raw", 0u64);
        return Ok(PullOutcome {
            headline: "Webex: up to date".to_string(),
            counts,
        });
    }

    // --- 1. drain ALL pages (RFC 5988 Link header pagination) ---------------
    let mut items: Vec<Value> = Vec::new();
    let (first_body, mut next_url) = api
        .list_transcripts(token, &from, &to)
        .map_err(fetch_err)?;
    collect_items(first_body, &mut items);

    while let Some(url) = next_url {
        let (body, next) = api.list_transcripts_next(token, &url).map_err(fetch_err)?;
        collect_items(body, &mut items);
        next_url = next;
    }

    // --- 2. dedupe by id (a transcript may appear on two pages edge-of-window)
    let mut seen_ids: HashMap<String, Value> = HashMap::new();
    for item in items {
        if let Some(id) = item.get("id").and_then(Value::as_str).map(str::to_string) {
            seen_ids.entry(id).or_insert(item);
        }
    }

    // --- 3. build contract + raw rows; download VTT sidecars ----------------
    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawTranscript> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_ts: Option<String> = state.last_transcript_ts.clone();

    for (id, obj) in &seen_ids {
        // Raw firehose: verbatim object (includes the expiring download links
        // at the time of the poll — raw is a snapshot, not a durable link store).
        if let Some(fields) = obj.as_object() {
            raw_rows.push(RawTranscript { fields: fields.clone() });
        }

        let Some(mut row) = meeting_from_transcript(obj) else {
            continue;
        };

        // Download VTT sidecar if the status is available.
        let status = str_opt(obj, "status").unwrap_or_default();
        if status == "available" {
            if let Some(vtt_url) = str_opt(obj, "vttDownloadLink") {
                match api.download_vtt(token, &vtt_url) {
                    Ok(vtt_text) => {
                        let ref_path = transcript_ref(id);
                        // Write VTT raw bytes (not parsed into utterances — the
                        // Webex VTT shape is standard WebVTT, zoom.rs parse_vtt
                        // could be reused, but the raw bytes are sufficient here;
                        // the sidecar is the transcript artifact).
                        let abs = vault.resolve(&ref_path)?;
                        if let Some(parent) = abs.parent() {
                            std::fs::create_dir_all(parent).with_context(|| {
                                format!("webex: create transcript dir {parent:?}")
                            })?;
                        }
                        std::fs::write(&abs, vtt_text.as_bytes())
                            .with_context(|| format!("webex: write VTT sidecar {id}"))?;
                        row.transcript_ref = ref_path;
                        transcripts_written += 1;
                    }
                    Err(FetchError::NotFound) => {
                        // Transcript not yet available; row still lands without ref.
                    }
                    Err(_) => {
                        // Non-fatal: the meeting row still lands.
                    }
                }
            }
        }

        // Advance high-water mark (use the UTC startTime lexically — UTC ISO
        // strings sort correctly without parsing).
        if let Some(ts_raw) = str_opt(obj, "startTime") {
            newest_ts = max_ts(newest_ts, ts_raw);
        }

        contract_rows.push(row);
    }

    // --- 4. persist ---------------------------------------------------------
    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    // Advance the cursor: next poll starts from today (clamped to overlap
    // floor via the logic above, so late-processing transcripts are still
    // covered on future polls).
    let next_from = {
        let candidate = advance_date(&to, 1);
        if candidate <= overlap_floor { candidate } else { overlap_floor.clone() }
    };
    state.from = Some(next_from);
    state.last_transcript_ts = newest_ts;
    vault.write_webex_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("meetings", contract_new);
    counts.insert("transcripts", transcripts_written);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!(
            "Webex synced — {contract_new} new meetings, {transcripts_written} transcripts"
        ),
        counts,
    })
}

/// Collect `items` out of a list response body into `dest`.
fn collect_items(body: Value, dest: &mut Vec<Value>) {
    if let Some(items) = body.get("items").and_then(Value::as_array) {
        dest.extend(items.clone());
    }
}

/// Return the later of two UTC ISO strings (lexical compare is correct for `…Z` form).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev.as_str() >= candidate.as_str() => Some(prev),
        _ => Some(candidate),
    }
}

/// Add `days` to a `YYYY-MM-DD` string. Returns the original on parse failure.
fn advance_date(date: &str, days: i64) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| (d + chrono::Duration::days(days)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// Map a [`FetchError`] to an anyhow error with a clear reconnect hint for 401.
fn fetch_err(e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => {
            anyhow::anyhow!("Webex rejected the token (401) — reconnect from the Integrations tab")
        }
        FetchError::RateLimited => {
            anyhow::anyhow!("Webex rate limit hit (429) — will retry on the next sync")
        }
        other => anyhow::anyhow!("Webex fetch failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-webex-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — from the confirmed developer.webex.com example JSON.

    /// A transcript object as returned by GET /meetingTranscripts (real API shape).
    fn transcript_available(id: &str, start: &str) -> Value {
        serde_json::json!({
            "id": id,
            "startTime": start,
            "meetingId": "0ed74a1c0551494fb7a04e2881bf50ae_I_166022169160077044",
            "meetingTopic": "Q3 Roadmap Sync",
            "siteUrl": "example.webex.com",
            "scheduledMeetingId": "0ed74a1c0551494fb7a04e2881bf50ae_20210401T232500Z",
            "meetingSeriesId": "0ed74a1c0551494fb7a04e2881bf50ae",
            "hostUserId": "Y2lzY29zcGFyazovL3VzL1BFT1BMRS83QkFCQkU5OS1CNDNFLTREM0YtOTE0Ny1BMUU5RDQ2QzlDQTA",
            "vttDownloadLink": "https://example.webex.com/v1/meetingTranscripts/abc/download?format=vtt",
            "txtDownloadLink": "https://example.webex.com/v1/meetingTranscripts/abc/download?format=txt",
            "status": "available"
        })
    }

    /// A transcript where status is "deleted" (no download should be attempted).
    fn transcript_deleted(id: &str, start: &str) -> Value {
        let mut t = transcript_available(id, start);
        t["status"] = serde_json::json!("deleted");
        t
    }

    /// A minimal transcript (only required fields).
    fn transcript_minimal(id: &str, start: &str) -> Value {
        serde_json::json!({
            "id": id,
            "startTime": start,
            "meetingTopic": "Standup",
            "status": "available",
            "vttDownloadLink": "https://example.webex.com/v1/meetingTranscripts/min/download?format=vtt"
        })
    }

    // Minimal VTT content for test sidecars.
    const SAMPLE_VTT: &str = "WEBVTT\n\n00:00:01.000 --> 00:00:05.000\n<v Alice>Hello there.</v>\n\n00:00:05.000 --> 00:00:10.000\n<v Bob>Let's begin.</v>\n";

    // -----------------------------------------------------------------------
    // Mock API.
    //
    // `add_page` registers the first-page response for ANY from/to window.
    // Tests don't assert on the exact query dates (the pull computes them from
    // today's date, which varies); they only care about the returned items and
    // the resulting vault writes. `add_next_page` registers continuation pages
    // by exact URL (for pagination tests).

    struct MockApi {
        /// First-page items (any from/to → return these items + next_url).
        /// Index 0 is returned for the first list_transcripts call.
        first_pages: RefCell<Vec<(Vec<Value>, Option<String>)>>,
        /// Continuation pages: url → (items, next_url).
        next_pages: RefCell<Vec<(String, Value, Option<String>)>>,
        /// VTT content to return for downloads (url-contains pattern → content).
        vtts: RefCell<Vec<(String, Result<String, ()>)>>,
        /// Requests made (for assertion).
        requests: RefCell<Vec<String>>,
        /// Call count for list_transcripts.
        list_call_count: RefCell<usize>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                first_pages: RefCell::new(Vec::new()),
                next_pages: RefCell::new(Vec::new()),
                vtts: RefCell::new(Vec::new()),
                requests: RefCell::new(Vec::new()),
                list_call_count: RefCell::new(0),
            }
        }

        /// Register the first-page response (any from/to).
        fn add_page(&self, items: Vec<Value>, next: Option<&str>) {
            self.first_pages.borrow_mut().push((items, next.map(str::to_string)));
        }

        fn add_next_page(&self, url: &str, items: Vec<Value>, next: Option<&str>) {
            let body = serde_json::json!({ "items": items });
            self.next_pages.borrow_mut().push((url.to_string(), body, next.map(str::to_string)));
        }

        fn add_vtt(&self, url_contains: &str, content: &str) {
            self.vtts.borrow_mut().push((url_contains.to_string(), Ok(content.to_string())));
        }

        fn add_vtt_404(&self, url_contains: &str) {
            self.vtts.borrow_mut().push((url_contains.to_string(), Err(())));
        }

        fn requested(&self, needle: &str) -> bool {
            self.requests.borrow().iter().any(|r| r.contains(needle))
        }
    }

    impl WebexApi for MockApi {
        fn list_transcripts(
            &self,
            _token: &str,
            from: &str,
            to: &str,
        ) -> Result<(Value, Option<String>), FetchError> {
            self.requests.borrow_mut().push(format!("list:from={from}&to={to}"));
            let idx = *self.list_call_count.borrow();
            *self.list_call_count.borrow_mut() = idx + 1;
            if let Some((items, next)) = self.first_pages.borrow().get(idx).cloned() {
                let body = serde_json::json!({ "items": items });
                return Ok((body, next));
            }
            Ok((serde_json::json!({"items": []}), None))
        }

        fn list_transcripts_next(
            &self,
            _token: &str,
            url: &str,
        ) -> Result<(Value, Option<String>), FetchError> {
            self.requests.borrow_mut().push(format!("next:{url}"));
            for (u, body, next) in self.next_pages.borrow().iter() {
                if u == url {
                    return Ok((body.clone(), next.clone()));
                }
            }
            Ok((serde_json::json!({"items": []}), None))
        }

        fn download_vtt(
            &self,
            _token: &str,
            url: &str,
        ) -> Result<String, FetchError> {
            self.requests.borrow_mut().push(format!("vtt:{url}"));
            for (pattern, result) in self.vtts.borrow().iter() {
                if url.contains(pattern.as_str()) {
                    return result
                        .as_ref()
                        .map(|s| s.clone())
                        .map_err(|_| FetchError::NotFound);
                }
            }
            Err(FetchError::NotFound)
        }
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn meeting_from_transcript_maps_all_fields() {
        let obj = transcript_available(
            "8ce1f918-c138-4041-bb4a-5e01eeaadedb_M_0fc6ec11d9b6c909e04e1f6c76accda9",
            "2020-06-02T20:30:15.042Z",
        );
        let m = meeting_from_transcript(&obj).unwrap();
        assert_eq!(m.source, "webex");
        // guid = transcript id (stable, unique).
        assert_eq!(
            m.guid,
            "8ce1f918-c138-4041-bb4a-5e01eeaadedb_M_0fc6ec11d9b6c909e04e1f6c76accda9"
        );
        assert_eq!(m.title, "Q3 Roadmap Sync");
        assert_eq!(m.platform, "webex");
        // ts / started = startTime → local RFC3339 (same instant as UTC).
        assert_eq!(
            DateTime::parse_from_rfc3339(&m.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2020-06-02T20:30:15.042Z").unwrap().timestamp(),
        );
        assert_eq!(m.ts, m.started);
        // Download links are NOT stored on the contract row (they expire).
        assert_eq!(m.transcript_ref, "", "transcript_ref is set only after VTT download");
        // Source-specific fields → extra.
        assert!(m.extra.contains_key("meetingId"));
        assert_eq!(m.extra["siteUrl"], serde_json::json!("example.webex.com"));
        assert!(m.extra.contains_key("meetingSeriesId"));
        assert!(m.extra.contains_key("hostUserId"));
        assert_eq!(m.extra["status"], serde_json::json!("available"));
        // Expiring links must NOT appear anywhere on the row or in extra.
        let serialized = serde_json::to_string(&m).unwrap();
        assert!(
            !serialized.contains("vttDownload") && !serialized.contains("txtDownload"),
            "expiring download links must not be persisted: {serialized}"
        );
    }

    #[test]
    fn meeting_from_transcript_none_on_missing_id_or_start() {
        // No id.
        assert!(meeting_from_transcript(&serde_json::json!({"startTime": "2020-06-01T00:00:00Z"})).is_none());
        // No startTime.
        assert!(meeting_from_transcript(&serde_json::json!({"id": "abc"})).is_none());
    }

    #[test]
    fn parse_link_next_extracts_url() {
        // Single next entry.
        let h = r#"<https://webexapis.com/v1/meetingTranscripts?cursor=abc>; rel="next""#;
        assert_eq!(
            parse_link_next(h),
            Some("https://webexapis.com/v1/meetingTranscripts?cursor=abc".to_string())
        );
        // Multiple entries — only next is extracted.
        let h2 = r#"<https://example.com/page2>; rel="next", <https://example.com/last>; rel="last""#;
        assert_eq!(parse_link_next(h2), Some("https://example.com/page2".to_string()));
        // No next entry.
        assert_eq!(parse_link_next(r#"<https://example.com/last>; rel="last""#), None);
        assert_eq!(parse_link_next(""), None);
    }

    // -----------------------------------------------------------------------
    // Full pull tests.

    #[test]
    fn full_pull_writes_contract_raw_and_vtt_sidecar() {
        let v = temp_vault("fullpull");
        let api = MockApi::new();
        let t1 = transcript_available("id-abc-123", "2020-06-02T20:30:15Z");
        api.add_page(vec![t1], None);
        api.add_vtt("abc/download", SAMPLE_VTT);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1), "one new contract row");
        assert_eq!(out.counts.get("transcripts"), Some(&1), "one VTT downloaded");
        assert_eq!(out.counts.get("raw"), Some(&1), "one raw row");

        // Contract row in the ts-month partition (June 2020).
        let month_key = Partition::Month.key(&to_local("2020-06-02T20:30:15Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&month_key).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.guid, "id-abc-123");
        assert_eq!(row.title, "Q3 Roadmap Sync");
        assert_eq!(row.platform, "webex");
        assert_eq!(row.transcript_ref, "meetings/webex/raw/transcripts/id-abc-123.vtt");

        // VTT sidecar on disk.
        let sidecar = v.root().join("meetings/webex/raw/transcripts/id-abc-123.vtt");
        assert!(sidecar.exists(), "VTT sidecar written");
        let content = std::fs::read_to_string(&sidecar).unwrap();
        assert!(content.contains("WEBVTT"));
        assert!(content.contains("Alice"));

        // Raw firehose row (verbatim, including expiring download links).
        let raw_content =
            std::fs::read_to_string(v.root().join("meetings/webex/raw/2020-06.jsonl")).unwrap();
        assert!(raw_content.contains("\"id\":\"id-abc-123\""));
        assert!(raw_content.contains("vttDownloadLink"), "raw includes expiring links");

        // Watermark advanced.
        let state = v.read_webex_sync();
        assert_eq!(state.last_transcript_ts.as_deref(), Some("2020-06-02T20:30:15Z"));
    }

    #[test]
    fn deleted_transcript_skips_vtt_download() {
        let v = temp_vault("deleted");
        let api = MockApi::new();
        let t = transcript_deleted("id-del-1", "2020-07-01T10:00:00Z");
        api.add_page(vec![t], None);

        let out = pull_with(&v, &api, "tok").unwrap();
        // Row still written (deleted is in the list endpoint; we preserve it).
        assert_eq!(out.counts.get("meetings"), Some(&1));
        // No VTT download attempted for deleted status.
        assert_eq!(out.counts.get("transcripts"), Some(&0));
        assert!(!api.requested("vtt:"), "no VTT download for deleted transcript");
    }

    #[test]
    fn vtt_404_still_writes_contract_row_without_ref() {
        let v = temp_vault("vtt404");
        let api = MockApi::new();
        let t = transcript_available("id-404-x", "2020-08-15T14:00:00Z");
        api.add_page(vec![t], None);
        api.add_vtt_404("404-x");

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1), "contract row written");
        assert_eq!(out.counts.get("transcripts"), Some(&0), "no transcript on 404");

        let month_key = Partition::Month.key(&to_local("2020-08-15T14:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&month_key).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].transcript_ref, "", "no transcript_ref when VTT 404s");
    }

    #[test]
    fn pagination_follows_link_header_next() {
        let v = temp_vault("pagination");
        let api = MockApi::new();
        let t1 = transcript_available("id-page1", "2020-09-01T09:00:00Z");
        let t2 = transcript_available("id-page2", "2020-09-05T11:00:00Z");
        let next_url = "https://webexapis.com/v1/meetingTranscripts?cursor=page2";
        api.add_page(vec![t1], Some(next_url));
        api.add_next_page(next_url, vec![t2], None);
        api.add_vtt("page1", SAMPLE_VTT);
        api.add_vtt("page2", SAMPLE_VTT);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&2), "both pages collected");
        assert!(api.requested(&format!("next:{next_url}")), "second page fetched via Link header");
    }

    #[test]
    fn upsert_is_idempotent_no_duplicate_guid() {
        let v = temp_vault("idempotent");
        let api1 = MockApi::new();
        let t = transcript_available("id-dedup", "2020-10-10T08:00:00Z");
        api1.add_page(vec![t.clone()], None);
        api1.add_vtt("dedup", SAMPLE_VTT);
        pull_with(&v, &api1, "tok").unwrap();

        // Second pull — same transcript reappears (poll overlap).
        let api2 = MockApi::new();
        api2.add_page(vec![t], None);
        api2.add_vtt("dedup", SAMPLE_VTT);
        let out2 = pull_with(&v, &api2, "tok").unwrap();

        // No new contract row on the second pull (upsert, not duplicate).
        assert_eq!(out2.counts.get("meetings"), Some(&0), "no duplicate row");

        // Exactly ONE row in the partition.
        let month_key = Partition::Month.key(&to_local("2020-10-10T08:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&month_key).unwrap();
        assert_eq!(rows.len(), 1, "exactly one row after two pulls");
    }

    #[test]
    fn minimal_transcript_writes_without_extra_fields() {
        let v = temp_vault("minimal");
        let api = MockApi::new();
        let t = transcript_minimal("id-min-1", "2020-11-20T16:00:00Z");
        api.add_page(vec![t], None);
        api.add_vtt("min/download", SAMPLE_VTT);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&1));
        let month_key = Partition::Month.key(&to_local("2020-11-20T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&month_key).unwrap();
        assert_eq!(rows[0].title, "Standup");
        assert_eq!(rows[0].platform, "webex");
    }

    #[test]
    fn empty_page_writes_nothing_and_does_not_error() {
        let v = temp_vault("empty");
        let api = MockApi::new();
        api.add_page(vec![], None);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("meetings"), Some(&0));
        assert_eq!(out.counts.get("transcripts"), Some(&0));
    }
}
