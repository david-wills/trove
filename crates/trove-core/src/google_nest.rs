//! Google Nest Smart Device Management API — thermostat and device state pull.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/google-nest.md.
//!
//! A **Periodic** pull (~5-min cadence) against Google's Smart Device Management
//! API (`smartdevicemanagement.googleapis.com/v1`). The SDM API is **current-state
//! only** — Trove's longitudinal value is the log it builds by polling repeatedly,
//! since the Nest app's 10-day history is not queryable.
//!
//! ## Layers
//!
//! - **Raw** — every full device object verbatim under
//!   `home/google-nest/raw/YYYY-MM.jsonl`, one line per device per poll.
//! - **Contract** — numeric thermostat traits fan out into [`HomeReading`] rows at
//!   `home/google-nest/YYYY-MM.jsonl`, one row per metric per device per snapshot.
//!   Non-numeric or absent traits go to raw only. Metrics written:
//!     - `temperature` (°C, from `sdm.devices.traits.Temperature.ambientTemperatureCelsius`)
//!     - `humidity` (%, from `sdm.devices.traits.Humidity.ambientHumidityPercent`)
//!     - `setpoint_heat` (°C, from `ThermostatTemperatureSetpoint.heatCelsius`)
//!     - `setpoint_cool` (°C, from `ThermostatTemperatureSetpoint.coolCelsius`)
//!
//! Non-numeric traits (mode, HVAC status, connectivity) ride in `extra` on every
//! numeric row so they are available at read time without a separate lookup.
//!
//! ## Auth
//!
//! Google Device Access is a **separate** OAuth program from the standard Google
//! API — it requires its own project at `console.nest.google.com` (one-time $5
//! developer fee) and its own PCM consent flow. Trove ships a `"google-nest"`
//! [`ConnectionDef`] that is **distinct** from the `"google"` connection even
//! though both use Google's OAuth endpoints. The scopes, consent screen, and PCM
//! flow differ.
//!
//! Refresh tokens should stay live as long as the periodic pull is active
//! (≥ one call every 6 months). A stale (expired) refresh token is surfaced via
//! `needs_reconnect` on the status hook.
//!
//! ## Dedup
//!
//! `guid` = `google-nest:{device_name}:{metric}:{ts_epoch_secs}` (snapshot
//! time, second resolution — the API has no sub-second timestamps). A snapshot
//! whose poll time can't be partitioned yields no rows. Re-runs within the same
//! second for the same device/metric are deduped by the guid check at write time.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

/// Service id used for the token / app-creds store.
const SERVICE: &str = "google-nest";

/// Non-secret rebuildable cursor (NOT under `.trove/sync/`).
const SYNC_FILE: &str = ".trove/google-nest-sync.json";

/// Contract-layer reading stream.
const DIR: &str = "home/google-nest";
/// Raw layer (one line per device per poll, full fidelity).
const RAW_DIR: &str = "home/google-nest/raw";

/// SDM API base URL.
const API_BASE: &str = "https://smartdevicemanagement.googleapis.com/v1";

/// Seconds between periodic syncs. ~5 min: the API is current-state only and
/// the integration's value is the longitudinal log Trove self-builds.
pub const NEST_SYNC_SECS: u64 = 300;

/// HTTP timeout for any single call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// OAuth provider — Google Device Access (SEPARATE from the "google" connection;
// different PCM consent program, different scopes).

