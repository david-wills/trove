//! Enphase Solar IQ Gateway — local LAN poll + Enlighten cloud backfill.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/enphase.md.
//!
//! Two pull paths, both producing raw energy-interval rows under
//! `home/enphase/energy/YYYY-MM.jsonl` (the home.energy sibling draft shape)
//! and verbatim raw at `home/enphase/raw/YYYY-MM.jsonl`:
//!
//! - **Local path** — HTTPS to the IQ Gateway on the LAN. The user pastes a
//!   1-year owner token (generated at enphase.com; expires after 1 year).
//!   Endpoints:
//!   - `GET /api/v1/production` — site-level watts/Wh now/today/lifetime;
//!     also consumption and storage arrays.
//!   - `GET /api/v1/production/inverters` — per-microinverter lastReportWatts.
//!
//!   The IQ Gateway serves a **self-signed TLS certificate**. We use HTTP
//!   by default for LAN access (the token rides in the Authorization header
//!   on a trusted local network). If the user passes an `https://` host we
//!   use it verbatim; the self-signed cert will be rejected by rustls without
//!   additional cert configuration, so HTTP is the recommended path.
//!
//! - **Enlighten cloud path** (Needs-David) — polls
//!   `developer-v4.enphase.com` for historical interval production. Not wired
//!   until David registers a cloud OAuth app; the local path is sufficient for
//!   real-time data.
//!
//! Token paste format: `host|token`  e.g.
//! `192.168.1.100|eyJhbGci…` or `envoy.local|eyJhbGci…`. The host is the
//! gateway's LAN address or hostname; the token is the 1-year owner JWT.
//!
//! Watermark cursor at `.trove/enphase-sync.json` (non-secret, rebuildable).
//! The local poll is a lightweight snapshot (no pagination); it appends rows
//! only when the readingTime has advanced past the last-seen epoch.
//!
//! **Contract layer**: the home.energy shape is an unbound Phase-3 sibling
//! draft — no Rust type exists for it yet. Rows are written as raw-only (the
//! home.energy JSON shape) under `home/enphase/energy/` and as verbatim API
//! responses under `home/enphase/raw/`. When the home.energy contract is
//! bound, the energy/ rows are already correctly shaped and need no migration.
//!
//! Field documentation confirmed against:
//! - `Matthew1471/Enphase-API` → `Documentation/IQ Gateway API/General/Production.adoc`
//!   (wNow, whLifetime, whToday, whLastSevenDays, readingTime, type, activeCount,
//!    measurementType, rmsCurrent, rmsVoltage, reactPwr, apprntPwr, pwrFactor)
//! - `Documentation/IQ Gateway API/V1/Production/Inverters.adoc`
//!   (serialNumber, lastReportDate, devType, lastReportWatts, maxReportWatts)
//!
//! **Known limitations (tracked for future work)**:
//!
//! - `kwh` is always `None` because the gateway reports cumulative totals
//!   (whToday/whLifetime), not per-interval deltas. When home.energy binds, a
//!   delta-based path can derive `kwh` from consecutive whLifetime differences.
//!   Until then, metered numbers live in `extra` only.
//!
//! - The `direction` field for net-consumption EIM rows is set to "consumption"
//!   unconditionally, even though net-consumption can be negative (export). The
//!   `circuit` field preserves `measurementType` (net-consumption vs
//!   total-consumption) and the raw layer is verbatim, so the distinction is
//!   recoverable from raw data.
//!
//! - `device` and `guid` are keyed on the user-pasted gateway host (LAN IP or
//!   hostname). If the user re-pastes a different address for the same physical
//!   gateway, rows are re-keyed. A future improvement is to fetch the gateway
//!   serial from `/info.xml` during connect and use it as the stable device id.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Vault path for home.energy-shaped interval rows (the sibling-draft shape).
const ENERGY_DIR: &str = "home/enphase/energy";
/// Vault path for verbatim API response rows (full fidelity).
const RAW_DIR: &str = "home/enphase/raw";
/// Non-secret rebuildable cursor — not under `.trove/sync/` (secrets).
const SYNC_FILE: &str = ".trove/enphase-sync.json";
/// Secret store slot for the `host|token` credential.
const SERVICE: &str = "enphase";

/// Seconds between syncs. Enphase gateway reports roughly every 15 minutes;
/// poll every 15 minutes to catch new intervals promptly.
pub const ENPHASE_SYNC_SECS: u64 = 900;

/// HTTP timeout for gateway requests (LAN; should be fast).
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Cursor.

/// Persisted non-secret cursor tracking last-written epochs per stream.
///
/// Two independent watermarks prevent the shared-cursor starvation bug: the
/// per-inverter `lastReportDate` clock always lags the production `readingTime`
/// clock, so a single max-epoch cursor would permanently gate out inverter rows
/// whose timestamps fall at or below the latest production reading.
#[derive(Debug, Serialize, Deserialize, Default)]
struct SyncState {
    /// The latest `readingTime` epoch written for production/consumption/storage
    /// device arrays. Zero = no prior data → silent baseline on first run.
    #[serde(default)]
    last_reading_time: i64,
    /// The latest `lastReportDate` epoch written for per-inverter rows.
    /// Tracked separately because inverter clocks lag the production clock.
    /// Zero = no prior data.
    #[serde(default)]
    last_inverter_time: i64,
}

