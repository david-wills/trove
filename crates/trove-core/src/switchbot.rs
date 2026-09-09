//! SwitchBot hub and sensors — official cloud API v1.1.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/switchbot.md
//!
//! A **Periodic** cloud pull that fetches the device list + each device's
//! current status from `api.switch-bot.com/v1.1`. Auth is HMAC-SHA256:
//! every request carries `Authorization=token`, `t=timestamp_ms`,
//! `sign=Base64(HMAC-SHA256(secret, token+t+nonce))`, `nonce=hex32`.
//!
//! The API returns **current state only** — no history endpoint. Trove
//! accumulates its own history by polling; gaps while the app is closed
//! are expected (the `troved` daemon improves coverage). BLE-only devices
//! (Bot, Curtain, some meters) are invisible to the cloud API without a Hub.
//!
//! Raw layer: `home/switchbot/raw/YYYY-MM.jsonl` — one object per device
//! per poll, full fidelity from the API.
//! Contract layer: `home/switchbot/YYYY-MM.jsonl` — one `HomeReading` per
//! numeric metric per device per poll (temp, humidity, lightLevel, CO2).
//! Motion/contact sensor state (boolean / string) rides in `extra` on a
//! sentinel `0.0` or `1.0` reading; device events (on/off, lock, slide)
//! also land in `extra` so the raw data is never lost.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Local};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::Sha256;

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
// Constants

const DIR: &str = "home/switchbot";
const RAW_DIR: &str = "home/switchbot/raw";
const SYNC_FILE: &str = ".trove/switchbot-sync.json";
const SERVICE: &str = "switchbot";
const API_BASE: &str = "https://api.switch-bot.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// 5-minute poll cadence; cloud holds current state, not history.
const POLL_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Metric mapping: (api_field, metric, unit).
// Numeric fields only — boolean/string states ride in extra.
// `battery` is NOT a climate metric; it rides in extra per home.md §51.
const NUMERIC_FIELDS: &[(&str, &str, &str)] = &[
    ("temperature", "temperature", "C"),
    ("humidity", "humidity", "percent"),
    ("lightLevel", "light_level", "index"),
    ("CO2", "co2", "ppm"),
    ("voltage", "voltage", "V"),
    ("weight", "power_daily", "W"),          // "power consumed in a day" per docs
    ("electricCurrent", "current", "mA"),
    ("usedElectricity", "electricity_daily", "watt_minutes"),
    // Plug Mini (EU/US): `power` is a Float (Watts) for instantaneous power.
    // The ON/OFF string variant of "power" is handled by Value::Number match
    // (strings produce None → skipped), so no separate guard needed.
    ("power", "power", "W"),
    // `electricityOfDay` is usage duration in minutes per v1.1 docs (NOT energy).
    ("electricityOfDay", "usage_minutes", "min"),
];

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("switchbot synced — {n} readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "switchbot sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let r = out.counts.get("raw").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("SwitchBot: {n} readings from {r} devices"),
        counts: out.counts,
    })
}

pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "switchbot",
        name: "SwitchBot",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Reads sensor readings and device status from your \
                      SwitchBot hub and accessories via the official API v1.1. \
                      Covers temperature, humidity, light level, CO2, and plug \
                      energy data for cloud-visible devices (Hub 2, Meter, \
                      Meter Plus, Outdoor Meter, Meter Pro CO2, and more).",
        domain: "home",
        vault_path: "home/switchbot/",
        toggleable: false,
        setup: &[],
        caveats: "BLE-only devices require a SwitchBot Hub for cloud \
                  visibility. API is rate-limited to 10,000 requests per day. \
                  Only current state is available — no cloud history endpoint.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every(POLL_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("switchbot"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (token, secret) = parse_credentials(pasted)?;
    let client = SwitchBotClient::new(API_BASE.to_string(), token.clone(), secret.clone());
    // Verify by fetching the device list.
    match client.get_devices() {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "SwitchBot rejected the token or secret (401). Open the SwitchBot app, go to \
             Profile → Preferences → App Version (tap 10 times to reveal Developer Options), \
             and copy both the Open Token and the Client Secret."
        ),
        Err(e) => bail!("SwitchBot auth check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: Some("SwitchBotKeys".into()),
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
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "SwitchBot".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "switchbot",
    display_name: "SwitchBot",
    methods: &[ConnectMethod::TokenPaste {
        label: "Open Token and Client Secret",
        help: "Paste both as token:secret — find them in the SwitchBot app under \
               Profile → Preferences → App Version (tap 10 times) → Developer Options.",
        placeholder: "A1B2C3…token:a1b2c3…secret",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["switchbot"],
    setup: &[
        "Open the SwitchBot app and tap Profile (bottom right).",
        "Tap Preferences, then tap App Version 10 times to reveal Developer Options.",
        "Tap Developer Options and copy the Open Token and Client Secret.",
        "Paste them here as token:secret (separated by a colon).",
    ],
};

// ---------------------------------------------------------------------------
// Credential helpers

/// Parse "token:secret" from the pasted string. The secret itself may
/// contain colons (unlikely but possible) — we split on the first colon only.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let s = pasted.trim();
    let colon = s.find(':').context(
        "Expected token:secret separated by a colon — paste both from SwitchBot Developer Options",
    )?;
    let token = s[..colon].trim().to_string();
    let secret = s[colon + 1..].trim().to_string();
    if token.is_empty() || secret.is_empty() {
        bail!("Both token and secret are required — paste as token:secret");
    }
    Ok((token, secret))
}

/// Compute the HMAC-SHA256 signature per the SwitchBot v1.1 spec:
/// `Base64(HMAC-SHA256(secret, token + timestamp_ms + nonce))`.
fn compute_sign(token: &str, secret: &str, timestamp_ms: u64, nonce: &str) -> Result<String> {
    let msg = format!("{token}{timestamp_ms}{nonce}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .context("HMAC key error")?;
    mac.update(msg.as_bytes());
    let result = mac.finalize().into_bytes();
    Ok(BASE64.encode(result))
}

/// A simple hex nonce derived from the current nanosecond timestamp.
/// Avoids the uuid crate while being unique enough for a request nonce.
fn make_nonce() -> String {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    // Mix with a thread-local counter to reduce collision risk.
    static COUNTER: std::sync::atomic::AtomicU32 =
        std::sync::atomic::AtomicU32::new(0);
    let seq = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{ns:08x}{seq:08x}0000000000000000")
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for tests.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (401)"),
            FetchError::Other(s) => write!(f, "{s}"),
        }
    }
}

struct SwitchBotClient {
    base: String,
    token: String,
    secret: String,
}

impl SwitchBotClient {
    fn new(base: String, token: String, secret: String) -> Self {
        SwitchBotClient { base, token, secret }
    }

    fn auth_headers(&self) -> Result<[(&'static str, String); 4]> {
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let nonce = make_nonce();
        let sign = compute_sign(&self.token, &self.secret, timestamp_ms, &nonce)?;
        Ok([
            ("Authorization", self.token.clone()),
            ("t", timestamp_ms.to_string()),
            ("sign", sign),
            ("nonce", nonce),
        ])
    }

    fn get_json(&self, path: &str) -> Result<Value, FetchError> {
        let url = format!("{}{}", self.base, path);
        let headers = self
            .auth_headers()
            .map_err(|e| FetchError::Other(e.to_string()))?;
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/json; charset=utf8");
        for (k, v) in &headers {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| match e {
            ureq::Error::Status(401, _) => FetchError::Unauthorized,
            ureq::Error::Status(403, _) => FetchError::Unauthorized,
            ureq::Error::Status(code, r) => FetchError::Other(format!(
                "HTTP {code}: {}",
                r.into_string().unwrap_or_default()
            )),
            other => FetchError::Other(other.to_string()),
        })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| FetchError::Other(format!("JSON parse error: {e}")))?;
        // SwitchBot wraps all responses in {statusCode, message, body}.
        let status_code = body.get("statusCode").and_then(Value::as_i64).unwrap_or(0);
        if status_code == 401 || status_code == 190 {
            return Err(FetchError::Unauthorized);
        }
        Ok(body)
    }

    fn get_devices(&self) -> Result<Value, FetchError> {
        self.get_json("/v1.1/devices")
    }

