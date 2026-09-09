//! Weather collector — hourly current conditions at the user's location,
//! from the Open-Meteo API.
//!
//! **Why Open-Meteo** (decided with David 2026-06-11): no API key or account
//! at all — every user's machine calls it directly and anonymously, so there
//! is no signup friction and no shared credential to rate-limit (principle 8:
//! anyone who downloads the app gets the same pulls). Free for non-commercial
//! use at ≤10k calls/day; this collector makes ~24. Full variable set
//! globally (incl. UV and true daily extremes via hourlies), and a historical
//! archive back to 1940 if backfill is ever wanted — weather is always
//! re-fetchable, so this source is *not* in the scrobbler urgency class.
//!
//! **Privacy posture:** the only thing that leaves the machine is a lat/lon
//! rounded to 2 decimals (~1 km). Open-Meteo requires no key and sets no
//! cookies. The collector is inert until the user provides a location —
//! granting Location Services or setting a manual location *is* the explicit
//! opt-in; without either it silently skips, TickTick-style.
//!
//! **Location ladder**, tried in order each observation:
//! 1. CoreLocation ([`crate::corelocation`]) when granted (requested once
//!    per pass while undetermined, like the EventKit collectors)
//! 2. the manual location in `.trove/weather-location.json`
//! 3. the last successfully-used location from the sync state (Macs rarely
//!    move; a one-off CoreLocation hiccup shouldn't lose an hour)
//!
//! **Store:** `weather/YYYY-MM.jsonl` keyed by observation month, one line
//! per local hour, stamped at fetch time. Volume is tiny (~720 lines/month),
//! so reads aggregate on the fly like activity. Full fidelity at write time:
//! every field the API returns for the current block is kept.

use std::collections::BTreeMap;
use std::fs;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::corelocation;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef};
use crate::vault::Vault;

const SYNC_FILE: &str = ".trove/weather-sync.json";

// Internally gated to one observation per clock hour, and a silent no-op
// until a location source exists (the grant or manual setting is the opt-in
// for the network call).
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_weather()?;
    Ok(crate::registry::CollectOutcome::note_if(s.observed, || {
        "weather observation recorded".to_string()
    }))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        // A manual location works without the grant.
        required: false,
    }
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_weather_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join("weather")))
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. Rides the slow tick;
/// the collector self-gates to one observation per clock hour.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "weather",
        name: "Weather",
        kind: IntegrationKind::CloudSync,
        default_on: true,
        description: "Records the weather at your location every hour — temperature, precipitation, wind, UV — from the Open-Meteo public API. No account or key; only coordinates rounded to ~1 km ever leave this Mac.",
        domain: "environment",
        vault_path: "weather/",
        toggleable: true,
        setup: &[
            "Approve the Location Services prompt when the app (and separately the troved daemon) asks — or skip it and set a location manually on the Weather tab.",
            "If the prompt was declined: System Settings → Privacy & Security → Location Services → enable Trove and troved, or just set a manual location.",
        ],
        caveats: "Inert until a location exists (granting Location Services or setting one manually is the opt-in for this network call). Conditions are recorded only while a collector runs — gaps are honest gaps, though Open-Meteo's historical archive could backfill them someday.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every(crate::browser::BROWSER_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};
const LOCATION_FILE: &str = ".trove/weather-location.json";

/// One observation per local hour: the slow tick polls more often, this
/// gate keeps the API traffic at ~24 calls/day.
pub const WEATHER_OBSERVE_SECS: u64 = 3600;

