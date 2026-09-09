//! Moen Flo smart water monitor — cloud pull via the unofficial
//! `api.meetflo.com` API (the reverse-engineered path implemented by the
//! Home Assistant `flo` integration and the `aioflo` library).
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/moen-flo.md.
//!
//! A **Periodic** cloud pull of water telemetry and consumption.  Three output
//! streams per device:
//!
//! - **Raw** (`home/moen-flo/raw/YYYY-MM.jsonl`) — every API response object at
//!   full fidelity (unconditional).
//! - **HomeReading contract** (`home/moen-flo/YYYY-MM.jsonl`) — one row per
//!   telemetry metric (flow rate, water temperature, pressure) via the bound
//!   [`crate::home::HomeReading`] contract.
//! - **Consumption raw** (`home/moen-flo/energy/YYYY-MM.jsonl`) — hourly water
//!   usage intervals (gallons + timestamps) in the home.energy draft layout (raw
//!   JSONL — the home.energy contract is not yet bound; persisted raw until the
//!   pioneer binds it).
//! - **Events raw** (`home/moen-flo/events/YYYY-MM.jsonl`) — alarm/notification
//!   events (leak, health-test, etc.) in the home.event draft layout (raw JSONL —
//!   the home.event contract is not yet bound).
//!
//! ## Auth
//!
//! The unofficial API authenticates via `POST /api/v1/users/auth` with
//! `{username, password}` → a bearer token valid for `tokenExpiration` seconds.
//! Credentials are pasted as `email:password` (the Emporia pattern), stored in
//! `.trove/sync/moen-flo.json` (0600) as the `access_token` (bearer) and
//! `refresh_token` (password, for re-auth on expiry).
//!
//! ## Cursor
//!
//! `.trove/moen-flo-sync.json` (non-secret, rebuildable) holds per-location
//! consumption watermarks (`location_id → RFC3339 of the latest interval start
//! ever written`) and the last-sync time.  The watermark advances only after the
//! full consumption drain for that location.
//!
//! ## Unofficial API — field sources
//!
//! Evidence from the `aioflo` Python library test fixtures and `flo` HA
//! coordinator:
//! - Login: `POST /api/v1/users/auth` → `{token, tokenExpiration,
//!   tokenPayload.user.user_id}`
//! - User info: `GET /api/v2/users/{user_id}?expand=locations` →
//!   `{locations: [{id, devices: [{id, macAddress, nickname, deviceType, …}]}]}`
//! - Device info: `GET /api/v2/devices/{device_id}` →
//!   `{telemetry.current.{gpm,psi,tempF,updated}, valve.lastKnown,
//!    systemMode.lastKnown, notifications.pending, serialNumber, macAddress, …}`
//! - Consumption: `GET /api/v2/water/consumption?locationId=&startDate=&endDate=
//!   &interval=1h` → `{items:[{time,gallonsConsumed}], aggregations.sumTotalGallonsConsumed}`
//! - Metrics: `GET /api/v2/water/metrics?macAddress=&startDate=&endDate=&interval=1h`
//!   → `{items:[{time,averageGpm,averagePsi,averageTempF}]}`

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const DIR: &str = "home/moen-flo";
const RAW_DIR: &str = "home/moen-flo/raw";
const ENERGY_DIR: &str = "home/moen-flo/energy";
const EVENTS_DIR: &str = "home/moen-flo/events";

/// Non-secret rebuildable cursor; NOT under `.trove/sync/` (that's 0600 secrets).
const SYNC_FILE: &str = ".trove/moen-flo-sync.json";

/// Service id under `.trove/sync/` where the bearer + re-auth creds are stored.
const SERVICE: &str = "moen-flo";