impl Vault {
    fn read_enphase_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_enphase_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// home.energy interval shape (sibling-draft — no Rust type in home.rs yet).
// We write plain serde_json::Value rows shaped per the home.md energy spec:
//   ts, source, device?, circuit?, kwh?, value?, unit?, interval_secs?,
//   direction?, guid?, extra?
//
// Using a typed struct mirrors how the bound contract would look when it lands,
// and lets the compiler catch field-name typos now.

/// One energy interval row — matches the home.energy shape from home.md.
/// Written under `home/enphase/energy/YYYY-MM.jsonl`, partitioned by `ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnergyRow {
    /// RFC3339 local time the interval starts.
    ts: String,
    /// Collector id, always "enphase".
    source: String,
    /// Site or inverter serial (the gateway host or serialNumber).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    device: String,
    /// Sub-channel — "production", "net-consumption", "total-consumption", or
    /// a per-inverter serial when writing inverter-level rows.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    circuit: String,
    /// Electricity for the interval in kWh. None here because the gateway
    /// snapshot reports cumulative totals (whToday/whLifetime), not interval
    /// deltas. The field is reserved for a future delta-based path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kwh: Option<f64>,
    /// Direction: "production" | "consumption".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    direction: String,
    /// Stable dedupe key: `enphase:{device}:{circuit}:{readingTime}`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    guid: String,
    /// Everything source-specific the normalized columns don't carry.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    extra: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// API response shapes — confirmed field names from Matthew1471/Enphase-API docs.

/// Top-level response from `GET /api/v1/production`.
#[derive(Debug, Deserialize, Serialize)]
struct ProductionResponse {
    #[serde(default)]
    production: Vec<serde_json::Value>,
    #[serde(default)]
    consumption: Vec<serde_json::Value>,
    #[serde(default)]
    storage: Vec<serde_json::Value>,
}

/// One entry from `GET /api/v1/production/inverters` (per-microinverter).
#[derive(Debug, Deserialize, Serialize)]
struct InverterEntry {
    /// Microinverter serial number.
    #[serde(rename = "serialNumber")]
    serial_number: Option<String>,
    /// Epoch timestamp of last report.
    #[serde(rename = "lastReportDate")]
    last_report_date: Option<i64>,
    /// Device type code.
    #[serde(rename = "devType")]
    dev_type: Option<u32>,
    /// Watts at last report.
    #[serde(rename = "lastReportWatts")]
    last_report_watts: Option<i32>,
    /// Peak watts ever reported.
    #[serde(rename = "maxReportWatts")]
    max_report_watts: Option<i32>,
}

/// Parsed production device for internal logic (deserialized from the JSON).
#[derive(Debug, Deserialize)]
struct ProductionDeviceParsed {
    #[serde(rename = "type")]
    device_type: Option<String>,
    #[serde(rename = "wNow")]
    w_now: Option<f64>,
    #[serde(rename = "whLifetime")]
    wh_lifetime: Option<f64>,
    #[serde(rename = "whToday")]
    wh_today: Option<f64>,
    #[serde(rename = "whLastSevenDays")]
    wh_last_seven_days: Option<f64>,
    #[serde(rename = "readingTime")]
    reading_time: Option<i64>,
    #[serde(rename = "activeCount")]
    active_count: Option<u32>,
    #[serde(rename = "measurementType")]
    measurement_type: Option<String>,
    #[serde(rename = "rmsCurrent")]
    rms_current: Option<f64>,
    #[serde(rename = "rmsVoltage")]
    rms_voltage: Option<f64>,
    #[serde(rename = "reactPwr")]
    react_pwr: Option<f64>,
    #[serde(rename = "apprntPwr")]
    apprnt_pwr: Option<f64>,
    #[serde(rename = "pwrFactor")]
    pwr_factor: Option<f64>,
}

// ---------------------------------------------------------------------------
// Credential parsing.

/// Parse `host|token` from the pasted string. The host may be an IP or a
/// hostname; the token is the 1-year JWT.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your IQ Gateway address and owner token as host|token");
    }
    let (host, token) = pasted.split_once('|').ok_or_else(|| {
        anyhow::anyhow!(
            "paste the gateway address and token separated by |, e.g. \
             192.168.1.100|eyJhbGci… or envoy.local|eyJhbGci…"
        )
    })?;
    let host = host.trim().trim_end_matches('/').to_string();
    let token = token.trim().to_string();
    if host.is_empty() {
        bail!("missing gateway address — paste as host|token");
    }
    if token.is_empty() {
        bail!("missing token — paste as host|token");
    }
    Ok((host, token))
}

/// Build the base URL for the gateway. Default to HTTP for LAN access to avoid
/// self-signed-cert rejections; preserve explicit http:// or https:// prefixes.
fn gateway_url(host: &str) -> String {
    if host.starts_with("http://") || host.starts_with("https://") {
        host.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", host.trim_end_matches('/'))
    }
}

// ---------------------------------------------------------------------------
// API client abstraction (trait lets tests run fully offline).

trait EnphaseApi {
    /// `GET /api/v1/production` — site-level production snapshot (raw Value).
    fn production_raw(&self) -> Result<(ProductionResponse, Value)>;
    /// `GET /api/v1/production/inverters` — per-inverter snapshot (raw Value).
    fn inverters_raw(&self) -> Result<(Vec<InverterEntry>, Value)>;
}

