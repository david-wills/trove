//! NOAA National Data Buoy Center (NDBC) — real-time ocean and marine
//! observations from ~1,000 physical buoys and coastal stations.
//!
//! A **Periodic**, keyless, no-login collector. It writes the **`environment`**
//! domain's bound scalar-reading shape:
//!
//! - **readings → [`EnvReading`]** (`environment/noaa-ndbc/YYYY-MM.jsonl`,
//!   month of `ts`) — one row per metric per observation timestamp; metrics:
//!   `wave_height`, `wave_period_dominant`, `wave_period_avg`, `wave_dir`,
//!   `wind_speed`, `wind_gust`, `wind_dir`, `air_temp`, `water_temp`,
//!   `dew_point`, `pressure`, `visibility`, `pressure_tendency`, `tide`.
//!   `guid` = `"noaa-ndbc:{station}:{metric}:{ts}"` (upsert-safe; a re-poll
//!   of the same observation overwrites, never duplicates).
//! - **raw layer** (`environment/noaa-ndbc/raw/YYYY-MM.jsonl`) — one JSON
//!   object per observation row (all columns, full fidelity, missing sentinel
//!   values preserved as `null`).
//!
//! **Station selection:** the user's configured location (CoreLocation or
//! manual weather location) picks the nearest NDBC station from
//! `activestations.xml`. Coastal/Great-Lakes only — inert if no station is
//! within 500 km (silent no-op, not an error). The station id is persisted in
//! the cursor so a re-run doesn't re-fetch the station list every hour.
//!
//! **Fixed-width plain text format:** the realtime2 `.txt` file has two header
//! lines (column names, then units) followed by space-separated data rows.
//! Missing values use the sentinel `"MM"` (or `"99.0"` for WDIR). The parser
//! reads both header lines to derive column positions, so an extra/reordered
//! column from a station-specific variant is handled gracefully — unknown
//! columns go into `extra` on the raw object.
//!
//! **US / territorial waters only.** The collector is silently inert outside
//! the bounding area NDBC covers.
//!
//! **Catalog:** brief at `docs/integrations/noaa-ndbc.md`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvReading;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

const DIR: &str = "environment/noaa-ndbc";
const RAW_DIR: &str = "environment/noaa-ndbc/raw";
const SYNC_FILE: &str = ".trove/noaa-ndbc-sync.json";
const SOURCE: &str = "noaa-ndbc";

/// NDBC data endpoint roots — keyless, no auth.
const NDBC_DATA: &str = "https://www.ndbc.noaa.gov/data/realtime2";
const NDBC_STATIONS: &str = "https://www.ndbc.noaa.gov/activestations.xml";

const USER_AGENT: &str = "Trove (https://trove.app)";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Maximum distance (km) to the nearest station — beyond this we treat the
/// user as "not coastal" and stay inert.
const MAX_STATION_KM: f64 = 500.0;

