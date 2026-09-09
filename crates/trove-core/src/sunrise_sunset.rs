//! Sunrise-Sunset.org / SunriseSunset.io — keyless fallback for solar times
//! and golden hour. Catalogued in the Phase 2 pass; brief:
//! docs/integrations/sunrise-sunset.md.
//!
//! A **Periodic** (daily) pull over two sibling keyless endpoints:
//!
//! - Primary: `GET https://api.sunrisesunset.io/json?lat=LAT&lng=LNG&date=YYYY-MM-DD`
//!   → all solar/twilight fields **plus** `golden_hour` (only available from .io).
//! - Fallback: `GET https://api.sunrise-sunset.org/json?lat=LAT&lng=LNG&date=YYYY-MM-DD&formatted=0`
//!   → solar + all twilight stages; no `golden_hour`. Returns UTC RFC3339 timestamps.
//!
//! **Location sharing**: sunrise-sunset is the keyless fallback for USNO. It
//! reuses the USNO location (`.trove/usno-sync.json`, field `location.lat` +
//! `location.lon`) so the user configures one lat/lon through USNO's connect
//! card and both collectors share it. No connect card for sunrise-sunset; it runs
//! silently when USNO is configured, or skips otherwise.
//!
//! Two layers written unconditionally for each day:
//! - **Raw**: `environment/sunrise-sunset/raw/YYYY-MM.jsonl` — the full API
//!   response, one row per (date, location).
//! - **Contract**: `environment/sunrise-sunset/almanac/YYYY-MM.jsonl` — an
//!   [`crate::environment::Almanac`] row, deduped by `date` + rounded coords.
//!
//! ### .io response shape (confirmed live 2026-06-17 via curl)
//! The .io endpoint returns times in `"H:MM:SS AM/PM"` format in local time.
//! Key confirmed fields:
//! - `utc_offset`: **integer minutes** (e.g. `-420` for PDT); NOT a `"+HH:MM"` string.
//! - Civil twilight: `dawn` / `dusk` (NOT `civil_twilight_begin/end`).
//! - Nautical twilight: `nautical_twilight_begin` / `nautical_twilight_end`.
//! - Astronomical twilight: `first_light` / `last_light`.
//! - Moon: `moonrise`, `moonset`, `moon_phase`, `moon_illumination` present.
//! - `day_length`: `"H:MM:SS"` string (normalized to `"H:MM"` for contract).
//! We convert times to RFC3339 local-offset using the integer offset.
//!
//! ### .org response shape (confirmed live 2026-06-17)
//! Returns UTC RFC3339 timestamps directly: `"2026-06-10T12:39:37+00:00"`.
//! `day_length` is an integer in seconds. Full civil/nautical/astronomical
//! twilight stages — a superset of USNO (which returns civil only).
//!
//! ## Cursor / backfill
//! Own cursor (`.trove/sunrise-sunset-sync.json`) holds the last `date`
//! watermark. Location is read from USNO's cursor; if unset, pull quietly skips.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::environment::Almanac;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const ALMANAC_DIR: &str = "environment/sunrise-sunset/almanac";
const RAW_DIR: &str = "environment/sunrise-sunset/raw";
/// Own cursor — last date written. Location is read from USNO's cursor.
const SYNC_FILE: &str = ".trove/sunrise-sunset-sync.json";
/// USNO non-secret cursor (plain config) — we read the `location` field from it.
const USNO_SYNC_FILE: &str = ".trove/usno-sync.json";

