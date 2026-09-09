//! NOAA Tides & Currents (CO-OPS) — tide predictions and water-level data.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/noaa-co-ops.md
//!
//! A **Periodic**, keyless, no-login collector.  It writes the **`environment`**
//! domain's reading shape ([`EnvReading`]):
//!
//! - **Tide predictions (hi/lo) → [`EnvReading`]** (`environment/noaa-co-ops/YYYY-MM.jsonl`,
//!   month of `ts`) — one row per High/Low event, metric = `water_level`,
//!   unit = `ft` or `m`, guid = `noaa-co-ops:<station>:pred:<t>:<type>`.
//!   Cursor on next `begin_date` advances daily by 30-day windows.
//!
//! - **Raw layer** (`environment/noaa-co-ops/raw/YYYY-MM.jsonl`) — the full
//!   API prediction objects verbatim (field names `t`/`v`/`type` exactly as the
//!   API returns them), full fidelity.
//!
//! **Keyless, no login.** The CO-OPS datagetter requires no API key; only the
//! station id and date range are needed.
//!
//! **Nearest-station detection:** at each pull the collector fetches the station
//! list (`mdapi/prod/webapi/stations.json?type=tidepredictions`) and finds the
//! nearest station to the user's location (Haversine distance).  If the nearest
//! station is >50 km away the pull silently returns no rows and records the
//! reason on the cursor (not an error for the UI).
//!
//! **US-only, coastal-only, degrades silently:** inland or non-US users get a
//! quiet no-op, not an error.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvReading;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

const DIR: &str = "environment/noaa-co-ops";
const RAW_DIR: &str = "environment/noaa-co-ops/raw";
const SYNC_FILE: &str = ".trove/noaa-co-ops-sync.json";

