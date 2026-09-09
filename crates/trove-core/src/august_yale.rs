//! August / Yale Smart Lock — entry log via unofficial cloud API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/august-yale.md.
//!
//! A **Periodic** cloud pull against the August production API:
//!   `https://api-production.august.com`
//!
//! Auth is a proprietary session: POST `/session` with email + `installId`
//! (a persistent UUID Trove generates per account) + password → response header
//! `x-august-access-token` carries the bearer token. Phone-based 2FA may be
//! required: a 6-digit SMS code is appended to the paste as a third segment.
//!
//! Connect flow:
//!   1. Paste `email|password` (no 2FA) **or** `email|password|code` (2FA).
//!   2. The connect fn POSTs `/session`, extracts the token from the
//!      `x-august-access-token` response header. If 2FA is pending and no code
//!      was supplied, it POSTs `/validation/phone` to trigger the SMS and tells
//!      the user to reconnect as `email|password|code`. With a code it POSTs
//!      `/validate/phone` to complete authentication.
//!
//! ## Vault layout
//!
//! - **Raw:** `home/august-yale/raw/YYYY-MM.jsonl` — verbatim activity API
//!   objects (full fidelity, unconditional).
//! - **Events:** `home/august-yale/events/YYYY-MM.jsonl` — one row per
//!   event in the `home.event` draft shape (`ts`, `source`, `device`,
//!   `event`, `guid`, optional `user`/`method`, `extra`). Written as
//!   [`serde_json::Value`] because the `home.event` Rust type is a Phase-3
//!   draft (no struct bound yet). A future follower maps the stored rows into
//!   the struct when the contract is ratified.
//!
//! ## Watermark
//!
//! Per-house (`houseId` → max `dateTime` ms) in `.trove/august-yale-sync.json`
//! (non-secret cursor, rebuildable). Advances only after the full drain.
//! The activities endpoint is paginated with `?limit=N&offset=N` (offset-based,
//! newest first). We page until a whole page is at/below the watermark.
//!
//! **Evidence:** yalexs Python library (https://github.com/Yale-Libs/yalexs),
//! the canonical reference used by Home Assistant. Endpoint paths, field names,
//! header names, and 2FA flow confirmed against `api_common.py`, `const.py`,
//! `authenticator_common.py`. Activity fixture shapes confirmed against
//! `pin_unlock_activity.json`, `auto_lock_activity.json`,
//! `remote_lock_activity.json`, `manual_lock_activity.json`,
//! `homekey_unlock_activity_v4.json`, and `get_house_activities.json`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

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

/// Vault dir for the home.event draft stream.
const EVENTS_DIR: &str = "home/august-yale/events";
/// Vault dir for the full-fidelity raw activity stream.
const RAW_DIR: &str = "home/august-yale/raw";

/// Non-secret rebuildable cursor (per-house watermarks + install_id).
const SYNC_FILE: &str = ".trove/august-yale-sync.json";

/// Service key under `.trove/sync/` for the stored access token (0600).
const SERVICE: &str = "august-yale";

/// August production API base URL (also Yale Access and Yale August — same backend).
const API_BASE: &str = "https://api-production.august.com";

/// API key for the August app (from yalexs `const.py`).
const API_KEY: &str = "d9984f29-07a6-816e-e1c9-44ec9d1be431";

/// Accept-Version header value (from yalexs api_common.py).
const ACCEPT_VERSION: &str = "0.0.1";

/// User-Agent header (from yalexs lib).
const USER_AGENT: &str = "August/Luna-22.17.0 (Android; SDK 31; gphone64_arm64)";

/// Per-page limit for the activities endpoint.
const PAGE_LIMIT: u32 = 100;

/// Seconds between periodic syncs. August retains ~90 days; 30-min polling
/// keeps events fresh without hammering the unofficial API.
pub const AY_SYNC_SECS: u64 = 1800;

/// Hard cap on pages fetched per house per sync (safety bound).
const MAX_PAGES: u32 = 200;

/// HTTP timeout for every request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Cursor — non-secret, rebuildable.

/// Persisted non-secret cursor. Keyed by house_id → max dateTime (epoch ms).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// `house_id → max dateTime (epoch ms) ever written`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    watermarks: BTreeMap<String, i64>,
    /// A stable random install_id generated once per account at connect time.
    /// Stored here (non-secret) because it is not sensitive.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    install_id: String,
    /// RFC3339 local time of the last successful sync (display only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ay_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ay_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Install-id generator (random UUIDv4 without the `uuid` crate).

/// Generate a random UUIDv4 string using `getrandom`.
fn new_install_id() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).unwrap_or_default();
    // Set version 4 bits (0100) at byte 6 upper nibble.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Set variant bits (10) at byte 8 upper two bits.
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],  bytes[1],  bytes[2],  bytes[3],
        bytes[4],  bytes[5],
        bytes[6],  bytes[7],
        bytes[8],  bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

// ---------------------------------------------------------------------------
// Credential parsing.

/// Parse the pasted credential string. Supported forms:
/// - `email|password`          — no 2FA (or accounts without 2FA)
/// - `email|password|code`     — 2FA verification code from SMS
///
/// Returns `(email, password, Option<code>)`.
fn parse_credentials(pasted: &str) -> Result<(String, String, Option<String>)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your August email and password as email|password");
    }
    let (email, rest) = pasted
        .split_once('|')
        .ok_or_else(|| anyhow::anyhow!("missing '|' — paste as email|password"))?;
    let email = email.trim().to_string();
    if email.is_empty() {
        bail!("missing email — paste as email|password");
    }
    // Check if there is a trailing |code. A 2FA code is 4–8 alphanumeric chars.
    if let Some((pw, code)) = rest.rsplit_once('|') {
        let code = code.trim();
        if code.len() >= 4 && code.len() <= 8 && code.chars().all(|c| c.is_alphanumeric()) {
            let pw = pw.trim().to_string();
            if pw.is_empty() {
                bail!("missing password — paste as email|password or email|password|code");
            }
            return Ok((email, pw, Some(code.to_string())));
        }
    }
    // No 2FA suffix — the whole rest is the password.
    let pw = rest.trim().to_string();
    if pw.is_empty() {
        bail!("missing password — paste as email|password");
    }
    Ok((email, pw, None))
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

/// The endpoints the pull needs. A trait so tests can drive the logic
/// with fixtures and never touch the network.
trait AugustApi {
    /// POST `/session` → `(access_token, needs_2fa)`.
    /// Returns the `x-august-access-token` header value and whether the
    /// account requires 2FA phone verification.
    fn create_session(
        &self,
        email: &str,
        password: &str,
        install_id: &str,
    ) -> Result<(String, bool)>;