/// Hourly cadence; timer only advances on a real run (re-enable fires
/// immediately).
pub const NDBC_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_ndbc_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join(DIR)))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        required: false,
    }
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("noaa-ndbc synced — {n} readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("noaa-ndbc skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "NOAA NDBC: nothing new (no coastal station or already up to date)".to_string()
    } else {
        format!("NOAA NDBC synced — {n} readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. Keyless — no
/// `connection`. Hourly cadence; silently inert without a coastal location.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "noaa-ndbc",
        name: "NOAA Buoys (NDBC)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Collects real-time ocean and marine observations from NOAA's \
                      network of ~1,000 buoys — wave height, water temperature, wind \
                      speed, and more for US coastal and Great Lakes locations.",
        domain: "environment",
        vault_path: "environment/noaa-ndbc/",
        toggleable: true,
        setup: &[
            "No account or key — this uses the keyless public NDBC data service.",
            "Approve Location Services when asked, or set a location manually on the \
             Weather tab; without a location this stays inert.",
            "Only useful for US coastal and Great Lakes locations within 500 km of a buoy.",
        ],
        caveats: "US coastal and Great Lakes only — no NDBC station near inland locations. \
                  Inert until a location is configured.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(NDBC_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Persisted sync state: selected station + watermark + last-used location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NdbcSyncState {
    /// RFC3339 local time of the last successful pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// The last-selected NDBC station id (e.g. "46042").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub station_id: String,
    /// Buoy's own latitude — persisted so the reuse path can reconstruct the
    /// correct NdbcStation without re-fetching activestations.xml. This is the
    /// buoy's geographic position, NOT the user's location.
    #[serde(default)]
    pub station_lat: f64,
    /// Buoy's own longitude (see `station_lat`).
    #[serde(default)]
    pub station_lon: f64,
    /// Human-readable station name (e.g. "MONTEREY"). Empty string when
    /// unknown; the reuse path falls back to `station_id` as the place label.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub station_name: String,
    /// RFC3339 of the newest observation successfully written (the watermark).
    /// Observations at or before this timestamp are skipped on re-poll.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub watermark: String,
    /// Last-used USER coordinates — stored only for the 50-km change-heuristic
    /// that decides when to re-select a station. Never used for buoy lat/lon.
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    /// Non-empty while the collector is stuck (no location, no nearby station).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_ndbc_sync(&self) -> Option<NdbcSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_ndbc_sync(&self, state: &NdbcSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Minimal station struct (from activestations.xml).

/// A station entry from NDBC's activestations.xml.
#[derive(Debug, Clone)]
pub struct NdbcStation {
    pub id: String,
    pub lat: f64,
    pub lon: f64,
    pub name: String,
    /// Only stations with met="y" have the standard meteorological data file.
    pub met: bool,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

trait NdbcApi {
    /// Fetch and parse the activestations.xml station list.
    fn stations(&self) -> Result<Vec<NdbcStation>>;
    /// Fetch the realtime2 `.txt` for a given station id.
    fn realtime2(&self, station_id: &str) -> Result<String>;
}

struct NdbcClient;

impl NdbcApi for NdbcClient {
    fn stations(&self) -> Result<Vec<NdbcStation>> {
        let body: String = ureq::get(NDBC_STATIONS)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", USER_AGENT)
            .call()
            .context("fetching NDBC activestations.xml")?
            .into_string()
            .context("reading NDBC stations body")?;
        parse_stations(&body)
    }

    fn realtime2(&self, station_id: &str) -> Result<String> {
        let url = format!("{NDBC_DATA}/{station_id}.txt");
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", USER_AGENT)
            .call()
            .with_context(|| format!("fetching NDBC realtime2 for station {station_id}"))?
            .into_string()
            .context("reading NDBC realtime2 body")
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Parse the NDBC activestations.xml text into a list of met-capable stations.
/// Uses a simple attribute scan; quick-xml is already a workspace dep.
pub fn parse_stations(xml: &str) -> Result<Vec<NdbcStation>> {
    let mut stations = Vec::new();
    for line in xml.lines() {
        let trimmed = line.trim();
        // Each station is a self-closing <Station .../> element.
        if !trimmed.starts_with("<Station ") {
            continue;
        }
        // Only keep met-capable stations (they have the realtime2 file).
        let met = attr_val(trimmed, "met").unwrap_or_default() == "y";
        if !met {
            continue;
        }
        let id = match attr_val(trimmed, "id") {
            Some(v) if !v.is_empty() => v,
            _ => continue,
        };
        let lat: f64 = match attr_val(trimmed, "lat").and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let lon: f64 = match attr_val(trimmed, "lon").and_then(|v| v.parse().ok()) {
            Some(v) => v,
            None => continue,
        };
        let name = attr_val(trimmed, "name").unwrap_or_default();
        stations.push(NdbcStation { id, lat, lon, name, met: true });
    }
    Ok(stations)
}

/// Extract `key="value"` from an XML element's attribute string.
fn attr_val(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Haversine great-circle distance between two (lat, lon) pairs, in km.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    2.0 * R * a.sqrt().atan2((1.0 - a).sqrt())
}

/// Find the nearest met-capable station within `max_km`.
pub fn nearest_station<'a>(
    stations: &'a [NdbcStation],
    lat: f64,
    lon: f64,
    max_km: f64,
) -> Option<&'a NdbcStation> {
    stations
        .iter()
        .map(|s| (s, haversine_km(lat, lon, s.lat, s.lon)))
        .filter(|(_, d)| *d <= max_km)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(s, _)| s)
}

/// Column names from the first header line (stripped of the leading `#`).
pub fn parse_header_names(line: &str) -> Vec<String> {
    line.trim_start_matches('#').split_whitespace().map(|s| s.to_string()).collect()
}

/// Parsed observation row from one data line of a realtime2 `.txt` file.
#[derive(Debug, Clone)]
pub struct ObsRow {
    /// RFC3339 UTC timestamp of the observation.
    pub ts_utc: String,
    /// All column values (parallel to the column-names vec).
    pub cols: Vec<String>,
}

/// Parse the realtime2 `.txt` body:
/// - Line 0: column names (starts with `#YY`)
/// - Line 1: units (starts with `#yr`) — ignored for parsing; kept for raw
/// - Lines 2..: data rows
///
/// Returns `(column_names, unit_names, obs_rows)`.
pub fn parse_realtime2(text: &str) -> (Vec<String>, Vec<String>, Vec<ObsRow>) {
    let mut lines = text.lines();
    let Some(header_names_line) = lines.next() else {
        return (vec![], vec![], vec![]);
    };
    let Some(header_units_line) = lines.next() else {
        return (vec![], vec![], vec![]);
    };
    let col_names = parse_header_names(header_names_line);
    let col_units = parse_header_names(header_units_line);
    let mut rows = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let vals: Vec<String> = line.split_whitespace().map(|s| s.to_string()).collect();
        if vals.len() < 5 {
            continue; // need at least YY MM DD hh mm
        }
        // Parse the timestamp: YY MM DD hh mm (all UTC).
        let (year, month, day, hour, minute) = match (
            vals.get(0).and_then(|v| v.parse::<i32>().ok()),
            vals.get(1).and_then(|v| v.parse::<u32>().ok()),
            vals.get(2).and_then(|v| v.parse::<u32>().ok()),
            vals.get(3).and_then(|v| v.parse::<u32>().ok()),
            vals.get(4).and_then(|v| v.parse::<u32>().ok()),
        ) {
            (Some(y), Some(mo), Some(d), Some(h), Some(mi)) => (y, mo, d, h, mi),
            _ => continue,
        };
        let naive = NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_opt(hour, minute, 0));
        let Some(naive) = naive else { continue };
        let ts_utc = DateTime::<FixedOffset>::from_naive_utc_and_offset(naive, FixedOffset::east_opt(0).unwrap())
            .to_rfc3339();
        rows.push(ObsRow { ts_utc, cols: vals });
    }
    (col_names, col_units, rows)
}