struct GatewayClient {
    base_url: String,
    token: String,
}

impl GatewayClient {
    fn new(base_url: String, token: String) -> Self {
        GatewayClient { base_url, token }
    }

    fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{}{}", self.base_url, path);
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json")
            .timeout(HTTP_TIMEOUT)
            .call()
            .with_context(|| {
                format!(
                    "Could not reach IQ Gateway at {} — check the address is correct \
                     and the gateway is on the same network. If using HTTPS, try the \
                     IP address with http:// to avoid self-signed cert issues.",
                    self.base_url
                )
            })?;
        if resp.status() == 401 {
            bail!(
                "Gateway returned 401 — the owner token may have expired (1-year \
                 lifetime). Generate a new token at enlightenapp.com → my home → \
                 Gear icon → IQ Gateway → Local Access Token and re-paste it here."
            );
        }
        if resp.status() < 200 || resp.status() >= 300 {
            bail!("Gateway HTTP {}: {}", resp.status(), resp.status_text());
        }
        let body = resp
            .into_string()
            .context("reading gateway response body")?;
        serde_json::from_str::<Value>(&body)
            .with_context(|| format!("parsing gateway JSON response: {body:.200}"))
    }
}

impl EnphaseApi for GatewayClient {
    fn production_raw(&self) -> Result<(ProductionResponse, Value)> {
        let v = self.get_json("/api/v1/production")?;
        let parsed: ProductionResponse = serde_json::from_value(v.clone())
            .context("parsing production response")?;
        Ok((parsed, v))
    }

    fn inverters_raw(&self) -> Result<(Vec<InverterEntry>, Value)> {
        let v = self.get_json("/api/v1/production/inverters")?;
        let parsed: Vec<InverterEntry> = serde_json::from_value(v.clone())
            .context("parsing inverters response")?;
        Ok((parsed, v))
    }
}

// ---------------------------------------------------------------------------
// Row builders.

/// Convert a Unix epoch to a local-time RFC3339 string.
fn epoch_to_local_rfc3339(epoch: i64) -> Option<String> {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt: DateTime<Local>| dt.to_rfc3339())
}

/// Build a stable guid: `enphase:{device}:{circuit}:{epoch}`.
///
/// NOTE: `device` is the user-pasted gateway host (IP or hostname). If the
/// user changes the pasted address (e.g. from a DHCP IP to `envoy.local`),
/// the guid prefix changes and every existing row appears as a duplicate on
/// the next poll. A future improvement is to fetch the gateway's stable serial
/// from `/info.xml` or `/inventory.json` during `conn_connect` and store it
/// alongside the credential so the guid prefix is address-independent.
fn make_guid(device: &str, circuit: &str, epoch: i64) -> String {
    format!("enphase:{device}:{circuit}:{epoch}")
}

/// Build EnergyRow(s) from a raw JSON production device value.
/// Returns an empty vec when the readingTime is missing or stale.
fn device_value_to_row(
    dev_val: &Value,
    direction: &str,
    gateway_host: &str,
    after_epoch: i64,
) -> Option<EnergyRow> {
    let dev: ProductionDeviceParsed = serde_json::from_value(dev_val.clone()).ok()?;
    let reading_time = dev.reading_time.filter(|&t| t > 0)?;
    if reading_time <= after_epoch {
        return None;
    }
    let ts = epoch_to_local_rfc3339(reading_time)?;
    // circuit = measurementType if set, else device_type, else direction.
    let circuit = dev
        .measurement_type
        .as_deref()
        .filter(|s| !s.is_empty())
        .or_else(|| dev.device_type.as_deref().filter(|s| !s.is_empty()))
        .unwrap_or(direction)
        .to_string();

    let guid = make_guid(gateway_host, &circuit, reading_time);

    let mut extra: Map<String, Value> = Map::new();
    if let Some(v) = dev.w_now {
        extra.insert("w_now".into(), Value::from(v));
    }
    if let Some(v) = dev.wh_today {
        extra.insert("wh_today".into(), Value::from(v));
    }
    if let Some(v) = dev.wh_lifetime {
        extra.insert("wh_lifetime".into(), Value::from(v));
    }
    if let Some(v) = dev.wh_last_seven_days {
        extra.insert("wh_last_seven_days".into(), Value::from(v));
    }
    if let Some(v) = dev.active_count {
        extra.insert("active_count".into(), Value::from(v));
    }
    if let Some(v) = dev.rms_current {
        extra.insert("rms_current_a".into(), Value::from(v));
    }
    if let Some(v) = dev.rms_voltage {
        extra.insert("rms_voltage_v".into(), Value::from(v));
    }
    if let Some(v) = dev.react_pwr {
        extra.insert("reactive_power_var".into(), Value::from(v));
    }
    if let Some(v) = dev.apprnt_pwr {
        extra.insert("apparent_power_va".into(), Value::from(v));
    }
    if let Some(v) = dev.pwr_factor {
        extra.insert("power_factor".into(), Value::from(v));
    }

    Some(EnergyRow {
        ts,
        source: "enphase".to_string(),
        device: gateway_host.to_string(),
        circuit,
        kwh: None,
        direction: direction.to_string(),
        guid,
        extra,
    })
}