const SOURCE: &str = "noaa-co-ops";
const API_BASE: &str = "https://api.tidesandcurrents.noaa.gov/api/prod";
const MDAPI_BASE: &str = "https://api.tidesandcurrents.noaa.gov/mdapi/prod/webapi";
/// CO-OPS is a public government API; a 15-second timeout is generous.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Fetch tide predictions 30 days at a time.
const WINDOW_DAYS: i64 = 30;
/// Skip silently when the nearest station is farther than 50 km.
const MAX_STATION_KM: f64 = 50.0;
/// Daily cadence (86400 s); LocalDay gate ensures only one pull per calendar day.
const CADENCE_SECS: u64 = 86_400;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_noaa_coops_sync()
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
            let preds = out.counts.get("predictions").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(
                preds > 0,
                || format!("noaa-co-ops synced — {preds} tide predictions"),
            ))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("noaa-co-ops sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let preds = out.counts.get("predictions").copied().unwrap_or(0);
    let headline = if preds == 0 {
        "NOAA Tides: nothing new (no nearby tidal station, or predictions up to date)".to_string()
    } else {
        format!("NOAA Tides synced — {preds} tide prediction readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].  Keyless — no login.
/// Daily cadence with a LocalDay gate so it runs once per calendar day.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "noaa-co-ops",
        name: "NOAA Tides & Currents",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls daily tide predictions (high/low) from the nearest \
                      NOAA CO-OPS tidal station (3,000+ US coastal gauges). \
                      Predictions are available up to 10 years ahead; no API key required.",
        domain: "environment",
        vault_path: "environment/noaa-co-ops/",
        toggleable: true,
        setup: &[
            "No account or key required — uses the public CO-OPS API.",
            "Approve Location Services when asked, or set a location on the Weather tab; \
             without a location this stays inert.",
        ],
        caveats: "US coastal only — silently skipped when no tidal station is within 50 km \
                  (e.g. inland users or non-US locations). Inert until a location is set.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::daily(CADENCE_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor.

/// Collector state: the station last used + the last prediction date window
/// fetched + poll timestamp.  Non-secret, rebuildable by re-scanning output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct NoaaCoopsSyncState {
    /// RFC3339 local time of the last poll.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Last latitude used to find the nearest station.
    #[serde(default)]
    pub lat: f64,
    /// Last longitude used to find the nearest station.
    #[serde(default)]
    pub lon: f64,
    /// Station id last used (e.g. "9414290").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub station_id: String,
    /// Station name last used (e.g. "San Francisco").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub station_name: String,
    /// The YYYY-MM-DD of the end of the last fetched window (exclusive next
    /// begin_date).  Empty = never fetched, so start from today.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub next_begin_date: String,
    /// Non-empty when the collector is stuck (no location, no nearby station)
    /// — for the UI banner.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_noaa_coops_sync(&self) -> Option<NoaaCoopsSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_noaa_coops_sync(&self, state: &NoaaCoopsSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait so tests run fully offline.

pub(crate) trait CoopsApi {
    /// `GET /mdapi/prod/webapi/stations.json?type=tidepredictions` → list.
    fn stations(&self) -> Result<Value>;
    /// `GET /api/prod/datagetter?product=predictions&...` → hi/lo predictions.
    fn predictions(&self, station: &str, begin: &str, end: &str) -> Result<Value>;
}

struct CoopsClient;

impl CoopsApi for CoopsClient {
    fn stations(&self) -> Result<Value> {
        let url = format!("{MDAPI_BASE}/stations.json?type=tidepredictions&units=english");
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("fetching NOAA station list")?
            .into_json()
            .context("reading NOAA station list response")
    }

    fn predictions(&self, station: &str, begin: &str, end: &str) -> Result<Value> {
        let url = format!(
            "{API_BASE}/datagetter?product=predictions&datum=MLLW&station={station}\
             &begin_date={begin}&end_date={end}&interval=hilo&time_zone=lst_ldt\
             &units=english&format=json"
        );
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("fetching NOAA tide predictions")?
            .into_json()
            .context("reading NOAA predictions response")
    }
}

// ---------------------------------------------------------------------------
// Station list parsing.

/// A tidal station as returned by the metadata API.
#[derive(Debug, Clone)]
pub struct TidalStation {
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
}

/// Parse the `stations` array from the mdapi response.  The metadata API
/// returns `{ "count": N, "stations": [...] }` where each station has
/// `id`/`name`/`lat`/`lng` (note: `lng` not `lon`).
pub fn parse_stations(body: &Value) -> Vec<TidalStation> {
    let arr = match body.get("stations").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .filter_map(|s| {
            let id = s.get("id").and_then(Value::as_str)?.trim().to_string();
            if id.is_empty() {
                return None;
            }
            let name = s.get("name").and_then(Value::as_str).unwrap_or("").trim().to_string();
            // Station list uses `lat` and `lng` (not `lon`).
            let lat = s.get("lat").and_then(Value::as_f64)?;
            let lon = s.get("lng").and_then(Value::as_f64)?;
            Some(TidalStation { id, name, lat, lon })
        })
        .collect()
}

/// Haversine distance in kilometres between two (lat, lon) points.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6_371.0_f64;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    r * c
}

/// Find the nearest tidal station to `(lat, lon)`.  Returns `None` when the
/// list is empty or the nearest station is farther than `max_km`.
pub fn nearest_station(
    stations: &[TidalStation],
    lat: f64,
    lon: f64,
    max_km: f64,
) -> Option<&TidalStation> {
    stations
        .iter()
        .map(|s| (s, haversine_km(lat, lon, s.lat, s.lon)))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .filter(|(_, dist)| *dist <= max_km)
        .map(|(s, _)| s)
}

// ---------------------------------------------------------------------------
// Prediction parsing.

/// Best-effort `"YYYY-MM-DD HH:MM"` → RFC3339 local.  The API returns
/// timestamps in the local standard/daylight time of the station when
/// `time_zone=lst_ldt`; we parse them as naïve and attach the local UTC
/// offset so the vault's `ts` is consistent with other collectors.  An
/// unparseable value is passed through verbatim (tolerant — better an odd
/// string than a dropped row).
pub fn coops_ts_to_local(s: &str) -> String {
    NaiveDate::parse_from_str(s.trim().get(..10).unwrap_or(""), "%Y-%m-%d")
        .ok()
        .and_then(|_| {
            chrono::NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M").ok()
        })
        .map(|ndt| {
            // Attach the current local offset (the station's time zone is
            // already baked in by the API; we don't convert, we just tag it).
            let offset = *Local::now().offset();
            DateTime::<chrono::FixedOffset>::from_naive_utc_and_offset(
                ndt - chrono::Duration::seconds(offset.local_minus_utc() as i64),
                offset,
            )
            .to_rfc3339()
        })
        .unwrap_or_else(|| s.to_string())
}

/// Parse a `/datagetter?product=predictions&interval=hilo` response into
/// (contract readings, raw API objects).
///
/// Exact field names confirmed from live API:
/// - `t` — timestamp string `"YYYY-MM-DD HH:MM"`
/// - `v` — height value string (feet when `units=english`)
/// - `type` — `"H"` (high) or `"L"` (low)
///
/// The response is `{ "predictions": [...] }` on success, or
/// `{ "error": { "message": "..." } }` on failure.
pub fn parse_predictions(
    body: &Value,
    station: &TidalStation,
) -> (Vec<EnvReading>, Vec<Value>) {
    // A CO-OPS error response contains { "error": { "message": "..." } }.
    // Return empty on error so the collector degrades gracefully (e.g. station
    // has no prediction data for the requested window).
    if body.get("error").is_some() {
        return (Vec::new(), Vec::new());
    }
    let preds = match body.get("predictions").and_then(Value::as_array) {
        Some(p) => p.clone(),
        None => return (Vec::new(), Vec::new()),
    };
    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for pred in preds {
        let t = pred.get("t").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if t.is_empty() {
            continue;
        }
        let v_str = pred.get("v").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let value: f64 = match v_str.parse() {
            Ok(f) => f,
            Err(_) => continue,
        };
        let tide_type = pred.get("type").and_then(Value::as_str).unwrap_or("").trim().to_string();
        // guid: stable — station:pred:t:type (H/L distinguishes same-minute
        // edge cases).
        let guid = format!("{}:pred:{}:{}", station.id, t, tide_type);
        let ts = coops_ts_to_local(&t);

        let mut extra: Map<String, Value> = Map::new();
        if !tide_type.is_empty() {
            extra.insert("tide_type".into(), Value::String(tide_type.clone()));
            // Human-readable expansion for the UI / read-time queries.
            let label = match tide_type.as_str() {
                "H" => "High Tide",
                "L" => "Low Tide",
                _ => tide_type.as_str(),
            };
            extra.insert("tide_label".into(), Value::String(label.to_string()));
        }
        extra.insert("datum".into(), Value::String("MLLW".into()));
        extra.insert("prediction".into(), Value::Bool(true));

        rows.push(EnvReading {
            ts,
            source: SOURCE.into(),
            metric: "water_level".into(),
            value,
            unit: "ft".into(),
            place: station.name.clone(),
            lat: Some(station.lat),
            lon: Some(station.lon),
            station: station.id.clone(),
            guid: Some(guid),
            extra,
        });
        raws.push(pred);
    }
    (rows, raws)
}

// ---------------------------------------------------------------------------
// Upsert helpers (follow the nws.rs pattern exactly).

/// A raw API object with metadata for month-partitioning.  Only `value`
/// is serialized to disk (flattened) — the raw line is the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(skip)]
    guid: String,
    #[serde(flatten)]
    value: Value,
}

fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvReading>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!("noaa-co-ops reading {:?} has unpartitionable ts {:?}", r.guid, r.ts)
        })?;
        by_key.entry(key.to_string()).or_default().push(r);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<EnvReading> = stream.read(&key)?;
        for row in incoming {
            let slot = existing.iter_mut().find(|e| e.guid == row.guid && e.guid.is_some());
            match slot {
                Some(slot) => *slot = row.clone(),
                None => existing.push(row.clone()),
            }
            written += 1;
        }
        vault.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(written)
}

