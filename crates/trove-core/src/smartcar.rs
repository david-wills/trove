//! Smartcar — multi-brand connected-car API for location snapshots and odometer.
//!
//! Smartcar normalises 40+ OEMs (Tesla, Ford, GM, BMW, Toyota, Hyundai/Kia,
//! VW, Mercedes-Benz, Stellantis, DS) behind one OAuth + REST surface. For
//! Trove it is a **polled mileage/location time series**: the API returns only
//! the latest known location and odometer (no trip history or trace replay).
//! Each hourly poll appends one row per vehicle, building a personal mileage
//! and location log over time.
//!
//! ## Auth
//!
//! OAuth 2.0 Authorization Code flow at `connect.smartcar.com`; token exchange
//! at `auth.smartcar.com/oauth/token`. Smartcar issues refresh tokens alongside
//! access tokens (new refresh token on every refresh call — persist both).
//! Scopes: `read_vehicle_info read_location read_odometer`.
//! App credentials baked at build time (`TROVE_SMARTCAR_CLIENT_ID` /
//! `TROVE_SMARTCAR_CLIENT_SECRET`); BYO credentials are supported per
//! ConnectSpec. Assigned production redirect port: 38691.
//!
//! ## Vault layout
//!
//! Raw only (location contract is trails-shaped; Smartcar yields point
//! snapshots — the contract designer decides at Phase 3 whether polled points
//! fold in or stay a sibling stream):
//!
//! - `location/smartcar/raw/YYYY-MM.jsonl` — one row per poll (per vehicle)
//!
//! ## Privacy
//!
//! Vehicle location is a location trail — this integration ships `default_on:
//! false` and requires explicit opt-in.
//!
//! Brief: docs/integrations/smartcar.md

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "smartcar";
const SYNC_FILE: &str = ".trove/smartcar-sync.json";
const RAW_DIR: &str = "location/smartcar/raw";
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Hourly — Smartcar returns only the current snapshot; each poll is one point.
pub const SMARTCAR_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// OAuth provider.

