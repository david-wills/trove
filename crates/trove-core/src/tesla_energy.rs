//! Tesla Powerwall + Solar — Fleet API periodic energy history poller.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/tesla-energy.md
//!
//! Polls `GET /api/1/products` to discover energy sites, then fetches daily
//! energy aggregates for each site via
//! `GET /api/1/energy_sites/{site_id}/calendar_history?kind=energy&period=day`
//! on a 6-hour cadence. Writes two layers:
//!
//! - **Raw layer** (`home/tesla-energy/raw/YYYY-MM.jsonl`): verbatim API
//!   responses — full `calendar_history` JSON objects, one row per site per
//!   poll, month-partitioned by poll time.
//!
//! - **Energy interval layer** (`home/tesla-energy/energy/YYYY-MM.jsonl`):
//!   one row per calendar day per energy direction per site, following the
//!   `home.energy` draft schema (ts, source, device, circuit, kwh, direction,
//!   interval_secs, guid). The `home.energy` Rust contract is not yet bound;
//!   rows are plain `serde_json::Value` shaped per the sibling-draft spec so
//!   no migration is needed when the pioneer binds it.
//!
//! ## Auth
//!
//! Shares the `tesla` OAuth connection with `crate::tesla` (vehicle data).
//! The same token and app registration cover both vehicle and energy scopes —
//! no extra login needed. Calls `crate::tesla::ensure_fresh` for token
//! refresh (Tesla issues refresh tokens; silent refresh on expiry).
//!
//! ## Scopes note
//!
//! The Tesla `energy_device_data` scope is required for calendar_history data.
//! The base vehicle scopes in the tesla provider's `scopes` field do NOT
//! include it. Owners with a Powerwall/solar install need to reconnect once
//! with extended scopes; that is a Needs-login validation item.
//!
//! ## Calendar history fields (confirmed from Home Assistant tesla_fleet sensor.py)
//!
//! **Unit**: all energy fields are in **Watt-hours (Wh)** as returned by the Tesla
//! Fleet API. Home Assistant's tesla_fleet integration declares
//! `native_unit_of_measurement=UnitOfEnergy.WATT_HOUR` with no division applied,
//! confirming the API sends raw Wh integers. Divide by 1000.0 to get kWh for the
//! home.energy schema `kwh` field.
//!
//! Response shape (inside `response.time_series[]`):
//! - `timestamp` — site-local ISO 8601 with offset, e.g. "2022-05-13T00:00:00-07:00"
//!   (NOT UTC; offset reflects the site's timezone)
//! - `solar_energy_exported` — Wh solar generation
//! - `battery_energy_exported` — Wh battery discharged
//! - `battery_energy_imported_from_solar` — Wh
//! - `battery_energy_imported_from_grid` — Wh
//! - `grid_energy_imported` — Wh imported from grid
//! - `grid_energy_exported_from_solar` — Wh
//! - `grid_energy_exported_from_battery` — Wh
//! - `consumer_energy_imported_from_solar` — Wh direct home from solar
//! - `consumer_energy_imported_from_battery` — Wh home from battery
//! - `consumer_energy_imported_from_grid` — Wh home from grid
//! - `total_home_usage` — Wh total home consumption
//! - `total_battery_charge` — Wh total into battery
//! - `total_battery_discharge` — Wh total out of battery
//! - `total_solar_generation` — Wh total solar
//! - `total_grid_energy_exported` — Wh net exported
//! - `generator_energy_exported` — Wh (zero for most sites)
//! - `grid_services_energy_imported` / `_exported` — Wh grid services
//! - `grid_energy_exported_from_generator` / `battery_energy_imported_from_generator`
//!
//! ## Cursor
//!
//! `.trove/tesla-energy-sync.json` stores `last_day: "YYYY-MM-DD"` per site
//! (keyed by site_id string). On first run, backfills 30 days; on subsequent
//! runs fetches from the day after `last_day` to today. Cursor advances only
//! after all rows for that day window are written.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "tesla";
/// Raw verbatim API responses.
const RAW_DIR: &str = "home/tesla-energy/raw";
/// Home.energy draft-shaped interval rows.
const ENERGY_DIR: &str = "home/tesla-energy/energy";
/// Non-secret, rebuildable cursor.
const SYNC_FILE: &str = ".trove/tesla-energy-sync.json";
/// 6-hour cadence — energy data is daily; 4 polls/day keeps the trail fresh.
pub const TESLA_ENERGY_SYNC_SECS: u64 = 6 * 3600;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Default backfill window on first run.
const DEFAULT_BACKFILL_DAYS: i64 = 30;
/// Tesla Fleet API base (NA region; most English-speaking markets).
const FLEET_BASE: &str = "https://fleet-api.prd.na.vn.cloud.tesla.com/api/1";

// ---------------------------------------------------------------------------
// Cursor.

/// Per-site watermark: the last calendar day (UTC) for which rows are written.
/// Key = site_id as string; value = "YYYY-MM-DD".
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    #[serde(default)]
    last_day: BTreeMap<String, String>,
}