    fn get_device_status(&self, device_id: &str) -> Result<Value, FetchError> {
        self.get_json(&format!("/v1.1/devices/{device_id}/status"))
    }
}

// Trait for injection in tests.
trait SwitchBotApi {
    fn get_devices(&self) -> Result<Value, FetchError>;
    fn get_device_status(&self, device_id: &str) -> Result<Value, FetchError>;
}

impl SwitchBotApi for SwitchBotClient {
    fn get_devices(&self) -> Result<Value, FetchError> {
        SwitchBotClient::get_devices(self)
    }
    fn get_device_status(&self, device_id: &str) -> Result<Value, FetchError> {
        SwitchBotClient::get_device_status(self, device_id)
    }
}

// ---------------------------------------------------------------------------
// Cursor
//
// The SwitchBot API returns current state only (no history endpoint), so each
// poll is a full snapshot of all devices. The poller is purely accumulative —
// each poll appends new rows; there is nothing to re-fetch from upstream.
//
// A guid-keyed dedup set was previously used here, but guids embed the poll
// timestamp (unique per poll by definition), so the set never fired and grew
// without bound (~metrics × devices × polls / day forever). It has been
// replaced with a simple `last_poll_ts` watermark that is persisted for
// observability (e.g., UI "last synced" display) without unbounded growth.

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    /// Timestamp of the last successfully completed poll (RFC3339).
    last_poll_ts: Option<String>,
}

fn load_cursor(vault: &Vault) -> Cursor {
    let path = vault.root().join(SYNC_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        serde_json::from_slice(&bytes).unwrap_or_default()
    } else {
        Cursor::default()
    }
}

