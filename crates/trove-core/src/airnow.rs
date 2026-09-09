//! AirNow (EPA AQI) — US/Canada ground-monitor air quality index.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/airnow.md
//!
//! A **Periodic** cloud pull: current AQI observations from the nearest
//! reporting area to the user's configured location, pulled hourly from the
//! US EPA's free public API. Writes the **`environment`** domain's scalar
//! reading shape ([`EnvReading`]):
//!
//! - **contract layer** — `environment/airnow/YYYY-MM.jsonl` — one
//!   [`EnvReading`] per pollutant per poll, keyed by
//!   `airnow-<ParameterName>-<YYYY-MM-DD HH:00 TZ>` (upserts — a re-poll of
//!   the same observation hour+pollutant updates in place).
//! - **raw layer** — `environment/airnow/raw/YYYY-MM.jsonl` — the API
//!   observation objects at full fidelity, one line per API object per poll
//!   (UNCONDITIONAL; full fidelity).
//!
//! **Auth:** a free API key pasted by the user via the connect card
//! ([`ConnectMethod::TokenPaste`]), stored in the 0600 secret store under
//! `.trove/sync/airnow.json`. The key is a plain opaque string issued
//! instantly by email registration at docs.airnowapi.org — no OAuth.
//!
//! **US/Canada only:** the API returns an empty array when no reporting area
//! covers the requested coordinates. The collector degrades gracefully (zero
//! rows, no error) so non-US users don't see failures.
//!
//! **Location:** reuses the same weather location ladder as
//! [`crate::nws`] and [`crate::weather`]: CoreLocation → manual weather
//! location → last-used cursor location.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvReading;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer reading directory; raw lines in `raw/`.
const DIR: &str = "environment/airnow";
const RAW_DIR: &str = "environment/airnow/raw";
/// Non-secret rebuildable cursor — last location used + last poll time.
const SYNC_FILE: &str = ".trove/airnow-sync.json";