/// Build an EnergyRow from a per-inverter entry.
fn inverter_entry_to_row(
    inv: &InverterEntry,
    gateway_host: &str,
    after_epoch: i64,
) -> Option<EnergyRow> {
    let serial = inv.serial_number.as_deref().filter(|s| !s.is_empty())?;
    let report_ts = inv.last_report_date.filter(|&t| t > 0)?;
    if report_ts <= after_epoch {
        return None;
    }
    let ts = epoch_to_local_rfc3339(report_ts)?;
    let guid = make_guid(gateway_host, serial, report_ts);
    let mut extra: Map<String, Value> = Map::new();
    if let Some(v) = inv.last_report_watts {
        extra.insert("last_report_watts".into(), Value::from(v));
    }
    if let Some(v) = inv.max_report_watts {
        extra.insert("max_report_watts".into(), Value::from(v));
    }
    if let Some(v) = inv.dev_type {
        extra.insert("dev_type".into(), Value::from(v));
    }
    Some(EnergyRow {
        ts,
        source: "enphase".to_string(),
        device: gateway_host.to_string(),
        circuit: serial.to_string(),
        kwh: None,
        direction: "production".to_string(),
        guid,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Pull logic (testable via trait).

#[derive(Debug, Default)]
struct PullCounts {
    energy_rows: u64,
    raw_rows: u64,
}

/// A raw row for full-fidelity capture — the verbatim API JSON tagged with a
/// ts for the monthly partition writer and an endpoint label.
#[derive(Serialize)]
struct RawLine {
    /// Partition key (local RFC3339 timestamp); skipped in output (field used
    /// only for routing to the correct monthly JSONL file).
    #[serde(skip)]
    ts: String,
    endpoint: String,
    data: Value,
}

fn pull_with(vault: &Vault, api: &dyn EnphaseApi, gateway_host: &str) -> Result<PullCounts> {
    let cursor = vault.read_enphase_sync();
    let now_ts = Local::now().to_rfc3339();

    // Fetch production + inverter snapshots.
    let (prod, prod_raw_val) = api.production_raw()?;
    let (inverters, inv_raw_val) = api.inverters_raw().unwrap_or_else(|_| {
        (vec![], Value::Array(vec![]))
    });

    // Use SEPARATE watermarks per stream. The inverter `lastReportDate` clock
    // always lags the production `readingTime` clock; a single combined max
    // would permanently suppress inverter rows that arrive after a production
    // row has already advanced the cursor past them.
    let after_prod = cursor.last_reading_time;
    let after_inv = cursor.last_inverter_time;
    let mut all_energy: Vec<EnergyRow> = Vec::new();

    for dev_val in &prod.production {
        if let Some(row) = device_value_to_row(dev_val, "production", gateway_host, after_prod) {
            all_energy.push(row);
        }
    }
    for dev_val in &prod.consumption {
        if let Some(row) = device_value_to_row(dev_val, "consumption", gateway_host, after_prod) {
            all_energy.push(row);
        }
    }
    for dev_val in &prod.storage {
        if let Some(row) = device_value_to_row(dev_val, "production", gateway_host, after_prod) {
            all_energy.push(row);
        }
    }
    for inv in &inverters {
        if let Some(row) = inverter_entry_to_row(inv, gateway_host, after_inv) {
            all_energy.push(row);
        }
    }

    // Compute new watermarks per stream independently.
    let new_prod_watermark: i64 = prod
        .production
        .iter()
        .chain(prod.consumption.iter())
        .chain(prod.storage.iter())
        .filter_map(|v| v.get("readingTime").and_then(Value::as_i64))
        .max()
        .unwrap_or(0);
    let new_inv_watermark: i64 = inverters
        .iter()
        .filter_map(|i| i.last_report_date)
        .max()
        .unwrap_or(0);

    // First sync (last_reading_time == 0): silent baseline — advance both
    // cursors and write the raw layer (full-fidelity capture), but emit no
    // energy rows (the current snapshot is not a historical interval delta).
    let is_first_sync = cursor.last_reading_time == 0;

    // Write raw layer unconditionally (including first sync) for full fidelity.
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let raw_lines = vec![
        RawLine {
            ts: now_ts.clone(),
            endpoint: "/api/v1/production".to_string(),
            data: prod_raw_val,
        },
        RawLine {
            ts: now_ts,
            endpoint: "/api/v1/production/inverters".to_string(),
            data: inv_raw_val,
        },
    ];
    let raw_count = raw_lines.len() as u64;
    raw_stream
        .append(&raw_lines, |r| &r.ts)
        .context("writing enphase raw rows")?;

    if is_first_sync {
        // Advance cursors so next poll sees a real delta, but emit no rows.
        vault.write_enphase_sync(&SyncState {
            last_reading_time: new_prod_watermark.max(cursor.last_reading_time),
            last_inverter_time: new_inv_watermark.max(cursor.last_inverter_time),
        })?;
        return Ok(PullCounts { energy_rows: 0, raw_rows: raw_count });
    }

    if all_energy.is_empty() {
        // Data hasn't advanced — no energy write needed.
        return Ok(PullCounts { energy_rows: 0, raw_rows: raw_count });
    }

    // Write energy rows (home.energy sibling-draft shape).
    let energy_stream = vault.stream(ENERGY_DIR, Partition::Month);
    let energy_count = all_energy.len() as u64;
    energy_stream
        .append(&all_energy, |r| &r.ts)
        .context("writing enphase energy rows")?;

    // Advance both cursors only after successful energy writes.
    vault.write_enphase_sync(&SyncState {
        last_reading_time: new_prod_watermark.max(cursor.last_reading_time),
        last_inverter_time: new_inv_watermark.max(cursor.last_inverter_time),
    })?;

    Ok(PullCounts { energy_rows: energy_count, raw_rows: raw_count })
}

fn pull(vault: &Vault) -> Result<PullCounts> {
    let token_set = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Enphase IQ Gateway is not connected — paste your gateway address \
                 and owner token as address|token"
            )
        })?;
    let (host, token) = parse_credentials(&token_set.access_token)?;
    let base_url = gateway_url(&host);
    let client = GatewayClient::new(base_url, token);
    pull_with(vault, &client, &host)
}