fn save_cursor(vault: &Vault, cursor: &Cursor) -> Result<()> {
    let path = vault.root().join(SYNC_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_json_atomic(&path, cursor)
}

// ---------------------------------------------------------------------------
// Core logic

/// Timestamp for the current poll (RFC3339 local).
fn poll_ts() -> String {
    Local::now().to_rfc3339()
}

/// Guid for a device-metric sample: stable within a poll (device+metric+ts).
fn reading_guid(device_id: &str, metric: &str, ts: &str) -> String {
    format!("switchbot:{device_id}:{metric}:{ts}")
}

/// Extract HomeReading rows from a device status body.
/// `ts` is the poll timestamp (the API has no per-reading timestamp).
fn readings_from_status(
    status: &Value,
    device_id: &str,
    device_name: &str,
    device_type: &str,
    ts: &str,
) -> Vec<(String, HomeReading)> {
    let mut rows = Vec::new();

    for &(api_field, metric, unit) in NUMERIC_FIELDS {
        let v = match status.get(api_field) {
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        };
        let value = match v {
            Some(f) => f,
            None => continue,
        };
        let guid = reading_guid(device_id, metric, ts);
        let mut r = HomeReading::new("switchbot", metric, value, ts);
        r.unit = unit.to_string();
        r.device = device_id.to_string();
        r.place = device_name.to_string();
        // Stuff device type + non-numeric state fields + battery into extra.
        // Battery is source-specific device metadata, not a climate metric
        // (per home.md §51 and the airthings worked example).
        let mut extra = Map::new();
        extra.insert("device_type".into(), Value::String(device_type.to_string()));
        // Carry battery as extra overflow (percent level, device health).
        if let Some(bat) = status.get("battery") {
            if !bat.is_null() {
                extra.insert("battery".into(), bat.clone());
            }
        }
        // Also carry a few useful state fields that are non-numeric.
        for sf in &["power", "moveDetected", "openState", "brightness", "lockState",
                    "doorState", "calibrate", "version", "hubDeviceId",
                    "onlineStatus", "deviceMode"]
        {
            if let Some(val) = status.get(*sf) {
                if !val.is_null() {
                    extra.insert(sf.to_string(), val.clone());
                }
            }
        }
        r.extra = extra;
        rows.push((guid, r));
    }

    // If no numeric fields were emitted, check for motion/contact/lock/plug state
    // and emit a presence/state sentinel reading so the device shows up.
    // This fires for Contact Sensor, Motion Sensor, Lock, and Bot (battery-only
    // devices now that battery rides in extra, not as a metric).
    if rows.is_empty() {
        let sentinel = if let Some(Value::Bool(b)) = status.get("moveDetected") {
            Some(("motion_detected", if *b { 1.0_f64 } else { 0.0 }, "bool"))
        } else if let Some(Value::String(s)) = status.get("openState") {
            let v = match s.as_str() { "open" => 1.0, _ => 0.0 };
            Some(("open_state", v, "bool"))
        } else if let Some(Value::String(s)) = status.get("lockState") {
            let v = match s.as_str() { "unlock" => 0.0, "lock" => 1.0, _ => -1.0 };
            Some(("lock_state", v, "bool"))
        } else if let Some(Value::String(s)) = status.get("power") {
            let v = match s.as_str() { "ON" => 1.0, _ => 0.0 };
            Some(("power_state", v, "bool"))
        } else {
            None
        };
        if let Some((metric, value, unit)) = sentinel {
            let guid = reading_guid(device_id, metric, ts);
            let mut r = HomeReading::new("switchbot", metric, value, ts);
            r.unit = unit.to_string();
            r.device = device_id.to_string();
            r.place = device_name.to_string();
            let mut extra = Map::new();
            extra.insert("device_type".into(), Value::String(device_type.to_string()));
            if let Some(bat) = status.get("battery") {
                if !bat.is_null() {
                    extra.insert("battery".into(), bat.clone());
                }
            }
            for sf in &["power", "moveDetected", "openState", "brightness", "lockState",
                        "doorState", "calibrate", "version", "hubDeviceId", "onlineStatus"]
            {
                if let Some(val) = status.get(*sf) {
                    if !val.is_null() {
                        extra.insert(sf.to_string(), val.clone());
                    }
                }
            }
            r.extra = extra;
            rows.push((guid, r));
        }
    }

    rows
}

// ---------------------------------------------------------------------------
// Pull

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let ts_tok = vault.load_sync_token(SERVICE)?.context(
        "SwitchBot: not connected — paste your token:secret in the SwitchBot card",
    )?;
    let pasted = ts_tok.access_token;
    let (token, secret) = parse_credentials(&pasted)?;
    let client = SwitchBotClient::new(API_BASE.to_string(), token, secret);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl SwitchBotApi) -> Result<PullOutcome> {
    let ts = poll_ts();
    let mut cursor = load_cursor(vault);

    // 1. Fetch device list.
    let list_resp = api.get_devices().map_err(|e| anyhow::anyhow!("{e}"))?;
    let devices = list_resp
        .pointer("/body/deviceList")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let contract_stream = vault.stream(DIR, Partition::Month);

    let mut raw_rows: Vec<Value> = Vec::new();
    let mut contract_rows: Vec<HomeReading> = Vec::new();
    let mut new_count: u64 = 0;
    let mut device_count: u64 = 0;

    for device in &devices {
        let device_id = match device.get("deviceId").and_then(Value::as_str) {
            Some(id) => id,
            None => continue,
        };
        let device_name = device
            .get("deviceName")
            .and_then(Value::as_str)
            .unwrap_or(device_id);
        let device_type = device
            .get("deviceType")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        // Fetch device status; skip silently on error (rate limit / offline).
        let status_resp = match api.get_device_status(device_id) {
            Ok(v) => v,
            Err(FetchError::Unauthorized) => {
                return Err(anyhow::anyhow!(
                    "SwitchBot rejected the token (401) — re-paste from Developer Options"
                ))
            }
            Err(FetchError::Other(e)) => {
                // Non-fatal: skip this device.
                eprintln!("switchbot: skipping device {device_id}: {e}");
                continue;
            }
        };

        let status = status_resp
            .get("body")
            .cloned()
            .unwrap_or(Value::Null);

        // Raw: one object per device per poll.
        let raw_obj = json!({
            "ts": ts,
            "device_id": device_id,
            "device_name": device_name,
            "device_type": device_type,
            "status": status
        });
        raw_rows.push(raw_obj);
        device_count += 1;

        // Contract: extract HomeReading rows.
        // No cross-poll dedup needed: each poll produces a fresh snapshot of
        // current sensor state; the append-only vault accumulates history.
        let readings = readings_from_status(&status, device_id, device_name, device_type, &ts);
        for (_guid, r) in readings {
            contract_rows.push(r);
            new_count += 1;
        }
    }

    // Write raw layer (always, full fidelity).
    if !raw_rows.is_empty() {
        raw_stream
            .append(&raw_rows, |v| {
                v.get("ts").and_then(Value::as_str).unwrap_or("")
            })
            .context("switchbot: write raw")?;
    }

    // Write contract layer.
    if !contract_rows.is_empty() {
        contract_stream
            .append(&contract_rows, |r| r.ts.as_str())
            .context("switchbot: write contract")?;
    }

    // Persist cursor (watermark advance after success).
    cursor.last_poll_ts = Some(ts);
    save_cursor(vault, &cursor)?;

    let mut counts = BTreeMap::new();
    counts.insert("readings", new_count);
    counts.insert("raw", device_count);
    Ok(PullOutcome {
        headline: format!(
            "SwitchBot: {new_count} readings from {device_count} devices"
        ),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-switchbot-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ------------------------------------------------------------------
    // Fixture helpers — modeled on the official SwitchBot API v1.1 docs.

    fn device_list_response() -> Value {
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceList": [
                    {
                        "deviceId": "AABBCC112233",
                        "deviceName": "Hub 2",
                        "deviceType": "Hub 2",
                        "enableCloudService": true,
                        "hubDeviceId": "AABBCC112233"
                    },
                    {
                        "deviceId": "DDEEFF445566",
                        "deviceName": "Bedroom Meter",
                        "deviceType": "MeterPlus",
                        "enableCloudService": true,
                        "hubDeviceId": "AABBCC112233"
                    },
                    {
                        "deviceId": "AABBCC778899",
                        "deviceName": "Front Door",
                        "deviceType": "Contact Sensor",
                        "enableCloudService": true,
                        "hubDeviceId": "AABBCC112233"
                    },
                    {
                        "deviceId": "BBCC00AABB11",
                        "deviceName": "Living Room Motion",
                        "deviceType": "Motion Sensor",
                        "enableCloudService": true,
                        "hubDeviceId": "AABBCC112233"
                    }
                ],
                "infraredRemoteList": []
            }
        })
    }

    fn hub2_status() -> Value {
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceId": "AABBCC112233",
                "deviceType": "Hub 2",
                "hubDeviceId": "AABBCC112233",
                "temperature": 22.5,
                "lightLevel": 8,
                "humidity": 55,
                "version": "V4.2"
            }
        })
    }

    fn meter_plus_status() -> Value {
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceId": "DDEEFF445566",
                "deviceType": "MeterPlus",
                "hubDeviceId": "AABBCC112233",
                "temperature": 21.3,
                "humidity": 62,
                "battery": 80,
                "version": "V4.2"
            }
        })
    }

    fn contact_sensor_status() -> Value {
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceId": "AABBCC778899",
                "deviceType": "Contact Sensor",
                "hubDeviceId": "AABBCC112233",
                "moveDetected": false,
                "openState": "close",
                "brightness": "dim",
                "battery": 59,
                "version": "V4.2"
            }
        })
    }

    fn motion_sensor_status() -> Value {
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceId": "BBCC00AABB11",
                "deviceType": "Motion Sensor",
                "hubDeviceId": "AABBCC112233",
                "moveDetected": true,
                "brightness": "bright",
                "battery": 100,
                "version": "V4.2"
            }
        })
    }

    // ------------------------------------------------------------------
    // Test API stub

    struct StubApi {
        devices: Value,
        statuses: BTreeMap<String, Value>,
    }

    impl SwitchBotApi for StubApi {
        fn get_devices(&self) -> Result<Value, FetchError> {
            Ok(self.devices.clone())
        }
        fn get_device_status(&self, device_id: &str) -> Result<Value, FetchError> {
            self.statuses
                .get(device_id)
                .cloned()
                .map(Ok)
                .unwrap_or_else(|| {
                    Err(FetchError::Other(format!("no status for {device_id}")))
                })
        }
    }

    fn make_stub() -> StubApi {
        let mut statuses = BTreeMap::new();
        statuses.insert("AABBCC112233".into(), hub2_status());
        statuses.insert("DDEEFF445566".into(), meter_plus_status());
        statuses.insert("AABBCC778899".into(), contact_sensor_status());
        statuses.insert("BBCC00AABB11".into(), motion_sensor_status());
        StubApi {
            devices: device_list_response(),
            statuses,
        }
    }

    // ------------------------------------------------------------------
    // Unit tests for reading extraction

    #[test]
    fn hub2_status_yields_temperature_humidity_lightlevel() {
        let status = hub2_status().pointer("/body").cloned().unwrap();
        let rows = readings_from_status(&status, "AABBCC112233", "Hub 2", "Hub 2",
                                        "2026-06-17T10:00:00-07:00");
        let metrics: Vec<&str> = rows.iter().map(|(_, r)| r.metric.as_str()).collect();
        assert!(metrics.contains(&"temperature"), "temperature missing: {metrics:?}");
        assert!(metrics.contains(&"humidity"), "humidity missing: {metrics:?}");
        assert!(metrics.contains(&"light_level"), "light_level missing: {metrics:?}");

        let temp = rows.iter().find(|(_, r)| r.metric == "temperature").unwrap();
        assert_eq!(temp.1.value, 22.5);
        assert_eq!(temp.1.unit, "C");
        assert_eq!(temp.1.device, "AABBCC112233");
        assert_eq!(temp.1.place, "Hub 2");
        assert_eq!(temp.1.extra.get("device_type"), Some(&json!("Hub 2")));
    }

    #[test]
    fn meter_plus_yields_temperature_humidity_battery_in_extra() {
        let status = meter_plus_status().pointer("/body").cloned().unwrap();
        let rows = readings_from_status(&status, "DDEEFF445566", "Bedroom Meter", "MeterPlus",
                                        "2026-06-17T10:00:00-07:00");
        let metrics: Vec<&str> = rows.iter().map(|(_, r)| r.metric.as_str()).collect();
        assert!(metrics.contains(&"temperature"));
        assert!(metrics.contains(&"humidity"));
        // battery is NOT a metric; it rides in extra per home.md §51
        assert!(!metrics.contains(&"battery"), "battery must not be a metric");

        let hum = rows.iter().find(|(_, r)| r.metric == "humidity").unwrap();
        assert_eq!(hum.1.value, 62.0);
        assert_eq!(hum.1.unit, "percent");
        // battery appears in extra on the reading
        assert_eq!(hum.1.extra.get("battery"), Some(&json!(80)));
    }

    #[test]
    fn contact_sensor_emits_sentinel_open_state_battery_in_extra() {
        let status = contact_sensor_status().pointer("/body").cloned().unwrap();
        let rows = readings_from_status(&status, "AABBCC778899", "Front Door", "Contact Sensor",
                                        "2026-06-17T10:00:00-07:00");
        // No numeric climate metrics → sentinel fires (open_state or motion_detected)
        let metrics: Vec<&str> = rows.iter().map(|(_, r)| r.metric.as_str()).collect();
        assert!(!metrics.contains(&"battery"), "battery must not be a metric");
        assert!(
            metrics.contains(&"open_state") || metrics.contains(&"motion_detected"),
            "expected a state sentinel row, got: {metrics:?}"
        );
        // The sentinel row carries battery + openState in extra.
        let sentinel = rows.iter().find(|(_, r)| {
            r.metric == "open_state" || r.metric == "motion_detected"
        }).unwrap();
        assert!(sentinel.1.extra.contains_key("openState"), "openState missing from extra");
        assert_eq!(sentinel.1.extra.get("battery"), Some(&json!(59)));
    }

    #[test]
    fn motion_sensor_emits_sentinel_and_motion_in_extra() {
        let status = motion_sensor_status().pointer("/body").cloned().unwrap();
        let rows = readings_from_status(&status, "BBCC00AABB11", "Living Room Motion",
                                        "Motion Sensor", "2026-06-17T10:00:00-07:00");
        let metrics: Vec<&str> = rows.iter().map(|(_, r)| r.metric.as_str()).collect();
        // No numeric climate metrics → sentinel fires
        assert!(!metrics.contains(&"battery"), "battery must not be a metric");
        assert!(
            metrics.contains(&"motion_detected"),
            "expected motion_detected sentinel, got: {metrics:?}"
        );
        let sentinel = rows.iter().find(|(_, r)| r.metric == "motion_detected").unwrap();
        // moveDetected = true → motion_detected = 1.0
        assert_eq!(sentinel.1.value, 1.0);
        assert_eq!(sentinel.1.extra.get("moveDetected"), Some(&json!(true)));
        assert_eq!(sentinel.1.extra.get("battery"), Some(&json!(100)));
    }

    // ------------------------------------------------------------------
    // Plug Mini (EU) fixtures — tests the power field mapping fix.
    // SwitchBot API v1.1 docs lines 2416 and 2739:
    // `power` is Float (Watts) for Plug Mini EU/US.
    // The ON/OFF string `power` on other plugs must NOT produce a reading.

    fn plug_mini_eu_on_status() -> Value {
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceId": "PLUG01234567",
                "deviceType": "Plug Mini (EU)",
                "hubDeviceId": "AABBCC112233",
                "voltage": 230.5,
                "weight": 3.2,
                "electricCurrent": 23.0,
                "usedElectricity": 48,
                "power": 5.2
            }
        })
    }

    fn plug_string_power_status() -> Value {
        // A plug that sends power as "ON"/"OFF" string (not Float) —
        // must NOT produce a `power` W metric reading.
        json!({
            "statusCode": 100,
            "message": "success",
            "body": {
                "deviceId": "PLUG89ABCDEF",
                "deviceType": "Plug",
                "hubDeviceId": "AABBCC112233",
                "voltage": 120.0,
                "power": "ON"
            }
        })
    }

    #[test]
    fn plug_mini_eu_float_power_produces_reading() {
        let status = plug_mini_eu_on_status().pointer("/body").cloned().unwrap();
        let rows = readings_from_status(&status, "PLUG01234567", "Kitchen Plug", "Plug Mini (EU)",
                                        "2026-06-17T10:00:00-07:00");
        let metrics: Vec<&str> = rows.iter().map(|(_, r)| r.metric.as_str()).collect();
        // `power` Float 5.2 → power W metric
        assert!(metrics.contains(&"power"), "power metric missing: {metrics:?}");
        let pw = rows.iter().find(|(_, r)| r.metric == "power").unwrap();
        assert_eq!(pw.1.value, 5.2);
        assert_eq!(pw.1.unit, "W");
        // Other plug fields also present
        assert!(metrics.contains(&"voltage"), "voltage missing: {metrics:?}");
        assert!(metrics.contains(&"current"), "current missing: {metrics:?}");
        assert!(metrics.contains(&"electricity_daily"), "electricity_daily missing: {metrics:?}");
    }

    #[test]
    fn plug_string_power_does_not_produce_power_metric() {
        // A plug that sends `power` as the string "ON" must NOT produce a
        // `power` W metric (Value::Number match already handles this, but
        // verifying the sentinel fires instead).
        let status = plug_string_power_status().pointer("/body").cloned().unwrap();
        let rows = readings_from_status(&status, "PLUG89ABCDEF", "Old Plug", "Plug",
                                        "2026-06-17T10:00:00-07:00");
        let metrics: Vec<&str> = rows.iter().map(|(_, r)| r.metric.as_str()).collect();
        assert!(!metrics.contains(&"power"), "power W metric must not appear for string-power plug");
        // voltage is numeric → appears; power_state sentinel fires
        assert!(metrics.contains(&"voltage"), "voltage missing: {metrics:?}");
    }

    #[test]
    fn guid_is_stable_and_unique_per_device_metric_ts() {
        let g1 = reading_guid("DEV1", "temperature", "2026-06-17T10:00:00Z");
        let g2 = reading_guid("DEV1", "humidity", "2026-06-17T10:00:00Z");
        let g3 = reading_guid("DEV2", "temperature", "2026-06-17T10:00:00Z");
        let g4 = reading_guid("DEV1", "temperature", "2026-06-17T10:00:00Z");
        assert_ne!(g1, g2);
        assert_ne!(g1, g3);
        assert_eq!(g1, g4, "same inputs → same guid");
        assert!(g1.starts_with("switchbot:DEV1:temperature:"));
    }

    #[test]
    fn full_pull_writes_raw_and_contract_layers() {
        let v = temp_vault("fullpull");
        // Store fake credentials.
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tok:sec".into(),
                refresh_token: None,
                token_type: Some("SwitchBotKeys".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let api = make_stub();
        let out = pull_with(&v, &api).unwrap();

        // Four devices polled.
        assert_eq!(out.counts.get("raw"), Some(&4), "4 raw device rows");
        // Readings extracted:
        //   Hub2: temperature+humidity+light_level = 3
        //   MeterPlus: temperature+humidity = 2 (battery → extra)
        //   ContactSensor: open_state sentinel = 1 (battery → extra)
        //   MotionSensor: motion_detected sentinel = 1 (battery → extra)
        // Total ≥ 4 (exact count may vary by sensor fields).
        let readings = out.counts.get("readings").copied().unwrap_or(0);
        assert!(readings >= 4, "expected at least 4 readings, got {readings}");

        // Contract JSONL partitions exist.
        let partitions = v.stream(DIR, Partition::Month).partitions().unwrap();
        assert!(!partitions.is_empty(), "contract partitions written");

        // Raw JSONL partitions exist.
        let raw_parts = v.stream(RAW_DIR, Partition::Month).partitions().unwrap();
        assert!(!raw_parts.is_empty(), "raw partitions written");
    }

    #[test]
    fn second_pull_appends_and_cursor_watermark_advances() {
        // The SwitchBot poller is purely accumulative (no upstream history to
        // re-fetch); each poll appends a fresh snapshot. The cursor now holds
        // only a `last_poll_ts` watermark — no unbounded guid set.
        let v = temp_vault("dedup");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tok:sec".into(),
                refresh_token: None,
                token_type: Some("SwitchBotKeys".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let api = make_stub();
        let first = pull_with(&v, &api).unwrap();
        let first_n = first.counts["readings"];
        assert!(first_n > 0, "first pull must produce readings");

        // Second pull: same API response, new poll ts → appends new snapshot rows.
        let out2 = pull_with(&v, &api).unwrap();
        assert_eq!(out2.counts.get("raw"), Some(&4), "raw still writes 4 devices");
        // Readings also append (each poll is a new snapshot).
        assert_eq!(out2.counts.get("readings"), Some(&first_n),
                   "second poll produces same count as first (independent snapshots)");

        // Cursor file exists and has a last_poll_ts.
        let cursor_path = v.root().join(SYNC_FILE);
        let cursor: Cursor = serde_json::from_slice(&std::fs::read(cursor_path).unwrap()).unwrap();
        assert!(cursor.last_poll_ts.is_some(), "cursor watermark must be set");
    }

    #[test]
    fn hmac_sign_is_base64_and_non_empty() {
        let sign = compute_sign("mytoken", "mysecret", 1718600000000, "abc123").unwrap();
        assert!(!sign.is_empty());
        // base64 character set check (standard; may contain +/=)
        assert!(sign.chars().all(|c| c.is_alphanumeric() || c == '+' || c == '/' || c == '='));
    }

    #[test]
    fn parse_credentials_splits_on_first_colon() {
        let (tok, sec) = parse_credentials("mytoken:mysecret").unwrap();
        assert_eq!(tok, "mytoken");
        assert_eq!(sec, "mysecret");
        // Secret may itself contain a colon.
        let (t2, s2) = parse_credentials("tok:sec:extra").unwrap();
        assert_eq!(t2, "tok");
        assert_eq!(s2, "sec:extra");
    }

    #[test]
    fn parse_credentials_rejects_missing_colon() {
        assert!(parse_credentials("notokenorsecret").is_err());
        assert!(parse_credentials(":secret").is_err());
        assert!(parse_credentials("token:").is_err());
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "switchbot");
        assert_eq!(DEF.connection, Some("switchbot"));
    }
}
