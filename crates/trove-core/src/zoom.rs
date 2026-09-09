//! Zoom — cloud recordings (OAuth) + local recordings (no login).
//!
//! Two mechanisms, one card:
//!
//! - **Cloud:** `GET /users/me/recordings` → meeting list → per-file
//!   `TRANSCRIPT` (VTT) download → contract row in `meetings/zoom/YYYY-MM.jsonl`.
//!   Requires a Zoom Pro+ account, OAuth `recording:read` scope, and the host
//!   to have enabled "Audio Transcript" in Zoom settings.
//! - **Local:** scans `~/Documents/Zoom/<dir>/` for new meeting directories,
//!   parses any `.vtt` transcript (standard WebVTT with speaker cues) and the
//!   chat `.txt`. Works on every Zoom plan with no OAuth required.
//!
//! Both paths write the same contract row shape (`meetings/zoom/YYYY-MM.jsonl`)
//! and dedup on meeting UUID.  Raw firehose lives in `meetings/zoom/raw/`:
//! cloud API objects in `YYYY-MM.jsonl`, VTT/chat artifacts under `files/`.
//!
//! ## Contract
//!
//! Reuses [`crate::meetings::Meeting`] (the `meetings` domain, bound by Fathom).
//! guid = Zoom meeting UUID (API) / directory-encoded UUID (local).
//!
//! ## Cloud API
//!
//! REST v2 at `https://api.zoom.us/v2/`.
//!
//! `GET /users/me/recordings?from=YYYY-MM-DD&to=YYYY-MM-DD&page_size=300`
//! returns a paged list (paginated via `next_page_token`). Each meeting object
//! carries:
//!
//! - `uuid` — stable meeting UUID (our guid)
//! - `topic` — meeting title
//! - `start_time` — ISO 8601 UTC
//! - `duration` — minutes (integer)
//! - `host_id`, `host_email`
//! - `timezone`
//! - `recording_files[]` — one element per recording file:
//!   - `file_type`: `"MP4"` / `"M4A"` / `"TRANSCRIPT"` / `"CC"` / `"CHAT"`
//!   - `recording_start`, `recording_end` (ISO 8601 UTC)
//!   - `download_url` — requires `Authorization: Bearer <token>`
//!   - `download_access_token` — a short-lived bearer token; use directly
//!   - `status`: `"completed"` when the file is ready
//!   - `file_size`, `id`
//!
//! Download tokens expire in ~24h and are re-issued by the list endpoint on
//! every poll; never persist them (they live only in the raw object).
//!
//! Pagination: follow `next_page_token` until absent or empty. Date window:
//! 30-day range; the cursor advances the `from` date forward so each poll
//! covers at most the last 30 days to stay within Zoom's range limit. On a
//! first sync we start from 1 year back (configurable).
//!
//! AI Companion summaries (`/users/me/meetings/{id}/ai_companion`) are only
//! accessible to the host and only when AI Companion is enabled on the
//! account; the pull attempts the fetch and silently falls through on 404/403.
//!
//! ## Local scan
//!
//! `~/Documents/Zoom/<MeetingTitle YYYY-MM-DD HH.MM.SS>/`
//!   - `*.mp4`, `*.m4a` — recording (not imported)
//!   - `*.vtt` — transcript (WebVTT with `<v Speaker>` cue tags)
//!   - `*.txt` — chat log (plain text, "hh:mm:ss\tSender\tMessage")
//!
//! Guid for local meetings: the directory date-stamp encoded as
//! `local-<YYYYMMDD-HHMMSS>` (no true UUID in the local path without parsing
//! the VTT NOTE header, which is absent on older Zoom versions). If the VTT
//! contains a `NOTE meeting uuid: <uuid>` header we use the real UUID instead,
//! which enables cloud+local dedup.
//!
//! ## Privacy
//!
//! Meeting transcripts are conversation content (≈ message bodies); this
//! integration ships `default_on: false`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
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

const SOURCE: &str = "zoom";
const SERVICE: &str = "zoom";

const CONTRACT_DIR: &str = "meetings/zoom";
const RAW_DIR: &str = "meetings/zoom/raw";
const TRANSCRIPT_DIR: &str = "meetings/zoom/raw/files";

const SYNC_FILE: &str = ".trove/zoom-sync.json";

const API_BASE: &str = "https://api.zoom.us/v2";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Seconds between periodic syncs (every 15 minutes — meetings are low-volume).
pub const ZOOM_SYNC_SECS: u64 = 900;

/// Page size for cloud recordings list (Zoom max is 300).
const PAGE_SIZE: u32 = 300;

/// Days per date window for the recordings list. Zoom's API limits to 30 days
/// per request.
const DATE_WINDOW_DAYS: i64 = 30;

/// How many days back to start on a first sync (no prior cursor).
const INITIAL_BACKFILL_DAYS: i64 = 365;