/// One recorded observation. Everything the API's `current` block returns,
/// plus the (rounded) coordinates it was asked about. Only `ts` is required
/// to deserialize so old lines keep parsing as fields evolve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WeatherObservation {
    /// RFC3339 local time, stamped at fetch.
    pub ts: String,
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    /// Human label for the location when one is known (manual config).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    #[serde(default)]
    pub temp_c: f64,
    #[serde(default)]
    pub apparent_c: f64,
    #[serde(default)]
    pub humidity_pct: f64,
    #[serde(default)]
    pub dew_point_c: f64,
    #[serde(default)]
    pub precip_mm: f64,
    #[serde(default)]
    pub rain_mm: f64,
    #[serde(default)]
    pub showers_mm: f64,
    #[serde(default)]
    pub snowfall_cm: f64,
    /// WMO weather interpretation code (0 clear … 99 thunderstorm w/ hail).
    #[serde(default)]
    pub weather_code: i64,
    #[serde(default)]
    pub cloud_pct: f64,
    /// Mean-sea-level pressure.
    #[serde(default)]
    pub pressure_hpa: f64,
    #[serde(default)]
    pub wind_kmh: f64,
    #[serde(default)]
    pub wind_dir_deg: f64,
    #[serde(default)]
    pub wind_gusts_kmh: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uv_index: Option<f64>,
    #[serde(default)]
    pub is_day: bool,
    #[serde(default)]
    pub source: String,
}

/// A user-set location, the no-permission path. `.trove/weather-location.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WeatherLocation {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub place: String,
}

/// Collector state: written every pass that does something, surfaced on the
/// Weather tab. Rebuildable — losing it costs at most one duplicate-hour gap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WeatherSyncState {
    /// RFC3339 local time of the last pass that got as far as a decision.
    #[serde(default)]
    pub updated: String,
    /// `ts` of the newest appended observation.
    #[serde(default)]
    pub last_observation: String,
    /// The location last used successfully (ladder rung 3).
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    #[serde(default)]
    pub place: String,
    /// Non-empty while the collector is stuck (no location, network error) —
    /// the UI banner reads this.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// What one pass did, for the runner's log line.
#[derive(Debug, Default)]
pub struct WeatherSyncStats {
    pub observed: bool,
    /// Why nothing was recorded (`"fresh"`, `"no-location"`); empty when
    /// `observed`.
    pub skipped: &'static str,
}

/// Per-day aggregate computed at read time from the hourly observations.
/// "Opinions at read time": the JSONL keeps every field, this is just what
/// the chart wants.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WeatherDay {
    /// YYYY-MM-DD.
    pub date: String,
    pub temp_min: f64,
    pub temp_max: f64,
    pub temp_avg: f64,
    pub precip_mm: f64,
    pub wind_max_kmh: f64,
    pub uv_max: Option<f64>,
    /// How many hourly observations the day has (24 = full coverage).
    pub observations: usize,
}

/// Should a new observation be taken? One per local clock hour: true when
/// there is no previous observation or the previous one is from an earlier
/// hour (of any day).
pub fn should_observe(last: Option<DateTime<Local>>, now: DateTime<Local>) -> bool {
    match last {
        None => true,
        Some(last) => {
            let hour_key = |t: DateTime<Local>| {
                (t.year(), t.month(), t.day(), t.hour())
            };
            hour_key(last) != hour_key(now)
        }
    }
}

/// ~1 km grid: what actually goes on the wire and into the files.
pub fn round_coord(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// The exact request this collector makes (testable, and honest in docs:
/// this URL is the complete description of what leaves the machine).
pub fn current_conditions_url(lat: f64, lon: f64) -> String {
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat:.2}&longitude={lon:.2}\
         &current=temperature_2m,relative_humidity_2m,apparent_temperature,dew_point_2m,\
         is_day,precipitation,rain,showers,snowfall,weather_code,cloud_cover,pressure_msl,\
         wind_speed_10m,wind_direction_10m,wind_gusts_10m,uv_index&timezone=auto"
    )
}