    /// POST `/validation/phone` — sends the 2FA SMS code.
    fn send_verification_code(&self, access_token: &str, username: &str) -> Result<()>;

    /// POST `/validate/phone` — validates the SMS code.
    fn validate_code(&self, access_token: &str, username: &str, code: &str) -> Result<()>;

    /// GET `/users/houses/mine` → list of house objects.
    fn houses(&self, access_token: &str) -> Result<Vec<Value>>;

    /// GET `/houses/{id}/activities?limit=N&offset=N` — page of activities
    /// (newest first, offset-based pagination).
    fn activities(
        &self,
        access_token: &str,
        house_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Value>>;
}

/// Live HTTP client backed by `ureq`.
struct AugustClient;

impl AugustApi for AugustClient {
    fn create_session(
        &self,
        email: &str,
        password: &str,
        install_id: &str,
    ) -> Result<(String, bool)> {
        let body = serde_json::json!({
            "installId": install_id,
            "identifier": email,
            "password": password,
        });
        let url = format!("{API_BASE}/session");
        let resp = ureq::post(&url)
            .set("Content-Type", "application/json; charset=UTF-8")
            .set("x-august-api-key", API_KEY)
            .set("x-august-access-token", "")
            .set("Accept-Version", ACCEPT_VERSION)
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .send_json(&body);
        match resp {
            Ok(r) => {
                let access_token = r
                    .header("x-august-access-token")
                    .or_else(|| r.header("x-access-token"))
                    .unwrap_or("")
                    .to_string();
                let json_body: Value = r.into_json().context("parsing August session response")?;
                // `vPassword: true` in response body signals 2FA is required.
                let needs_2fa = json_body
                    .get("vPassword")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || access_token.is_empty();
                if access_token.is_empty() && !needs_2fa {
                    bail!("August session response had no access token — check credentials");
                }
                Ok((access_token, needs_2fa))
            }
            Err(ureq::Error::Status(400, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "August rejected credentials (400): {}",
                    body.chars().take(300).collect::<String>()
                )
            }
            Err(ureq::Error::Status(401, _)) => {
                bail!("August rejected credentials (401) — check your email and password")
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "August session failed ({code}): {}",
                    body.chars().take(300).collect::<String>()
                )
            }
            Err(e) => bail!("August session request failed: {e}"),
        }
    }

    fn send_verification_code(&self, access_token: &str, username: &str) -> Result<()> {
        let url = format!("{API_BASE}/validation/phone");
        let body = serde_json::json!({
            "value": username,
            "smsHashString": "anY0ZsRmXw+"
        });
        let resp = ureq::post(&url)
            .set("Content-Type", "application/json; charset=UTF-8")
            .set("x-august-api-key", API_KEY)
            .set("x-august-access-token", access_token)
            .set("Accept-Version", ACCEPT_VERSION)
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .send_json(&body);
        match resp {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "August send-code failed ({code}): {}",
                    body.chars().take(300).collect::<String>()
                )
            }
            Err(e) => bail!("August send-code request failed: {e}"),
        }
    }

    fn validate_code(&self, access_token: &str, username: &str, code: &str) -> Result<()> {
        let url = format!("{API_BASE}/validate/phone");
        let body = serde_json::json!({
            "phone": username,
            "code": code,
        });
        let resp = ureq::post(&url)
            .set("Content-Type", "application/json; charset=UTF-8")
            .set("x-august-api-key", API_KEY)
            .set("x-august-access-token", access_token)
            .set("Accept-Version", ACCEPT_VERSION)
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .send_json(&body);
        match resp {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "August validate-code failed ({code}): {}",
                    body.chars().take(300).collect::<String>()
                )
            }
            Err(e) => bail!("August validate-code request failed: {e}"),
        }
    }

    fn houses(&self, access_token: &str) -> Result<Vec<Value>> {
        let url = format!("{API_BASE}/users/houses/mine");
        let resp = ureq::get(&url)
            .set("x-august-api-key", API_KEY)
            .set("x-august-access-token", access_token)
            .set("Accept-Version", ACCEPT_VERSION)
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .call();
        match resp {
            Ok(r) => {
                let v: Value = r.into_json().context("parsing August houses response")?;
                match v {
                    Value::Array(arr) => Ok(arr),
                    obj @ Value::Object(_) => Ok(vec![obj]),
                    _ => Ok(vec![]),
                }
            }
            Err(ureq::Error::Status(401, _)) => {
                bail!("August auth expired (401) — reconnect from the Integrations tab")
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "August houses failed ({code}): {}",
                    body.chars().take(300).collect::<String>()
                )
            }
            Err(e) => bail!("August houses request failed: {e}"),
        }
    }

    fn activities(
        &self,
        access_token: &str,
        house_id: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Value>> {
        let url = format!("{API_BASE}/houses/{house_id}/activities?limit={limit}&offset={offset}");
        let resp = ureq::get(&url)
            .set("x-august-api-key", API_KEY)
            .set("x-august-access-token", access_token)
            .set("Accept-Version", ACCEPT_VERSION)
            .set("x-august-country", "US")
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .call();
        match resp {
            Ok(r) => {
                let v: Value = r.into_json().context("parsing August activities response")?;
                match v {
                    Value::Array(arr) => Ok(arr),
                    _ => Ok(vec![]),
                }
            }
            Err(ureq::Error::Status(401, _)) => {
                bail!("August auth expired (401) — reconnect from the Integrations tab")
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                bail!(
                    "August activities failed ({code}): {}",
                    body.chars().take(300).collect::<String>()
                )
            }
            Err(e) => bail!("August activities request failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Session / token storage.
//
// The bearer token is stored as `access_token` in the 0600-protected
// `.trove/sync/august-yale.json`. No email/password is stored.

fn store_session(vault: &Vault, access_token: &str) -> Result<()> {
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: access_token.to_string(),
            refresh_token: None,
            token_type: Some("AugustSession".into()),
            scope: None,
            expires_at: None,
        },
    )
}

fn load_access_token(vault: &Vault) -> Result<String> {
    let stored = vault
        .load_sync_token(SERVICE)?
        .context("August / Yale not connected — add credentials in the Integrations tab")?;
    Ok(stored.access_token)
}

// ---------------------------------------------------------------------------
// Event parsing helpers.

/// Extract `dateTime` or `timestamp` (epoch ms, i64) from an activity object.
fn activity_datetime(act: &Value) -> Option<i64> {
    act.get("dateTime")
        .or_else(|| act.get("timestamp"))
        .and_then(Value::as_i64)
}

/// Convert an epoch-ms dateTime to an RFC3339 local timestamp.
fn datetime_to_ts(epoch_ms: i64) -> Option<String> {
    use chrono::TimeZone;
    let secs = epoch_ms.checked_div(1000)?;
    let nanos = (epoch_ms.rem_euclid(1000) * 1_000_000) as u32;
    let utc = chrono::Utc.timestamp_opt(secs, nanos).single()?;
    Some(utc.with_timezone(&Local).to_rfc3339())
}

/// Extract the stable event id.
///
/// Priority (confirmed against yalexs `activity.py` and `get_house_activities.json`):
/// 1. `entities.activity` — the canonical id on the list endpoint (primary source)
/// 2. `eventID` — present in some single-activity fixtures
/// 3. `id` — present in some fixture families
/// 4. `activityID` — legacy fallback
///
/// If none of the above yields a non-empty string, the caller must synthesise
/// a guid from stable context fields so a parse-miss can never silently empty
/// the write set (see `activity_id_or_synth`).
fn activity_id(act: &Value) -> Option<String> {
    // entities.activity is the canonical id on the real list endpoint.
    if let Some(v) = act.get("entities").and_then(|e| e.get("activity")) {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        if !s.is_empty() && s != "null" {
            return Some(s);
        }
    }
    // Fallbacks for other fixture families (single-activity endpoints, legacy).
    for key in &["eventID", "id", "activityID"] {
        if let Some(v) = act.get(key) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => continue,
            };
            if !s.is_empty() && s != "null" {
                return Some(s);
            }
        }
    }
    None
}

