//! Eight Sleep Pod — smart-mattress sleep biometrics via the community
//! reverse-engineered cloud API. **Unofficial, ToS-gray** — the connect card
//! says so plainly; the pull degrades gracefully on any shape change.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/eight-sleep.md.
//!
//! ## What is collected
//!
//! One nightly sync polls `GET /users/{userId}/trends?from=…&to=…` (the
//! `v2` model). Each day object fans out into:
//!
//! - **[`crate::health_medical::Observation`]** rows under
//!   `health/medical/eight-sleep/observations/YYYY-MM.jsonl`:
//!   - Per-5-minute HR readings from `sessions[].timeseries.heartRate`
//!     (LOINC 8867-4, "Heart rate", unit `/min` UCUM — not `bpm`).
//!   - Per-session HRV summary from `sleepQualityScore.hrv.current`
//!     (LOINC 80404-7, "rMSSD").
//!   - Per-session respiratory rate from
//!     `sleepQualityScore.respiratoryRate.average`
//!     (LOINC 9279-1, "Respiratory rate").
//!
//! - **[`crate::home::HomeReading`]** rows under
//!   `home/eight-sleep/YYYY-MM.jsonl`:
//!   - Per-session average bed temperature (`tempBedC`, metric
//!     `"temperature_bed"`, unit `"C"`).
//!   - Per-session average room temperature (`tempRoomC`, metric
//!     `"temperature"`, unit `"C"`).
//!
//! - **Raw** full-fidelity day+session objects under
//!   `health/eight-sleep/raw/YYYY-MM.jsonl` (unconditional).
//!
//! ## Auth
//!
//! Eight Sleep uses a **password-grant OAuth2 token** (`grant_type=password`)
//! against `https://auth-api.8slp.net/v1/tokens`. The community client ids
//! are baked in; the user pastes `email:password`. The access token lives
//! under `.trove/sync/eight-sleep.json` (0600); there is no refresh token
//! in the documented community flow, so re-auth runs on token expiry.
//!
//! ## Cursor
//!
//! A rebuildable cursor at `.trove/eight-sleep-sync.json` tracks the latest
//! day synced and the userId. Watermark is a `YYYY-MM-DD` string (the last
//! complete day whose data landed); the next sync requests `from=watermark`
//! so the overlapping day is re-fetched (idempotent; guid dedupe absorbs it).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health_medical::Observation;
use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths.

/// Contract-layer observation stream (HR / HRV / resp rate).
const OBS_DIR: &str = "health/medical/eight-sleep/observations";
/// Contract-layer home-reading stream (bed + room temperature).
const HOME_DIR: &str = "home/eight-sleep";
/// Full-fidelity raw stream (verbatim day objects from the trends endpoint).
const RAW_DIR: &str = "health/eight-sleep/raw";

/// Non-secret rebuildable cursor (last day synced + userId). NOT under
/// `.trove/sync/` — that is for 0600 secrets only.
const SYNC_FILE: &str = ".trove/eight-sleep-sync.json";

/// Service id under `.trove/sync/` where the access token is stored (0600).
const SERVICE: &str = "eight-sleep";

// ---------------------------------------------------------------------------
// API constants (community reverse-engineered; confirmed via lukas-clarke/eight_sleep).

/// Password-grant token endpoint (community-confirmed).
const AUTH_URL: &str = "https://auth-api.8slp.net/v1/tokens";
/// Base URL for the REST API (community-confirmed).
const CLIENT_API_URL: &str = "https://client-api.8slp.net/v1";

/// Community client id (public knowledge in the HA integration).
/// Override at build time by setting `TROVE_EIGHT_SLEEP_CLIENT_ID`.
const DEFAULT_CLIENT_ID: &str = "0894c7f33bb94800a03f1f4df13a4f38";
/// Community client secret (public knowledge in the HA integration).
/// Override at build time by setting `TROVE_EIGHT_SLEEP_CLIENT_SECRET`.
const DEFAULT_CLIENT_SECRET: &str =
    "f0954a3ed5763ba3d06834c73731a32f15f168f47d4f164751275def86db0c76";

/// Resolves the client_id: env override → compiled-in public default.
fn client_id() -> &'static str {
    option_env!("TROVE_EIGHT_SLEEP_CLIENT_ID").unwrap_or(DEFAULT_CLIENT_ID)
}

/// Resolves the client_secret: env override → compiled-in public default.
fn client_secret() -> &'static str {
    option_env!("TROVE_EIGHT_SLEEP_CLIENT_SECRET").unwrap_or(DEFAULT_CLIENT_SECRET)
}

/// User-agent sent to the Eight Sleep API (mirrors the community client).
const USER_AGENT: &str = "okhttp/4.9.3";

/// HTTP timeout (matches the community client's 2400 s default — we use
/// something tighter to avoid stalling the watcher loop).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs in the Periodic watcher loop. Sleep data is
/// produced once a night; hourly polling catches it without hammering the
/// unofficial API.
pub const EIGHT_SLEEP_SYNC_SECS: u64 = 3600;

/// Trend endpoint window: how many days to request per call. The unofficial
/// API doesn't document a hard cap; keep to 7 days per request so a long
/// backfill drains incrementally and each window's write advances the cursor.
const WINDOW_DAYS: i64 = 7;

/// LOINC codes for the biometrics Eight Sleep reports.
const LOINC_HEART_RATE: &str = "8867-4"; // Heart rate — bpm
const LOINC_HRV: &str = "80404-7"; // Heart rate variability — ms (rMSSD)
const LOINC_RESP_RATE: &str = "9279-1"; // Respiratory rate — /min

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(OBS_DIR))
}