/// Map an Open-Meteo response body onto an observation. Pure — unit-tested
/// against a captured live response. Temperature is the one field whose
/// absence fails the parse (an empty `current` block means the response is
/// not what we asked for); everything else degrades to its default.
pub fn parse_current(
    body: &Value,
    lat: f64,
    lon: f64,
    place: &str,
    ts: &str,
) -> Result<WeatherObservation> {
    let cur = body
        .get("current")
        .ok_or_else(|| anyhow!("response has no `current` block"))?;
    let f = |key: &str| cur.get(key).and_then(Value::as_f64);
    Ok(WeatherObservation {
        ts: ts.to_string(),
        lat,
        lon,
        place: place.to_string(),
        temp_c: f("temperature_2m").ok_or_else(|| anyhow!("no temperature_2m in response"))?,
        apparent_c: f("apparent_temperature").unwrap_or_default(),
        humidity_pct: f("relative_humidity_2m").unwrap_or_default(),
        dew_point_c: f("dew_point_2m").unwrap_or_default(),
        precip_mm: f("precipitation").unwrap_or_default(),
        rain_mm: f("rain").unwrap_or_default(),
        showers_mm: f("showers").unwrap_or_default(),
        snowfall_cm: f("snowfall").unwrap_or_default(),
        weather_code: cur.get("weather_code").and_then(Value::as_i64).unwrap_or_default(),
        cloud_pct: f("cloud_cover").unwrap_or_default(),
        pressure_hpa: f("pressure_msl").unwrap_or_default(),
        wind_kmh: f("wind_speed_10m").unwrap_or_default(),
        wind_dir_deg: f("wind_direction_10m").unwrap_or_default(),
        wind_gusts_kmh: f("wind_gusts_10m").unwrap_or_default(),
        uv_index: f("uv_index"),
        is_day: f("is_day").unwrap_or_default() != 0.0,
        source: "open-meteo".into(),
    })
}

fn fetch_current(lat: f64, lon: f64) -> Result<Value> {
    ureq::get(&current_conditions_url(lat, lon))
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .context("requesting Open-Meteo current conditions")?
        .into_json()
        .context("reading Open-Meteo response")
}

impl Vault {
    /// One collector pass: gate to one observation per hour, resolve a
    /// location down the ladder, fetch, append, update state. "Stuck"
    /// outcomes (no location yet) are Ok-with-skip, not errors — the state
    /// file carries the reason for the UI; only transport/API failures are
    /// `Err` (the runner logs those and retries next tick).
    pub fn collect_weather(&self) -> Result<WeatherSyncStats> {
        let now = Local::now();
        let mut state = self.read_weather_sync().unwrap_or_default();
        let last = DateTime::parse_from_rfc3339(&state.last_observation)
            .ok()
            .map(|t| t.with_timezone(&Local));
        if !should_observe(last, now) {
            return Ok(WeatherSyncStats { observed: false, skipped: "fresh" });
        }
        let Some((lat, lon, place)) = self.resolve_weather_location(&state) else {
            state.updated = now.to_rfc3339();
            state.error =
                "no location: grant Location Services or set a location on the Weather tab"
                    .into();
            self.write_weather_sync(&state)?;
            return Ok(WeatherSyncStats { observed: false, skipped: "no-location" });
        };
        let body = match fetch_current(lat, lon) {
            Ok(b) => b,
            Err(e) => {
                state.updated = now.to_rfc3339();
                state.error = format!("{e:#}");
                self.write_weather_sync(&state)?;
                return Err(e);
            }
        };
        let obs = parse_current(&body, lat, lon, &place, &now.to_rfc3339())?;
        self.append_weather_observations(&[obs.clone()])?;
        self.write_weather_sync(&WeatherSyncState {
            updated: now.to_rfc3339(),
            last_observation: obs.ts,
            lat,
            lon,
            place,
            error: String::new(),
        })?;
        Ok(WeatherSyncStats { observed: true, skipped: "" })
    }