// ---------------------------------------------------------------------------
// DEF registry hooks.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ENERGY_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.energy_rows;
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("Enphase synced — {n} energy rows")
            }))
        }
        Err(e) => {
            // Token expiry and connectivity failures are reported but don't
            // crash the loop. We distinguish expiry for a clearer message.
            let msg = e.to_string();
            let is_expiry = msg.contains("expired") || msg.contains("401");
            if is_expiry {
                Ok(CollectOutcome::note(format!(
                    "Enphase sync skipped — gateway token expired, re-paste required: {e}"
                )))
            } else {
                Ok(CollectOutcome::note(format!("Enphase sync skipped: {e}")))
            }
        }
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.energy_rows;
    let headline = if n == 0 {
        "Enphase is up to date — no new energy readings".to_string()
    } else {
        format!("Enphase synced — {n} energy rows")
    };
    let mut counts = BTreeMap::new();
    counts.insert("energy_rows", out.energy_rows);
    counts.insert("raw_rows", out.raw_rows);
    Ok(PullOutcome { headline, counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "enphase",
        name: "Enphase Solar",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Polls your Enphase IQ Gateway on the local network for solar production and \
             consumption data. Per-inverter snapshots and site totals land in the home \
             energy store. The gateway owner token is valid for one year and must be \
             renewed at enphase.com when it expires.",
        domain: "home",
        vault_path: "home/enphase/",
        toggleable: true,
        setup: &[
            "Log in to enlightenapp.com → my home → Gear icon → IQ Gateway → Local Access Token.",
            "Set the token duration to 1 year and click Generate Token, then copy it.",
            "Paste your gateway's LAN address and the token below as address|token, \
             e.g. 192.168.1.100|eyJhbGci…",
        ],
        caveats:
            "The local owner token expires after 1 year — regenerate it at enlightenapp.com \
             when it expires. The gateway must be reachable on your local network. HTTPS with \
             the gateway's self-signed certificate is not yet supported — use the IP address \
             with http:// (e.g. 192.168.1.100|token) for the local path.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(ENPHASE_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("enphase"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste: host|token composite).

fn conn_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (host, token) = parse_credentials(pasted)?;
    let base_url = gateway_url(&host);
    let client = GatewayClient::new(base_url.clone(), token.clone());
    // Verify with a real production call — proves the host is reachable and
    // the token is accepted. A 401 surfaces the "token expired" message.
    client.get_json("/api/v1/production").with_context(|| {
        format!(
            "Could not reach the IQ Gateway at {host}. Make sure your device is on the \
             same local network and the address is correct (IP or hostname from the \
             Enlighten app). If HTTPS is in use, try the plain IP with http://."
        )
    })?;
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: Some("EnphaseOwner".into()),
            scope: None,
            expires_at: None, // 1-year expiry is manually tracked; surfaced in caveats
        },
    )
}

