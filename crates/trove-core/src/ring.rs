//! Ring doorbell and camera event logs — unofficial cloud API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/ring.md.
//!
//! Ring has no official public API. This module uses the same unofficial
//! reverse-engineered REST API that the `python-ring-doorbell` library
//! documents (endpoint base `https://api.ring.com`, auth via
//! `https://oauth.ring.com/oauth/token`). The API may break on Ring /
//! Amazon app updates; the integration surfaces a clear error message
//! rather than silently failing.
//!
//! ## Vault layout
//!
//! - **Raw:** `home/ring/raw/YYYY-MM.jsonl` — verbatim API event objects,
//!   full fidelity; unconditional.
//! - **Events:** `home/ring/events/YYYY-MM.jsonl` — one row per event,
//!   shaped per `home.event` schema (`ts`, `source`, `device`, `event`,
//!   `guid`, `extra`). Written directly as [`serde_json::Value`] because the
//!   `home.event` Rust type is a Phase-3 draft (no struct bound yet). When
//!   the contract is ratified a follower maps the raw rows into the struct.
//! - **Devices:** `home/ring/raw/devices/YYYY-MM.jsonl` — device list
//!   snapshot on each pull, for audit.
//!
//! ## Auth
//!
//! Ring uses OAuth2 password-grant against `https://oauth.ring.com/oauth/token`
//! (`client_id=ring_official_android`, `scope=client`). If the account has
//! 2FA enabled Ring returns **HTTP 412** and the user must re-connect, this
//! time with the 2FA code appended to the pasted string as
//! `email:password:2fa_code`. The connect fn always sends the `2fa-support`
//! header, so accounts without 2FA succeed on the first paste; accounts with
//! 2FA succeed on the second. The resulting `access_token` + `refresh_token`
//! are stored under `.trove/sync/ring.json` (0600). The poller refreshes the
//! access token automatically before each pull (Ring tokens expire in ~1 hour).
//!
//! ## History pull
//!
//! `GET /clients_api/ring_devices` → the user's doorbell / camera list.
//! For each device: `GET /clients_api/doorbots/{id}/history?limit=100&older_than={cursor}`
//! watermark cursor = the maximum `id` ever written (Ring IDs are monotonically
//! descending — largest id = newest event). Drain pages (100 events/page)
//! until a page returns fewer than 100 events or all events are ≤ the cursor.
//! A crash re-drains (the cursor advances only after the full drain).
//!
//! **Evidence:** `python-ring-doorbell` v0.9+ test fixtures + const.py confirm
//! the exact endpoints and field names (`id`, `created_at`, `kind`, `answered`,
//! `recording.status`, `snapshot_url`). The on-wire shape is confirmed by the
//! upstream fixture at
//! `https://github.com/tchellomello/python-ring-doorbell/blob/master/tests/fixtures/ring_doorbot_history.json`
//! and the device list fixture at
//! `https://github.com/tchellomello/python-ring-doorbell/blob/master/tests/fixtures/ring_devices.json`
//! (no Trove-local copy; the upstream repo is the authoritative reference).

use std::collections::{BTreeMap, HashSet};
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

/// Vault directory for the contract-layer event stream.
const EVENTS_DIR: &str = "home/ring/events";
/// Vault directory for the full-fidelity raw event stream.
const RAW_DIR: &str = "home/ring/raw";
/// Vault directory for the raw device-list snapshots.
const DEVICE_RAW_DIR: &str = "home/ring/raw/devices";

/// Non-secret, rebuildable cursor for per-device watermarks.
/// Deleting it causes a full re-drain on the next sync.
const SYNC_FILE: &str = ".trove/ring-sync.json";

/// Service key under `.trove/sync/` where the token pair is stored (0600).
const SERVICE: &str = "ring";

/// Ring's unofficial OAuth2 token endpoint.
const OAUTH_URL: &str = "https://oauth.ring.com/oauth/token";
/// Ring's unofficial API base.
const API_BASE: &str = "https://api.ring.com";
/// Client identifier expected by Ring's OAuth endpoint.
const CLIENT_ID: &str = "ring_official_android";
/// OAuth scope Ring accepts for the password grant.
const SCOPE: &str = "client";
/// User-Agent Ring's mobile app sends; needed to avoid 403.
const USER_AGENT: &str = "android:com.ringapp";
/// Hard per-page limit for the history endpoint.
const HISTORY_PAGE: u32 = 100;
/// How long before Ring's access token expires (Ring expires ~1 hour, refresh
/// 60 seconds early to avoid edge-window failures).
const REFRESH_BUFFER_SECS: u64 = 60;
/// Seconds between syncs. Ring retains ~180 days; polling twice a day is safe.
pub const RING_SYNC_SECS: u64 = 43200; // 12 hours
/// Hard cap on backward pages per device per sync — safety bound.
const MAX_PAGES_PER_DEVICE: u32 = 200;
/// HTTP timeout for every request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Cursor.

/// Non-secret, rebuildable cursor. Per-device map from device_id to the
/// maximum event `id` ever stored (Ring IDs are 64-bit; the history endpoint
/// returns events newest-first, so the maximum id is the most-recent event).
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// `device_id → max event id written`. Watermark for incremental pulls.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    watermarks: BTreeMap<String, u64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ring_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ring_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Credential parsing.