/// Return the event id from the activity, or synthesise a deterministic guid
/// from `house_id + deviceID + dateTime + action` so that a missing id field
/// never silently drops the event.
fn activity_id_or_synth(act: &Value, datetime_ms: i64) -> String {
    if let Some(id) = activity_id(act) {
        return id;
    }
    // Synthesise from stable context — same inputs always produce the same guid.
    let house_id = act
        .get("entities").and_then(|e| e.get("house")).and_then(Value::as_str)
        .or_else(|| act.get("house").and_then(|h| h.get("houseID")).and_then(Value::as_str))
        .unwrap_or("unknown-house");
    let device_id = act.get("deviceID").and_then(Value::as_str).unwrap_or("unknown-device");
    let action = act.get("action").and_then(Value::as_str).unwrap_or("unknown");
    format!("synth:{house_id}:{device_id}:{datetime_ms}:{action}")
}

/// Map the `action` field to a home.event-vocabulary event string.
fn normalize_action(action: &str) -> &str {
    match action {
        "lock" | "auto_lock" | "manual_lock" | "remote_lock"
        | "pin_lock" | "finger_lock" | "bluetooth_lock" | "keypad_lock"
        | "linked_lock" | "homekey_lock" | "rf_lock"
        // yalexs ACTION_LOCK_ONETOUCH_LOCK and auto_relock.
        | "onetouchlock" | "auto_relock" => "lock",
        "unlock" | "auto_unlock" | "manual_unlock" | "remote_unlock"
        | "pin_unlock" | "finger_unlock" | "bluetooth_unlock" | "keypad_unlock"
        | "linked_unlock" | "homekey_unlock" | "rf_unlock" => "unlock",
        "dooropen" => "door_open",
        "doorclosed" => "door_closed",
        other => other,
    }
}

/// Determine the lock method from the action and `info` object.
fn lock_method(action: &str, info: &Value) -> Option<String> {
    // Prefer the granular action string prefix.
    if action.starts_with("auto_") || action == "auto_relock" {
        return Some("auto".to_string());
    }
    if action.starts_with("manual_") {
        return Some("manual".to_string());
    }
    if action.starts_with("pin_") || action.starts_with("keypad_") {
        return Some("keypad".to_string());
    }
    if action.starts_with("remote_") {
        return Some("remote".to_string());
    }
    if action.starts_with("finger_") {
        return Some("fingerprint".to_string());
    }
    if action.starts_with("bluetooth_") {
        return Some("bluetooth".to_string());
    }
    if action.starts_with("homekey_") {
        return Some("homekey".to_string());
    }
    if action.starts_with("rf_") {
        return Some("rf".to_string());
    }
    if action.starts_with("linked_") {
        return Some("linked".to_string());
    }
    // yalexs ACTION_LOCK_ONETOUCH_LOCK.
    if action == "onetouchlock" {
        return Some("onetouch".to_string());
    }
    // Fall back to info flags.
    if let Some(info_obj) = info.as_object() {
        if info_obj.get("remote").and_then(Value::as_bool).unwrap_or(false) {
            return Some("remote".to_string());
        }
        if info_obj.get("keypad").and_then(Value::as_bool).unwrap_or(false) {
            return Some("keypad".to_string());
        }
        if info_obj.get("manual").and_then(Value::as_bool).unwrap_or(false) {
            return Some("manual".to_string());
        }
        if info_obj.get("tag").and_then(Value::as_bool).unwrap_or(false) {
            return Some("rfid".to_string());
        }
    }
    None
}

/// Extract the calling user's display name from an activity object.
fn calling_user(act: &Value) -> Option<String> {
    let user = act.get("callingUser").or_else(|| act.get("user"))?;
    let first = user.get("FirstName").and_then(Value::as_str).unwrap_or("").trim();
    let last = user.get("LastName").and_then(Value::as_str).unwrap_or("").trim();
    let name = format!("{first} {last}").trim().to_string();
    if name.is_empty() {
        user.get("UserID").and_then(Value::as_str).map(str::to_string)
    } else {
        Some(name)
    }
}