fn conn_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn conn_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let display = if let Some((host, _)) = token.access_token.split_once('|') {
            host.trim().to_string()
        } else {
            "IQ Gateway".to_string()
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: display,
            connected_at: None,
            expires_at: None, // 1-year expiry surfaced in caveats
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "enphase",
    display_name: "Enphase IQ Gateway",
    methods: &[ConnectMethod::TokenPaste {
        label: "IQ Gateway address and owner token",
        help: "Paste as address|token, e.g. 192.168.1.100|eyJhbGci… — generate the \
               token at enlightenapp.com → my home → Gear icon → IQ Gateway → Local \
               Access Token (1-year duration). The token is stored locally and sent \
               only to your gateway on the local network.",
        placeholder: "192.168.1.100|eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9…",
        run: conn_connect,
    }],
    status: conn_status,
    disconnect: conn_disconnect,
    auto_pull: &["enphase"],
    setup: &[
        "In the Enlighten app, go to your home → Gear icon → IQ Gateway → Local Access Token.",
        "Select 1 year duration and click Generate Token.",
        "Copy the token and paste it here with your gateway address as address|token.",
    ],
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---------------------------------------------------------------------------
    // Fixtures — field names confirmed against Matthew1471/Enphase-API docs.

    /// Minimal production snapshot — one inverters device + one EIM.
    fn fixture_production_json() -> Value {
        json!({
            "production": [
                {
                    "type": "inverters",
                    "activeCount": 14,
                    "readingTime": 1749254400,
                    "wNow": 2840.0,
                    "whLifetime": 18543200.0,
                    "whToday": 12400.0,
                    "whLastSevenDays": 87300.0
                },
                {
                    "type": "eim",
                    "activeCount": 1,
                    "measurementType": "production",
                    "readingTime": 1749254400,
                    "wNow": 2835.5,
                    "whLifetime": 18543150.0,
                    "whToday": 12395.0,
                    "rmsCurrent": 11.7,
                    "rmsVoltage": 242.3,
                    "reactPwr": -45.2,
                    "apprntPwr": 2836.1,
                    "pwrFactor": 0.9998
                }
            ],
            "consumption": [
                {
                    "type": "eim",
                    "activeCount": 1,
                    "measurementType": "total-consumption",
                    "readingTime": 1749254400,
                    "wNow": 1150.3,
                    "whLifetime": 9870450.0,
                    "whToday": 5820.0
                }
            ],
            "storage": []
        })
    }

    /// Per-inverter response (2 entries for test brevity).
    fn fixture_inverters_json() -> Value {
        json!([
            {
                "serialNumber": "202311012345",
                "lastReportDate": 1749254380,
                "devType": 1,
                "lastReportWatts": 205,
                "maxReportWatts": 250
            },
            {
                "serialNumber": "202311012346",
                "lastReportDate": 1749254380,
                "devType": 1,
                "lastReportWatts": 198,
                "maxReportWatts": 250
            }
        ])
    }

    // ---------------------------------------------------------------------------
    // Stub API for fully offline tests.

    struct StubApi {
        prod_val: Value,
        inv_val: Value,
        prod_err: Option<&'static str>,
    }

    impl StubApi {
        fn ok() -> Self {
            StubApi {
                prod_val: fixture_production_json(),
                inv_val: fixture_inverters_json(),
                prod_err: None,
            }
        }
        fn auth_fail() -> Self {
            StubApi {
                prod_val: Value::Null,
                inv_val: Value::Null,
                prod_err: Some(
                    "Gateway returned 401 — the owner token may have expired (1-year lifetime).",
                ),
            }
        }
        fn empty_production() -> Self {
            StubApi {
                prod_val: json!({ "production": [], "consumption": [], "storage": [] }),
                inv_val: json!([]),
                prod_err: None,
            }
        }
    }

    impl EnphaseApi for StubApi {
        fn production_raw(&self) -> Result<(ProductionResponse, Value)> {
            if let Some(msg) = self.prod_err {
                bail!("{msg}");
            }
            let parsed: ProductionResponse =
                serde_json::from_value(self.prod_val.clone()).context("fixture parse")?;
            Ok((parsed, self.prod_val.clone()))
        }
        fn inverters_raw(&self) -> Result<(Vec<InverterEntry>, Value)> {
            let parsed: Vec<InverterEntry> =
                serde_json::from_value(self.inv_val.clone()).context("fixture parse")?;
            Ok((parsed, self.inv_val.clone()))
        }
    }

    fn make_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-enphase-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let vault = Vault::open_or_create(dir).unwrap();
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: "192.168.1.100|fake-token".to_string(),
                    refresh_token: None,
                    token_type: Some("EnphaseOwner".into()),
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();
        vault
    }

    // ---------------------------------------------------------------------------
    // Unit tests — pure mapping.

    #[test]
    fn enphase_parse_credentials_ok() {
        let (host, token) = parse_credentials("192.168.1.100|eyJhbGci…").unwrap();
        assert_eq!(host, "192.168.1.100");
        assert_eq!(token, "eyJhbGci…");
    }

    #[test]
    fn enphase_parse_credentials_with_hostname() {
        let (host, _) = parse_credentials("envoy.local|tok").unwrap();
        assert_eq!(host, "envoy.local");
    }

    #[test]
    fn enphase_parse_credentials_no_pipe_errors() {
        assert!(parse_credentials("192.168.1.100").is_err());
    }

    #[test]
    fn enphase_parse_credentials_empty_errors() {
        assert!(parse_credentials("").is_err());
    }

    #[test]
    fn enphase_parse_credentials_missing_host_errors() {
        assert!(parse_credentials("|mytoken").is_err());
    }

    #[test]
    fn enphase_gateway_url_adds_http_scheme() {
        assert_eq!(gateway_url("192.168.1.100"), "http://192.168.1.100");
        assert_eq!(gateway_url("envoy.local"), "http://envoy.local");
    }

    #[test]
    fn enphase_gateway_url_preserves_explicit_scheme() {
        assert_eq!(gateway_url("http://192.168.1.100"), "http://192.168.1.100");
        assert_eq!(gateway_url("https://envoy.local"), "https://envoy.local");
    }

    #[test]
    fn enphase_gateway_url_strips_trailing_slash() {
        assert_eq!(
            gateway_url("https://192.168.1.100/"),
            "https://192.168.1.100"
        );
    }

    #[test]
    fn enphase_production_inverters_device_maps_to_row() {
        let prod_val = fixture_production_json();
        let inv_dev = &prod_val["production"][0];
        // readingTime 1749254400, after_epoch = 0 → should produce a row.
        let row = device_value_to_row(inv_dev, "production", "192.168.1.100", 0).unwrap();
        assert_eq!(row.source, "enphase");
        assert_eq!(row.device, "192.168.1.100");
        assert_eq!(row.direction, "production");
        // circuit = device_type "inverters" (no measurementType on this entry).
        assert_eq!(row.circuit, "inverters");
        assert!(row.ts.starts_with("20"), "ts must be RFC3339");
        assert!(row.guid.contains("192.168.1.100"), "guid must include device");
        assert!(row.guid.contains("1749254400"), "guid must include readingTime");
        assert!(row.extra.contains_key("w_now"), "extra must have w_now");
        assert!(row.extra.contains_key("wh_today"), "extra must have wh_today");
        assert!(row.extra.contains_key("wh_lifetime"), "extra must have wh_lifetime");
        assert!(row.kwh.is_none(), "kwh must be None for a snapshot row");
    }

    #[test]
    fn enphase_eim_device_carries_electrical_extras_and_measurement_type_circuit() {
        let prod_val = fixture_production_json();
        let eim_dev = &prod_val["production"][1]; // EIM with measurementType="production"
        let row = device_value_to_row(eim_dev, "production", "192.168.1.100", 0).unwrap();
        // circuit = measurementType "production" (takes priority over type "eim").
        assert_eq!(row.circuit, "production");
        assert!(
            row.extra.contains_key("rms_current_a"),
            "EIM row must have rms_current_a"
        );
        assert!(
            row.extra.contains_key("power_factor"),
            "EIM row must have power_factor"
        );
    }

    #[test]
    fn enphase_consumption_device_direction_is_consumption() {
        let prod_val = fixture_production_json();
        let cons_dev = &prod_val["consumption"][0];
        let row = device_value_to_row(cons_dev, "consumption", "192.168.1.100", 0).unwrap();
        assert_eq!(row.direction, "consumption");
        assert_eq!(row.circuit, "total-consumption");
    }

    #[test]
    fn enphase_stale_device_yields_no_row() {
        let prod_val = fixture_production_json();
        let dev = &prod_val["production"][0]; // readingTime = 1749254400
        // after_epoch >= readingTime → stale, no row.
        let row = device_value_to_row(dev, "production", "192.168.1.100", 1749254400);
        assert!(row.is_none(), "stale readingTime must yield no row");
    }

    #[test]
    fn enphase_zero_reading_time_yields_no_row() {
        let dev = json!({
            "type": "inverters",
            "activeCount": 0,
            "readingTime": 0,
            "wNow": 0.0
        });
        let row = device_value_to_row(&dev, "production", "192.168.1.100", 0);
        assert!(row.is_none(), "readingTime=0 must yield no row");
    }

    #[test]
    fn enphase_inverter_entry_maps_to_row() {
        let inv_val = fixture_inverters_json();
        let entries: Vec<InverterEntry> =
            serde_json::from_value(inv_val).unwrap();
        let row = inverter_entry_to_row(&entries[0], "192.168.1.100", 0).unwrap();
        assert_eq!(row.source, "enphase");
        assert_eq!(row.circuit, "202311012345");
        assert_eq!(row.direction, "production");
        assert!(
            row.extra.contains_key("last_report_watts"),
            "inverter row must have last_report_watts"
        );
        assert!(
            row.extra.contains_key("max_report_watts"),
            "inverter row must have max_report_watts"
        );
        assert!(row.guid.contains("202311012345"), "guid must include serial");
    }

    // ---------------------------------------------------------------------------
    // Integration-level tests: pull_with behaviour.

    #[test]
    fn enphase_first_sync_is_silent_baseline() {
        let vault = make_vault("baseline");
        let api = StubApi::ok();
        let out = pull_with(&vault, &api, "192.168.1.100").unwrap();
        assert_eq!(out.energy_rows, 0, "first sync must write no energy rows");
        // Raw layer is written even on first sync for full-fidelity capture.
        assert_eq!(out.raw_rows, 2, "first sync must write raw rows");
        let cursor = vault.read_enphase_sync();
        assert!(
            cursor.last_reading_time > 0,
            "production cursor must advance after first sync"
        );
        assert!(
            cursor.last_inverter_time > 0,
            "inverter cursor must advance after first sync"
        );
    }

    #[test]
    fn enphase_second_sync_writes_energy_and_raw_rows() {
        let vault = make_vault("second-sync");
        // Prime cursor to just before the fixture readingTime (1749254400).
        vault
            .write_enphase_sync(&SyncState { last_reading_time: 1749250000, last_inverter_time: 0 })
            .unwrap();
        let api = StubApi::ok();
        let out = pull_with(&vault, &api, "192.168.1.100").unwrap();
        // 3 production/consumption devices + 2 inverters = 5 energy rows.
        assert!(
            out.energy_rows > 0,
            "second sync must write energy rows, got 0"
        );
        assert_eq!(out.raw_rows, 2, "must write 2 raw rows (production + inverters)");
    }

    #[test]
    fn enphase_stale_cursor_writes_no_energy_rows() {
        let vault = make_vault("stale");
        // Cursor well beyond the fixture readingTime.
        vault
            .write_enphase_sync(&SyncState { last_reading_time: 1_800_000_000, last_inverter_time: 1_800_000_000 })
            .unwrap();
        let api = StubApi::ok();
        let out = pull_with(&vault, &api, "192.168.1.100").unwrap();
        assert_eq!(out.energy_rows, 0, "stale cursor must write no new energy rows");
        // Raw layer is still written for fidelity even when nothing is new.
        assert_eq!(out.raw_rows, 2, "raw rows are always written");
    }

    #[test]
    fn enphase_empty_production_response_is_ok() {
        let vault = make_vault("empty");
        vault
            .write_enphase_sync(&SyncState { last_reading_time: 1, last_inverter_time: 0 })
            .unwrap();
        let api = StubApi::empty_production();
        let out = pull_with(&vault, &api, "192.168.1.100").unwrap();
        assert_eq!(out.energy_rows, 0);
    }

    #[test]
    fn enphase_auth_failure_propagates_as_error() {
        let vault = make_vault("auth-fail");
        vault
            .write_enphase_sync(&SyncState { last_reading_time: 1, last_inverter_time: 0 })
            .unwrap();
        let api = StubApi::auth_fail();
        let err = pull_with(&vault, &api, "192.168.1.100").unwrap_err();
        assert!(
            err.to_string().contains("401") || err.to_string().contains("expired"),
            "auth failure must mention token expiry: {err}"
        );
    }

    #[test]
    fn enphase_cursor_advances_after_write() {
        let vault = make_vault("cursor-advance");
        vault
            .write_enphase_sync(&SyncState { last_reading_time: 1749250000, last_inverter_time: 0 })
            .unwrap();
        let api = StubApi::ok();
        let _ = pull_with(&vault, &api, "192.168.1.100").unwrap();
        let cursor = vault.read_enphase_sync();
        // Fixture readingTime = 1749254400; inverters lastReportDate = 1749254380.
        assert_eq!(
            cursor.last_reading_time, 1749254400,
            "cursor must advance to max production readingTime"
        );
        assert_eq!(
            cursor.last_inverter_time, 1749254380,
            "inverter cursor must advance to max lastReportDate"
        );
    }

    #[test]
    fn enphase_split_watermarks_prevent_inverter_starvation() {
        // Set the production cursor beyond the inverter fixture timestamp (1749254380)
        // but below the production fixture timestamp (1749254400). This simulates the
        // starvation scenario: a shared max-cursor would suppress inverter rows at 1749254380
        // because 1749250000 < 1749254380 < 1749254400; with split watermarks, the
        // inverter cursor (last_inverter_time=0) gates inverter rows independently.
        let vault = make_vault("split-watermark");
        vault
            .write_enphase_sync(&SyncState {
                last_reading_time: 1749250000,
                last_inverter_time: 0,
            })
            .unwrap();
        let api = StubApi::ok();
        let out = pull_with(&vault, &api, "192.168.1.100").unwrap();
        // Should include inverter rows (2 inverters) since last_inverter_time=0 < 1749254380.
        assert!(out.energy_rows >= 2, "inverter rows must not be starved by production cursor");
    }

    #[test]
    fn enphase_connection_def_is_token_paste() {
        assert_eq!(CONNECTION.id, "enphase");
        assert!(
            CONNECTION.method("token-paste").is_some(),
            "CONNECTION must expose a token-paste method"
        );
    }

    #[test]
    fn enphase_def_is_periodic_with_connection() {
        assert_eq!(DEF.meta.id, "enphase");
        assert_eq!(DEF.connection, Some("enphase"));
        assert!(
            matches!(DEF.behavior, Behavior::Periodic { .. }),
            "DEF must be Periodic"
        );
    }

    #[test]
    fn enphase_energy_row_omits_none_kwh() {
        let row = EnergyRow {
            ts: "2026-06-17T10:00:00-07:00".to_string(),
            source: "enphase".to_string(),
            device: "192.168.1.100".to_string(),
            circuit: "production".to_string(),
            kwh: None,
            direction: "production".to_string(),
            guid: "enphase:192.168.1.100:production:1749254400".to_string(),
            extra: {
                let mut m = Map::new();
                m.insert("w_now".into(), json!(2840.0));
                m.insert("wh_today".into(), json!(12400.0));
                m
            },
        };
        let j = serde_json::to_value(&row).unwrap();
        assert_eq!(j["source"], "enphase");
        assert_eq!(j["direction"], "production");
        // kwh is None → must be omitted (skip_serializing_if).
        assert!(j.get("kwh").is_none(), "kwh:None must not appear in output");
        assert_eq!(j["extra"]["w_now"], 2840.0);
    }

    #[test]
    fn enphase_energy_row_round_trips() {
        let row = EnergyRow {
            ts: "2026-06-17T10:00:00-07:00".to_string(),
            source: "enphase".to_string(),
            device: "envoy.local".to_string(),
            circuit: "202311012345".to_string(),
            kwh: None,
            direction: "production".to_string(),
            guid: "enphase:envoy.local:202311012345:1749254380".to_string(),
            extra: Map::new(),
        };
        let s = serde_json::to_string(&row).unwrap();
        let row2: EnergyRow = serde_json::from_str(&s).unwrap();
        assert_eq!(row.ts, row2.ts);
        assert_eq!(row.guid, row2.guid);
        assert!(row2.kwh.is_none());
    }
}