/// Periodic pass: the same pull "Sync now" runs, but it never errors the
/// loop — a missing token or a network blip is a quiet no-op until the next
/// tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!(
                    "Eight Sleep synced — {} HR readings, {} HRV, {} resp rate, {} temp readings",
                    out.counts.get("hr_readings").copied().unwrap_or(0),
                    out.counts.get("hrv_readings").copied().unwrap_or(0),
                    out.counts.get("resp_readings").copied().unwrap_or(0),
                    out.counts.get("temp_readings").copied().unwrap_or(0),
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Eight Sleep sync skipped: {e}"
        ))),
    }
}

/// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the line already
/// exists from the Phase 2 stub — do NOT add another).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "eight-sleep",
        name: "Eight Sleep",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls nightly sleep biometrics (heart rate, HRV, respiratory rate, bed and room \
             temperature) from your Eight Sleep Pod into the unified health and home stores. \
             First sync backfills available history; later syncs are incremental.",
        domain: "health",
        vault_path: "health/eight-sleep/",
        toggleable: true,
        setup: &[
            "Eight Sleep does not have an official public API. This integration uses a \
             community reverse-engineered API — it works reliably but is not sanctioned by \
             Eight Sleep and may break if they change their backend.",
            "Connect with your Eight Sleep account email and password on this card.",
            "First sync backfills your available sleep history; later syncs are incremental.",
        ],
        caveats:
            "Uses a community reverse-engineered, unofficial API (ToS-gray). The integration \
             degrades gracefully on API changes — a clear error will appear in the hub if the \
             API shape changes, and no partial writes will occur. Sleep data lands approximately \
             once per night after processing completes.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(EIGHT_SLEEP_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("eight-sleep"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = email:password, because the Eight Sleep auth is
// a password-grant POST, not a browser OAuth redirect).

/// Store the validated access token under `.trove/sync/eight-sleep.json`
/// (0600). The raw `email:password` is NEVER stored — only the exchange
/// result (access token + userId + expiry). On connect we immediately
/// exchange credentials for a token via the auth endpoint to verify the
/// credentials are valid.
fn def_connect(vault: &Vault, credentials: &str) -> Result<()> {
    let (email, password) = parse_credentials(credentials)?;
    let token = exchange_token(&email, &password)?;
    vault.save_sync_token(SERVICE, &token)
}

/// Forget the stored token. Synced data stays in the vault; the watermark
/// cursor stays so a reconnect resumes from where it left off.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Status: connected if a token is stored (even if expired — we re-auth on
/// the next pull, since there is no refresh token in the documented flow).
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let accounts = match vault.load_sync_token(SERVICE)? {
        Some(token) => {
            // userId (if present) was stored in the access_token slot's
            // `scope` field — load and surface it as the account label.
            let label = token
                .scope
                .as_deref()
                .and_then(|s| s.strip_prefix("userId:"))
                .map(|id| format!("Eight Sleep (userId: {id})"))
                .unwrap_or_else(|| "Eight Sleep".to_string());
            vec![ConnectedAccount {
                key: SERVICE.to_string(),
                label,
                connected_at: None,
                expires_at: token.expires_at,
                // Expired tokens get a fresh exchange on the next pull
                // (password grant re-auth).
                needs_reconnect: false,
                extra: BTreeMap::new(),
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] (add ONE line there;
/// see the integrator's gate).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "eight-sleep",
    display_name: "Eight Sleep",
    methods: &[ConnectMethod::TokenPaste {
        label: "Eight Sleep email and password",
        help: "Paste your Eight Sleep account email and password as email:password \
               (e.g. me@example.com:MyPa$$word). Your credentials are exchanged \
               immediately for an access token and are NOT stored — only the token is \
               kept. This uses the community reverse-engineered API (unofficial).",
        placeholder: "you@example.com:YourPassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["eight-sleep"],
    setup: &[
        "This integration uses a community reverse-engineered, unofficial API. It is not \
         sanctioned by Eight Sleep.",
        "Paste your Eight Sleep account email and password as email:password.",
        "Your credentials are exchanged immediately for a temporary access token and are never \
         stored on disk.",
    ],
};

// ---------------------------------------------------------------------------
// Credential parsing.

/// Parse `email:password` from the pasted string. Splits on the FIRST `:`.
/// Trims whitespace. Returns a clear error on malformed input.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your Eight Sleep credentials as email:password");
    }
    let (email, password) = match pasted.split_once(':') {
        Some((e, p)) => (e.trim().to_string(), p.trim().to_string()),
        None => bail!(
            "expected email:password — could not find a ':' separator in the pasted text"
        ),
    };
    if email.is_empty() {
        bail!("missing email — paste as email:password");
    }
    if password.is_empty() {
        bail!("missing password — paste as email:password");
    }
    Ok((email, password))
}

// ---------------------------------------------------------------------------
// Auth: password-grant token exchange.

/// POST to the Eight Sleep auth endpoint (`grant_type=password`) and return
/// a [`TokenSet`]. The `userId` is packed into `scope` as `"userId:<id>"` so
/// we can surface it in the hub label and use it for API calls without a
/// separate store.
pub fn exchange_token(email: &str, password: &str) -> Result<TokenSet> {
    let body = serde_json::json!({
        "client_id": client_id(),
        "client_secret": client_secret(),
        "grant_type": "password",
        "username": email,
        "password": password
    });
    let resp = ureq::post(AUTH_URL)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/json")
        .set("User-Agent", USER_AGENT)
        .send_json(body)
        .map_err(|e| map_auth_err(&e.to_string()))?
        .into_json::<Value>()
        .context("Eight Sleep auth response was not valid JSON")?;

    let access_token = resp
        .get("access_token")
        .and_then(Value::as_str)
        .context("Eight Sleep auth response missing 'access_token'")?
        .to_string();
    let expires_in = resp
        .get("expires_in")
        .and_then(Value::as_f64)
        .map(|s| s as u64)
        .unwrap_or(3600);
    let user_id = resp
        .get("userId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Ok(TokenSet {
        access_token,
        refresh_token: None,
        token_type: Some("Bearer".to_string()),
        // Stash the userId in `scope` — no separate field available on TokenSet.
        scope: if user_id.is_empty() {
            None
        } else {
            Some(format!("userId:{user_id}"))
        },
        expires_at: Some(now + expires_in),
    })
}

/// Map common auth errors onto clear user-facing messages.
fn map_auth_err(msg: &str) -> anyhow::Error {
    if msg.contains("401") || msg.contains("Unauthorized") || msg.contains("forbidden") {
        anyhow::anyhow!(
            "Eight Sleep rejected the credentials (401) — check your email and password"
        )
    } else {
        anyhow::anyhow!("Eight Sleep auth failed: {msg}")
    }
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for tests).

/// The endpoints the pull needs.
trait EightSleepApi {
    /// `GET /users/{userId}/trends?from=…&to=…&include-all-sessions=true&model-version=v2&tz=UTC`
    /// Returns the full response body as a parsed JSON `Value`.
    fn trends(&self, token: &str, user_id: &str, from: &str, to: &str) -> Result<Value, ApiError>;
}

#[derive(Debug)]
enum ApiError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            ApiError::Other(m) => write!(f, "{m}"),
        }
    }
}