    /// The location ladder. CoreLocation fixes get the manual place label
    /// when they round to the same spot, else no label.
    fn resolve_weather_location(&self, state: &WeatherSyncState) -> Option<(f64, f64, String)> {
        let manual = self.weather_location();
        if corelocation::auth_status() == AuthStatus::NotDetermined {
            // Fires the TCC prompt when the host carries the usage string;
            // instant when already decided, skipped-this-pass when a live
            // prompt outlives the wait (same shape as the EventKit pulls).
            corelocation::request_access(5);
        }
        if let Some(fix) = corelocation::current_location(8) {
            let (lat, lon) = (round_coord(fix.lat), round_coord(fix.lon));
            let place = match &manual {
                Some(m) if round_coord(m.lat) == lat && round_coord(m.lon) == lon => {
                    m.place.clone()
                }
                _ if state.lat == lat && state.lon == lon => state.place.clone(),
                _ => String::new(),
            };
            return Some((lat, lon, place));
        }
        if let Some(m) = manual {
            return Some((round_coord(m.lat), round_coord(m.lon), m.place));
        }
        // Last known: zero-zero is the Atlantic off Ghana, safe as "unset".
        (state.lat != 0.0 || state.lon != 0.0)
            .then(|| (state.lat, state.lon, state.place.clone()))
    }

    /// Append observations to their monthly files (keyed by `ts` month).
    /// Only the watcher owner writes weather, so no extra locking — same
    /// single-writer contract as activity.
    pub fn append_weather_observations(&self, observations: &[WeatherObservation]) -> Result<()> {
        self.stream("weather", crate::store::Partition::Month).append(observations, |o| &o.ts)
    }