const IO_BASE: &str = "https://api.sunrisesunset.io";
const ORG_BASE: &str = "https://api.sunrise-sunset.org";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Most days to backfill in one pull.
const MAX_BACKFILL_DAYS: i64 = 35;
/// Sync interval: daily (checked every 6 h, gate fires at most once per local day).
pub const SUNRISE_SUNSET_SYNC_SECS: u64 = 6 * 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ALMANAC_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n: u64 = out.counts.values().sum();
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("sunrise-sunset synced — {n} day(s)")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("sunrise-sunset skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub already has the line).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "sunrise-sunset",
        name: "Sunrise-Sunset.org",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Records daily sunrise, sunset, and golden-hour times for your location \
                      via the keyless sunrise-sunset.org and SunriseSunset.io APIs.",
        domain: "environment",
        vault_path: "environment/sunrise-sunset/",
        toggleable: true,
        setup: &[
            "Configure a location in USNO Astronomy — sunrise-sunset shares that lat/lon.",
            "No separate sign-in or API key required; it is fully keyless.",
        ],
        caveats: "Keyless fallback for USNO Astronomy. USNO is the primary authoritative \
                  source and also provides moon data. Requires USNO location to be configured.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::daily(SUNRISE_SUNSET_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    // Keyless; no connection / login needed. Location is shared from USNO.
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// USNO location reader (reads the USNO non-secret cursor file for lat/lon).

/// The minimal shape we read from the USNO cursor file. We only need lat/lon.
#[derive(Deserialize)]
struct UsnoLocation {
    lat: f64,
    lon: f64,
}

#[derive(Deserialize)]
struct UsnoSyncPeek {
    #[serde(default)]
    location: Option<UsnoLocation>,
}

/// Read the USNO configured location from the USNO non-secret cursor.
/// Returns `None` if the cursor file doesn't exist or no location is set.
fn read_usno_location(vault: &Vault) -> Option<(f64, f64)> {
    let path = vault.resolve(USNO_SYNC_FILE).ok()?;
    let text = std::fs::read_to_string(path).ok()?;
    let peek: UsnoSyncPeek = serde_json::from_str(&text).ok()?;
    let loc = peek.location?;
    Some((loc.lat, loc.lon))
}

// ---------------------------------------------------------------------------
// Own cursor (watermark only; location lives in the USNO cursor).

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// Last calendar day (`YYYY-MM-DD`) successfully written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_date: Option<String>,
    /// RFC3339 local time of last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ss_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ss_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API shapes.

/// The `.org` response `results` object (confirmed live 2026-06-17).
/// All time fields are UTC RFC3339 strings; `day_length` is integer seconds.
#[derive(Debug, Deserialize)]
struct OrgResults {
    sunrise: Option<String>,
    sunset: Option<String>,
    solar_noon: Option<String>,
    day_length: Option<u64>,
    civil_twilight_begin: Option<String>,
    civil_twilight_end: Option<String>,
    nautical_twilight_begin: Option<String>,
    nautical_twilight_end: Option<String>,
    astronomical_twilight_begin: Option<String>,
    astronomical_twilight_end: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrgResponse {
    results: OrgResults,
    status: String,
}

/// The `.io` response `results` object.
/// Times are `"HH:MM:SS AM"` local strings; `golden_hour` is one of them.
///
/// **Confirmed live (2026-06-17) field layout:**
/// - `utc_offset`: integer minutes (e.g. `-420` for PDT), NOT a `"+HH:MM"` string.
/// - Civil twilight: `dawn` / `dusk` (NOT `civil_twilight_begin/end`).
/// - Nautical twilight: `nautical_twilight_begin` / `nautical_twilight_end` (real fields).
/// - Astronomical twilight: `first_light` / `last_light` (NOT `astronomical_twilight_begin/end`).
/// - Moon: `moonrise`, `moonset`, `moon_phase`, `moon_illumination` — all present.
/// - Extra: `sun_altitude`, `sun_azimuth`, `sunrise_azimuth`, `sunset_azimuth`, `elevation`.
/// The raw `Value` is kept verbatim for the raw layer.
#[derive(Debug, Deserialize)]
struct IoResults {
    sunrise: Option<String>,
    sunset: Option<String>,
    solar_noon: Option<String>,
    day_length: Option<String>,
    // Civil twilight uses `dawn`/`dusk` in the real .io API.
    dawn: Option<String>,
    dusk: Option<String>,
    // Nautical twilight: actual field names match contract.
    nautical_twilight_begin: Option<String>,
    nautical_twilight_end: Option<String>,
    // Astronomical twilight: `first_light`/`last_light` in the real .io API.
    first_light: Option<String>,
    last_light: Option<String>,
    golden_hour: Option<String>,
    /// UTC offset as **integer minutes** (e.g. `-420` for UTC-7). Confirmed live.
    utc_offset: Option<i64>,
    timezone: Option<String>,
    // Moon data — all present in confirmed live .io response.
    moonrise: Option<String>,
    moonset: Option<String>,
    moon_phase: Option<String>,
    moon_illumination: Option<f64>,
    // Solar geometry extras.
    sun_altitude: Option<f64>,
    sun_azimuth: Option<f64>,
    sunrise_azimuth: Option<f64>,
    sunset_azimuth: Option<f64>,
    elevation: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct IoResponse {
    results: IoResults,
    status: String,
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable seam for tests).

trait SunriseSunsetApi {
    /// Fetch from `.io` endpoint. Returns the full response `Value`.
    fn fetch_io(&self, date: &str, lat: f64, lon: f64) -> Result<Value, String>;
    /// Fetch from `.org` endpoint. Returns the full response `Value`.
    fn fetch_org(&self, date: &str, lat: f64, lon: f64) -> Result<Value, String>;
}

struct LiveClient {
    io_base: String,
    org_base: String,
}

impl LiveClient {
    fn new() -> Self {
        LiveClient {
            io_base: IO_BASE.to_string(),
            org_base: ORG_BASE.to_string(),
        }
    }
}

fn http_fetch(url: &str) -> Result<Value, String> {
    match ureq::get(url).timeout(HTTP_TIMEOUT).call() {
        Ok(resp) => resp
            .into_json::<Value>()
            .map_err(|e| format!("JSON parse error: {e}")),
        Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {code}")),
        Err(e) => Err(e.to_string()),
    }
}

impl SunriseSunsetApi for LiveClient {
    fn fetch_io(&self, date: &str, lat: f64, lon: f64) -> Result<Value, String> {
        let url = format!("{}/json?lat={lat}&lng={lon}&date={date}", self.io_base);
        http_fetch(&url)
    }

    fn fetch_org(&self, date: &str, lat: f64, lon: f64) -> Result<Value, String> {
        let url = format!(
            "{}/json?lat={lat}&lng={lon}&date={date}&formatted=0",
            self.org_base
        );
        http_fetch(&url)
    }
}

// ---------------------------------------------------------------------------
// Helpers: convert .io 12-hour time strings to RFC3339 UTC-offset.

/// Format a signed-minute UTC offset as `"+HH:MM"` or `"-HH:MM"`.
fn offset_suffix_from_mins(total_min: i64) -> String {
    let sign = if total_min < 0 { '-' } else { '+' };
    let abs = total_min.unsigned_abs() as u64;
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// Convert a `.io` 12-hour time string (`"6:02:11 AM"`) + date + UTC offset
/// minutes → RFC3339 local timestamp. Returns `None` on any parse failure.
fn io_to_rfc3339(date: &str, time_str: &str, off_mins: i64) -> Option<String> {
    let time_str = time_str.trim();
    if time_str.is_empty() {
        return None;
    }
    // Parse "H:MM:SS AM" or "HH:MM:SS AM"
    let (time_part, ampm) = if let Some(a) = time_str.strip_suffix("AM").or(time_str.strip_suffix("am")) {
        (a.trim(), false)
    } else if let Some(p) = time_str.strip_suffix("PM").or(time_str.strip_suffix("pm")) {
        (p.trim(), true)
    } else {
        return None;
    };

    let mut parts = time_part.split(':');
    let h_str = parts.next()?;
    let m_str = parts.next()?;
    let s_str = parts.next().unwrap_or("00");
    let h: u8 = h_str.trim().parse().ok()?;
    let m: u8 = m_str.trim().parse().ok()?;
    let s: u8 = s_str.trim().parse().ok()?;

    // 12-hour → 24-hour conversion
    let h24 = match (h, ampm) {
        (12, false) => 0, // 12:xx AM → 00:xx
        (12, true) => 12, // 12:xx PM → 12:xx
        (h, false) => h,
        (h, true) => h + 12,
    };
    if h24 > 23 || m > 59 || s > 59 {
        return None;
    }

    let off_str = offset_suffix_from_mins(off_mins);
    Some(format!("{date}T{h24:02}:{m:02}:{s:02}{off_str}"))
}

/// Convert `.org` day_length (seconds integer) to `"HH:MM"` string.
fn seconds_to_hhmm(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    format!("{h}:{m:02}")
}

// ---------------------------------------------------------------------------
// Mapping: build an Almanac row + raw Value from each API response.

/// Convert a UTC RFC3339 timestamp from `.org` (e.g. `"2026-06-10T12:39:37+00:00"`)
/// to a local-offset RFC3339 timestamp (e.g. `"2026-06-10T05:39:37-07:00"`) using
/// the supplied UTC offset in minutes. Returns the original string unchanged when
/// `off_mins` is `None` or parsing fails (UTC is a valid RFC3339 instant; local
/// representation is preferred per contract but falls back to UTC gracefully).
fn utc_to_local_rfc3339(utc_str: &str, off_mins: Option<i64>) -> String {
    let off = match off_mins {
        Some(m) if m != 0 => m,
        // offset is 0 or unknown: the UTC string is already correct.
        _ => return utc_str.to_string(),
    };
    // Parse the UTC RFC3339 string to a fixed-offset DateTime.
    // chrono can parse "+00:00" suffix directly.
    use chrono::{DateTime, FixedOffset};
    let dt = match DateTime::parse_from_rfc3339(utc_str) {
        Ok(d) => d,
        Err(_) => return utc_str.to_string(),
    };
    // Build the target fixed offset.
    let total_secs = (off * 60) as i32;
    let local_offset = match FixedOffset::east_opt(total_secs) {
        Some(o) => o,
        None => return utc_str.to_string(),
    };
    let local_dt = dt.with_timezone(&local_offset);
    local_dt.to_rfc3339()
}

/// Map from a confirmed `.org` response (UTC RFC3339 times) into an Almanac.
/// Returns `(almanac, raw_value)`.
///
/// The `.org` API returns UTC RFC3339 timestamps (`+00:00`). The contract
/// specifies RFC3339 *local* time (carrying the location's UTC offset). When
/// `off_mins` is supplied (from a successful `.io` response or the USNO offset),
/// timestamps are converted to local-offset RFC3339. When `off_mins` is `None`,
/// UTC times are stored verbatim — valid RFC3339 instants, but not local-offset.
///
/// Twilight stages: civil, nautical, astronomical — a superset of USNO (civil only).
fn almanac_from_org(
    resp_value: &Value,
    date: &str,
    lat: f64,
    lon: f64,
    off_mins: Option<i64>,
) -> Option<(Almanac, Value)> {
    let parsed: OrgResponse = serde_json::from_value(resp_value.clone()).ok()?;
    if parsed.status != "OK" {
        return None;
    }
    let r = parsed.results;

    let conv = |opt: Option<String>| {
        opt.map(|s| utc_to_local_rfc3339(&s, off_mins))
            .unwrap_or_default()
    };
    let day_length_str = r.day_length.map(seconds_to_hhmm).unwrap_or_default();

    let mut extra = Map::new();
    if let Some(secs) = r.day_length {
        extra.insert("day_length_seconds".into(), Value::Number(serde_json::Number::from(secs)));
    }
    if let Some(m) = off_mins {
        extra.insert("utc_offset".into(), Value::String(offset_suffix_from_mins(m)));
    }

    let almanac = Almanac {
        date: date.to_string(),
        source: "sunrise-sunset".into(),
        lat: Some(lat),
        lon: Some(lon),
        sunrise: conv(r.sunrise),
        sunset: conv(r.sunset),
        solar_noon: conv(r.solar_noon),
        day_length: day_length_str,
        civil_twilight_begin: conv(r.civil_twilight_begin),
        civil_twilight_end: conv(r.civil_twilight_end),
        nautical_twilight_begin: conv(r.nautical_twilight_begin),
        nautical_twilight_end: conv(r.nautical_twilight_end),
        astronomical_twilight_begin: conv(r.astronomical_twilight_begin),
        astronomical_twilight_end: conv(r.astronomical_twilight_end),
        golden_hour: String::new(),
        moonrise: String::new(),
        moonset: String::new(),
        moon_phase: String::new(),
        extra,
    };

    Some((almanac, resp_value.clone()))
}

/// Map from a `.io` response into an Almanac.
///
/// Confirmed live (2026-06-17) field mapping:
/// - `utc_offset`: integer minutes (e.g. `-420`). Used directly; no string parse needed.
/// - Civil twilight: `dawn`/`dusk` → `civil_twilight_begin`/`civil_twilight_end`.
/// - Nautical twilight: `nautical_twilight_begin`/`nautical_twilight_end` (match contract).
/// - Astronomical twilight: `first_light`/`last_light` → `astronomical_twilight_begin`/`end`.
/// - `golden_hour` is a native field on this endpoint.
/// - Moon: `moonrise`/`moonset`/`moon_phase` → contract fields; extras → `extra`.
/// - `day_length` (`"H:MM:SS"`) is truncated to `"H:MM"` for consistency with .org.
fn almanac_from_io(resp_value: &Value, date: &str, lat: f64, lon: f64) -> Option<(Almanac, Value)> {
    let parsed: IoResponse = serde_json::from_value(resp_value.clone()).ok()?;
    if parsed.status != "OK" {
        return None;
    }
    let r = parsed.results;

    // utc_offset is an integer number of minutes (e.g. -420 for UTC-7). Use directly.
    let off_mins = r.utc_offset.unwrap_or(0);

    let ts = |opt: Option<String>| {
        opt.as_deref()
            .and_then(|s| io_to_rfc3339(date, s, off_mins))
            .unwrap_or_default()
    };

    // Normalize day_length from "H:MM:SS" to "H:MM" (contract example: "14:21").
    let day_length = r
        .day_length
        .as_deref()
        .map(|s| {
            // Truncate the trailing ":SS" if present (e.g. "14:25:48" → "14:25").
            let parts: Vec<&str> = s.splitn(3, ':').collect();
            if parts.len() >= 2 {
                format!("{}:{}", parts[0], parts[1])
            } else {
                s.to_string()
            }
        })
        .unwrap_or_default();

    let mut extra = Map::new();
    if let Some(tz) = &r.timezone {
        if !tz.is_empty() {
            extra.insert("timezone".into(), Value::String(tz.clone()));
        }
    }
    // Store the raw offset and raw day_length in extra for reference.
    if let Some(off) = r.utc_offset {
        extra.insert("utc_offset_minutes".into(), Value::Number(off.into()));
        extra.insert(
            "utc_offset".into(),
            Value::String(offset_suffix_from_mins(off)),
        );
    }
    if let Some(dl) = &r.day_length {
        if dl.len() > 5 {
            // Only store raw when it differs from the normalized form (has seconds).
            extra.insert("day_length_raw".into(), Value::String(dl.clone()));
        }
    }
    // Solar geometry extras.
    if let Some(v) = r.sun_altitude {
        extra.insert("sun_altitude".into(), json!(v));
    }
    if let Some(v) = r.sun_azimuth {
        extra.insert("sun_azimuth".into(), json!(v));
    }
    if let Some(v) = r.sunrise_azimuth {
        extra.insert("sunrise_azimuth".into(), json!(v));
    }
    if let Some(v) = r.sunset_azimuth {
        extra.insert("sunset_azimuth".into(), json!(v));
    }
    if let Some(v) = r.elevation {
        extra.insert("elevation".into(), json!(v));
    }
    if let Some(v) = r.moon_illumination {
        extra.insert("moon_illumination".into(), json!(v));
    }

    let almanac = Almanac {
        date: date.to_string(),
        source: "sunrise-sunset".into(),
        lat: Some(lat),
        lon: Some(lon),
        sunrise: ts(r.sunrise),
        sunset: ts(r.sunset),
        solar_noon: ts(r.solar_noon),
        day_length,
        // Civil twilight: `dawn`/`dusk` in the real .io API.
        civil_twilight_begin: ts(r.dawn),
        civil_twilight_end: ts(r.dusk),
        // Nautical twilight: actual .io field names match contract names.
        nautical_twilight_begin: ts(r.nautical_twilight_begin),
        nautical_twilight_end: ts(r.nautical_twilight_end),
        // Astronomical twilight: `first_light`/`last_light` in the real .io API.
        astronomical_twilight_begin: ts(r.first_light),
        astronomical_twilight_end: ts(r.last_light),
        golden_hour: ts(r.golden_hour),
        // Moon data: all three fields present in confirmed live .io response.
        moonrise: r.moonrise.as_deref()
            .and_then(|s| io_to_rfc3339(date, s, off_mins))
            .unwrap_or_default(),
        moonset: r.moonset.as_deref()
            .and_then(|s| io_to_rfc3339(date, s, off_mins))
            .unwrap_or_default(),
        moon_phase: r.moon_phase.unwrap_or_default(),
        extra,
    };

    Some((almanac, resp_value.clone()))
}

// ---------------------------------------------------------------------------
// Dedupe key.

fn round_coord(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

fn dedupe_key(date: &str, lat: f64, lon: f64) -> String {
    format!("{date}@{:.4},{:.4}", round_coord(lat), round_coord(lon))
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by (date, coords).

fn write_rows(vault: &Vault, rows: Vec<(Almanac, Value)>) -> Result<u64> {
    let contract = vault.stream(ALMANAC_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let date = v.get("date").and_then(Value::as_str).unwrap_or("");
            let lat = v.get("lat").and_then(Value::as_f64);
            let lon = v.get("lon").and_then(Value::as_f64);
            if let (false, Some(lat), Some(lon)) = (date.is_empty(), lat, lon) {
                seen.insert(dedupe_key(date, lat, lon));
            }
        }
    }

    /// Wrapper for raw rows: the raw API value tagged with date for partitioning.
    #[derive(Serialize)]
    struct RawLine {
        /// Used only for partitioning (Partition::Month takes first 7 chars).
        #[serde(skip)]
        date: String,
        #[serde(flatten)]
        value: Value,
    }

    let mut new_almanacs: Vec<Almanac> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        let key = dedupe_key(&row.date, row.lat.unwrap_or_default(), row.lon.unwrap_or_default());
        if !seen.insert(key) {
            continue;
        }
        new_raws.push(RawLine { date: row.date.clone(), value: raw_val });
        new_almanacs.push(row);
    }

    // Partition almanac by month of `date`; raw mirrors that.
    // `date` is `YYYY-MM-DD`; Partition::Month takes the first 7 chars → "YYYY-MM".
    contract.append(&new_almanacs, |r| r.date.as_str())?;
    raw_stream.append(&new_raws, |r| r.date.as_str())?;
    Ok(new_almanacs.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let client = LiveClient::new();
    pull_with(vault, &client, Local::now())
}

fn pull_with(vault: &Vault, api: &impl SunriseSunsetApi, now: DateTime<Local>) -> Result<PullOutcome> {
    // Read location from USNO's cursor (shared config; not re-asked here).
    let (lat, lon) = read_usno_location(vault)
        .context("No location configured — set a latitude/longitude in USNO Astronomy")?;

    let mut state = vault.read_ss_sync();
    let today = now.date_naive();

    let start = match state
        .last_date
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    {
        Some(last) => last.succ_opt().unwrap_or(today),
        None => today,
    };
    let start = start
        .max(today - chrono::Duration::days(MAX_BACKFILL_DAYS - 1))
        .min(today);

    let mut rows: Vec<(Almanac, Value)> = Vec::new();
    let mut max_written: Option<NaiveDate> = None;
    let mut day = start;

    while day <= today {
        let date = day.format("%Y-%m-%d").to_string();

        // Try .io first (provides golden_hour, moon data, and local UTC offset).
        // Fall back to .org when:
        //   (a) .io returns a transport/HTTP error, OR
        //   (b) .io returns HTTP 200 but the body fails to map to an Almanac
        //       (e.g. unexpected schema). This ensures the primary path failure
        //       (the original silent-zero bug) is caught and retried on .org.
        //
        // When falling back, extract the .io utc_offset (if the raw value was
        // received) so .org UTC timestamps can be converted to local RFC3339.
        let mapped = match api.fetch_io(&date, lat, lon) {
            Ok(io_val) => {
                match almanac_from_io(&io_val, &date, lat, lon) {
                    Some(pair) => Some(pair),
                    None => {
                        // .io 200 but mapping failed — try .org with offset hint
                        // extracted from the (partially parseable) .io body.
                        let io_off_mins = io_val
                            .get("results")
                            .and_then(|r| r.get("utc_offset"))
                            .and_then(Value::as_i64);
                        match api.fetch_org(&date, lat, lon) {
                            Ok(org_val) => almanac_from_org(&org_val, &date, lat, lon, io_off_mins),
                            Err(_) => None, // both failed; skip day
                        }
                    }
                }
            }
            Err(_) => match api.fetch_org(&date, lat, lon) {
                Ok(val) => almanac_from_org(&val, &date, lat, lon, None),
                Err(e) => {
                    // Transport error on both: stop the window; commit earlier days.
                    if !rows.is_empty() {
                        break;
                    }
                    bail!("sunrise-sunset fetch failed for {date}: {e}");
                }
            },
        };

        if let Some((almanac, raw)) = mapped {
            max_written = Some(max_written.map_or(day, |m: NaiveDate| m.max(day)));
            rows.push((almanac, raw));
        }

        day = match day.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }

    let written = write_rows(vault, rows)?;

    if let Some(m) = max_written {
        let m = m.format("%Y-%m-%d").to_string();
        if state.last_date.as_deref().is_none_or(|cur| m.as_str() > cur) {
            state.last_date = Some(m);
        }
    }
    state.updated = Some(now.to_rfc3339());
    vault.write_ss_sync(&state)?;

    Ok(PullOutcome {
        headline: if written == 0 {
            "Sunrise-Sunset up to date".to_string()
        } else {
            format!("Sunrise-Sunset synced — {written} day(s) at {lat:.4},{lon:.4}")
        },
        counts: [("days", written)].into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ss-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Write a USNO cursor with a configured location so pull_with sees a lat/lon.
    fn set_usno_location(vault: &Vault, lat: f64, lon: f64) {
        let path = vault.resolve(USNO_SYNC_FILE).unwrap();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let state = json!({"location": {"lat": lat, "lon": lon, "place": "Test"}});
        crate::store::write_json_atomic(&path, &state).unwrap();
    }

    // -----------------------------------------------------------------------
    // Fixtures: confirmed API shapes.

    /// Confirmed live .org response for Los Angeles (2026-06-10, formatted=0).
    /// Times are UTC RFC3339 strings; day_length is integer seconds.
    fn org_la() -> Value {
        json!({
            "results": {
                "sunrise": "2026-06-10T12:39:37+00:00",
                "sunset": "2026-06-11T03:05:25+00:00",
                "solar_noon": "2026-06-10T19:52:31+00:00",
                "day_length": 51948,
                "civil_twilight_begin": "2026-06-10T12:11:58+00:00",
                "civil_twilight_end": "2026-06-11T03:33:04+00:00",
                "nautical_twilight_begin": "2026-06-10T11:36:11+00:00",
                "nautical_twilight_end": "2026-06-11T04:08:51+00:00",
                "astronomical_twilight_begin": "2026-06-10T10:57:03+00:00",
                "astronomical_twilight_end": "2026-06-11T04:47:59+00:00"
            },
            "status": "OK",
            "tzid": "UTC"
        })
    }

    /// Confirmed live .io response for Los Angeles (2026-06-17, confirmed via curl).
    /// Real shape: `utc_offset` is integer minutes (-420), NOT a "+HH:MM" string.
    /// Civil twilight: `dawn`/`dusk`; nautical: `nautical_twilight_begin/end`;
    /// astronomical: `first_light`/`last_light`. Moon fields present.
    /// Dates adjusted to 2026-06-10 to align with org fixture for combined tests.
    fn io_la() -> Value {
        json!({
            "results": {
                "date": "June 10, 2026",
                "sunrise": "5:39:37 AM",
                "sunset": "8:05:25 PM",
                "first_light": "3:57:03 AM",
                "last_light": "9:47:59 PM",
                "dawn": "5:11:58 AM",
                "dusk": "8:33:04 PM",
                "solar_noon": "12:52:31 PM",
                "golden_hour": "7:29:14 PM",
                "day_length": "14:25:48",
                "timezone": "America/Los_Angeles",
                "utc_offset": -420,
                "nautical_twilight_begin": "4:36:11 AM",
                "nautical_twilight_end": "9:08:51 PM",
                "sun_altitude": 12.5,
                "sun_azimuth": 95.2,
                "sunrise_azimuth": 60.5,
                "sunset_azimuth": 299.5,
                "moonrise": "8:17:23 AM",
                "moonset": "10:14:55 PM",
                "moon_illumination": 9.98,
                "moon_phase": "Waxing Crescent",
                "moon_phase_value": 0.1,
                "moon_always_up": false,
                "moon_always_down": false,
                "elevation": 101.0
            },
            "status": "OK"
        })
    }

    // -----------------------------------------------------------------------
    // Helper + mapping tests.

    #[test]
    fn seconds_to_hhmm_converts_correctly() {
        assert_eq!(seconds_to_hhmm(51948), "14:25");
        assert_eq!(seconds_to_hhmm(3600), "1:00");
        assert_eq!(seconds_to_hhmm(0), "0:00");
        assert_eq!(seconds_to_hhmm(86399), "23:59");
    }

    #[test]
    fn io_to_rfc3339_converts_12h_to_rfc3339() {
        // PDT = -420 mins = -07:00
        assert_eq!(
            io_to_rfc3339("2026-06-10", "5:39:37 AM", -420),
            Some("2026-06-10T05:39:37-07:00".to_string())
        );
        assert_eq!(
            io_to_rfc3339("2026-06-10", "8:05:25 PM", -420),
            Some("2026-06-10T20:05:25-07:00".to_string())
        );
        // Midnight special case: 12:00:00 AM → 00:xx
        assert_eq!(
            io_to_rfc3339("2026-06-10", "12:00:00 AM", 0),
            Some("2026-06-10T00:00:00+00:00".to_string())
        );
        // Noon special case: 12:00:00 PM → 12:xx
        assert_eq!(
            io_to_rfc3339("2026-06-10", "12:00:00 PM", 0),
            Some("2026-06-10T12:00:00+00:00".to_string())
        );
        // 1 PM → 13
        assert_eq!(
            io_to_rfc3339("2026-06-10", "1:30:00 PM", 60),
            Some("2026-06-10T13:30:00+01:00".to_string())
        );
        // Empty / malformed → None.
        assert_eq!(io_to_rfc3339("2026-06-10", "", -420), None);
        assert_eq!(io_to_rfc3339("2026-06-10", "not-a-time", -420), None);
    }

    #[test]
    fn offset_suffix_from_mins_formats_correctly() {
        assert_eq!(offset_suffix_from_mins(-420), "-07:00");
        assert_eq!(offset_suffix_from_mins(330), "+05:30");
        assert_eq!(offset_suffix_from_mins(0), "+00:00");
    }

    #[test]
    fn utc_to_local_converts_correctly() {
        // UTC-7 (PDT): 12:39:37+00:00 → 05:39:37-07:00
        assert_eq!(
            utc_to_local_rfc3339("2026-06-10T12:39:37+00:00", Some(-420)),
            "2026-06-10T05:39:37-07:00"
        );
        // Zero offset: unchanged.
        assert_eq!(
            utc_to_local_rfc3339("2026-06-10T12:39:37+00:00", Some(0)),
            "2026-06-10T12:39:37+00:00"
        );
        // No offset provided: return verbatim.
        assert_eq!(
            utc_to_local_rfc3339("2026-06-10T12:39:37+00:00", None),
            "2026-06-10T12:39:37+00:00"
        );
        // Sunset crosses midnight in UTC (28:05:25 UTC-7 local = next day 03:05:25 UTC)
        assert_eq!(
            utc_to_local_rfc3339("2026-06-11T03:05:25+00:00", Some(-420)),
            "2026-06-10T20:05:25-07:00"
        );
    }

    #[test]
    fn almanac_from_org_maps_confirmed_live_response_no_offset() {
        // Without offset: UTC times stored verbatim (valid RFC3339 instants).
        let val = org_la();
        let (a, _raw) = almanac_from_org(&val, "2026-06-10", 34.0522, -118.2437, None).unwrap();

        assert_eq!(a.date, "2026-06-10");
        assert_eq!(a.source, "sunrise-sunset");
        assert_eq!(a.lat, Some(34.0522));
        assert_eq!(a.lon, Some(-118.2437));
        // Times stored verbatim (UTC) when no offset supplied.
        assert_eq!(a.sunrise, "2026-06-10T12:39:37+00:00");
        assert_eq!(a.sunset, "2026-06-11T03:05:25+00:00");
        assert_eq!(a.solar_noon, "2026-06-10T19:52:31+00:00");
        assert_eq!(a.civil_twilight_begin, "2026-06-10T12:11:58+00:00");
        assert_eq!(a.civil_twilight_end, "2026-06-11T03:33:04+00:00");
        assert_eq!(a.nautical_twilight_begin, "2026-06-10T11:36:11+00:00");
        assert_eq!(a.nautical_twilight_end, "2026-06-11T04:08:51+00:00");
        assert_eq!(a.astronomical_twilight_begin, "2026-06-10T10:57:03+00:00");
        assert_eq!(a.astronomical_twilight_end, "2026-06-11T04:47:59+00:00");
        // day_length converted from 51948 seconds.
        assert_eq!(a.day_length, "14:25", "51948s = 14h25m");
        // .org has no golden_hour.
        assert!(a.golden_hour.is_empty());
        // .org has no moon data.
        assert!(a.moonrise.is_empty());
        assert!(a.moon_phase.is_empty());
        // Raw seconds stored in extra.
        assert_eq!(a.extra.get("day_length_seconds"), Some(&json!(51948u64)));
    }

    #[test]
    fn almanac_from_org_converts_to_local_when_offset_supplied() {
        // With PDT offset (-420 mins): UTC times converted to local RFC3339.
        let val = org_la();
        let (a, _raw) =
            almanac_from_org(&val, "2026-06-10", 34.0522, -118.2437, Some(-420)).unwrap();

        // 12:39:37+00:00 → 05:39:37-07:00 (PDT)
        assert_eq!(a.sunrise, "2026-06-10T05:39:37-07:00");
        // 03:05:25+00:00 next day → 20:05:25-07:00 same day (correct local time)
        assert_eq!(a.sunset, "2026-06-10T20:05:25-07:00");
        assert_eq!(a.solar_noon, "2026-06-10T12:52:31-07:00");
        assert_eq!(a.civil_twilight_begin, "2026-06-10T05:11:58-07:00");
        assert_eq!(a.astronomical_twilight_begin, "2026-06-10T03:57:03-07:00");
        // Offset stored in extra for reference.
        assert_eq!(
            a.extra.get("utc_offset"),
            Some(&Value::String("-07:00".to_string()))
        );
    }

    #[test]
    fn almanac_from_org_status_not_ok_returns_none() {
        let val = json!({"results": {}, "status": "INVALID_REQUEST"});
        assert!(almanac_from_org(&val, "2026-06-10", 34.05, -118.25, None).is_none());
    }

    #[test]
    fn almanac_from_io_maps_real_shape_with_integer_utc_offset() {
        // Use the fixture with integer utc_offset (-420) — the REAL .io shape.
        // No fixture mutation; the parser must accept integer minutes directly.
        let val = io_la();
        let (a, _raw) = almanac_from_io(&val, "2026-06-10", 34.0522, -118.2437).unwrap();

        assert_eq!(a.date, "2026-06-10");
        assert_eq!(a.source, "sunrise-sunset");
        assert_eq!(a.lat, Some(34.0522));
        assert_eq!(a.lon, Some(-118.2437));

        // Solar times: 12-hour local → RFC3339 local (-07:00).
        // 5:39:37 AM PDT → 05:39:37-07:00
        assert_eq!(a.sunrise, "2026-06-10T05:39:37-07:00");
        // 8:05:25 PM PDT → 20:05:25-07:00
        assert_eq!(a.sunset, "2026-06-10T20:05:25-07:00");
        // 12:52:31 PM PDT → 12:52:31-07:00
        assert_eq!(a.solar_noon, "2026-06-10T12:52:31-07:00");
        // golden_hour is the .io-native field.
        assert_eq!(a.golden_hour, "2026-06-10T19:29:14-07:00");

        // Civil twilight from `dawn`/`dusk` (NOT civil_twilight_begin/end).
        // dawn: 5:11:58 AM → 05:11:58-07:00
        assert_eq!(a.civil_twilight_begin, "2026-06-10T05:11:58-07:00");
        // dusk: 8:33:04 PM → 20:33:04-07:00
        assert_eq!(a.civil_twilight_end, "2026-06-10T20:33:04-07:00");

        // Nautical twilight: actual field names match contract.
        // 4:36:11 AM → 04:36:11-07:00
        assert_eq!(a.nautical_twilight_begin, "2026-06-10T04:36:11-07:00");
        // 9:08:51 PM → 21:08:51-07:00
        assert_eq!(a.nautical_twilight_end, "2026-06-10T21:08:51-07:00");

        // Astronomical twilight from `first_light`/`last_light`.
        // 3:57:03 AM → 03:57:03-07:00
        assert_eq!(a.astronomical_twilight_begin, "2026-06-10T03:57:03-07:00");
        // 9:47:59 PM → 21:47:59-07:00
        assert_eq!(a.astronomical_twilight_end, "2026-06-10T21:47:59-07:00");

        // day_length: "14:25:48" truncated to "14:25" (H:MM contract form).
        assert_eq!(a.day_length, "14:25");

        // Moon fields from confirmed .io response.
        assert_eq!(a.moonrise, "2026-06-10T08:17:23-07:00");
        assert_eq!(a.moonset, "2026-06-10T22:14:55-07:00");
        assert_eq!(a.moon_phase, "Waxing Crescent");

        // Solar geometry and moon_illumination in extra.
        assert!(a.extra.contains_key("moon_illumination"), "moon_illumination in extra");
        assert!(a.extra.contains_key("sun_altitude"), "sun_altitude in extra");
        // Offset stored in extra for reference.
        assert_eq!(
            a.extra.get("utc_offset"),
            Some(&Value::String("-07:00".to_string()))
        );
        // Raw day_length with seconds stored in extra.
        assert_eq!(
            a.extra.get("day_length_raw"),
            Some(&Value::String("14:25:48".to_string()))
        );
    }

    #[test]
    fn almanac_from_io_fails_gracefully_on_string_utc_offset() {
        // If somehow a string offset arrives, serde should fail to parse IoResponse
        // (utc_offset: Option<i64> won't accept a string), returning None.
        // This confirms the old bug: a string offset was silently killing all .io rows.
        let bad_val = json!({
            "results": {
                "sunrise": "5:39:37 AM",
                "sunset": "8:05:25 PM",
                "solar_noon": "12:52:31 PM",
                "day_length": "14:25:48",
                "dawn": "5:11:58 AM",
                "dusk": "8:33:04 PM",
                "golden_hour": "7:29:14 PM",
                "utc_offset": "-07:00",
                "timezone": "America/Los_Angeles"
            },
            "status": "OK"
        });
        // serde should fail: utc_offset is a string but field expects i64 → None
        let result = almanac_from_io(&bad_val, "2026-06-10", 34.05, -118.25);
        assert!(result.is_none(), "string utc_offset must be rejected by the typed parser");
    }

    // -----------------------------------------------------------------------
    // Mock API for pull_with tests.

    struct MockApi {
        /// Per-date fixture responses. Date → (io_result, org_result).
        /// `None` means "HTTP error".
        responses: std::collections::HashMap<String, (Option<Value>, Option<Value>)>,
    }

    impl MockApi {
        fn with_org(date: &str, val: Value) -> Self {
            let mut m = MockApi { responses: Default::default() };
            m.responses.insert(date.to_string(), (None, Some(val)));
            m
        }
        fn with_io(date: &str, val: Value) -> Self {
            let mut m = MockApi { responses: Default::default() };
            m.responses.insert(date.to_string(), (Some(val), None));
            m
        }
    }

    impl SunriseSunsetApi for MockApi {
        fn fetch_io(&self, date: &str, _lat: f64, _lon: f64) -> Result<Value, String> {
            match self.responses.get(date) {
                Some((Some(v), _)) => Ok(v.clone()),
                _ => Err("mock: no io response".to_string()),
            }
        }

        fn fetch_org(&self, date: &str, _lat: f64, _lon: f64) -> Result<Value, String> {
            match self.responses.get(date) {
                Some((_, Some(v))) => Ok(v.clone()),
                Some((_, None)) => Err("mock: no org response".to_string()),
                None => Err("mock: no response".to_string()),
            }
        }
    }

    fn configured_vault(name: &str) -> Vault {
        let v = temp_vault(name);
        set_usno_location(&v, 34.0522, -118.2437);
        v
    }

    #[test]
    fn pull_writes_today_from_org_both_layers() {
        let v = configured_vault("pull_org");
        let api = MockApi::with_org("2026-06-10", org_la());
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();

        let out = pull_with(&v, &api, now).unwrap();
        assert_eq!(out.counts.get("days"), Some(&1));

        // Contract almanac row. .org without offset → UTC times verbatim.
        let body = std::fs::read_to_string(
            v.root().join("environment/sunrise-sunset/almanac/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(body.lines().count(), 1);
        assert!(body.contains("\"date\":\"2026-06-10\""), "{body}");
        assert!(body.contains("\"source\":\"sunrise-sunset\""), "{body}");
        assert!(body.contains("\"sunrise\":\"2026-06-10T12:39:37+00:00\""), "{body}");
        assert!(body.contains("\"nautical_twilight_begin\""), "{body}");
        assert!(body.contains("\"astronomical_twilight_begin\""), "{body}");
        assert!(body.contains("\"day_length\":\"14:25\""), "{body}");

        // Raw layer: full API response verbatim.
        let raw = std::fs::read_to_string(
            v.root().join("environment/sunrise-sunset/raw/2026-06.jsonl"),
        )
        .unwrap();
        assert!(raw.contains("\"tzid\":\"UTC\""), "raw keeps un-mapped fields: {raw}");

        // Watermark advanced.
        assert_eq!(v.read_ss_sync().last_date.as_deref(), Some("2026-06-10"));
    }

    #[test]
    fn pull_writes_golden_hour_and_moon_from_io() {
        // Use the real .io fixture (integer utc_offset; no fixture mutation).
        let v = configured_vault("pull_io");
        let api = MockApi::with_io("2026-06-10", io_la());
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();

        let out = pull_with(&v, &api, now).unwrap();
        assert_eq!(out.counts.get("days"), Some(&1));

        let body = std::fs::read_to_string(
            v.root().join("environment/sunrise-sunset/almanac/2026-06.jsonl"),
        )
        .unwrap();
        // golden_hour and moon data from confirmed .io response.
        assert!(body.contains("\"golden_hour\""), "golden_hour from .io: {body}");
        assert!(body.contains("\"moonrise\""), "moonrise from .io: {body}");
        assert!(body.contains("\"moon_phase\""), "moon_phase from .io: {body}");
        // Local RFC3339 timestamps (not UTC).
        assert!(body.contains("-07:00"), "local offset in .io timestamps: {body}");
        // day_length normalized to H:MM form.
        assert!(body.contains("\"day_length\":\"14:25\""), "day_length normalized: {body}");
    }

    #[test]
    fn pull_io_ok_but_unmappable_falls_back_to_org() {
        // Regression for the silent-zero bug: .io returns 200 but with a shape
        // that fails to parse (e.g. old string utc_offset) → falls back to .org.
        struct IoOkButUnparseable(Value, Value);
        impl SunriseSunsetApi for IoOkButUnparseable {
            fn fetch_io(&self, _date: &str, _lat: f64, _lon: f64) -> Result<Value, String> {
                Ok(self.0.clone()) // 200 but unmappable
            }
            fn fetch_org(&self, _date: &str, _lat: f64, _lon: f64) -> Result<Value, String> {
                Ok(self.1.clone())
            }
        }
        // .io body with string utc_offset → IoResponse deserialization fails → None
        let bad_io = json!({
            "results": {
                "sunrise": "5:39:37 AM", "sunset": "8:05:25 PM",
                "solar_noon": "12:52:31 PM", "day_length": "14:25:48",
                "dawn": "5:11:58 AM", "dusk": "8:33:04 PM",
                "golden_hour": "7:29:14 PM",
                "utc_offset": "-07:00",  // string → deserialization fails
                "timezone": "America/Los_Angeles"
            },
            "status": "OK"
        });
        let v = configured_vault("io_fallback_on_map_fail");
        let api = IoOkButUnparseable(bad_io, org_la());
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();

        let out = pull_with(&v, &api, now).unwrap();
        // Must fall back to .org and write 1 row (not 0 — the original silent bug).
        assert_eq!(out.counts.get("days"), Some(&1), "fallback must write row");
        let body = std::fs::read_to_string(
            v.root().join("environment/sunrise-sunset/almanac/2026-06.jsonl"),
        )
        .unwrap();
        assert!(body.contains("\"date\":\"2026-06-10\""), "{body}");
    }

    #[test]
    fn pull_falls_back_to_org_when_io_fails() {
        let v = configured_vault("pull_fallback");
        // .io returns error; .org has the confirmed response.
        let api = MockApi::with_org("2026-06-10", org_la());
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();

        let out = pull_with(&v, &api, now).unwrap();
        assert_eq!(out.counts.get("days"), Some(&1));

        let body = std::fs::read_to_string(
            v.root().join("environment/sunrise-sunset/almanac/2026-06.jsonl"),
        )
        .unwrap();
        assert!(body.contains("\"source\":\"sunrise-sunset\""), "{body}");
        // From .org: golden_hour absent (omit-if-empty).
        assert!(!body.contains("\"golden_hour\""), "no golden_hour from .org: {body}");
    }

    #[test]
    fn dedupe_prevents_re_writing_same_day() {
        let v = configured_vault("dedupe");
        let api = MockApi::with_org("2026-06-10", org_la());
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();

        let out1 = pull_with(&v, &api, now).unwrap();
        assert_eq!(out1.counts.get("days"), Some(&1));

        // Reset watermark so pull_with re-fetches the same day.
        let mut st = v.read_ss_sync();
        st.last_date = None;
        v.write_ss_sync(&st).unwrap();

        let out2 = pull_with(&v, &api, now).unwrap();
        assert_eq!(out2.counts.get("days"), Some(&0), "already stored");

        // File unchanged.
        let body = std::fs::read_to_string(
            v.root().join("environment/sunrise-sunset/almanac/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(body.lines().count(), 1, "still only one row");
    }

    #[test]
    fn backfill_is_capped() {
        let v = configured_vault("cap");
        let mut st = v.read_ss_sync();
        st.last_date = Some("2025-06-11".into());
        v.write_ss_sync(&st).unwrap();

        // Build mock with responses for any date: a map that returns org_la for all.
        struct AllDaysOrg(Value);
        impl SunriseSunsetApi for AllDaysOrg {
            fn fetch_io(&self, _date: &str, _lat: f64, _lon: f64) -> Result<Value, String> {
                Err("no io".to_string())
            }
            fn fetch_org(&self, _date: &str, _lat: f64, _lon: f64) -> Result<Value, String> {
                Ok(self.0.clone())
            }
        }

        let now = Local.with_ymd_and_hms(2026, 6, 11, 9, 0, 0).single().unwrap();
        // We can't easily count requests on AllDaysOrg without extra scaffolding,
        // but we can assert the written row count is capped to MAX_BACKFILL_DAYS.
        let out = pull_with(&v, &AllDaysOrg(org_la()), now).unwrap();
        assert_eq!(out.counts.get("days"), Some(&(MAX_BACKFILL_DAYS as u64)));
    }

    #[test]
    fn no_usno_location_errors_clearly() {
        let v = temp_vault("noloc");
        let api = MockApi::with_org("2026-06-10", org_la());
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();
        let err = pull_with(&v, &api, now).unwrap_err().to_string();
        assert!(err.contains("No location"), "clear error: {err}");
    }

    #[test]
    fn cursor_back_compat_empty_deserializes() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_date.is_none());
        let partial: SyncState =
            serde_json::from_str(r#"{"last_date":"2026-06-01"}"#).unwrap();
        assert_eq!(partial.last_date.as_deref(), Some("2026-06-01"));
        assert!(partial.updated.is_none());
    }

    #[test]
    fn def_metadata_is_correct() {
        assert_eq!(DEF.meta.id, "sunrise-sunset");
        assert_eq!(DEF.meta.domain, "environment");
        assert!(DEF.connection.is_none());
        assert!(DEF.pull.is_some());
    }
}