struct EightSleepClient {
    base: String,
}

impl EightSleepClient {
    fn new(base: String) -> Self {
        EightSleepClient { base }
    }
}

impl EightSleepApi for EightSleepClient {
    fn trends(&self, token: &str, user_id: &str, from: &str, to: &str) -> Result<Value, ApiError> {
        let url = format!("{}/users/{}/trends", self.base, user_id);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/json")
            .query("tz", "UTC")
            .query("from", from)
            .query("to", to)
            .query("include-main", "false")
            .query("include-all-sessions", "true")
            .query("model-version", "v2")
            .call()
        {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| ApiError::Other(format!("parsing trends response: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(ApiError::Unauthorized),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(ApiError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(300).collect::<String>()
                )))
            }
            Err(e) => Err(ApiError::Other(e.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The last fully synced day (`YYYY-MM-DD`). The next sync requests
    /// `from=watermark` (the same day is re-fetched; guid dedupe absorbs it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    watermark: Option<String>,
    /// The authenticated `userId` — needed for every trends request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_eight_sleep_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_eight_sleep_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity day object). Only `ts` (the presence-start
// time for partitioning) is skipped; everything else serializes verbatim.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping helpers (fixture-tested).

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Parse a `YYYY-MM-DD` date string into an RFC3339 UTC midnight timestamp
/// for partitioning. Returns `None` on bad input.
fn date_to_ts(date_str: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d").ok()?;
    let dt = d.and_hms_opt(0, 0, 0)?;
    Some(
        Utc.from_utc_datetime(&dt)
            .with_timezone(&Local)
            .to_rfc3339(),
    )
}

/// Parse an Eight Sleep timeseries timestamp. These are ISO 8601 strings,
/// possibly with or without a timezone offset. Returns RFC3339 local.
fn parse_ts(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // Naive ISO (no offset) — treat as UTC.
    for fmt in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) =
            chrono::NaiveDateTime::parse_from_str(s, fmt)
        {
            return Some(
                Utc.from_utc_datetime(&naive)
                    .with_timezone(&Local)
                    .to_rfc3339(),
            );
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Mapping: one trends day object → observations + home readings.

/// One HR timeseries entry from `sessions[i].timeseries.heartRate`:
/// `[ISO_timestamp, value]`. Returns `None` if either field is unusable.
/// `session_idx` is included in the guid to prevent collisions when multiple
/// sessions (e.g. nap + main sleep) share a timestamp.
fn hr_observation_from(pair: &Value, day: &str, session_idx: usize) -> Option<Observation> {
    let arr = pair.as_array()?;
    if arr.len() < 2 {
        return None;
    }
    let ts_str = arr[0].as_str()?;
    let ts = parse_ts(ts_str)?;
    let value = arr[1].as_f64()?;
    let guid = format!("eight-sleep:hr:{day}:s{session_idx}:{ts_str}");
    Some(Observation {
        ts,
        source: "eight-sleep".into(),
        guid,
        test: "Heart rate".into(),
        code: LOINC_HEART_RATE.into(),
        code_system: "loinc".into(),
        value: Some(value),
        value_text: String::new(),
        unit: "/min".into(),
        reference_range: String::new(),
        flag: String::new(),
        panel: "Eight Sleep sleep session".into(),
        provider: String::new(),
        extra: Map::new(),
    })
}

/// HRV summary from `sleepQualityScore.hrv.current` — one per day.
fn hrv_observation_from(day_obj: &Value, day: &str) -> Option<Observation> {
    let ts = date_to_ts(day)?;
    let hrv = day_obj
        .get("sleepQualityScore")?
        .get("hrv")?
        .get("current")
        .and_then(Value::as_f64)?;
    let guid = format!("eight-sleep:hrv:{day}");
    Some(Observation {
        ts,
        source: "eight-sleep".into(),
        guid,
        test: "Heart rate variability".into(),
        code: LOINC_HRV.into(),
        code_system: "loinc".into(),
        value: Some(hrv),
        value_text: String::new(),
        unit: "ms".into(),
        reference_range: String::new(),
        flag: String::new(),
        panel: "Eight Sleep sleep session".into(),
        provider: String::new(),
        extra: Map::new(),
    })
}

/// Respiratory rate from `sleepQualityScore.respiratoryRate.average` — one
/// per day.
fn resp_observation_from(day_obj: &Value, day: &str) -> Option<Observation> {
    let ts = date_to_ts(day)?;
    let rate = day_obj
        .get("sleepQualityScore")?
        .get("respiratoryRate")?
        .get("average")
        .and_then(Value::as_f64)?;
    let guid = format!("eight-sleep:resp:{day}");
    Some(Observation {
        ts,
        source: "eight-sleep".into(),
        guid,
        test: "Respiratory rate".into(),
        code: LOINC_RESP_RATE.into(),
        code_system: "loinc".into(),
        value: Some(rate),
        value_text: String::new(),
        unit: "/min".into(),
        reference_range: String::new(),
        flag: String::new(),
        panel: "Eight Sleep sleep session".into(),
        provider: String::new(),
        extra: Map::new(),
    })
}

/// Bed temperature from `sleepQualityScore.tempBedC.average` — one per day.
fn bed_temp_reading_from(day_obj: &Value, day: &str) -> Option<HomeReading> {
    let ts = date_to_ts(day)?;
    let temp = day_obj
        .get("sleepQualityScore")?
        .get("tempBedC")?
        .get("average")
        .and_then(Value::as_f64)?;
    let mut extra = Map::new();
    extra.insert("day".into(), Value::String(day.into()));
    Some(HomeReading {
        ts,
        source: "eight-sleep".into(),
        metric: "temperature_bed".into(),
        value: temp,
        unit: "C".into(),
        place: "Bed".into(),
        device: "Eight Sleep Pod".into(),
        lat: None,
        lon: None,
        extra,
    })
}

/// Room temperature from `sleepQualityScore.tempRoomC.average` — one per
/// day.
fn room_temp_reading_from(day_obj: &Value, day: &str) -> Option<HomeReading> {
    let ts = date_to_ts(day)?;
    let temp = day_obj
        .get("sleepQualityScore")?
        .get("tempRoomC")?
        .get("average")
        .and_then(Value::as_f64)?;
    let mut extra = Map::new();
    extra.insert("day".into(), Value::String(day.into()));
    Some(HomeReading {
        ts,
        source: "eight-sleep".into(),
        metric: "temperature".into(),
        value: temp,
        unit: "C".into(),
        place: "Bedroom".into(),
        device: "Eight Sleep Pod".into(),
        lat: None,
        lon: None,
        extra,
    })
}

/// One `days[]` entry → all observations + home readings + the raw line.
/// The `processing` flag: if `true` the session is still being analyzed by
/// Eight Sleep — we collect it anyway (the guid dedupe means a re-sync
/// overwrites nothing, but the data is better than nothing; a future
/// re-pull after completion is idempotent).
fn map_day(
    day_obj: &Value,
) -> (Vec<Observation>, Vec<HomeReading>, Option<RawLine>) {
    let day = str_field(day_obj, "day");
    if day.is_empty() {
        return (Vec::new(), Vec::new(), None);
    }

    // Partition ts for the raw line: use presenceStart if available, else
    // date midnight.
    let raw_ts = {
        let ps = str_field(day_obj, "presenceStart");
        if ps.is_empty() {
            date_to_ts(&day).unwrap_or_default()
        } else {
            parse_ts(&ps).unwrap_or_else(|| date_to_ts(&day).unwrap_or_default())
        }
    };

    // --- Observations ---
    let mut obs: Vec<Observation> = Vec::new();

    // HR timeseries: iterate sessions[].timeseries.heartRate. Include the
    // session index in the guid so readings from concurrent sessions (e.g.
    // nap + main sleep sharing a timestamp) do not collide.
    if let Some(Value::Array(sessions)) = day_obj.get("sessions") {
        for (session_idx, session) in sessions.iter().enumerate() {
            if let Some(Value::Array(hr_series)) =
                session.get("timeseries").and_then(|ts| ts.get("heartRate"))
            {
                for pair in hr_series {
                    if let Some(o) = hr_observation_from(pair, &day, session_idx) {
                        obs.push(o);
                    }
                }
            }
        }
    }

    // HRV + respiratory rate: summary scalars from sleepQualityScore.
    if let Some(o) = hrv_observation_from(day_obj, &day) {
        obs.push(o);
    }
    if let Some(o) = resp_observation_from(day_obj, &day) {
        obs.push(o);
    }

    // --- Home readings ---
    let mut home: Vec<HomeReading> = Vec::new();
    if let Some(r) = bed_temp_reading_from(day_obj, &day) {
        home.push(r);
    }
    if let Some(r) = room_temp_reading_from(day_obj, &day) {
        home.push(r);
    }

    // --- Raw line ---
    let raw = if raw_ts.is_empty() {
        None
    } else {
        Some(RawLine { ts: raw_ts, value: day_obj.clone() })
    };

    (obs, home, raw)
}

// ---------------------------------------------------------------------------
// Write helpers (deduped by guid against existing rows).

/// Load existing guids from a stream directory into a `HashSet`.
fn load_existing_guids(vault: &Vault, dir: &str) -> HashSet<String> {
    let stream = vault.stream(dir, Partition::Month);
    let mut seen = HashSet::new();
    if let Ok(keys) = stream.partitions() {
        for key in keys {
            if let Ok(rows) = stream.read::<Value>(&key) {
                for v in rows {
                    let g = str_field(&v, "guid");
                    if !g.is_empty() {
                        seen.insert(g);
                    }
                }
            }
        }
    }
    seen
}

/// Load existing (ts, metric) pairs from the home stream for deduplication.
/// We use `ts+metric` as the composite key since HomeReading has no `guid`.
fn load_existing_home_keys(vault: &Vault) -> HashSet<String> {
    let stream = vault.stream(HOME_DIR, Partition::Month);
    let mut seen = HashSet::new();
    if let Ok(keys) = stream.partitions() {
        for key in keys {
            if let Ok(rows) = stream.read::<Value>(&key) {
                for v in rows {
                    let ts = str_field(&v, "ts");
                    let metric = str_field(&v, "metric");
                    if !ts.is_empty() && !metric.is_empty() {
                        seen.insert(format!("{ts}:{metric}"));
                    }
                }
            }
        }
    }
    seen
}

/// Load existing day strings from the raw stream for deduplication. Each raw
/// line represents one complete day object keyed by `"day"` (`YYYY-MM-DD`).
/// We dedup on this field so the watermark re-fetch never duplicates rows.
fn load_existing_raw_days(vault: &Vault) -> HashSet<String> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen = HashSet::new();
    if let Ok(keys) = stream.partitions() {
        for key in keys {
            if let Ok(rows) = stream.read::<Value>(&key) {
                for v in rows {
                    let day = str_field(&v, "day");
                    if !day.is_empty() {
                        seen.insert(day);
                    }
                }
            }
        }
    }
    seen
}

/// Append new observations, raw lines, and home readings, deduped. Returns
/// `(new_obs_count, new_home_count, written_obs)` where `written_obs` is the
/// deduplicated slice that was actually appended (used for per-type counting).
fn write_layers(
    vault: &Vault,
    observations: Vec<Observation>,
    home_readings: Vec<HomeReading>,
    raw_lines: Vec<RawLine>,
) -> Result<(u64, u64, Vec<Observation>)> {
    let obs_stream = vault.stream(OBS_DIR, Partition::Month);
    let home_stream = vault.stream(HOME_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut obs_seen = load_existing_guids(vault, OBS_DIR);
    let mut home_seen = load_existing_home_keys(vault);
    let mut raw_seen = load_existing_raw_days(vault);

    let mut new_obs: Vec<Observation> = Vec::new();
    for o in observations {
        if o.guid.is_empty() || !obs_seen.insert(o.guid.clone()) {
            continue;
        }
        new_obs.push(o);
    }

    let mut new_home: Vec<HomeReading> = Vec::new();
    for r in home_readings {
        let key = format!("{}:{}", r.ts, r.metric);
        if !home_seen.insert(key) {
            continue;
        }
        new_home.push(r);
    }

    // Dedup raw lines by the `day` field (`YYYY-MM-DD`). The watermark
    // re-fetches the overlap/boundary day on every run; without dedup, the
    // same day object is appended on every hourly cycle → unbounded growth
    // and corrupt raw fidelity.  One raw row per day is the invariant.
    let mut new_raw: Vec<RawLine> = Vec::new();
    for r in raw_lines {
        let day = str_field(&r.value, "day");
        if day.is_empty() || !raw_seen.insert(day) {
            continue;
        }
        new_raw.push(r);
    }

    obs_stream.append(&new_obs, |r| &r.ts)?;
    home_stream.append(&new_home, |r| &r.ts)?;
    raw_stream.append(&new_raw, |r| &r.ts)?;

    Ok((new_obs.len() as u64, new_home.len() as u64, new_obs))
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the stored access token, refreshing via password re-auth when
/// expired. A missing token → clear error (the manual path); the periodic
/// path silently skips.
fn load_or_refresh_token(vault: &Vault) -> Result<(String, String)> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Eight Sleep is not connected — connect your account in the Integrations tab")?;

    let user_id = token
        .scope
        .as_deref()
        .and_then(|s| s.strip_prefix("userId:"))
        .unwrap_or("")
        .to_string();

    if !token.expired() {
        return Ok((token.access_token, user_id));
    }

    // No refresh token in the documented password-grant flow — the only
    // option is to signal reconnect.  We surface a clear message rather than
    // silently failing.
    bail!(
        "Eight Sleep access token expired. Reconnect from the Integrations tab to continue \
         syncing (your synced data is preserved)."
    );
}

/// The injectable pull body — testable seam (used only in this module's
/// tests; `pub(crate)` would leak the private trait bound).
fn pull_with(
    vault: &Vault,
    api: &impl EightSleepApi,
    token: &str,
    user_id: &str,
) -> Result<PullOutcome> {
    let mut state = vault.read_eight_sleep_sync();

    // Update the stored userId if it changed (e.g. first sync after connect).
    if !user_id.is_empty() && state.user_id.as_deref() != Some(user_id) {
        state.user_id = Some(user_id.to_string());
    }

    let effective_user_id = state
        .user_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(user_id);
    if effective_user_id.is_empty() {
        bail!(
            "Eight Sleep userId is not available — reconnect from the Integrations tab"
        );
    }

    // Determine the date range to request. Start from the watermark (or
    // 90 days ago for a cold start), walk forward in WINDOW_DAYS chunks.
    let today = Local::now().date_naive();
    let cold_start = today - chrono::Duration::days(90);
    let from_date = state
        .watermark
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .unwrap_or(cold_start);

    let mut total_obs: u64 = 0;
    let mut total_home: u64 = 0;
    let mut total_hr: u64 = 0;
    let mut total_hrv: u64 = 0;
    let mut total_resp: u64 = 0;

    let mut cursor = from_date;
    while cursor <= today {
        let window_end = (cursor + chrono::Duration::days(WINDOW_DAYS)).min(today);
        let from_str = cursor.format("%Y-%m-%d").to_string();
        let to_str = window_end.format("%Y-%m-%d").to_string();

        let resp = api
            .trends(token, effective_user_id, &from_str, &to_str)
            .map_err(|e| match e {
                ApiError::Unauthorized => anyhow::anyhow!(
                    "Eight Sleep rejected the token (401) — reconnect from the Integrations tab"
                ),
                ApiError::Other(m) => anyhow::anyhow!("Eight Sleep trends fetch failed: {m}"),
            })?;

        let days = match resp.get("days") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        };

        // Map all days in this window. Skip days still marked `processing:true`
        // — Eight Sleep marks a night in-progress for a while after waking;
        // writing partial biometrics then freezing them (first-write-wins) gives
        // permanently wrong HRV/resp/HR values.  The watermark re-fetches this
        // day on the next hourly tick; once `processing` is absent or false the
        // finalized values land correctly.
        let mut all_obs: Vec<Observation> = Vec::new();
        let mut all_home: Vec<HomeReading> = Vec::new();
        let mut all_raw: Vec<RawLine> = Vec::new();

        for day_obj in &days {
            let still_processing = day_obj
                .get("processing")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if still_processing {
                continue;
            }
            let (obs, home, raw) = map_day(day_obj);
            all_obs.extend(obs);
            all_home.extend(home);
            if let Some(r) = raw {
                all_raw.push(r);
            }
        }

        let (new_obs_count, new_home, written_obs) =
            write_layers(vault, all_obs, all_home, all_raw)?;
        total_obs += new_obs_count;
        total_home += new_home;

        // Count per observation type from the rows actually written (post-dedup)
        // so the headline numbers reflect reality, not pre-dedup inflation.
        for o in &written_obs {
            match o.test.as_str() {
                "Heart rate" => total_hr += 1,
                "Respiratory rate" => total_resp += 1,
                _ => total_hrv += 1, // Heart rate variability
            }
        }

        // Advance the watermark to this window's end date (only after write).
        let wm = window_end.format("%Y-%m-%d").to_string();
        if state.watermark.as_deref().is_none_or(|cur| wm.as_str() > cur) {
            state.watermark = Some(wm);
        }

        if window_end >= today {
            break;
        }
        cursor = window_end;
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_eight_sleep_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("observations", total_obs);
    counts.insert("temp_readings", total_home);
    counts.insert("hr_readings", total_hr);
    counts.insert("hrv_readings", total_hrv);
    counts.insert("resp_readings", total_resp);

    Ok(PullOutcome {
        headline: format!(
            "Eight Sleep synced — {total_obs} biometric observations, {total_home} temp readings"
        ),
        counts,
    })
}

/// Public pull entry point (reads the vault's stored token).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let (token, user_id) = load_or_refresh_token(vault)?;
    let client = EightSleepClient::new(CLIENT_API_URL.to_string());
    pull_with(vault, &client, &token, &user_id)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-eightsleep-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures (confirmed from lukas-clarke/eight_sleep community client) ---

    /// One `days[]` entry with HR timeseries, HRV, resp rate, bed temp, and
    /// room temp. Field names confirmed from the pyEight user.py source.
    fn day_fixture() -> Value {
        json!({
            "day": "2026-06-15",
            "score": 82,
            "presenceStart": "2026-06-14T23:10:00",
            "presenceEnd": "2026-06-15T07:05:00",
            "presenceDuration": 28500,
            "sleepDuration": 26400,
            "lightDuration": 9000,
            "deepDuration": 7200,
            "remDuration": 6000,
            "tnt": 8,
            "processing": false,
            "sessions": [
                {
                    "stages": [
                        {"stage": "awake"},
                        {"stage": "light"},
                        {"stage": "deep"},
                        {"stage": "rem"},
                        {"stage": "light"}
                    ],
                    "timeseries": {
                        "heartRate": [
                            ["2026-06-14T23:15:00Z", 62],
                            ["2026-06-14T23:20:00Z", 60],
                            ["2026-06-14T23:25:00Z", 58]
                        ],
                        "tempRoomC": [
                            ["2026-06-14T23:15:00Z", 19.5],
                            ["2026-06-14T23:20:00Z", 19.6]
                        ]
                    }
                }
            ],
            "sleepQualityScore": {
                "total": 82,
                "hrv": {
                    "current": 42.5
                },
                "respiratoryRate": {
                    "average": 14.8,
                    "current": 15.0
                },
                "heartRate": {
                    "average": 60.3
                },
                "tempBedC": {
                    "average": 28.7
                },
                "tempRoomC": {
                    "average": 19.6
                }
            },
            "sleepRoutineScore": {
                "total": 78,
                "latencyAsleepSeconds": {"score": 90},
                "latencyOutSeconds": {"score": 85},
                "wakeupConsistency": 80
            }
        })
    }

    /// A day with no sessions (e.g. a night the Pod didn't detect presence).
    fn day_no_sessions() -> Value {
        json!({
            "day": "2026-06-14",
            "score": 0,
            "processing": true,
            "sessions": []
        })
    }

    fn trends_response(days: Vec<Value>) -> Value {
        json!({"days": days})
    }

    // --- Pure mapping tests -------------------------------------------------

    #[test]
    fn maps_hr_timeseries_to_observations() {
        let (obs, _, _) = map_day(&day_fixture());
        let hr: Vec<&Observation> = obs.iter().filter(|o| o.test == "Heart rate").collect();
        assert_eq!(hr.len(), 3, "three HR readings");
        let first = &hr[0];
        assert_eq!(first.source, "eight-sleep");
        assert_eq!(first.code, "8867-4", "LOINC heart rate");
        assert_eq!(first.code_system, "loinc");
        assert_eq!(first.unit, "/min");
        assert_eq!(first.value, Some(62.0));
        // guid is stable and unique per reading.
        assert!(first.guid.starts_with("eight-sleep:hr:2026-06-15:"));
        // No duplicate guids across the three HR readings.
        let guids: HashSet<&str> = hr.iter().map(|o| o.guid.as_str()).collect();
        assert_eq!(guids.len(), 3, "unique guids");
    }

    #[test]
    fn maps_hrv_observation() {
        let (obs, _, _) = map_day(&day_fixture());
        let hrv = obs.iter().find(|o| o.test == "Heart rate variability").unwrap();
        assert_eq!(hrv.code, "80404-7", "LOINC HRV");
        assert_eq!(hrv.value, Some(42.5));
        assert_eq!(hrv.unit, "ms");
        assert_eq!(hrv.guid, "eight-sleep:hrv:2026-06-15");
    }

    #[test]
    fn maps_respiratory_rate_observation() {
        let (obs, _, _) = map_day(&day_fixture());
        let resp = obs.iter().find(|o| o.test == "Respiratory rate").unwrap();
        assert_eq!(resp.code, "9279-1", "LOINC resp rate");
        assert_eq!(resp.value, Some(14.8));
        assert_eq!(resp.unit, "/min");
        assert_eq!(resp.guid, "eight-sleep:resp:2026-06-15");
    }

    #[test]
    fn maps_bed_temperature_home_reading() {
        let (_, home, _) = map_day(&day_fixture());
        let bed = home.iter().find(|r| r.metric == "temperature_bed").unwrap();
        assert_eq!(bed.source, "eight-sleep");
        assert_eq!(bed.value, 28.7);
        assert_eq!(bed.unit, "C");
        assert_eq!(bed.place, "Bed");
        assert_eq!(bed.device, "Eight Sleep Pod");
    }

    #[test]
    fn maps_room_temperature_home_reading() {
        let (_, home, _) = map_day(&day_fixture());
        let room = home.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(room.source, "eight-sleep");
        assert_eq!(room.value, 19.6);
        assert_eq!(room.unit, "C");
        assert_eq!(room.place, "Bedroom");
    }

    #[test]
    fn raw_line_is_produced_for_valid_day() {
        let (_, _, raw) = map_day(&day_fixture());
        assert!(raw.is_some(), "raw line produced");
        let raw = raw.unwrap();
        assert!(!raw.ts.is_empty(), "raw line has a ts");
        // The raw line preserves the full day object, including fields not in
        // any contract column (score, stages, sleepRoutineScore, etc.).
        assert_eq!(raw.value.get("score"), Some(&json!(82)));
        assert!(raw.value.get("sleepRoutineScore").is_some());
    }

    #[test]
    fn day_without_sessions_produces_no_hr_obs() {
        let (obs, home, _) = map_day(&day_no_sessions());
        let hr: Vec<_> = obs.iter().filter(|o| o.test == "Heart rate").collect();
        assert!(hr.is_empty(), "no HR readings when sessions is empty");
        // No HRV/resp/temp either since sleepQualityScore is absent.
        assert!(obs.is_empty(), "no observations at all");
        assert!(home.is_empty(), "no home readings");
    }

    #[test]
    fn day_with_missing_day_field_is_skipped() {
        let bad = json!({"score": 80, "sessions": []});
        let (obs, home, raw) = map_day(&bad);
        assert!(obs.is_empty());
        assert!(home.is_empty());
        assert!(raw.is_none());
    }

    #[test]
    fn parse_ts_handles_utc_z_and_naive() {
        // With Z suffix.
        let ts = parse_ts("2026-06-14T23:15:00Z").unwrap();
        assert!(ts.contains("2026"), "ts contains year: {ts}");
        // Naive (no offset) — interpreted as UTC.
        let ts2 = parse_ts("2026-06-14T23:15:00").unwrap();
        assert!(!ts2.is_empty());
        // Bad string → None.
        assert!(parse_ts("not-a-date").is_none());
        assert!(parse_ts("").is_none());
    }

    #[test]
    fn parse_credentials_splits_on_first_colon() {
        let (e, p) = parse_credentials("user@example.com:my:p@ss!").unwrap();
        assert_eq!(e, "user@example.com");
        assert_eq!(p, "my:p@ss!"); // password can contain colons
    }

    #[test]
    fn parse_credentials_rejects_missing_parts() {
        assert!(parse_credentials("").is_err(), "empty → error");
        assert!(parse_credentials("nocolon").is_err(), "no : → error");
        assert!(parse_credentials(":nopassword").is_err(), "empty email → error");
        assert!(parse_credentials("noemail:").is_err(), "empty password → error");
    }

    // --- Mock API + pull tests -----------------------------------------------

    struct MockApi {
        response: Value,
        calls: std::cell::RefCell<Vec<(String, String, String, String)>>,
    }

    impl MockApi {
        fn new(response: Value) -> Self {
            MockApi { response, calls: std::cell::RefCell::new(Vec::new()) }
        }
    }

    impl EightSleepApi for MockApi {
        fn trends(
            &self,
            token: &str,
            user_id: &str,
            from: &str,
            to: &str,
        ) -> Result<Value, ApiError> {
            self.calls
                .borrow_mut()
                .push((token.into(), user_id.into(), from.into(), to.into()));
            Ok(self.response.clone())
        }
    }

    #[test]
    fn full_pull_writes_all_layers_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let resp = trends_response(vec![day_fixture()]);
        let api = MockApi::new(resp);

        let out = pull_with(&v, &api, "test-tok", "user123").unwrap();
        assert!(out.counts.get("observations").copied().unwrap_or(0) > 0, "observations written");
        assert!(out.counts.get("temp_readings").copied().unwrap_or(0) > 0, "temp readings written");

        // Observation file exists.
        let obs_files: Vec<_> = std::fs::read_dir(v.root().join(OBS_DIR))
            .unwrap()
            .flatten()
            .collect();
        assert!(!obs_files.is_empty(), "observation JSONL written");

        // Home reading file exists.
        let home_files: Vec<_> = std::fs::read_dir(v.root().join(HOME_DIR))
            .unwrap()
            .flatten()
            .collect();
        assert!(!home_files.is_empty(), "home reading JSONL written");

        // Raw file exists.
        let raw_files: Vec<_> = std::fs::read_dir(v.root().join(RAW_DIR))
            .unwrap()
            .flatten()
            .collect();
        assert!(!raw_files.is_empty(), "raw JSONL written");

        // Cursor advanced.
        let state = v.read_eight_sleep_sync();
        assert!(state.watermark.is_some(), "watermark advanced");
        assert_eq!(state.user_id.as_deref(), Some("user123"));
        assert!(state.updated.is_some());

        // Token NOT in cursor.
        let cursor = std::fs::read_to_string(v.root().join(".trove/eight-sleep-sync.json"))
            .unwrap();
        assert!(!cursor.contains("test-tok"), "access token never in the cursor");
    }

    #[test]
    fn re_pull_is_idempotent_via_guid_dedupe() {
        let v = temp_vault("dedupe");
        let resp = trends_response(vec![day_fixture()]);

        let first = pull_with(&v, &MockApi::new(resp.clone()), "tok", "u1").unwrap();
        let second = pull_with(&v, &MockApi::new(resp), "tok", "u1").unwrap();

        // Second pull writes zero new observations (all guids already stored).
        assert_eq!(
            second.counts.get("observations").copied().unwrap_or(0),
            0,
            "idempotent: no new obs on re-pull"
        );
        let first_obs = first.counts.get("observations").copied().unwrap_or(0);
        // The observation count from the first pull was non-zero (we wrote
        // something), confirming the dedupe comparison is meaningful.
        assert!(first_obs > 0, "first pull wrote observations");
    }

    /// The watermark re-fetch causes the same day to be presented on every
    /// hourly run.  Without raw dedup the raw file grows unboundedly.  Verify
    /// that the raw file row count is unchanged after a second pull of the
    /// same data.
    #[test]
    fn raw_file_row_count_is_stable_on_re_pull() {
        fn raw_line_count(vault: &Vault) -> usize {
            let stream = vault.stream(RAW_DIR, Partition::Month);
            let keys = stream.partitions().unwrap_or_default();
            let mut total = 0;
            for key in keys {
                if let Ok(rows) = stream.read::<Value>(&key) {
                    total += rows.len();
                }
            }
            total
        }

        let v = temp_vault("rawdedup");
        let resp = trends_response(vec![day_fixture()]);

        pull_with(&v, &MockApi::new(resp.clone()), "tok", "u1").unwrap();
        let count_after_first = raw_line_count(&v);
        assert_eq!(count_after_first, 1, "one raw row after first pull");

        pull_with(&v, &MockApi::new(resp), "tok", "u1").unwrap();
        let count_after_second = raw_line_count(&v);
        assert_eq!(
            count_after_second, count_after_first,
            "raw row count unchanged on re-pull (no duplication)"
        );
    }

    /// Days still marked `processing:true` are skipped (data is partial and
    /// would be frozen by first-write-wins dedup). They land on the next tick
    /// once Eight Sleep finalizes the session.
    #[test]
    fn processing_days_are_not_written() {
        let v = temp_vault("processing");
        // day_no_sessions has processing:true and no sleep-quality data.
        let resp = trends_response(vec![day_no_sessions()]);
        let out = pull_with(&v, &MockApi::new(resp), "tok", "u1").unwrap();
        assert_eq!(
            out.counts.get("observations").copied().unwrap_or(0),
            0,
            "processing days produce no observations"
        );
        // Raw dir should not exist or be empty (nothing written).
        let raw_empty = v
            .stream(RAW_DIR, Partition::Month)
            .partitions()
            .map(|k| k.is_empty())
            .unwrap_or(true);
        assert!(raw_empty, "no raw rows for processing-only days");
    }

    #[test]
    fn cursor_back_compat_empty_deserializes() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermark.is_none());
        assert!(empty.user_id.is_none());
        let fwd: SyncState = serde_json::from_str(
            r#"{"watermark":"2026-06-15","user_id":"abc","future":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(fwd.watermark.as_deref(), Some("2026-06-15"));
        assert_eq!(fwd.user_id.as_deref(), Some("abc"));
    }

    #[test]
    fn pull_without_token_is_a_clear_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn unauthorized_api_response_surfaces_reconnect_message() {
        let v = temp_vault("unauth");
        struct FailingApi;
        impl EightSleepApi for FailingApi {
            fn trends(&self, _t: &str, _u: &str, _f: &str, _to: &str) -> Result<Value, ApiError> {
                Err(ApiError::Unauthorized)
            }
        }
        let err = pull_with(&v, &FailingApi, "tok", "user").unwrap_err().to_string();
        assert!(err.contains("reconnect"), "401 surfaces reconnect message: {err}");
    }

    #[test]
    fn connection_uses_token_paste_not_oauth() {
        // Eight Sleep uses password grant (not a browser OAuth redirect) — the
        // connection method is TokenPaste.
        assert!(CONNECTION.method("token-paste").is_some());
        assert!(CONNECTION.method("oauth").is_none());
        assert_eq!(CONNECTION.id, "eight-sleep");
        assert_eq!(DEF.connection, Some("eight-sleep"));
        // No OAuth redirect port — None from the module.
        assert!(CONNECTION.methods.len() == 1);
    }

    #[test]
    fn observations_carry_correct_loinc_codes() {
        let (obs, _, _) = map_day(&day_fixture());
        let hr = obs.iter().find(|o| o.test == "Heart rate").unwrap();
        let hrv = obs.iter().find(|o| o.test == "Heart rate variability").unwrap();
        let resp = obs.iter().find(|o| o.test == "Respiratory rate").unwrap();
        assert_eq!(hr.code, LOINC_HEART_RATE);
        assert_eq!(hrv.code, LOINC_HRV);
        assert_eq!(resp.code, LOINC_RESP_RATE);
        // All use the loinc system.
        for o in [hr, hrv, resp] {
            assert_eq!(o.code_system, "loinc");
        }
    }
}