/// Overlap lookback in days: on each poll, re-cover at least this many recent
/// days so that cloud recordings that took hours to process after a meeting
/// (Zoom processes transcripts asynchronously) are not permanently missed if
/// the cursor already advanced past the meeting day. The upsert is idempotent
/// by guid so re-covering is free.
const CURSOR_OVERLAP_DAYS: i64 = 2;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static ZOOM: Provider = Provider {
    service: SERVICE,
    display_name: "Zoom",
    auth_url: "https://zoom.us/oauth/authorize",
    token_url: "https://zoom.us/oauth/token",
    // recording:read — list and download cloud recordings.
    scopes: "recording:read",
    // Assigned unique production redirect port for zoom (INDEX #92).
    // Must match the registered redirect URI: http://localhost:38672/callback.
    redirect_port: 38672,
    use_pkce: false,
    // Zoom wants client_id / client_secret as HTTP Basic auth on the token
    // endpoint.
    basic_auth: true,
    default_client_id: option_env!("TROVE_ZOOM_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_ZOOM_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured =
        vault.load_sync_app(ZOOM.service)?.is_some() || ZOOM.default_credentials().is_some();
    let accounts = match vault.load_sync_token(ZOOM.service)? {
        Some(token) => vec![ConnectedAccount {
            key: ZOOM.service.to_string(),
            label: ZOOM.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Zoom issues a refresh token; a token expired with a refresh token
            // does NOT need reconnect (the pull refreshes silently).
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
    id: "zoom",
    display_name: "Zoom",
    methods: &[ConnectMethod::OAuth {
        provider: &ZOOM,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["zoom"],
    setup: &[
        "Sign in at marketplace.zoom.us and create a User-managed OAuth app (type: \
         User-managed, not Server-to-Server).",
        "Add the scope: recording:read.",
        "Set the OAuth redirect URL to http://localhost:38672/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved, so every \
         future connect is just a login.",
        "The local recordings scan (~Documents/Zoom/) works without any OAuth — only \
         the cloud path needs a Pro plan and a connected account.",
    ],
};

/// Interactive connect: opens the consent page, waits for the redirect, saves
/// the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(ZOOM.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(ZOOM.service)?
            .or_else(|| ZOOM.default_credentials())
            .context(
                "no Zoom app credentials — register an OAuth app at marketplace.zoom.us and \
                 enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&ZOOM, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(ZOOM.service, &token)?;
    Ok(token)
}

/// Refresh an expired token silently. If there is no refresh token or the
/// refresh fails, the stored token is deleted and the error surfaces so the
/// user sees a reconnect prompt.
fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(ZOOM.service)?
        .or_else(|| ZOOM.default_credentials())
        .context("Zoom token expired and no app credentials — reconnect from the Integrations tab")?;
    match oauth::refresh_token(&ZOOM, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(ZOOM.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(ZOOM.service)?;
            bail!("Zoom token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
struct SyncState {
    /// The `from` date for the next cloud poll window, as `YYYY-MM-DD` (UTC).
    /// Advances after each successful drain. On a first sync, set to 1 year
    /// back from today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cloud_from: Option<String>,
    /// RFC3339 UTC of the newest meeting we have stored (high-water mark).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_meeting_ts: Option<String>,
    /// Set of local meeting directories we have already imported (relative to
    /// `~/Documents/Zoom/`). Stored as a sorted list so it serializes
    /// consistently.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    seen_local_dirs: Vec<String>,
}

impl Vault {
    fn read_zoom_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_zoom_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

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

/// The endpoints the pull needs. A trait so tests drive the logic with
/// fixtures, never the network.
trait ZoomApi {
    /// `GET /users/me/recordings?from=…&to=…&page_size=…[&next_page_token=…]`.
    fn recordings_page(
        &self,
        token: &str,
        from: &str,
        to: &str,
        next_page_token: Option<&str>,
    ) -> Result<Value, FetchError>;

    /// Download a VTT transcript file from the given URL using the
    /// `download_access_token` as a bearer token. Returns the VTT bytes.
    fn download_vtt(&self, url: &str, access_token: &str) -> Result<String, FetchError>;
}

/// Live implementation using ureq + rustls.
struct ZoomClient;

impl ZoomApi for ZoomClient {
    fn recordings_page(
        &self,
        token: &str,
        from: &str,
        to: &str,
        next_page_token: Option<&str>,
    ) -> Result<Value, FetchError> {
        let mut url = format!(
            "{API_BASE}/users/me/recordings?from={from}&to={to}&page_size={PAGE_SIZE}"
        );
        if let Some(npt) = next_page_token {
            url.push_str(&format!("&next_page_token={}", urlencode(npt)));
        }
        http_get_json(&url, token)
    }

    fn download_vtt(&self, url: &str, access_token: &str) -> Result<String, FetchError> {
        let resp = ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {access_token}"))
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

fn http_get_json(url: &str, bearer: &str) -> Result<Value, FetchError> {
    let resp = ureq::get(url)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {bearer}"))
        .set("Accept", "application/json")
        .call();
    match resp {
        Ok(r) => r.into_json().map_err(|e| FetchError::Other(format!("parse: {e}"))),
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

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_zoom_sync().last_meeting_ts.filter(|s| !s.is_empty())
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(total > 0, || {
                let c = |k: &str| out.counts.get(k).copied().unwrap_or(0);
                format!(
                    "zoom synced — {} cloud meetings, {} local meetings",
                    c("cloud_meetings"),
                    c("local_meetings"),
                )
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("zoom sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub already there).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "zoom",
        name: "Zoom",
        kind: IntegrationKind::CloudSync,
        // 🔒 Opt-in: meeting transcripts are conversation content.
        default_on: false,
        description: "Syncs Zoom cloud recordings, VTT transcripts, and AI Companion summaries \
                      via the Zoom API (Pro+ plan with cloud recording enabled), plus scans \
                      ~/Documents/Zoom/ for local recordings on any plan. Both paths write to \
                      the same meetings store, merged by meeting UUID. Transcripts and chat logs \
                      are stored as sidecar files.",
        domain: "meetings",
        vault_path: "meetings/zoom/",
        toggleable: true,
        setup: &[
            "Local recordings (~/Documents/Zoom/) are scanned automatically with no account needed.",
            "For cloud recordings, connect your Zoom account above. Requires a Pro or higher plan \
             with cloud recording and 'Audio Transcript' enabled in Zoom Settings.",
        ],
        caveats: "Cloud transcripts require a Pro+ plan with the host having enabled \
                  'Audio Transcript' before the meeting. AI Companion summaries are only \
                  available to the meeting host.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(ZOOM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("zoom"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Raw meeting shape (verbatim API object).

/// One verbatim Zoom API meeting object in `meetings/zoom/raw/YYYY-MM.jsonl`.
/// Round-trips byte-identically; `uuid` (dedup key) and `start_time` (partition
/// key) are read back via accessors.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct RawMeeting {
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl RawMeeting {
    fn guid(&self) -> String {
        self.fields.get("uuid").and_then(Value::as_str).unwrap_or("").to_string()
    }

    fn start(&self) -> &str {
        self.fields.get("start_time").and_then(Value::as_str).unwrap_or("")
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
/// pass through verbatim (the fathom/strava idiom).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Zoom `duration` field is in **minutes**; convert to seconds.
fn duration_secs_from_minutes(v: &Value) -> Option<i64> {
    v.get("duration")
        .and_then(Value::as_i64)
        .filter(|&m| m > 0)
        .map(|m| m * 60)
}

/// Map a cloud API meeting object → a [`Meeting`] row. Returns `None` when
/// the object has no `uuid` (can't dedup) or no `start_time` (can't partition).
fn meeting_from_cloud(obj: &Value) -> Option<Meeting> {
    let uuid = str_opt(obj, "uuid")?;
    let start_raw = str_opt(obj, "start_time")?;
    let ts = to_local(&start_raw);

    let mut m = Meeting::new(SOURCE, &uuid, ts.clone());
    m.started = ts;
    m.platform = "zoom".to_string();

    if let Some(title) = str_opt(obj, "topic") {
        m.title = title;
    }
    if let Some(host) = str_opt(obj, "host_email").map(|e| e.to_lowercase()) {
        m.host = host;
    }
    if let Some(dur) = duration_secs_from_minutes(obj) {
        m.duration_secs = Some(dur);
    }

    // recording_url: share_url is a recording share link
    // (https://zoom.us/rec/share/…), not a join/conference URL.
    if let Some(url) = str_opt(obj, "share_url") {
        m.recording_url = url;
    }
    // meeting_url (join URL) is not present in the recordings list response.

    // Extra: preserve host_id, timezone, recording_count.
    if let Some(host_id) = str_opt(obj, "host_id") {
        m.extra.insert("host_id".into(), Value::from(host_id));
    }
    if let Some(tz) = str_opt(obj, "timezone") {
        m.extra.insert("timezone".into(), Value::from(tz));
    }
    if let Some(count) = obj.get("recording_count").filter(|v| !v.is_null()) {
        m.extra.insert("recording_count".into(), count.clone());
    }

    Some(m)
}

/// Map a local meeting directory → a [`Meeting`] row. `guid` is the real UUID
/// extracted from the VTT `NOTE meeting uuid:` header if present, otherwise a
/// stable `local-<YYYYMMDD-HHmmSS>` key derived from the directory datestamp.
/// Returns `None` when the directory name has no parseable datestamp.
fn meeting_from_local_dir(dir_name: &str, start_ts: Option<&str>) -> Option<Meeting> {
    let ts = start_ts.unwrap_or("").to_string();
    if ts.is_empty() {
        return None;
    }
    // Fallback guid: stable local key derived from the datestamp portion.
    let guid = local_guid_from_dir(dir_name);
    let mut m = Meeting::new(SOURCE, &guid, to_local(&ts));
    m.started = to_local(&ts);
    m.platform = "zoom".to_string();

    // Title: the portion of the directory name before the first datestamp token.
    // Zoom dirs look like "My Meeting Title 2026-06-15 09.30.00" or
    // "My Meeting Title 2026-06-15" — everything before the YYYY part.
    if let Some(title) = extract_title_from_dir(dir_name) {
        m.title = title;
    }
    m.extra.insert("local_dir".into(), Value::from(dir_name));
    Some(m)
}

/// Stable guid for a local meeting directory: `local-<stripped-digits-from-datestamp>`.
fn local_guid_from_dir(dir_name: &str) -> String {
    // Extract date+time digits: "2026-06-15 09.30.00" → "20260615093000"
    let digits: String = dir_name
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .take(14) // YYYYMMDDHHMMSS
        .collect();
    if digits.len() >= 8 {
        format!("local-{digits}")
    } else {
        format!("local-{}", dir_name.replace(' ', "-").replace('/', "-"))
    }
}

/// Extract a meeting title from a Zoom local directory name.
/// Zoom names meetings like "Q3 Roadmap Sync 2026-06-15 09.30.00" or
/// "Meeting Name 2026-06-15". We keep everything before the first YYYY block.
fn extract_title_from_dir(dir_name: &str) -> Option<String> {
    // Find the byte offset of the first "YYYY-MM-DD" date pattern.
    let idx = (0..dir_name.len()).find(|&i| {
        is_date_chunk(&dir_name[i..].get(..10).unwrap_or(""))
    });
    let title = match idx {
        Some(0) => return None, // name starts with a year, no title prefix
        Some(i) => dir_name[..i].trim_end_matches(|c: char| c == ' ' || c == '-').to_string(),
        None => dir_name.to_string(),
    };
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

// ---------------------------------------------------------------------------
// WebVTT parser — extract utterances from Zoom/standard VTT files.

/// One utterance extracted from a WebVTT file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VttUtterance {
    /// Start timestamp as it appears in the VTT (e.g. `"00:00:05.000"`).
    pub start: String,
    /// End timestamp.
    pub end: String,
    /// Speaker label extracted from a `<v SpeakerName>` cue tag (empty when
    /// no `<v …>` is present).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub speaker: String,
    /// Cue payload text (HTML tags stripped).
    pub text: String,
}

/// Try to extract a meeting UUID from a WebVTT NOTE header.
/// Zoom embeds `NOTE\nmeetingId: <uuid>` in some VTT files.
fn uuid_from_vtt(vtt: &str) -> Option<String> {
    for line in vtt.lines() {
        let l = line.trim();
        // Zoom VTT may have lines like "meetingId: <uuid>" or
        // "NOTE meeting uuid: <uuid>".
        if let Some(rest) = l.strip_prefix("meetingId:").or_else(|| l.strip_prefix("NOTE meeting uuid:")) {
            let id = rest.trim().to_string();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Parse a WebVTT file into a list of utterances. Speaker labels are extracted
/// from `<v SpeakerName>` voice-span tags per the WebVTT spec.
pub fn parse_vtt(vtt: &str) -> Vec<VttUtterance> {
    let mut utterances = Vec::new();
    let mut lines = vtt.lines().peekable();

    // Skip the WEBVTT header line and any optional header block.
    while let Some(line) = lines.next() {
        if line.trim().starts_with("WEBVTT") {
            break;
        }
    }

    // Drain cue blocks. Each cue: optional id → timestamp line → payload lines
    // → blank line.
    let mut start = String::new();
    let mut end = String::new();
    let mut payload: Vec<String> = Vec::new();
    let mut in_cue = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            // End of a cue block (or a separator between blocks).
            if in_cue && !start.is_empty() && !payload.is_empty() {
                let full_text = payload.join(" ");
                let (speaker, text) = extract_speaker_and_text(&full_text);
                utterances.push(VttUtterance { start: start.clone(), end: end.clone(), speaker, text });
            }
            start.clear();
            end.clear();
            payload.clear();
            in_cue = false;
            continue;
        }

        if trimmed.contains("-->") {
            // Timestamp line: "00:00:05.000 --> 00:00:10.000 align:start"
            let parts: Vec<&str> = trimmed.splitn(3, " --> ").collect();
            if parts.len() >= 2 {
                start = parts[0].trim().to_string();
                // End may have optional cue settings after a space.
                end = parts[1].split_whitespace().next().unwrap_or("").to_string();
                in_cue = true;
            }
        } else if in_cue {
            payload.push(trimmed.to_string());
        }
        // Lines before the first timestamp (cue ids, NOTE blocks, etc.) are
        // silently skipped.
    }
    // Flush any unterminated trailing cue.
    if in_cue && !start.is_empty() && !payload.is_empty() {
        let full_text = payload.join(" ");
        let (speaker, text) = extract_speaker_and_text(&full_text);
        utterances.push(VttUtterance { start, end, speaker, text });
    }

    utterances
}

/// Extract the speaker from a `<v SpeakerName>text</v>` or `<v.class SpeakerName>text`
/// voice span, then strip all HTML/WebVTT tags from the remaining text.
///
/// WebVTT voice span syntax: `<v SPEAKER-NAME>` where the name follows
/// immediately after the space. An optional dot-class prefix (`<v.en David>`)
/// means the name is the part after the first space that follows the `>`.
fn extract_speaker_and_text(raw: &str) -> (String, String) {
    let speaker = if let Some(after_v) = raw.strip_prefix("<v") {
        // `after_v` is everything after `<v`, e.g. " David Wills>" or ".en David>"
        if let Some(close) = after_v.find('>') {
            let between = after_v[..close].trim();
            // If it starts with '.', there is a dot-class: `.lang SPEAKER_NAME`.
            // Strip the first whitespace-separated token if it starts with '.'.
            let speaker = if between.starts_with('.') {
                // ".class SpeakerName" — take everything after the first space.
                between.split_once(' ').map(|(_, rest)| rest.trim()).unwrap_or("").to_string()
            } else {
                // No dot-class: the whole `between` is the speaker name.
                between.to_string()
            };
            speaker
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Strip all HTML/WebVTT markup tags.
    let text = strip_vtt_tags(raw);
    (speaker, text)
}

/// Strip HTML/WebVTT cue tags (`<…>`), leaving only the text content.
fn strip_vtt_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Upsert helpers (same pattern as fathom.rs).

fn upsert_contract(vault: &Vault, rows: Vec<Meeting>) -> Result<u64> {
    upsert_partition(vault, CONTRACT_DIR, rows, |m| m.ts.clone(), |m| m.guid.clone())
}

fn upsert_raw(vault: &Vault, rows: Vec<RawMeeting>) -> Result<u64> {
    upsert_partition(vault, RAW_DIR, rows, |r| r.start().to_string(), |r| r.guid())
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
            .with_context(|| format!("zoom: ts {ts:?} has no month (dir {dir})"))?
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
// Pull entry points.

/// Public entry point — resolves credentials, refreshes the token if needed,
/// then runs both cloud and local paths.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let mut cloud_meetings = 0u64;
    let mut cloud_transcripts = 0u64;
    let local_meetings;

    // Cloud path: optional (no-op when not connected).
    match vault.load_sync_token(SERVICE)? {
        Some(token) => match ensure_fresh(vault, token) {
            Ok(fresh) => {
                let client = ZoomClient;
                let out = pull_cloud_with(vault, &client, &fresh.access_token)?;
                cloud_meetings = out.counts.get("cloud_meetings").copied().unwrap_or(0);
                cloud_transcripts = out.counts.get("cloud_transcripts").copied().unwrap_or(0);
            }
            Err(e) => {
                // Token refresh failed: log as a note, proceed to local scan.
                let _ = e; // surfaced via def_pull on manual path
            }
        },
        None => {
            // Not connected: skip cloud path silently (local scan still runs).
        }
    }

    // Local path: always runs.
    let local_out = pull_local(vault)?;
    local_meetings = local_out.counts.get("local_meetings").copied().unwrap_or(0);

    let mut counts = BTreeMap::new();
    counts.insert("cloud_meetings", cloud_meetings);
    counts.insert("cloud_transcripts", cloud_transcripts);
    counts.insert("local_meetings", local_meetings);
    Ok(PullOutcome {
        headline: format!(
            "Zoom synced — {cloud_meetings} cloud meetings, {local_meetings} local meetings"
        ),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Cloud pull.

/// Cloud pull over an injected API (the testable seam).
fn pull_cloud_with(vault: &Vault, api: &impl ZoomApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_zoom_sync();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();

    // Compute the `from` / `to` window.
    //
    // Re-anchor with CURSOR_OVERLAP_DAYS: cloud recordings are processed
    // asynchronously after the meeting ends (sometimes hours later). If the
    // cursor strictly advances past today, any recording that becomes available
    // later that day would be permanently missed.  We always look back at least
    // CURSOR_OVERLAP_DAYS so late-arriving transcripts are picked up.  The
    // upsert is idempotent by guid, so re-covering recent days is free.
    let stored_from = state.cloud_from.clone().unwrap_or_else(|| {
        let start = chrono::Utc::now()
            - chrono::Duration::days(INITIAL_BACKFILL_DAYS);
        start.format("%Y-%m-%d").to_string()
    });
    let overlap_floor = {
        let today_dt = chrono::Utc::now().date_naive();
        (today_dt - chrono::Duration::days(CURSOR_OVERLAP_DAYS))
            .format("%Y-%m-%d")
            .to_string()
    };
    // Use whichever is earlier: the stored cursor or the overlap floor.
    let from = if stored_from <= overlap_floor { stored_from } else { overlap_floor.clone() };
    // Clamp to DATE_WINDOW_DAYS.
    let to = {
        let from_dt = chrono::NaiveDate::parse_from_str(&from, "%Y-%m-%d")
            .unwrap_or_else(|_| chrono::Utc::now().date_naive());
        let end_dt = from_dt + chrono::Duration::days(DATE_WINDOW_DAYS);
        let today_dt = chrono::Utc::now().date_naive();
        if end_dt > today_dt { today_dt } else { end_dt }
            .format("%Y-%m-%d")
            .to_string()
    };
    // `from > today` can no longer happen with the overlap floor, but guard
    // defensively.
    if from > today {
        // Already fully caught up (should not happen with the overlap floor).
        let mut counts = BTreeMap::new();
        counts.insert("cloud_meetings", 0u64);
        counts.insert("cloud_transcripts", 0u64);
        return Ok(PullOutcome {
            headline: "Zoom cloud: up to date".to_string(),
            counts,
        });
    }

    // Drain all pages for this window.
    let mut meetings: Vec<Value> = Vec::new();
    let mut next_page_token: Option<String> = None;
    loop {
        let body = api
            .recordings_page(token, &from, &to, next_page_token.as_deref())
            .map_err(|e| anyhow::anyhow!("Zoom /users/me/recordings: {e}"))?;

        // Parse the meetings list from the response body.
        let items = body
            .get("meetings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        meetings.extend(items);

        let npt = body
            .get("next_page_token")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        match npt {
            Some(t) => next_page_token = Some(t),
            None => break,
        }
    }

    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut raw_rows: Vec<RawMeeting> = Vec::new();
    let mut transcripts_written = 0u64;
    let mut newest_ts: Option<String> = state.last_meeting_ts.clone();

    for obj in &meetings {
        // Raw firehose: verbatim object.
        if let Some(fields) = obj.as_object() {
            raw_rows.push(RawMeeting { fields: fields.clone() });
        }

        let Some(mut row) = meeting_from_cloud(obj) else { continue };
        let guid = row.guid.clone();

        // Look for a TRANSCRIPT recording file.
        // download_access_token is a meeting-level field (not per recording_files entry):
        // it is a short-lived bearer token issued by the recordings list endpoint and
        // is a sibling of recording_files[], not embedded inside each file object.
        let meeting_access_token = obj
            .get("download_access_token")
            .and_then(Value::as_str)
            .unwrap_or("");
        if let Some(transcript_file) = find_transcript_file(obj) {
            let dl_url =
                transcript_file.get("download_url").and_then(Value::as_str).unwrap_or("");
            let access_token = meeting_access_token;

            if !dl_url.is_empty() && !access_token.is_empty() {
                match api.download_vtt(dl_url, access_token) {
                    Ok(vtt_text) => {
                        let ref_path = cloud_transcript_ref(&guid);
                        let utterances = parse_vtt(&vtt_text);
                        vault.write_snapshot(&ref_path, &utterances)?;
                        // Also preserve the raw VTT text.
                        let raw_path = format!("{TRANSCRIPT_DIR}/{guid}.vtt");
                        if let Ok(abs) = vault.resolve(&raw_path) {
                            if let Some(parent) = abs.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::write(&abs, &vtt_text);
                        }
                        row.transcript_ref = ref_path;
                        transcripts_written += 1;
                    }
                    Err(FetchError::NotFound) => {
                        // Transcript not yet processed; row stored without it.
                    }
                    Err(e) => {
                        // Non-fatal: the meeting row still lands.
                        let _ = e;
                    }
                }
            }
        }

        // Advance the high-water mark.
        let start = str_opt(obj, "start_time").unwrap_or_default();
        if !start.is_empty() {
            newest_ts = max_ts(newest_ts, start);
        }

        contract_rows.push(row);
    }

    let raw_new = upsert_raw(vault, raw_rows)?;
    let contract_new = upsert_contract(vault, contract_rows)?;

    // Advance the cursor: next window starts the day after the current `to`,
    // but never past (today - CURSOR_OVERLAP_DAYS) so that recordings that
    // process hours after a meeting ends are still picked up on future polls.
    let next_from = {
        let candidate = advance_date(&to, 1);
        if candidate <= overlap_floor { candidate } else { overlap_floor.clone() }
    };
    state.cloud_from = Some(next_from);
    state.last_meeting_ts = newest_ts;
    vault.write_zoom_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("cloud_meetings", contract_new);
    counts.insert("cloud_transcripts", transcripts_written);
    counts.insert("raw", raw_new);
    Ok(PullOutcome {
        headline: format!(
            "Zoom cloud synced — {contract_new} new meetings, {transcripts_written} transcripts"
        ),
        counts,
    })
}

/// Vault-relative path for a cloud meeting transcript sidecar.
fn cloud_transcript_ref(uuid: &str) -> String {
    format!("{TRANSCRIPT_DIR}/{uuid}.jsonl")
}

/// Find the first `file_type = "TRANSCRIPT"` entry in `recording_files[]`.
fn find_transcript_file(obj: &Value) -> Option<&Value> {
    obj.get("recording_files")
        .and_then(Value::as_array)
        .and_then(|files| {
            files.iter().find(|f| {
                f.get("file_type").and_then(Value::as_str) == Some("TRANSCRIPT")
                    && f.get("status").and_then(Value::as_str) == Some("completed")
            })
        })
}

/// Return the later of two RFC3339 UTC date strings (lexical comparison is
/// correct for the `…Z` form Zoom uses).
fn max_ts(cur: Option<String>, candidate: String) -> Option<String> {
    match cur {
        Some(prev) if prev >= candidate => Some(prev),
        _ => Some(candidate),
    }
}

/// Add `days` to a `YYYY-MM-DD` string. Returns the original on parse failure.
fn advance_date(date: &str, days: i64) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| (d + chrono::Duration::days(days)).format("%Y-%m-%d").to_string())
        .unwrap_or_else(|_| date.to_string())
}

// ---------------------------------------------------------------------------
// Local scan.

/// Scan `~/Documents/Zoom/` for new meeting directories and import them.
fn pull_local(vault: &Vault) -> Result<PullOutcome> {
    let zoom_dir = zoom_local_dir();
    let mut state = vault.read_zoom_sync();
    let seen: std::collections::HashSet<String> =
        state.seen_local_dirs.iter().cloned().collect();

    let mut contract_rows: Vec<Meeting> = Vec::new();
    let mut new_seen: Vec<String> = Vec::new();

    if !zoom_dir.exists() {
        let mut counts = BTreeMap::new();
        counts.insert("local_meetings", 0u64);
        return Ok(PullOutcome { headline: "Zoom local: no ~/Documents/Zoom/ found".into(), counts });
    }

    let entries = match std::fs::read_dir(&zoom_dir) {
        Ok(e) => e,
        Err(_) => {
            let mut counts = BTreeMap::new();
            counts.insert("local_meetings", 0u64);
            return Ok(PullOutcome {
                headline: "Zoom local: could not read ~/Documents/Zoom/".into(),
                counts,
            });
        }
    };

    for entry in entries.flatten() {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if seen.contains(&dir_name) {
            continue;
        }

        // Try to extract a datestamp from the directory name.
        let start_ts = datestamp_from_dir(&dir_name);
        if start_ts.is_none() {
            // Not a Zoom meeting directory (no parseable date).
            continue;
        }

        let dir_path = entry.path();

        // Look for a VTT file in this directory.
        let vtt_path = first_file_with_ext(&dir_path, "vtt");
        let chat_path = first_file_with_ext(&dir_path, "txt");

        let vtt_text = vtt_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());

        // If the VTT has a real UUID, use it (enables cloud+local dedup).
        let real_uuid = vtt_text.as_deref().and_then(uuid_from_vtt);

        let Some(mut row) = meeting_from_local_dir(&dir_name, start_ts.as_deref()) else {
            continue;
        };
        if let Some(uuid) = real_uuid {
            row.guid = uuid;
        }

        let guid = row.guid.clone();

        // Write the VTT sidecar.
        // Note: guid already carries the "local-" prefix (from local_guid_from_dir)
        // when no real UUID was found; use it directly to avoid "local-local-..." paths.
        if let Some(vtt) = &vtt_text {
            let utterances = parse_vtt(vtt);
            if !utterances.is_empty() {
                let ref_path = format!("{TRANSCRIPT_DIR}/{guid}.jsonl");
                vault.write_snapshot(&ref_path, &utterances)?;
                row.transcript_ref = ref_path;
            }
        }

        // Preserve the chat log in raw/files/ if present.
        if let Some(chat) = chat_path {
            if let Ok(chat_text) = std::fs::read_to_string(&chat) {
                let raw_path = format!("{TRANSCRIPT_DIR}/{guid}-chat.txt");
                if let Ok(abs) = vault.resolve(&raw_path) {
                    if let Some(parent) = abs.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(&abs, &chat_text);
                }
            }
        }

        contract_rows.push(row);
        new_seen.push(dir_name);
    }

    let new_count = contract_rows.len() as u64;
    if !contract_rows.is_empty() {
        upsert_contract(vault, contract_rows)?;
    }

    // Persist the seen-dirs set.
    let mut all_seen: std::collections::BTreeSet<String> =
        state.seen_local_dirs.iter().cloned().collect();
    all_seen.extend(new_seen);
    state.seen_local_dirs = all_seen.into_iter().collect();
    vault.write_zoom_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("local_meetings", new_count);
    Ok(PullOutcome {
        headline: format!("Zoom local synced — {new_count} new meetings"),
        counts,
    })
}

/// The local Zoom recordings directory.
fn zoom_local_dir() -> PathBuf {
    dirs::document_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()))
        .join("Zoom")
}

/// Find the first file with a given extension (case-insensitive) in a
/// directory, or `None`.
fn first_file_with_ext(dir: &std::path::Path, ext: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        if p.extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case(ext))
        {
            Some(p)
        } else {
            None
        }
    })
}

/// Extract an ISO 8601 UTC datestamp from a Zoom local directory name.
/// Zoom formats directories as `<Title> YYYY-MM-DD HH.MM.SS` (spaces and
/// dots). Returns `YYYY-MM-DDTHH:MM:SSZ` on success.
fn datestamp_from_dir(name: &str) -> Option<String> {
    let s = name;
    let mut pos = 0usize;
    while pos + 10 <= s.len() {
        let chunk = &s[pos..pos + 10];
        if is_date_chunk(chunk) {
            let date = chunk;
            // Try to grab the time: " HH.MM.SS" or " HH:MM:SS" right after.
            let rest = s[pos + 10..].trim_start();
            let time = if rest.len() >= 8 {
                let t = &rest[..8];
                let sep = t.as_bytes().get(2).copied();
                if sep == Some(b'.') || sep == Some(b':') {
                    let cleaned = t.replace('.', ":");
                    Some(cleaned)
                } else {
                    None
                }
            } else {
                None
            };
            let ts = match time {
                Some(t) => format!("{date}T{t}Z"),
                None => format!("{date}T00:00:00Z"),
            };
            return Some(ts);
        }
        // Advance by one char (handle multi-byte safely).
        let ch_len = s[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        pos += ch_len;
    }
    None
}

/// Check whether a 10-character slice looks like `YYYY-MM-DD`.
fn is_date_chunk(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[0..4].iter().all(|c| c.is_ascii_digit())
        && b[4] == b'-'
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[7] == b'-'
        && b[8..10].iter().all(|c| c.is_ascii_digit())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-zoom-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — representative cloud API meeting object shapes.

    /// A meeting with a completed TRANSCRIPT file (the happy path).
    ///
    /// `download_access_token` is at the MEETING level (sibling of
    /// `recording_files[]`), not inside each file entry — this matches the
    /// real Zoom API where the token is issued per-meeting-response, not
    /// per-file. The per-file entries carry only `download_url`.
    fn meeting_with_transcript(uuid: &str) -> Value {
        serde_json::json!({
            "uuid": uuid,
            "id": 12345678901_i64,
            "topic": "Q3 Roadmap Sync",
            "start_time": "2026-06-10T16:00:00Z",
            "duration": 49,
            "host_id": "aB1cD2eF3g",
            "host_email": "DWills@Example.com",
            "timezone": "America/Los_Angeles",
            "recording_count": 3,
            "total_size": 1048576_i64,
            "share_url": "https://zoom.us/rec/share/abc123",
            // Meeting-level token (runtime-issued, per Zoom API docs).
            "download_access_token": "tok-meeting-level",
            "recording_files": [
                {
                    "id": "file-001",
                    "meeting_id": uuid,
                    "recording_start": "2026-06-10T16:00:05Z",
                    "recording_end": "2026-06-10T16:49:00Z",
                    "file_type": "MP4",
                    "file_size": 1048576_i64,
                    "play_url": "https://zoom.us/rec/play/abc",
                    "download_url": "https://zoom.us/rec/download/mp4",
                    "status": "completed",
                    "recording_type": "shared_screen_with_speaker_view"
                },
                {
                    "id": "file-002",
                    "meeting_id": uuid,
                    "recording_start": "2026-06-10T16:00:05Z",
                    "recording_end": "2026-06-10T16:49:00Z",
                    "file_type": "TRANSCRIPT",
                    "file_size": 4096,
                    "download_url": "https://zoom.us/rec/download/vtt",
                    "status": "completed",
                    "recording_type": "audio_transcript"
                }
            ]
        })
    }

    /// A meeting with no transcript file (cloud recording only).
    fn meeting_no_transcript(uuid: &str) -> Value {
        serde_json::json!({
            "uuid": uuid,
            "id": 99999_i64,
            "topic": "Team Standup",
            "start_time": "2026-06-11T09:00:00Z",
            "duration": 15,
            "host_id": "aB1cD2eF3g",
            "host_email": "DWills@Example.com",
            "timezone": "America/Los_Angeles",
            "download_access_token": "tok-meeting-level-2",
            "recording_files": [
                {
                    "id": "file-003",
                    "file_type": "MP4",
                    "status": "completed",
                    "download_url": "https://zoom.us/rec/download/mp4"
                }
            ]
        })
    }

    // A sample WebVTT transcript as Zoom produces it.
    fn sample_vtt() -> String {
        "WEBVTT\n\n\
         00:00:05.000 --> 00:00:10.000\n\
         <v David Wills>Hello everyone, thanks for joining.\n\n\
         00:00:11.000 --> 00:00:15.000\n\
         <v Sam Ortiz>Hi David, let's dive in.\n\n\
         00:00:16.000 --> 00:00:20.000\n\
         No speaker tag here.\n"
            .to_string()
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        /// (from, to, next_page_token) → page body
        pages: RefCell<Vec<(String, String, Option<String>, Value)>>,
        vtt_responses: RefCell<HashMap<String, Result<String, String>>>,
        requests: RefCell<Vec<String>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                pages: RefCell::new(Vec::new()),
                vtt_responses: RefCell::new(HashMap::new()),
                requests: RefCell::new(Vec::new()),
            }
        }

        fn add_page(&self, from: &str, to: &str, npt_in: Option<&str>, items: Vec<Value>, npt_out: Option<&str>) {
            let body = serde_json::json!({ "meetings": items, "next_page_token": npt_out });
            self.pages.borrow_mut().push((from.to_string(), to.to_string(), npt_in.map(str::to_string), body));
        }

        fn add_vtt(&self, url: &str, vtt: &str) {
            self.vtt_responses.borrow_mut().insert(url.to_string(), Ok(vtt.to_string()));
        }
    }

    impl ZoomApi for MockApi {
        fn recordings_page(
            &self,
            _token: &str,
            from: &str,
            to: &str,
            next_page_token: Option<&str>,
        ) -> Result<Value, FetchError> {
            let req = format!("recordings from={from} to={to} npt={next_page_token:?}");
            self.requests.borrow_mut().push(req);
            for (pf, pt, pnpt, body) in self.pages.borrow().iter() {
                if pf == from && pt == to && pnpt.as_deref() == next_page_token {
                    return Ok(body.clone());
                }
            }
            Ok(serde_json::json!({ "meetings": [], "next_page_token": null }))
        }

        fn download_vtt(&self, url: &str, access_token: &str) -> Result<String, FetchError> {
            // Record "vtt:<url>|tok:<token>" so tests can verify the token came
            // from the meeting level.
            self.requests.borrow_mut().push(format!("vtt:{url}|tok:{access_token}"));
            match self.vtt_responses.borrow().get(url) {
                Some(Ok(vtt)) => Ok(vtt.clone()),
                Some(Err(_)) => Err(FetchError::NotFound),
                None => Err(FetchError::NotFound),
            }
        }
    }

    // -----------------------------------------------------------------------
    // VTT parser tests.

    #[test]
    fn parses_webvtt_with_speaker_tags() {
        let utterances = parse_vtt(&sample_vtt());
        assert_eq!(utterances.len(), 3);

        assert_eq!(utterances[0].start, "00:00:05.000");
        assert_eq!(utterances[0].end, "00:00:10.000");
        assert_eq!(utterances[0].speaker, "David Wills");
        assert_eq!(utterances[0].text, "Hello everyone, thanks for joining.");

        assert_eq!(utterances[1].speaker, "Sam Ortiz");
        assert_eq!(utterances[1].text, "Hi David, let's dive in.");

        assert_eq!(utterances[2].speaker, "", "no speaker tag → empty speaker");
        assert_eq!(utterances[2].text, "No speaker tag here.");
    }

    #[test]
    fn parses_empty_vtt_to_empty_vec() {
        assert!(parse_vtt("WEBVTT\n").is_empty());
        assert!(parse_vtt("").is_empty());
    }

    #[test]
    fn strips_html_tags_from_cue_text() {
        let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\n<b>Bold</b> and <i>italic</i>\n";
        let u = parse_vtt(vtt);
        assert_eq!(u[0].text, "Bold and italic");
    }

    #[test]
    fn uuid_from_vtt_extracts_meeting_id() {
        let vtt = "WEBVTT\nmeetingId: abc-uuid-123\n\n00:00:01.000 --> 00:00:02.000\nHello\n";
        assert_eq!(uuid_from_vtt(vtt).as_deref(), Some("abc-uuid-123"));
        assert_eq!(uuid_from_vtt("WEBVTT\n\n"), None);
    }

    // -----------------------------------------------------------------------
    // Directory name parsing tests.

    #[test]
    fn datestamp_from_standard_zoom_dir_name() {
        let ts = datestamp_from_dir("Q3 Roadmap Sync 2026-06-15 09.30.00").unwrap();
        assert_eq!(ts, "2026-06-15T09:30:00Z");
    }

    #[test]
    fn datestamp_from_dir_name_with_colon_separator() {
        let ts = datestamp_from_dir("Team Meeting 2026-06-11 14:00:00").unwrap();
        assert_eq!(ts, "2026-06-11T14:00:00Z");
    }

    #[test]
    fn datestamp_from_dir_date_only() {
        let ts = datestamp_from_dir("Standup 2026-06-16").unwrap();
        assert_eq!(ts, "2026-06-16T00:00:00Z");
    }

    #[test]
    fn datestamp_from_dir_no_date_returns_none() {
        assert!(datestamp_from_dir("not-a-zoom-dir").is_none());
        assert!(datestamp_from_dir("").is_none());
    }

    #[test]
    fn extract_title_from_dir_name() {
        assert_eq!(
            extract_title_from_dir("Q3 Roadmap Sync 2026-06-15 09.30.00"),
            Some("Q3 Roadmap Sync".to_string())
        );
        assert_eq!(
            extract_title_from_dir("Standup 2026-06-16"),
            Some("Standup".to_string())
        );
        assert_eq!(extract_title_from_dir(""), None);
        // Starts with a year — no title prefix.
        assert_eq!(extract_title_from_dir("2026-06-15 09.30.00"), None);
    }

    #[test]
    fn local_guid_is_stable_from_dir_name() {
        let g = local_guid_from_dir("Q3 Sync 2026-06-15 09.30.00");
        assert!(g.starts_with("local-"));
        // Same input → same guid (deterministic).
        assert_eq!(g, local_guid_from_dir("Q3 Sync 2026-06-15 09.30.00"));
    }

    // -----------------------------------------------------------------------
    // Cloud mapping tests.

    #[test]
    fn maps_cloud_meeting_guid_ts_duration_host() {
        let obj = meeting_with_transcript("uuid-abc-123");
        let m = meeting_from_cloud(&obj).unwrap();
        assert_eq!(m.guid, "uuid-abc-123");
        assert_eq!(
            DateTime::parse_from_rfc3339(&m.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T16:00:00Z").unwrap().timestamp()
        );
        assert_eq!(m.title, "Q3 Roadmap Sync");
        assert_eq!(m.duration_secs, Some(49 * 60), "duration in minutes → seconds");
        assert_eq!(m.host, "dwills@example.com", "host_email lowercased");
        assert_eq!(m.platform, "zoom");
        assert_eq!(
            m.recording_url, "https://zoom.us/rec/share/abc123",
            "share_url maps to recording_url (it is a share link, not a join URL)"
        );
        assert!(m.meeting_url.is_empty(), "meeting_url stays empty — join URL not in recordings list response");
        assert!(m.extra.contains_key("host_id"));
        assert!(m.extra.contains_key("timezone"));
    }

    #[test]
    fn find_transcript_file_picks_completed_transcript() {
        let obj = meeting_with_transcript("u1");
        let f = find_transcript_file(&obj).unwrap();
        assert_eq!(f.get("file_type").and_then(Value::as_str), Some("TRANSCRIPT"));
        assert_eq!(f.get("status").and_then(Value::as_str), Some("completed"));
    }

    #[test]
    fn find_transcript_file_returns_none_when_absent() {
        let obj = meeting_no_transcript("u2");
        assert!(find_transcript_file(&obj).is_none());
    }

    // -----------------------------------------------------------------------
    // Full cloud pull tests.
    //
    // NOTE: Tests set cloud_from to dates well in the past (2024-xx-xx) so the
    // computed `to = from + 30 days` window is never clamped to today. This
    // keeps the expected (from, to) pair deterministic regardless of the run date.

    const TEST_FROM: &str = "2024-06-10";
    const TEST_TO: &str = "2024-07-10"; // 2024-06-10 + 30 days

    fn test_state_from(from: &str) -> SyncState {
        SyncState { cloud_from: Some(from.to_string()), ..Default::default() }
    }

    #[test]
    fn cloud_pull_writes_contract_raw_and_transcript() {
        let v = temp_vault("cloud-pull");
        let api = MockApi::new();
        v.write_zoom_sync(&test_state_from(TEST_FROM)).unwrap();

        api.add_page(TEST_FROM, TEST_TO, None, vec![meeting_with_transcript("UUID-001")], None);
        api.add_vtt("https://zoom.us/rec/download/vtt", &sample_vtt());

        let out = pull_cloud_with(&v, &api, "access-token").unwrap();
        assert_eq!(out.counts.get("cloud_meetings"), Some(&1));
        assert_eq!(out.counts.get("cloud_transcripts"), Some(&1));

        // Contract row in the ts-month partition.
        let key = Partition::Month
            .key(&to_local("2026-06-10T16:00:00Z"))
            .unwrap()
            .to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].guid, "UUID-001");
        assert_eq!(rows[0].title, "Q3 Roadmap Sync");
        assert!(!rows[0].transcript_ref.is_empty(), "transcript_ref set");

        // Sidecar written with parsed utterances.
        let sidecar_path = rows[0].transcript_ref.clone();
        let abs = v.root().join(&sidecar_path);
        assert!(abs.exists(), "transcript sidecar exists at {sidecar_path}");
        let body = std::fs::read_to_string(&abs).unwrap();
        assert!(body.contains("David Wills"), "speaker in sidecar");

        // Raw firehose.
        assert!(v.root().join("meetings/zoom/raw/2026-06.jsonl").exists());

        // The download used the MEETING-level token (not a per-file token).
        let reqs = api.requests.borrow();
        let vtt_req = reqs.iter().find(|r| r.starts_with("vtt:")).expect("VTT download was made");
        assert!(
            vtt_req.contains("|tok:tok-meeting-level"),
            "download_access_token must come from meeting level, got: {vtt_req}"
        );
    }

    #[test]
    fn cloud_pull_deduplicates_same_uuid_on_repoll() {
        let v = temp_vault("cloud-dedup");
        let api1 = MockApi::new();
        v.write_zoom_sync(&test_state_from(TEST_FROM)).unwrap();
        api1.add_page(TEST_FROM, TEST_TO, None, vec![meeting_with_transcript("UUID-DUP")], None);
        api1.add_vtt("https://zoom.us/rec/download/vtt", &sample_vtt());
        pull_cloud_with(&v, &api1, "tok").unwrap();

        // Re-poll from the same start.
        let mut st = v.read_zoom_sync();
        st.cloud_from = Some(TEST_FROM.to_string());
        v.write_zoom_sync(&st).unwrap();

        let api2 = MockApi::new();
        api2.add_page(TEST_FROM, TEST_TO, None, vec![meeting_with_transcript("UUID-DUP")], None);
        api2.add_vtt("https://zoom.us/rec/download/vtt", &sample_vtt());
        pull_cloud_with(&v, &api2, "tok").unwrap();

        let key = Partition::Month.key(&to_local("2026-06-10T16:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1, "upsert by guid — no duplicate");
        let raw = std::fs::read_to_string(v.root().join("meetings/zoom/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "raw deduplicated");
    }

    #[test]
    fn cloud_pull_handles_missing_transcript_gracefully() {
        let v = temp_vault("no-transcript");
        v.write_zoom_sync(&test_state_from(TEST_FROM)).unwrap();
        let api = MockApi::new();
        api.add_page(TEST_FROM, TEST_TO, None, vec![meeting_no_transcript("UUID-NO-VTT")], None);

        let out = pull_cloud_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("cloud_meetings"), Some(&1), "meeting still lands");
        assert_eq!(out.counts.get("cloud_transcripts"), Some(&0), "no transcript");

        let key = Partition::Month.key(&to_local("2026-06-11T09:00:00Z")).unwrap().to_string();
        let rows: Vec<Meeting> = v.stream(CONTRACT_DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows[0].transcript_ref, "", "no transcript_ref set");
    }

    #[test]
    fn cloud_pull_paginates_all_pages() {
        let v = temp_vault("paginate");
        v.write_zoom_sync(&test_state_from(TEST_FROM)).unwrap();
        let api = MockApi::new();
        api.add_page(TEST_FROM, TEST_TO, None, vec![meeting_no_transcript("P1")], Some("CURSOR2"));
        api.add_page(TEST_FROM, TEST_TO, Some("CURSOR2"), vec![meeting_no_transcript("P2")], None);

        let out = pull_cloud_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("cloud_meetings"), Some(&2), "both pages collected");
    }

    #[test]
    fn cursor_advances_after_successful_pull() {
        let v = temp_vault("cursor-advance");
        v.write_zoom_sync(&test_state_from(TEST_FROM)).unwrap();
        let api = MockApi::new();
        api.add_page(TEST_FROM, TEST_TO, None, vec![meeting_no_transcript("UUID-ADV")], None);
        pull_cloud_with(&v, &api, "tok").unwrap();
        let st = v.read_zoom_sync();
        assert!(
            st.cloud_from.as_deref().unwrap_or("") > TEST_FROM,
            "cursor advanced past the `from` date"
        );
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.cloud_from.is_none());
        assert!(empty.last_meeting_ts.is_none());
        assert!(empty.seen_local_dirs.is_empty());

        let partial: SyncState =
            serde_json::from_str(r#"{"cloud_from":"2026-06-01","last_meeting_ts":"2026-06-01T00:00:00Z"}"#).unwrap();
        assert_eq!(partial.cloud_from.as_deref(), Some("2026-06-01"));

        // Unknown fields tolerated.
        let legacy: SyncState = serde_json::from_str(
            r#"{"cloud_from":"2026-05-01","future_field":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(legacy.cloud_from.as_deref(), Some("2026-05-01"));
    }

    #[test]
    fn raw_meeting_roundtrips_full_fidelity() {
        let obj = meeting_with_transcript("U1");
        let r = RawMeeting { fields: obj.as_object().unwrap().clone() };
        assert_eq!(r.guid(), "U1");
        assert_eq!(r.start(), "2026-06-10T16:00:00Z");
        let line = serde_json::to_string(&r).unwrap();
        let back: RawMeeting = serde_json::from_str(&line).unwrap();
        assert_eq!(back, r);
        assert!(line.contains("\"uuid\":\"U1\""));
    }

    #[test]
    fn connection_uses_assigned_port_38672() {
        assert_eq!(ZOOM.redirect_port, 38672);
        assert_eq!(ZOOM.redirect_uri(), "http://localhost:38672/callback");
    }

    #[test]
    fn connection_exposes_oauth_method() {
        assert!(CONNECTION.method("oauth").is_some());
        assert_eq!(CONNECTION.id, "zoom");
    }

    #[test]
    fn meeting_serde_back_compat() {
        // An old sparse row must still deserialize.
        let old = serde_json::json!({
            "ts": "2026-06-02T11:05:00-07:00",
            "source": "zoom",
            "guid": "UUID-OLD",
            "title": "Old Meeting",
            "future_field": "ignored"
        });
        let m: Meeting = serde_json::from_value(old).unwrap();
        assert_eq!(m.guid, "UUID-OLD");
        assert!(m.transcript_ref.is_empty());
        let re = serde_json::to_value(&m).unwrap();
        assert!(re.get("future_field").is_none());
    }

    #[test]
    fn local_dir_meeting_from_dir_name() {
        let dir = "Q3 Sync 2026-06-15 09.30.00";
        let ts = datestamp_from_dir(dir).unwrap();
        let m = meeting_from_local_dir(dir, Some(&ts)).unwrap();
        assert_eq!(m.title, "Q3 Sync");
        assert_eq!(m.source, "zoom");
        assert_eq!(m.platform, "zoom");
        assert!(m.guid.starts_with("local-"), "guid is local- prefixed");
    }
}