    fn read_weather_month(&self, month: &str) -> Vec<WeatherObservation> {
        let Ok(path) = self.resolve(&format!("weather/{month}.jsonl")) else {
            return Vec::new();
        };
        let Ok(body) = fs::read_to_string(path) else {
            return Vec::new();
        };
        body.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// A day's observations, ts-sorted. `date` is YYYY-MM-DD.
    pub fn weather_timeline(&self, date: &str) -> Result<Vec<WeatherObservation>> {
        let month = date.get(..7).unwrap_or(date);
        let mut day: Vec<WeatherObservation> = self
            .read_weather_month(month)
            .into_iter()
            .filter(|o| o.ts.starts_with(date))
            .collect();
        day.sort_by(|a, b| a.ts.cmp(&b.ts));
        Ok(day)
    }

    /// The newest observation on file (this month, falling back one month so
    /// the card survives a month rollover during a collection gap).
    pub fn weather_latest(&self) -> Result<Option<WeatherObservation>> {
        let now = Local::now();
        let this_month = now.format("%Y-%m").to_string();
        let prev_month = (now.date_naive() - chrono::Days::new(31)).format("%Y-%m").to_string();
        for month in [this_month, prev_month] {
            if let Some(latest) = self
                .read_weather_month(&month)
                .into_iter()
                .max_by(|a, b| a.ts.cmp(&b.ts))
            {
                return Ok(Some(latest));
            }
        }
        Ok(None)
    }

    /// Per-day aggregates for `[from, to]` (YYYY-MM-DD, inclusive). Days with
    /// no observations are omitted — an honest gap, not a zero.
    pub fn weather_daily(&self, from: &str, to: &str) -> Result<Vec<WeatherDay>> {
        let (from_d, to_d) = (
            NaiveDate::parse_from_str(from, "%Y-%m-%d").context("bad `from` date")?,
            NaiveDate::parse_from_str(to, "%Y-%m-%d").context("bad `to` date")?,
        );
        let mut by_day: BTreeMap<String, Vec<WeatherObservation>> = BTreeMap::new();
        let mut month = from_d.with_day(1).unwrap();
        while month <= to_d {
            for o in self.read_weather_month(&month.format("%Y-%m").to_string()) {
                let day = o.ts.get(..10).unwrap_or_default().to_string();
                if day.as_str() >= from && day.as_str() <= to {
                    by_day.entry(day).or_default().push(o);
                }
            }
            month = (month + chrono::Days::new(32)).with_day(1).unwrap();
        }
        Ok(by_day
            .into_iter()
            .map(|(date, obs)| {
                let temps: Vec<f64> = obs.iter().map(|o| o.temp_c).collect();
                WeatherDay {
                    date,
                    temp_min: temps.iter().cloned().fold(f64::INFINITY, f64::min),
                    temp_max: temps.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                    temp_avg: temps.iter().sum::<f64>() / temps.len() as f64,
                    precip_mm: obs.iter().map(|o| o.precip_mm).sum(),
                    wind_max_kmh: obs
                        .iter()
                        .map(|o| o.wind_kmh)
                        .fold(f64::NEG_INFINITY, f64::max),
                    uv_max: obs
                        .iter()
                        .filter_map(|o| o.uv_index)
                        .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v)))),
                    observations: obs.len(),
                }
            })
            .collect())
    }

    /// The manual location, if the user set one.
    pub fn weather_location(&self) -> Option<WeatherLocation> {
        let path = self.resolve(LOCATION_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// Set (or with `None`, clear) the manual location.
    pub fn set_weather_location(&self, location: Option<WeatherLocation>) -> Result<()> {
        let path = self.resolve(LOCATION_FILE)?;
        match location {
            Some(loc) => {
                crate::store::write_json_atomic(&path, &loc)
                    .context("writing weather location")?;
            }
            None => {
                if path.exists() {
                    fs::remove_file(&path)?;
                }
            }
        }
        Ok(())
    }

    pub fn read_weather_sync(&self) -> Option<WeatherSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_weather_sync(&self, state: &WeatherSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-weather-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn obs(ts: &str, temp: f64) -> WeatherObservation {
        WeatherObservation {
            ts: ts.into(),
            lat: 34.05,
            lon: -118.24,
            place: String::new(),
            temp_c: temp,
            apparent_c: temp + 2.0,
            humidity_pct: 50.0,
            dew_point_c: 10.0,
            precip_mm: 0.5,
            rain_mm: 0.5,
            showers_mm: 0.0,
            snowfall_cm: 0.0,
            weather_code: 3,
            cloud_pct: 40.0,
            pressure_hpa: 1010.0,
            wind_kmh: 12.0,
            wind_dir_deg: 200.0,
            wind_gusts_kmh: 20.0,
            uv_index: Some(5.0),
            is_day: true,
            source: "open-meteo".into(),
        }
    }

    /// Captured from a live call on 2026-06-11 (LA, rounded coords).
    const FIXTURE: &str = r#"{
        "latitude": 34.060257, "longitude": -118.23433,
        "utc_offset_seconds": -25200, "timezone": "America/Los_Angeles",
        "current_units": {"time": "iso8601", "temperature_2m": "°C"},
        "current": {
            "time": "2026-06-11T12:15", "interval": 900,
            "temperature_2m": 30.1, "relative_humidity_2m": 46,
            "apparent_temperature": 33.9, "dew_point_2m": 17.2, "is_day": 1,
            "precipitation": 0.00, "rain": 0.00, "showers": 0.00,
            "snowfall": 0.00, "weather_code": 0, "cloud_cover": 1,
            "pressure_msl": 1009.0, "wind_speed_10m": 10.0,
            "wind_direction_10m": 210, "wind_gusts_10m": 11.2, "uv_index": 8.40
        }
    }"#;

    #[test]
    fn parses_live_fixture() {
        let body: Value = serde_json::from_str(FIXTURE).unwrap();
        let o =
            parse_current(&body, 34.05, -118.24, "Los Angeles", "2026-06-11T12:20:00-07:00")
                .unwrap();
        assert_eq!(o.temp_c, 30.1);
        assert_eq!(o.apparent_c, 33.9);
        assert_eq!(o.humidity_pct, 46.0);
        assert_eq!(o.dew_point_c, 17.2);
        assert_eq!(o.weather_code, 0);
        assert_eq!(o.pressure_hpa, 1009.0);
        assert_eq!(o.wind_kmh, 10.0);
        assert_eq!(o.wind_gusts_kmh, 11.2);
        assert_eq!(o.uv_index, Some(8.4));
        assert!(o.is_day);
        assert_eq!(o.place, "Los Angeles");
        assert_eq!(o.source, "open-meteo");
    }

    #[test]
    fn parse_requires_current_block_and_temperature() {
        let no_current: Value = serde_json::from_str(r#"{"error": true}"#).unwrap();
        assert!(parse_current(&no_current, 0.0, 0.0, "", "t").is_err());
        let no_temp: Value = serde_json::from_str(r#"{"current": {"is_day": 1}}"#).unwrap();
        assert!(parse_current(&no_temp, 0.0, 0.0, "", "t").is_err());
    }

    #[test]
    fn observe_gate_is_one_per_clock_hour() {
        assert!(should_observe(None, at(2026, 6, 11, 9, 30)));
        // Same hour: skip, even at the other end of it.
        assert!(!should_observe(
            Some(at(2026, 6, 11, 9, 1)),
            at(2026, 6, 11, 9, 59)
        ));
        // Next hour: due.
        assert!(should_observe(
            Some(at(2026, 6, 11, 9, 59)),
            at(2026, 6, 11, 10, 0)
        ));
        // Same clock hour on a different day: due.
        assert!(should_observe(
            Some(at(2026, 6, 10, 9, 30)),
            at(2026, 6, 11, 9, 30)
        ));
    }

    /// Byte-parity contract for the three write paths (month-keyed
    /// observation append, manual location, sync state): exact bytes, pinned
    /// before the port onto `store` and unchanged by it.
    #[test]
    fn writes_are_byte_identical() {
        let v = temp_vault("parity");
        let o = obs("2026-06-10T09:05:00-07:00", 18.5);
        v.append_weather_observations(&[o.clone()]).unwrap();
        v.append_weather_observations(&[o]).unwrap(); // second call extends
        let line = "{\"ts\":\"2026-06-10T09:05:00-07:00\",\"lat\":34.05,\"lon\":-118.24,\"temp_c\":18.5,\"apparent_c\":20.5,\"humidity_pct\":50.0,\"dew_point_c\":10.0,\"precip_mm\":0.5,\"rain_mm\":0.5,\"showers_mm\":0.0,\"snowfall_cm\":0.0,\"weather_code\":3,\"cloud_pct\":40.0,\"pressure_hpa\":1010.0,\"wind_kmh\":12.0,\"wind_dir_deg\":200.0,\"wind_gusts_kmh\":20.0,\"uv_index\":5.0,\"is_day\":true,\"source\":\"open-meteo\"}\n";
        assert_eq!(
            fs::read_to_string(v.root().join("weather/2026-06.jsonl")).unwrap(),
            format!("{line}{line}"),
            "month-keyed append extends across calls"
        );

        v.set_weather_location(Some(WeatherLocation {
            lat: 34.05,
            lon: -118.24,
            place: "Los Angeles".into(),
        }))
        .unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join(".trove/weather-location.json")).unwrap(),
            "{\n  \"lat\": 34.05,\n  \"lon\": -118.24,\n  \"place\": \"Los Angeles\"\n}"
        );

        v.write_weather_sync(&WeatherSyncState {
            updated: "2026-06-10T09:05:00-07:00".into(),
            last_observation: "2026-06-10T09:05:00-07:00".into(),
            lat: 34.05,
            lon: -118.24,
            place: "Los Angeles".into(),
            error: String::new(),
        })
        .unwrap();
        assert_eq!(
            fs::read_to_string(v.root().join(".trove/weather-sync.json")).unwrap(),
            "{\n  \"updated\": \"2026-06-10T09:05:00-07:00\",\n  \"last_observation\": \"2026-06-10T09:05:00-07:00\",\n  \"lat\": 34.05,\n  \"lon\": -118.24,\n  \"place\": \"Los Angeles\"\n}"
        );
    }

    #[test]
    fn append_and_read_back_across_months() {
        let v = temp_vault("roundtrip");
        v.append_weather_observations(&[
            obs("2026-05-31T23:05:00-07:00", 18.0),
            obs("2026-06-01T00:05:00-07:00", 17.5),
            obs("2026-06-01T01:05:00-07:00", 17.0),
        ])
        .unwrap();
        assert!(v.root().join("weather/2026-05.jsonl").exists());
        assert!(v.root().join("weather/2026-06.jsonl").exists());

        let day = v.weather_timeline("2026-06-01").unwrap();
        assert_eq!(day.len(), 2);
        assert_eq!(day[0].temp_c, 17.5);

        let days = v.weather_daily("2026-05-31", "2026-06-01").unwrap();
        assert_eq!(days.len(), 2);
        assert_eq!(days[0].date, "2026-05-31");
        assert_eq!(days[1].observations, 2);
        assert_eq!(days[1].temp_min, 17.0);
        assert_eq!(days[1].temp_max, 17.5);
        assert_eq!(days[1].uv_max, Some(5.0));
        assert!((days[1].precip_mm - 1.0).abs() < 1e-9);
    }

    #[test]
    fn daily_omits_uncovered_days() {
        let v = temp_vault("gaps");
        v.append_weather_observations(&[obs("2026-06-03T12:00:00-07:00", 20.0)]).unwrap();
        let days = v.weather_daily("2026-06-01", "2026-06-07").unwrap();
        assert_eq!(days.len(), 1);
        assert_eq!(days[0].date, "2026-06-03");
    }

    #[test]
    fn latest_finds_newest_and_handles_empty() {
        let v = temp_vault("latest");
        assert!(v.weather_latest().unwrap().is_none());
        let this_month = Local::now().format("%Y-%m").to_string();
        v.append_weather_observations(&[
            obs(&format!("{this_month}-01T08:00:00-07:00"), 15.0),
            obs(&format!("{this_month}-01T09:00:00-07:00"), 16.0),
        ])
        .unwrap();
        assert_eq!(v.weather_latest().unwrap().unwrap().temp_c, 16.0);
    }

    #[test]
    fn old_minimal_lines_still_parse() {
        // Forward compatibility: a line with only `ts` (or with fields this
        // version doesn't know) must keep deserializing.
        let v = temp_vault("compat");
        fs::create_dir_all(v.root().join("weather")).unwrap();
        fs::write(
            v.root().join("weather/2026-06.jsonl"),
            "{\"ts\":\"2026-06-02T10:00:00-07:00\",\"future_field\":1}\n",
        )
        .unwrap();
        let day = v.weather_timeline("2026-06-02").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].temp_c, 0.0);
        assert_eq!(day[0].uv_index, None);
    }

    #[test]
    fn location_config_round_trip() {
        let v = temp_vault("location");
        assert!(v.weather_location().is_none());
        v.set_weather_location(Some(WeatherLocation {
            lat: 34.0522,
            lon: -118.2437,
            place: "Los Angeles".into(),
        }))
        .unwrap();
        let loc = v.weather_location().unwrap();
        assert_eq!(loc.place, "Los Angeles");
        v.set_weather_location(None).unwrap();
        assert!(v.weather_location().is_none());
    }

    #[test]
    fn sync_state_round_trip() {
        let v = temp_vault("sync");
        assert!(v.read_weather_sync().is_none());
        v.write_weather_sync(&WeatherSyncState {
            updated: "2026-06-11T10:00:00-07:00".into(),
            last_observation: "2026-06-11T10:00:00-07:00".into(),
            lat: 34.05,
            lon: -118.24,
            place: "LA".into(),
            error: String::new(),
        })
        .unwrap();
        let s = v.read_weather_sync().unwrap();
        assert_eq!(s.lat, 34.05);
        assert!(s.error.is_empty());
    }

    #[test]
    fn coords_round_to_two_decimals() {
        assert_eq!(round_coord(34.0522), 34.05);
        assert_eq!(round_coord(-118.2437), -118.24);
        let url = current_conditions_url(34.0522, -118.2437);
        assert!(url.contains("latitude=34.05&"), "{url}");
        assert!(url.contains("longitude=-118.24&"), "{url}");
    }
}