/// Map one August activity object to the `home.event` draft shape.
///
/// Schema fields (`docs/vault-spec/schemas/home.event.schema.json`):
///   `ts`, `source`, `device`, `event`, `guid` (required)
///   `detail` — free-form method payload (e.g. "keypad", "remote")
///   `who`     — raw handle: UserID from callingUser or keypad code id
///   `who_name` — display name if the source gives one
///   `extra`   — everything source-specific
fn to_home_event(act: &Value, datetime_ms: i64) -> Option<Value> {
    let ts = datetime_to_ts(datetime_ms)?;
    if Partition::Month.key(&ts).is_none() {
        return None;
    }
    // Use synth fallback so a missing id never drops the event.
    let id = activity_id_or_synth(act, datetime_ms);
    let action = act.get("action").and_then(Value::as_str).unwrap_or("unknown");
    let event_type = normalize_action(action);
    let device_name = act
        .get("deviceName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            act.get("deviceID")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| "unknown".to_string())
        });

    let info = act.get("info").cloned().unwrap_or(Value::Null);
    let method = lock_method(action, &info);

    // `who_name` — display name of the calling user.
    let who_name = calling_user(act);
    // `who` — raw user handle (UserID). The schema specifies: "raw handle, never
    // resolved to a person". For keypad events the UserID identifies the code holder.
    let who = act
        .get("callingUser")
        .or_else(|| act.get("user"))
        .and_then(|u| u.get("UserID"))
        .and_then(Value::as_str)
        .map(str::to_string);

    // Build extra with full-fidelity source fields.
    let mut extra: Map<String, Value> = Map::new();
    extra.insert("august_action".into(), Value::String(action.to_string()));
    if let Some(device_id) = act.get("deviceID").and_then(Value::as_str) {
        extra.insert("device_id".into(), Value::String(device_id.to_string()));
    }
    if let Some(device_type) = act.get("deviceType").and_then(Value::as_str) {
        extra.insert("device_type".into(), Value::String(device_type.to_string()));
    }
    if let Value::Object(info_map) = &info {
        for (k, v) in info_map {
            extra.insert(format!("info_{k}"), v.clone());
        }
    }
    if let Some(house_id) = act
        .get("entities")
        .and_then(|e| e.get("house"))
        .and_then(Value::as_str)
        .or_else(|| {
            act.get("house")
                .and_then(|h| h.get("houseID"))
                .and_then(Value::as_str)
        })
    {
        extra.insert("house_id".into(), Value::String(house_id.to_string()));
    }

    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts));
    obj.insert("source".into(), Value::String("august-yale".into()));
    obj.insert("device".into(), Value::String(device_name));
    obj.insert("event".into(), Value::String(event_type.to_string()));
    obj.insert("guid".into(), Value::String(format!("august-yale:{id}")));
    // Schema: detail = the method ("keypad"), who_name = display name, who = raw handle.
    if let Some(m) = method {
        obj.insert("detail".into(), Value::String(m));
    }
    if let Some(wn) = who_name {
        obj.insert("who_name".into(), Value::String(wn));
    }
    if let Some(w) = who {
        obj.insert("who".into(), Value::String(w));
    }
    if !extra.is_empty() {
        obj.insert("extra".into(), Value::Object(extra));
    }
    Some(Value::Object(obj))
}

// ---------------------------------------------------------------------------
// Raw row wrapper (verbatim API objects, tagged with ts for partitioning).

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pull logic — injectable for offline tests.

/// Drain one house's activity feed into raw + events streams.
/// Returns `(event_count, raw_count, new_watermark)`.
fn drain_house(
    api: &impl AugustApi,
    access_token: &str,
    house_id: &str,
    prior_watermark: Option<i64>,
    vault: &Vault,
) -> Result<(usize, usize, Option<i64>)> {
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let evt_stream = vault.stream(EVENTS_DIR, Partition::Month);

    let mut offset: u32 = 0;
    let mut event_count = 0usize;
    let mut raw_count = 0usize;
    let mut new_watermark = prior_watermark;
    let mut pages = 0u32;

    loop {
        if pages >= MAX_PAGES {
            break;
        }
        pages += 1;

        let page = api.activities(access_token, house_id, PAGE_LIMIT, offset)?;
        if page.is_empty() {
            break;
        }

        let mut any_new = false;
        let mut raw_lines: Vec<RawLine> = Vec::new();
        let mut evt_lines: Vec<Value> = Vec::new();

        for act in &page {
            let Some(dt_ms) = activity_datetime(act) else {
                continue;
            };
            // If this activity is at/below the watermark, stop collecting from this page.
            if let Some(prior) = prior_watermark {
                if dt_ms <= prior {
                    // All remaining items are older — stop paging entirely.
                    // (API returns newest-first so once we hit the watermark we're done.)
                    break;
                }
            }
            any_new = true;
            // Track the maximum dateTime seen (should be the first item, but be safe).
            new_watermark = Some(new_watermark.map_or(dt_ms, |w: i64| w.max(dt_ms)));

            // Raw: verbatim activity object, tagged with ts for partitioning.
            let ts = datetime_to_ts(dt_ms)
                .unwrap_or_else(|| "1970-01-01T00:00:00+00:00".to_string());
            raw_lines.push(RawLine { ts, value: act.clone() });

            // Events: home.event draft shape.
            if let Some(ev) = to_home_event(act, dt_ms) {
                evt_lines.push(ev);
            }
        }

        if !raw_lines.is_empty() {
            raw_count += raw_lines.len();
            raw_stream.append(&raw_lines, |r| &r.ts)?;
        }
        if !evt_lines.is_empty() {
            event_count += evt_lines.len();
            evt_stream.append(&evt_lines, |v| {
                v.get("ts").and_then(Value::as_str).unwrap_or("")
            })?;
        }

        // If we got a short page (< limit) or nothing was new, we're caught up.
        if !any_new || page.len() < PAGE_LIMIT as usize {
            break;
        }

        offset += PAGE_LIMIT;
    }

    Ok((event_count, raw_count, new_watermark))
}