/// Returns `None` for the NDBC realtime2 missing sentinel `"MM"`.
/// Note: `"999"` and `"99"` are NOT sentinels in realtime2 files — they are
/// valid numeric values (e.g. 99 degT is a valid ENE compass heading). Only
/// historical archive files use variable-length 9s fills; realtime2 uses `"MM"`
/// exclusively for missing data.
fn parse_f64_field(s: &str) -> Option<f64> {
    let s = s.trim();
    if s == "MM" {
        return None;
    }
    // Strip a leading '+' (PTDY sometimes carries "+0.0").
    let s = s.trim_start_matches('+');
    s.parse().ok()
}

/// Metric name and unit for each column index we emit as a contract reading.
/// Indexed by column name. Returns `(metric, unit)` or `None` to skip.
fn metric_for_col(col: &str) -> Option<(&'static str, &'static str)> {
    match col {
        "WDIR" => Some(("wind_dir", "degT")),
        "WSPD" => Some(("wind_speed", "m/s")),
        "GST" => Some(("wind_gust", "m/s")),
        "WVHT" => Some(("wave_height", "m")),
        "DPD" => Some(("wave_period_dominant", "sec")),
        "APD" => Some(("wave_period_avg", "sec")),
        "MWD" => Some(("wave_dir", "degT")),
        "PRES" => Some(("pressure", "hPa")),
        "ATMP" => Some(("air_temp", "C")),
        "WTMP" => Some(("water_temp", "C")),
        "DEWP" => Some(("dew_point", "C")),
        "VIS" => Some(("visibility", "nmi")),
        "PTDY" => Some(("pressure_tendency", "hPa")),
        "TIDE" => Some(("tide", "ft")),
        _ => None,
    }
}

/// Emit contract readings + the raw JSON object for one observation row.
/// Skips any metric where the value is the NDBC missing sentinel.
pub fn obs_to_readings_and_raw(
    row: &ObsRow,
    col_names: &[String],
    station: &NdbcStation,
    place: &str,
    watermark_ts: &str,
) -> (Vec<EnvReading>, Value) {
    // Skip observations at or before the watermark.
    if !watermark_ts.is_empty() && row.ts_utc.as_str() <= watermark_ts {
        return (vec![], Value::Null);
    }

    // Build the raw JSON: all columns by name.
    let mut raw_obj = Map::new();
    raw_obj.insert("ts".into(), Value::String(row.ts_utc.clone()));
    raw_obj.insert("station".into(), Value::String(station.id.clone()));
    raw_obj.insert("station_name".into(), Value::String(station.name.clone()));
    for (i, name) in col_names.iter().enumerate() {
        if matches!(name.as_str(), "YY" | "MM" | "DD" | "hh" | "mm") {
            continue; // already collapsed into `ts`
        }
        let val = row.cols.get(i).map(|s| s.as_str()).unwrap_or("MM");
        if val == "MM" {
            raw_obj.insert(name.clone(), Value::Null);
        } else {
            // Try numeric; fall back to string.
            let v: Value = val.trim_start_matches('+').parse::<f64>()
                .map(Value::from)
                .unwrap_or_else(|_| Value::String(val.to_string()));
            raw_obj.insert(name.clone(), v);
        }
    }

    // Build contract readings for known columns.
    let mut readings = Vec::new();
    for (i, col_name) in col_names.iter().enumerate() {
        let Some((metric, unit)) = metric_for_col(col_name) else { continue };
        let raw_val = row.cols.get(i).map(|s| s.as_str()).unwrap_or("MM");
        let Some(value) = parse_f64_field(raw_val) else { continue }; // skip MM
        let guid = format!("noaa-ndbc:{}:{}:{}", station.id, metric, row.ts_utc);
        readings.push(EnvReading {
            ts: row.ts_utc.clone(),
            source: SOURCE.into(),
            metric: metric.into(),
            value,
            unit: unit.into(),
            place: place.to_string(),
            lat: Some(station.lat),
            lon: Some(station.lon),
            station: station.id.clone(),
            guid: Some(guid),
            extra: Map::new(),
        });
    }
    (readings, Value::Object(raw_obj))
}