pub static SMARTCAR: Provider = Provider {
    service: SERVICE,
    display_name: "Smartcar",
    // User-facing consent: connect.smartcar.com
    auth_url: "https://connect.smartcar.com/oauth/authorize",
    // Token exchange and refresh.
    token_url: "https://auth.smartcar.com/oauth/token",
    // Permissions needed: vehicle info (make/model/year), location, odometer.
    scopes: "read_vehicle_info read_location read_odometer",
    // Assigned unique production port for Smartcar.
    redirect_port: 38691,
    use_pkce: false,
    // Smartcar uses HTTP Basic auth on token requests.
    basic_auth: true,
    default_client_id: option_env!("TROVE_SMARTCAR_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_SMARTCAR_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(SMARTCAR.service)?.is_some()
        || SMARTCAR.default_credentials().is_some();
    let accounts = match vault.load_sync_token(SMARTCAR.service)? {
        Some(token) => vec![ConnectedAccount {
            key: SMARTCAR.service.to_string(),
            label: SMARTCAR.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Smartcar issues refresh tokens — expiry with a refresh token
            // does NOT require reconnect (the pull refreshes silently).
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
/// one `&crate::smartcar::CONNECTION,` line).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "smartcar",
    display_name: "Smartcar",
    methods: &[ConnectMethod::OAuth {
        provider: &SMARTCAR,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["smartcar"],
    setup: &[
        "Sign in at dashboard.smartcar.com and create an application.",
        "Add http://localhost:38691/callback as an allowed redirect URI — it must match exactly.",
        "Paste the Client ID and Client Secret here. They're saved, so every future connect is just a login.",
        "After connecting, Smartcar will prompt you to link your vehicle(s) via your automaker's login.",
    ],
};

/// Interactive connect: opens the consent page, waits for the redirect, saves
/// the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(SMARTCAR.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(SMARTCAR.service)?
            .or_else(|| SMARTCAR.default_credentials())
            .context(
                "no Smartcar app credentials — register an app at dashboard.smartcar.com and \
                 enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&SMARTCAR, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(SMARTCAR.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face (DEF).

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(total > 0, || {
                format!("smartcar synced — {total} snapshot(s) recorded")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("smartcar sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (stub already there —
/// do NOT add another `pub mod` or INTEGRATIONS line).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "smartcar",
        name: "Smartcar",
        kind: IntegrationKind::CloudSync,
        // Vehicle location is sensitive; require explicit opt-in.
        default_on: false,
        description:
            "Periodically log your vehicle's location and odometer reading via Smartcar, \
             which supports 40+ brands including Tesla, Ford, BMW, Toyota, GM, and VW. \
             Builds a mileage and location time series over time.",
        domain: "location",
        vault_path: "location/smartcar/",
        toggleable: true,
        setup: &[
            "Smartcar collects your vehicle's GPS location and odometer — enabling this stores \
             that data locally.",
            "Connect your Smartcar account on this card. Smartcar will walk you through linking \
             your vehicle(s) via your automaker's login.",
            "Polls hourly; each poll records the latest snapshot for each linked vehicle.",
        ],
        caveats:
            "Smartcar provides current location and odometer snapshots only — it does not store \
             trip history or GPS traces. Trips can be reconstructed at read time from the \
             accumulated snapshot series. Not all OEMs grant location or odometer scope to all \
             vehicles.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SMARTCAR_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("smartcar"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor.

/// Persisted sync state. Not a secret.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(crate) struct SyncState {
    /// RFC3339 local timestamp of the last successful poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) updated: Option<String>,
}

impl Vault {
    pub(crate) fn read_smartcar_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub(crate) fn write_smartcar_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API types (confirmed from smartcar.com official docs).

/// Response from `GET /v2.0/vehicles/{id}/location`.
/// Fields confirmed at smartcar.com/docs/api-reference/get-location.md.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LocationResponse {
    /// Latitude in degrees.
    pub latitude: f64,
    /// Longitude in degrees.
    pub longitude: f64,
}

/// Response from `GET /v2.0/vehicles/{id}/odometer`.
/// Fields confirmed at smartcar.com/docs/api-reference/get-odometer.md.
/// Units depend on the `SC-Unit-System` request header sent (we send "metric"
/// via `SC-Unit-System: metric`); the `SC-Unit-System` response header confirms
/// what was actually used. The stored `PollRow.unit_system` field records this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OdometerResponse {
    /// Odometer reading in the unit indicated by SC-Unit-System response header.
    pub distance: f64,
}

/// Deserialize `year` from either a JSON integer (`2014`) or a JSON string
/// (`"2014"`) — the Smartcar docs show a string in examples even though the
/// schema labels it integer; real accounts have returned both forms.
fn deserialize_year_flexible<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let v = Value::deserialize(deserializer)?;
    match &v {
        Value::Number(n) => n
            .as_u64()
            .map(|y| y as u32)
            .ok_or_else(|| serde::de::Error::custom(format!("year is not a valid integer: {v}"))),
        Value::String(s) => s
            .parse::<u32>()
            .map_err(|_| serde::de::Error::custom(format!("year string not parseable: {s}"))),
        _ => Err(serde::de::Error::custom(format!("year must be number or string, got: {v}"))),
    }
}

/// Response from `GET /v2.0/vehicles/{id}` (vehicle attributes).
/// Fields confirmed at smartcar.com/docs/api-reference/get-vehicle-info.md.
/// Note: `year` may arrive as a JSON number OR a JSON string (both seen in
/// docs/production) — `deserialize_year_flexible` handles both.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VehicleInfo {
    pub id: String,
    pub make: String,
    pub model: String,
    #[serde(deserialize_with = "deserialize_year_flexible")]
    pub year: u32,
}

/// The raw poll row stored per vehicle per poll.
/// Full-fidelity: carries raw API fields + derived metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PollRow {
    /// RFC3339 UTC timestamp of when Trove polled Smartcar (wall-clock
    /// collection time). Used for partitioning YYYY-MM files.
    pub ts: String,
    /// Same as `ts` — wall-clock UTC time Trove issued this poll.
    /// Absent on rows written before this field was added (back-compat).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub polled_at: String,
    /// RFC3339 timestamp from the `SC-Data-Age` response header: when the
    /// vehicle *actually recorded* the reading. For a parked car this stays
    /// constant across consecutive hourly polls. `None` if the header was
    /// absent (older API versions or test stubs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_age: Option<String>,
    /// Unit system confirmed by the `SC-Unit-System` response header
    /// (e.g. `"metric"` or `"imperial"`). `None` if the header was absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_system: Option<String>,
    pub source: String,
    /// Stable Smartcar vehicle uuid — the canonical guid component.
    pub vehicle_id: String,
    /// guid = vehicle_id + "#" + ts (stable per-vehicle per-poll).
    pub guid: String,
    /// Latitude in degrees (None if scope not granted or endpoint failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    /// Longitude in degrees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    /// Odometer reading in the unit indicated by `unit_system`
    /// (km when `unit_system` = `"metric"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odometer_km: Option<f64>,
    /// Vehicle make, model, year (from /vehicles/{id}).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    /// Full raw location response (may have additional fields Smartcar adds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_location: Option<Value>,
    /// Full raw odometer response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_odometer: Option<Value>,
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for tests).

/// Response headers returned alongside the parsed body from Smartcar endpoints.
/// `SC-Data-Age` tells when the vehicle recorded the measurement.
/// `SC-Unit-System` confirms the unit system used (metric/imperial).
#[derive(Debug, Clone, Default)]
pub(crate) struct ResponseMeta {
    /// Value of the `SC-Data-Age` response header (RFC3339 string), if present.
    pub data_age: Option<String>,
    /// Value of the `SC-Unit-System` response header, if present.
    pub unit_system: Option<String>,
}

/// The API calls the pull needs.
pub(crate) trait SmartcarApi {
    /// `GET /v2.0/vehicles` — list of vehicle ids.
    fn list_vehicles(&self, token: &str) -> Result<Vec<String>, FetchError>;
    /// `GET /v2.0/vehicles/{id}` — make/model/year.
    fn vehicle_info(&self, token: &str, id: &str) -> Result<VehicleInfo, FetchError>;
    /// `GET /v2.0/vehicles/{id}/location` — returns location + response meta.
    fn location(
        &self,
        token: &str,
        id: &str,
    ) -> Result<(LocationResponse, ResponseMeta), FetchError>;
    /// `GET /v2.0/vehicles/{id}/odometer` — returns odometer + response meta.
    fn odometer(
        &self,
        token: &str,
        id: &str,
    ) -> Result<(OdometerResponse, ResponseMeta), FetchError>;
}

/// Status-level errors.
#[derive(Debug)]
pub(crate) enum FetchError {
    Unauthorized,
    /// 501 Not Capable — OEM doesn't support this endpoint for this vehicle.
    NotCapable,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::NotCapable => write!(f, "vehicle not capable (HTTP 501)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Production HTTP client.
pub(crate) struct SmartcarClient {
    base: String,
}

impl SmartcarClient {
    pub(crate) fn new() -> Self {
        SmartcarClient { base: "https://api.smartcar.com/v2.0".into() }
    }

    #[cfg(test)]
    fn with_base(base: impl Into<String>) -> Self {
        SmartcarClient { base: base.into() }
    }

    /// Issue a GET request and return the parsed JSON body together with
    /// response meta (SC-Data-Age, SC-Unit-System). The response headers must
    /// be read *before* `into_json()` consumes the body.
    fn get(&self, url: &str, token: &str) -> Result<(Value, ResponseMeta), FetchError> {
        match ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            // Request SI units (km for odometer) using the correct header name.
            .set("SC-Unit-System", "metric")
            .call()
        {
            Ok(resp) => {
                // Read response headers BEFORE consuming the body.
                let meta = ResponseMeta {
                    data_age: resp.header("SC-Data-Age").map(str::to_string),
                    unit_system: resp.header("SC-Unit-System").map(str::to_string),
                };
                let body = resp
                    .into_json::<Value>()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                Ok((body, meta))
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            // 501 = vehicle not capable of this endpoint
            Err(ureq::Error::Status(501, _)) => Err(FetchError::NotCapable),
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

impl SmartcarApi for SmartcarClient {
    fn list_vehicles(&self, token: &str) -> Result<Vec<String>, FetchError> {
        // Pass limit=50 (Smartcar max per page) to avoid silently missing
        // vehicles on accounts with more than the default page size of 10.
        let (v, _meta) =
            self.get(&format!("{}/vehicles?limit=50", self.base), token)?;
        // Response: {"vehicles": ["uuid1", "uuid2", ...], "paging": {...}}
        let arr = v
            .get("vehicles")
            .and_then(Value::as_array)
            .ok_or_else(|| FetchError::Other("vehicles endpoint: missing 'vehicles' array".into()))?;
        Ok(arr.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
    }

    fn vehicle_info(&self, token: &str, id: &str) -> Result<VehicleInfo, FetchError> {
        let (v, _meta) = self.get(&format!("{}/vehicles/{}", self.base, id), token)?;
        serde_json::from_value(v)
            .map_err(|e| FetchError::Other(format!("parsing vehicle info: {e}")))
    }

    fn location(
        &self,
        token: &str,
        id: &str,
    ) -> Result<(LocationResponse, ResponseMeta), FetchError> {
        let (v, meta) =
            self.get(&format!("{}/vehicles/{}/location", self.base, id), token)?;
        let loc = serde_json::from_value(v)
            .map_err(|e| FetchError::Other(format!("parsing location: {e}")))?;
        Ok((loc, meta))
    }

    fn odometer(
        &self,
        token: &str,
        id: &str,
    ) -> Result<(OdometerResponse, ResponseMeta), FetchError> {
        let (v, meta) =
            self.get(&format!("{}/vehicles/{}/odometer", self.base, id), token)?;
        let odo = serde_json::from_value(v)
            .map_err(|e| FetchError::Other(format!("parsing odometer: {e}")))?;
        Ok((odo, meta))
    }
}

// ---------------------------------------------------------------------------
// Token refresh helper.

/// Ensure the token is fresh; silently refresh if expired + refresh token
/// available. Smartcar issues a NEW refresh token on every refresh — persist
/// both (handled by `save_sync_token`).
pub(crate) fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(SMARTCAR.service)?
        .or_else(|| SMARTCAR.default_credentials())
        .context(
            "Smartcar token expired and no app credentials — reconnect from the Integrations tab",
        )?;
    match oauth::refresh_token(&SMARTCAR, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(SMARTCAR.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(SMARTCAR.service)?;
            anyhow::bail!("Smartcar token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Manual "Sync now" and periodic pass entry point.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Smartcar is not connected — connect your vehicle(s) in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = SmartcarClient::new();
    pull_with(vault, &client, &token.access_token)
}

/// Testable body: accepts an injected API + token string.
pub(crate) fn pull_with(
    vault: &Vault,
    api: &impl SmartcarApi,
    token: &str,
) -> Result<PullOutcome> {
    // Wall-clock UTC time of this poll pass (used for partitioning and as the
    // stable `polled_at` field — NOT as the observation timestamp).
    let polled_at = Utc::now().to_rfc3339();

    // 1. List all connected vehicle ids.
    let vehicle_ids = api.list_vehicles(token).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Smartcar rejected the token — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Smartcar /vehicles failed: {other}"),
    })?;

    if vehicle_ids.is_empty() {
        let mut state = vault.read_smartcar_sync();
        state.updated = Some(polled_at);
        vault.write_smartcar_sync(&state)?;
        return Ok(PullOutcome {
            headline: "Smartcar is up to date — no vehicles linked".to_string(),
            counts: BTreeMap::from([("vehicles", 0u64), ("snapshots", 0u64)]),
        });
    }

    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut snapshots_written: u64 = 0;

    for vehicle_id in &vehicle_ids {
        // Fetch vehicle attributes (tolerant — skip the vehicle on hard error).
        let info = match api.vehicle_info(token, vehicle_id) {
            Ok(i) => Some(i),
            Err(FetchError::Unauthorized) => {
                return Err(anyhow::anyhow!(
                    "Smartcar rejected the token — reconnect from the Integrations tab"
                ));
            }
            Err(e) => {
                // Non-fatal: log but continue with other vehicles.
                // vehicle_info failure drops make/model/year — still record
                // location + odometer.
                eprintln!("smartcar: vehicle_info failed for {vehicle_id}: {e}");
                None
            }
        };

        // Fetch location (non-fatal: 501 = OEM doesn't support).
        let (lat, lon, raw_loc, loc_meta) = match api.location(token, vehicle_id) {
            Ok((loc, meta)) => {
                let raw = serde_json::to_value(&loc).ok();
                (Some(loc.latitude), Some(loc.longitude), raw, meta)
            }
            Err(FetchError::Unauthorized) => {
                return Err(anyhow::anyhow!(
                    "Smartcar rejected the token — reconnect from the Integrations tab"
                ));
            }
            Err(_) => (None, None, None, ResponseMeta::default()), // NotCapable or transient error
        };

        // Fetch odometer (non-fatal: 501 = OEM doesn't support).
        let (odo, raw_odo, odo_meta) = match api.odometer(token, vehicle_id) {
            Ok((o, meta)) => {
                let raw = serde_json::to_value(&o).ok();
                (Some(o.distance), raw, meta)
            }
            Err(FetchError::Unauthorized) => {
                return Err(anyhow::anyhow!(
                    "Smartcar rejected the token — reconnect from the Integrations tab"
                ));
            }
            Err(_) => (None, None, ResponseMeta::default()), // NotCapable or transient error
        };

        // Prefer the data_age from the odometer response (most canonical for
        // a mileage series); fall back to location's data_age.
        let data_age = odo_meta.data_age.or(loc_meta.data_age);
        // Prefer the unit_system from the odometer response (where it matters
        // most for the stored km label).
        let unit_system = odo_meta.unit_system.or(loc_meta.unit_system);

        // Build guid: vehicle_id + "#" + polled_at (stable & unique per poll).
        let guid = format!("{vehicle_id}#{polled_at}");

        let row = PollRow {
            ts: polled_at.clone(),
            polled_at: polled_at.clone(),
            data_age,
            unit_system,
            source: "smartcar".to_string(),
            vehicle_id: vehicle_id.clone(),
            guid,
            latitude: lat,
            longitude: lon,
            odometer_km: odo,
            make: info.as_ref().map(|i| i.make.clone()),
            model: info.as_ref().map(|i| i.model.clone()),
            year: info.as_ref().map(|i| i.year),
            raw_location: raw_loc,
            raw_odometer: raw_odo,
        };

        stream.append(&[row], |r| r.ts.as_str())?;
        snapshots_written += 1;
    }

    let mut state = vault.read_smartcar_sync();
    state.updated = Some(polled_at);
    vault.write_smartcar_sync(&state)?;

    let headline = if snapshots_written == 0 {
        "Smartcar: no data recorded (no vehicles or all endpoints failed)".to_string()
    } else {
        format!(
            "Smartcar synced — {snapshots_written} snapshot(s) across {} vehicle(s)",
            vehicle_ids.len()
        )
    };

    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("vehicles", vehicle_ids.len() as u64),
            ("snapshots", snapshots_written),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- Fixtures from official Smartcar API docs ----------------------------

    /// Official example from smartcar.com/docs/api-reference/user-vehicles.md
    fn vehicles_fixture() -> Vec<String> {
        vec!["36ab27d0-fd9d-4455-823a-ce30af709ffc".to_string()]
    }

    /// Official example from smartcar.com/docs/api-reference/get-vehicle-info.md
    fn vehicle_info_fixture() -> VehicleInfo {
        VehicleInfo {
            id: "36ab27d0-fd9d-4455-823a-ce30af709ffc".to_string(),
            make: "TESLA".to_string(),
            model: "Model S".to_string(),
            year: 2014,
        }
    }

    /// Official example from smartcar.com/docs/api-reference/get-location.md
    fn location_fixture() -> LocationResponse {
        LocationResponse { latitude: 37.4292, longitude: 122.1381 }
    }

    /// Official example from smartcar.com/docs/api-reference/get-odometer.md
    fn odometer_fixture() -> OdometerResponse {
        OdometerResponse { distance: 104.32 }
    }

    // -- Mock API -----------------------------------------------------------

    struct MockApi {
        vehicles: Result<Vec<String>, String>,
        infos: BTreeMap<String, Result<VehicleInfo, String>>,
        locations: BTreeMap<String, Result<LocationResponse, String>>,
        odometers: BTreeMap<String, Result<OdometerResponse, String>>,
        /// Optional SC-Data-Age header to inject into location/odometer responses.
        mock_data_age: Option<String>,
        /// Optional SC-Unit-System header to inject into location/odometer responses.
        mock_unit_system: Option<String>,
    }

    impl MockApi {
        fn ok() -> Self {
            let vid = "36ab27d0-fd9d-4455-823a-ce30af709ffc".to_string();
            let mut infos = BTreeMap::new();
            let mut locs = BTreeMap::new();
            let mut odos = BTreeMap::new();
            infos.insert(vid.clone(), Ok(vehicle_info_fixture()));
            locs.insert(vid.clone(), Ok(location_fixture()));
            odos.insert(vid.clone(), Ok(odometer_fixture()));
            MockApi {
                vehicles: Ok(vehicles_fixture()),
                infos,
                locations: locs,
                odometers: odos,
                mock_data_age: None,
                mock_unit_system: None,
            }
        }

        fn ok_with_meta(data_age: &str, unit_system: &str) -> Self {
            let mut m = Self::ok();
            m.mock_data_age = Some(data_age.to_string());
            m.mock_unit_system = Some(unit_system.to_string());
            m
        }

        fn empty_vehicles() -> Self {
            MockApi {
                vehicles: Ok(vec![]),
                infos: BTreeMap::new(),
                locations: BTreeMap::new(),
                odometers: BTreeMap::new(),
                mock_data_age: None,
                mock_unit_system: None,
            }
        }

        fn unauthorized_vehicles() -> Self {
            MockApi {
                vehicles: Err("unauthorized".into()),
                infos: BTreeMap::new(),
                locations: BTreeMap::new(),
                odometers: BTreeMap::new(),
                mock_data_age: None,
                mock_unit_system: None,
            }
        }

        fn make_meta(&self) -> ResponseMeta {
            ResponseMeta {
                data_age: self.mock_data_age.clone(),
                unit_system: self.mock_unit_system.clone(),
            }
        }
    }

    impl SmartcarApi for MockApi {
        fn list_vehicles(&self, _token: &str) -> Result<Vec<String>, FetchError> {
            self.vehicles
                .as_ref()
                .map(|v| v.clone())
                .map_err(|_| FetchError::Unauthorized)
        }

        fn vehicle_info(&self, _token: &str, id: &str) -> Result<VehicleInfo, FetchError> {
            self.infos
                .get(id)
                .map(|r| {
                    r.as_ref()
                        .map(|v| v.clone())
                        .map_err(|_| FetchError::Other("info error".into()))
                })
                .unwrap_or(Err(FetchError::Other(format!("no fixture for {id}"))))
        }

        fn location(
            &self,
            _token: &str,
            id: &str,
        ) -> Result<(LocationResponse, ResponseMeta), FetchError> {
            self.locations
                .get(id)
                .map(|r| {
                    r.as_ref()
                        .map(|v| (v.clone(), self.make_meta()))
                        .map_err(|_| FetchError::Other("loc error".into()))
                })
                .unwrap_or(Err(FetchError::NotCapable))
        }

        fn odometer(
            &self,
            _token: &str,
            id: &str,
        ) -> Result<(OdometerResponse, ResponseMeta), FetchError> {
            self.odometers
                .get(id)
                .map(|r| {
                    r.as_ref()
                        .map(|v| (v.clone(), self.make_meta()))
                        .map_err(|_| FetchError::Other("odo error".into()))
                })
                .unwrap_or(Err(FetchError::NotCapable))
        }
    }

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-smartcar-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -- fixture round-trip tests -------------------------------------------

    #[test]
    fn location_response_parses_official_example() {
        let raw = json!({"latitude": 37.4292, "longitude": 122.1381});
        let loc: LocationResponse = serde_json::from_value(raw).unwrap();
        assert!((loc.latitude - 37.4292).abs() < 1e-6);
        assert!((loc.longitude - 122.1381).abs() < 1e-6);
    }

    #[test]
    fn odometer_response_parses_official_example() {
        let raw = json!({"distance": 104.32});
        let odo: OdometerResponse = serde_json::from_value(raw).unwrap();
        assert!((odo.distance - 104.32).abs() < 1e-6);
    }

    #[test]
    fn vehicle_info_parses_official_example() {
        // JSON integer form.
        let raw = json!({"id": "36ab27d0-fd9d-4455-823a-ce30af709ffc", "make": "TESLA", "model": "Model S", "year": 2014});
        let info: VehicleInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(info.make, "TESLA");
        assert_eq!(info.year, 2014);
    }

    #[test]
    fn vehicle_info_parses_year_as_string() {
        // Real Smartcar responses sometimes return year as a quoted string
        // (e.g. "\"year\": \"2014\""); both forms must round-trip.
        let raw = json!({"id": "36ab27d0-fd9d-4455-823a-ce30af709ffc", "make": "TESLA", "model": "Model S", "year": "2014"});
        let info: VehicleInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(info.year, 2014, "string year parsed to u32");

        // An unexpected type (float) should error, not silently produce garbage.
        let bad = json!({"id": "x", "make": "X", "model": "Y", "year": 2014.5});
        assert!(serde_json::from_value::<VehicleInfo>(bad).is_err());
    }

    #[test]
    fn poll_row_serialises_and_back_compat() {
        // Confirm skip_serializing_if=None fields round-trip gracefully.
        let row = PollRow {
            ts: "2026-06-16T12:00:00+00:00".to_string(),
            polled_at: "2026-06-16T12:00:00+00:00".to_string(),
            data_age: Some("2026-06-16T10:00:00Z".to_string()),
            unit_system: Some("metric".to_string()),
            source: "smartcar".to_string(),
            vehicle_id: "36ab27d0-fd9d-4455-823a-ce30af709ffc".to_string(),
            guid: "36ab27d0-fd9d-4455-823a-ce30af709ffc#2026-06-16T12:00:00+00:00".to_string(),
            latitude: Some(37.4292),
            longitude: Some(122.1381),
            odometer_km: Some(104.32),
            make: Some("TESLA".into()),
            model: Some("Model S".into()),
            year: Some(2014),
            raw_location: None,
            raw_odometer: None,
        };
        let s = serde_json::to_string(&row).unwrap();
        assert!(s.contains("\"latitude\""));
        assert!(s.contains("\"odometer_km\""));
        assert!(s.contains("\"data_age\""));
        assert!(s.contains("\"unit_system\""));
        // None fields absent (skip_serializing_if).
        assert!(!s.contains("\"raw_location\""));
        assert!(!s.contains("\"raw_odometer\""));

        // A sparse legacy row (no data_age/unit_system/polled_at) still deserializes
        // — back-compat with rows written before this fix.
        let sparse = r#"{"ts":"2026-06-16T12:00:00+00:00","source":"smartcar","vehicle_id":"abc","guid":"abc#ts","odometer_km":104.32,"future_field":"x"}"#;
        let back: PollRow = serde_json::from_str(sparse).unwrap();
        assert!(back.latitude.is_none());
        assert_eq!(back.odometer_km, Some(104.32));
        // New optional fields default to None on legacy rows.
        assert!(back.data_age.is_none());
        assert!(back.unit_system.is_none());
    }

    #[test]
    fn response_meta_data_age_and_unit_system_stored_in_poll_row() {
        // When the mock returns SC-Data-Age and SC-Unit-System headers,
        // those values should land in the stored PollRow.
        let mock = MockApi::ok_with_meta("2026-06-16T10:00:00Z", "metric");
        let v = temp_vault("meta-headers");
        pull_with(&v, &mock, "tok").unwrap();

        let raw_dir = v.root().join("location/smartcar/raw");
        let files: Vec<_> = std::fs::read_dir(&raw_dir).unwrap().flatten().collect();
        let content = std::fs::read_to_string(&files[0].path()).unwrap();
        let row: PollRow = serde_json::from_str(content.trim()).unwrap();

        assert_eq!(row.data_age.as_deref(), Some("2026-06-16T10:00:00Z"),
            "SC-Data-Age stored in data_age field");
        assert_eq!(row.unit_system.as_deref(), Some("metric"),
            "SC-Unit-System stored in unit_system field");
        // polled_at is the wall-clock poll time, distinct from data_age.
        assert!(!row.polled_at.is_empty(), "polled_at populated");
    }

    // -- pull tests ---------------------------------------------------------

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn empty_vehicle_list_is_a_clean_noop() {
        let v = temp_vault("empty");
        let out = pull_with(&v, &MockApi::empty_vehicles(), "tok").unwrap();
        assert_eq!(out.counts["vehicles"], 0);
        assert_eq!(out.counts["snapshots"], 0);
        assert!(v.read_smartcar_sync().updated.is_some());
    }

    #[test]
    fn unauthorized_list_vehicles_is_a_reconnect_error() {
        let v = temp_vault("unauth");
        let err = pull_with(&v, &MockApi::unauthorized_vehicles(), "tok")
            .unwrap_err()
            .to_string();
        assert!(err.contains("reconnect"), "reconnect message: {err}");
    }

    #[test]
    fn single_vehicle_snapshot_lands_in_raw_partition() {
        let v = temp_vault("snap");
        let out = pull_with(&v, &MockApi::ok(), "tok").unwrap();
        assert_eq!(out.counts["vehicles"], 1);
        assert_eq!(out.counts["snapshots"], 1);

        // Exactly one JSONL file exists under location/smartcar/raw/.
        let raw_dir = v.root().join("location/smartcar/raw");
        assert!(raw_dir.exists(), "raw dir created");
        let files: Vec<_> = std::fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(files.len(), 1, "one partition file");

        // Parse the row and verify all fields.
        let content = std::fs::read_to_string(&files[0].path()).unwrap();
        let row: PollRow = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(row.source, "smartcar");
        assert_eq!(row.vehicle_id, "36ab27d0-fd9d-4455-823a-ce30af709ffc");
        assert!(row.guid.contains(&row.vehicle_id));
        assert!((row.latitude.unwrap() - 37.4292).abs() < 1e-6);
        assert!((row.longitude.unwrap() - 122.1381).abs() < 1e-6);
        assert!((row.odometer_km.unwrap() - 104.32).abs() < 1e-6);
        assert_eq!(row.make.as_deref(), Some("TESLA"));
        assert_eq!(row.model.as_deref(), Some("Model S"));
        assert_eq!(row.year, Some(2014));

        // Cursor updated.
        assert!(v.read_smartcar_sync().updated.is_some());
    }

    #[test]
    fn vehicle_with_no_location_scope_still_records_odometer() {
        // Simulate a vehicle that supports odometer but not location (501).
        let vid = "36ab27d0-fd9d-4455-823a-ce30af709ffc".to_string();
        let mut mock = MockApi::ok();
        mock.locations.remove(&vid); // will fall through to NotCapable

        let v = temp_vault("nolocscope");
        let out = pull_with(&v, &mock, "tok").unwrap();
        assert_eq!(out.counts["snapshots"], 1);

        let raw_dir = v.root().join("location/smartcar/raw");
        let files: Vec<_> = std::fs::read_dir(&raw_dir).unwrap().flatten().collect();
        let content = std::fs::read_to_string(&files[0].path()).unwrap();
        let row: PollRow = serde_json::from_str(content.trim()).unwrap();
        assert!(row.latitude.is_none());
        assert!(row.longitude.is_none());
        assert!(row.odometer_km.is_some());
    }

    #[test]
    fn guid_is_stable_and_contains_vehicle_id() {
        let vid = "36ab27d0-fd9d-4455-823a-ce30af709ffc";
        let ts = "2026-06-16T12:00:00+00:00";
        let guid = format!("{vid}#{ts}");
        assert!(guid.starts_with(vid));
        assert!(guid.contains('#'));
    }

    #[test]
    fn sync_state_back_compat() {
        // Empty cursor deserializes to all-None.
        let s: SyncState = serde_json::from_str("{}").unwrap();
        assert!(s.updated.is_none());
        // Forward-compat: extra field survives.
        let fwd: SyncState =
            serde_json::from_str(r#"{"updated":"2026-06-16T12:00:00+00:00","new_field":"x"}"#)
                .unwrap();
        assert!(fwd.updated.is_some());
    }

    // -- connection / def tests ---------------------------------------------

    #[test]
    fn connection_uses_assigned_port_38691() {
        assert_eq!(SMARTCAR.redirect_port, 38691);
        assert_eq!(SMARTCAR.redirect_uri(), "http://localhost:38691/callback");
        assert_eq!(CONNECTION.id, "smartcar");
        assert_eq!(DEF.connection, Some("smartcar"));
    }

    #[test]
    fn def_is_default_off_toggleable_periodic() {
        assert!(!DEF.default_on, "location trail requires explicit opt-in");
        assert!(DEF.toggleable);
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert!(DEF.pull.is_some());
    }

    #[test]
    fn status_with_no_token_has_no_accounts() {
        let v = temp_vault("nostatus");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
    }

    #[test]
    fn status_maps_live_token_to_one_account() {
        let v = temp_vault("livetoken");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: Some("ref".into()),
                token_type: Some("Bearer".into()),
                scope: Some("read_vehicle_info read_location read_odometer".into()),
                expires_at: Some(1_900_000_000),
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        let a = &s.accounts[0];
        assert_eq!(a.key, "smartcar");
        assert_eq!(a.label, "Smartcar");
        assert_eq!(a.expires_at, Some(1_900_000_000));
        assert!(!a.needs_reconnect);
    }

    #[test]
    fn expired_token_with_refresh_does_not_need_reconnect_at_status() {
        let v = temp_vault("exp-refreshable");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: Some("ref".into()),
                token_type: None,
                scope: None,
                expires_at: Some(1_000), // past
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert!(!s.accounts[0].needs_reconnect);
    }

    #[test]
    fn expired_token_without_refresh_needs_reconnect() {
        let v = temp_vault("exp-norefresh");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "acc".into(),
                refresh_token: None,
                token_type: None,
                scope: None,
                expires_at: Some(1_000), // past
            },
        )
        .unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts[0].needs_reconnect);
    }

    #[test]
    fn disconnect_forgets_token_but_keeps_app_credentials() {
        use crate::sync::oauth::AppCredentials;
        let v = temp_vault("disconnect");
        v.save_sync_app(
            SERVICE,
            &AppCredentials { client_id: "id".into(), client_secret: Some("s".into()) },
        )
        .unwrap();
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
        def_disconnect(&v, "smartcar").unwrap();
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "app credentials survive a disconnect");
    }
}