fn upsert_raw(vault: &Vault, lines: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: BTreeMap<String, Vec<&RawLine>> = BTreeMap::new();
    for l in lines {
        let key = Partition::Month
            .key(&l.ts)
            .with_context(|| format!("noaa-co-ops raw {} unpartitionable ts {:?}", l.guid, l.ts))?;
        by_key.entry(key.to_string()).or_default().push(l);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<Value> = stream.read(&key)?;
        for l in incoming {
            // Raw dedupe by the `t` field (same timestamp = same prediction slot).
            let t = l.value.get("t").and_then(Value::as_str).unwrap_or("").to_string();
            let typ = l.value.get("type").and_then(Value::as_str).unwrap_or("").to_string();
            let raw_key = format!("{t}:{typ}");
            let pos = existing.iter().position(|v| {
                let vt = v.get("t").and_then(Value::as_str).unwrap_or("");
                let vty = v.get("type").and_then(Value::as_str).unwrap_or("");
                format!("{vt}:{vty}") == raw_key
            });
            match pos {
                Some(i) => existing[i] = l.value.clone(),
                None => existing.push(l.value.clone()),
            }
        }
        vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the user's location (production ladder).
fn resolve_point(vault: &Vault, state: &NoaaCoopsSyncState) -> Option<(f64, f64)> {
    if corelocation::auth_status() == AuthStatus::NotDetermined {
        corelocation::request_access(5);
    }
    if let Some(fix) = corelocation::current_location(8) {
        return Some((fix.lat, fix.lon));
    }
    if let Some(m) = vault.weather_location() {
        return Some((m.lat, m.lon));
    }
    // Last known: 0,0 is off Ghana, safe as "unset".
    (state.lat != 0.0 || state.lon != 0.0).then_some((state.lat, state.lon))
}

/// Top-level pull: resolve location + sync.  Inert when no location exists.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_noaa_coops_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error =
            "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_noaa_coops_sync(&state)?;
        return Ok(PullOutcome {
            headline: "NOAA Tides: no location set".into(),
            counts: BTreeMap::from([("predictions", 0)]),
        });
    }
    let client = CoopsClient;
    pull_at_with(vault, point, &client)
}