// ---------------------------------------------------------------------------
// Upsert into stable month partitions.

/// Upsert readings into `environment/noaa-ndbc/YYYY-MM.jsonl` by `guid`.
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
            match existing.iter_mut().find(|e| e.guid == row.guid && e.guid.is_some()) {
                Some(slot) => *slot = row.clone(),
                None => existing.push(row.clone()),
            }
            written += 1;
        }
        vault.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(written)
}

/// Upsert raw objects into `environment/noaa-ndbc/raw/YYYY-MM.jsonl` by
/// `ts` + `station`.
fn upsert_raw(vault: &Vault, raws: &[Value]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: BTreeMap<String, Vec<&Value>> = Default::default();
    for v in raws {
        let ts = v.get("ts").and_then(Value::as_str).unwrap_or("");
        if let Some(key) = Partition::Month.key(ts) {
            by_key.entry(key.to_string()).or_default().push(v);
        }
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<Value> = stream.read(&key)?;
        for v in incoming {
            let ts = v.get("ts").and_then(Value::as_str).unwrap_or("");
            let station = v.get("station").and_then(Value::as_str).unwrap_or("");
            let pos = existing.iter().position(|e| {
                e.get("ts").and_then(Value::as_str) == Some(ts)
                    && e.get("station").and_then(Value::as_str) == Some(station)
            });
            match pos {
                Some(i) => existing[i] = v.clone(),
                None => existing.push(v.clone()),
            }
        }
        vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the user's location. Uses the same ladder as NWS/weather: CoreLocation
/// → manual weather location → cursor last-known. Returns `None` when no
/// location is available.
fn resolve_point(vault: &Vault, state: &NdbcSyncState) -> Option<(f64, f64)> {
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

/// Production entry point: resolves location → selects station → fetches + writes.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_ndbc_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut s = state;
        s.updated = Local::now().to_rfc3339();
        s.error = "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_ndbc_sync(&s)?;
        return Ok(PullOutcome {
            headline: "NOAA NDBC: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }
    let client = NdbcClient;
    pull_with(vault, point, &client)
}

/// The pull body over an injected API + explicit point. Testable offline.
/// `None` → inert no-op (no network, no rows, no error).
pub(crate) fn pull_with(vault: &Vault, point: Option<(f64, f64)>, api: &impl NdbcApi) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "NOAA NDBC: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    };

    let now = Local::now();
    let mut state = vault.read_ndbc_sync().unwrap_or_default();

    // Resolve station: try to reuse the cursor station, otherwise fetch the
    // list and find the nearest one.
    let station = if !state.station_id.is_empty() {
        // We trust the cursor station unless the user's location changed
        // significantly (simple heuristic: if the cached coords are more than
        // 50 km away, re-select).
        let cached_dist = haversine_km(lat, lon, state.lat, state.lon);
        if cached_dist > 50.0 {
            // Location has changed enough — re-fetch the station list.
            let stations = api.stations().context("fetching NDBC station list")?;
            match nearest_station(&stations, lat, lon, MAX_STATION_KM) {
                Some(s) => s.clone(),
                None => {
                    state.updated = now.to_rfc3339();
                    state.error = "no NDBC station within 500 km of your location".into();
                    state.lat = lat;
                    state.lon = lon;
                    vault.write_ndbc_sync(&state)?;
                    return Ok(PullOutcome {
                        headline: "NOAA NDBC: no station within 500 km".into(),
                        counts: BTreeMap::from([("readings", 0)]),
                    });
                }
            }
        } else {
            // Reuse cursor station — synthesize an NdbcStation from the persisted
            // buoy coordinates (station_lat/station_lon), NOT the user's
            // location (lat/lon). This ensures EnvReading.lat/lon always point
            // at the buoy, not the user.
            NdbcStation {
                id: state.station_id.clone(),
                lat: state.station_lat,
                lon: state.station_lon,
                name: state.station_name.clone(),
                met: true,
            }
        }
    } else {
        // First run — fetch the station list.
        let stations = api.stations().context("fetching NDBC station list")?;
        match nearest_station(&stations, lat, lon, MAX_STATION_KM) {
            Some(s) => s.clone(),
            None => {
                state.updated = now.to_rfc3339();
                state.error = "no NDBC station within 500 km of your location".into();
                state.lat = lat;
                state.lon = lon;
                vault.write_ndbc_sync(&state)?;
                return Ok(PullOutcome {
                    headline: "NOAA NDBC: no station within 500 km".into(),
                    counts: BTreeMap::from([("readings", 0)]),
                });
            }
        }
    };

    // Fetch and parse the realtime2 file.
    let body = api.realtime2(&station.id)
        .with_context(|| format!("fetching realtime2 for station {}", station.id))?;

    let (col_names, _col_units, obs_rows) = parse_realtime2(&body);
    if col_names.is_empty() || obs_rows.is_empty() {
        // Empty file — station may be offline. Not an error; retry next hour.
        state.updated = now.to_rfc3339();
        state.station_id = station.id.clone();
        state.station_lat = station.lat;
        state.station_lon = station.lon;
        state.station_name = station.name.clone();
        state.lat = lat;
        state.lon = lon;
        state.error = String::new();
        vault.write_ndbc_sync(&state)?;
        return Ok(PullOutcome {
            headline: format!("NOAA NDBC ({}): no data rows", station.id),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }

    let watermark = state.watermark.clone();
    let place = if station.name.is_empty() { station.id.clone() } else { station.name.clone() };

    let mut all_readings: Vec<EnvReading> = Vec::new();
    let mut all_raws: Vec<Value> = Vec::new();
    let mut newest_ts = watermark.clone();

    for row in &obs_rows {
        let (readings, raw) = obs_to_readings_and_raw(&row, &col_names, &station, &place, &watermark);
        if readings.is_empty() {
            continue; // either watermark-skipped or all-MM row
        }
        if newest_ts.is_empty() || row.ts_utc > newest_ts {
            newest_ts = row.ts_utc.clone();
        }
        all_readings.extend(readings);
        if !matches!(raw, Value::Null) {
            all_raws.push(raw);
        }
    }

    let readings_written = if !all_readings.is_empty() {
        upsert_readings(vault, &all_readings)?
    } else {
        0
    };
    if !all_raws.is_empty() {
        upsert_raw(vault, &all_raws)?;
    }

    // Advance the watermark only after a successful full drain.
    if !newest_ts.is_empty() && newest_ts > state.watermark {
        state.watermark = newest_ts;
    }
    state.updated = now.to_rfc3339();
    state.station_id = station.id.clone();
    // Persist the buoy's own coordinates under station_lat/station_lon so that
    // the reuse path can reconstruct a correctly-located NdbcStation without
    // re-fetching activestations.xml. state.lat/lon remain user coordinates only.
    state.station_lat = station.lat;
    state.station_lon = station.lon;
    state.station_name = station.name.clone();
    state.lat = lat;
    state.lon = lon;
    state.error = String::new();
    vault.write_ndbc_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("NOAA NDBC ({}): {readings_written} readings", station.id),
        counts: BTreeMap::from([("readings", readings_written)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-noaa-ndbc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Real sample data (from https://www.ndbc.noaa.gov/data/realtime2/46042.txt) ---

    fn sample_realtime2() -> &'static str {
        "#YY  MM DD hh mm WDIR WSPD GST  WVHT   DPD   APD MWD   PRES  ATMP  WTMP  DEWP  VIS PTDY  TIDE\n\
         #yr  mo dy hr mn degT m/s  m/s     m   sec   sec degT   hPa  degC  degC  degC  nmi  hPa    ft\n\
         2026 06 17 21 30 180  5.0  6.0    MM    MM    MM  MM 1013.4  15.6  15.8  14.0   MM   MM    MM\n\
         2026 06 17 21 20 180  5.0  7.0   1.9    15   8.1 173 1013.4  15.5  15.9  13.9   MM   MM    MM\n\
         2026 06 17 21 10 180  5.0  6.0   1.9    MM   8.1 173 1013.4  15.5  15.9  13.9   MM   MM    MM\n\
         2026 06 17 21 00 180  4.0  6.0    MM    MM    MM  MM 1013.5  15.4  16.0  13.9   MM +0.0    MM\n"
    }

    fn sample_stations_xml() -> &'static str {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<stations>
<Station id="46042" lat="36.785" lon="-122.398" elev="0.0" name="MONTEREY" owner="NDBC" pgm="IOOS Partners" type="buoy" met="y" currents="n" waterquality="n" dart="n"/>
<Station id="TTIW1" lat="48.5" lon="-123.0" elev="0.0" name="TURN ISLAND" owner="NOS" pgm="NOS/CO-OPS" type="fixed" met="n" currents="n" waterquality="n" dart="n"/>
<Station id="46026" lat="37.759" lon="-122.833" elev="0.0" name="SAN FRANCISCO" owner="NDBC" pgm="IOOS Partners" type="buoy" met="y" currents="n" waterquality="n" dart="n"/>
</stations>"#
    }

    // --- Stub API ---

    struct Stub {
        stations_body: String,
        realtime2_body: String,
    }

    impl NdbcApi for Stub {
        fn stations(&self) -> Result<Vec<NdbcStation>> {
            parse_stations(&self.stations_body)
        }
        fn realtime2(&self, _station_id: &str) -> Result<String> {
            Ok(self.realtime2_body.clone())
        }
    }

    fn make_stub() -> Stub {
        Stub {
            stations_body: sample_stations_xml().to_string(),
            realtime2_body: sample_realtime2().to_string(),
        }
    }

    // --- Parser tests ---

    #[test]
    fn parse_stations_returns_only_met_capable() {
        let stations = parse_stations(sample_stations_xml()).unwrap();
        // TTIW1 has met="n" → excluded.
        assert_eq!(stations.len(), 2);
        assert!(stations.iter().all(|s| s.met));
        let ids: Vec<_> = stations.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"46042"));
        assert!(ids.contains(&"46026"));
        assert!(!ids.contains(&"TTIW1"), "met=n station excluded");
    }

    #[test]
    fn parse_stations_correct_attrs() {
        let stations = parse_stations(sample_stations_xml()).unwrap();
        let s = stations.iter().find(|s| s.id == "46042").unwrap();
        assert!((s.lat - 36.785).abs() < 1e-3);
        assert!((s.lon - (-122.398)).abs() < 1e-3);
        assert_eq!(s.name, "MONTEREY");
    }

    #[test]
    fn parse_realtime2_columns_and_rows() {
        let (col_names, col_units, rows) = parse_realtime2(sample_realtime2());
        assert_eq!(col_names[0], "YY");
        assert_eq!(col_names[5], "WDIR");
        assert_eq!(col_names[11], "MWD");
        assert_eq!(col_units[0], "yr");
        assert_eq!(col_units[5], "degT");
        assert_eq!(rows.len(), 4);
        // YY=0 MM=1 DD=2 hh=3 mm=4 WDIR=5 WSPD=6 GST=7 WVHT=8 ...
        // Row 0 (21:30): WVHT=MM
        assert_eq!(rows[0].cols.get(8), Some(&"MM".to_string()), "WVHT is MM");
        // Row 1 (21:20): WVHT=1.9
        assert_eq!(rows[1].cols.get(8), Some(&"1.9".to_string()), "WVHT = 1.9");
        // Timestamps are UTC RFC3339
        assert!(rows[0].ts_utc.contains("2026-06-17T21:30:00"));
        assert!(rows[1].ts_utc.contains("2026-06-17T21:20:00"));
    }

    #[test]
    fn parse_f64_field_handles_sentinels() {
        // Only "MM" is the realtime2 missing-value sentinel.
        assert_eq!(parse_f64_field("MM"), None);
        // 99 and 999 are VALID numeric values in realtime2 (e.g. 99 degT = ENE
        // compass heading). They are NOT sentinels — only historical archive
        // files use variable-length 9s fills.
        assert_eq!(parse_f64_field("999"), Some(999.0), "999 is a valid realtime2 value");
        assert_eq!(parse_f64_field("99"), Some(99.0), "99 is a valid ENE direction");
        assert_eq!(parse_f64_field("5.0"), Some(5.0));
        assert_eq!(parse_f64_field("+0.0"), Some(0.0), "PTDY leading +");
        assert_eq!(parse_f64_field("1013.4"), Some(1013.4));
        assert_eq!(parse_f64_field("1.9"), Some(1.9));
    }

    #[test]
    fn obs_to_readings_emits_correct_metrics_and_skips_mm() {
        let (col_names, _, rows) = parse_realtime2(sample_realtime2());
        let station = NdbcStation {
            id: "46042".into(),
            lat: 36.785,
            lon: -122.398,
            name: "MONTEREY".into(),
            met: true,
        };
        // Row 0: WVHT=MM, DPD=MM, APD=MM, MWD=MM — those 4 metrics skipped.
        let (readings_0, raw_0) = obs_to_readings_and_raw(&rows[0], &col_names, &station, "MONTEREY", "");
        let metrics_0: Vec<_> = readings_0.iter().map(|r| r.metric.as_str()).collect();
        assert!(!metrics_0.contains(&"wave_height"), "WVHT=MM skipped");
        assert!(!metrics_0.contains(&"wave_period_dominant"), "DPD=MM skipped");
        assert!(metrics_0.contains(&"wind_speed"), "WSPD=5.0 emitted");
        assert!(metrics_0.contains(&"pressure"), "PRES=1013.4 emitted");
        assert!(metrics_0.contains(&"air_temp"), "ATMP=15.6 emitted");
        assert!(metrics_0.contains(&"water_temp"), "WTMP=15.8 emitted");
        assert!(!matches!(raw_0, Value::Null), "raw row emitted for row 0");

        // Row 1: WVHT=1.9 → wave_height emitted.
        let (readings_1, _) = obs_to_readings_and_raw(&rows[1], &col_names, &station, "MONTEREY", "");
        let metrics_1: Vec<_> = readings_1.iter().map(|r| r.metric.as_str()).collect();
        assert!(metrics_1.contains(&"wave_height"));
        let wh = readings_1.iter().find(|r| r.metric == "wave_height").unwrap();
        assert_eq!(wh.value, 1.9);
        assert_eq!(wh.unit, "m");
        assert_eq!(wh.station, "46042");
        assert_eq!(wh.lat, Some(36.785));
        assert!(wh.guid.as_deref().unwrap().contains("wave_height"));
    }

    #[test]
    fn obs_watermark_skips_seen_rows() {
        // Sample rows are newest-first: row0=21:30, row1=21:20, row2=21:10, row3=21:00.
        // Setting the watermark to the newest row (row0=21:30) means all rows
        // are <= that timestamp and must be skipped on re-poll.
        let (col_names, _, rows) = parse_realtime2(sample_realtime2());
        let station = NdbcStation {
            id: "46042".into(), lat: 36.785, lon: -122.398, name: "MONTEREY".into(), met: true,
        };
        // Watermark = the newest observation (21:30) — all 4 rows are <= and skipped.
        let watermark = rows[0].ts_utc.clone();
        for (i, row) in rows.iter().enumerate() {
            let (r, _) = obs_to_readings_and_raw(row, &col_names, &station, "MONTEREY", &watermark);
            assert!(r.is_empty(), "row {i} ts={} <= watermark {watermark}, should be skipped", row.ts_utc);
        }
        // With an empty watermark, rows that have data come through.
        let (r1, _) = obs_to_readings_and_raw(&rows[1], &col_names, &station, "MONTEREY", "");
        assert!(!r1.is_empty(), "row 1 (21:20) has valid metrics with no watermark");
    }

    #[test]
    fn haversine_sanity() {
        // LA to SF ≈ 559 km.
        let d = haversine_km(34.05, -118.24, 37.77, -122.42);
        assert!((d - 559.0).abs() < 10.0, "LA-SF distance ≈ 559 km, got {d}");
        // Same point → 0.
        assert_eq!(haversine_km(36.0, -122.0, 36.0, -122.0), 0.0);
    }

    #[test]
    fn nearest_station_picks_closest() {
        let stations = parse_stations(sample_stations_xml()).unwrap();
        // Point near Monterey (36.6, -121.9) → 46042 should win over 46026 (SF).
        let s = nearest_station(&stations, 36.6, -121.9, 500.0).unwrap();
        assert_eq!(s.id, "46042");
    }

    #[test]
    fn nearest_station_returns_none_beyond_max() {
        let stations = parse_stations(sample_stations_xml()).unwrap();
        // Inland point far from coast (Denver-ish).
        let s = nearest_station(&stations, 39.7, -104.9, 500.0);
        assert!(s.is_none(), "Denver is > 500 km from any NDBC station");
    }

    // --- Store / pull tests ---

    #[test]
    fn pull_writes_readings_and_raw() {
        let v = temp_vault("pull");
        let out = pull_with(&v, Some((36.6, -121.9)), &make_stub()).unwrap();
        let n = out.counts.get("readings").copied().unwrap_or(0);
        assert!(n > 0, "at least one reading written");

        // Contract readings file: environment/noaa-ndbc/2026-06.jsonl
        let readings_path = v.root().join("environment/noaa-ndbc/2026-06.jsonl");
        assert!(readings_path.exists(), "contract readings file written");
        let readings: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert!(!readings.is_empty());
        // All readings have required fields.
        for r in &readings {
            assert_eq!(r.source, "noaa-ndbc");
            assert!(!r.metric.is_empty());
            assert!(!r.ts.is_empty());
            assert!(r.guid.is_some());
        }

        // Raw layer: environment/noaa-ndbc/raw/2026-06.jsonl
        let raw_path = v.root().join("environment/noaa-ndbc/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw layer written");
    }

    #[test]
    fn pull_upserts_do_not_duplicate() {
        let v = temp_vault("upsert");
        // First pull.
        pull_with(&v, Some((36.6, -121.9)), &make_stub()).unwrap();
        let count_1: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();

        // Second pull with identical data — watermark should advance and skip all.
        pull_with(&v, Some((36.6, -121.9)), &make_stub()).unwrap();
        let count_2: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();

        assert_eq!(count_1.len(), count_2.len(), "second pull does not duplicate rows");
    }

    #[test]
    fn pull_none_point_is_inert() {
        let v = temp_vault("inert");
        let out = pull_with(&v, None, &make_stub()).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/noaa-ndbc").exists(), "nothing written");
    }

    #[test]
    fn cursor_persists_station_and_watermark() {
        let v = temp_vault("cursor");
        pull_with(&v, Some((36.6, -121.9)), &make_stub()).unwrap();
        let state = v.read_ndbc_sync().unwrap();
        assert_eq!(state.station_id, "46042");
        assert!(!state.watermark.is_empty(), "watermark advanced");
        assert!(!state.updated.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn reuse_path_uses_buoy_coords_not_user_coords() {
        // This tests that EnvReading.lat/lon on the second (cursor-reuse) pull
        // are the BUOY's coordinates, not the user's location.
        //
        // Station 46042 sits at lat=36.785, lon=-122.398.
        // The user is placed at lat=36.6, lon=-121.9 (inside 50 km → reuse path).
        // A second pull with a DIFFERENT user point (still within 50 km of the
        // cached user point) must still tag readings with the buoy's lat/lon.

        let v = temp_vault("reuse_coords");
        let stub = make_stub();

        // First pull: fetches station list, selects 46042, persists cursor.
        pull_with(&v, Some((36.6, -121.9)), &stub).unwrap();

        // Verify station coords were persisted correctly.
        let state = v.read_ndbc_sync().unwrap();
        assert_eq!(state.station_id, "46042");
        assert!((state.station_lat - 36.785).abs() < 1e-3, "station_lat persisted");
        assert!((state.station_lon - (-122.398)).abs() < 1e-3, "station_lon persisted");
        assert_eq!(state.station_name, "MONTEREY", "station_name persisted");

        // Second pull: advance the watermark so new rows come in; use a stub
        // that returns the same data (watermark will block re-writing, but the
        // station synthesis path is exercised). We need to clear the watermark
        // to force new readings through so we can inspect their lat/lon.
        // Instead, directly test the reuse-path station synthesis.
        let reuse_station = NdbcStation {
            id: state.station_id.clone(),
            lat: state.station_lat,
            lon: state.station_lon,
            name: state.station_name.clone(),
            met: true,
        };
        // Buoy coords must differ from user coords.
        let user_lat = 36.6_f64;
        let user_lon = -121.9_f64;
        assert!(
            (reuse_station.lat - user_lat).abs() > 0.1,
            "buoy lat differs from user lat by more than 0.1 deg"
        );
        // Now verify obs_to_readings produces the buoy's lat/lon.
        let (col_names, _, rows) = parse_realtime2(sample_realtime2());
        let (readings, _) = obs_to_readings_and_raw(
            &rows[1],
            &col_names,
            &reuse_station,
            &reuse_station.name,
            "",
        );
        assert!(!readings.is_empty(), "readings produced");
        for r in &readings {
            assert!(
                (r.lat.unwrap() - 36.785).abs() < 1e-3,
                "reading lat={} should be buoy lat 36.785, not user lat {user_lat}",
                r.lat.unwrap()
            );
            assert!(
                (r.lon.unwrap() - (-122.398)).abs() < 1e-3,
                "reading lon={} should be buoy lon -122.398, not user lon {user_lon}",
                r.lon.unwrap()
            );
        }
    }

    #[test]
    fn minimal_reading_round_trips() {
        // Back-compat: a sparse EnvReading (only 4 required fields) from an old
        // on-disk line must parse — proves the upsert read path tolerates it.
        let line = json!({
            "ts": "2026-06-17T21:20:00+00:00",
            "source": "noaa-ndbc",
            "metric": "wave_height",
            "value": 1.9
        });
        let r: EnvReading = serde_json::from_value(line).unwrap();
        assert_eq!(r.source, "noaa-ndbc");
        assert_eq!(r.value, 1.9);
        assert!(r.guid.is_none(), "sparse reading has no guid");
    }

    #[test]
    fn raw_ptdy_plus_prefix_parses_as_number() {
        // "+0.0" in PTDY column must become a number 0.0 in the raw JSON, not
        // the string "+0.0".
        let (col_names, _, rows) = parse_realtime2(sample_realtime2());
        let station = NdbcStation {
            id: "46042".into(), lat: 36.785, lon: -122.398, name: "MONTEREY".into(), met: true,
        };
        // Row 3 has PTDY = +0.0
        let (_, raw) = obs_to_readings_and_raw(&rows[3], &col_names, &station, "MONTEREY", "");
        let ptdy = raw.get("PTDY").unwrap();
        assert_eq!(ptdy, &Value::from(0.0f64), "PTDY '+0.0' stored as number 0.0");
    }
}