const API_V1_BASE: &str = "https://api.meetflo.com/api/v1";
const API_V2_BASE: &str = "https://api.meetflo.com/api/v2";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// One sync poll every hour; consumption aggregates are daily but telemetry
/// snapshots are worth capturing hourly.
pub const MOEN_FLO_SYNC_SECS: u64 = 3600;

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
            let n = out
                .counts
                .get("readings")
                .copied()
                .unwrap_or(0)
                .saturating_add(out.counts.get("intervals").copied().unwrap_or(0));
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("Moen Flo synced — {n} records")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "Moen Flo sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let readings = out.counts.get("readings").copied().unwrap_or(0);
    let intervals = out.counts.get("intervals").copied().unwrap_or(0);
    let headline = if readings + intervals == 0 {
        "Moen Flo is up to date — no new data".to_string()
    } else {
        format!("Moen Flo synced — {readings} readings, {intervals} consumption intervals")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "moen-flo",
        name: "Moen Flo",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Collects water usage, flow rate, and leak-detection events from \
                      your Moen Flo smart water monitor, providing a local record of \
                      household water consumption.",
        domain: "home",
        vault_path: "home/moen-flo/",
        toggleable: true,
        setup: &[
            "Enter your Flo account email and password as email:password on the connect card.",
            "First sync pulls all available consumption history and current telemetry.",
            "Note: uses the unofficial Flo cloud API (same as the Home Assistant integration) \
             — Moen may change it without notice.",
        ],
        caveats: "Uses an unofficial API (the Home Assistant Flo integration is the \
                  reference implementation) — it may break without notice. No local \
                  API or official export is available.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(MOEN_FLO_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("moen-flo"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection definition.

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (email, password) = parse_credentials(pasted)?;
    // Authenticate to verify credentials and obtain a bearer token.
    let token_set = flo_login(API_V1_BASE, &email, &password)
        .context("Moen Flo login failed — check your email and password")?;
    vault.save_sync_token(SERVICE, &token_set)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(tok) = vault.load_sync_token(SERVICE)? {
        // email is stored in token_type (the Emporia convention for non-OAuth services).
        let label = tok.token_type.unwrap_or_else(|| "Flo Account".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: tok.expires_at,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
/// Credentials are pasted as `email:password` (the Emporia pattern).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "moen-flo",
    display_name: "Moen Flo",
    methods: &[ConnectMethod::TokenPaste {
        label: "Flo account email and password",
        help: "Enter your Flo by Moen account email and password as email:password. \
               Credentials authenticate against Moen's cloud service and are stored \
               locally (0600) — never sent anywhere except Moen's official servers. \
               This integration uses an unofficial, community-maintained API path \
               (the same one used by the Home Assistant Flo integration).",
        placeholder: "you@example.com:yourpassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["moen-flo"],
    setup: &[
        "Use your existing Flo by Moen app account email and password.",
        "Paste them as email:password on this card.",
        "Note: this integration uses an unofficial API — it may break if \
         Moen changes their backend.",
    ],
};

// ---------------------------------------------------------------------------
// Credential parsing (email:password — first `:` is the split).

fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let s = pasted.trim();
    if s.is_empty() {
        bail!("empty — paste your Flo credentials as email:password");
    }
    match s.split_once(':') {
        Some((u, p)) => {
            let u = u.trim().to_string();
            let p = p.trim().to_string();
            if u.is_empty() {
                bail!("missing email address");
            }
            if p.is_empty() {
                bail!("missing password");
            }
            Ok((u, p))
        }
        None => bail!("paste credentials as email:password"),
    }
}

// ---------------------------------------------------------------------------
// Auth: POST /api/v1/users/auth → bearer token + user_id.

/// Response from `/api/v1/users/auth`.
#[derive(Deserialize)]
struct AuthResponse {
    token: String,
    #[serde(rename = "tokenExpiration", default)]
    token_expiration: u64,
    #[serde(rename = "tokenPayload")]
    token_payload: AuthPayload,
}

#[derive(Deserialize)]
struct AuthPayload {
    user: AuthUser,
}

#[derive(Deserialize)]
struct AuthUser {
    #[serde(rename = "user_id")]
    user_id: String,
}

/// Authenticate and return a [`TokenSet`] with the bearer in `access_token`,
/// the email in `token_type`, the password in `refresh_token` (for re-auth on
/// expiry), and `expires_at` set from `tokenExpiration`.
fn flo_login(base: &str, email: &str, password: &str) -> Result<TokenSet> {
    let url = format!("{base}/users/auth");
    let body = serde_json::json!({ "username": email, "password": password });
    let resp = ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/json")
        .send_json(&body)
        .context("Flo auth request failed")?;
    let auth: AuthResponse =
        resp.into_json().context("parsing Flo auth response")?;
    // Store email in token_type for the status display, password in
    // refresh_token so we can re-authenticate on token expiry without
    // prompting the user again.
    let expires_at: Option<u64> = if auth.token_expiration > 0 {
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        Some(now_secs + auth.token_expiration)
    } else {
        None
    };
    Ok(TokenSet {
        access_token: auth.token,
        refresh_token: Some(password.to_string()),
        token_type: Some(email.to_string()),
        scope: Some(auth.token_payload.user.user_id),
        expires_at,
    })
}

// ---------------------------------------------------------------------------
// HTTP client.

struct FloClient {
    base_v2: String,
    token: String,
}

impl FloClient {
    fn new(_base_v1: &str, base_v2: &str, token: &str) -> Self {
        FloClient {
            base_v2: base_v2.to_string(),
            token: token.to_string(),
        }
    }

    fn get(&self, url: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .call()
        {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parse error: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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

    fn get_with_query(&self, url: &str, params: &[(&str, &str)]) -> Result<Value, FetchError> {
        let mut req = ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json");
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parse error: {e}"))),
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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

    /// `GET /api/v2/users/{user_id}?expand=locations` → user + locations + devices.
    fn user_info(&self, user_id: &str) -> Result<Value, FetchError> {
        let url = format!("{}/users/{user_id}", self.base_v2);
        self.get_with_query(&url, &[("expand", "locations")])
    }

    /// `GET /api/v2/devices/{device_id}` → full device snapshot.
    fn device_info(&self, device_id: &str) -> Result<Value, FetchError> {
        let url = format!("{}/devices/{device_id}", self.base_v2);
        self.get(&url)
    }

    /// `GET /api/v2/water/consumption` → hourly consumption items for a location.
    fn consumption(
        &self,
        location_id: &str,
        start_date: &str,
        end_date: &str,
    ) -> Result<Value, FetchError> {
        let url = format!("{}/water/consumption", self.base_v2);
        self.get_with_query(
            &url,
            &[
                ("locationId", location_id),
                ("startDate", start_date),
                ("endDate", end_date),
                ("interval", "1h"),
            ],
        )
    }
}

// ---------------------------------------------------------------------------
// Fetch error.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor.

/// Snapshot of the last-emitted alert state for one device, used to suppress
/// duplicate event rows when `notifications.pending` is a standing snapshot
/// (i.e., counts have not changed since the previous poll).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct AlertState {
    critical: u64,
    warning: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Per-location (location_id → RFC3339 of latest consumption interval
    /// start ever written) watermarks.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    consumption_watermarks: BTreeMap<String, String>,
    /// Per-device alert state as of the last poll.  Only updated (and an
    /// event row emitted) when the counts change from the previous poll.
    /// On first run the state is absent → treated as `{critical:0,warning:0}`
    /// establishing a silent baseline, so a standing alert at first-run
    /// does NOT generate a spurious row.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    alert_states: BTreeMap<String, AlertState>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_moen_flo_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_moen_flo_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shapes.

/// Wrapper for raw API blobs — skips the inner `data` from serialization so
/// the file contains the device info directly.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    data: Value,
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// Extract `telemetry.current.{gpm,psi,tempF,updated}` from a device info
/// response and convert to [`HomeReading`] rows (one per present numeric
/// metric). Returns empty Vec on any missing/bad data.
fn telemetry_to_readings(device: &Value) -> Vec<HomeReading> {
    let Some(current) = device
        .get("telemetry")
        .and_then(|t| t.get("current"))
    else {
        return Vec::new();
    };
    let ts_raw = current.get("updated").and_then(Value::as_str).unwrap_or("");
    let ts = if ts_raw.is_empty() {
        return Vec::new();
    } else {
        // Convert UTC ISO to local RFC3339.
        match DateTime::parse_from_rfc3339(ts_raw) {
            Ok(dt) => dt.with_timezone(&Local).to_rfc3339(),
            Err(_) => return Vec::new(),
        }
    };
    if Partition::Month.key(&ts).is_none() {
        return Vec::new();
    }

    let device_id = device.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    let nickname =
        device.get("nickname").and_then(Value::as_str).unwrap_or("").to_string();

    let metrics: &[(&str, &str, &str)] = &[
        ("gpm", "flow_rate", "gal_min"),
        ("psi", "water_pressure", "psi"),
        ("tempF", "water_temperature", "F"),
    ];

    let mut rows = Vec::new();
    for (field, metric, unit) in metrics {
        let Some(v) = current.get(*field).and_then(Value::as_f64) else {
            continue;
        };
        let mut r = HomeReading::new("moen-flo", *metric, v, ts.clone());
        r.unit = unit.to_string();
        r.device = device_id.clone();
        if !nickname.is_empty() {
            r.place = nickname.clone();
        }
        // Store the valve state and system mode in extra for context.
        let valve = device
            .get("valve")
            .and_then(|v| v.get("lastKnown"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !valve.is_empty() {
            r.extra.insert("valve_state".into(), valve.into());
        }
        rows.push(r);
    }
    rows
}

/// Convert a consumption API response into home.energy-shaped raw JSONL rows.
/// Each item: `{ts, source, device, circuit, value, unit, interval_secs, direction, guid}`.
fn consumption_to_energy_rows(
    resp: &Value,
    location_id: &str,
) -> Vec<Value> {
    let items = match resp.get("items").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut rows = Vec::new();
    for item in items {
        let time_raw = item.get("time").and_then(Value::as_str).unwrap_or("");
        if time_raw.is_empty() {
            continue;
        }
        // Normalize to local RFC3339.
        let ts = match DateTime::parse_from_rfc3339(time_raw) {
            Ok(dt) => dt.with_timezone(&Local).to_rfc3339(),
            Err(_) => continue,
        };
        if Partition::Month.key(&ts).is_none() {
            continue;
        }
        let gallons = match item.get("gallonsConsumed").and_then(Value::as_f64) {
            Some(g) => g,
            None => continue,
        };
        let guid = format!("moen-flo:{}:{}", location_id, time_raw);
        let row = serde_json::json!({
            "ts": ts,
            "source": "moen-flo",
            "circuit": location_id,
            "value": gallons,
            "unit": "gal",
            "interval_secs": 3600,
            "direction": "consumption",
            "guid": guid,
        });
        rows.push(row);
    }
    rows
}

// ---------------------------------------------------------------------------
// Writers.

/// Append raw device-info or consumption snapshot to the raw stream.
fn append_raw(vault: &Vault, ts: &str, data: Value) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let line = RawLine { ts: ts.to_string(), data };
    stream.append(&[line], |r| &r.ts)
}

/// Append HomeReading contract rows, deduplicating by a stable natural key
/// `moen-flo:{device_id}:{metric}:{ts}` stored in `extra.guid`.
fn append_readings(vault: &Vault, readings: &[HomeReading]) -> Result<u64> {
    if readings.is_empty() {
        return Ok(0);
    }
    // Build seen-guid set from existing partitions.
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for r in stream.read::<HomeReading>(&key)? {
            if let Some(g) = r.extra.get("guid").and_then(Value::as_str) {
                seen.insert(g.to_string());
            }
        }
    }
    // Tag each reading with a stable guid and filter already-seen.
    let mut fresh: Vec<HomeReading> = Vec::new();
    for r in readings {
        let guid = format!("moen-flo:{}:{}:{}", r.device, r.metric, r.ts);
        if seen.insert(guid.clone()) {
            let mut r2 = r.clone();
            r2.extra.insert("guid".into(), guid.into());
            fresh.push(r2);
        }
    }
    let n = fresh.len() as u64;
    stream.append(&fresh, |r| &r.ts)?;
    Ok(n)
}

/// Append home.energy-shaped consumption rows (raw JSONL), deduplicating by `guid`.
fn append_energy(vault: &Vault, rows: &[Value]) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(ENERGY_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                seen.insert(g.to_string());
            }
        }
    }
    // Wrap each row so the stream partition key comes from `ts`.
    #[derive(Serialize)]
    struct EnergyLine {
        #[serde(skip)]
        partition_ts: String,
        #[serde(flatten)]
        data: Value,
    }
    let fresh: Vec<EnergyLine> = rows
        .iter()
        .filter(|r| {
            let g = r.get("guid").and_then(Value::as_str).unwrap_or("");
            g.is_empty() || seen.insert(g.to_string())
        })
        .map(|r| {
            let ts = r.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
            EnergyLine { partition_ts: ts, data: r.clone() }
        })
        .collect();
    let n = fresh.len() as u64;
    stream.append(&fresh, |r| &r.partition_ts)?;
    Ok(n)
}

/// Append a home.event-shaped alarm event row (raw JSONL).
fn append_event(vault: &Vault, ts: &str, event: Value) -> Result<()> {
    let stream = vault.stream(EVENTS_DIR, Partition::Month);
    #[derive(Serialize)]
    struct EventLine {
        #[serde(skip)]
        partition_ts: String,
        #[serde(flatten)]
        data: Value,
    }
    let line = EventLine { partition_ts: ts.to_string(), data: event };
    stream.append(&[line], |r| &r.partition_ts)
}

// ---------------------------------------------------------------------------
// Token resolution + refresh.

/// Load the stored bearer token, re-authenticating via stored credentials if
/// expired or absent.
fn ensure_token(vault: &Vault) -> Result<(String, String)> {
    let tok = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| anyhow::anyhow!("Moen Flo not connected — paste credentials on the hub card"))?;
    // `scope` holds the user_id from initial login.
    let user_id = tok.scope.clone().unwrap_or_default();
    // Re-auth when within 5 minutes of expiry or if already expired.
    let needs_refresh = tok
        .expires_at
        .map(|exp_epoch| {
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            // 300-second margin (5 minutes).
            exp_epoch < now_secs + 300
        })
        .unwrap_or(false);
    if needs_refresh {
        let email = tok.token_type.as_deref().unwrap_or("");
        let password = tok.refresh_token.as_deref().unwrap_or("");
        if email.is_empty() || password.is_empty() {
            bail!("Moen Flo token expired — reconnect on the hub card");
        }
        let new_tok = flo_login(API_V1_BASE, email, password)
            .context("Moen Flo token refresh failed")?;
        let new_user_id = new_tok.scope.clone().unwrap_or(user_id);
        vault.save_sync_token(SERVICE, &new_tok)?;
        return Ok((new_tok.access_token, new_user_id));
    }
    Ok((tok.access_token, user_id))
}

// ---------------------------------------------------------------------------
// Core pull logic.

/// Pull all accounts, devices, telemetry and consumption.  Returns counts for
/// `readings`, `intervals`, `raw_blobs`.
fn pull(vault: &Vault) -> Result<PullOutcome> {
    let (token, user_id) = ensure_token(vault)?;
    if user_id.is_empty() {
        bail!("Moen Flo user_id not stored — reconnect on the hub card");
    }
    let client = FloClient::new(API_V1_BASE, API_V2_BASE, &token);
    let mut state = vault.read_moen_flo_sync();

    let now_local = Local::now();
    let now_ts = now_local.to_rfc3339();

    // Fetch user info with locations + devices.
    let user_info = client
        .user_info(&user_id)
        .map_err(|e| anyhow::anyhow!("user info: {e}"))?;
    // Raw: user info blob.
    append_raw(vault, &now_ts, user_info.clone())?;

    let mut total_readings: u64 = 0;
    let mut total_intervals: u64 = 0;

    let locations = user_info
        .get("locations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for location in &locations {
        let location_id = match location.get("id").and_then(Value::as_str) {
            Some(id) if !id.is_empty() => id.to_string(),
            _ => continue,
        };

        // Collect telemetry from each device in this location.
        let devices = location
            .get("devices")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for device in &devices {
            let device_id = match device.get("id").and_then(Value::as_str) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            // Full device snapshot (telemetry, valve, alarms, …).
            let dev_info = match client.device_info(&device_id) {
                Ok(v) => v,
                Err(e) => {
                    // Non-fatal: log and continue.
                    let _ = append_raw(
                        vault,
                        &now_ts,
                        serde_json::json!({ "error": e.to_string(), "device_id": device_id }),
                    );
                    continue;
                }
            };
            // Raw: full device snapshot.
            append_raw(vault, &now_ts, dev_info.clone())?;

            // Contract: telemetry → HomeReading rows.
            let readings = telemetry_to_readings(&dev_info);
            total_readings += append_readings(vault, &readings)?;

            // Events: emit a home.event row only when the alert COUNTS
            // CHANGE from the last recorded state (diff-based, not snapshot-
            // based).  `notifications.pending` is a current-state snapshot,
            // NOT an event stream — appending on every poll whenever counts
            // are >0 would produce ~24 identical rows per day for any device
            // with a standing warning.
            //
            // First-run behaviour: the stored state is absent (treated as
            // {critical:0, warning:0}).  If a device already has standing
            // alerts at first run we do NOT emit a row, because we have no
            // way to know when those alerts were raised — we establish a
            // baseline and will emit on the NEXT change.
            let pending = dev_info
                .get("notifications")
                .and_then(|n| n.get("pending"));
            if let Some(pending) = pending {
                let critical = pending
                    .get("criticalCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let warning = pending
                    .get("warningCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let new_state = AlertState { critical, warning };
                let prev_state = state
                    .alert_states
                    .get(&device_id)
                    .cloned()
                    .unwrap_or_default(); // default = {critical:0, warning:0}
                // Emit only on state change; always update the stored state.
                if new_state != prev_state {
                    // Only emit non-zero → zero transition OR non-zero state.
                    // (Zero → zero is the default baseline and never reaches here
                    // because prev==new_state==default.)
                    let event_type = if critical == 0 && warning == 0 {
                        "alert_cleared"
                    } else {
                        "alert"
                    };
                    let event = serde_json::json!({
                        "ts": now_ts,
                        "source": "moen-flo",
                        "device": device_id,
                        "event": event_type,
                        "extra": {
                            "critical_count": critical,
                            "warning_count": warning,
                            "alarm_count": pending.get("alarmCount"),
                            "prev_critical_count": prev_state.critical,
                            "prev_warning_count": prev_state.warning,
                        }
                    });
                    if Partition::Month.key(&now_ts).is_some() {
                        let _ = append_event(vault, &now_ts, event);
                    }
                }
                // Update stored state regardless of whether we emitted (so the
                // next poll compares against the current counts).
                state.alert_states.insert(device_id.clone(), new_state);
            }
        }

        // Consumption: hourly intervals from the location watermark to now.
        // The HA flo coordinator requests one day at a time (midnight–23:59:59).
        // We follow the same pattern: chunk by day across the gap so that a
        // window-scoped API never silently truncates a large backfill.  Cap
        // the first-sync default at 30 days (documented limit of this path).
        let watermark = state.consumption_watermarks.get(&location_id).cloned();
        let now_utc = chrono::Utc::now();
        let first_sync_start = now_utc - chrono::Duration::days(30);
        let start_dt: DateTime<chrono::Utc> = watermark
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or(first_sync_start);

        // Build per-day windows: [day_start, day_end) stepping forward until now.
        // Align to UTC midnight boundaries (matching the HA coordinator pattern).
        use chrono::{Datelike, TimeZone};
        let start_day =
            chrono::Utc.with_ymd_and_hms(start_dt.year(), start_dt.month(), start_dt.day(), 0, 0, 0)
                .single()
                .unwrap_or(start_dt);
        let mut day_cursor = start_day;
        let mut newest_ts: Option<String> = None;

        while day_cursor <= now_utc {
            let day_end = day_cursor + chrono::Duration::days(1);
            let window_end = day_end.min(now_utc);
            // Skip zero-length windows that can arise on the last partial day.
            if window_end <= day_cursor {
                break;
            }
            let start_str = day_cursor.to_rfc3339_opts(SecondsFormat::Secs, true);
            let end_str = window_end.to_rfc3339_opts(SecondsFormat::Secs, true);

            let consumption_resp = match client.consumption(&location_id, &start_str, &end_str) {
                Ok(v) => v,
                Err(e) => {
                    // Non-fatal: log and break out of the day loop for this location.
                    let _ = append_raw(
                        vault,
                        &now_ts,
                        serde_json::json!({ "error": e.to_string(), "location_id": location_id }),
                    );
                    break;
                }
            };

            // Raw: consumption response (one per day window).
            append_raw(vault, &now_ts, consumption_resp.clone())?;

            let rows = consumption_to_energy_rows(&consumption_resp, &location_id);
            // Track the newest interval ts for the watermark.
            for row in &rows {
                if let Some(ts) = row.get("ts").and_then(Value::as_str) {
                    match &newest_ts {
                        None => newest_ts = Some(ts.to_string()),
                        Some(prev) if ts > prev.as_str() => newest_ts = Some(ts.to_string()),
                        _ => {}
                    }
                }
            }
            let n = append_energy(vault, &rows)?;
            total_intervals += n;

            day_cursor = day_end;
        }

        // Advance watermark only after a successful drain of all days.
        if let Some(ts) = newest_ts {
            state.consumption_watermarks.insert(location_id.clone(), ts);
        }
    }

    state.updated = Some(now_ts);
    vault.write_moen_flo_sync(&state)?;

    let mut counts = BTreeMap::new();
    counts.insert("readings", total_readings);
    counts.insert("intervals", total_intervals);
    Ok(PullOutcome { headline: String::new(), counts })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- parse_credentials --------------------------------------------------

    #[test]
    fn parse_creds_splits_on_first_colon() {
        let (u, p) = parse_credentials("user@example.com:mypassword").unwrap();
        assert_eq!(u, "user@example.com");
        assert_eq!(p, "mypassword");
    }

    #[test]
    fn parse_creds_password_may_contain_colons() {
        let (u, p) = parse_credentials("user@example.com:pass:with:colons").unwrap();
        assert_eq!(u, "user@example.com");
        assert_eq!(p, "pass:with:colons");
    }

    #[test]
    fn parse_creds_empty_rejects() {
        assert!(parse_credentials("").is_err());
    }

    #[test]
    fn parse_creds_no_colon_rejects() {
        assert!(parse_credentials("justanemail").is_err());
    }

    #[test]
    fn parse_creds_missing_password_rejects() {
        assert!(parse_credentials("user@example.com:").is_err());
    }

    // ---- telemetry_to_readings ----------------------------------------------

    fn fixture_device() -> Value {
        json!({
            "id": "98765",
            "nickname": "Smart Water Shutoff",
            "deviceType": "flo_device_v2",
            "macAddress": "111111111111",
            "telemetry": {
                "current": {
                    "gpm": 0.0,
                    "psi": 54.2,
                    "tempF": 70.0,
                    "updated": "2020-07-24T12:20:58Z"
                }
            },
            "valve": {
                "lastKnown": "open",
                "target": "open"
            },
            "systemMode": {
                "lastKnown": "home",
                "target": "home"
            },
            "notifications": {
                "pending": {
                    "warningCount": 2,
                    "criticalCount": 0,
                    "alarmCount": [
                        {"id": 30, "severity": "warning", "count": 1}
                    ]
                }
            }
        })
    }

    #[test]
    fn telemetry_three_metrics_emitted() {
        let dev = fixture_device();
        let rows = telemetry_to_readings(&dev);
        assert_eq!(rows.len(), 3, "expected flow_rate + water_pressure + water_temperature");
        let metrics: Vec<&str> = rows.iter().map(|r| r.metric.as_str()).collect();
        assert!(metrics.contains(&"flow_rate"), "flow_rate missing");
        assert!(metrics.contains(&"water_pressure"), "water_pressure missing");
        assert!(metrics.contains(&"water_temperature"), "water_temperature missing");
    }

    #[test]
    fn telemetry_values_correct() {
        let dev = fixture_device();
        let rows = telemetry_to_readings(&dev);
        let psi = rows.iter().find(|r| r.metric == "water_pressure").unwrap();
        assert!((psi.value - 54.2).abs() < 0.01);
        assert_eq!(psi.unit, "psi");
        assert_eq!(psi.device, "98765");
        assert_eq!(psi.place, "Smart Water Shutoff");
    }

    #[test]
    fn telemetry_valve_state_in_extra() {
        let dev = fixture_device();
        let rows = telemetry_to_readings(&dev);
        for r in &rows {
            let valve = r.extra.get("valve_state").and_then(Value::as_str).unwrap_or("");
            assert_eq!(valve, "open", "valve_state missing from extra");
        }
    }

    #[test]
    fn telemetry_missing_updated_returns_empty() {
        let mut dev = fixture_device();
        dev["telemetry"]["current"].as_object_mut().unwrap().remove("updated");
        let rows = telemetry_to_readings(&dev);
        assert!(rows.is_empty());
    }

    #[test]
    fn telemetry_no_telemetry_field_returns_empty() {
        let dev = json!({ "id": "x" });
        assert!(telemetry_to_readings(&dev).is_empty());
    }

    // ---- consumption_to_energy_rows -----------------------------------------

    fn fixture_consumption() -> Value {
        json!({
            "params": {
                "startDate": "2020-01-16T07:00:00.000Z",
                "endDate": "2020-01-17T07:00:00.000Z",
                "interval": "1h",
                "tz": "US/Mountain",
                "locationId": "mmnnoopp"
            },
            "aggregations": {
                "sumTotalGallonsConsumed": 3.674
            },
            "items": [
                { "time": "2020-01-16T00:00:00-07:00", "gallonsConsumed": 0.04 },
                { "time": "2020-01-16T01:00:00-07:00", "gallonsConsumed": 0.477 },
                { "time": "2020-01-16T03:00:00-07:00", "gallonsConsumed": 0.442 },
                { "time": "2020-01-16T07:00:00-07:00", "gallonsConsumed": 1.216 },
                { "time": "2020-01-16T08:00:00-07:00", "gallonsConsumed": 1.499 }
            ]
        })
    }

    #[test]
    fn consumption_rows_correct_count() {
        let resp = fixture_consumption();
        let rows = consumption_to_energy_rows(&resp, "mmnnoopp");
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn consumption_row_shape() {
        let resp = fixture_consumption();
        let rows = consumption_to_energy_rows(&resp, "mmnnoopp");
        let row = &rows[0];
        assert_eq!(row["source"].as_str().unwrap(), "moen-flo");
        assert_eq!(row["unit"].as_str().unwrap(), "gal");
        assert_eq!(row["interval_secs"].as_u64().unwrap(), 3600);
        assert_eq!(row["direction"].as_str().unwrap(), "consumption");
        assert!(!row["guid"].as_str().unwrap().is_empty());
        // gallonsConsumed=0.04 → value=0.04
        assert!((row["value"].as_f64().unwrap() - 0.04).abs() < 0.001);
    }

    #[test]
    fn consumption_guids_are_unique() {
        let resp = fixture_consumption();
        let rows = consumption_to_energy_rows(&resp, "mmnnoopp");
        let guids: HashSet<&str> =
            rows.iter().filter_map(|r| r.get("guid").and_then(Value::as_str)).collect();
        assert_eq!(guids.len(), rows.len(), "GUIDs not unique");
    }

    #[test]
    fn consumption_empty_items_returns_empty() {
        let resp = json!({ "items": [] });
        let rows = consumption_to_energy_rows(&resp, "loc");
        assert!(rows.is_empty());
    }

    #[test]
    fn consumption_missing_items_returns_empty() {
        let rows = consumption_to_energy_rows(&json!({}), "loc");
        assert!(rows.is_empty());
    }

    // ---- round-trip: HomeReading serialises per contract --------------------

    #[test]
    fn home_reading_round_trips() {
        let dev = fixture_device();
        let rows = telemetry_to_readings(&dev);
        let flow = rows.iter().find(|r| r.metric == "flow_rate").unwrap();
        let json = serde_json::to_value(flow).unwrap();
        // Required fields present.
        assert!(json.get("ts").is_some());
        assert_eq!(json["source"].as_str().unwrap(), "moen-flo");
        assert_eq!(json["metric"].as_str().unwrap(), "flow_rate");
        assert!(json.get("value").is_some());
        // Optional unit.
        assert_eq!(json["unit"].as_str().unwrap(), "gal_min");
    }

    // ---- persist: append_readings de-duplication ----------------------------

    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-moen-flo-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn append_readings_deduplicates() {
        let vault = temp_vault("readings-dedup");
        let dev = fixture_device();
        let rows = telemetry_to_readings(&dev);
        // First append: all rows are new.
        let n1 = append_readings(&vault, &rows).unwrap();
        assert_eq!(n1, rows.len() as u64);
        // Second append: all rows are duplicates.
        let n2 = append_readings(&vault, &rows).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn append_energy_deduplicates() {
        let vault = temp_vault("energy-dedup");
        let resp = fixture_consumption();
        let rows = consumption_to_energy_rows(&resp, "loc1");
        let n1 = append_energy(&vault, &rows).unwrap();
        assert_eq!(n1, rows.len() as u64);
        let n2 = append_energy(&vault, &rows).unwrap();
        assert_eq!(n2, 0, "second append should produce 0 new rows");
    }

    // ---- sync state round-trips ---------------------------------------------

    #[test]
    fn sync_state_round_trips() {
        let mut s = SyncState::default();
        s.consumption_watermarks
            .insert("loc1".into(), "2026-06-01T00:00:00+00:00".into());
        s.updated = Some("2026-06-17T10:00:00-07:00".into());
        let json = serde_json::to_string(&s).unwrap();
        let s2: SyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(s2.consumption_watermarks["loc1"], "2026-06-01T00:00:00+00:00");
    }

    // ---- sync state: alert state serialisation -------------------------------

    #[test]
    fn sync_state_alert_states_round_trips() {
        let mut s = SyncState::default();
        s.alert_states.insert("dev-abc".into(), AlertState { critical: 1, warning: 2 });
        let json = serde_json::to_string(&s).unwrap();
        let s2: SyncState = serde_json::from_str(&json).unwrap();
        let a = &s2.alert_states["dev-abc"];
        assert_eq!(a.critical, 1);
        assert_eq!(a.warning, 2);
    }

    #[test]
    fn alert_state_default_is_zero() {
        let s = AlertState::default();
        assert_eq!(s.critical, 0);
        assert_eq!(s.warning, 0);
    }

    #[test]
    fn alert_state_eq_and_ne() {
        let a = AlertState { critical: 0, warning: 2 };
        let b = AlertState { critical: 0, warning: 2 };
        let c = AlertState { critical: 1, warning: 2 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// An absent (first-run) alert state equals the default {0,0}.
    /// New counts of {0,2} should NOT equal default, so an event is emitted.
    #[test]
    fn alert_state_absent_means_zero_baseline() {
        let mut state = SyncState::default();
        // Simulate first poll: device not yet in alert_states.
        let prev = state.alert_states.get("dev-x").cloned().unwrap_or_default();
        assert_eq!(prev, AlertState::default(), "absent device should default to zero baseline");

        // New state has warnings.
        let new_state = AlertState { critical: 0, warning: 2 };
        assert_ne!(new_state, prev, "non-zero should differ from baseline → triggers event on first change");

        // After update, next poll with same counts should match.
        state.alert_states.insert("dev-x".into(), new_state.clone());
        let prev2 = state.alert_states.get("dev-x").cloned().unwrap_or_default();
        assert_eq!(prev2, new_state, "stored state should match on next poll → no event");
    }

    /// Standing alert at first run: baseline {0,0} → new state {0,2} → event emitted.
    /// Second poll with same counts {0,2}: no new event.
    #[test]
    fn alert_state_standing_alert_emits_only_on_first_change() {
        let mut state = SyncState::default();

        // Poll 1: first ever observation of a device with warnings.
        let new_state_1 = AlertState { critical: 0, warning: 2 };
        let prev_1 = state.alert_states.get("dev-y").cloned().unwrap_or_default();
        let should_emit_1 = new_state_1 != prev_1;
        state.alert_states.insert("dev-y".into(), new_state_1.clone());
        assert!(should_emit_1, "first poll with standing alert should emit (baseline diff)");

        // Poll 2: same counts — no new event.
        let new_state_2 = AlertState { critical: 0, warning: 2 };
        let prev_2 = state.alert_states.get("dev-y").cloned().unwrap_or_default();
        let should_emit_2 = new_state_2 != prev_2;
        assert!(!should_emit_2, "second poll with unchanged counts must NOT emit");

        // Poll 3: alert cleared.
        let new_state_3 = AlertState { critical: 0, warning: 0 };
        let prev_3 = state.alert_states.get("dev-y").cloned().unwrap_or_default();
        let should_emit_3 = new_state_3 != prev_3;
        assert!(should_emit_3, "cleared alert should emit");
    }
}