/// Google Device Access OAuth provider. Requires a project registered at
/// `console.nest.google.com` (one-time $5 developer fee). The redirect URI
/// `http://localhost:38799/callback` must be registered in that project's OAuth
/// client.
pub static GOOGLE_NEST_PROVIDER: Provider = Provider {
    service: SERVICE,
    display_name: "Google Nest",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    // SDM API scope: read access to device traits and structures.
    scopes: "https://www.googleapis.com/auth/sdm.service",
    // Assigned unique production port for this integration.
    redirect_port: 38799,
    use_pkce: true,
    basic_auth: false,
    // Bake credentials at build time: TROVE_GOOGLE_NEST_CLIENT_ID /
    // TROVE_GOOGLE_NEST_CLIENT_SECRET.
    default_client_id: option_env!("TROVE_GOOGLE_NEST_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_GOOGLE_NEST_CLIENT_SECRET"),
    // offline → issue a refresh token; consent → re-issue on every connect.
    extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(GOOGLE_NEST_PROVIDER.service)?.is_some()
        || GOOGLE_NEST_PROVIDER.default_credentials().is_some();
    let accounts = match vault.load_sync_token(GOOGLE_NEST_PROVIDER.service)? {
        Some(token) => vec![ConnectedAccount {
            key: GOOGLE_NEST_PROVIDER.service.to_string(),
            label: GOOGLE_NEST_PROVIDER.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Surface needs_reconnect when the access token is expired AND
            // there is no refresh token to silently renew it.
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

/// Save the Device Access enterprise project ID (a non-secret UUID) into
/// the Nest sync cursor. Called from the `TokenPaste` connect method.
fn connect_project_id(vault: &Vault, raw: &str) -> Result<()> {
    let id = raw.trim();
    if id.is_empty() {
        anyhow::bail!("enterprise project ID is required — copy it from console.nest.google.com");
    }
    // Minimal sanity check: the ID should look like a non-empty slug with no
    // slashes (it is a UUID or short alphanumeric string in Google's docs).
    if id.contains('/') || id.contains(' ') {
        anyhow::bail!(
            "enterprise project ID looks wrong (got {:?}) — it should be the UUID \
             shown at the top of your Device Access project, not the full URL",
            id
        );
    }
    let mut state = vault.read_nest_sync_ext();
    state.enterprise_id = Some(id.to_string());
    vault.write_nest_sync_ext(&state)?;
    Ok(())
}

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds
/// one `&crate::google_nest::CONNECTION,` line).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "google-nest",
    display_name: "Google Nest",
    methods: &[
        ConnectMethod::OAuth {
            provider: &GOOGLE_NEST_PROVIDER,
            multi_account: false,
            run: connect_oauth,
        },
        ConnectMethod::TokenPaste {
            label: "Device Access project ID",
            help: "Open console.nest.google.com, select your project, and copy the \
                   project ID (a UUID shown at the top of the project page). This is \
                   required for Trove to call the SDM API on your behalf.",
            placeholder: "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx",
            run: connect_project_id,
        },
    ],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["google-nest"],
    setup: &[
        "Go to console.nest.google.com and create a Device Access project \
         (one-time $5 registration fee — required by Google for any third-party \
         Nest integration). Copy the project ID UUID shown at the top of the project.",
        "In your Google Cloud project linked to Device Access, create an OAuth 2.0 \
         client (type: Desktop or Web) and add \
         http://localhost:38799/callback as an authorized redirect URI.",
        "Paste the client ID and client secret here, then sign in with Google. \
         After the OAuth step completes, paste your Device Access project ID UUID \
         to finish the connection.",
    ],
};

/// Interactive OAuth connect: opens the consent page, waits for the redirect,
/// saves the token. Blocking — callers off the main thread only.
///
/// Credentials resolve: explicit → previously saved → compiled-in defaults.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(GOOGLE_NEST_PROVIDER.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(GOOGLE_NEST_PROVIDER.service)?
            .or_else(|| GOOGLE_NEST_PROVIDER.default_credentials())
            .context(
                "no Google Nest app credentials — register a Device Access project at \
                 console.nest.google.com and enter the client ID and secret in the \
                 Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&GOOGLE_NEST_PROVIDER, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(GOOGLE_NEST_PROVIDER.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face (DEF).

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("google nest synced — {n} readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "google nest sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Google Nest is up to date — no new readings".to_string()
    } else {
        format!("Google Nest synced — {n} readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-nest",
        name: "Google Nest",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Polls your Nest thermostats via Google's Smart Device Management API every 5 \
             minutes to build a local history of temperature, humidity, setpoints, and HVAC \
             state — history the Nest app itself never exposes.",
        domain: "home",
        vault_path: "home/google-nest/",
        toggleable: true,
        setup: &[
            "Register a Device Access project at console.nest.google.com (one-time $5 fee).",
            "Connect your Google account with Nest access — a separate consent from any \
             existing Google login.",
            "Polling begins every 5 minutes and builds a growing local history.",
        ],
        caveats: "Requires a separate Google Device Access registration (one-time $5 developer \
                  fee) distinct from any existing Google account connection. The SDM API returns \
                  current state only — history is built by Trove's 5-minute poll. Refresh tokens \
                  expire after 6 months of complete inactivity; the periodic pull itself keeps \
                  them alive.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(NEST_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("google-nest"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// HTTP layer.

/// A live access token, refreshed if needed before calling.
fn fresh_token(vault: &Vault) -> Result<String> {
    let mut token = vault
        .load_sync_token(SERVICE)?
        .context(
            "Google Nest is not connected — add your Device Access credentials \
             in the Integrations tab",
        )?;
    // Refresh if the access token is expired and we have a refresh token.
    if token.expired() {
        if token.refresh_token.is_some() {
            let creds = vault
                .load_sync_app(SERVICE)?
                .or_else(|| GOOGLE_NEST_PROVIDER.default_credentials())
                .context("Google Nest app credentials missing — reconnect")?;
            token = oauth::refresh_token(&GOOGLE_NEST_PROVIDER, &creds, &token)?;
            vault.save_sync_token(SERVICE, &token)?;
        } else {
            anyhow::bail!(
                "Google Nest access token expired and no refresh token is stored — \
                 reconnect from the Integrations tab"
            );
        }
    }
    Ok(token.access_token)
}

/// `GET {API_BASE}/enterprises/{enterprise_id}/devices`
/// Returns the raw JSON array of devices.
fn list_devices(enterprise_id: &str, token: &str) -> Result<Vec<Value>> {
    let url = format!("{API_BASE}/enterprises/{enterprise_id}/devices");
    let resp = ureq::get(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| {
            anyhow::anyhow!("Google Nest device list request failed: {e}")
        })?;
    let body: Value = resp.into_json().context("parsing Google Nest device list")?;
    // Response is `{"devices": [...]}`.
    let devices = body
        .get("devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    Ok(devices)
}

// ---------------------------------------------------------------------------
// Enterprise ID resolution (test utility only).
//
// The enterprise project ID is a UUID the user must supply via the
// "Device Access project ID" TokenPaste step — it is NOT API-discoverable.
// This helper parses it out of a device `name` field for test assertions.

/// Parse the enterprise project id from a device `name` field.
/// Used only in tests; not called from production code (the user-supplied
/// ID from the connect step is the authoritative source).
#[cfg(test)]
fn enterprise_id_from_device(device: &Value) -> Option<String> {
    // "name": "enterprises/XYZ123/devices/ABC"
    let name = device.get("name")?.as_str()?;
    let after_prefix = name.strip_prefix("enterprises/")?;
    let id = after_prefix.split('/').next()?;
    if id.is_empty() { None } else { Some(id.to_string()) }
}

// ---------------------------------------------------------------------------
// Mapping: device traits → HomeReading rows.

/// Top-level trait keys for the metrics we map.
const TRAIT_TEMPERATURE: &str = "sdm.devices.traits.Temperature";
const TRAIT_HUMIDITY: &str = "sdm.devices.traits.Humidity";
const TRAIT_SETPOINT: &str = "sdm.devices.traits.ThermostatTemperatureSetpoint";
const TRAIT_MODE: &str = "sdm.devices.traits.ThermostatMode";
const TRAIT_HVAC: &str = "sdm.devices.traits.ThermostatHvac";
const TRAIT_CONNECTIVITY: &str = "sdm.devices.traits.Connectivity";
const TRAIT_INFO: &str = "sdm.devices.traits.Info";

/// Extract a short display name for the device: Info trait customName → the
/// last segment of the device `name` as a fallback.
fn device_display_name(device: &Value) -> String {
    if let Some(traits) = device.get("traits").and_then(Value::as_object) {
        if let Some(custom) = traits
            .get(TRAIT_INFO)
            .and_then(|t| t.get("customName"))
            .and_then(Value::as_str)
        {
            let n = custom.trim();
            if !n.is_empty() {
                return n.to_string();
            }
        }
    }
    // Fallback: last path segment of name ("enterprises/X/devices/Y" → "Y").
    device
        .get("name")
        .and_then(Value::as_str)
        .and_then(|n| n.split('/').last())
        .unwrap_or("")
        .to_string()
}

/// Extract the room display name from parentRelations (first entry's displayName).
fn room_name(device: &Value) -> String {
    device
        .get("parentRelations")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|r| r.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Build a string capturing extra non-numeric trait fields for context.
fn extra_fields(device: &Value) -> Map<String, Value> {
    let mut extra = Map::new();
    // device_type lives at the top level of the device object, not inside
    // traits — capture it regardless of whether traits are present.
    if let Some(dtype) = device.get("type").and_then(Value::as_str) {
        extra.insert("device_type".into(), Value::String(dtype.to_string()));
    }
    if let Some(traits) = device.get("traits").and_then(Value::as_object) {
        // mode
        if let Some(mode) = traits
            .get(TRAIT_MODE)
            .and_then(|t| t.get("mode"))
            .and_then(Value::as_str)
        {
            extra.insert("thermostat_mode".into(), Value::String(mode.to_string()));
        }
        // hvac_status
        if let Some(status) = traits
            .get(TRAIT_HVAC)
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str)
        {
            extra.insert("hvac_status".into(), Value::String(status.to_string()));
        }
        // connectivity
        if let Some(conn) = traits
            .get(TRAIT_CONNECTIVITY)
            .and_then(|t| t.get("status"))
            .and_then(Value::as_str)
        {
            extra.insert("connectivity".into(), Value::String(conn.to_string()));
        }
    }
    extra
}

/// Map `(metric, value_in_celsius)` pairs out of a device's traits.
/// Only the thermostat numeric traits are mapped to contract rows; missing or
/// non-thermostat traits produce no rows.
fn trait_metrics(traits: &Value) -> Vec<(&'static str, f64)> {
    let mut out = Vec::new();
    let obj = match traits.as_object() {
        Some(o) => o,
        None => return out,
    };
    // Ambient temperature.
    if let Some(v) = obj
        .get(TRAIT_TEMPERATURE)
        .and_then(|t| t.get("ambientTemperatureCelsius"))
        .and_then(Value::as_f64)
    {
        out.push(("temperature", v));
    }
    // Ambient humidity.
    if let Some(v) = obj
        .get(TRAIT_HUMIDITY)
        .and_then(|t| t.get("ambientHumidityPercent"))
        .and_then(Value::as_f64)
    {
        out.push(("humidity", v));
    }
    // Heat setpoint.
    if let Some(v) = obj
        .get(TRAIT_SETPOINT)
        .and_then(|t| t.get("heatCelsius"))
        .and_then(Value::as_f64)
    {
        out.push(("setpoint_heat", v));
    }
    // Cool setpoint.
    if let Some(v) = obj
        .get(TRAIT_SETPOINT)
        .and_then(|t| t.get("coolCelsius"))
        .and_then(Value::as_f64)
    {
        out.push(("setpoint_cool", v));
    }
    out
}

/// Map one device snapshot into its [`HomeReading`] rows. Returns empty when
/// the device has no mappable traits or no usable `name`.
fn readings_from_device(device: &Value, poll_ts: &str) -> Vec<HomeReading> {
    let device_name = match device.get("name").and_then(Value::as_str) {
        Some(n) if !n.is_empty() => n,
        _ => return Vec::new(),
    };
    // The ts must be partitionable (drives the month file).
    if Partition::Month.key(poll_ts).is_none() {
        return Vec::new();
    }
    let epoch_secs = match DateTime::parse_from_rfc3339(poll_ts) {
        Ok(dt) => dt.timestamp(),
        Err(_) => return Vec::new(),
    };

    let traits = match device.get("traits") {
        Some(t) => t,
        None => return Vec::new(),
    };
    let metrics = trait_metrics(traits);
    if metrics.is_empty() {
        return Vec::new();
    }

    let display_name = device_display_name(device);
    let place = room_name(device);
    let extra = extra_fields(device);

    metrics
        .into_iter()
        .map(|(metric, value)| {
            let mut r = HomeReading::new("google-nest", metric, value, poll_ts.to_string());
            r.unit = if metric == "humidity" { "percent".to_string() } else { "C".to_string() };
            r.device = display_name.clone();
            r.place = place.clone();
            // Stable guid = source:device_name:metric:epoch_secs
            let mut ex = extra.clone();
            ex.insert(
                "guid".into(),
                Value::String(format!("google-nest:{device_name}:{metric}:{epoch_secs}")),
            );
            r.extra = ex;
            r
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid.

fn reading_guid(r: &HomeReading) -> String {
    r.extra
        .get("guid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Raw line: the full device object verbatim, with a `_poll_ts` tag for
/// partitioning. Only `_poll_ts` is used for month placement; the rest is the
/// source object.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Build a dedup key for a raw device snapshot: `{device_name}@{poll_ts}`.
/// This is consistent with the contract guid (which also uses device name +
/// timestamp) so that a same-second re-poll drops both layers equally.
fn raw_dedup_key(device_name: &str, poll_ts: &str) -> String {
    format!("{device_name}@{poll_ts}")
}

/// Append new contract + raw rows for one poll, deduped by guid (contract)
/// and by `{device_name}@{poll_ts}` (raw). Returns the number of new
/// contract rows written.
fn write_layers(
    vault: &Vault,
    rows: Vec<HomeReading>,
    raws: Vec<(String, Value)>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // Build the existing-guid set for contract dedup.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            if let Some(g) = v
                .get("extra")
                .and_then(|e| e.get("guid"))
                .and_then(Value::as_str)
            {
                seen.insert(g.to_string());
            }
        }
    }

    // Build the existing-raw-key set for raw dedup (device_name@poll_ts).
    let mut seen_raw: HashSet<String> = HashSet::new();
    for key in raw_stream.partitions()? {
        for v in raw_stream.read::<Value>(&key)? {
            let name = v.get("name").and_then(Value::as_str).unwrap_or("");
            let ts = v.get("_poll_ts").and_then(Value::as_str).unwrap_or("");
            if !name.is_empty() && !ts.is_empty() {
                seen_raw.insert(raw_dedup_key(name, ts));
            }
        }
    }

    let mut new_rows: Vec<HomeReading> = Vec::new();
    for row in rows {
        let g = reading_guid(&row);
        if g.is_empty() || !seen.insert(g) {
            continue;
        }
        new_rows.push(row);
    }

    let mut new_raws: Vec<RawLine> = Vec::new();
    for (ts, value) in raws {
        let name = value.get("name").and_then(Value::as_str).unwrap_or("");
        let key = raw_dedup_key(name, &ts);
        if seen_raw.insert(key) {
            new_raws.push(RawLine { ts, value });
        }
    }

    contract.append(&new_rows, |r| &r.ts)?;
    raw_stream.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Persisted non-secret cursor. Stores the enterprise project id (derived
/// from the first device list response's `name` field) so subsequent polls
/// don't need to re-discover it. Also records the last successful poll time.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ExtendedSyncState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enterprise_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_nest_sync_ext(&self) -> ExtendedSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_nest_sync_ext(&self, state: &ExtendedSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

/// Standalone inner pull, injected-API seam for tests.
fn pull_devices(
    vault: &Vault,
    poll_ts: &str,
    devices: &[Value],
    enterprise_id: &str,
) -> Result<u64> {
    let mut all_rows: Vec<HomeReading> = Vec::new();
    let mut raws: Vec<(String, Value)> = Vec::new();

    for device in devices {
        // Raw line: tag the device object with poll_ts for month partitioning.
        let mut raw_obj = device.clone();
        if let Value::Object(ref mut m) = raw_obj {
            m.insert("_poll_ts".into(), Value::String(poll_ts.to_string()));
        }
        raws.push((poll_ts.to_string(), raw_obj));

        let rows = readings_from_device(device, poll_ts);
        all_rows.extend(rows);
    }

    // Raw device objects verbatim.
    let _ = enterprise_id; // used for context; device names are full paths already.
    write_layers(vault, all_rows, raws)
}

/// Resolve credentials, fetch devices, persist raw + contract.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = fresh_token(vault)?;
    let poll_ts = Local::now().to_rfc3339();

    let mut state = vault.read_nest_sync_ext();

    // The Device Access enterprise project ID is a mandatory UUID assigned by
    // Google at console.nest.google.com — it is NOT API-discoverable. The user
    // must supply it via the "Device Access project ID" TokenPaste step during
    // connect. Without it, every SDM API call would fail.
    let enterprise_id = match &state.enterprise_id {
        Some(e) => e.clone(),
        None => {
            anyhow::bail!(
                "Google Nest enterprise project ID not configured — \
                 open the Integrations tab, find Google Nest, and paste \
                 your Device Access project ID (from console.nest.google.com)"
            );
        }
    };

    let devices = list_devices(&enterprise_id, &token)?;

    let n = pull_devices(vault, &poll_ts, &devices, &enterprise_id)?;

    state.updated = Some(poll_ts);
    vault.write_nest_sync_ext(&state)?;

    Ok(PullOutcome {
        headline: format!("{n} readings"),
        counts: BTreeMap::from([("readings", n)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-google-nest-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (exact field names confirmed from SDM API docs) ----------

    fn thermostat_device(name: &str) -> Value {
        json!({
            "name": format!("enterprises/XYZ123/devices/{name}"),
            "type": "sdm.devices.types.THERMOSTAT",
            "traits": {
                "sdm.devices.traits.Info": {
                    "customName": "Living Room"
                },
                "sdm.devices.traits.Temperature": {
                    "ambientTemperatureCelsius": 23.0
                },
                "sdm.devices.traits.Humidity": {
                    "ambientHumidityPercent": 35.0
                },
                "sdm.devices.traits.ThermostatMode": {
                    "mode": "COOL",
                    "availableModes": ["HEAT", "COOL", "HEATCOOL", "OFF"]
                },
                "sdm.devices.traits.ThermostatHvac": {
                    "status": "COOLING"
                },
                "sdm.devices.traits.ThermostatTemperatureSetpoint": {
                    "heatCelsius": 20.0,
                    "coolCelsius": 24.0
                },
                "sdm.devices.traits.Connectivity": {
                    "status": "ONLINE"
                }
            },
            "parentRelations": [
                {
                    "parent": "enterprises/XYZ123/structures/ABC/rooms/R1",
                    "displayName": "Living Room"
                }
            ]
        })
    }

    /// A sparse camera device — no thermostat traits → no contract rows, raw only.
    fn camera_device(name: &str) -> Value {
        json!({
            "name": format!("enterprises/XYZ123/devices/{name}"),
            "type": "sdm.devices.types.CAMERA",
            "traits": {
                "sdm.devices.traits.Connectivity": {
                    "status": "ONLINE"
                }
            },
            "parentRelations": [
                {
                    "parent": "enterprises/XYZ123/structures/ABC/rooms/R2",
                    "displayName": "Front Door"
                }
            ]
        })
    }

    // --- pure mapping tests -----------------------------------------------

    #[test]
    fn trait_metrics_reads_all_four_numeric_traits() {
        let d = thermostat_device("THERM1");
        let traits = d.get("traits").unwrap();
        let metrics = trait_metrics(traits);
        let by_metric: BTreeMap<&str, f64> = metrics.iter().map(|(m, v)| (*m, *v)).collect();
        assert_eq!(by_metric.get("temperature"), Some(&23.0), "temperature");
        assert_eq!(by_metric.get("humidity"), Some(&35.0), "humidity");
        assert_eq!(by_metric.get("setpoint_heat"), Some(&20.0), "heat setpoint");
        assert_eq!(by_metric.get("setpoint_cool"), Some(&24.0), "cool setpoint");
    }

    #[test]
    fn camera_device_has_no_thermostat_traits() {
        let d = camera_device("CAM1");
        let traits = d.get("traits").unwrap();
        assert!(trait_metrics(traits).is_empty(), "no thermostat traits → no contract rows");
    }

    #[test]
    fn readings_from_device_produces_correct_rows() {
        let d = thermostat_device("THERM1");
        let poll_ts = "2026-06-17T10:00:00-07:00";
        let rows = readings_from_device(&d, poll_ts);
        let by_metric: BTreeMap<&str, &HomeReading> =
            rows.iter().map(|r| (r.metric.as_str(), r)).collect();

        assert!(by_metric.contains_key("temperature"));
        assert!(by_metric.contains_key("humidity"));
        assert!(by_metric.contains_key("setpoint_heat"));
        assert!(by_metric.contains_key("setpoint_cool"));

        let temp = by_metric["temperature"];
        assert_eq!(temp.source, "google-nest");
        assert_eq!(temp.value, 23.0);
        assert_eq!(temp.unit, "C");
        assert_eq!(temp.device, "Living Room");
        assert_eq!(temp.place, "Living Room");

        // Stable guid format.
        let guid = temp.extra.get("guid").and_then(Value::as_str).unwrap();
        assert!(
            guid.starts_with("google-nest:enterprises/XYZ123/devices/THERM1:temperature:"),
            "guid: {guid}"
        );

        // Non-numeric traits ride in extra.
        assert_eq!(
            temp.extra.get("thermostat_mode").and_then(Value::as_str),
            Some("COOL")
        );
        assert_eq!(
            temp.extra.get("hvac_status").and_then(Value::as_str),
            Some("COOLING")
        );
        assert_eq!(
            temp.extra.get("connectivity").and_then(Value::as_str),
            Some("ONLINE")
        );

        // Humidity uses "percent".
        assert_eq!(by_metric["humidity"].unit, "percent");
    }

    #[test]
    fn device_with_missing_name_produces_no_rows() {
        let d = json!({"type": "sdm.devices.types.THERMOSTAT", "traits": {
            "sdm.devices.traits.Temperature": {"ambientTemperatureCelsius": 22.0}
        }});
        let rows = readings_from_device(&d, "2026-06-17T10:00:00-07:00");
        assert!(rows.is_empty(), "no name → no rows");
    }

    #[test]
    fn enterprise_id_extracted_from_device_name() {
        let d = thermostat_device("THERM1");
        assert_eq!(
            enterprise_id_from_device(&d),
            Some("XYZ123".to_string())
        );
        // Missing name → None.
        assert!(enterprise_id_from_device(&json!({})).is_none());
    }

    #[test]
    fn device_display_name_prefers_info_trait_custom_name() {
        let d = thermostat_device("THERM1");
        assert_eq!(device_display_name(&d), "Living Room");
        // Fallback: last segment of device name.
        let no_info = json!({"name": "enterprises/X/devices/MY_DEVICE"});
        assert_eq!(device_display_name(&no_info), "MY_DEVICE");
    }

    #[test]
    fn full_poll_writes_both_layers_and_dedupes() {
        let v = temp_vault("fullpoll");
        let poll_ts = "2026-06-17T10:00:00-07:00";
        let devices = vec![thermostat_device("THERM1"), camera_device("CAM1")];

        let written = pull_devices(&v, poll_ts, &devices, "XYZ123").unwrap();
        assert!(written > 0, "thermostat readings written");

        // Contract layer has the thermostat readings.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        assert!(all.iter().any(|r| r.metric == "temperature"), "temperature reading");
        assert!(all.iter().any(|r| r.metric == "humidity"), "humidity reading");
        assert!(all.iter().any(|r| r.metric == "setpoint_heat"), "heat setpoint");
        assert!(all.iter().any(|r| r.metric == "setpoint_cool"), "cool setpoint");
        // No camera rows in contract.
        assert!(!all.iter().any(|r| r.device.contains("Front Door")));

        // Raw layer: both devices (thermostat + camera).
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_rows: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 2, "both devices in raw");
        // Raw carries the full trait object including camera connectivity.
        assert!(
            raw_rows.iter().any(|r| r
                .get("type")
                .and_then(Value::as_str)
                == Some("sdm.devices.types.CAMERA")),
            "camera device in raw"
        );

        // Re-run → guid dedup for contract AND raw dedup for raw layer.
        let written2 = pull_devices(&v, poll_ts, &devices, "XYZ123").unwrap();
        assert_eq!(written2, 0, "same poll_ts → all guids already stored");

        // Raw must NOT have accumulated duplicates from the re-run.
        let mut raw_rows2: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows2.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows2.len(), 2, "same-second re-poll must not duplicate raw lines");
    }

    #[test]
    fn different_poll_times_produce_different_guids() {
        let v = temp_vault("twosnaps");
        let devices = vec![thermostat_device("THERM1")];
        let n1 = pull_devices(&v, "2026-06-17T10:00:00-07:00", &devices, "XYZ123").unwrap();
        let n2 = pull_devices(&v, "2026-06-17T10:05:00-07:00", &devices, "XYZ123").unwrap();
        assert!(n1 > 0, "first snapshot written");
        assert!(n2 > 0, "second snapshot with different ts written (different guids)");
        // Both snapshots live in the store.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        let temp_rows: Vec<&HomeReading> =
            all.iter().filter(|r| r.metric == "temperature").collect();
        assert_eq!(temp_rows.len(), 2, "one temperature reading per snapshot");
    }

    #[test]
    fn cursor_back_compat_partial_deserialize() {
        // An empty cursor deserializes to all-default.
        let empty: ExtendedSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.enterprise_id.is_none());
        assert!(empty.updated.is_none());
        // A cursor with enterprise_id round-trips.
        let s = r#"{"enterprise_id":"XYZ123","updated":"2026-06-17T10:00:00-07:00"}"#;
        let state: ExtendedSyncState = serde_json::from_str(s).unwrap();
        assert_eq!(state.enterprise_id.as_deref(), Some("XYZ123"));
    }

    #[test]
    fn connection_has_oauth_method() {
        assert!(CONNECTION.method("oauth").is_some());
        assert_eq!(CONNECTION.id, "google-nest");
        assert_eq!(DEF.connection, Some("google-nest"));
    }

    /// The connection must also expose the TokenPaste step for the Device
    /// Access project ID — without it the `pull()` bootstrap can never
    /// succeed (the enterprise_id would never be written).
    #[test]
    fn connection_has_project_id_token_paste_method() {
        assert!(
            CONNECTION.method("token-paste").is_some(),
            "google-nest CONNECTION must include a TokenPaste method for the enterprise project ID"
        );
    }

    /// `connect_project_id` writes the enterprise id into the sync cursor and
    /// roundtrips through the cursor read path — the equivalent of completing
    /// the project-ID step in the UI after OAuth.
    #[test]
    fn connect_project_id_writes_enterprise_id() {
        let v = temp_vault("projid");

        // Empty → error.
        let err = connect_project_id(&v, "   ").unwrap_err().to_string();
        assert!(err.contains("required"), "empty input must error: {err}");

        // Slash in ID → error.
        let err = connect_project_id(&v, "enterprises/XYZ").unwrap_err().to_string();
        assert!(err.contains("wrong") || err.contains("UUID"), "slash input must error: {err}");

        // Valid UUID writes the enterprise_id into the cursor.
        connect_project_id(&v, "  abc-123-uuid  ").unwrap();
        let state = v.read_nest_sync_ext();
        assert_eq!(
            state.enterprise_id.as_deref(),
            Some("abc-123-uuid"),
            "trimmed enterprise_id persisted in cursor"
        );

        // A subsequent pull() with a token would use this value. Without a
        // token it still fails at fresh_token(), proving the bootstrap path
        // is correct: ID is written, only the OAuth step is missing.
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("not connected"),
            "pull fails at token step, not enterprise_id step: {err}"
        );
    }

    /// device_type is captured even when the device has no traits object.
    #[test]
    fn extra_fields_device_type_captured_without_traits() {
        // A device that has type but no traits at all.
        let d = json!({
            "name": "enterprises/X/devices/Y",
            "type": "sdm.devices.types.DOORBELL"
        });
        let extra = extra_fields(&d);
        assert_eq!(
            extra.get("device_type").and_then(Value::as_str),
            Some("sdm.devices.types.DOORBELL"),
            "device_type must be captured even without a traits object"
        );
        // Thermostat device still carries device_type (was inside traits guard before fix).
        let t = thermostat_device("T1");
        let extra_t = extra_fields(&t);
        assert_eq!(
            extra_t.get("device_type").and_then(Value::as_str),
            Some("sdm.devices.types.THERMOSTAT"),
            "device_type captured for thermostat with traits"
        );
    }

    #[test]
    fn pull_needs_connection() {
        let v = temp_vault("noconn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("not connected") || err.contains("enterprise project ID"),
            "clear error without connection: {err}"
        );
    }
}