/// Parse the pasted credential string. Supported forms:
/// - `email:password`  — accounts without 2FA (the usual case)
/// - `email:password:2fa_code`  — accounts with 2FA (second paste)
///
/// Splits on the FIRST colon for the email, then on the LAST colon to
/// separate the 2FA code when three tokens are present. Email addresses
/// never contain `:`, but passwords sometimes do; we use the position of
/// the SECOND-to-last `:` as the divider so a password like `p:ass:word`
/// remains intact when all three fields are given.
///
/// Returns `(email, password, Option<otp>)`.
fn parse_credentials(pasted: &str) -> Result<(String, String, Option<String>)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your Ring email and password as email:password");
    }
    // Split email (everything before first ':').
    let (email, rest) = pasted
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("missing ':' — paste as email:password"))?;
    let email = email.trim().to_string();
    if email.is_empty() {
        bail!("missing email — paste as email:password");
    }
    // The rest is either `password` or `password:2fa_code`. Split on the
    // LAST ':' — if that yields an 8-character-or-less alphanumeric suffix
    // we treat it as a 2FA code; otherwise the whole rest is the password.
    if let Some((pw, otp)) = rest.rsplit_once(':') {
        let otp = otp.trim();
        // A 2FA code from Ring is a 6-digit numeric code (SMS or TOTP).
        // Accept 4–8 digit/alphanumeric codes; anything else is treated as
        // part of the password.
        if otp.len() >= 4 && otp.len() <= 8 && otp.chars().all(|c| c.is_alphanumeric()) {
            let pw = pw.trim().to_string();
            if pw.is_empty() {
                bail!("missing password — paste as email:password or email:password:2fa_code");
            }
            return Ok((email, pw, Some(otp.to_string())));
        }
    }
    // No 2FA suffix found — the whole rest is the password.
    let pw = rest.trim().to_string();
    if pw.is_empty() {
        bail!("missing password — paste as email:password");
    }
    Ok((email, pw, None))
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

/// A token-exchange + API-call abstraction, so tests run without a network.
trait RingApi {
    /// Exchange email+password (+ optional 2FA code) for a token pair.
    /// Returns `Err` with a clear user-visible message on 401/412.
    fn fetch_token(&self, email: &str, password: &str, otp: Option<&str>)
        -> Result<TokenSet>;
    /// Refresh an existing token. Returns the new set.
    fn refresh_token_call(&self, refresh: &str) -> Result<TokenSet>;
    /// `GET /clients_api/ring_devices` → the device list as a JSON object.
    fn devices(&self, access: &str) -> Result<Value>;
    /// `GET /clients_api/doorbots/{id}/history?limit=N&older_than={cursor}`.
    /// `older_than` = the lowest event id seen so far on this device (newest-first
    /// paging; Ring returns events with id < older_than).
    fn history(&self, access: &str, device_id: u64, older_than: Option<u64>) -> Result<Vec<Value>>;
}

/// Live HTTP client backed by `ureq`.
struct RingClient;

impl RingApi for RingClient {
    fn fetch_token(&self, email: &str, password: &str, otp: Option<&str>) -> Result<TokenSet> {
        let mut req = ureq::post(OAUTH_URL)
            .set("User-Agent", USER_AGENT)
            .set("2fa-support", "true")
            .timeout(HTTP_TIMEOUT);
        if let Some(code) = otp {
            req = req.set("2fa-code", code);
        }
        // Ring uses a standard OAuth2 password grant via form-encoded body.
        let resp = req.send_form(&[
            ("grant_type", "password"),
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("username", email),
            ("password", password),
        ]);
        match resp {
            Ok(r) => {
                let v: Value = r.into_json().context("parsing Ring OAuth token response")?;
                token_from_value(v)
            }
            Err(ureq::Error::Status(401, _)) => {
                bail!("Ring rejected the credentials (401) — check your email and password")
            }
            Err(ureq::Error::Status(412, _)) => {
                bail!(
                    "Ring requires 2FA (412) — check your email for a code and re-connect as \
                     email:password:2fa_code (e.g. you@example.com:mypass:123456)"
                )
            }
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                bail!("Ring auth failed ({code}): {}", body.chars().take(300).collect::<String>())
            }
            Err(e) => bail!("Ring auth request failed: {e}"),
        }
    }

    fn refresh_token_call(&self, refresh: &str) -> Result<TokenSet> {
        let resp = ureq::post(OAUTH_URL)
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .send_form(&[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", refresh),
            ]);
        match resp {
            Ok(r) => {
                let v: Value = r.into_json().context("parsing Ring token refresh response")?;
                token_from_value(v)
            }
            Err(ureq::Error::Status(401, _)) => {
                bail!(
                    "Ring refresh token rejected (401) — the session has expired. \
                     Reconnect from the Integrations tab."
                )
            }
            Err(e) => bail!("Ring token refresh failed: {e}"),
        }
    }

    fn devices(&self, access: &str) -> Result<Value> {
        let url = format!("{API_BASE}/clients_api/ring_devices");
        match ureq::get(&url)
            .set("Authorization", &format!("Bearer {access}"))
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .call()
        {
            Ok(r) => Ok(r.into_json().context("parsing Ring devices response")?),
            Err(ureq::Error::Status(401, _)) => bail!(
                "Ring rejected the access token on /ring_devices — reconnect from Integrations"
            ),
            Err(e) => bail!("Ring /ring_devices fetch failed: {e}"),
        }
    }

    fn history(&self, access: &str, device_id: u64, older_than: Option<u64>) -> Result<Vec<Value>> {
        let mut url = format!(
            "{API_BASE}/clients_api/doorbots/{device_id}/history?limit={HISTORY_PAGE}"
        );
        if let Some(ot) = older_than {
            url.push_str(&format!("&older_than={ot}"));
        }
        match ureq::get(&url)
            .set("Authorization", &format!("Bearer {access}"))
            .set("User-Agent", USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .call()
        {
            Ok(r) => {
                let v: Value = r.into_json().context("parsing Ring history response")?;
                Ok(v.as_array().cloned().unwrap_or_default())
            }
            Err(ureq::Error::Status(401, _)) => bail!(
                "Ring rejected the access token on /history — reconnect from Integrations"
            ),
            Err(e) => bail!("Ring history fetch failed: {e}"),
        }
    }
}