/// Network+write body over an explicit point + injected API — the offline test seam.
pub(crate) fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    client: &impl CoopsApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "NOAA Tides: no location set".into(),
            counts: BTreeMap::from([("predictions", 0)]),
        });
    };
    let now = Local::now();
    let mut state = vault.read_noaa_coops_sync().unwrap_or_default();

    // 1. Find nearest tidal station.
    let stations_body = client.stations()?;
    let stations = parse_stations(&stations_body);
    let station = nearest_station(&stations, lat, lon, MAX_STATION_KM);
    if station.is_none() {
        state.updated = now.to_rfc3339();
        state.lat = lat;
        state.lon = lon;
        state.error = format!("no tidal station within {MAX_STATION_KM} km of ({lat:.3}, {lon:.3})");
        vault.write_noaa_coops_sync(&state)?;
        return Ok(PullOutcome {
            headline: format!(
                "NOAA Tides: no station within {MAX_STATION_KM} km — no tidal data for this location"
            ),
            counts: BTreeMap::from([("predictions", 0)]),
        });
    }
    let station = station.unwrap();

    // 2. Determine the date window.  Start from the stored cursor; if the
    //    station changed, reset to today.
    let today = now.date_naive();
    let begin = if state.station_id != station.id || state.next_begin_date.is_empty() {
        today
    } else {
        NaiveDate::parse_from_str(&state.next_begin_date, "%Y%m%d").unwrap_or(today)
    };
    let end = begin + Duration::days(WINDOW_DAYS - 1);
    let begin_str = begin.format("%Y%m%d").to_string();
    let end_str = end.format("%Y%m%d").to_string();
    // next_begin_date is the day after the end of the window we fetched.
    let next_begin = end + Duration::days(1);

    // 3. Fetch predictions.
    let pred_body = client.predictions(&station.id, &begin_str, &end_str)?;
    let (rows, raws) = parse_predictions(&pred_body, station);

    // 4. Write.
    let predictions_written = if !rows.is_empty() { upsert_readings(vault, &rows)? } else { 0 };
    if !raws.is_empty() {
        let raw_lines: Vec<RawLine> = rows
            .iter()
            .zip(raws.iter())
            .map(|(r, raw)| RawLine {
                ts: r.ts.clone(),
                guid: r.guid.clone().unwrap_or_default(),
                value: raw.clone(),
            })
            .collect();
        upsert_raw(vault, &raw_lines)?;
    }

    // 5. Advance cursor.
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.station_id = station.id.clone();
    state.station_name = station.name.clone();
    state.next_begin_date = next_begin.format("%Y%m%d").to_string();
    state.error = String::new();
    vault.write_noaa_coops_sync(&state)?;

    Ok(PullOutcome {
        headline: format!(
            "NOAA Tides synced — {predictions_written} predictions from {} ({}–{})",
            station.name, begin_str, end_str
        ),
        counts: BTreeMap::from([("predictions", predictions_written)]),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-noaa-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures (field names confirmed from live NOAA CO-OPS API) -----------

    /// A stations list with two stations: one coastal (SF), one far inland.
    /// Station list uses `lng` (not `lon`) — confirmed from live API.
    fn stations_fixture() -> Value {
        json!({
            "count": 2,
            "stations": [
                {
                    "id": "9414290",
                    "name": "San Francisco",
                    "lat": 37.8063,
                    "lng": -122.4659,
                    "state": "CA",
                    "type": "R"
                },
                {
                    "id": "9415020",
                    "name": "Point Reyes",
                    "lat": 38.0000,
                    "lng": -122.9758,
                    "state": "CA",
                    "type": "R"
                }
            ]
        })
    }

    /// A station far from any user — tests the >50 km inland skip.
    fn stations_far_fixture() -> Value {
        json!({
            "count": 1,
            "stations": [
                {
                    "id": "8518750",
                    "name": "The Battery, NY",
                    "lat": 40.6994,
                    "lng": -74.0156,
                    "state": "NY",
                    "type": "R"
                }
            ]
        })
    }

    /// A predictions response — field names `t`/`v`/`type` confirmed from live API.
    /// `type` is "H" for High, "L" for Low.
    fn predictions_fixture() -> Value {
        json!({
            "predictions": [
                { "t": "2026-06-10 02:11", "v": "0.97", "type": "L" },
                { "t": "2026-06-10 08:45", "v": "5.64", "type": "H" },
                { "t": "2026-06-10 15:28", "v": "1.23", "type": "L" },
                { "t": "2026-06-10 21:55", "v": "4.89", "type": "H" }
            ]
        })
    }

    /// A CO-OPS error response (e.g. station has no predictions for the range).
    fn predictions_error_fixture() -> Value {
        json!({
            "error": { "message": "No data was found." }
        })
    }

    // Stub API implementation.
    struct Stub {
        stations: Value,
        predictions: Value,
    }
    impl CoopsApi for Stub {
        fn stations(&self) -> Result<Value> {
            Ok(self.stations.clone())
        }
        fn predictions(&self, _station: &str, _begin: &str, _end: &str) -> Result<Value> {
            Ok(self.predictions.clone())
        }
    }

    /// A coastal point near San Francisco (~2 km from the SF station).
    const SF: (f64, f64) = (37.80, -122.40);
    /// A point in Denver — well inland, >50 km from any coastal station.
    const DENVER: (f64, f64) = (39.74, -104.98);

    // --- Unit: station list parsing ------------------------------------------

    #[test]
    fn parse_stations_reads_id_name_lat_lng() {
        let stations = parse_stations(&stations_fixture());
        assert_eq!(stations.len(), 2);
        let sf = &stations[0];
        assert_eq!(sf.id, "9414290");
        assert_eq!(sf.name, "San Francisco");
        assert!((sf.lat - 37.8063).abs() < 1e-4);
        assert!((sf.lon - -122.4659).abs() < 1e-4);
    }

    #[test]
    fn parse_stations_empty_on_missing_array() {
        assert!(parse_stations(&json!({})).is_empty());
        assert!(parse_stations(&json!({ "stations": [] })).is_empty());
    }

    // --- Unit: nearest station -----------------------------------------------

    #[test]
    fn nearest_station_finds_sf_from_sf_coords() {
        let stations = parse_stations(&stations_fixture());
        let s = nearest_station(&stations, SF.0, SF.1, MAX_STATION_KM).unwrap();
        assert_eq!(s.id, "9414290");
    }

    #[test]
    fn nearest_station_returns_none_when_beyond_max_km() {
        let stations = parse_stations(&stations_far_fixture());
        // Denver is >2000 km from The Battery NY — well past the 50 km limit.
        assert!(nearest_station(&stations, DENVER.0, DENVER.1, MAX_STATION_KM).is_none());
    }

    #[test]
    fn haversine_sf_to_sf_station_is_small() {
        // SF user to SF station: ~4 km.
        let d = haversine_km(SF.0, SF.1, 37.8063, -122.4659);
        assert!(d < 10.0, "expected < 10 km, got {d}");
    }

    #[test]
    fn haversine_sf_to_ny_is_large() {
        let d = haversine_km(SF.0, SF.1, 40.6994, -74.0156);
        assert!(d > 4000.0, "expected > 4000 km, got {d}");
    }

    // --- Unit: prediction parsing --------------------------------------------

    #[test]
    fn parse_predictions_maps_all_fields() {
        let stations = parse_stations(&stations_fixture());
        let sf_station = &stations[0];
        let (rows, raws) = parse_predictions(&predictions_fixture(), sf_station);
        assert_eq!(rows.len(), 4);
        assert_eq!(raws.len(), 4);

        let low = &rows[0];
        assert_eq!(low.source, SOURCE);
        assert_eq!(low.metric, "water_level");
        assert!((low.value - 0.97).abs() < 1e-6);
        assert_eq!(low.unit, "ft");
        assert_eq!(low.station, "9414290");
        assert_eq!(low.place, "San Francisco");
        assert_eq!(low.lat, Some(37.8063));
        assert_eq!(low.lon, Some(-122.4659));
        let guid = low.guid.as_deref().unwrap();
        assert!(guid.starts_with("9414290:pred:2026-06-10 02:11:L"), "guid={guid}");
        assert_eq!(low.extra.get("tide_type"), Some(&json!("L")));
        assert_eq!(low.extra.get("tide_label"), Some(&json!("Low Tide")));
        assert_eq!(low.extra.get("datum"), Some(&json!("MLLW")));
        assert_eq!(low.extra.get("prediction"), Some(&json!(true)));

        let high = &rows[1];
        assert!((high.value - 5.64).abs() < 1e-6);
        assert_eq!(high.extra.get("tide_label"), Some(&json!("High Tide")));
    }

    #[test]
    fn parse_predictions_returns_empty_on_error_response() {
        let stations = parse_stations(&stations_fixture());
        let (rows, raws) = parse_predictions(&predictions_error_fixture(), &stations[0]);
        assert!(rows.is_empty(), "error response should produce no rows");
        assert!(raws.is_empty());
    }

    #[test]
    fn parse_predictions_skips_rows_with_missing_t_or_bad_v() {
        let stations = parse_stations(&stations_fixture());
        let body = json!({
            "predictions": [
                { "v": "3.5", "type": "H" },      // missing t
                { "t": "2026-06-10 08:00", "v": "not_a_number", "type": "H" }, // bad v
                { "t": "2026-06-10 14:00", "v": "2.1", "type": "L" }  // valid
            ]
        });
        let (rows, _) = parse_predictions(&body, &stations[0]);
        assert_eq!(rows.len(), 1, "only the valid row should be kept");
        assert!((rows[0].value - 2.1).abs() < 1e-6);
    }

    // --- Unit: timestamp conversion ------------------------------------------

    #[test]
    fn coops_ts_to_local_parses_api_format() {
        // The API returns "YYYY-MM-DD HH:MM"; we convert to RFC3339.
        let ts = coops_ts_to_local("2026-06-10 02:11");
        assert!(
            DateTime::parse_from_rfc3339(&ts).is_ok(),
            "expected valid RFC3339, got: {ts}"
        );
        // The original date/time components must be preserved (same instant modulo offset).
        let dt = DateTime::parse_from_rfc3339(&ts).unwrap();
        // We attached the local offset, so local().naive_local() == original naïve.
        let local = dt.with_timezone(&Local);
        assert_eq!(local.hour(), 2, "hour preserved");
        assert_eq!(local.minute(), 11, "minute preserved");
    }

    #[test]
    fn coops_ts_to_local_passes_through_unparseable() {
        let bad = "not-a-date";
        assert_eq!(coops_ts_to_local(bad), bad, "unparseable passed through verbatim");
    }

    // --- Integration: pull_at_with -------------------------------------------

    #[test]
    fn pull_writes_readings_and_raw_updates_cursor() {
        let v = temp_vault("pull");
        let stub = Stub { stations: stations_fixture(), predictions: predictions_fixture() };
        let out = pull_at_with(&v, Some(SF), &stub).unwrap();
        assert_eq!(out.counts.get("predictions"), Some(&4));

        // Readings landed in environment/noaa-co-ops/
        let dir = v.root().join("environment/noaa-co-ops");
        assert!(dir.exists());
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .collect();
        assert!(!files.is_empty(), "at least one JSONL partition written");
        let content = std::fs::read_to_string(&files[0].path()).unwrap();
        assert!(content.contains("\"metric\":\"water_level\""));
        assert!(content.contains("\"unit\":\"ft\""));
        assert!(content.contains("\"station\":\"9414290\""));
        assert!(content.contains("\"tide_label\":\"Low Tide\"") || content.contains("\"tide_label\":\"High Tide\""));

        // Raw layer written.
        let raw_dir = v.root().join("environment/noaa-co-ops/raw");
        assert!(raw_dir.exists());
        let raw_files: Vec<_> = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(!raw_files.is_empty(), "raw partition written");
        let raw_content = std::fs::read_to_string(&raw_files[0].path()).unwrap();
        // Raw line carries original API fields verbatim.
        assert!(raw_content.contains("\"t\":"), "raw has t field");
        assert!(raw_content.contains("\"v\":"), "raw has v field");
        assert!(raw_content.contains("\"type\":"), "raw has type field");

        // Cursor advanced.
        let state = v.read_noaa_coops_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.station_id, "9414290");
        assert_eq!(state.station_name, "San Francisco");
        assert!(!state.next_begin_date.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn pull_skips_silently_when_no_nearby_station() {
        let v = temp_vault("inland");
        let stub = Stub { stations: stations_far_fixture(), predictions: json!({}) };
        // Denver: well inland, >50 km from The Battery NY.
        let out = pull_at_with(&v, Some(DENVER), &stub).unwrap();
        assert_eq!(out.counts.get("predictions"), Some(&0));
        // No readings or raw files written.
        assert!(!v.root().join("environment/noaa-co-ops").join("2026-06.jsonl").exists());
        // Cursor records the no-nearby-station reason.
        let state = v.read_noaa_coops_sync().unwrap();
        assert!(!state.error.is_empty(), "no-station reason recorded in cursor");
        assert!(state.station_id.is_empty(), "station_id stays empty when no station found");
    }

    #[test]
    fn pull_at_none_is_inert() {
        let v = temp_vault("inert");
        let stub = Stub { stations: stations_fixture(), predictions: predictions_fixture() };
        let out = pull_at_with(&v, None, &stub).unwrap();
        assert_eq!(out.counts.get("predictions"), Some(&0));
        assert!(!v.root().join("environment/noaa-co-ops").exists());
        assert!(v.read_noaa_coops_sync().is_none(), "no point ⇒ no cursor write");
    }

    #[test]
    fn prediction_repoll_does_not_duplicate_upserts_by_guid() {
        let v = temp_vault("dedup");
        let stub = Stub { stations: stations_fixture(), predictions: predictions_fixture() };
        // First poll.
        pull_at_with(&v, Some(SF), &stub).unwrap();
        // Second poll with the same predictions.
        let out2 = pull_at_with(&v, Some(SF), &stub).unwrap();
        assert_eq!(out2.counts.get("predictions"), Some(&4), "still seen and re-upserted");
        // Count the total rows across all partition files.
        let dir = v.root().join("environment/noaa-co-ops");
        let total: usize = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "jsonl").unwrap_or(false))
            .map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .count()
            })
            .sum();
        assert_eq!(total, 4, "upsert by guid — 4 rows, no duplicates after re-poll");
    }

    #[test]
    fn api_error_response_degrades_gracefully() {
        let v = temp_vault("api-error");
        let stub = Stub { stations: stations_fixture(), predictions: predictions_error_fixture() };
        let out = pull_at_with(&v, Some(SF), &stub).unwrap();
        assert_eq!(out.counts.get("predictions"), Some(&0), "error response → 0 rows");
        // No partition file written.
        assert!(!v.root().join("environment/noaa-co-ops/2026-06.jsonl").exists());
        // Cursor still advanced (station was found, window was attempted).
        let state = v.read_noaa_coops_sync().unwrap();
        assert_eq!(state.station_id, "9414290");
        assert!(state.error.is_empty(), "API error is graceful — no error banner");
    }

    #[test]
    fn sync_state_round_trips_and_back_compat() {
        let empty: NoaaCoopsSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.lat, 0.0);
        assert!(empty.updated.is_empty());
        assert!(empty.station_id.is_empty());

        let v = temp_vault("sync");
        v.write_noaa_coops_sync(&NoaaCoopsSyncState {
            updated: "2026-06-10T08:00:00-07:00".into(),
            lat: 37.80,
            lon: -122.40,
            station_id: "9414290".into(),
            station_name: "San Francisco".into(),
            next_begin_date: "20260711".into(),
            error: String::new(),
        })
        .unwrap();
        let s = v.read_noaa_coops_sync().unwrap();
        assert_eq!(s.station_id, "9414290");
        assert_eq!(s.next_begin_date, "20260711");
        assert!(s.error.is_empty());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Back-compat: a minimal reading line (only the 4 required fields) must
        // parse without error — proves the upsert read path tolerates sparse data.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/noaa-co-ops")).unwrap();
        std::fs::write(
            v.root().join("environment/noaa-co-ops/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T02:11:00-07:00\",\"source\":\"noaa-co-ops\",\"metric\":\"water_level\",\"value\":0.97}\n",
        )
        .unwrap();
        let readings: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(readings.len(), 1);
        assert!((readings[0].value - 0.97).abs() < 1e-6);
        assert_eq!(readings[0].guid, None, "sparse reading has no guid");
    }
}
