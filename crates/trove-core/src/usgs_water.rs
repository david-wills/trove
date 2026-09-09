//! USGS Water Services — stream gauge readings and flood-stage categories
//! from the modernized `api.waterdata.usgs.gov` OGC API.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/usgs-water.md.
//!
//! A **Periodic**, keyless, no-login collector. It writes the **`environment`**
//! domain's bound reading shape:
//!
//! - **gauge readings → [`EnvReading`]**
//!   (`environment/usgs-water/YYYY-MM.jsonl`, month of `ts`) — one row per
//!   observation per parameter (gage height `water_level`/`ft`; discharge
//!   `discharge`/`cfs`). Flood-stage category and raw thresholds ride in `extra`.
//!
//! A **raw layer** (`environment/usgs-water/raw/YYYY-MM.jsonl`) keeps the full
//! GeoJSON feature objects verbatim — full fidelity, never dropped.
//!
//! **Keyless — no `User-Agent` mandate**, unlike api.weather.gov; the OGC API
//! returns standard GeoJSON with a `?f=json` format param. A `User-Agent` is
//! sent anyway as good citizenship. US-only; outside the US (no bbox overlap)
//! the monitoring-location discovery returns zero sites and the collector is
//! silently inert.
//!
//! **Collection strategy:**
//! 1. Discover nearest stream-gauge sites within ~50 km of the user's location
//!    via the `monitoring-locations` collection (bbox filter on geometry).
//! 2. For each site, fetch the latest gage-height (param `00065`) and discharge
//!    (param `00060`) observations from `latest-continuous`, using a CQL filter.
//! 3. For each time-series that has a gage-height reading, attempt to fetch
//!    flood-stage thresholds from `time-series-metadata` so flood category can
//!    be annotated in `extra` (best-effort; absent thresholds are not an error).
//! 4. Write contract readings + raw objects; advance the cursor.
//!
//! **Cursor:** a JSON file at `.trove/usgs-water-sync.json` persists the last
//! location used (for the ladder fallback) and last poll time. Losing it is
//! free — the next pass re-derives everything.
//!
//! **Dedup:** guid = `usgs-water:<monitoring_location_id>:<parameter_code>:<time>`
//! (matches the environment domain spec example). Upsert-by-guid so a re-poll
//! of a still-current reading updates in place without duplicating.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvReading;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Contract-layer reading directory; raw objects go one level deeper.
const DIR: &str = "environment/usgs-water";
const RAW_DIR: &str = "environment/usgs-water/raw";
/// Rebuildable cursor — last location + last poll time. Deleting costs nothing.
const SYNC_FILE: &str = ".trove/usgs-water-sync.json";

const SOURCE: &str = "usgs-water";
const API_BASE: &str = "https://api.waterdata.usgs.gov/ogcapi/v0";
const USER_AGENT: &str = "Trove (https://trove.app)";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// ~50 km in degrees (rough; used for bbox discovery). One degree of latitude
/// ≈ 111 km, so 0.45° ≈ 50 km is a safe approximation at US latitudes.
const SEARCH_DEG: f64 = 0.45;

/// Maximum sites to poll per pass (guard against very dense gauge areas).
const MAX_SITES: usize = 5;