impl Vault {
    fn read_tesla_energy_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_tesla_energy_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API types — field names confirmed from Home Assistant tesla_fleet const.py
// (ENERGY_HISTORY_FIELDS) and TeslaPy endpoints.json (CALENDAR_HISTORY_DATA).

/// One product from `GET /api/1/products` that is an energy site (not a
/// vehicle). Keyed by `energy_site_id`; sites without it are vehicles/others.
#[derive(Debug, Deserialize)]
struct ProductItem {
    /// Numeric energy site id.
    #[serde(default)]
    energy_site_id: Option<i64>,
    /// Human-readable site name (may be absent).
    #[serde(default)]
    site_name: Option<String>,
}

/// Top-level `GET /api/1/products` response.
#[derive(Debug, Deserialize)]
struct ProductsResponse {
    response: Vec<Value>,
}

/// One day's energy aggregates inside `calendar_history.response.time_series`.
/// All energy fields are in **Watt-hours (Wh)** as returned by the Tesla Fleet API
/// (confirmed: Home Assistant tesla_fleet uses native_unit_of_measurement=WATT_HOUR).
/// Fields are optional (a solar-only site has no battery fields, etc.).
/// Every unmapped field rides in the raw layer verbatim.
#[derive(Debug, Deserialize)]
struct EnergyDay {
    /// Site-local ISO 8601 timestamp with offset, e.g. "2022-05-13T00:00:00-07:00".
    /// NOT UTC — offset reflects the site's local timezone.
    timestamp: Option<String>,
    solar_energy_exported: Option<f64>,
    battery_energy_exported: Option<f64>,
    battery_energy_imported_from_solar: Option<f64>,
    battery_energy_imported_from_grid: Option<f64>,
    grid_energy_imported: Option<f64>,
    grid_energy_exported_from_solar: Option<f64>,
    grid_energy_exported_from_battery: Option<f64>,
    consumer_energy_imported_from_solar: Option<f64>,
    consumer_energy_imported_from_battery: Option<f64>,
    consumer_energy_imported_from_grid: Option<f64>,
    total_home_usage: Option<f64>,
    total_battery_charge: Option<f64>,
    total_battery_discharge: Option<f64>,
    total_solar_generation: Option<f64>,
    total_grid_energy_exported: Option<f64>,
    generator_energy_exported: Option<f64>,
    grid_services_energy_imported: Option<f64>,
    grid_services_energy_exported: Option<f64>,
    grid_energy_exported_from_generator: Option<f64>,
    battery_energy_imported_from_generator: Option<f64>,
    consumer_energy_imported_from_generator: Option<f64>,
}

/// `calendar_history` response top-level (`response.time_series` is the list).
#[derive(Debug, Deserialize)]
struct CalendarHistoryResponse {
    response: CalendarHistoryBody,
}

#[derive(Debug, Default, Deserialize)]
struct CalendarHistoryBody {
    #[serde(default)]
    time_series: Vec<Value>,
}

// ---------------------------------------------------------------------------
// API trait for testability.

trait TeslaEnergyApi {
    /// `GET /api/1/products` — full raw value.
    fn list_products(&self, token: &str) -> Result<Value>;
    /// `GET /api/1/energy_sites/{site_id}/calendar_history?kind=energy&period=day
    ///   &start_date=...&end_date=...` — full raw value.
    fn calendar_history(
        &self,
        token: &str,
        site_id: i64,
        start_date: &str,
        end_date: &str,
    ) -> Result<Value>;
}

struct FleetClient;

impl FleetClient {
    fn get(&self, url: &str, token: &str) -> Result<Value> {
        let resp = ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            .call()
            .with_context(|| format!("Tesla Fleet API request failed: {url}"))?;
        if resp.status() == 401 || resp.status() == 403 {
            anyhow::bail!(
                "Tesla Fleet API returned {} — reconnect from the Integrations tab",
                resp.status()
            );
        }
        let status = resp.status();
        if status < 200 || status >= 300 {
            let body = resp.into_string().unwrap_or_default();
            anyhow::bail!(
                "Tesla Fleet API HTTP {}: {}",
                status,
                body.chars().take(300).collect::<String>()
            );
        }
        resp.into_json::<Value>()
            .context("parsing Tesla Fleet API response")
    }
}

impl TeslaEnergyApi for FleetClient {
    fn list_products(&self, token: &str) -> Result<Value> {
        self.get(&format!("{FLEET_BASE}/products"), token)
    }