const SERVICE: &str = "airnow";
const SOURCE: &str = "airnow";
const API_BASE: &str = "https://www.airnowapi.org";
/// Default search radius in miles (AirNow default).
const DISTANCE_MILES: u32 = 25;
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Hourly cadence: AQI observations update about once per hour per reporting
/// area, so polling more often is noise.
pub const AIRNOW_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_airnow_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join(DIR)))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        required: false, // manual location also works
    }
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("airnow synced — {n} AQI readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "airnow sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "AirNow: nothing new (no reporting area or already up to date)".to_string()
    } else {
        format!("AirNow synced — {n} AQI readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "airnow",
        name: "AirNow (EPA AQI)",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls hourly AQI readings from the US EPA's AirNow ground-monitor \
                      network. Covers the US and Canada; falls back to Open-Meteo air \
                      quality for users outside that coverage area.",
        domain: "environment",
        vault_path: "environment/airnow/",
        toggleable: true,
        setup: &[
            "Register for a free API key at docs.airnowapi.org (instant, email registration).",
            "Paste the key in the connect card.",
            "Approve Location Services when asked, or set a location manually on the Weather tab.",
        ],
        caveats: "US and Canada only — outside that coverage area the API returns no data \
                  (no error; the collector is simply inert). Requires Location Services or \
                  a manually set location.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(AIRNOW_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: Some("airnow"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — the free API key).

fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API key must not be empty");
    }
    // Store the key verbatim in the 0600 secret store.
    let token = crate::sync::oauth::TokenSet {
        access_token: key.to_string(),
        refresh_token: None,
        token_type: None,
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &token)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(_token) = vault.load_sync_token(SERVICE)? {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "AirNow API key (configured)".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    let configured = !accounts.is_empty();
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "airnow",
    display_name: "AirNow (EPA AQI)",
    methods: &[ConnectMethod::TokenPaste {
        label: "AirNow API key",
        help: "Free key issued instantly by email registration at docs.airnowapi.org — \
               sign up, verify your email, and your key appears in the upper-right corner \
               of the Web Services page.",
        placeholder: "XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["airnow"],
    setup: &[
        "Register for a free API key at docs.airnowapi.org.",
        "Verify your email; your key appears in the Web Services page upper-right corner.",
        "Paste it here and connect.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state: last-used location + last poll time.
/// Losing it costs nothing — the next pass re-resolves the location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct AirNowSyncState {
    /// RFC3339 local time of the last pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Last-used latitude (0.0 = unset).
    #[serde(default)]
    pub lat: f64,
    /// Last-used longitude (0.0 = unset).
    #[serde(default)]
    pub lon: f64,
    /// The reporting area label last seen.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    /// Non-empty while stuck (no location, no key, network error) — for the UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_airnow_sync(&self) -> Option<AirNowSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_airnow_sync(&self, state: &AirNowSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API shapes — confirmed from the official AirNow API (field names verified
// against the Home Assistant airnow integration const.py:
// DateObserved, HourObserved, LocalTimeZone, ReportingArea, StateCode,
// Latitude, Longitude, ParameterName, AQI, Category.{Number,Name}).

/// One AirNow observation object as returned by the API. Fields match the
/// documented JSON names exactly (PascalCase, confirmed against multiple
/// open-source AirNow integrations).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct AirNowObs {
    /// Date string `"YYYY-MM-DD "` (trailing space is common in practice).
    #[serde(default)]
    date_observed: String,
    /// Hour of observation in the local timezone (0–23).
    #[serde(default)]
    hour_observed: i32,
    /// Timezone abbreviation (`"PST"`, `"EST"`, `"PDT"`, …).
    #[serde(default)]
    local_time_zone: String,
    /// Human label of the nearest reporting area (`"Los Angeles-Long Beach"`, …).
    #[serde(default)]
    reporting_area: String,
    /// Two-letter state code (`"CA"`, `"NY"`, …).
    #[serde(default)]
    state_code: String,
    /// Latitude of the reporting station (not the queried point).
    #[serde(default)]
    latitude: f64,
    /// Longitude of the reporting station.
    #[serde(default)]
    longitude: f64,
    /// Pollutant name (`"PM2.5"`, `"PM10"`, `"O3"`, `"CO"`, `"NO2"`, `"SO2"`).
    #[serde(default)]
    parameter_name: String,
    /// NowCast AQI value for this pollutant (integer, but typed f64 for
    /// compatibility with the contract's `value: f64`).
    #[serde(default, rename = "AQI")]
    aqi: f64,
    /// AQI category: `{Number: 1–6, Name: "Good" / "Moderate" / … }`.
    #[serde(default)]
    category: AirNowCategory,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct AirNowCategory {
    #[serde(default)]
    number: i32,
    #[serde(default)]
    name: String,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

trait AirNowApi {
    /// `GET /aq/observation/latLong/current/?latitude=LAT&longitude=LON&…`
    /// Returns the raw JSON array from the API, or an error.
    fn current_obs(&self, lat: f64, lon: f64, key: &str) -> Result<Value>;
}

struct AirNowClient {
    base: String,
}

impl AirNowClient {
    fn new(base: String) -> Self {
        AirNowClient { base }
    }
}

impl AirNowApi for AirNowClient {
    fn current_obs(&self, lat: f64, lon: f64, key: &str) -> Result<Value> {
        let url = format!(
            "{}/aq/observation/latLong/current/?format=application/json\
             &latitude={lat}&longitude={lon}&distance={DISTANCE_MILES}&API_KEY={key}",
            self.base
        );
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("requesting AirNow current observations")?
            .into_json()
            .context("reading AirNow response")
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Build a timestamp string from the AirNow observation fields.
/// AirNow returns date as `"YYYY-MM-DD "` (often with trailing space) and
/// hour as an integer 0–23 with a timezone abbreviation string.
/// We synthesize `YYYY-MM-DD HH:00 TZ` as the canonical observation key.
fn obs_timestamp_key(obs: &AirNowObs) -> String {
    let date = obs.date_observed.trim();
    let hour = obs.hour_observed;
    let tz = obs.local_time_zone.trim();
    format!("{date} {:02}:00 {tz}", hour)
}

/// Map AirNow's `LocalTimeZone` abbreviation to a fixed UTC offset in seconds.
/// Covers the US and Canadian time zones that AirNow reporting areas use.
/// Returns `None` for unknown abbreviations (fallback: use machine local time).
fn tz_abbr_to_offset_secs(abbr: &str) -> Option<i32> {
    // (hours west * 3600 negated = seconds east)
    match abbr {
        "HST"  => Some(-10 * 3600),       // Hawaii Standard
        "AKST" => Some(-9 * 3600),        // Alaska Standard
        "AKDT" => Some(-8 * 3600),        // Alaska Daylight
        "PST"  => Some(-8 * 3600),        // Pacific Standard
        "PDT"  => Some(-7 * 3600),        // Pacific Daylight
        "MST"  => Some(-7 * 3600),        // Mountain Standard (also AZ no-DST)
        "MDT"  => Some(-6 * 3600),        // Mountain Daylight
        "CST"  => Some(-6 * 3600),        // Central Standard
        "CDT"  => Some(-5 * 3600),        // Central Daylight
        "EST"  => Some(-5 * 3600),        // Eastern Standard
        "EDT"  => Some(-4 * 3600),        // Eastern Daylight
        "NST"  => Some(-(3 * 3600 + 1800)), // Newfoundland Standard (-3:30)
        "NDT"  => Some(-(2 * 3600 + 1800)), // Newfoundland Daylight (-2:30)
        "AST"  => Some(-4 * 3600),        // Atlantic Standard (some CA coverage)
        "ADT"  => Some(-3 * 3600),        // Atlantic Daylight
        _ => None,
    }
}

/// Parse one AirNow observation object into a contract [`EnvReading`] +
/// the raw [`Value`] for the raw layer.
///
/// `guid` = `airnow-<ReportingArea>-<ParameterName>-<date HH:00 TZ>` so a
/// re-poll of the same observation hour+pollutant+area upserts rather than
/// duplicates, and two different reporting areas never collide.
fn parse_obs(obs: &AirNowObs, queried_lat: f64, queried_lon: f64) -> Option<EnvReading> {
    // Skip observations with no usable data.
    if obs.parameter_name.is_empty() || obs.date_observed.trim().is_empty() {
        return None;
    }
    // AQI = -1 is the API's sentinel for "no data for this pollutant".
    if obs.aqi < 0.0 {
        return None;
    }

    let ts_key = obs_timestamp_key(obs);
    // Include the reporting area in the guid so two different areas at the
    // same pollutant + hour + TZ produce distinct, non-colliding keys.
    let area_slug = obs.reporting_area.replace(' ', "_").replace(',', "");
    let guid = format!("airnow-{}-{}-{}", area_slug, obs.parameter_name, ts_key);

    // Build the RFC3339 ts: we have date + hour + TZ abbreviation.
    // Map the abbreviation to a known fixed offset so the absolute instant
    // on the timeline is correct regardless of the machine's local TZ.
    // (A US-East user querying a West-coast reporting area must not have the
    // ts shifted by the machine offset.)
    let ts = {
        let date_trimmed = obs.date_observed.trim();
        let hour = obs.hour_observed;
        let naive_str = format!("{} {:02}:00:00", date_trimmed, hour);
        // Try to map the TZ abbreviation to a fixed offset.
        let offset_secs = tz_abbr_to_offset_secs(obs.local_time_zone.trim());
        match (NaiveDateTime::parse_from_str(&naive_str, "%Y-%m-%d %H:%M:%S"), offset_secs) {
            (Ok(ndt), Some(secs)) => {
                // Build a FixedOffset DateTime from the reporting area's own offset.
                let offset = FixedOffset::east_opt(secs).unwrap_or(FixedOffset::east_opt(0).unwrap());
                offset
                    .from_local_datetime(&ndt)
                    .single()
                    .map(|dt: DateTime<FixedOffset>| dt.to_rfc3339())
                    .unwrap_or_else(|| Local::now().to_rfc3339())
            }
            (Ok(ndt), None) => {
                // Unknown TZ abbreviation — best-effort: use machine local offset.
                // Documents the deviation for diagnostic purposes.
                let now = Local::now();
                let offset = now.offset();
                DateTime::<FixedOffset>::from_naive_utc_and_offset(
                    ndt - chrono::Duration::seconds(offset.local_minus_utc() as i64),
                    *offset,
                )
                .to_rfc3339()
            }
            (Err(_), _) => Local::now().to_rfc3339(),
        }
    };

    // metric: normalize the pollutant name to the environment domain vocabulary.
    // AirNow returns "PM2.5", "PM10", "O3", "CO", "NO2", "SO2".
    // The domain spec (environment.md) mandates "ozone" not "o3" so cross-source
    // merge on metric works (a home ozone sensor writes "ozone"; we must match).
    let metric = match obs.parameter_name.as_str() {
        "PM2.5" => "pm25",
        "PM10" => "pm10",
        "O3"   => "ozone",
        "CO"   => "co",
        "NO2"  => "no2",
        "SO2"  => "so2",
        other => {
            // Unknown pollutant: use the raw name lowercased, dots → underscores.
            &*Box::leak(
                other.to_lowercase().replace('.', "_").into_boxed_str()
            )
        }
    };

    // Build extra: category info + queried point + state + raw timestamp key.
    // The queried point goes into extra (it's a lookup input, not the obs location).
    let mut extra = Map::new();
    extra.insert("aqi_raw".into(), Value::Number(serde_json::Number::from_f64(obs.aqi).unwrap_or_else(|| serde_json::Number::from(0))));
    extra.insert("parameter_name".into(), Value::String(obs.parameter_name.clone()));
    extra.insert("category_number".into(), Value::Number(obs.category.number.into()));
    if !obs.category.name.is_empty() {
        extra.insert("category_name".into(), Value::String(obs.category.name.clone()));
    }
    if !obs.state_code.is_empty() {
        extra.insert("state_code".into(), Value::String(obs.state_code.clone()));
    }
    // Store the user's queried location in extra (separate from the observation's lat/lon).
    extra.insert(
        "queried_lat".into(),
        Value::Number(serde_json::Number::from_f64(queried_lat).unwrap_or(serde_json::Number::from(0))),
    );
    extra.insert(
        "queried_lon".into(),
        Value::Number(serde_json::Number::from_f64(queried_lon).unwrap_or(serde_json::Number::from(0))),
    );
    extra.insert("obs_key".into(), Value::String(ts_key));

    // lat/lon: use the observation/station coordinates (the reporting station's
    // position), not the queried user point. The schema describes lat/lon as
    // "latitude/longitude of the observation". The queried point is preserved
    // in extra above for reference.
    let obs_lat = if obs.latitude != 0.0 { Some(obs.latitude) } else { None };
    let obs_lon = if obs.longitude != 0.0 { Some(obs.longitude) } else { None };

    Some(EnvReading {
        ts,
        source: SOURCE.into(),
        metric: metric.to_string(),
        value: obs.aqi,
        unit: "aqi".into(),
        place: obs.reporting_area.clone(),
        lat: obs_lat,
        lon: obs_lon,
        // The current-by-latLong endpoint returns no station id; leave empty
        // rather than mis-populate with the human area label (which is in `place`).
        station: String::new(),
        guid: Some(guid),
        extra,
    })
}

/// Parse a full current-observation API response body into (readings, raws).
/// Empty array / non-array → empty vecs (US/Canada only — no error for zero
/// results, as non-US locations return `[]` legitimately).
fn parse_response(
    body: &Value,
    queried_lat: f64,
    queried_lon: f64,
) -> (Vec<EnvReading>, Vec<Value>) {
    let arr = match body.as_array() {
        Some(a) => a,
        None => return (Vec::new(), Vec::new()),
    };
    let mut readings = Vec::new();
    let mut raws = Vec::new();
    for item in arr {
        // Deserialize into our typed shape; skip items that fail.
        let obs: AirNowObs = match serde_json::from_value(item.clone()) {
            Ok(o) => o,
            Err(_) => {
                raws.push(item.clone()); // still keep raw even if parse fails
                continue;
            }
        };
        if let Some(r) = parse_obs(&obs, queried_lat, queried_lon) {
            readings.push(r);
        }
        raws.push(item.clone());
    }
    (readings, raws)
}

// ---------------------------------------------------------------------------
// Upsert into stable month partitions (nws.rs / toggl_track.rs pattern).

/// Upsert readings into `environment/airnow/YYYY-MM.jsonl` by `guid`.
/// A re-poll of the same observation hour+pollutant updates in place.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvReading>> = Default::default();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!(
                "airnow reading {:?} has unpartitionable ts {:?}",
                r.guid, r.ts
            )
        })?;
        by_key.entry(key.to_string()).or_default().push(r);
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

/// Upsert raw observation objects into `environment/airnow/raw/YYYY-MM.jsonl`.
/// Raw lines are deduped by the same guid logic as the contract rows.
fn upsert_raw(vault: &Vault, raws: &[Value], ts: &str) -> Result<()> {
    let key = Partition::Month.key(ts).context("airnow raw partition key")?;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut existing: Vec<Value> = stream.read(&key)?;
    for item in raws {
        // Dedupe by guid: ParameterName + obs key.
        let item_guid = raw_guid(item);
        if item_guid.is_empty() {
            existing.push(item.clone());
        } else {
            match existing.iter_mut().find(|v| raw_guid(v) == item_guid) {
                Some(slot) => *slot = item.clone(),
                None => existing.push(item.clone()),
            }
        }
    }
    vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)
}

/// Compute the guid of a raw object.
/// Format: `airnow-<ReportingArea_slug>-<ParameterName>-<DateObserved HH:00 TZ>`
/// The reporting area is included so two different areas at the same pollutant +
/// hour produce distinct guids (mirrors the contract guid format).
fn raw_guid(v: &Value) -> String {
    let param = v
        .get("ParameterName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let date = v
        .get("DateObserved")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let hour = v.get("HourObserved").and_then(Value::as_i64).unwrap_or(-1);
    let tz = v
        .get("LocalTimeZone")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let area = v
        .get("ReportingArea")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .replace(' ', "_")
        .replace(',', "");
    if param.is_empty() || date.is_empty() || hour < 0 {
        return String::new();
    }
    format!("airnow-{area}-{param}-{date} {:02}:00 {tz}", hour)
}

// ---------------------------------------------------------------------------
// The pull.

/// Load the API key from the 0600 secret store.
fn load_api_key(vault: &Vault) -> Result<String> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("AirNow API key not configured — connect via the hub card")?;
    let key = token.access_token.trim().to_string();
    if key.is_empty() {
        bail!("AirNow API key is empty — reconnect via the hub card");
    }
    Ok(key)
}

/// Resolve the user's location using the same ladder as [`crate::nws`]:
/// CoreLocation → manual weather location → cursor.
fn resolve_point(vault: &Vault, state: &AirNowSyncState) -> Option<(f64, f64)> {
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

/// Production entry point: resolve the location ladder and sync.
/// Inert when no location is available (no rows, no error).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_airnow_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error =
            "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_airnow_sync(&state)?;
        return Ok(PullOutcome {
            headline: "AirNow: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }
    let client = AirNowClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &client)
}

/// Network+write body over an explicit point. `None` ⇒ inert no-op (never
/// touched in production; called by `pull` after location resolution).
/// Tests drive this directly to avoid touching CoreLocation.
pub fn pull_at(vault: &Vault, point: Option<(f64, f64)>) -> Result<PullOutcome> {
    let client = AirNowClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &client)
}

/// Offline-testable pull body: explicit point + injected API.
fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    client: &impl AirNowApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "AirNow: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    };

    let key = load_api_key(vault)?;
    let now = Local::now();
    let mut state = vault.read_airnow_sync().unwrap_or_default();

    let body = client.current_obs(lat, lon, &key)?;
    let (readings, raws) = parse_response(&body, lat, lon);

    let readings_written = if !readings.is_empty() {
        upsert_readings(vault, &readings)?
    } else {
        0
    };

    // Raw layer: unconditional full fidelity.
    // Partition by the first reading's ts (observation time) so raw and contract
    // rows for the same observation always land in the same YYYY-MM file.
    // Fall back to poll time only when there are no parseable readings.
    if !raws.is_empty() {
        let raw_ts = readings
            .first()
            .map(|r| r.ts.clone())
            .unwrap_or_else(|| now.to_rfc3339());
        upsert_raw(vault, &raws, &raw_ts)?;
    }

    // Advance cursor.
    let place = readings
        .first()
        .map(|r| r.place.clone())
        .unwrap_or_default();
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.place = place.clone();
    state.error = String::new();
    vault.write_airnow_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{readings_written} AQI readings"),
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
        let dir =
            std::env::temp_dir().join(format!("trove-airnow-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Real-shaped multi-pollutant current observation response.
    /// Field names confirmed from official AirNow API (verified against
    /// Home Assistant airnow/const.py and multiple open-source clients).
    fn obs_two_pollutants() -> Value {
        json!([
            {
                "DateObserved": "2026-06-15 ",
                "HourObserved": 10,
                "LocalTimeZone": "PDT",
                "ReportingArea": "Los Angeles-Long Beach",
                "StateCode": "CA",
                "Latitude": 34.0522,
                "Longitude": -118.2437,
                "ParameterName": "PM2.5",
                "AQI": 42,
                "Category": { "Number": 1, "Name": "Good" }
            },
            {
                "DateObserved": "2026-06-15 ",
                "HourObserved": 10,
                "LocalTimeZone": "PDT",
                "ReportingArea": "Los Angeles-Long Beach",
                "StateCode": "CA",
                "Latitude": 34.0522,
                "Longitude": -118.2437,
                "ParameterName": "O3",
                "AQI": 78,
                "Category": { "Number": 2, "Name": "Moderate" }
            }
        ])
    }

    /// Single-pollutant sparse response (only PM2.5; O3 data absent).
    fn obs_single_pollutant() -> Value {
        json!([
            {
                "DateObserved": "2026-06-15 ",
                "HourObserved": 11,
                "LocalTimeZone": "PDT",
                "ReportingArea": "San Francisco",
                "StateCode": "CA",
                "Latitude": 37.7749,
                "Longitude": -122.4194,
                "ParameterName": "PM2.5",
                "AQI": 15,
                "Category": { "Number": 1, "Name": "Good" }
            }
        ])
    }

    /// Updated observation for same hour + pollutant (revised AQI).
    fn obs_two_pollutants_revised() -> Value {
        json!([
            {
                "DateObserved": "2026-06-15 ",
                "HourObserved": 10,
                "LocalTimeZone": "PDT",
                "ReportingArea": "Los Angeles-Long Beach",
                "StateCode": "CA",
                "Latitude": 34.0522,
                "Longitude": -118.2437,
                "ParameterName": "PM2.5",
                "AQI": 55,
                "Category": { "Number": 2, "Name": "Moderate" }
            },
            {
                "DateObserved": "2026-06-15 ",
                "HourObserved": 10,
                "LocalTimeZone": "PDT",
                "ReportingArea": "Los Angeles-Long Beach",
                "StateCode": "CA",
                "Latitude": 34.0522,
                "Longitude": -118.2437,
                "ParameterName": "O3",
                "AQI": 85,
                "Category": { "Number": 2, "Name": "Moderate" }
            }
        ])
    }

    /// Empty response (non-US/Canada location, or no reporting area nearby).
    fn obs_empty() -> Value {
        json!([])
    }

    /// Response with a -1 AQI sentinel (no data for this pollutant).
    fn obs_no_data_sentinel() -> Value {
        json!([
            {
                "DateObserved": "2026-06-15 ",
                "HourObserved": 10,
                "LocalTimeZone": "PDT",
                "ReportingArea": "Los Angeles-Long Beach",
                "StateCode": "CA",
                "Latitude": 34.0522,
                "Longitude": -118.2437,
                "ParameterName": "PM2.5",
                "AQI": -1,
                "Category": { "Number": 0, "Name": "Unavailable" }
            }
        ])
    }

    // A stub client.
    struct Stub {
        body: Value,
    }
    impl AirNowApi for Stub {
        fn current_obs(&self, _lat: f64, _lon: f64, _key: &str) -> Result<Value> {
            Ok(self.body.clone())
        }
    }

    const SEED: (f64, f64) = (34.05, -118.24);

    // Inject a test API key so load_api_key doesn't bail.
    fn seed_key(vault: &Vault) {
        let token = crate::sync::oauth::TokenSet {
            access_token: "test-api-key".to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        };
        vault.save_sync_token(SERVICE, &token).unwrap();
    }

    // -----------------------------------------------------------------------
    // Pure parser tests.

    #[test]
    fn parse_response_maps_contract_fields() {
        let (rows, raws) = parse_response(&obs_two_pollutants(), 34.05, -118.24);
        assert_eq!(rows.len(), 2);
        assert_eq!(raws.len(), 2);

        let pm25 = rows.iter().find(|r| r.metric == "pm25").unwrap();
        assert_eq!(pm25.source, "airnow");
        assert_eq!(pm25.value, 42.0);
        assert_eq!(pm25.unit, "aqi");
        assert_eq!(pm25.place, "Los Angeles-Long Beach");
        // lat/lon are the station/reporting-area coords from the API (not the queried point).
        assert_eq!(pm25.lat, Some(34.0522));
        assert_eq!(pm25.lon, Some(-118.2437));
        // station field is left empty — the latLong endpoint returns no station id.
        assert!(pm25.station.is_empty(), "station must be empty (no id from this endpoint)");
        // queried point moved to extra.
        assert_eq!(pm25.extra.get("queried_lat"), Some(&json!(34.05)));
        assert_eq!(pm25.extra.get("queried_lon"), Some(&json!(-118.24)));
        // guid includes the reporting area + pollutant + obs key.
        let guid = pm25.guid.as_deref().unwrap_or("");
        assert!(guid.contains("PM2.5"), "guid must carry ParameterName; got={guid}");
        assert!(guid.contains("Los_Angeles-Long_Beach"), "guid must carry reporting area; got={guid}");
        // extra carries category info.
        assert_eq!(pm25.extra.get("category_name"), Some(&json!("Good")));
        assert_eq!(pm25.extra.get("category_number"), Some(&json!(1)));
        assert_eq!(pm25.extra.get("state_code"), Some(&json!("CA")));
        assert_eq!(pm25.extra.get("parameter_name"), Some(&json!("PM2.5")));

        // O3 → "ozone" per domain vocabulary.
        let ozone = rows.iter().find(|r| r.metric == "ozone").unwrap();
        assert_eq!(ozone.value, 78.0);
        assert_eq!(ozone.extra.get("category_name"), Some(&json!("Moderate")));
    }

    #[test]
    fn parse_skips_negative_aqi_sentinel() {
        let (rows, raws) = parse_response(&obs_no_data_sentinel(), 34.05, -118.24);
        // The -1 AQI observation must be skipped from contract rows.
        assert_eq!(rows.len(), 0, "negative AQI is a no-data sentinel — skip");
        // Raw layer still keeps it for full fidelity.
        assert_eq!(raws.len(), 1, "raw keeps all objects verbatim");
    }

    #[test]
    fn parse_empty_response_is_silent() {
        let (rows, raws) = parse_response(&obs_empty(), 34.05, -118.24);
        assert_eq!(rows.len(), 0);
        assert_eq!(raws.len(), 0);
    }

    #[test]
    fn parameter_name_normalized_to_metric() {
        // PM2.5 → pm25, O3 → ozone (domain vocabulary from environment.md).
        let (rows, _) = parse_response(&obs_two_pollutants(), 34.05, -118.24);
        let metrics: Vec<_> = rows.iter().map(|r| r.metric.as_str()).collect();
        assert!(metrics.contains(&"pm25"), "PM2.5 → pm25");
        assert!(metrics.contains(&"ozone"), "O3 → ozone (domain vocabulary)");
        assert!(!metrics.contains(&"o3"), "o3 must NOT appear — breaks cross-source merge");
    }

    #[test]
    fn guid_encodes_area_pollutant_and_obs_hour() {
        let (rows, _) = parse_response(&obs_two_pollutants(), 34.05, -118.24);
        let pm25 = rows.iter().find(|r| r.metric == "pm25").unwrap();
        let guid = pm25.guid.as_deref().unwrap_or("");
        // Must include the reporting area, ParameterName, and the observation hour.
        assert!(guid.contains("PM2.5"), "guid must carry ParameterName; got={guid}");
        assert!(guid.contains("10:00"), "guid must carry the observation hour; got={guid}");
        assert!(guid.contains("Los_Angeles"), "guid must carry reporting area; got={guid}");

        // Two different reporting areas with same pollutant + hour must NOT share guid.
        let (sf_rows, _) = parse_response(&obs_single_pollutant(), 37.77, -122.41);
        let sf_pm25 = sf_rows.iter().find(|r| r.metric == "pm25").unwrap();
        let sf_guid = sf_pm25.guid.as_deref().unwrap_or("");
        // SF obs is hour 11 so different anyway, but also area differs.
        assert_ne!(guid, sf_guid, "different reporting areas must produce different guids");
        assert!(sf_guid.contains("San_Francisco"), "SF guid must carry its area; got={sf_guid}");
    }

    #[test]
    fn ts_uses_reporting_area_tz_not_machine_tz() {
        // PDT = UTC-7. The observation at 2026-06-15 10:00 PDT should be
        // 2026-06-15T10:00:00-07:00 regardless of the machine's local timezone.
        let (rows, _) = parse_response(&obs_two_pollutants(), 34.05, -118.24);
        let pm25 = rows.iter().find(|r| r.metric == "pm25").unwrap();
        // The ts must carry -07:00 offset (PDT fixed), not the machine's local offset.
        assert!(
            pm25.ts.contains("-07:00"),
            "ts must encode PDT (-07:00) from the static TZ table; got={}",
            pm25.ts,
        );
        // The wall-clock hour must be 10 (as reported by AirNow), not offset-shifted.
        assert!(pm25.ts.starts_with("2026-06-15T10:00"), "wall-clock hour must be 10; got={}", pm25.ts);
    }

    // -----------------------------------------------------------------------
    // Pull / store / dedupe tests.

    #[test]
    fn pull_writes_readings_and_raw() {
        let v = temp_vault("pull");
        seed_key(&v);
        let stub = Stub { body: obs_two_pollutants() };
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&2));

        // Contract readings in environment/airnow/YYYY-MM.jsonl.
        let june = v.root().join("environment/airnow/2026-06.jsonl");
        assert!(june.exists(), "contract file must exist");
        let content = std::fs::read_to_string(&june).unwrap();
        assert_eq!(content.lines().count(), 2, "one line per pollutant");
        assert!(content.contains("\"metric\":\"pm25\""));
        assert!(content.contains("\"metric\":\"ozone\""), "O3 must be stored as 'ozone'");
        assert!(content.contains("\"unit\":\"aqi\""));
        assert!(content.contains("\"source\":\"airnow\""));

        // Raw layer: environment/airnow/raw/YYYY-MM.jsonl.
        let raw_file = v.root().join("environment/airnow/raw/2026-06.jsonl");
        assert!(raw_file.exists(), "raw file must exist");
        let raw_content = std::fs::read_to_string(&raw_file).unwrap();
        assert_eq!(raw_content.lines().count(), 2, "one raw line per API object");
        assert!(raw_content.contains("\"ParameterName\""), "raw keeps original field names");
        assert!(raw_content.contains("\"PM2.5\""));

        // Cursor advanced.
        let state = v.read_airnow_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.lat, SEED.0);
        assert_eq!(state.place, "Los Angeles-Long Beach");
        assert!(state.error.is_empty());
    }

    #[test]
    fn repoll_same_hour_upserts_not_duplicates() {
        let v = temp_vault("upsert");
        seed_key(&v);
        // First poll.
        pull_at_with(&v, Some(SEED), &Stub { body: obs_two_pollutants() }).unwrap();
        let rows1: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows1.len(), 2);
        let pm25_before = rows1.iter().find(|r| r.metric == "pm25").unwrap().value;
        assert_eq!(pm25_before, 42.0);

        // Second poll with revised AQI — same hour + pollutant → upsert.
        let out2 = pull_at_with(
            &v,
            Some(SEED),
            &Stub { body: obs_two_pollutants_revised() },
        )
        .unwrap();
        assert_eq!(out2.counts.get("readings"), Some(&2));

        let rows2: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows2.len(), 2, "upsert — no duplicate rows");
        let pm25_after = rows2.iter().find(|r| r.metric == "pm25").unwrap().value;
        assert_eq!(pm25_after, 55.0, "revised AQI updated in place");
        let ozone_after = rows2.iter().find(|r| r.metric == "ozone").unwrap().value;
        assert_eq!(ozone_after, 85.0, "ozone (O3) also updated in place");
    }

    #[test]
    fn different_hours_accumulate_not_overwrite() {
        let v = temp_vault("accumulate");
        seed_key(&v);
        // Hour 10.
        pull_at_with(&v, Some(SEED), &Stub { body: obs_two_pollutants() }).unwrap();
        // Hour 11 — single pollutant at a different location (different guid).
        pull_at_with(&v, Some((37.77, -122.41)), &Stub { body: obs_single_pollutant() }).unwrap();

        // Both month files should have accumulated.
        let rows: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 3, "2 @ hr10 + 1 @ hr11 = 3 distinct readings");
    }

    #[test]
    fn empty_response_writes_no_contract_rows() {
        let v = temp_vault("empty");
        seed_key(&v);
        let out =
            pull_at_with(&v, Some(SEED), &Stub { body: obs_empty() }).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(
            !v.root().join("environment/airnow/2026-06.jsonl").exists(),
            "no contract file when no readings"
        );
        // Raw file also absent (nothing to write).
        assert!(!v.root().join("environment/airnow/raw").exists());
        let state = v.read_airnow_sync().unwrap();
        assert!(state.error.is_empty(), "empty result is not an error");
    }

    #[test]
    fn no_point_is_inert() {
        let v = temp_vault("inert");
        seed_key(&v);
        let out = pull_at(&v, None).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/airnow").exists());
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("sync");
        let state = AirNowSyncState {
            updated: "2026-06-15T10:00:00-07:00".into(),
            lat: 34.05,
            lon: -118.24,
            place: "Los Angeles-Long Beach".into(),
            error: String::new(),
        };
        v.write_airnow_sync(&state).unwrap();
        let loaded = v.read_airnow_sync().unwrap();
        assert_eq!(loaded.lat, 34.05);
        assert_eq!(loaded.place, "Los Angeles-Long Beach");
        assert!(loaded.error.is_empty());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Back-compat: a sparse reading line (only the 4 required fields) must
        // parse cleanly via the upsert read path.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/airnow")).unwrap();
        std::fs::write(
            v.root().join("environment/airnow/2026-06.jsonl"),
            "{\"ts\":\"2026-06-15T10:00:00-07:00\",\"source\":\"airnow\",\"metric\":\"pm25\",\"value\":42}\n",
        )
        .unwrap();
        let rows: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metric, "pm25");
        assert_eq!(rows[0].value, 42.0);
        assert_eq!(rows[0].guid, None, "sparse line has no guid");
    }

    #[test]
    fn connection_status_reflects_stored_key() {
        let v = temp_vault("conn-status");
        let s1 = def_status(&v).unwrap();
        assert!(!s1.configured, "no key stored yet");
        seed_key(&v);
        let s2 = def_status(&v).unwrap();
        assert!(s2.configured, "key stored → configured");
        def_disconnect(&v, "airnow").unwrap();
        let s3 = def_status(&v).unwrap();
        assert!(!s3.configured, "disconnected → not configured");
    }

    #[test]
    fn connect_rejects_empty_key() {
        let v = temp_vault("empty-key");
        assert!(def_connect(&v, "").is_err());
        assert!(def_connect(&v, "   ").is_err());
    }
}