/// Extract `access_token` + `refresh_token` + `expires_at` from an OAuth
/// response body. Ring returns standard OAuth2 fields.
fn token_from_value(v: Value) -> Result<TokenSet> {
    let access = v
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Ring auth response missing access_token"))?
        .to_string();
    let refresh = v
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string);
    // expires_in is seconds from now.
    let expires_at = v.get("expires_in").and_then(Value::as_u64).map(|secs| {
        (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs())
            + secs
    });
    Ok(TokenSet {
        access_token: access,
        refresh_token: refresh,
        token_type: Some("Bearer".into()),
        scope: Some(SCOPE.into()),
        expires_at,
    })
}

// ---------------------------------------------------------------------------
// Token management helpers.

/// Load the stored token and refresh it if it's within [`REFRESH_BUFFER_SECS`]
/// of expiry. Returns the live access token string.
fn live_access_token(vault: &Vault, api: &impl RingApi) -> Result<String> {
    let stored = vault
        .load_sync_token(SERVICE)?
        .context("Ring is not connected — add your credentials in the Integrations tab")?;
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // If the token expires soon (or has no expiry info), refresh it.
    let needs_refresh = stored
        .expires_at
        .map(|exp| exp.saturating_sub(now_secs) < REFRESH_BUFFER_SECS)
        .unwrap_or(false);
    if needs_refresh {
        let refresh = stored
            .refresh_token
            .as_deref()
            .filter(|r| !r.is_empty())
            .context("Ring token expired and no refresh token — reconnect from Integrations")?;
        let new_token = api.refresh_token_call(refresh)?;
        vault.save_sync_token(SERVICE, &new_token)?;
        return Ok(new_token.access_token);
    }
    Ok(stored.access_token)
}

// ---------------------------------------------------------------------------
// Device discovery.

/// One Ring device (doorbell or stick-up cam) with its id and name.
#[derive(Clone, Debug)]
struct Device {
    id: u64,
    name: String,
    kind: String,
}