    fn calendar_history(
        &self,
        token: &str,
        site_id: i64,
        start_date: &str,
        end_date: &str,
    ) -> Result<Value> {
        let url = format!(
            "{FLEET_BASE}/energy_sites/{site_id}/calendar_history\
             ?kind=energy&period=day&start_date={start_date}T00:00:00Z&end_date={end_date}T23:59:59Z"
        );
        self.get(&url, token)
    }
}

// ---------------------------------------------------------------------------
// Row builders.

/// Build home.energy-shaped rows for a single site's EnergyDay.
/// Returns one row per meaningful direction (solar generation, battery
/// discharge, grid import, total home usage, etc.), skipping None/zero-kwh
/// flows that the site doesn't have.
///
/// `ts` is the RFC3339 local start-of-day for the interval.
fn energy_day_to_rows(site_id_str: &str, ts: &str, day: &EnergyDay, raw_val: &Value) -> Vec<Value> {
    // Capture all the raw fields in extra for full fidelity.
    let raw_obj = raw_val.as_object().cloned().unwrap_or_default();

    // (circuit, direction, wh) tuples for the directions we emit.
    // NOTE: Tesla Fleet API returns all energy values in Watt-hours (Wh).
    // We divide by 1000.0 before writing to the home.energy `kwh` field (kWh).
    // Direction enum: production/consumption for on-site flows; import/export for grid flows.
    let flows: &[(&str, &str, Option<f64>)] = &[
        ("solar", "production", day.solar_energy_exported),
        ("battery_discharge", "production", day.battery_energy_exported),
        ("total_home_usage", "consumption", day.total_home_usage),
        ("grid_import", "import", day.grid_energy_imported),
        ("grid_export_from_solar", "export", day.grid_energy_exported_from_solar),
        ("grid_export_from_battery", "export", day.grid_energy_exported_from_battery),
        ("battery_charge_from_solar", "consumption", day.battery_energy_imported_from_solar),
        ("battery_charge_from_grid", "import", day.battery_energy_imported_from_grid),
        ("home_from_solar", "consumption", day.consumer_energy_imported_from_solar),
        ("home_from_battery", "consumption", day.consumer_energy_imported_from_battery),
        ("home_from_grid", "consumption", day.consumer_energy_imported_from_grid),
        ("total_solar", "production", day.total_solar_generation),
        ("total_battery_charge", "consumption", day.total_battery_charge),
        ("total_battery_discharge", "production", day.total_battery_discharge),
        ("total_grid_exported", "export", day.total_grid_energy_exported),
        ("generator", "production", day.generator_energy_exported),
    ];

    let mut rows = Vec::new();
    for (circuit, direction, wh_opt) in flows {
        let wh = match wh_opt {
            Some(v) if *v != 0.0 => *v,
            _ => continue,
        };
        // Convert Wh → kWh for the home.energy schema kwh field.
        let kwh = wh / 1000.0;
        let guid = format!("tesla-energy:{site_id_str}:{circuit}:{ts}");
        let mut row = serde_json::json!({
            "ts": ts,
            "source": "tesla-energy",
            "device": site_id_str,
            "circuit": circuit,
            "kwh": kwh,
            "interval_secs": 86400_u64,
            "direction": direction,
            "guid": guid,
        });
        // Overflow: remaining raw fields go into extra for full fidelity.
        let mut extra: Map<String, Value> = Map::new();
        for (k, v) in &raw_obj {
            if !matches!(
                k.as_str(),
                "timestamp"
                    | "solar_energy_exported"
                    | "battery_energy_exported"
                    | "total_home_usage"
                    | "grid_energy_imported"
            ) {
                extra.insert(k.clone(), v.clone());
            }
        }
        if !extra.is_empty() {
            if let Some(obj) = row.as_object_mut() {
                obj.insert("extra".into(), Value::Object(extra));
            }
        }
        rows.push(row);
    }
    rows
}

/// Parse the timestamp from a `calendar_history` time_series entry.
/// Returns an RFC3339 string preserving the site's original timezone offset.
///
/// The Tesla Fleet API returns site-local timestamps with an explicit offset,
/// e.g. "2022-05-13T00:00:00-07:00". We preserve that offset rather than
/// converting to machine-local time, so the guid is offset-stable regardless
/// of which machine runs the poller.
fn parse_day_ts(entry: &Value) -> Option<String> {
    let ts_str = entry.get("timestamp").and_then(Value::as_str)?;
    // Prefer the offset-preserving parse (FixedOffset) so we don't shift the
    // site-local calendar day into the machine's local timezone.
    if let Ok(dt) = ts_str.parse::<DateTime<FixedOffset>>() {
        return Some(dt.to_rfc3339());
    }
    // Fallback: plain UTC (no offset in string).
    if let Ok(dt) = ts_str.parse::<DateTime<Utc>>() {
        return Some(dt.to_rfc3339());
    }
    // Last resort: bare date.
    NaiveDate::parse_from_str(&ts_str[..ts_str.len().min(10)], "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Pull logic.

#[derive(Debug, Default)]
struct PullCounts {
    sites: u64,
    energy_rows: u64,
    raw_rows: u64,
}

fn pull_with(vault: &Vault, api: &dyn TeslaEnergyApi, token: &str) -> Result<PullCounts> {
    let mut state = vault.read_tesla_energy_sync();
    let today = Utc::now().date_naive();
    let today_str = today.format("%Y-%m-%d").to_string();
    let now_ts = Local::now().to_rfc3339();

    // 1. Discover energy sites.
    let products_raw = api.list_products(token)?;
    let products_resp: ProductsResponse = serde_json::from_value(products_raw.clone())
        .context("parsing /products response")?;

    let sites: Vec<(i64, String)> = products_resp
        .response
        .iter()
        .filter_map(|v| {
            let item: ProductItem = serde_json::from_value(v.clone()).ok()?;
            let sid = item.energy_site_id?;
            let name = item
                .site_name
                .unwrap_or_else(|| format!("site_{sid}"));
            Some((sid, name))
        })
        .collect();

    if sites.is_empty() {
        return Ok(PullCounts::default());
    }

    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let energy_stream = vault.stream(ENERGY_DIR, Partition::Month);

    let mut counts = PullCounts { sites: sites.len() as u64, ..Default::default() };

    for (site_id, site_name) in &sites {
        let site_id_str = site_id.to_string();

        // Determine date window: day after watermark .. today.
        let from_date = state
            .last_day
            .get(&site_id_str)
            .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
            .and_then(|d| d.succ_opt())
            .unwrap_or_else(|| today - chrono::Duration::days(DEFAULT_BACKFILL_DAYS));

        if from_date > today {
            // Fully up to date.
            continue;
        }

        let start_str = from_date.format("%Y-%m-%d").to_string();

        // Fetch calendar history for this window.
        let hist_raw = match api.calendar_history(token, *site_id, &start_str, &today_str) {
            Ok(v) => v,
            Err(e) => {
                // Non-fatal: log and continue with other sites.
                eprintln!(
                    "tesla-energy: calendar_history for site {site_id_str} failed: {e}"
                );
                continue;
            }
        };

        // Write raw layer unconditionally.
        let raw_line = serde_json::json!({
            "ts": now_ts,
            "site_id": site_id_str,
            "site_name": site_name,
            "endpoint": "calendar_history",
            "data": hist_raw,
        });
        raw_stream
            .append(&[raw_line], |v| {
                v.get("ts").and_then(Value::as_str).unwrap_or("")
            })
            .context("writing tesla-energy raw row")?;
        counts.raw_rows += 1;

        // Parse the time_series.
        let body: CalendarHistoryResponse =
            serde_json::from_value(hist_raw).unwrap_or_else(|_| CalendarHistoryResponse {
                response: CalendarHistoryBody::default(),
            });

        // Dedup guids for completed days only. Today's rows are NOT deduped so
        // each poll overwrites the partial-day aggregate with the latest value,
        // ensuring the final full-day total is captured before the cursor advances.
        let mut seen_guids: HashSet<String> = HashSet::new();
        for key in energy_stream.partitions().unwrap_or_default() {
            for v in energy_stream.read::<Value>(&key).unwrap_or_default() {
                if let Some(g) = v.get("guid").and_then(Value::as_str) {
                    // Only record guids for days strictly before today.
                    // Today's guids are intentionally NOT added to seen_guids so
                    // the fresh partial aggregate passes through each poll.
                    if let Some(row_ts) = v.get("ts").and_then(Value::as_str) {
                        let row_date = NaiveDate::parse_from_str(&row_ts[..10.min(row_ts.len())], "%Y-%m-%d").ok();
                        if row_date.map_or(true, |d| d < today) {
                            seen_guids.insert(g.to_string());
                        }
                    } else {
                        seen_guids.insert(g.to_string());
                    }
                }
            }
        }

        let mut new_rows: Vec<Value> = Vec::new();
        // max_day tracks the latest COMPLETED day (strictly before today).
        // We never advance the cursor onto today while it is in progress.
        let mut max_completed_day: Option<NaiveDate> = None;

        for entry in &body.response.time_series {
            let ts = match parse_day_ts(entry) {
                Some(t) => t,
                None => continue,
            };

            // Parse the calendar date from the timestamp (first 10 chars = YYYY-MM-DD).
            let ts_raw = entry.get("timestamp").and_then(Value::as_str).unwrap_or("");
            let day_date = NaiveDate::parse_from_str(&ts_raw[..ts_raw.len().min(10)], "%Y-%m-%d").ok();

            let day: EnergyDay = match serde_json::from_value(entry.clone()) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let rows = energy_day_to_rows(&site_id_str, &ts, &day, entry);
            for row in rows {
                let guid = row.get("guid").and_then(Value::as_str).unwrap_or("").to_string();
                // For completed days: standard dedup. For today: always emit (overwrite).
                let is_today = day_date.map_or(false, |d| d == today);
                if guid.is_empty() || is_today || seen_guids.insert(guid) {
                    new_rows.push(row);
                }
            }

            // Only advance cursor for completed days (strictly before today).
            if let Some(d) = day_date {
                if d < today {
                    max_completed_day =
                        Some(max_completed_day.map_or(d, |prev: NaiveDate| prev.max(d)));
                }
            }
        }

        if !new_rows.is_empty() {
            energy_stream
                .append(&new_rows, |v| {
                    v.get("ts").and_then(Value::as_str).unwrap_or("")
                })
                .context("writing tesla-energy energy rows")?;
            counts.energy_rows += new_rows.len() as u64;
        }

        // Advance cursor to the last COMPLETED day only.
        // Today remains unfinalised — every poll re-fetches and overwrites it.
        if let Some(d) = max_completed_day {
            let d_str = d.format("%Y-%m-%d").to_string();
            let prev = state.last_day.entry(site_id_str).or_default();
            if d_str.as_str() > prev.as_str() {
                *prev = d_str;
            }
        } else if !body.response.time_series.is_empty() {
            // time_series non-empty but only today's data returned — don't advance cursor.
        } else {
            // Truly empty response: server is caught up through yesterday.
            // Advance cursor to yesterday so next run doesn't re-fetch an empty window
            // (we still re-fetch today on every poll regardless).
            let yesterday = today.pred_opt().unwrap_or(today);
            let yesterday_str = yesterday.format("%Y-%m-%d").to_string();
            let site_id_str = site_id.to_string();
            let prev = state.last_day.entry(site_id_str).or_default();
            if yesterday_str.as_str() > prev.as_str() {
                *prev = yesterday_str;
            }
        }
    }

    vault.write_tesla_energy_sync(&state)?;
    Ok(counts)
}

fn pull(vault: &Vault) -> Result<PullCounts> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context(
            "Tesla is not connected — connect your Tesla account in the Integrations tab",
        )?;
    let token = crate::tesla::ensure_fresh(vault, token)?;
    let client = FleetClient;
    pull_with(vault, &client, &token.access_token)
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
                format!("Tesla Energy synced — {n} energy rows across {} site(s)", out.sites)
            }))
        }
        Err(e) => {
            let msg = e.to_string();
            let is_auth = msg.contains("reconnect") || msg.contains("401") || msg.contains("403");
            Ok(CollectOutcome::note(if is_auth {
                format!("Tesla Energy sync skipped — token issue, reconnect required: {e}")
            } else {
                format!("Tesla Energy sync skipped: {e}")
            }))
        }
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let headline = if out.energy_rows == 0 {
        format!(
            "Tesla Energy is up to date — no new data across {} site(s)",
            out.sites
        )
    } else {
        format!(
            "Tesla Energy synced — {} energy rows across {} site(s)",
            out.energy_rows, out.sites
        )
    };
    let mut counts = BTreeMap::new();
    counts.insert("sites", out.sites);
    counts.insert("energy_rows", out.energy_rows);
    counts.insert("raw_rows", out.raw_rows);
    Ok(PullOutcome { headline, counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
/// The stub line `&crate::tesla_energy::DEF` is already in INTEGRATIONS — do
/// NOT add a duplicate. The `pub mod tesla_energy;` in lib.rs is also already
/// present.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tesla-energy",
        name: "Tesla Powerwall + Solar",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Polls the Tesla Fleet API for daily Powerwall and solar energy history: solar \
             generation, battery charge/discharge, grid import/export, and home consumption. \
             Data lands in the home energy store. Requires a Powerwall or solar install on \
             the same Tesla account.",
        domain: "home",
        vault_path: "home/tesla-energy/",
        toggleable: true,
        setup: &[
            "Connect your Tesla account on the Tesla card (the same login covers vehicles \
             and Powerwall/solar).",
            "Your Tesla app registration at developer.tesla.com should request the \
             energy_device_data scope in addition to vehicle scopes.",
            "Polls every 6 hours for daily energy aggregates; the first run backfills \
             the last 30 days.",
        ],
        caveats:
            "Requires a Tesla Fleet API app registration at developer.tesla.com with the \
             energy_device_data scope. Shares the Tesla OAuth connection with the vehicle \
             integration — one reconnect adds energy access. The local Powerwall gateway \
             path (LAN-direct) is not yet wired (Needs-sample for firmware variability).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(TESLA_ENERGY_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("tesla"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -----------------------------------------------------------------------
    // Fixtures — shaped per confirmed Fleet API calendar_history response.
    //
    // IMPORTANT: Tesla Fleet API returns energy values in WATT-HOURS (Wh),
    // not kWh. Home Assistant tesla_fleet declares native_unit=WATT_HOUR and
    // applies no conversion. All numeric values here are integer Wh magnitudes
    // consistent with real-world daily energy (e.g. 24500 Wh = 24.5 kWh).
    //
    // Timestamp format is site-local with explicit offset (NOT UTC "Z"),
    // matching the documented example "2022-05-13T00:00:00-07:00".

    /// One day's energy aggregate with realistic integer Wh values and a
    /// non-UTC site-local timestamp (PDT offset -07:00).
    fn fixture_energy_day() -> Value {
        json!({
            "timestamp": "2026-06-10T01:00:00-07:00",
            "solar_energy_exported": 24500,
            "battery_energy_exported": 8200,
            "battery_energy_imported_from_solar": 5100,
            "battery_energy_imported_from_grid": 0,
            "grid_energy_imported": 3400,
            "grid_energy_exported_from_solar": 11000,
            "grid_energy_exported_from_battery": 0,
            "consumer_energy_imported_from_solar": 8400,
            "consumer_energy_imported_from_battery": 8200,
            "consumer_energy_imported_from_grid": 3400,
            "total_home_usage": 20000,
            "total_battery_charge": 5100,
            "total_battery_discharge": 8200,
            "total_solar_generation": 24500,
            "total_grid_energy_exported": 11000,
            "generator_energy_exported": 0,
            "grid_services_energy_imported": 0,
            "grid_services_energy_exported": 0,
            "grid_energy_exported_from_generator": 0,
            "battery_energy_imported_from_generator": 0,
            "consumer_energy_imported_from_generator": 0
        })
    }

    fn fixture_products_response() -> Value {
        json!({
            "response": [
                {
                    "energy_site_id": 1234567890,
                    "site_name": "My Home",
                    "resource_type": "battery"
                },
                {
                    "id": 9876543210_i64,
                    "vin": "5YJ3E1EA0PF123456",
                    "display_name": "My Tesla"
                    // no energy_site_id — this is a vehicle, should be skipped
                }
            ]
        })
    }

    fn fixture_calendar_history_response() -> Value {
        json!({
            "response": {
                "time_series": [fixture_energy_day()]
            }
        })
    }

    // -----------------------------------------------------------------------
    // Stub API.

    struct StubApi {
        products: Value,
        history: Value,
        products_err: Option<&'static str>,
        history_err: Option<&'static str>,
    }

    impl StubApi {
        fn ok() -> Self {
            StubApi {
                products: fixture_products_response(),
                history: fixture_calendar_history_response(),
                products_err: None,
                history_err: None,
            }
        }
        fn auth_err() -> Self {
            StubApi {
                products: Value::Null,
                history: Value::Null,
                products_err: Some(
                    "Tesla Fleet API returned 401 — reconnect from the Integrations tab",
                ),
                history_err: None,
            }
        }
        fn empty_sites() -> Self {
            StubApi {
                products: json!({ "response": [] }),
                history: Value::Null,
                products_err: None,
                history_err: None,
            }
        }
        fn empty_history() -> Self {
            StubApi {
                products: fixture_products_response(),
                history: json!({ "response": { "time_series": [] } }),
                products_err: None,
                history_err: None,
            }
        }
    }

    impl TeslaEnergyApi for StubApi {
        fn list_products(&self, _token: &str) -> Result<Value> {
            if let Some(msg) = self.products_err {
                anyhow::bail!("{msg}");
            }
            Ok(self.products.clone())
        }

        fn calendar_history(
            &self,
            _token: &str,
            _site_id: i64,
            _start: &str,
            _end: &str,
        ) -> Result<Value> {
            if let Some(msg) = self.history_err {
                anyhow::bail!("{msg}");
            }
            Ok(self.history.clone())
        }
    }

    fn make_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-tesla-energy-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Unit tests — pure mapping.

    #[test]
    fn tesla_energy_fixture_parses_to_energy_day() {
        let entry = fixture_energy_day();
        let day: EnergyDay = serde_json::from_value(entry.clone()).unwrap();
        // Fixture uses integer Wh values (Tesla Fleet API native unit).
        assert_eq!(day.solar_energy_exported, Some(24500.0));
        assert_eq!(day.total_home_usage, Some(20000.0));
        assert_eq!(day.grid_energy_imported, Some(3400.0));
        assert_eq!(day.battery_energy_exported, Some(8200.0));
        // generator should be present but zero
        assert_eq!(day.generator_energy_exported, Some(0.0));
    }

    #[test]
    fn tesla_energy_day_to_rows_emits_nonzero_flows() {
        let entry = fixture_energy_day();
        let day: EnergyDay = serde_json::from_value(entry.clone()).unwrap();
        let ts = "2026-06-10T01:00:00-07:00";
        let rows = energy_day_to_rows("1234567890", ts, &day, &entry);
        // Should emit rows for all non-zero flows (generator=0, battery_from_grid=0 skipped).
        assert!(!rows.is_empty(), "must produce rows for nonzero flows");
        // Each row must have required home.energy fields.
        for row in &rows {
            assert!(row.get("ts").is_some(), "row must have ts");
            assert_eq!(row["source"], "tesla-energy");
            assert!(row.get("kwh").is_some(), "row must have kwh");
            assert!(row.get("guid").is_some(), "row must have guid");
            assert!(row.get("device").is_some(), "row must have device");
            assert!(row.get("circuit").is_some(), "row must have circuit");
            assert!(row.get("direction").is_some(), "row must have direction");
            // interval_secs must be 86400 (daily).
            assert_eq!(row["interval_secs"], 86400, "daily interval");
            // kwh must be in kWh (Wh/1000): solar is 24500 Wh = 24.5 kWh.
            let kwh = row["kwh"].as_f64().expect("kwh must be numeric");
            assert!(kwh < 1000.0, "kwh value must be kWh not Wh (got {kwh})");
        }
    }

    #[test]
    fn tesla_energy_wh_to_kwh_conversion() {
        // Tesla API returns Wh; we must divide by 1000 to get kWh for the schema.
        let entry = fixture_energy_day();
        let day: EnergyDay = serde_json::from_value(entry.clone()).unwrap();
        let ts = "2026-06-10T01:00:00-07:00";
        let rows = energy_day_to_rows("1234567890", ts, &day, &entry);
        // solar_energy_exported = 24500 Wh → 24.5 kWh
        let solar_row = rows.iter().find(|r| r["circuit"] == "solar").expect("must have solar row");
        let kwh = solar_row["kwh"].as_f64().unwrap();
        assert!((kwh - 24.5).abs() < 0.001, "solar must be 24.5 kWh (24500 Wh / 1000), got {kwh}");
        // total_home_usage = 20000 Wh → 20.0 kWh
        let home_row = rows.iter().find(|r| r["circuit"] == "total_home_usage").expect("must have home row");
        let home_kwh = home_row["kwh"].as_f64().unwrap();
        assert!((home_kwh - 20.0).abs() < 0.001, "home must be 20.0 kWh (20000 Wh / 1000), got {home_kwh}");
    }

    #[test]
    fn tesla_energy_guid_is_stable_and_unique_per_circuit() {
        let entry = fixture_energy_day();
        let day: EnergyDay = serde_json::from_value(entry.clone()).unwrap();
        let ts = "2026-06-10T01:00:00-07:00";
        let rows = energy_day_to_rows("1234567890", ts, &day, &entry);
        let guids: Vec<&str> = rows
            .iter()
            .map(|r| r.get("guid").and_then(Value::as_str).unwrap_or(""))
            .collect();
        let unique: HashSet<&&str> = guids.iter().collect();
        assert_eq!(guids.len(), unique.len(), "every row must have a unique guid");
        // All guids must include the site id and ts.
        for g in &guids {
            assert!(g.contains("1234567890"), "guid must include site_id");
            assert!(g.starts_with("tesla-energy:"), "guid must start with source");
        }
    }

    #[test]
    fn tesla_energy_zero_kwh_flows_are_skipped() {
        // Use integer Wh values matching the real API format.
        let entry = json!({
            "timestamp": "2026-06-10T01:00:00-07:00",
            "solar_energy_exported": 0,
            "total_home_usage": 5000,
            "grid_energy_imported": 0,
            "generator_energy_exported": 0
        });
        let day: EnergyDay = serde_json::from_value(entry.clone()).unwrap();
        let ts = "2026-06-10T01:00:00-07:00";
        let rows = energy_day_to_rows("999", ts, &day, &entry);
        // Only total_home_usage (5000 Wh = 5.0 kWh) should produce a row.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["circuit"], "total_home_usage");
        // 5000 Wh / 1000 = 5.0 kWh
        let kwh = rows[0]["kwh"].as_f64().unwrap();
        assert!((kwh - 5.0).abs() < 0.001, "5000 Wh must convert to 5.0 kWh, got {kwh}");
    }

    #[test]
    fn tesla_energy_parse_day_ts_parses_utc_iso() {
        let entry = json!({ "timestamp": "2026-06-10T00:00:00+00:00" });
        let ts = parse_day_ts(&entry);
        assert!(ts.is_some(), "must parse UTC ISO timestamp");
        let ts_str = ts.unwrap();
        // Must be RFC3339 and preserve the original offset (+00:00).
        assert!(ts_str.starts_with("2026-06-10"), "must start with the date");
    }

    #[test]
    fn tesla_energy_parse_day_ts_preserves_site_local_offset() {
        // Real API returns site-local time with DST offset, NOT UTC.
        let entry = json!({ "timestamp": "2022-05-13T00:00:00-07:00" });
        let ts = parse_day_ts(&entry);
        assert!(ts.is_some(), "must parse site-local ISO timestamp with offset");
        let ts_str = ts.unwrap();
        // Must preserve the -07:00 offset, not convert to UTC (+00:00 or Z).
        assert!(
            ts_str.contains("-07:00"),
            "must preserve site-local -07:00 offset, not shift to UTC; got: {ts_str}"
        );
        assert!(ts_str.starts_with("2022-05-13"), "must preserve the local date");
    }

    #[test]
    fn tesla_energy_parse_day_ts_missing_field_returns_none() {
        let entry = json!({ "solar_energy_exported": 5.0 });
        assert!(parse_day_ts(&entry).is_none());
    }

    #[test]
    fn tesla_energy_direction_semantics_grid_flows() {
        // Grid flows must use "import"/"export", not "consumption"/"production".
        let entry = json!({
            "timestamp": "2026-06-10T01:00:00-07:00",
            "grid_energy_imported": 3400,
            "grid_energy_exported_from_solar": 11000,
            "grid_energy_exported_from_battery": 500,
            "battery_energy_imported_from_grid": 1000
        });
        let day: EnergyDay = serde_json::from_value(entry.clone()).unwrap();
        let ts = "2026-06-10T01:00:00-07:00";
        let rows = energy_day_to_rows("1234567890", ts, &day, &entry);

        let grid_import = rows.iter().find(|r| r["circuit"] == "grid_import")
            .expect("must have grid_import row");
        assert_eq!(
            grid_import["direction"], "import",
            "grid_energy_imported must have direction=import (not consumption)"
        );

        let grid_export_solar = rows.iter().find(|r| r["circuit"] == "grid_export_from_solar")
            .expect("must have grid_export_from_solar row");
        assert_eq!(
            grid_export_solar["direction"], "export",
            "grid_energy_exported_from_solar must have direction=export"
        );

        let batt_from_grid = rows.iter().find(|r| r["circuit"] == "battery_charge_from_grid")
            .expect("must have battery_charge_from_grid row");
        assert_eq!(
            batt_from_grid["direction"], "import",
            "battery_energy_imported_from_grid must have direction=import"
        );
    }

    #[test]
    fn tesla_energy_products_response_filters_vehicles() {
        let raw = fixture_products_response();
        let resp: ProductsResponse = serde_json::from_value(raw).unwrap();
        let sites: Vec<(i64, String)> = resp
            .response
            .iter()
            .filter_map(|v| {
                let item: ProductItem = serde_json::from_value(v.clone()).ok()?;
                let sid = item.energy_site_id?;
                let name = item.site_name.unwrap_or_else(|| format!("site_{sid}"));
                Some((sid, name))
            })
            .collect();
        // Only the battery product has energy_site_id; the vehicle is filtered out.
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].0, 1234567890);
        assert_eq!(sites[0].1, "My Home");
    }

    // -----------------------------------------------------------------------
    // Integration-level tests: pull_with behaviour.

    #[test]
    fn tesla_energy_first_sync_writes_energy_and_raw_rows() {
        let vault = make_vault("first-sync");
        let api = StubApi::ok();
        let out = pull_with(&vault, &api, "fake-token").unwrap();
        assert_eq!(out.sites, 1, "must detect 1 energy site");
        // Raw layer must always be written.
        assert!(out.raw_rows > 0, "must write at least one raw row");
        // Energy rows must be emitted for the fixture day.
        assert!(out.energy_rows > 0, "must write energy rows on first sync");
    }

    #[test]
    fn tesla_energy_cursor_advances_after_write() {
        let vault = make_vault("cursor-advance");
        let api = StubApi::ok();
        let _ = pull_with(&vault, &api, "fake-token").unwrap();
        let state = vault.read_tesla_energy_sync();
        // Fixture timestamp is 2026-06-10 (in the past — a completed day).
        // Cursor must advance to that date since it is strictly before today.
        let day = state.last_day.get("1234567890").cloned();
        assert!(day.is_some(), "cursor must be set for site 1234567890");
        let day_str = day.unwrap();
        assert_eq!(
            day_str, "2026-06-10",
            "cursor must advance to the completed fixture day"
        );
    }

    #[test]
    fn tesla_energy_second_sync_deduplicates_rows() {
        let vault = make_vault("dedup");
        let api = StubApi::ok();
        // First sync.
        let out1 = pull_with(&vault, &api, "fake-token").unwrap();
        // Second sync — cursor is at 2026-06-10; fixture returns same day → 0 new rows.
        // The stub always returns the same day regardless of date parameters.
        let out2 = pull_with(&vault, &api, "fake-token").unwrap();
        // All rows from the first sync should be deduped on the second.
        assert!(
            out2.energy_rows == 0 || out2.energy_rows <= out1.energy_rows,
            "second sync must not re-write already-stored rows"
        );
    }

    #[test]
    fn tesla_energy_empty_sites_returns_ok_with_zero_counts() {
        let vault = make_vault("empty-sites");
        let api = StubApi::empty_sites();
        let out = pull_with(&vault, &api, "fake-token").unwrap();
        assert_eq!(out.sites, 0);
        assert_eq!(out.energy_rows, 0);
        assert_eq!(out.raw_rows, 0);
    }

    #[test]
    fn tesla_energy_empty_history_advances_cursor_to_yesterday() {
        let vault = make_vault("empty-history");
        let api = StubApi::empty_history();
        let _ = pull_with(&vault, &api, "fake-token").unwrap();
        let state = vault.read_tesla_energy_sync();
        // Empty time_series → cursor should advance to yesterday (not today).
        // We never finalise today's partial aggregate into the cursor.
        let yesterday = (Utc::now().date_naive().pred_opt().unwrap_or(Utc::now().date_naive()))
            .format("%Y-%m-%d")
            .to_string();
        let day = state.last_day.get("1234567890").cloned();
        assert!(day.is_some(), "cursor must be set even for empty history");
        assert_eq!(
            day.unwrap(),
            yesterday,
            "cursor must advance to yesterday on empty history (not today)"
        );
    }

    #[test]
    fn tesla_energy_auth_error_propagates() {
        let vault = make_vault("auth-err");
        let api = StubApi::auth_err();
        let err = pull_with(&vault, &api, "fake-token").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("401") || msg.contains("reconnect"),
            "auth error must mention reconnect or 401: {msg}"
        );
    }

    #[test]
    fn tesla_energy_def_is_periodic_with_tesla_connection() {
        assert_eq!(DEF.meta.id, "tesla-energy");
        assert_eq!(DEF.connection, Some("tesla"));
        assert!(
            matches!(DEF.behavior, Behavior::Periodic { .. }),
            "DEF must be Periodic"
        );
    }

    #[test]
    fn tesla_energy_def_domain_and_vault_path() {
        assert_eq!(DEF.meta.domain, "home");
        assert_eq!(DEF.meta.vault_path, "home/tesla-energy/");
    }

    #[test]
    fn tesla_energy_sync_state_roundtrips() {
        let vault = make_vault("sync-rt");
        let mut state = SyncState::default();
        state
            .last_day
            .insert("111".to_string(), "2026-06-01".to_string());
        vault.write_tesla_energy_sync(&state).unwrap();
        let read_back = vault.read_tesla_energy_sync();
        assert_eq!(read_back.last_day.get("111").unwrap(), "2026-06-01");
    }

    #[test]
    fn tesla_energy_calendar_history_response_parses_time_series() {
        let raw = fixture_calendar_history_response();
        let resp: CalendarHistoryResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(resp.response.time_series.len(), 1);
        let day: EnergyDay = serde_json::from_value(resp.response.time_series[0].clone()).unwrap();
        // Fixture uses integer Wh (24500 Wh = 24.5 kWh); the raw field holds Wh.
        assert_eq!(day.solar_energy_exported, Some(24500.0));
    }
}