/// Poll no more often than every 30 minutes (gauges update every 15 min).
pub const USGS_WATER_SYNC_SECS: u64 = 1800;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_usgs_water_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join(DIR)))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        // A manual weather location works without the grant.
        required: false,
    }
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(
                n > 0,
                || format!("usgs-water synced — {n} gauge readings"),
            ))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("usgs-water sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "USGS Water: nothing new (no US gauge in range or no location set)".to_string()
    } else {
        format!("USGS Water: {n} gauge readings written")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. Keyless — no
/// `connection`. ~30-minute cadence; inert until a location exists.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "usgs-water",
        name: "USGS Water Data",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Records stream gauge readings and official flood-stage categories \
                      (action, flood, moderate, major) from the nearest USGS water gauge \
                      via the modernized Water Resources API.",
        domain: "environment",
        vault_path: "environment/usgs-water/",
        toggleable: true,
        setup: &[
            "No account or key — this uses the keyless public api.waterdata.usgs.gov.",
            "Approve Location Services when asked, or set a location manually on the Weather \
             tab; without a location this stays inert.",
        ],
        caveats: "US only; uses the modern api.waterdata.usgs.gov OGC API. Outside the US, \
                  no gauge is found and no rows are written (no error).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(USGS_WATER_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct UsgsWaterSyncState {
    /// RFC3339 local time of the last pass that completed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Location last used successfully (ladder fallback).
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    /// Non-empty while stuck (no location, network error) — for the UI banner.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_usgs_water_sync(&self) -> Option<UsgsWaterSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_usgs_water_sync(&self, state: &UsgsWaterSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

trait UsgsApi {
    /// Discover stream-gauge monitoring locations within a bounding box.
    /// `bbox` = (min_lon, min_lat, max_lon, max_lat).
    fn monitoring_locations(&self, bbox: (f64, f64, f64, f64)) -> Result<Value>;

    /// Latest observation for a monitoring location and parameter code.
    fn latest_observation(&self, monitoring_location_id: &str, parameter_code: &str)
        -> Result<Value>;

    /// Time-series metadata (flood-stage thresholds) for a monitoring location
    /// and parameter code. Best-effort: `Ok(Value::Null)` on any error.
    fn time_series_metadata(
        &self,
        monitoring_location_id: &str,
        parameter_code: &str,
    ) -> Result<Value>;
}

struct UsgsClient {
    base: String,
}

impl UsgsClient {
    fn new(base: String) -> Self {
        UsgsClient { base }
    }

    fn req(url: &str) -> ureq::Request {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/geo+json, application/json")
    }
}

impl UsgsApi for UsgsClient {
    fn monitoring_locations(&self, (min_lon, min_lat, max_lon, max_lat): (f64, f64, f64, f64)) -> Result<Value> {
        // CQL filter: site_type_code = 'ST' (stream) within bbox.
        let bbox_str = format!("{min_lon},{min_lat},{max_lon},{max_lat}");
        let filter = "site_type_code='ST'";
        let url = format!(
            "{}/collections/monitoring-locations/items?f=json&limit={MAX_SITES}&bbox={bbox_str}&filter={filter}",
            self.base
        );
        Self::req(&url)
            .call()
            .context("requesting USGS monitoring locations")?
            .into_json()
            .context("reading USGS monitoring-locations response")
    }

    fn latest_observation(
        &self,
        monitoring_location_id: &str,
        parameter_code: &str,
    ) -> Result<Value> {
        // CQL filter for this exact monitoring location + parameter code.
        let filter = format!(
            "monitoring_location_id='{monitoring_location_id}' AND parameter_code='{parameter_code}'"
        );
        let url = format!(
            "{}/collections/latest-continuous/items?f=json&limit=1&filter={}",
            self.base,
            urlencoding::encode(&filter)
        );
        Self::req(&url)
            .call()
            .context("requesting USGS latest observation")?
            .into_json()
            .context("reading USGS latest-observation response")
    }

    fn time_series_metadata(
        &self,
        monitoring_location_id: &str,
        parameter_code: &str,
    ) -> Result<Value> {
        let filter = format!(
            "monitoring_location_id='{monitoring_location_id}' AND parameter_code='{parameter_code}'"
        );
        let url = format!(
            "{}/collections/time-series-metadata/items?f=json&limit=1&filter={}",
            self.base,
            urlencoding::encode(&filter)
        );
        match Self::req(&url).call() {
            Ok(resp) => resp.into_json().context("reading USGS time-series-metadata response"),
            // Non-fatal: thresholds are best-effort.
            Err(_) => Ok(Value::Null),
        }
    }
}

// Simple percent-encoding for the filter string in the URL query param.
// Only encodes the characters that break URL query strings.
mod urlencoding {
    pub fn encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 16);
        for c in s.chars() {
            match c {
                // Unreserved chars in query strings.
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' | '=' | ',' | '\'' => {
                    out.push(c);
                }
                ' ' => out.push('+'),
                c => {
                    for byte in c.to_string().as_bytes() {
                        out.push_str(&format!("%{byte:02X}"));
                    }
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Map API parameter_code to the contract metric name and unit.
fn metric_for_param(code: &str) -> Option<(&'static str, &'static str)> {
    match code {
        "00065" => Some(("water_level", "ft")),
        "00060" => Some(("discharge", "cfs")),
        _ => None,
    }
}

/// RFC3339 UTC → local-offset RFC3339. The OGC API returns UTC timestamps
/// (`+00:00`); we re-render via the local timezone so the vault's `ts` is
/// consistent with every other collector. An unparseable value passes through.
fn to_local(s: &str) -> String {
    // Try RFC3339 with offset first (what the API returns: "2026-06-21T21:20:00+00:00").
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// A non-empty trimmed string from `obj[key]`, or "".
fn str_field(obj: &Value, key: &str) -> String {
    obj.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Parse a `/monitoring-locations` FeatureCollection → site ids + names + coords.
fn parse_sites(body: &Value) -> Vec<SiteInfo> {
    let features = match body.get("features").and_then(Value::as_array) {
        Some(f) => f.clone(),
        None => return Vec::new(),
    };
    let mut sites = Vec::new();
    for feat in features {
        let props = feat.get("properties").cloned().unwrap_or(Value::Null);
        let agency_code = str_field(&props, "agency_code");
        let loc_num = str_field(&props, "monitoring_location_number");
        if loc_num.is_empty() {
            continue;
        }
        // The monitoring_location_id used in query filters is "<agency>-<number>".
        let monitoring_location_id = if agency_code.is_empty() {
            loc_num.clone()
        } else {
            format!("{agency_code}-{loc_num}")
        };
        let name = str_field(&props, "monitoring_location_name");
        // Coordinates: GeoJSON [lon, lat].
        let (lat, lon) = feat
            .get("geometry")
            .and_then(|g| g.get("coordinates"))
            .and_then(Value::as_array)
            .and_then(|c| {
                let lon = c.first().and_then(Value::as_f64)?;
                let lat = c.get(1).and_then(Value::as_f64)?;
                Some((lat, lon))
            })
            .unwrap_or((0.0, 0.0));
        sites.push(SiteInfo { monitoring_location_id, name, lat, lon });
    }
    sites
}

struct SiteInfo {
    monitoring_location_id: String,
    name: String,
    lat: f64,
    lon: f64,
}

/// Parse one `latest-continuous` FeatureCollection → the first observation.
/// Returns (contract EnvReading, raw feature Value) or None if nothing usable.
fn parse_observation(
    body: &Value,
    site: &SiteInfo,
    flood_stage: Option<&str>,
    thresholds_raw: Option<&Value>,
) -> Option<(EnvReading, Value)> {
    let features = body.get("features").and_then(Value::as_array)?;
    let feat = features.first()?;
    let props = feat.get("properties").cloned().unwrap_or(Value::Null);

    let parameter_code = str_field(&props, "parameter_code");
    let (metric, unit) = metric_for_param(&parameter_code)?;

    let time_raw = str_field(&props, "time");
    if time_raw.is_empty() {
        return None;
    }
    let ts = to_local(&time_raw);

    // value is a string in the API ("24.98"), parse to f64.
    let value_str = str_field(&props, "value");
    let value: f64 = value_str.parse().ok()?;

    // unit_of_measure from API (e.g. "ft", "ft³/s") — use the canonical contract
    // unit from metric_for_param ("ft", "cfs") for consistency in the contract
    // layer; keep the raw API unit in the raw layer.
    let _ = str_field(&props, "unit_of_measure"); // kept verbatim in raw via feat

    let monitoring_location_id = str_field(&props, "monitoring_location_id");
    // guid = source:monitoring_location_id:parameter_code:time (following the
    // environment domain spec example).
    let guid = format!("{SOURCE}:{monitoring_location_id}:{parameter_code}:{time_raw}");

    let mut extra: Map<String, Value> = Map::new();
    extra.insert("parameter_code".into(), Value::String(parameter_code.clone()));
    extra.insert(
        "monitoring_location_id".into(),
        Value::String(monitoring_location_id.clone()),
    );
    let approval = str_field(&props, "approval_status");
    if !approval.is_empty() {
        extra.insert("approval_status".into(), Value::String(approval));
    }
    // Flood-stage annotation (best-effort).
    if let Some(stage) = flood_stage {
        if !stage.is_empty() {
            extra.insert("flood_stage".into(), Value::String(stage.to_string()));
        }
    }
    if let Some(thresh) = thresholds_raw {
        if !thresh.is_null() {
            extra.insert("flood_thresholds".into(), thresh.clone());
        }
    }

    let lat = if site.lat != 0.0 { Some(site.lat) } else { None };
    let lon = if site.lon != 0.0 { Some(site.lon) } else { None };

    let reading = EnvReading {
        ts,
        source: SOURCE.into(),
        metric: metric.into(),
        value,
        unit: unit.into(),
        place: site.name.clone(),
        lat,
        lon,
        station: monitoring_location_id,
        guid: Some(guid),
        extra,
    };
    Some((reading, feat.clone()))
}

/// Parse thresholds from a `time-series-metadata` response.
/// Returns (flood_stage_label, raw_thresholds_array).
/// `flood_stage_label` is the NWS stage category the current value falls into
/// (if the gage-height value is provided), or "none" if below all stages.
/// Returns (None, None) when no threshold data is available.
fn parse_thresholds(metadata: &Value, value: f64) -> (Option<String>, Option<Value>) {
    let features = match metadata.get("features").and_then(Value::as_array) {
        Some(f) => f,
        None => return (None, None),
    };
    let feat = match features.first() {
        Some(f) => f,
        None => return (None, None),
    };
    let props = match feat.get("properties") {
        Some(p) => p,
        None => return (None, None),
    };
    let thresholds = match props.get("thresholds").and_then(Value::as_array) {
        Some(t) if !t.is_empty() => t,
        _ => return (None, None),
    };

    // Build a list of (name, reference_value) for NWS thresholds above-type.
    // Threshold object keys are PascalCase: Name, Type, Periods[].ReferenceValue.
    let mut stages: Vec<(String, f64)> = Vec::new();
    for t in thresholds {
        let name = t.get("Name").and_then(Value::as_str).unwrap_or("").trim().to_lowercase();
        // Only "ThresholdAbove" direction is meaningful for flood stages.
        let typ = t.get("Type").and_then(Value::as_str).unwrap_or("").trim();
        if typ != "ThresholdAbove" {
            continue;
        }
        // Get the most-recent period's ReferenceValue.
        let ref_val = t
            .get("Periods")
            .and_then(Value::as_array)
            .and_then(|ps| ps.last())
            .and_then(|p| p.get("ReferenceValue"))
            .and_then(Value::as_f64);
        if let Some(rv) = ref_val {
            // Normalize NWS stage names.
            let stage_key = if name.contains("major") {
                "major"
            } else if name.contains("moderate") {
                "moderate"
            } else if name.contains("flood stage") || name.starts_with("flood") {
                "flood"
            } else if name.contains("action") {
                "action"
            } else {
                continue; // Not a recognized flood stage
            };
            stages.push((stage_key.to_string(), rv));
        }
    }

    if stages.is_empty() {
        return (None, Some(Value::Array(thresholds.to_vec())));
    }

    // Sort ascending by threshold value.
    stages.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

    // Find the highest stage the current value exceeds.
    let label = stages
        .iter()
        .filter(|(_, rv)| value >= *rv)
        .last()
        .map(|(name, _)| name.clone())
        .unwrap_or_else(|| "below_action".to_string());

    (Some(label), Some(Value::Array(thresholds.to_vec())))
}

// ---------------------------------------------------------------------------
// Store helpers — upsert by guid.

/// Wrapper carrying ts + guid for the raw-line upsert.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(skip)]
    guid: String,
    #[serde(flatten)]
    value: Value,
}

/// Upsert contract readings into `environment/usgs-water/YYYY-MM.jsonl` by guid.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvReading>> = Default::default();
    for r in rows {
        if let Some(key) = Partition::Month.key(&r.ts) {
            by_key.entry(key.to_string()).or_default().push(r);
        }
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<EnvReading> = stream.read(&key)?;
        for row in incoming {
            let slot = existing
                .iter_mut()
                .find(|e| e.guid.is_some() && e.guid == row.guid);
            match slot {
                Some(s) => *s = row.clone(),
                None => existing.push(row.clone()),
            }
            written += 1;
        }
        vault.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(written)
}

/// Upsert raw feature objects into `environment/usgs-water/raw/YYYY-MM.jsonl`.
fn upsert_raw(vault: &Vault, lines: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: BTreeMap<String, Vec<&RawLine>> = Default::default();
    for l in lines {
        if let Some(key) = Partition::Month.key(&l.ts) {
            by_key.entry(key.to_string()).or_default().push(l);
        }
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<Value> = stream.read(&key)?;
        for l in incoming {
            let pos = existing.iter().position(|v| raw_guid(v) == l.guid);
            match pos {
                Some(i) => existing[i] = l.value.clone(),
                None => existing.push(l.value.clone()),
            }
        }
        vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(())
}

/// Guid of a stored raw feature: `properties.monitoring_location_id` +
/// `properties.parameter_code` + `properties.time`.
fn raw_guid(v: &Value) -> String {
    let props = match v.get("properties") {
        Some(p) => p,
        None => return String::new(),
    };
    let mlid = str_field(props, "monitoring_location_id");
    let code = str_field(props, "parameter_code");
    let time = str_field(props, "time");
    if mlid.is_empty() || time.is_empty() {
        return String::new();
    }
    format!("{SOURCE}:{mlid}:{code}:{time}")
}

// ---------------------------------------------------------------------------
// The pull.

/// Production pull: resolves the location via the ladder and syncs.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_usgs_water_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error = "no location: grant Location Services or set a location on the Weather tab"
            .into();
        vault.write_usgs_water_sync(&state)?;
        return Ok(PullOutcome {
            headline: "USGS Water: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }
    pull_at(vault, point)
}

/// Network+write body over an explicit point — the IO seam tests drive directly.
pub fn pull_at(vault: &Vault, point: Option<(f64, f64)>) -> Result<PullOutcome> {
    let client = UsgsClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &client)
}

fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    client: &impl UsgsApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "USGS Water: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    };
    let now = Local::now();
    let mut state = vault.read_usgs_water_sync().unwrap_or_default();

    // 1. Discover stream gauges near the user's location.
    let bbox = (lon - SEARCH_DEG, lat - SEARCH_DEG, lon + SEARCH_DEG, lat + SEARCH_DEG);
    let sites_body = client.monitoring_locations(bbox)?;
    let sites = parse_sites(&sites_body);

    if sites.is_empty() {
        // No US stream gauges nearby — inert, not an error.
        state.updated = now.to_rfc3339();
        state.lat = lat;
        state.lon = lon;
        state.error = String::new();
        vault.write_usgs_water_sync(&state)?;
        return Ok(PullOutcome {
            headline: "USGS Water: no stream gauge within range".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }

    let mut all_readings: Vec<EnvReading> = Vec::new();
    let mut all_raws: Vec<RawLine> = Vec::new();

    // 2. For each site, poll gage height (00065) and discharge (00060).
    for site in sites.iter().take(MAX_SITES) {
        for &param_code in &["00065", "00060"] {
            let obs_body = match client.latest_observation(&site.monitoring_location_id, param_code) {
                Ok(b) => b,
                Err(_) => continue, // network blip — skip this param
            };

            // Check if we got an observation.
            let feat_count = obs_body
                .get("features")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(0);
            if feat_count == 0 {
                continue; // no data for this site+param
            }

            // 3. Best-effort flood-stage thresholds (gage height only).
            let (flood_stage, thresholds_raw) = if param_code == "00065" {
                // Peek at the value to categorize.
                let val: f64 = obs_body
                    .get("features")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(|f| f.get("properties"))
                    .and_then(|p| p.get("value"))
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let meta = client
                    .time_series_metadata(&site.monitoring_location_id, param_code)
                    .unwrap_or(Value::Null);
                let (stage, raw) = parse_thresholds(&meta, val);
                (stage, raw)
            } else {
                (None, None)
            };

            if let Some((reading, raw_feat)) = parse_observation(
                &obs_body,
                site,
                flood_stage.as_deref(),
                thresholds_raw.as_ref(),
            ) {
                let ts = reading.ts.clone();
                let guid = reading.guid.clone().unwrap_or_default();
                all_readings.push(reading);
                all_raws.push(RawLine { ts, guid, value: raw_feat });
            }
        }
    }

    // 4. Write contract + raw layers.
    let mut readings_written = 0u64;
    if !all_readings.is_empty() {
        readings_written = upsert_readings(vault, &all_readings)?;
        upsert_raw(vault, &all_raws)?;
    }

    // 5. Advance cursor.
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.error = String::new();
    vault.write_usgs_water_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("USGS Water: {readings_written} gauge readings"),
        counts: BTreeMap::from([("readings", readings_written)]),
    })
}

/// Location ladder: CoreLocation → manual weather location → last-used cursor.
fn resolve_point(vault: &Vault, state: &UsgsWaterSyncState) -> Option<(f64, f64)> {
    if corelocation::auth_status() == AuthStatus::NotDetermined {
        corelocation::request_access(5);
    }
    if let Some(fix) = corelocation::current_location(8) {
        return Some((fix.lat, fix.lon));
    }
    if let Some(m) = vault.weather_location() {
        return Some((m.lat, m.lon));
    }
    (state.lat != 0.0 || state.lon != 0.0).then_some((state.lat, state.lon))
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-usgs-water-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures modeled on documented api.waterdata.usgs.gov OGC API ------

    /// Monitoring-locations FeatureCollection — two stream gauges (ST).
    /// Shape confirmed from live API (agency_code, monitoring_location_number,
    /// monitoring_location_name, geometry.coordinates [lon, lat]).
    fn sites_near() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": [-121.5008, 38.4564] },
                    "properties": {
                        "agency_code": "USGS",
                        "monitoring_location_number": "11447650",
                        "monitoring_location_name": "SACRAMENTO R A FREEPORT CA",
                        "site_type_code": "ST",
                        "state_name": "California"
                    }
                },
                {
                    "type": "Feature",
                    "geometry": { "type": "Point", "coordinates": [-121.4912, 38.5789] },
                    "properties": {
                        "agency_code": "USGS",
                        "monitoring_location_number": "11447890",
                        "monitoring_location_name": "SACRAMENTO R BL DISCOVERY PARK CA",
                        "site_type_code": "ST",
                        "state_name": "California"
                    }
                }
            ]
        })
    }

    fn sites_empty() -> Value {
        json!({ "type": "FeatureCollection", "features": [] })
    }

    /// Latest-continuous observation — gage height at Sacramento R gauge.
    /// Shape confirmed from live API: monitoring_location_id, parameter_code,
    /// time (UTC offset), value (string), unit_of_measure, approval_status.
    fn obs_gage_height() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [-121.5008, 38.4564] },
                "properties": {
                    "id": "abc-uuid-1",
                    "time_series_id": "ts-abc",
                    "monitoring_location_id": "USGS-11447650",
                    "parameter_code": "00065",
                    "statistic_id": "00011",
                    "time": "2026-06-10T14:00:00+00:00",
                    "value": "12.5",
                    "unit_of_measure": "ft",
                    "approval_status": "Provisional",
                    "qualifier": null,
                    "last_modified": "2026-06-10T14:05:00+00:00"
                }
            }]
        })
    }

    /// Latest-continuous observation — discharge at the same gauge.
    fn obs_discharge() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [-121.5008, 38.4564] },
                "properties": {
                    "id": "abc-uuid-2",
                    "time_series_id": "ts-def",
                    "monitoring_location_id": "USGS-11447650",
                    "parameter_code": "00060",
                    "statistic_id": "00011",
                    "time": "2026-06-10T14:00:00+00:00",
                    "value": "1240",
                    "unit_of_measure": "ft³/s",
                    "approval_status": "Provisional",
                    "qualifier": null,
                    "last_modified": "2026-06-10T14:05:00+00:00"
                }
            }]
        })
    }

    fn obs_empty() -> Value {
        json!({ "type": "FeatureCollection", "features": [] })
    }

    /// Time-series-metadata with NWS flood-stage thresholds (ThresholdAbove).
    /// Shape confirmed from live API: thresholds array with PascalCase keys.
    fn metadata_with_stages() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [-121.5008, 38.4564] },
                "properties": {
                    "monitoring_location_id": "USGS-11447650",
                    "parameter_code": "00065",
                    "parameter_name": "Gage height",
                    "unit_of_measure": "ft",
                    "thresholds": [
                        {
                            "Name": "Action Stage (National Weather Service)",
                            "Type": "ThresholdAbove",
                            "Severity": null,
                            "Description": null,
                            "DisplayColor": "#00CCFF",
                            "ReferenceCode": "NWS-ACT",
                            "ProcessingOrder": 1,
                            "Periods": [{"StartTime": "2017-04-04T00:00:00+00:00", "EndTime": null, "ReferenceValue": 10.0, "SecondaryReferenceValue": null, "SuppressData": false, "AppliedTime": null}]
                        },
                        {
                            "Name": "Flood Stage (National Weather Service)",
                            "Type": "ThresholdAbove",
                            "Severity": null,
                            "Description": null,
                            "DisplayColor": "#FFFF00",
                            "ReferenceCode": "NWS-FLD",
                            "ProcessingOrder": 2,
                            "Periods": [{"StartTime": "2017-04-04T00:00:00+00:00", "EndTime": null, "ReferenceValue": 15.0, "SecondaryReferenceValue": null, "SuppressData": false, "AppliedTime": null}]
                        },
                        {
                            "Name": "Moderate Flood Stage (National Weather Service)",
                            "Type": "ThresholdAbove",
                            "Severity": null,
                            "Description": null,
                            "DisplayColor": "#FF8C00",
                            "ReferenceCode": "NWS-MOD",
                            "ProcessingOrder": 3,
                            "Periods": [{"StartTime": "2017-04-04T00:00:00+00:00", "EndTime": null, "ReferenceValue": 20.0, "SecondaryReferenceValue": null, "SuppressData": false, "AppliedTime": null}]
                        },
                        {
                            "Name": "Major Flood Stage (National Weather Service)",
                            "Type": "ThresholdAbove",
                            "Severity": null,
                            "Description": null,
                            "DisplayColor": "#CC1111",
                            "ReferenceCode": "NWS-MAJ",
                            "ProcessingOrder": 4,
                            "Periods": [{"StartTime": "2017-04-04T00:00:00+00:00", "EndTime": null, "ReferenceValue": 25.0, "SecondaryReferenceValue": null, "SuppressData": false, "AppliedTime": null}]
                        },
                        {
                            "Name": "Operational limit (minimum)",
                            "Type": "ThresholdBelow",
                            "Periods": [{"StartTime": "2017-04-04T00:00:00+00:00", "EndTime": null, "ReferenceValue": 1.0, "SuppressData": true, "AppliedTime": null}]
                        }
                    ]
                }
            }]
        })
    }

    fn metadata_empty() -> Value {
        json!({ "type": "FeatureCollection", "features": [] })
    }

    // --- Configurable stub ---------------------------------------------------

    struct Stub {
        sites: Value,
        gage_height: Value,
        discharge: Value,
        metadata: Value,
    }

    impl UsgsApi for Stub {
        fn monitoring_locations(&self, _bbox: (f64, f64, f64, f64)) -> Result<Value> {
            Ok(self.sites.clone())
        }
        fn latest_observation(&self, _id: &str, param: &str) -> Result<Value> {
            Ok(if param == "00065" {
                self.gage_height.clone()
            } else {
                self.discharge.clone()
            })
        }
        fn time_series_metadata(&self, _id: &str, _param: &str) -> Result<Value> {
            Ok(self.metadata.clone())
        }
    }

    const SEED: (f64, f64) = (38.45, -121.50);

    // --- Parser unit tests --------------------------------------------------

    #[test]
    fn parse_sites_extracts_ids_and_coords() {
        let sites = parse_sites(&sites_near());
        assert_eq!(sites.len(), 2);
        let s = &sites[0];
        assert_eq!(s.monitoring_location_id, "USGS-11447650");
        assert_eq!(s.name, "SACRAMENTO R A FREEPORT CA");
        assert!((s.lat - 38.4564).abs() < 1e-3);
        assert!((s.lon - (-121.5008)).abs() < 1e-3);
    }

    #[test]
    fn parse_sites_empty_gives_no_sites() {
        assert!(parse_sites(&sites_empty()).is_empty());
    }

    #[test]
    fn parse_observation_gage_height() {
        let site = SiteInfo {
            monitoring_location_id: "USGS-11447650".into(),
            name: "SACRAMENTO R A FREEPORT CA".into(),
            lat: 38.4564,
            lon: -121.5008,
        };
        let (reading, raw) = parse_observation(&obs_gage_height(), &site, Some("action"), None)
            .expect("should parse");
        assert_eq!(reading.source, "usgs-water");
        assert_eq!(reading.metric, "water_level");
        assert_eq!(reading.value, 12.5);
        assert_eq!(reading.unit, "ft");
        assert_eq!(reading.station, "USGS-11447650");
        assert_eq!(reading.place, "SACRAMENTO R A FREEPORT CA");
        assert_eq!(reading.lat, Some(38.4564));
        assert_eq!(reading.lon, Some(-121.5008));
        // guid = source:mlid:param:time
        assert_eq!(
            reading.guid.as_deref(),
            Some("usgs-water:USGS-11447650:00065:2026-06-10T14:00:00+00:00")
        );
        // extra has parameter_code and flood_stage
        assert_eq!(reading.extra.get("parameter_code"), Some(&json!("00065")));
        assert_eq!(reading.extra.get("flood_stage"), Some(&json!("action")));
        assert_eq!(reading.extra.get("approval_status"), Some(&json!("Provisional")));
        // raw is verbatim feature
        assert!(raw.get("properties").is_some());
        assert_eq!(raw["properties"]["value"], json!("12.5"));
    }

    #[test]
    fn parse_observation_discharge() {
        let site = SiteInfo {
            monitoring_location_id: "USGS-11447650".into(),
            name: "SACRAMENTO R A FREEPORT CA".into(),
            lat: 38.4564,
            lon: -121.5008,
        };
        let (reading, _) = parse_observation(&obs_discharge(), &site, None, None).expect("should parse");
        assert_eq!(reading.metric, "discharge");
        assert_eq!(reading.value, 1240.0);
        assert_eq!(reading.unit, "cfs");
    }

    #[test]
    fn parse_observation_unknown_param_skipped() {
        let site = SiteInfo {
            monitoring_location_id: "USGS-11447650".into(),
            name: "Test".into(),
            lat: 38.0,
            lon: -121.0,
        };
        let body = json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": null,
                "properties": {
                    "monitoring_location_id": "USGS-11447650",
                    "parameter_code": "00010",  // water temperature — not in metric_for_param
                    "time": "2026-06-10T14:00:00+00:00",
                    "value": "15.0",
                    "unit_of_measure": "deg C"
                }
            }]
        });
        assert!(parse_observation(&body, &site, None, None).is_none());
    }

    #[test]
    fn parse_thresholds_categorizes_flood_stage() {
        let meta = metadata_with_stages();
        // 12.5 ft: above action (10) but below flood (15) → "action"
        let (stage, raw) = parse_thresholds(&meta, 12.5);
        assert_eq!(stage.as_deref(), Some("action"));
        assert!(raw.is_some());

        // 16 ft: above action (10) + flood (15), below moderate (20) → "flood"
        let (stage, _) = parse_thresholds(&meta, 16.0);
        assert_eq!(stage.as_deref(), Some("flood"));

        // 22 ft: above moderate (20) but below major (25) → "moderate"
        let (stage, _) = parse_thresholds(&meta, 22.0);
        assert_eq!(stage.as_deref(), Some("moderate"));

        // 30 ft: above all thresholds → "major"
        let (stage, _) = parse_thresholds(&meta, 30.0);
        assert_eq!(stage.as_deref(), Some("major"));

        // 5 ft: below action stage → "below_action"
        let (stage, _) = parse_thresholds(&meta, 5.0);
        assert_eq!(stage.as_deref(), Some("below_action"));
    }

    #[test]
    fn parse_thresholds_no_data_gives_none() {
        let (stage, raw) = parse_thresholds(&metadata_empty(), 12.5);
        assert!(stage.is_none());
        assert!(raw.is_none());
    }

    #[test]
    fn to_local_converts_utc_to_local_offset() {
        // The API returns UTC timestamps ("2026-06-10T14:00:00+00:00").
        // to_local should parse and re-render in the local timezone.
        let result = to_local("2026-06-10T14:00:00+00:00");
        // Must be valid RFC3339.
        assert!(DateTime::parse_from_rfc3339(&result).is_ok(), "result: {result}");
        // Must represent the same instant.
        let orig = DateTime::parse_from_rfc3339("2026-06-10T14:00:00+00:00").unwrap();
        let converted = DateTime::parse_from_rfc3339(&result).unwrap();
        assert_eq!(orig.timestamp(), converted.timestamp());
    }

    // --- Pull / store / dedupe integration tests ----------------------------

    #[test]
    fn pull_writes_readings_and_raw_for_two_sites_two_params() {
        let v = temp_vault("pull");
        // Two sites but only first has both params (second has obs_empty for
        // gage_height — stub returns same response for all calls by param, not site,
        // so both sites get readings).
        let stub = Stub {
            sites: sites_near(),
            gage_height: obs_gage_height(),
            discharge: obs_discharge(),
            metadata: metadata_with_stages(),
        };
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        // 2 sites × 2 params = 4 readings (stub returns data for every call).
        assert_eq!(out.counts.get("readings"), Some(&4));

        let readings_file = v.root().join("environment/usgs-water/2026-06.jsonl");
        assert!(readings_file.exists());
        let lines: Vec<&str> = std::fs::read_to_string(&readings_file)
            .unwrap()
            .lines()
            .count()
            .to_string()
            .parse::<usize>()
            .map(|_| vec![])
            .unwrap_or_default();
        let _ = lines; // just checking exists

        // Raw layer exists.
        let raw_file = v.root().join("environment/usgs-water/raw/2026-06.jsonl");
        assert!(raw_file.exists());

        // Contract rows have the right fields.
        let stream_rows: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert!(!stream_rows.is_empty());
        assert!(stream_rows.iter().any(|r| r.metric == "water_level"));
        assert!(stream_rows.iter().any(|r| r.metric == "discharge"));
        // Flood stage annotated on gage-height rows.
        let gage_row = stream_rows.iter().find(|r| r.metric == "water_level").unwrap();
        assert_eq!(gage_row.extra.get("flood_stage"), Some(&json!("action")));

        // Cursor advanced.
        let state = v.read_usgs_water_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn repoll_upserts_not_duplicates() {
        let v = temp_vault("dedup");
        let stub = Stub {
            sites: sites_near(),
            gage_height: obs_gage_height(),
            discharge: obs_discharge(),
            metadata: metadata_with_stages(),
        };
        pull_at_with(&v, Some(SEED), &stub).unwrap();
        let count1: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();

        // Re-poll: same timestamps → same guids → upsert, no new rows.
        pull_at_with(&v, Some(SEED), &stub).unwrap();
        let count2: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(count1.len(), count2.len(), "re-poll must not duplicate by guid");
    }

    #[test]
    fn no_sites_is_inert_no_error() {
        let v = temp_vault("no-sites");
        let stub = Stub {
            sites: sites_empty(),
            gage_height: obs_empty(),
            discharge: obs_empty(),
            metadata: metadata_empty(),
        };
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/usgs-water").exists() || {
            !v.root().join("environment/usgs-water/2026-06.jsonl").exists()
        });
        // Cursor still advanced (location resolved, just no gauges).
        let state = v.read_usgs_water_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn no_point_is_inert() {
        let v = temp_vault("no-point");
        let stub = Stub {
            sites: sites_empty(),
            gage_height: obs_empty(),
            discharge: obs_empty(),
            metadata: metadata_empty(),
        };
        let out = pull_at_with(&v, None, &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/usgs-water/2026-06.jsonl").exists());
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("sync");
        v.write_usgs_water_sync(&UsgsWaterSyncState {
            updated: "2026-06-10T14:00:00-07:00".into(),
            lat: 38.45,
            lon: -121.50,
            error: String::new(),
        })
        .unwrap();
        let s = v.read_usgs_water_sync().unwrap();
        assert_eq!(s.lat, 38.45);
        assert_eq!(s.lon, -121.50);
        assert!(s.error.is_empty());
    }

    #[test]
    fn old_env_reading_lines_still_deserialize() {
        // Forward/back-compat: a minimal EnvReading (only 4 required fields) must
        // parse — proves the upsert read path tolerates sparse existing data.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/usgs-water")).unwrap();
        std::fs::write(
            v.root().join("environment/usgs-water/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T14:00:00-07:00\",\"source\":\"usgs-water\",\"metric\":\"water_level\",\"value\":10.5}\n",
        )
        .unwrap();
        let rows: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, 10.5);
        assert_eq!(rows[0].guid, None, "sparse row has no guid");
    }
}