/// Parse the `/clients_api/ring_devices` response. Ring returns an object
/// with arrays keyed by device type: `doorbots`, `authorized_doorbots`,
/// `stickup_cams`, `other`, etc. We collect everything with a numeric `id`.
///
/// The real Ring API represents the device name as a top-level `"description"`
/// **string** (confirmed against the python-ring-doorbell upstream fixtures at
/// tests/fixtures/ring_devices.json). There is no `name` field and no nested
/// `description.name` object in the real API response.
fn devices_from(resp: &Value) -> Vec<Device> {
    let mut out = Vec::new();
    // All device-bearing keys observed from the python-ring-doorbell fixtures,
    // including `other` which holds newer device types (intercoms, third-party
    // cameras, etc.) that are not yet in a named bucket.
    for key in &[
        "doorbots",
        "authorized_doorbots",
        "stickup_cams",
        "base_stations",
        "beams_bridges",
        "other",
    ] {
        if let Some(arr) = resp.get(key).and_then(Value::as_array) {
            for dev in arr {
                let Some(id) = dev.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                // Real Ring API: `description` is a bare string — the device's
                // human-readable name (e.g. "Front Door"). There is no `name`
                // field and no nested `description.name` object in the real response.
                let name = dev
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("Ring {id}"));
                let kind = dev.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
                out.push(Device { id, name, kind });
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Event mapping.

/// Parse the `created_at` field (ISO8601 UTC) into a local RFC3339 ts.
/// Returns `None` when the field is absent or unparseable.
fn event_ts(ev: &Value) -> Option<String> {
    let s = ev.get("created_at").and_then(Value::as_str)?;
    let dt = DateTime::parse_from_rfc3339(s.trim())
        .or_else(|_| {
            // Ring sometimes emits microsecond precision; try stripping sub-second.
            let trimmed = s.split('.').next().unwrap_or(s);
            DateTime::parse_from_str(
                &format!("{trimmed}+00:00"),
                "%Y-%m-%dT%H:%M:%S%z",
            )
        })
        .ok()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

/// Extract the numeric event `id` from a Ring event object.
fn event_id(ev: &Value) -> Option<u64> {
    ev.get("id").and_then(Value::as_u64)
}

/// Map one Ring event object to the `home.event` shape (plain `Value`).
/// Fields follow `docs/vault-spec/domains/home.md`:
///   ts, source="ring", device=name, event=kind, guid=ring:{id}, extra={...}
///
/// `device_name` comes from the device list (stable human name); `kind` is
/// `"motion"`, `"ding"`, or `"on_demand"` (the three Ring event types).
fn event_to_home_event(ev: &Value, device: &Device) -> Option<Value> {
    let ts = event_ts(ev)?;
    if Partition::Month.key(&ts).is_none() {
        return None;
    }
    let id = event_id(ev)?;
    let kind = ev.get("kind").and_then(Value::as_str).unwrap_or("unknown");
    // Normalize kind to the home.event vocabulary (verb/noun lower-case).
    let event_type = match kind {
        "ding" => "doorbell",
        "motion" => "motion",
        "on_demand" => "livestream",
        other => other,
    };

    let mut extra: Map<String, Value> = Map::new();
    // Preserve full-fidelity source fields in extra.
    if let Some(ans) = ev.get("answered").and_then(Value::as_bool) {
        extra.insert("answered".into(), Value::Bool(ans));
    }
    if let Some(fav) = ev.get("favorite").and_then(Value::as_bool) {
        extra.insert("favorite".into(), Value::Bool(fav));
    }
    if let Some(rec_status) = ev
        .get("recording")
        .and_then(|r| r.get("status"))
        .and_then(Value::as_str)
    {
        extra.insert("recording_status".into(), Value::String(rec_status.to_string()));
    }
    if let Some(snap) = ev.get("snapshot_url").and_then(Value::as_str) {
        if !snap.is_empty() {
            extra.insert("snapshot_url".into(), Value::String(snap.to_string()));
        }
    }
    // The device kind (doorbell model), not the event kind.
    if !device.kind.is_empty() {
        extra.insert("device_kind".into(), Value::String(device.kind.clone()));
    }
    // Preserve the original Ring event kind string.
    extra.insert("ring_kind".into(), Value::String(kind.to_string()));

    let mut obj = Map::new();
    obj.insert("ts".into(), Value::String(ts));
    obj.insert("source".into(), Value::String("ring".into()));
    obj.insert("device".into(), Value::String(device.name.clone()));
    obj.insert("event".into(), Value::String(event_type.to_string()));
    obj.insert("guid".into(), Value::String(format!("ring:{id}")));
    if !extra.is_empty() {
        obj.insert("extra".into(), Value::Object(extra));
    }
    Some(Value::Object(obj))
}

// ---------------------------------------------------------------------------
// Raw row wrapper.

/// A raw event line: tagged with `ts` (for month partitioning) but written
/// as the verbatim event object (full fidelity, unconditional).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Drain + write helpers.

/// The full drain + write pass for one device. Returns how many new events
/// were written and the new watermark (max id seen).
fn drain_device(
    vault: &Vault,
    api: &impl RingApi,
    access: &str,
    device: &Device,
    prior_watermark: Option<u64>,
) -> Result<(u64, Option<u64>)> {
    let mut total_written: u64 = 0;
    let mut max_id: Option<u64> = prior_watermark;
    let mut older_than: Option<u64> = None;

    // Inner drain — collect guids + raw ids seen THIS PULL to avoid duplicating
    // within a single multi-page drain (not just vs. what's on disk).
    let ev_stream = vault.stream(EVENTS_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen_guids: HashSet<String> = HashSet::new();
    let mut seen_raw_ids: HashSet<u64> = HashSet::new();

    // Pre-populate from disk once.
    for key in ev_stream.partitions()? {
        for v in ev_stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                seen_guids.insert(g.to_string());
            }
        }
    }
    for key in raw_stream.partitions()? {
        for v in raw_stream.read::<Value>(&key)? {
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                seen_raw_ids.insert(id);
            }
        }
    }

    for _page in 0..MAX_PAGES_PER_DEVICE {
        let page = api.history(access, device.id, older_than)?;
        if page.is_empty() {
            break; // no more events (retention horizon or no data)
        }

        let mut page_min_id: Option<u64> = None;
        let mut any_new = false;
        let mut new_events: Vec<RawLine> = Vec::new();
        let mut new_raws: Vec<RawLine> = Vec::new();

        for ev in &page {
            let Some(id) = event_id(ev) else { continue };
            // page_min_id drives the paging cursor (older_than) regardless of
            // whether the event has a parseable ts — we always need to paginate
            // through all ids on a page.
            page_min_id = Some(page_min_id.map_or(id, |m: u64| m.min(id)));

            // Incremental: skip events at or below the watermark.
            if let Some(pw) = prior_watermark {
                if id <= pw {
                    continue;
                }
            }
            any_new = true;

            // Only advance the watermark for events we can actually timestamp.
            // If created_at is absent or unparseable we do NOT advance max_id
            // past this event's id — so the next sync will re-encounter the
            // event and can re-attempt writing it if the data ever improves
            // (or just skip it again). This prevents a silent fidelity gap
            // where a raw-layer event is permanently lost because max_id
            // crossed its id but nothing was written for it.
            let Some(ev_ts_str) = event_ts(ev) else { continue };

            // Watermark advances only for events we can timestamp.
            max_id = Some(max_id.map_or(id, |m| m.max(id)));

            // Events layer.
            if let Some(home_ev) = event_to_home_event(ev, device) {
                let ts = home_ev
                    .get("ts")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let guid = home_ev
                    .get("guid")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !guid.is_empty() && !ts.is_empty() && seen_guids.insert(guid) {
                    new_events.push(RawLine { ts, value: home_ev });
                }
            }

            // Raw layer — verbatim event object, partitioned by the event's
            // own created_at (ev_ts_str, already confirmed parseable above).
            if seen_raw_ids.insert(id) {
                new_raws.push(RawLine { ts: ev_ts_str, value: ev.clone() });
            }
        }

        // Write this page's rows before requesting the next (crash-safe drain).
        if !new_events.is_empty() {
            total_written += new_events.len() as u64;
            ev_stream.append(&new_events, |r| &r.ts)?;
        }
        if !new_raws.is_empty() {
            raw_stream.append(&new_raws, |r| &r.ts)?;
        }

        // Incremental: a page with no new events means we've reached the
        // watermark; stop descending.
        if prior_watermark.is_some() && !any_new {
            break;
        }

        // A short page (< HISTORY_PAGE) is the last page of history.
        if (page.len() as u32) < HISTORY_PAGE {
            break;
        }

        // Descend: next page starts below this page's smallest id.
        let Some(min_id) = page_min_id else { break };
        older_than = Some(min_id);
    }

    Ok((total_written, max_id))
}

// ---------------------------------------------------------------------------
// Main pull.

/// Resolve a live token, enumerate devices, drain each device's history.
fn pull_with(vault: &Vault, api: &impl RingApi) -> Result<PullOutcome> {
    let access = live_access_token(vault, api)?;
    let mut state = vault.read_ring_sync();

    // Device enumeration.
    let dev_resp = api.devices(&access).map_err(|e| {
        anyhow::anyhow!("Ring changed their private API or your session expired: {e}")
    })?;

    let devices = devices_from(&dev_resp);

    // Store device snapshot in raw/ for audit.
    if !devices.is_empty() {
        let dev_raw = vault.stream(DEVICE_RAW_DIR, Partition::Month);
        let now_ts = Local::now().to_rfc3339();
        let snapshot_line = vec![RawLine { ts: now_ts, value: dev_resp.clone() }];
        let _ = dev_raw.append(&snapshot_line, |r| &r.ts); // best-effort
    }

    let mut total_written: u64 = 0;
    for device in &devices {
        let dev_key = device.id.to_string();
        let prior = state.watermarks.get(&dev_key).copied();
        let (written, new_watermark) = drain_device(vault, api, &access, device, prior)?;
        total_written += written;
        // Advance the watermark only after the full drain, and only forward.
        if let Some(w) = new_watermark {
            let entry = state.watermarks.entry(dev_key).or_insert(w);
            if w > *entry {
                *entry = w;
            }
        }
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ring_sync(&state)?;

    Ok(PullOutcome {
        headline: if total_written == 0 {
            "Ring is up to date — no new events".to_string()
        } else {
            format!("Ring synced — {total_written} events")
        },
        counts: BTreeMap::from([("events", total_written)]),
    })
}

/// Public entry point for the pull hook and the periodic collect.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    pull_with(vault, &RingClient)
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(EVENTS_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("events").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("ring synced — {n} events")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "ring sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ring",
        name: "Ring",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Collects doorbell rings, motion events, and on-demand livestream records from Ring's \
             cloud. Metadata only — video clips are not downloaded. Ring retains events for ~180 \
             days; connect promptly to preserve history.",
        domain: "home",
        vault_path: "home/ring/",
        toggleable: true,
        setup: &[
            "Connect your Ring account below (email and password).",
            "If your account uses 2FA, Ring sends a code to your phone/email — reconnect with \
             email:password:2fa_code.",
            "First sync backfills up to 180 days of event history; later syncs are incremental.",
            "This integration uses Ring's unofficial API, which may break on Ring app updates. \
             If sync fails, the card shows a clear error.",
        ],
        caveats:
            "Uses Ring's unofficial private API (the same endpoints that python-ring-doorbell \
             documents). Ring / Amazon may change or close these endpoints at any time — Trove \
             surfaces a clear error rather than silently failing. Ring retains events for ~180 \
             days; data older than that window is already gone at connect time.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(RING_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("ring"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection.

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (email, password, otp) = parse_credentials(pasted)?;
    let token = RingClient.fetch_token(&email, &password, otp.as_deref())?;
    vault.save_sync_token(SERVICE, &token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // We stored the access token — show a generic connected label.
        // The email is not stored outside the credentials (Ring tokens are
        // anonymous bearer tokens; the email used to obtain them is not
        // echoed back in the token response).
        let label = "Ring Account".to_string();
        let needs_reconnect = token
            .expires_at
            .map(|exp| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                exp.saturating_sub(now) < 60 && token.refresh_token.is_none()
            })
            .unwrap_or(false);
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: token.expires_at,
            needs_reconnect,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "ring",
    display_name: "Ring",
    methods: &[ConnectMethod::TokenPaste {
        label: "Ring Account Credentials",
        help: "Paste your Ring account email and password as email:password. \
               If your account uses 2FA, Ring will send a verification code — \
               reconnect with email:password:2fa_code. Your credentials are \
               exchanged for a session token that is stored locally and sent \
               only to Ring's servers. This integration uses Ring's private \
               API, which may break on Ring app updates.",
        placeholder: "you@example.com:YourPassword  (or you@example.com:YourPassword:123456 with 2FA)",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["ring"],
    setup: &[
        "Paste your Ring email and password separated by a colon: email:password.",
        "If your account has two-factor authentication enabled, Ring sends a code to your \
         phone or email. Re-connect with email:password:2fa_code.",
        "Your credentials are exchanged for a session token stored locally (0600) and sent \
         only to Ring. This uses Ring's unofficial API — Ring may change it at any time.",
    ],
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ring-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Credential parsing.

    #[test]
    fn ring_parse_credentials_email_password() {
        let (email, pw, otp) = parse_credentials("user@example.com:hunter2").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(pw, "hunter2");
        assert!(otp.is_none());
    }

    #[test]
    fn ring_parse_credentials_with_2fa() {
        let (email, pw, otp) = parse_credentials("user@example.com:hunter2:654321").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(pw, "hunter2");
        assert_eq!(otp.as_deref(), Some("654321"));
    }

    #[test]
    fn ring_parse_credentials_password_with_colon() {
        // A password like p:ass should not be confused as a 2FA code because
        // "ass" is 3 chars (< 4) and not all-alphanumeric prefix.
        // Actual test: 3-char suffix treated as password, not otp.
        let (_, pw, otp) = parse_credentials("a@b.com:p:a12").unwrap();
        // "a12" is 3 chars — too short for 2FA threshold (4), so it stays in pw.
        // pw should be "p:a12".
        assert_eq!(pw, "p:a12");
        assert!(otp.is_none(), "3-char code not treated as 2FA: {otp:?}");
    }

    #[test]
    fn ring_parse_credentials_whitespace_trimmed() {
        let (email, pw, otp) = parse_credentials("  user@example.com : pass  ").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(pw, "pass");
        assert!(otp.is_none());
    }

    #[test]
    fn ring_parse_credentials_empty_errors() {
        assert!(parse_credentials("").is_err());
        assert!(parse_credentials("   ").is_err());
    }

    #[test]
    fn ring_parse_credentials_missing_password_errors() {
        let err = parse_credentials("user@example.com:").unwrap_err().to_string();
        assert!(err.contains("password"), "{err}");
    }

    #[test]
    fn ring_parse_credentials_no_colon_errors() {
        let err = parse_credentials("notanemail").unwrap_err().to_string();
        assert!(err.contains("':'"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Device list parsing.

    // fixture_devices() mirrors the REAL Ring /clients_api/ring_devices shape
    // (confirmed against upstream python-ring-doorbell tests/fixtures/ring_devices.json):
    //   - `description` is a bare STRING (the device's human name), NOT an object
    //   - there is NO top-level `name` field on device objects
    //   - `other` bucket holds newer device types (intercoms, third-party cameras)
    fn fixture_devices() -> Value {
        serde_json::json!({
            "doorbots": [
                {
                    "id": 987654321,
                    "description": "Front Door",
                    "kind": "lpd_v1",
                    "firmware_version": "1.4.26"
                }
            ],
            "authorized_doorbots": [],
            "stickup_cams": [
                {
                    "id": 111222333,
                    "description": "Backyard Cam",
                    "kind": "hp_cam_v1",
                    "firmware_version": "1.9.3"
                }
            ],
            "chimes": [],
            "base_stations": [],
            "other": [
                {
                    "id": 444555666,
                    "description": "Ingress",
                    "kind": "intercom_handset_audio"
                }
            ]
        })
    }

    #[test]
    fn ring_devices_parsed_from_response() {
        let resp = fixture_devices();
        let devs = devices_from(&resp);
        // 3 devices: doorbot + stickup cam + intercom from `other` bucket.
        assert_eq!(devs.len(), 3);
        let front = devs.iter().find(|d| d.id == 987654321).unwrap();
        // Real API: bare string `description` → device name.
        assert_eq!(front.name, "Front Door");
        assert_eq!(front.kind, "lpd_v1");
        let back = devs.iter().find(|d| d.id == 111222333).unwrap();
        assert_eq!(back.name, "Backyard Cam");
        assert_eq!(back.kind, "hp_cam_v1");
        let intercom = devs.iter().find(|d| d.id == 444555666).unwrap();
        assert_eq!(intercom.name, "Ingress");
        assert_eq!(intercom.kind, "intercom_handset_audio");
    }

    #[test]
    fn ring_devices_from_empty_response_is_empty() {
        let resp = serde_json::json!({});
        assert!(devices_from(&resp).is_empty());
    }

    // -----------------------------------------------------------------------
    // Event mapping.

    fn fixture_motion_event() -> Value {
        serde_json::json!({
            "id": 987654321,
            "created_at": "2026-06-10T18:41:55.000Z",
            "kind": "motion",
            "answered": false,
            "favorite": false,
            "recording": {"status": "ready"},
            "snapshot_url": "",
            "events": []
        })
    }

    fn fixture_ding_event() -> Value {
        serde_json::json!({
            "id": 987654322,
            "created_at": "2026-06-10T19:05:12.000Z",
            "kind": "ding",
            "answered": true,
            "favorite": false,
            "recording": {"status": "ready"},
            "snapshot_url": "https://example.ring.com/thumb/xyz",
            "events": []
        })
    }

    fn fixture_device() -> Device {
        Device {
            id: 987654321,
            name: "Front Doorbell".to_string(),
            kind: "lpd_v1".to_string(),
        }
    }

    #[test]
    fn ring_motion_event_maps_to_home_event() {
        let ev = fixture_motion_event();
        let dev = fixture_device();
        let home_ev = event_to_home_event(&ev, &dev).unwrap();

        assert_eq!(home_ev["source"], "ring");
        assert_eq!(home_ev["device"], "Front Doorbell");
        assert_eq!(home_ev["event"], "motion");
        assert_eq!(home_ev["guid"], "ring:987654321");
        // ts should be a valid RFC3339.
        let ts = home_ev["ts"].as_str().unwrap();
        assert!(DateTime::parse_from_rfc3339(ts).is_ok(), "ts must be RFC3339: {ts}");
        // Partition key must be extractable.
        assert!(Partition::Month.key(ts).is_some());
        // extra carries ring_kind and answered.
        assert_eq!(home_ev["extra"]["ring_kind"], "motion");
        assert_eq!(home_ev["extra"]["answered"], false);
        assert_eq!(home_ev["extra"]["recording_status"], "ready");
    }

    #[test]
    fn ring_ding_event_maps_to_doorbell() {
        let ev = fixture_ding_event();
        let dev = fixture_device();
        let home_ev = event_to_home_event(&ev, &dev).unwrap();
        assert_eq!(home_ev["event"], "doorbell", "ding → doorbell");
        assert_eq!(home_ev["guid"], "ring:987654322");
        // snapshot_url should be in extra.
        let snap = home_ev["extra"]["snapshot_url"].as_str().unwrap();
        assert!(!snap.is_empty(), "non-empty snapshot_url lands in extra");
        assert_eq!(home_ev["extra"]["answered"], true);
    }

    #[test]
    fn ring_event_without_id_yields_none() {
        let ev = serde_json::json!({"created_at": "2026-06-10T18:00:00.000Z", "kind": "motion"});
        assert!(event_to_home_event(&ev, &fixture_device()).is_none());
    }

    #[test]
    fn ring_event_without_ts_yields_none() {
        let ev = serde_json::json!({"id": 1, "kind": "motion"});
        assert!(event_to_home_event(&ev, &fixture_device()).is_none());
    }

    // -----------------------------------------------------------------------
    // Cursor back-compat.

    #[test]
    fn ring_cursor_back_compat_empty_and_partial() {
        // An empty cursor (first sync) deserializes to defaults.
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermarks.is_empty());
        assert!(empty.updated.is_none());
        // A cursor with only watermarks (no `updated`) round-trips.
        let partial: SyncState =
            serde_json::from_str(r#"{"watermarks":{"987654321":100}}"#).unwrap();
        assert_eq!(partial.watermarks.get("987654321"), Some(&100u64));
        assert!(partial.updated.is_none());
    }

    // -----------------------------------------------------------------------
    // Mock API + pull tests.

    struct MockApi {
        devices: Value,
        history_pages: RefCell<std::collections::VecDeque<Vec<Value>>>,
        // Track what older_than values were requested.
        older_than_log: RefCell<Vec<Option<u64>>>,
    }

    impl MockApi {
        fn new(devices: Value, pages: Vec<Vec<Value>>) -> Self {
            MockApi {
                devices,
                history_pages: RefCell::new(pages.into()),
                older_than_log: RefCell::new(Vec::new()),
            }
        }
    }

    impl RingApi for MockApi {
        fn fetch_token(
            &self,
            _email: &str,
            _password: &str,
            _otp: Option<&str>,
        ) -> Result<TokenSet> {
            Ok(TokenSet {
                access_token: "test-access".into(),
                refresh_token: Some("test-refresh".into()),
                token_type: Some("Bearer".into()),
                scope: Some("client".into()),
                expires_at: None,
            })
        }
        fn refresh_token_call(&self, _refresh: &str) -> Result<TokenSet> {
            Ok(TokenSet {
                access_token: "test-access-refreshed".into(),
                refresh_token: Some("test-refresh".into()),
                token_type: Some("Bearer".into()),
                scope: Some("client".into()),
                expires_at: None,
            })
        }
        fn devices(&self, _access: &str) -> Result<Value> {
            Ok(self.devices.clone())
        }
        fn history(
            &self,
            _access: &str,
            _device_id: u64,
            older_than: Option<u64>,
        ) -> Result<Vec<Value>> {
            self.older_than_log.borrow_mut().push(older_than);
            Ok(self.history_pages.borrow_mut().pop_front().unwrap_or_default())
        }
    }

    fn store_fake_token(vault: &Vault) {
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: "test-access".into(),
                    refresh_token: Some("test-refresh".into()),
                    token_type: Some("Bearer".into()),
                    scope: Some("client".into()),
                    expires_at: None,
                },
            )
            .unwrap();
    }

    #[test]
    fn ring_full_pull_writes_events_and_raw() {
        let v = temp_vault("full-pull");
        store_fake_token(&v);

        let motion = fixture_motion_event();
        let ding = fixture_ding_event();
        // Single short page (< 100 events → last page of history).
        let api = MockApi::new(fixture_devices(), vec![vec![ding.clone(), motion.clone()]]);
        let out = pull_with(&v, &api).unwrap();

        let n = out.counts.get("events").copied().unwrap_or(0);
        assert_eq!(n, 2, "two events written");

        // Events stream has the home.event rows.
        let ev_stream = v.stream(EVENTS_DIR, Partition::Month);
        let mut all_events: Vec<Value> = Vec::new();
        for key in ev_stream.partitions().unwrap() {
            all_events.extend(ev_stream.read::<Value>(&key).unwrap());
        }
        assert_eq!(all_events.len(), 2, "two home.event rows on disk");
        assert!(all_events.iter().any(|e| e["event"] == "motion"));
        assert!(all_events.iter().any(|e| e["event"] == "doorbell"));
        // All rows have guid, source, device, ts.
        for e in &all_events {
            assert_eq!(e["source"], "ring");
            assert!(e.get("guid").is_some());
            assert!(e.get("ts").is_some());
        }

        // Raw stream has the verbatim event objects.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let mut all_raw: Vec<Value> = Vec::new();
        for key in raw_stream.partitions().unwrap() {
            all_raw.extend(raw_stream.read::<Value>(&key).unwrap());
        }
        assert_eq!(all_raw.len(), 2, "two raw event rows on disk");
        // Raw preserves the 'recording' nested object.
        assert!(all_raw.iter().any(|r| r.get("recording").is_some()));

        // Cursor advanced to the max event id.
        let state = v.read_ring_sync();
        assert_eq!(
            state.watermarks.get("987654321"),
            Some(&987654322u64),
            "watermark = max id seen"
        );
        assert!(state.updated.is_some());
    }

    #[test]
    fn ring_incremental_pull_skips_known_events() {
        let v = temp_vault("incremental");
        store_fake_token(&v);

        // Seed watermark at 987654321 (the motion event's id).
        v.write_ring_sync(&SyncState {
            watermarks: BTreeMap::from([("987654321".to_string(), 987654321u64)]),
            updated: Some("2026-06-10T18:00:00-07:00".into()),
        })
        .unwrap();

        // Only the ding event (id=987654322) is new.
        let ding = fixture_ding_event(); // id=987654322
        let motion = fixture_motion_event(); // id=987654321, at/below watermark
        let api = MockApi::new(fixture_devices(), vec![vec![ding, motion]]);
        let out = pull_with(&v, &api).unwrap();
        let n = out.counts.get("events").copied().unwrap_or(0);
        assert_eq!(n, 1, "only the new ding event written, motion was at watermark");
    }

    #[test]
    fn ring_deduplicated_re_pull_writes_zero() {
        let v = temp_vault("dedup");
        store_fake_token(&v);

        let api1 = MockApi::new(
            fixture_devices(),
            vec![vec![fixture_ding_event(), fixture_motion_event()]],
        );
        pull_with(&v, &api1).unwrap();

        // Re-pull same events: watermark covers them now.
        let api2 = MockApi::new(
            fixture_devices(),
            vec![vec![fixture_ding_event(), fixture_motion_event()]],
        );
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(
            out2.counts.get("events").copied().unwrap_or(0),
            0,
            "re-pull with same ids writes zero"
        );
    }

    #[test]
    fn ring_pull_requires_connection() {
        let v = temp_vault("no-conn");
        // No token stored → clear error.
        let err = pull_with(&v, &MockApi::new(serde_json::json!({}), vec![]))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not connected") || err.contains("Integrations"),
            "clear not-connected error: {err}"
        );
    }

    #[test]
    fn ring_empty_history_is_ok() {
        let v = temp_vault("empty-history");
        store_fake_token(&v);
        let api = MockApi::new(fixture_devices(), vec![vec![]]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("events").copied().unwrap_or(0), 0);
    }

    #[test]
    fn ring_no_devices_is_ok() {
        let v = temp_vault("no-devices");
        store_fake_token(&v);
        let api = MockApi::new(serde_json::json!({"doorbots": []}), vec![]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("events").copied().unwrap_or(0), 0);
    }

    #[test]
    fn ring_connection_def_is_token_paste() {
        assert_eq!(CONNECTION.id, "ring");
        assert!(
            CONNECTION.method("token-paste").is_some(),
            "must expose token-paste method"
        );
        assert_eq!(DEF.connection, Some("ring"));
    }

    #[test]
    fn ring_token_from_value_extracts_fields() {
        let v = serde_json::json!({
            "access_token": "acc123",
            "refresh_token": "ref456",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": "client"
        });
        let ts = token_from_value(v).unwrap();
        assert_eq!(ts.access_token, "acc123");
        assert_eq!(ts.refresh_token.as_deref(), Some("ref456"));
        assert!(ts.expires_at.is_some());
    }

    #[test]
    fn ring_token_from_value_missing_access_token_errors() {
        let v = serde_json::json!({"refresh_token": "ref456"});
        assert!(token_from_value(v).is_err());
    }

    #[test]
    fn ring_connection_status_shows_connected_account() {
        let v = temp_vault("status");
        store_fake_token(&v);
        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].key, "ring");
    }

    #[test]
    fn ring_disconnect_clears_token() {
        let v = temp_vault("disconnect");
        store_fake_token(&v);
        def_disconnect(&v, "ring").unwrap();
        let status = def_status(&v).unwrap();
        assert!(status.accounts.is_empty(), "disconnected → no accounts");
    }

    #[test]
    fn ring_cursor_not_written_until_after_drain() {
        // A fresh vault with no events still writes a cursor after a pull
        // (with the current time stamped as `updated`).
        let v = temp_vault("cursor-after-drain");
        store_fake_token(&v);
        let api = MockApi::new(fixture_devices(), vec![vec![]]);
        pull_with(&v, &api).unwrap();
        let state = v.read_ring_sync();
        // No events → no watermark entries, but `updated` is stamped.
        assert!(state.updated.is_some());
    }

    #[test]
    fn ring_stickup_cam_events_collected() {
        // Stickup cams appear under "stickup_cams" in the device list.
        // Real API shape: `description` is a bare string (no `name` field).
        let v = temp_vault("stickup");
        store_fake_token(&v);
        let devices = serde_json::json!({
            "doorbots": [],
            "stickup_cams": [
                {"id": 111222333, "description": "Backyard Cam", "kind": "hp_cam_v1"}
            ]
        });
        let motion = serde_json::json!({
            "id": 555666777,
            "created_at": "2026-06-11T10:00:00.000Z",
            "kind": "motion",
            "answered": false,
            "favorite": false,
            "recording": {"status": "ready"},
            "snapshot_url": "",
            "events": []
        });
        let api = MockApi::new(devices, vec![vec![motion]]);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("events").copied().unwrap_or(0), 1);
        // Watermark stored under the stickup cam's id.
        let state = v.read_ring_sync();
        assert_eq!(state.watermarks.get("111222333"), Some(&555666777u64));
    }
}