/// Core pull: drain all houses and persist updated watermarks.
fn pull_with(vault: &Vault, api: &impl AugustApi) -> Result<PullOutcome> {
    let access_token = load_access_token(vault)?;
    let mut state = vault.read_ay_sync();

    let houses = api.houses(&access_token)?;
    if houses.is_empty() {
        return Ok(PullOutcome {
            headline: "August / Yale: no houses found — check account".to_string(),
            counts: BTreeMap::<&'static str, u64>::new(),
        });
    }

    let mut total_events = 0usize;
    let mut total_raw = 0usize;

    for house in &houses {
        let house_id = house
            .get("HouseID")
            .or_else(|| house.get("houseID"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if house_id.is_empty() {
            continue;
        }
        let prior = state.watermarks.get(house_id).copied();
        let (ev, raw, new_wm) = drain_house(api, &access_token, house_id, prior, vault)?;
        total_events += ev;
        total_raw += raw;
        if let Some(w) = new_wm {
            state.watermarks.insert(house_id.to_string(), w);
        }
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ay_sync(&state)?;

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("events", total_events as u64);
    counts.insert("raw_activities", total_raw as u64);

    let headline = if total_events == 0 && total_raw == 0 {
        "August / Yale is up to date — no new lock events".to_string()
    } else {
        format!(
            "August / Yale synced — {total_events} lock events, {total_raw} raw activities"
        )
    };
    Ok(PullOutcome { headline, counts })
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(EVENTS_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(RAW_DIR)))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("events").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("august-yale synced — {n} lock events")
            }))
        }
        Err(e) => {
            // Only swallow transient network failures; surface auth/API errors.
            let is_transient = e.chain().any(|c| {
                let s = c.to_string();
                s.contains("connection refused")
                    || s.starts_with("unreachable:")
                    || s.contains("dns")
                    || s.contains("timed out")
            });
            if is_transient {
                Ok(crate::registry::CollectOutcome::note(format!(
                    "august-yale sync skipped (transient network): {e}"
                )))
            } else {
                Err(e)
            }
        }
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "august-yale",
        name: "August / Yale Smart Lock",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Collects your smart lock entry log — lock and unlock events with user and \
             method — from August or Yale connected locks via the cloud API.",
        domain: "home",
        vault_path: "home/august-yale/",
        toggleable: true,
        setup: &[
            "Paste your August or Yale account email and password as email|password.",
            "If your account requires 2FA, connect once — you will receive an SMS code — \
             then reconnect as email|password|code (e.g. you@example.com|mypass|123456).",
            "Entry logs are presence-rich data: this integration records when your lock \
             was used and by whom.",
        ],
        caveats:
            "Uses an unofficial API; behaviour may change without notice. Entry logs \
             record comings and goings at the front door — enable only for yourself \
             or with the knowledge of everyone in the household.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(AY_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("august-yale"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste: email|password or email|password|2fa_code).

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (email, password, code) = parse_credentials(pasted)?;

    // Reuse or generate a stable install_id (non-secret; stored in the cursor).
    let mut state = vault.read_ay_sync();
    if state.install_id.is_empty() {
        state.install_id = new_install_id();
    }
    let install_id = state.install_id.clone();

    let api = AugustClient;
    let (access_token, needs_2fa) = api.create_session(&email, &password, &install_id)?;

    if needs_2fa {
        // Save the cursor (install_id) first so reconnect finds the same id.
        vault.write_ay_sync(&state)?;
        if let Some(code) = code {
            // Code provided — validate it (requires a valid access_token first).
            if access_token.is_empty() {
                bail!(
                    "August requires 2FA but returned no session token — \
                     try without the code first"
                );
            }
            api.validate_code(&access_token, &email, &code)?;
        } else {
            // No code yet — trigger SMS and ask user to reconnect.
            if !access_token.is_empty() {
                // Best-effort: failure here is non-fatal (user can retry).
                let _ = api.send_verification_code(&access_token, &email);
            }
            bail!(
                "August requires 2FA — check your phone for an SMS code and reconnect as \
                 email|password|code (e.g. you@example.com|mypass|123456)"
            );
        }
    }

    // Persist the install_id in the (non-secret) cursor.
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ay_sync(&state)?;

    // Persist the bearer token in the 0600-protected sync token store.
    store_session(vault, &access_token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(_token) = vault.load_sync_token(SERVICE)? {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "August / Yale".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
/// The integrator must add `&crate::august_yale::CONNECTION,` to `CONNECTIONS`
/// in `integrations.rs`.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "august-yale",
    display_name: "August / Yale Smart Lock",
    methods: &[ConnectMethod::TokenPaste {
        label: "August / Yale email and password",
        help: "Paste as email|password (e.g. you@example.com|mypassword). \
               If your account requires 2FA, you will receive an SMS code — reconnect \
               as email|password|code (e.g. you@example.com|mypassword|123456). \
               Credentials are stored locally and sent only to api-production.august.com.",
        placeholder: "you@example.com|mypassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["august-yale"],
    setup: &[
        "Use your August or Yale account email and password.",
        "If 2FA is enabled: connect once, receive the SMS code, then reconnect \
         adding the code (email|password|code).",
    ],
};

// ---------------------------------------------------------------------------
// Public pull entry point.

/// Entry point for the periodic collect and manual "Sync now".
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    pull_with(vault, &AugustClient)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(test_name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-august-yale-{}-{test_name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---- Credential parsing ------------------------------------------------

    #[test]
    fn parse_credentials_email_password() {
        let (email, pw, code) = parse_credentials("user@example.com|secret").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(pw, "secret");
        assert!(code.is_none());
    }

    #[test]
    fn parse_credentials_with_2fa_code() {
        let (email, pw, code) =
            parse_credentials("user@example.com|secret|123456").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(pw, "secret");
        assert_eq!(code.as_deref(), Some("123456"));
    }

    #[test]
    fn parse_credentials_empty_fails() {
        assert!(parse_credentials("").is_err());
    }

    #[test]
    fn parse_credentials_no_pipe_fails() {
        assert!(parse_credentials("user@example.com").is_err());
    }

    #[test]
    fn parse_credentials_empty_email_fails() {
        assert!(parse_credentials("|password").is_err());
    }

    // ---- Timestamp helpers -------------------------------------------------

    #[test]
    fn datetime_to_ts_known_epoch() {
        // 1665378377000 ms = 2022-10-10T04:26:17Z (UTC); local timezone may shift the date.
        let ts = datetime_to_ts(1665378377000).unwrap();
        // The timestamp string must be partitionable (RFC3339 with month).
        assert!(Partition::Month.key(&ts).is_some(), "not partitionable: {ts}");
        // It must be in Oct 2022 in any timezone.
        assert!(ts.starts_with("2022-10"), "expected 2022-10, got: {ts}");
    }

    #[test]
    fn datetime_to_ts_produces_partitionable_string() {
        let ts = datetime_to_ts(1665378987000).unwrap();
        assert!(Partition::Month.key(&ts).is_some(), "ts={ts}");
    }

    // ---- Action normalization -----------------------------------------------

    #[test]
    fn normalize_action_lock_variants() {
        for action in &[
            "lock",
            "auto_lock",
            "manual_lock",
            "remote_lock",
            "pin_lock",
            "finger_lock",
            "keypad_lock",
            "linked_lock",
            "homekey_lock",
            "rf_lock",
            // yalexs ACTION_LOCK_ONETOUCH_LOCK and auto_relock.
            "onetouchlock",
            "auto_relock",
        ] {
            assert_eq!(normalize_action(action), "lock", "action={action}");
        }
    }

    #[test]
    fn normalize_action_unlock_variants() {
        for action in &[
            "unlock",
            "auto_unlock",
            "manual_unlock",
            "remote_unlock",
            "pin_unlock",
            "finger_unlock",
            "keypad_unlock",
            "linked_unlock",
            "homekey_unlock",
            "rf_unlock",
        ] {
            assert_eq!(normalize_action(action), "unlock", "action={action}");
        }
    }

    #[test]
    fn normalize_action_door_events() {
        assert_eq!(normalize_action("dooropen"), "door_open");
        assert_eq!(normalize_action("doorclosed"), "door_closed");
    }

    // ---- Lock method -------------------------------------------------------

    #[test]
    fn lock_method_from_action_prefix() {
        assert_eq!(lock_method("auto_lock", &Value::Null).as_deref(), Some("auto"));
        assert_eq!(lock_method("remote_unlock", &Value::Null).as_deref(), Some("remote"));
        assert_eq!(lock_method("pin_unlock", &Value::Null).as_deref(), Some("keypad"));
        assert_eq!(lock_method("finger_lock", &Value::Null).as_deref(), Some("fingerprint"));
        assert_eq!(lock_method("homekey_unlock", &Value::Null).as_deref(), Some("homekey"));
        assert_eq!(lock_method("rf_lock", &Value::Null).as_deref(), Some("rf"));
        assert_eq!(lock_method("linked_unlock", &Value::Null).as_deref(), Some("linked"));
        assert_eq!(lock_method("bluetooth_lock", &Value::Null).as_deref(), Some("bluetooth"));
    }

    #[test]
    fn lock_method_falls_back_to_info_remote() {
        let info = json!({"remote": true});
        assert_eq!(lock_method("lock", &info).as_deref(), Some("remote"));
    }

    #[test]
    fn lock_method_falls_back_to_info_keypad() {
        let info = json!({"keypad": true});
        assert_eq!(lock_method("lock", &info).as_deref(), Some("keypad"));
    }

    #[test]
    fn lock_method_onetouchlock_and_auto_relock() {
        assert_eq!(lock_method("onetouchlock", &Value::Null).as_deref(), Some("onetouch"));
        assert_eq!(lock_method("auto_relock", &Value::Null).as_deref(), Some("auto"));
    }

    // ---- to_home_event mapping ---------------------------------------------

    #[test]
    fn pin_unlock_maps_to_unlock_event() {
        // Fixture shape from yalexs tests/fixtures/pin_unlock_activity.json
        let act = json!({
            "id": "e05b1c22-71d1-44b8-8a4c-e729174a8972",
            "timestamp": 1665378377000i64,
            "action": "pin_unlock",
            "deviceID": "3ED27C5DB830439CAA4F381DC8F16444",
            "deviceName": "Front Door",
            "deviceType": "lock",
            "user": {
                "UserID": "xxxsasdd-230b-4c6a-8a5c-155043492f3b",
                "FirstName": "Sample",
                "LastName": "Person"
            }
        });
        let ev = to_home_event(&act, 1665378377000).unwrap();
        assert_eq!(ev["source"], "august-yale");
        assert_eq!(ev["event"], "unlock");
        assert_eq!(ev["device"], "Front Door");
        // Schema: `detail` carries the method, `who_name` the display name, `who` the raw handle.
        assert_eq!(ev["detail"], "keypad");
        assert_eq!(ev["who_name"], "Sample Person");
        assert_eq!(ev["who"], "xxxsasdd-230b-4c6a-8a5c-155043492f3b");
        assert!(ev["guid"].as_str().unwrap().starts_with("august-yale:"));
        assert!(Partition::Month.key(ev["ts"].as_str().unwrap()).is_some());
        // Schema field `method` must NOT appear (it is NOT in home.event schema).
        assert!(ev.get("method").is_none(), "`method` is not a schema field — use `detail`");
        assert!(ev.get("user").is_none(), "`user` is not a schema field — use `who_name`");
    }

    #[test]
    fn auto_lock_maps_to_lock_event_no_user() {
        // Fixture shape from yalexs tests/fixtures/auto_lock_activity.json
        let act = json!({
            "id": "c410074a-4cb9-4552-8a4e-68d3abb9117d",
            "timestamp": 1665378987000i64,
            "action": "auto_lock",
            "deviceID": "3ED27C5DB8304395AA4F381DC8F167BB",
            "deviceName": "Front Door",
            "deviceType": "lock"
        });
        let ev = to_home_event(&act, 1665378987000).unwrap();
        assert_eq!(ev["event"], "lock");
        assert_eq!(ev["detail"], "auto");
        // Auto-lock has no callingUser.
        assert!(ev.get("who_name").is_none());
        assert!(ev.get("who").is_none());
        assert!(ev.get("user").is_none(), "`user` is not a schema field");
    }

    #[test]
    fn remote_lock_with_info_flag_maps_correctly() {
        // Fixture shape from yalexs tests/fixtures/remote_lock_activity.json
        let act = json!({
            "eventID": "eventid",
            "dateTime": 1583535312002i64,
            "action": "lock",
            "deviceName": "lockname",
            "deviceID": "deviceid",
            "deviceType": "lock",
            "callingUser": {
                "UserID": "userid",
                "FirstName": "My",
                "LastName": "Name"
            },
            "info": { "remote": true }
        });
        let ev = to_home_event(&act, 1583535312002).unwrap();
        assert_eq!(ev["event"], "lock");
        // "lock" action (no prefix) → falls back to info.remote.
        assert_eq!(ev["detail"], "remote");
        assert_eq!(ev["who_name"], "My Name");
        assert_eq!(ev["who"], "userid");
        assert_eq!(ev["guid"], "august-yale:eventid");
        // info.remote should appear in extra.
        assert_eq!(ev["extra"]["info_remote"], true);
        assert!(ev.get("method").is_none(), "`method` is not a schema field");
        assert!(ev.get("user").is_none(), "`user` is not a schema field");
    }

    #[test]
    fn homekey_unlock_maps_correctly() {
        // Fixture from yalexs tests/fixtures/homekey_unlock_activity_v4.json
        let act = json!({
            "id": "b4e50f6c-730a-4ae1-991e-e5231d727c11",
            "timestamp": 1665388800273i64,
            "action": "homekey_unlock",
            "deviceID": "102A6A4F31584849971D11807272E7A8",
            "deviceName": "Gate",
            "deviceType": "lock",
            "user": {
                "UserID": "83f02a1d-c08a-4c3d-9c30-3b66e185c7bd",
                "FirstName": "89",
                "LastName": "House"
            }
        });
        let ev = to_home_event(&act, 1665388800273).unwrap();
        assert_eq!(ev["event"], "unlock");
        assert_eq!(ev["detail"], "homekey");
        assert_eq!(ev["device"], "Gate");
        assert_eq!(ev["who_name"], "89 House");
        assert_eq!(ev["who"], "83f02a1d-c08a-4c3d-9c30-3b66e185c7bd");
    }

    #[test]
    fn manual_lock_maps_correctly() {
        // Fixture from yalexs tests/fixtures/manual_lock_activity.json
        let act = json!({
            "id": "9fe32136-08b2-48b3-93cc-63a88c1c8236",
            "timestamp": 1665527802000i64,
            "action": "manual_lock",
            "deviceID": "3ED27C5DB8304395AA4F381DC8F167BB",
            "deviceName": "Front Door",
            "deviceType": "lock"
        });
        let ev = to_home_event(&act, 1665527802000).unwrap();
        assert_eq!(ev["event"], "lock");
        assert_eq!(ev["detail"], "manual");
    }

    // ---- Regression: real list-endpoint shape (entities.activity, no top-level id) --

    #[test]
    fn list_endpoint_shape_entities_activity_id_parsed() {
        // Real shape from yalexs get_house_activities.json / lock_activity.json:
        // NO top-level id/eventID; activity id lives at entities.activity.
        let act = json!({
            "action": "lock",
            "callingUser": {
                "FirstName": "MockHouse",
                "LastName": "House",
                "UserID": "mockUserId2"
            },
            "dateTime": 1582007218000i64,
            "deviceID": "ABC",
            "deviceName": "MockHouseTDoor",
            "deviceType": "lock",
            "entities": {
                "activity": "mockActivity2",
                "callingUser": "mockUserId2",
                "device": "ABC",
                "house": "123",
                "otherUser": "deleted"
            },
            "house": {
                "houseID": "123",
                "houseName": "MockHouse"
            },
            "info": {
                "DateLogActionID": "ABC+Time",
                "remote": true
            }
        });

        // activity_id() must extract from entities.activity, not miss.
        assert_eq!(
            activity_id(&act).as_deref(),
            Some("mockActivity2"),
            "must read entities.activity on the list-endpoint shape"
        );

        // to_home_event() must produce a populated event (not None).
        let ev = to_home_event(&act, 1582007218000)
            .expect("must parse list-endpoint shape to event");
        assert_eq!(ev["event"], "lock");
        assert_eq!(ev["source"], "august-yale");
        assert_eq!(ev["guid"], "august-yale:mockActivity2");
        // detail carries the method (falls back to info.remote = true → "remote").
        assert_eq!(ev["detail"], "remote");
        // who_name = display name, who = raw UserID.
        assert_eq!(ev["who_name"], "MockHouse House");
        assert_eq!(ev["who"], "mockUserId2");
        // house_id ends up in extra from entities.house.
        assert_eq!(ev["extra"]["house_id"], "123");
        assert!(Partition::Month.key(ev["ts"].as_str().unwrap()).is_some());
    }

    #[test]
    fn list_endpoint_shape_guid_is_stable() {
        // Same activity object parsed twice must produce the same guid.
        let act = json!({
            "action": "unlock",
            "dateTime": 45454i64,
            "deviceID": "mockDeviceId2",
            "deviceName": "MockHouseXDoor",
            "deviceType": "lock",
            "entities": {
                "activity": "ActivityId",
                "callingUser": "mockUserId2",
                "device": "mockDeviceId2",
                "house": "mock-house-id"
            },
            "house": {
                "houseID": "mock-house-id",
                "houseName": "MockHouse"
            }
        });
        let ev1 = to_home_event(&act, 45454).unwrap();
        let ev2 = to_home_event(&act, 45454).unwrap();
        assert_eq!(ev1["guid"], ev2["guid"], "guid must be stable across parses");
        assert_eq!(ev1["guid"], "august-yale:ActivityId");
    }

    #[test]
    fn synth_guid_when_no_id_field() {
        // Activity with absolutely no id anywhere — synth must fire and be stable.
        let act = json!({
            "action": "dooropen",
            "dateTime": 1665378377000i64,
            "deviceID": "DOORDEV",
            "deviceName": "Front Door",
            "deviceType": "door_sense",
            "house": { "houseID": "house-x" }
        });
        let id1 = activity_id_or_synth(&act, 1665378377000);
        let id2 = activity_id_or_synth(&act, 1665378377000);
        assert_eq!(id1, id2, "synth guid must be deterministic");
        assert!(id1.starts_with("synth:"), "must be a synth guid: {id1}");

        // The event must still be written (not None) even with no id.
        let ev = to_home_event(&act, 1665378377000)
            .expect("event must be produced even without an explicit id");
        assert_eq!(ev["event"], "door_open");
        assert!(ev["guid"].as_str().unwrap().starts_with("august-yale:synth:"));
    }

    #[test]
    fn full_pull_with_list_endpoint_shape_writes_events() {
        // Regression: drain_house must NOT silently zero the event layer when
        // activities carry the real list-endpoint shape (entities.activity, no top-level id).
        let vault = temp_vault("full_pull_with_list_endpoint_shape_writes_events");
        seed_token(&vault);

        let api = MockApi {
            houses: vec![json!({"HouseID": "mock-house-id", "HouseName": "MockHouse"})],
            activity_pages: vec![vec![
                // Real list-endpoint shape — only entities.activity has the id.
                json!({
                    "action": "lock",
                    "callingUser": {
                        "FirstName": "MockHouse",
                        "LastName": "House",
                        "UserID": "mockUserId2"
                    },
                    "dateTime": 1582007218000i64,
                    "deviceID": "ABC",
                    "deviceName": "MockHouseTDoor",
                    "deviceType": "lock",
                    "entities": {
                        "activity": "mockActivity2",
                        "callingUser": "mockUserId2",
                        "device": "ABC",
                        "house": "mock-house-id",
                        "otherUser": "deleted"
                    },
                    "house": { "houseID": "mock-house-id", "houseName": "MockHouse" },
                    "info": { "remote": true }
                }),
                json!({
                    "action": "unlock",
                    "callingUser": {
                        "FirstName": "MockHouse",
                        "LastName": "House",
                        "UserID": "mockUserId2"
                    },
                    "dateTime": 45454i64,
                    "deviceID": "mockDeviceId2",
                    "deviceName": "MockHouseXDoor",
                    "deviceType": "lock",
                    "entities": {
                        "activity": "ActivityId",
                        "callingUser": "mockUserId2",
                        "device": "mockDeviceId2",
                        "house": "mock-house-id"
                    },
                    "house": { "houseID": "mock-house-id", "houseName": "MockHouse" },
                    "info": { "remote": true }
                }),
            ]],
        };

        let out = pull_with(&vault, &api).unwrap();
        assert!(
            out.counts.get("events").copied().unwrap_or(0) > 0,
            "events layer must be non-zero on real list-endpoint shape (was silently zeroed before fix)"
        );
        assert!(
            out.counts.get("raw_activities").copied().unwrap_or(0) > 0,
            "raw layer must be non-zero"
        );
    }

    // ---- activity_id extraction -------------------------------------------

    #[test]
    fn activity_id_prefers_entities_activity() {
        // Real list-endpoint shape: entities.activity is the canonical id.
        let act = json!({"entities": {"activity": "ent-id"}, "eventID": "abc", "id": "xyz"});
        assert_eq!(activity_id(&act).as_deref(), Some("ent-id"),
            "entities.activity must be highest priority");
    }

    #[test]
    fn activity_id_falls_back_to_event_id() {
        let act = json!({"eventID": "abc", "id": "xyz"});
        assert_eq!(activity_id(&act).as_deref(), Some("abc"));
    }

    #[test]
    fn activity_id_falls_back_to_id() {
        let act = json!({"id": "xyz"});
        assert_eq!(activity_id(&act).as_deref(), Some("xyz"));
    }

    #[test]
    fn activity_id_handles_numeric_id() {
        let act = json!({"id": 12345i64});
        assert_eq!(activity_id(&act).as_deref(), Some("12345"));
    }

    #[test]
    fn activity_id_returns_none_when_no_id_present() {
        let act = json!({"action": "lock", "dateTime": 1234i64});
        assert!(activity_id(&act).is_none(), "must return None when no id field present");
    }

    // ---- install_id generation ---------------------------------------------

    #[test]
    fn new_install_id_is_uuid_shaped() {
        let id = new_install_id();
        // UUID v4 format: 8-4-4-4-12 hex chars separated by hyphens, 36 chars total.
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0].len(), 8);
        assert_eq!(parts[1].len(), 4);
        assert_eq!(parts[2].len(), 4);
        assert_eq!(parts[3].len(), 4);
        assert_eq!(parts[4].len(), 12);
        // Version 4: middle nibble of third group is '4'.
        assert_eq!(&parts[2][0..1], "4");
    }

    // ---- Drain + store (offline) ------------------------------------------

    struct MockApi {
        houses: Vec<Value>,
        /// Pages of activities, indexed by page number (offset / PAGE_LIMIT).
        activity_pages: Vec<Vec<Value>>,
    }

    impl AugustApi for MockApi {
        fn create_session(&self, _: &str, _: &str, _: &str) -> Result<(String, bool)> {
            Ok(("mock-token".to_string(), false))
        }
        fn send_verification_code(&self, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn validate_code(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
        fn houses(&self, _: &str) -> Result<Vec<Value>> {
            Ok(self.houses.clone())
        }
        fn activities(&self, _: &str, _: &str, _: u32, offset: u32) -> Result<Vec<Value>> {
            let idx = (offset / PAGE_LIMIT) as usize;
            Ok(self.activity_pages.get(idx).cloned().unwrap_or_default())
        }
    }

    fn fixture_activities() -> Vec<Value> {
        vec![
            // Newest first (API order).
            json!({
                "id": "event-1",
                "timestamp": 1665378987000i64,
                "action": "auto_lock",
                "deviceID": "LOCK001",
                "deviceName": "Front Door",
                "deviceType": "lock"
            }),
            json!({
                "id": "event-2",
                "timestamp": 1665378377000i64,
                "action": "pin_unlock",
                "deviceID": "LOCK001",
                "deviceName": "Front Door",
                "deviceType": "lock",
                "user": {
                    "UserID": "user-1",
                    "FirstName": "Sample",
                    "LastName": "Person"
                }
            }),
        ]
    }

    fn seed_token(vault: &Vault) {
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: "mock-access-token".to_string(),
                    refresh_token: None,
                    token_type: Some("AugustSession".into()),
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn full_pull_writes_both_layers() {
        let vault = temp_vault("full_pull_writes_both_layers");
        seed_token(&vault);

        let api = MockApi {
            houses: vec![json!({"HouseID": "house-1", "HouseName": "My Home"})],
            activity_pages: vec![fixture_activities()],
        };

        let out = pull_with(&vault, &api).unwrap();
        assert!(
            out.counts.get("events").copied().unwrap_or(0) > 0,
            "no events written"
        );
        assert!(
            out.counts.get("raw_activities").copied().unwrap_or(0) > 0,
            "no raw written"
        );

        // Watermark advanced to max dateTime.
        let state = vault.read_ay_sync();
        assert!(state.watermarks.contains_key("house-1"), "watermark not written");
        assert_eq!(
            state.watermarks["house-1"],
            1665378987000,
            "watermark should be max dateTime"
        );
    }

    #[test]
    fn incremental_pull_skips_seen_events() {
        let vault = temp_vault("incremental_pull_skips_seen_events");
        seed_token(&vault);

        // Seed the watermark to the max event dateTime so all fixture events are old.
        let mut state = SyncState::default();
        state.watermarks.insert("house-1".to_string(), 1665378987000);
        vault.write_ay_sync(&state).unwrap();

        let api = MockApi {
            houses: vec![json!({"HouseID": "house-1"})],
            activity_pages: vec![fixture_activities()],
        };

        let out = pull_with(&vault, &api).unwrap();
        assert_eq!(
            out.counts.get("events").copied().unwrap_or(0),
            0,
            "no new events expected above watermark"
        );
        assert_eq!(
            out.counts.get("raw_activities").copied().unwrap_or(0),
            0,
            "no new raw expected above watermark"
        );
    }

    #[test]
    fn pull_with_no_houses_returns_gracefully() {
        let vault = temp_vault("pull_with_no_houses_returns_gracefully");
        seed_token(&vault);

        let api = MockApi {
            houses: vec![],
            activity_pages: vec![],
        };

        let out = pull_with(&vault, &api).unwrap();
        assert!(out.headline.contains("no houses"));
    }

    #[test]
    fn cursor_advances_to_max_datetime() {
        let vault = temp_vault("cursor_advances_to_max_datetime");
        seed_token(&vault);

        let api = MockApi {
            houses: vec![json!({"HouseID": "house-abc"})],
            activity_pages: vec![vec![
                json!({
                    "id": "e2",
                    "timestamp": 2000000000000i64,
                    "action": "unlock",
                    "deviceID": "D1",
                    "deviceName": "Side Door",
                    "deviceType": "lock"
                }),
                json!({
                    "id": "e1",
                    "timestamp": 1000000000000i64,
                    "action": "lock",
                    "deviceID": "D1",
                    "deviceName": "Side Door",
                    "deviceType": "lock"
                }),
            ]],
        };

        let out = pull_with(&vault, &api).unwrap();
        assert_eq!(out.counts["events"], 2);
        let state = vault.read_ay_sync();
        // Watermark must be the MAX dateTime seen.
        assert_eq!(state.watermarks["house-abc"], 2000000000000i64);
    }

    #[test]
    fn cursor_file_missing_returns_default() {
        let vault = temp_vault("cursor_file_missing_returns_default");
        // No cursor file written yet.
        let state = vault.read_ay_sync();
        assert!(state.watermarks.is_empty());
        assert!(state.install_id.is_empty());
    }

    #[test]
    fn device_name_falls_back_to_device_id() {
        let act = json!({
            "id": "ev-x",
            "timestamp": 1665378377000i64,
            "action": "lock",
            "deviceID": "DEV123",
            "deviceType": "lock"
            // No "deviceName" field
        });
        let ev = to_home_event(&act, 1665378377000).unwrap();
        assert_eq!(ev["device"], "DEV123");
    }
}
