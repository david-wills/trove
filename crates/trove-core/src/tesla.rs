//! Tesla — Fleet API periodic vehicle location and telemetry poller.
//!
//! Polls `GET /api/1/vehicles/{id}/vehicle_data` for every linked vehicle on a
//! 15-minute cadence and writes two layers to the vault:
//!
//! - **Raw layer** (`location/tesla/raw/YYYY-MM.jsonl`): full `vehicle_data`
//!   JSON response verbatim, one row per vehicle per poll, month-partitioned.
//! - **Contract layer** (`location/tesla/YYYY-MM-DD.jsonl`): one [`Fix`] per
//!   vehicle per poll when location is available, day-partitioned by the local
//!   `ts` of the GPS fix (`drive_state.gps_as_of`). Tesla-specific fields
//!   (odometer, battery, charging state, VIN) ride in `extra`.
//!
//! ## Auth
//!
//! OAuth 2.0 Authorization Code flow at `auth.tesla.com`. App registration is
//! required at `developer.tesla.com`; credentials are baked at build time via
//! `TROVE_TESLA_CLIENT_ID` / `TROVE_TESLA_CLIENT_SECRET` (BYO via ConnectSpec
//! also supported). Scopes: `vehicle_location vehicle_state vehicle_charging_cmds
//! openid email offline_access`.  `vehicle_location` became mandatory for
//! location data in the 2025 Fleet API update.  Assigned production redirect
//! port: **38694**.
//!
//! This connection is **shared** with `tesla-energy` (one Tesla login, multiple
//! defs). The `CONNECTION` is declared here; `tesla-energy` references it via
//! `connection: Some("tesla")`.
//!
//! ## Field provenance
//!
//! Field names confirmed from the unofficial timdorr community docs
//! (tesla-api.timdorr.com) which document the same `vehicle_data` endpoint
//! Tesla's Fleet API inherits:
//!
//! - `drive_state.latitude` / `.longitude` — float decimal degrees (WGS84)
//! - `drive_state.heading` — integer, degrees (0–360)
//! - `drive_state.speed` — numeric or null (mph); stored in extra as-is
//! - `drive_state.gps_as_of` — Unix epoch seconds (when GPS was captured)
//! - `drive_state.timestamp` — millisecond epoch (when API served the data)
//! - `vehicle_state.odometer` — decimal miles
//! - `charge_state.battery_level` — integer percent
//! - `charge_state.battery_range` — decimal miles
//! - `charge_state.charging_state` — string: "Charging", "Stopped", "Complete"
//! - Top-level: `id`, `id_s`, `vehicle_id`, `vin`, `display_name`, `state`
//!
//! ## Privacy
//!
//! Vehicle location is a continuous trail of the owner's movements. This
//! integration ships `default_on: false` and is marked `toggleable: true` to
//! require explicit opt-in. All vault writes stay local.
//!
//! Brief: docs/integrations/tesla.md

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::location::Fix;
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "tesla";
const SYNC_FILE: &str = ".trove/tesla-sync.json";
const DIR: &str = "location/tesla";
const RAW_DIR: &str = "location/tesla/raw";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// 15-minute cadence — a parked car won't move; active driving accumulates
/// a point every 15 min which is enough to reconstruct the route at read time.
pub const TESLA_SYNC_SECS: u64 = 900;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static TESLA: Provider = Provider {
    service: SERVICE,
    display_name: "Tesla",
    // User-facing consent.
    auth_url: "https://auth.tesla.com/oauth2/v3/authorize",
    token_url: "https://auth.tesla.com/oauth2/v3/token",
    // vehicle_location mandatory since Jan 2025; offline_access for refresh token.
    scopes: "vehicle_location vehicle_state openid email offline_access",
    // Assigned unique production port for Tesla.
    redirect_port: 38694,
    use_pkce: false,
    basic_auth: false,
    default_client_id: option_env!("TROVE_TESLA_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_TESLA_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection (shared with tesla-energy).

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(TESLA.service)?.is_some()
        || TESLA.default_credentials().is_some();
    let accounts = match vault.load_sync_token(TESLA.service)? {
        Some(token) => vec![ConnectedAccount {
            key: TESLA.service.to_string(),
            label: TESLA.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Tesla issues refresh tokens; expiry alone does not need reconnect.
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

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds
/// one `&crate::tesla::CONNECTION,` line). Shared with `tesla-energy`.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "tesla",
    display_name: "Tesla",
    methods: &[ConnectMethod::OAuth {
        provider: &TESLA,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["tesla"],
    setup: &[
        "Register an app at developer.tesla.com and add http://localhost:38694/callback as an \
         allowed redirect URI (must match exactly).",
        "Paste the Client ID and Client Secret here — stored locally, used for every future \
         connect.",
        "After connecting you will be prompted to grant access to your vehicle(s).",
    ],
};

/// Interactive OAuth connect: opens the consent page, waits for the redirect,
/// saves the token. Blocking — callers must be off the main thread.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(TESLA.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(TESLA.service)?
            .or_else(|| TESLA.default_credentials())
            .context(
                "no Tesla app credentials — register an app at developer.tesla.com and \
                 enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&TESLA, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(TESLA.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face (DEF).

fn def_last_data(vault: &Vault) -> Option<String> {
    // Show the newest contract day file if any, otherwise the raw partition.
    crate::registry::newest_stem(&vault.root().join(DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(RAW_DIR)))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(total > 0, || {
                format!("tesla synced — {total} snapshot(s) recorded")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("tesla sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub already there —
/// do NOT add another `pub mod` or INTEGRATIONS line).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tesla",
        name: "Tesla",
        kind: IntegrationKind::CloudSync,
        // Vehicle location is a continuous trail: require explicit opt-in.
        default_on: false,
        description:
            "Periodically polls the Tesla Fleet API for your vehicle's location, odometer, and \
             charge state. Builds a timestamped position trail locally — a private \"where my car \
             has been\" log. Polls every 15 minutes; only writes when your vehicle reports new \
             GPS data.",
        domain: "location",
        vault_path: "location/tesla/",
        toggleable: true,
        setup: &[
            "Tesla vehicle location is a sensitive trail of your movements — enabling this stores \
             it locally in your vault.",
            "Connect your Tesla account on this card (requires a developer.tesla.com app \
             registration with vehicle_location scope).",
            "Polls every 15 minutes; full-fidelity raw data is archived alongside the \
             normalised location trail.",
        ],
        caveats:
            "The Tesla Fleet API returns the vehicle's *current* state only — it does not store \
             historical trip data server-side. Trove accumulates a trail by polling; gaps occur \
             when the vehicle is offline or asleep. A Tesla app registration at \
             developer.tesla.com is required (Needs-login).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(TESLA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("tesla"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor.

/// Persisted sync state. Not a secret.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct SyncState {
    /// RFC3339 UTC timestamp of the last successful poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated: Option<String>,
}

impl Vault {
    pub(crate) fn read_tesla_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_tesla_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API types — field names confirmed from tesla-api.timdorr.com documentation.

/// `drive_state` sub-object from `vehicle_data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DriveState {
    /// Latitude, decimal degrees (WGS84).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// Longitude, decimal degrees (WGS84).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    /// Course over ground, degrees 0–360.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<i64>,
    /// Speed in mph (null when stationary).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// Unix epoch seconds: when the GPS fix was captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gps_as_of: Option<i64>,
    /// Unix epoch milliseconds: when the API served this response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// Shift state: "P", "D", "R", "N", or null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shift_state: Option<String>,
}

/// `vehicle_state` sub-object from `vehicle_data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VehicleState {
    /// Odometer reading, in miles (decimal).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer: Option<f64>,
}

/// `charge_state` sub-object from `vehicle_data`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ChargeState {
    /// Battery level, integer percent 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_level: Option<i64>,
    /// Battery range in miles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_range: Option<f64>,
    /// "Charging", "Stopped", "Complete", "Disconnected", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charging_state: Option<String>,
}

/// The top-level `vehicle_data` response, wrapped in a `"response"` envelope.
/// Only the fields Trove maps are typed; everything else is captured in the
/// raw layer verbatim via the full API response value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VehicleData {
    /// Numeric id (may exceed f64 precision — use id_s for display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    /// String form of `id` — use for display and storage to avoid precision loss.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_s: Option<String>,
    /// Internal vehicle id used by streaming/Autopark APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_id: Option<i64>,
    /// VIN — the canonical stable vehicle identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vin: Option<String>,
    /// User-assigned display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// "online", "asleep", "offline", etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drive_state: Option<DriveState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vehicle_state: Option<VehicleState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_state: Option<ChargeState>,
}

/// The raw row stored per vehicle per poll (full-fidelity raw layer).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PollRow {
    /// RFC3339 UTC timestamp of when Trove issued this poll.
    pub polled_at: String,
    /// Source id ("tesla").
    pub source: String,
    /// VIN — stable per vehicle; used for guid + raw identification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vin: Option<String>,
    /// guid = `{vin}#{polled_at}` (raw poll identity; Fix contract rows use gps_as_of instead).
    pub guid: String,
    /// Full parsed vehicle_data response, full fidelity.
    pub data: Value,
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for tests).

/// The API calls the pull needs.
pub(crate) trait TeslaApi {
    /// `GET /api/1/vehicles` — list vehicles (id_s, vin, display_name, state).
    fn list_vehicles(&self, token: &str) -> Result<Vec<Value>, FetchError>;
    /// `GET /api/1/vehicles/{id}/vehicle_data` — full state snapshot.
    fn vehicle_data(&self, token: &str, id: &str) -> Result<Value, FetchError>;
}

#[derive(Debug)]
pub(crate) enum FetchError {
    Unauthorized,
    /// 408/503 — vehicle is asleep or offline; not an error, just skip.
    VehicleAsleep,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::VehicleAsleep => write!(f, "vehicle asleep / offline"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

pub(crate) struct TeslaClient {
    base: String,
}

impl TeslaClient {
    pub(crate) fn new() -> Self {
        TeslaClient { base: "https://fleet-api.prd.na.vn.cloud.tesla.com/api/1".into() }
    }

    fn get(&self, url: &str, token: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            .call()
        {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            // 408 = RequestTimeout (vehicle asleep); 503 = vehicle offline.
            Err(ureq::Error::Status(408 | 503, _)) => Err(FetchError::VehicleAsleep),
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

impl TeslaApi for TeslaClient {
    fn list_vehicles(&self, token: &str) -> Result<Vec<Value>, FetchError> {
        let v = self.get(&format!("{}/vehicles", self.base), token)?;
        let arr = v
            .get("response")
            .and_then(Value::as_array)
            .ok_or_else(|| FetchError::Other("vehicles response: missing 'response' array".into()))?;
        Ok(arr.clone())
    }

    fn vehicle_data(&self, token: &str, id: &str) -> Result<Value, FetchError> {
        let v = self.get(
            &format!("{}/vehicles/{}/vehicle_data", self.base, id),
            token,
        )?;
        // The response is wrapped: {"response": {...vehicle_data...}}
        v.get("response")
            .cloned()
            .ok_or_else(|| FetchError::Other("vehicle_data: missing 'response' key".into()))
    }
}

// ---------------------------------------------------------------------------
// Token refresh helper.

pub(crate) fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(TESLA.service)?
        .or_else(|| TESLA.default_credentials())
        .context(
            "Tesla token expired and no app credentials — reconnect from the Integrations tab",
        )?;
    match oauth::refresh_token(&TESLA, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(TESLA.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(TESLA.service)?;
            anyhow::bail!(
                "Tesla token refresh failed ({e}) — reconnect from the Integrations tab"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Manual "Sync now" and periodic pass entry point.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Tesla is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = TeslaClient::new();
    pull_with(vault, &client, &token.access_token)
}

/// Testable body: accepts an injected API + token string.
pub(crate) fn pull_with(
    vault: &Vault,
    api: &impl TeslaApi,
    token: &str,
) -> Result<PullOutcome> {
    let polled_at = Utc::now().to_rfc3339();

    // 1. List vehicles.
    let vehicles = api.list_vehicles(token).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Tesla rejected the token — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Tesla /vehicles failed: {other}"),
    })?;

    if vehicles.is_empty() {
        let mut state = vault.read_tesla_sync();
        state.updated = Some(polled_at);
        vault.write_tesla_sync(&state)?;
        return Ok(PullOutcome {
            headline: "Tesla: no vehicles found on this account".to_string(),
            counts: BTreeMap::from([("vehicles", 0u64), ("snapshots", 0u64), ("fixes", 0u64)]),
        });
    }

    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let fix_stream = vault.stream(DIR, Partition::Day);

    let mut snapshots_written: u64 = 0;
    let mut fixes_written: u64 = 0;

    for vehicle in &vehicles {
        // Stable vehicle id for the API call (prefer id_s; fall back to id).
        let vehicle_id = vehicle
            .get("id_s")
            .and_then(Value::as_str)
            .or_else(|| vehicle.get("id").and_then(Value::as_str))
            .map(str::to_string)
            .or_else(|| vehicle.get("id").and_then(Value::as_i64).map(|i| i.to_string()))
            .unwrap_or_default();
        if vehicle_id.is_empty() {
            continue;
        }

        // Fetch vehicle_data (non-fatal for asleep/offline).
        let data = match api.vehicle_data(token, &vehicle_id) {
            Ok(d) => d,
            Err(FetchError::Unauthorized) => {
                return Err(anyhow::anyhow!(
                    "Tesla rejected the token — reconnect from the Integrations tab"
                ));
            }
            Err(FetchError::VehicleAsleep) => {
                // Parked/sleeping vehicle is normal; skip silently.
                continue;
            }
            Err(e) => {
                eprintln!("tesla: vehicle_data failed for {vehicle_id}: {e}");
                continue;
            }
        };

        // Parse the typed sub-objects (tolerant — unknown fields are ignored).
        let parsed: VehicleData = serde_json::from_value(data.clone()).unwrap_or(VehicleData {
            id: None,
            id_s: None,
            vehicle_id: None,
            vin: None,
            display_name: None,
            state: None,
            drive_state: None,
            vehicle_state: None,
            charge_state: None,
        });

        let vin = parsed.vin.clone();

        // Raw poll guid: VIN (or vehicle_id) + poll instant — identifies this poll record.
        let guid_base = vin.as_deref().unwrap_or(&vehicle_id);
        let guid = format!("{guid_base}#{polled_at}");

        // --- Raw layer (unconditional, full fidelity) ----------------------
        let row = PollRow {
            polled_at: polled_at.clone(),
            source: "tesla".to_string(),
            vin: vin.clone(),
            guid: guid.clone(),
            data: data.clone(),
        };
        raw_stream.append(&[row], |r| r.polled_at.as_str())?;
        snapshots_written += 1;

        // --- Contract layer (Fix row when location is available) -----------
        if let Some(drive) = &parsed.drive_state {
            if let (Some(lat), Some(lon)) = (drive.latitude, drive.longitude) {
                // Use gps_as_of (epoch seconds) as the fix timestamp; fall back
                // to local wall-clock time (local so day-partitioning is correct).
                let gps_as_of_local: Option<String> = drive.gps_as_of.and_then(|secs| {
                    Utc.timestamp_opt(secs, 0)
                        .single()
                        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
                });
                let ts = gps_as_of_local
                    .clone()
                    .unwrap_or_else(|| Local::now().to_rfc3339());

                // Fix guid uses the GPS fix identity (gps_as_of), NOT the poll
                // instant — so re-polling a stationary vehicle yields a stable
                // guid and deduplication works correctly.  Fall back to polled_at
                // only when gps_as_of is absent.
                let fix_guid_ts = gps_as_of_local
                    .as_deref()
                    .unwrap_or(polled_at.as_str());
                let fix_guid = format!("{guid_base}#{fix_guid_ts}");

                let mut fix = Fix::new("tesla", &ts, lat, lon);
                fix.guid = fix_guid;
                if let Some(h) = drive.heading {
                    fix.heading = Some(h as f64);
                }
                // speed in mph from the API; stored in extra for fidelity.
                // (The Fix contract field is m/s — Tesla doesn't give m/s
                //  directly; store mph in extra rather than converting silently.)

                // Build extra from all Tesla-specific fields.
                let mut extra = Map::new();
                if let Some(s) = drive.speed {
                    extra.insert("speed_mph".into(), s.into());
                }
                if let Some(ss) = &drive.shift_state {
                    extra.insert("shift_state".into(), ss.clone().into());
                }
                if let Some(v) = &vin {
                    extra.insert("vin".into(), v.clone().into());
                }
                if let Some(name) = &parsed.display_name {
                    extra.insert("display_name".into(), name.clone().into());
                }
                if let Some(vs) = &parsed.vehicle_state {
                    if let Some(odo) = vs.odometer {
                        extra.insert("odometer_mi".into(), odo.into());
                    }
                }
                if let Some(cs) = &parsed.charge_state {
                    if let Some(batt) = cs.battery_level {
                        extra.insert("battery_level_pct".into(), batt.into());
                    }
                    if let Some(range) = cs.battery_range {
                        extra.insert("battery_range_mi".into(), range.into());
                    }
                    if let Some(chg) = &cs.charging_state {
                        extra.insert("charging_state".into(), chg.clone().into());
                    }
                }
                fix.extra = extra;

                fix_stream.append(&[fix], |f| f.ts.as_str())?;
                fixes_written += 1;
            }
        }
    }

    let mut state = vault.read_tesla_sync();
    state.updated = Some(polled_at);
    vault.write_tesla_sync(&state)?;

    let headline = if snapshots_written == 0 {
        "Tesla: all vehicles asleep or offline — nothing recorded".to_string()
    } else {
        format!(
            "Tesla synced — {snapshots_written} snapshot(s) across {} vehicle(s); \
             {fixes_written} location fix(es) written",
            vehicles.len()
        )
    };

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("vehicles", vehicles.len() as u64),
            ("snapshots", snapshots_written),
            ("fixes", fixes_written),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Fixtures — from tesla-api.timdorr.com documentation (community-maintained
    // docs for the same vehicle_data endpoint Tesla Fleet API inherits).

    /// A full vehicle_data fixture with all typed sub-objects populated.
    /// drive_state fields confirmed: latitude, longitude, heading, speed,
    /// gps_as_of, timestamp, shift_state. vehicle_state: odometer.
    /// charge_state: battery_level, battery_range, charging_state.
    fn vehicle_data_fixture() -> Value {
        json!({
            "response": {
                "id": 12345678901234567i64,
                "id_s": "12345678901234567",
                "vehicle_id": 9876543210i64,
                "vin": "5YJ3E1EA1PF000001",
                "display_name": "My Model 3",
                "state": "online",
                "drive_state": {
                    "latitude": 37.4292,
                    "longitude": -122.1381,
                    "heading": 8,
                    "speed": null,
                    "gps_as_of": 1718031243i64,
                    "timestamp": 1718031244000i64,
                    "shift_state": null
                },
                "vehicle_state": {
                    "odometer": 12345.678
                },
                "charge_state": {
                    "battery_level": 80,
                    "battery_range": 225.43,
                    "charging_state": "Stopped"
                }
            }
        })
    }

    /// A vehicle_data response where drive_state has no location (offline GPS).
    fn vehicle_data_no_location() -> Value {
        json!({
            "response": {
                "id_s": "99999999999999999",
                "vin": "5YJ3E1EA1PF000002",
                "display_name": "Parked",
                "state": "online",
                "drive_state": {
                    "latitude": null,
                    "longitude": null,
                    "gps_as_of": null
                },
                "vehicle_state": { "odometer": 5000.0 },
                "charge_state": { "battery_level": 50, "charging_state": "Complete" }
            }
        })
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        vehicles: Result<Vec<Value>, String>,
        data: BTreeMap<String, Result<Value, FetchError>>,
    }

    impl MockApi {
        fn ok() -> Self {
            let vid = "12345678901234567";
            let full = vehicle_data_fixture()
                .get("response")
                .cloned()
                .unwrap();
            let mut data = BTreeMap::new();
            data.insert(vid.to_string(), Ok(full));
            MockApi {
                vehicles: Ok(vec![json!({"id_s": vid, "vin": "5YJ3E1EA1PF000001"})]),
                data,
            }
        }

        fn no_location() -> Self {
            let vid = "99999999999999999";
            let full = vehicle_data_no_location()
                .get("response")
                .cloned()
                .unwrap();
            let mut data = BTreeMap::new();
            data.insert(vid.to_string(), Ok(full));
            MockApi {
                vehicles: Ok(vec![json!({"id_s": vid, "vin": "5YJ3E1EA1PF000002"})]),
                data,
            }
        }

        fn asleep() -> Self {
            let vid = "12345678901234567";
            let mut data = BTreeMap::new();
            data.insert(vid.to_string(), Err(FetchError::VehicleAsleep));
            MockApi {
                vehicles: Ok(vec![json!({"id_s": vid})]),
                data,
            }
        }

        fn no_vehicles() -> Self {
            MockApi { vehicles: Ok(vec![]), data: BTreeMap::new() }
        }

        fn unauthorized() -> Self {
            MockApi { vehicles: Err("unauth".into()), data: BTreeMap::new() }
        }
    }

    impl TeslaApi for MockApi {
        fn list_vehicles(&self, _token: &str) -> Result<Vec<Value>, FetchError> {
            self.vehicles
                .as_ref()
                .map(|v| v.clone())
                .map_err(|_| FetchError::Unauthorized)
        }

        fn vehicle_data(&self, _token: &str, id: &str) -> Result<Value, FetchError> {
            self.data
                .get(id)
                .map(|r| r.as_ref().map(|v| v.clone()).map_err(|e| match e {
                    FetchError::VehicleAsleep => FetchError::VehicleAsleep,
                    FetchError::Unauthorized => FetchError::Unauthorized,
                    FetchError::Other(m) => FetchError::Other(m.clone()),
                }))
                .unwrap_or(Err(FetchError::Other(format!("no fixture for {id}"))))
        }
    }

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-tesla-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixture parse tests — confirm exact field names against documented shape.

    #[test]
    fn vehicle_data_parses_drive_state_fields() {
        let fixture = vehicle_data_fixture();
        let response = fixture.get("response").unwrap().clone();
        let parsed: VehicleData = serde_json::from_value(response).unwrap();
        let drive = parsed.drive_state.unwrap();
        assert!((drive.latitude.unwrap() - 37.4292).abs() < 1e-6, "latitude");
        assert!((drive.longitude.unwrap() - -122.1381).abs() < 1e-6, "longitude");
        assert_eq!(drive.heading, Some(8), "heading");
        assert_eq!(drive.speed, None, "speed null -> None");
        assert_eq!(drive.gps_as_of, Some(1718031243), "gps_as_of (epoch seconds)");
        assert_eq!(drive.shift_state, None, "shift_state null -> None");
    }

    #[test]
    fn vehicle_data_parses_vehicle_state_and_charge_state() {
        let fixture = vehicle_data_fixture();
        let response = fixture.get("response").unwrap().clone();
        let parsed: VehicleData = serde_json::from_value(response).unwrap();
        let vs = parsed.vehicle_state.unwrap();
        assert!((vs.odometer.unwrap() - 12345.678).abs() < 0.001, "odometer in miles");
        let cs = parsed.charge_state.unwrap();
        assert_eq!(cs.battery_level, Some(80), "battery_level percent");
        assert!((cs.battery_range.unwrap() - 225.43).abs() < 0.01, "battery_range miles");
        assert_eq!(cs.charging_state.as_deref(), Some("Stopped"), "charging_state");
    }

    #[test]
    fn vehicle_data_parses_top_level_id_and_vin() {
        let fixture = vehicle_data_fixture();
        let response = fixture.get("response").unwrap().clone();
        let parsed: VehicleData = serde_json::from_value(response).unwrap();
        assert_eq!(parsed.id_s.as_deref(), Some("12345678901234567"), "id_s string");
        assert_eq!(parsed.vin.as_deref(), Some("5YJ3E1EA1PF000001"), "vin");
        assert_eq!(parsed.display_name.as_deref(), Some("My Model 3"));
        assert_eq!(parsed.state.as_deref(), Some("online"));
    }

    #[test]
    fn poll_row_round_trips_with_full_fidelity_data() {
        let fixture = vehicle_data_fixture()
            .get("response")
            .cloned()
            .unwrap();
        let row = PollRow {
            polled_at: "2026-06-16T12:00:00+00:00".to_string(),
            source: "tesla".to_string(),
            vin: Some("5YJ3E1EA1PF000001".to_string()),
            guid: "5YJ3E1EA1PF000001#2026-06-16T12:00:00+00:00".to_string(),
            data: fixture.clone(),
        };
        let s = serde_json::to_string(&row).unwrap();
        // Full fidelity: the nested data should survive the round-trip.
        let back: PollRow = serde_json::from_str(&s).unwrap();
        assert_eq!(back.vin.as_deref(), Some("5YJ3E1EA1PF000001"));
        assert_eq!(back.source, "tesla");
        assert!(back.data.get("drive_state").is_some(), "nested data preserved");
    }

    #[test]
    fn sync_state_back_compat() {
        let s: SyncState = serde_json::from_str("{}").unwrap();
        assert!(s.updated.is_none(), "empty cursor → all-None");
        let fwd: SyncState =
            serde_json::from_str(r#"{"updated":"2026-06-16T12:00:00+00:00","future_field":"x"}"#)
                .unwrap();
        assert!(fwd.updated.is_some(), "forward-compat: extra field tolerated");
    }

    // -----------------------------------------------------------------------
    // Pull integration tests.

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn no_vehicles_is_a_clean_noop() {
        let v = temp_vault("novehicles");
        let out = pull_with(&v, &MockApi::no_vehicles(), "tok").unwrap();
        assert_eq!(out.counts["vehicles"], 0);
        assert_eq!(out.counts["snapshots"], 0);
        assert_eq!(out.counts["fixes"], 0);
        assert!(v.read_tesla_sync().updated.is_some(), "cursor updated even on noop");
    }

    #[test]
    fn unauthorized_list_vehicles_is_a_reconnect_error() {
        let v = temp_vault("unauth");
        let err = pull_with(&v, &MockApi::unauthorized(), "tok")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reconnect"), "reconnect message: {err}");
    }

    #[test]
    fn asleep_vehicle_skipped_silently() {
        let v = temp_vault("asleep");
        let out = pull_with(&v, &MockApi::asleep(), "tok").unwrap();
        // Vehicle asleep: no raw or fix rows — silent skip.
        assert_eq!(out.counts["snapshots"], 0, "asleep vehicle: no snapshot");
        assert_eq!(out.counts["fixes"], 0, "asleep vehicle: no fix");
        assert!(out.headline.contains("asleep") || out.headline.contains("offline"),
            "headline notes sleep: {}", out.headline);
    }

    #[test]
    fn full_snapshot_writes_raw_and_fix() {
        let v = temp_vault("fullsnap");
        let out = pull_with(&v, &MockApi::ok(), "tok").unwrap();
        assert_eq!(out.counts["vehicles"], 1);
        assert_eq!(out.counts["snapshots"], 1, "one raw row written");
        assert_eq!(out.counts["fixes"], 1, "one Fix written");

        // Raw layer: one JSONL file under location/tesla/raw/.
        let raw_dir = v.root().join("location/tesla/raw");
        assert!(raw_dir.exists(), "raw dir created");
        let raw_files: Vec<_> = std::fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(raw_files.len(), 1, "one raw partition file");
        let raw_content = std::fs::read_to_string(&raw_files[0].path()).unwrap();
        let raw_row: PollRow = serde_json::from_str(raw_content.trim()).unwrap();
        assert_eq!(raw_row.source, "tesla");
        assert_eq!(raw_row.vin.as_deref(), Some("5YJ3E1EA1PF000001"));
        // Full fidelity: drive_state present in raw data.
        assert!(raw_row.data.get("drive_state").is_some(), "raw data has drive_state");

        // Contract layer: one Fix file under location/tesla/.
        let fix_files: Vec<_> = std::fs::read_dir(v.root().join("location/tesla"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert_eq!(fix_files.len(), 1, "one Fix day-partition");
        let fix_content = std::fs::read_to_string(&fix_files[0].path()).unwrap();
        let fix: Fix = serde_json::from_str(fix_content.trim()).unwrap();
        assert_eq!(fix.source, "tesla");
        assert!((fix.lat - 37.4292).abs() < 1e-6, "lat");
        assert!((fix.lon - -122.1381).abs() < 1e-6, "lon");
        assert_eq!(fix.heading, Some(8.0), "heading");
        assert!(fix.guid.contains("5YJ3E1EA1PF000001"), "guid contains VIN");
        // Tesla-specific fields ride in extra.
        assert!(fix.extra.contains_key("odometer_mi"), "odometer in extra");
        assert!(fix.extra.contains_key("battery_level_pct"), "battery in extra");
        assert!(fix.extra.contains_key("charging_state"), "charging_state in extra");
        assert_eq!(fix.extra["charging_state"].as_str(), Some("Stopped"));
        assert_eq!(fix.extra["vin"].as_str(), Some("5YJ3E1EA1PF000001"));
    }

    #[test]
    fn vehicle_with_no_gps_writes_raw_only() {
        let v = temp_vault("nogps");
        let out = pull_with(&v, &MockApi::no_location(), "tok").unwrap();
        assert_eq!(out.counts["snapshots"], 1, "raw row written even without location");
        assert_eq!(out.counts["fixes"], 0, "no Fix without GPS coordinates");

        // Raw dir has a file; contract layer has no jsonl files.
        assert!(v.root().join("location/tesla/raw").exists(), "raw dir created");
        let fix_files: Vec<_> = std::fs::read_dir(v.root().join("location/tesla"))
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .collect();
        assert!(fix_files.is_empty(), "no contract day-files without GPS");
    }

    #[test]
    fn cursor_updated_after_successful_pull() {
        let v = temp_vault("cursor");
        assert!(v.read_tesla_sync().updated.is_none(), "cursor empty before pull");
        pull_with(&v, &MockApi::ok(), "tok").unwrap();
        assert!(v.read_tesla_sync().updated.is_some(), "cursor set after pull");
    }

    // -----------------------------------------------------------------------
    // Connection / def tests.

    #[test]
    fn connection_uses_assigned_port_38694() {
        assert_eq!(TESLA.redirect_port, 38694, "assigned production port");
        assert_eq!(TESLA.redirect_uri(), "http://localhost:38694/callback");
        assert_eq!(CONNECTION.id, "tesla");
        assert_eq!(DEF.connection, Some("tesla"));
    }

    #[test]
    fn def_is_default_off_toggleable_periodic() {
        assert!(!DEF.default_on, "location trail requires explicit opt-in");
        assert!(DEF.toggleable);
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert!(DEF.pull.is_some());
        assert_eq!(DEF.meta.domain, "location");
    }

    #[test]
    fn status_with_no_token_has_no_accounts() {
        let v = temp_vault("nostatus");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
    }

    #[test]
    fn status_with_live_token_shows_one_account() {
        let v = temp_vault("livestatus");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: Some("ref".into()),
                token_type: Some("Bearer".into()),
                scope: Some("vehicle_location vehicle_state openid email offline_access".into()),
                expires_at: Some(1_900_000_000),
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "tesla");
        assert!(!a.needs_reconnect, "not expired");
    }

    #[test]
    fn expired_token_with_refresh_does_not_need_reconnect() {
        let v = temp_vault("exprefresh");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: Some("ref".into()),
                token_type: None,
                scope: None,
                expires_at: Some(1_000),
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect, "refresh token available → no reconnect");
    }

    #[test]
    fn disconnect_clears_token() {
        let v = temp_vault("disconnect");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();
        def_disconnect(&v, "tesla").unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty(), "token cleared after disconnect");
    }

    #[test]
    fn fix_extra_fields_round_trip() {
        // Confirm that extra fields (odometer_mi etc.) survive a Fix serialization
        // round-trip — used by the contract write seam.
        let mut fix = Fix::new("tesla", "2026-06-16T12:00:00+00:00", 37.4292, -122.1381);
        fix.guid = "5YJ3E1EA1PF000001#2026-06-16T12:00:00+00:00".into();
        fix.heading = Some(8.0);
        let mut extra = Map::new();
        extra.insert("odometer_mi".into(), json!(12345.678));
        extra.insert("battery_level_pct".into(), json!(80));
        extra.insert("charging_state".into(), json!("Stopped"));
        extra.insert("vin".into(), json!("5YJ3E1EA1PF000001"));
        fix.extra = extra;

        let s = serde_json::to_string(&fix).unwrap();
        let back: Fix = serde_json::from_str(&s).unwrap();
        assert!((back.lat - 37.4292).abs() < 1e-6);
        assert_eq!(back.heading, Some(8.0));
        assert_eq!(back.extra["odometer_mi"], json!(12345.678));
        assert_eq!(back.extra["charging_state"], json!("Stopped"));
        assert_eq!(back.extra["vin"], json!("5YJ3E1EA1PF000001"));
    }
}
