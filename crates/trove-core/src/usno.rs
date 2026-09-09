//! USNO Astronomical Applications API — the authoritative, keyless source for
//! daily solar and lunar geometry, pulled into the bound
//! [`crate::environment`] `Almanac` contract. Catalogued in the Phase 2 pass;
//! brief: docs/integrations/usno.md. **First collector to write the
//! `environment` *almanac* shape** — this build binds that shape (see
//! `crate::environment::Almanac` / `crate::contracts`).
//!
//! A **Periodic** (daily) pull over one keyless endpoint, one configured
//! location:
//!
//! - `GET https://aa.usno.navy.mil/api/rstt/oneday?date=YYYY-MM-DD&coords=LAT,LON&tz=OFF`
//!   → one day of sun + **civil** twilight + moon data. Each day becomes an
//!   [`crate::environment::Almanac`] under `environment/usno/almanac/YYYY-MM.jsonl`
//!   (keyed by `date` + rounded coords). The API returns sun/moon events as a
//!   `sundata` / `moondata` array of `{phen, time}` where `time` is the local
//!   `HH:MM` for the requested `date` and offset (or JSON `null` when the body
//!   stays above/below the horizon, e.g. Arctic summer); we assemble each into
//!   an RFC3339 local timestamp `date T HH:MM:00 ±OFF`. `curphase` →
//!   `moon_phase`; `fracillum` (e.g. `"25%"`) rides verbatim in `extra` and is
//!   also normalized to a numeric `extra.illumination` 0–1 fraction (the
//!   contract's worked-example key); the `closestphase` object rides in `extra`.
//!   **Golden hour is computed** (sunset − 1 h) per the brief, since USNO doesn't
//!   name it.
//!
//! ### Twilight: civil only (a deliberate capability bound)
//!
//! USNO's `rstt/oneday` computes **civil** twilight only — confirmed by the
//! official docs ("also computes the times at which civil twilight begins and
//! ends") and by 5 live probes (LA summer/winter, London solstice, equator
//! equinox, Svalbard winter), which returned only `Begin/End Civil Twilight`. No
//! USNO endpoint returns nautical or astronomical twilight, so the contract's
//! `nautical_*`/`astronomical_*` fields stay empty (omit-if-empty) for this
//! source. USNO's edge over a plain sunrise/sunset feed is its authoritative
//! **moon** data + civil-twilight bounds — not extra twilight stages.
//!
//! Two layers: the **raw** API `properties.data` object verbatim under
//! `environment/usno/raw/YYYY-MM.jsonl` (full fidelity, unconditional), and the
//! normalized **contract** almanac rows, deduped by `(date, lat, lon)` — the
//! contract's natural key, since an almanac carries no `guid`.
//!
//! ## Location (keyless, but it needs a place)
//!
//! USNO is truly keyless, but a daily astronomy pull needs a latitude/longitude
//! to compute geometry for. We capture it through the registry-driven connect
//! card (a [`ConnectMethod::TokenPaste`] whose "token" is the coordinate string
//! `LAT,LON` or `LAT,LON,Label`) — the only declarative text input the hub
//! renders. The coordinate is **non-secret configuration**, so it is stored in
//! the plain rebuildable cursor (`.trove/usno-sync.json`), never the 0600 secret
//! store. No account, no API key, nothing sensitive ever leaves the machine but
//! the lat/lon in the request URL.
//!
//! ## Cursor / backfill
//!
//! The non-secret cursor (`.trove/usno-sync.json`) holds the location plus the
//! last `date` successfully written. A pull fetches every date from the day
//! after that watermark through today (capped at [`MAX_BACKFILL_DAYS`] so a long
//! gap can't issue thousands of calls), one call per day. The watermark advances
//! only after a day's row is written, so a crash re-fetches rather than skips.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::environment::Almanac;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer almanac stream; raw under `raw/`.
const DIR: &str = "environment/usno/almanac";
const RAW_DIR: &str = "environment/usno/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that is for 0600
/// secrets). Holds the location (non-secret config) plus the last date written.
/// Deleting it just re-asks for the configured location from scratch.
const SYNC_FILE: &str = ".trove/usno-sync.json";

const API_BASE: &str = "https://aa.usno.navy.mil";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Checked this often; the daily gate runs it at most once per local day.
pub const USNO_SYNC_SECS: u64 = 6 * 3600;
/// Most days to backfill in one pull when the cursor is far behind (or unset) —
/// one HTTP call per day, so cap it. A first sync writes just today; a resumed
/// gap fills up to this many days per pull and catches up over a few ticks.
const MAX_BACKFILL_DAYS: i64 = 35;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// an unset location or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("usno almanac synced — {n} day(s)")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("usno sync skipped: {e}"))),
    }
}

// Manual "Sync now": surfaces errors (not configured) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "usno",
        name: "USNO Astronomy",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Records daily solar and lunar data from the US Naval Observatory: \
                      sunrise, sunset, solar noon, civil twilight, moonrise, moonset, \
                      moon phase, and illumination for your location — the authoritative, \
                      keyless almanac (valid 1700–2100).",
        domain: "environment",
        vault_path: "environment/usno/",
        toggleable: true,
        setup: &[
            "Set your location (latitude, longitude) on this card — USNO needs a point to \
             compute sun and moon geometry for.",
            "Each day it records sunrise/sunset, twilight, and moon data for that location.",
        ],
        caveats: "Keyless — no account or API key, only the latitude/longitude you set leaves \
                  the machine (in the request URL). One call per day per location.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::daily(USNO_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("usno"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = a coordinate string, NON-secret config).

/// Parse + validate the pasted location and store it in the (non-secret)
/// cursor. Accepts `LAT,LON` or `LAT,LON,Place Label`. Rejects out-of-range or
/// unparseable coordinates with a clear message. Never touches the secret store
/// — the lat/lon is plain configuration.
fn def_connect(vault: &Vault, coords: &str) -> Result<()> {
    let loc = parse_location(coords)?;
    let mut state = vault.read_usno_sync();
    state.location = Some(loc);
    vault.write_usno_sync(&state)
}

/// Forget the configured location (and the date watermark). Synced almanac data
/// stays in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    let mut state = vault.read_usno_sync();
    state.location = None;
    state.last_date = None;
    vault.write_usno_sync(&state)
}

/// Connected = a location is configured.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(loc) = vault.read_usno_sync().location {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: loc.label(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: Default::default(),
        });
    }
    // Keyless: nothing to bring (no app credentials) — connecting is just
    // setting a location.
    Ok(ConnectStatus { configured: true, accounts })
}

/// The disconnect key / account id — keyless, so a single fixed slot.
const SERVICE: &str = "usno";

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// location. Keyless — the "token" is just the coordinate string, stored as
/// plain config, never a secret.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "usno",
    display_name: "USNO Astronomy",
    methods: &[ConnectMethod::TokenPaste {
        label: "Location (latitude, longitude)",
        help: "Enter the latitude and longitude to compute sun and moon times for, e.g. \
               34.05,-118.25 (optionally add a place name: 34.05,-118.25,Los Angeles). \
               USNO is keyless — only this coordinate is ever sent, stored locally.",
        placeholder: "34.05,-118.25",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["usno"],
    setup: &[
        "Find the latitude and longitude of the place you want (e.g. from Maps).",
        "Enter it as lat,lon — optionally lat,lon,Place Name.",
        "No account or key is needed; it's stored locally and used to query USNO.",
    ],
};

// ---------------------------------------------------------------------------
// Location config (non-secret).

/// A configured query point. Coordinates are rounded to the contract's
/// dedupe precision so the same place always produces the same key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Location {
    lat: f64,
    lon: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    place: String,
}

impl Location {
    fn label(&self) -> String {
        if self.place.is_empty() {
            format!("{:.4}, {:.4}", self.lat, self.lon)
        } else {
            self.place.clone()
        }
    }
}

/// Round a coordinate to 4 decimal places (~11 m) — the precision the contract
/// example uses and the granularity at which the almanac is deduped, so jitter
/// in a stored value never spawns a duplicate day row.
fn round_coord(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

/// Parse `LAT,LON` or `LAT,LON,Place Label` into a [`Location`], validating
/// ranges (lat ∈ [-90, 90], lon ∈ [-180, 180]). The label may itself contain
/// commas (only the first two fields are the numbers).
fn parse_location(s: &str) -> Result<Location> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty location — enter a latitude and longitude, e.g. 34.05,-118.25");
    }
    let mut parts = s.splitn(3, ',');
    let lat_s = parts.next().unwrap_or("").trim();
    let lon_s = parts.next().context(
        "location needs both a latitude and a longitude, comma-separated (e.g. 34.05,-118.25)",
    )?;
    let lat: f64 = lat_s
        .parse()
        .with_context(|| format!("latitude {lat_s:?} is not a number (expected e.g. 34.05)"))?;
    let lon: f64 = lon_s
        .trim()
        .parse()
        .with_context(|| format!("longitude {:?} is not a number (expected e.g. -118.25)", lon_s.trim()))?;
    if !(-90.0..=90.0).contains(&lat) {
        bail!("latitude {lat} is out of range (must be between -90 and 90)");
    }
    if !(-180.0..=180.0).contains(&lon) {
        bail!("longitude {lon} is out of range (must be between -180 and 180)");
    }
    let place = parts.next().unwrap_or("").trim().to_string();
    Ok(Location { lat: round_coord(lat), lon: round_coord(lon), place })
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The configured query point (non-secret config).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<Location>,
    /// Last calendar day (`YYYY-MM-DD`) successfully written — the lower bound
    /// (exclusive) for the next pull's date window. Advanced only after a row
    /// is written, so a crash re-fetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_date: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_usno_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_usno_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity API `properties.data` object). The on-disk line
// is the verbatim data object, tagged with the contract `date` purely so the
// month-partition writer files it under the right month. Only `value` is
// serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    date: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

#[derive(Debug)]
enum FetchError {
    /// USNO returned a body with an `error` field (bad date/coords). Carries the
    /// message; not retried.
    Api(String),
    /// Network / transport / non-200.
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Api(m) => write!(f, "USNO error: {m}"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The one endpoint the pull needs, as a trait so tests drive the
/// mapping/persist logic with fixtures rather than the network. Returns the full
/// JSON response (we keep `properties.data` for the raw layer and map it).
trait UsnoApi {
    /// `GET /api/rstt/oneday` for one `date` at `lat,lon` with UTC offset
    /// `tz_hours`. Returns the parsed JSON response.
    fn oneday(&self, date: &str, lat: f64, lon: f64, tz_hours: f64) -> Result<Value, FetchError>;
}

/// Thin client; base URL injected (the readwise/todoist pattern).
struct UsnoClient {
    base: String,
}

impl UsnoClient {
    fn new(base: String) -> Self {
        UsnoClient { base }
    }
}

impl UsnoApi for UsnoClient {
    fn oneday(&self, date: &str, lat: f64, lon: f64, tz_hours: f64) -> Result<Value, FetchError> {
        // USNO wants tz as a number (may be fractional, e.g. 5.5). It does the
        // DST bookkeeping itself given `dst`; we pass the location's *current*
        // standard/observed offset and let `isdst` in the response tell us what
        // it used. We pass dst=false and supply the exact offset we want the
        // times anchored to, matching the offset we then write into the RFC3339
        // stamps.
        let url = format!(
            "{}/api/rstt/oneday?date={date}&coords={lat},{lon}&tz={tz_hours}&dst=false",
            self.base
        );
        match ureq::get(&url).timeout(HTTP_TIMEOUT).call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                if let Some(err) = v.get("error") {
                    let msg = err.as_str().map(str::to_string).unwrap_or_else(|| err.to_string());
                    return Err(FetchError::Api(msg));
                }
                Ok(v)
            }
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

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// Format a signed UTC offset in hours (possibly fractional) as an RFC3339
/// zone suffix `±HH:MM`. `-7.0 → "-07:00"`, `5.5 → "+05:30"`.
fn offset_suffix(tz_hours: f64) -> String {
    let total_min = (tz_hours * 60.0).round() as i64;
    let sign = if total_min < 0 { '-' } else { '+' };
    let abs = total_min.abs();
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// Assemble an RFC3339 local timestamp from the query `date`, a USNO `"HH:MM"`
/// local clock time, and the offset suffix. `None` when `time` is JSON null or
/// not an `HH:MM` string (the body never rose/set that day). USNO reports the
/// event on the requested calendar `date`, so no day rollover is applied.
fn local_ts(date: &str, time: &Value, off: &str) -> Option<String> {
    let hhmm = time.as_str()?.trim();
    // Guard the shape: exactly HH:MM of digits. Anything else (a phen with no
    // time, an unexpected token) is skipped rather than written malformed.
    let (h, m) = hhmm.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(format!("{date}T{hhmm}:00{off}"))
}

/// The `time` value for the first entry in `arr` whose `phen` equals `phen`.
/// Returns the raw `Value` (which may be a string or null) so the caller decides
/// whether it yields a timestamp.
fn phen_time<'a>(arr: &'a [Value], phen: &str) -> Option<&'a Value> {
    arr.iter()
        .find(|e| e.get("phen").and_then(Value::as_str) == Some(phen))
        .and_then(|e| e.get("time"))
}

/// Compute evening golden hour as one hour before sunset, in the same offset.
/// `None` when there is no sunset (polar day/night) or it doesn't parse.
fn golden_hour_from_sunset(sunset: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(sunset)
        .ok()
        .map(|t| (t - chrono::Duration::hours(1)).to_rfc3339())
}

/// Parse USNO's source-native illumination string (`"25%"`, possibly with stray
/// whitespace) into a 0–1 fraction (`0.25`). `None` if it isn't an `N%` integer
/// percentage, so a malformed value never invents a number.
fn parse_fracillum(frac: &str) -> Option<f64> {
    let pct = frac.trim().strip_suffix('%')?.trim();
    let n: f64 = pct.parse().ok()?;
    Some(n / 100.0)
}

/// Map one USNO `properties.data` object (for query `date` at `lat,lon`,
/// offset `off`) → a contract [`Almanac`]. `None` only if the data object is
/// unusable (no `sundata`/`moondata` arrays at all — should not happen for a
/// valid response).
fn almanac_from(data: &Value, date: &str, lat: f64, lon: f64, off: &str) -> Almanac {
    let sun = data.get("sundata").and_then(Value::as_array).cloned().unwrap_or_default();
    let moon = data.get("moondata").and_then(Value::as_array).cloned().unwrap_or_default();

    let sun_ts = |phen: &str| phen_time(&sun, phen).and_then(|t| local_ts(date, t, off)).unwrap_or_default();
    let moon_ts = |phen: &str| phen_time(&moon, phen).and_then(|t| local_ts(date, t, off)).unwrap_or_default();

    let sunrise = sun_ts("Rise");
    let sunset = sun_ts("Set");
    // Evening golden hour: USNO doesn't name it, so compute sunset − 1h.
    let golden_hour = if sunset.is_empty() {
        String::new()
    } else {
        golden_hour_from_sunset(&sunset).unwrap_or_default()
    };

    // fracillum ("25%") and the named-phase context ride verbatim in extra.
    let mut extra = Map::new();
    if let Some(frac) = data.get("fracillum").and_then(Value::as_str) {
        let frac = frac.trim();
        if !frac.is_empty() {
            extra.insert("fracillum".into(), Value::String(frac.to_string()));
            // Also expose a normalized numeric `illumination` fraction (the
            // contract's worked example key, `extra.illumination: 0.78`) so usno
            // and any sunrise-sunset feed surface the same key. fracillum is the
            // raw source string ("25%"); illumination is it as a 0–1 fraction.
            if let Some(frac01) = parse_fracillum(frac) {
                extra.insert(
                    "illumination".into(),
                    Value::Number(serde_json::Number::from_f64(frac01).unwrap()),
                );
            }
        }
    }
    if let Some(cp) = data.get("closestphase") {
        if !cp.is_null() {
            extra.insert("closestphase".into(), cp.clone());
        }
    }
    if let Some(isdst) = data.get("isdst").and_then(Value::as_bool) {
        extra.insert("isdst".into(), Value::Bool(isdst));
    }

    Almanac {
        date: date.to_string(),
        source: "usno".into(),
        lat: Some(lat),
        lon: Some(lon),
        sunrise,
        sunset,
        // "Upper Transit" of the sun is solar noon.
        solar_noon: sun_ts("Upper Transit"),
        // USNO's rstt/oneday computes **civil** twilight only — confirmed by the
        // official docs ("also computes the times at which civil twilight begins
        // and ends") and live probes (LA summer/winter, equator, high-Arctic all
        // returned only Begin/End Civil Twilight). It emits no nautical or
        // astronomical twilight phenomena, and no USNO endpoint does, so those
        // four contract fields stay empty (omit-if-empty) for this source. USNO's
        // edge over a plain sunrise/sunset feed is its **moon** data + the
        // authoritative civil-twilight bounds, not extra twilight stages.
        civil_twilight_begin: sun_ts("Begin Civil Twilight"),
        civil_twilight_end: sun_ts("End Civil Twilight"),
        nautical_twilight_begin: String::new(),
        nautical_twilight_end: String::new(),
        astronomical_twilight_begin: String::new(),
        astronomical_twilight_end: String::new(),
        // USNO doesn't return a formatted day length; readers derive it from
        // sunrise/sunset, so leave it empty (omit-if-empty).
        day_length: String::new(),
        golden_hour,
        moonrise: moon_ts("Rise"),
        moonset: moon_ts("Set"),
        moon_phase: data
            .get("curphase")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .unwrap_or_default(),
        extra,
    }
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by the almanac's natural key (date+coords).

/// The dedupe key for an almanac row — `date` plus rounded coords, the
/// contract's stated key (an almanac has no `guid`). Coordinates are rounded so
/// re-serialized float jitter can't spawn a duplicate.
fn dedupe_key(date: &str, lat: f64, lon: f64) -> String {
    format!("{date}@{:.4},{:.4}", round_coord(lat), round_coord(lon))
}

/// Append new contract + raw rows, skipping any (date, coords) already on disk.
/// Returns the number of new contract rows written.
fn write_rows(vault: &Vault, rows: Vec<(Almanac, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing (date, coords) keys — re-runnable: a re-pull of an overlapping
    // window never duplicates a day.
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

    let mut new_rows: Vec<Almanac> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (row, raw_val) in rows {
        let key = dedupe_key(&row.date, row.lat.unwrap_or_default(), row.lon.unwrap_or_default());
        if !seen.insert(key) {
            continue; // already stored
        }
        new_raws.push(RawLine { date: row.date.clone(), value: raw_val });
        new_rows.push(row);
    }

    // Contract rows partition by the month of `date`; raw mirrors that.
    contract.append(&new_rows, |r| &r.date)?;
    raw.append(&new_raws, |r| &r.date)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the configured location and sync. Missing location ⇒ a quiet skip on
/// the periodic path (mirror todoist/readwise), a clear error on the manual
/// path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let client = UsnoClient::new(API_BASE.to_string());
    pull_with(vault, &client, Local::now())
}

/// The pull body over an injected API + a fixed "now" — the testable seam.
fn pull_with(vault: &Vault, api: &impl UsnoApi, now: DateTime<Local>) -> Result<PullOutcome> {
    let mut state = vault.read_usno_sync();
    let loc = state
        .location
        .clone()
        .context("USNO has no location set — add a latitude/longitude in the Integrations tab")?;

    // The query offset is the location's *observed* current offset. We anchor
    // every timestamp in the response to this same offset (USNO returns local
    // clock times for the offset we request).
    let tz_hours = now.offset().local_minus_utc() as f64 / 3600.0;
    let off = offset_suffix(tz_hours);
    let today = now.date_naive();

    // Date window: the day after the watermark through today, capped. A first
    // sync (no watermark) writes just today.
    let start = match state.last_date.as_deref().and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()) {
        Some(last) => last.succ_opt().unwrap_or(today),
        None => today,
    };
    let start = start.max(today - chrono::Duration::days(MAX_BACKFILL_DAYS - 1)).min(today);

    let mut rows: Vec<(Almanac, Value)> = Vec::new();
    let mut max_written: Option<NaiveDate> = None;
    let mut day = start;
    while day <= today {
        let date = day.format("%Y-%m-%d").to_string();
        match api.oneday(&date, loc.lat, loc.lon, tz_hours) {
            Ok(resp) => {
                // Keep `properties.data` for the raw layer + map it.
                let data = resp
                    .get("properties")
                    .and_then(|p| p.get("data"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if data.is_object() {
                    let almanac = almanac_from(&data, &date, loc.lat, loc.lon, &off);
                    rows.push((almanac, data));
                    max_written = Some(max_written.map_or(day, |m| m.max(day)));
                }
            }
            // A per-day API error (e.g. a date outside 1700–2100) shouldn't sink
            // the whole window; skip that day and keep going. A transport error
            // ends the window here so the watermark only covers what we wrote.
            Err(FetchError::Api(_)) => {}
            Err(e) => {
                // Persist what we already gathered before surfacing the error,
                // so a mid-window network blip still commits earlier days.
                if !rows.is_empty() {
                    break;
                }
                return Err(anyhow::anyhow!("USNO oneday fetch failed for {date}: {e}"));
            }
        }
        day = match day.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }

    let written = write_rows(vault, rows)?;

    // Advance the watermark to the newest day actually written, and only
    // forward — so a crash before this re-fetches the un-committed tail.
    if let Some(m) = max_written {
        let m = m.format("%Y-%m-%d").to_string();
        if state.last_date.as_deref().is_none_or(|cur| m.as_str() > cur) {
            state.last_date = Some(m);
        }
    }
    state.updated = Some(now.to_rfc3339());
    vault.write_usno_sync(&state)?;

    Ok(PullOutcome {
        headline: if written == 0 {
            "USNO almanac up to date".to_string()
        } else {
            format!("USNO almanac synced — {written} day(s) at {}", loc.label())
        },
        counts: [("days", written)].into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use chrono::TimeZone;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-usno-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures: the documented rstt/oneday shape (confirmed against a live
    //     apiversion 4.0.1 response on 2026-06-15). ------------------------

    /// A full Los Angeles day (lat 34.05, lon -118.25, tz -7) with sun rise/set/
    /// transit + civil twilight, moon rise/transit/set, curphase, fracillum, and
    /// the closestphase object. The exact field names/structure USNO returns.
    fn oneday_la() -> Value {
        json!({
            "apiversion": "4.0.1",
            "geometry": {"coordinates": [-118.25, 34.05], "type": "Point"},
            "properties": {
                "data": {
                    "closestphase": {"day": 8, "month": 6, "phase": "Last Quarter", "time": "03:00", "year": 2026},
                    "curphase": "Waning Crescent",
                    "day": 10,
                    "day_of_week": "Wednesday",
                    "fracillum": "25%",
                    "isdst": false,
                    "label": null,
                    "month": 6,
                    "moondata": [
                        {"phen": "Rise", "time": "02:01"},
                        {"phen": "Upper Transit", "time": "08:37"},
                        {"phen": "Set", "time": "15:22"}
                    ],
                    "sundata": [
                        {"phen": "Begin Civil Twilight", "time": "05:12"},
                        {"phen": "Rise", "time": "05:41"},
                        {"phen": "Upper Transit", "time": "12:53"},
                        {"phen": "Set", "time": "20:04"},
                        {"phen": "End Civil Twilight", "time": "20:33"}
                    ],
                    "tz": -7.0,
                    "year": 2026
                }
            },
            "type": "Feature"
        })
    }

    /// A high-Arctic summer day (midnight sun): the sun never rises/sets, so
    /// USNO emits descriptive phens with `"time": null` (continuously above the
    /// Horizon / Twilight Limit) plus its lower/upper transits, and no rise/set/
    /// civil-twilight events. The moon still has real times. **Captured verbatim
    /// from the live `apiversion 4.0.1` response for 70N,-150 on the 2026-06-21
    /// solstice (tz -9)** — the exact bytes USNO returned.
    fn oneday_arctic() -> Value {
        json!({
            "apiversion": "4.0.1",
            "geometry": {"coordinates": [-150.0, 70.0], "type": "Point"},
            "properties": {
                "data": {
                    "closestphase": {"day": 21, "month": 6, "phase": "First Quarter", "time": "12:55", "year": 2026},
                    "curphase": "First Quarter",
                    "day": 21,
                    "day_of_week": "Sunday",
                    "fracillum": "50%",
                    "isdst": false,
                    "label": null,
                    "month": 6,
                    "moondata": [
                        {"phen": "Set", "time": "00:42"},
                        {"phen": "Rise", "time": "13:29"},
                        {"phen": "Upper Transit", "time": "19:08"}
                    ],
                    "sundata": [
                        {"phen": "Object continuously above the Horizon", "time": null},
                        {"phen": "Object continuously above the Twilight Limit", "time": null},
                        {"phen": "Lower Transit", "time": "01:02"},
                        {"phen": "Upper Transit", "time": "13:02"}
                    ],
                    "tz": -9.0,
                    "year": 2026
                }
            },
            "type": "Feature"
        })
    }

    fn data_of(resp: &Value) -> Value {
        resp["properties"]["data"].clone()
    }

    // --- pure helper tests -----------------------------------------------

    #[test]
    fn offset_suffix_formats_whole_and_fractional_hours() {
        assert_eq!(offset_suffix(-7.0), "-07:00");
        assert_eq!(offset_suffix(0.0), "+00:00");
        assert_eq!(offset_suffix(5.5), "+05:30");
        assert_eq!(offset_suffix(-9.5), "-09:30");
        assert_eq!(offset_suffix(1.0), "+01:00");
    }

    #[test]
    fn local_ts_assembles_or_skips() {
        let off = "-07:00";
        assert_eq!(
            local_ts("2026-06-10", &json!("05:41"), off).as_deref(),
            Some("2026-06-10T05:41:00-07:00")
        );
        // JSON null (no rise/set that day) → no timestamp.
        assert_eq!(local_ts("2026-06-10", &Value::Null, off), None);
        // Non-HH:MM tokens are skipped, never written malformed.
        assert_eq!(local_ts("2026-06-10", &json!("9:41"), off), None, "needs 2-digit hour");
        assert_eq!(local_ts("2026-06-10", &json!("0541"), off), None, "needs a colon");
        assert_eq!(local_ts("2026-06-10", &json!("ab:cd"), off), None, "needs digits");
    }

    #[test]
    fn golden_hour_is_one_hour_before_sunset() {
        let g = golden_hour_from_sunset("2026-06-10T20:04:00-07:00").unwrap();
        // Same instant minus an hour, same offset.
        assert_eq!(
            DateTime::parse_from_rfc3339(&g).unwrap(),
            DateTime::parse_from_rfc3339("2026-06-10T19:04:00-07:00").unwrap()
        );
    }

    #[test]
    fn parse_fracillum_to_fraction_or_none() {
        assert_eq!(parse_fracillum("25%"), Some(0.25));
        assert_eq!(parse_fracillum("100%"), Some(1.0));
        assert_eq!(parse_fracillum("0%"), Some(0.0));
        assert_eq!(parse_fracillum(" 49 %"), Some(0.49), "tolerates stray whitespace");
        // Malformed → None (never invents a number).
        assert_eq!(parse_fracillum("waning"), None);
        assert_eq!(parse_fracillum("25"), None, "needs the percent sign");
        assert_eq!(parse_fracillum(""), None);
    }

    #[test]
    fn usno_emits_civil_twilight_only_never_nautical_or_astro() {
        // Locks in the civil-only decision: USNO's rstt/oneday computes civil
        // twilight only (official docs + 5 live probes). Even if a response were
        // to carry Begin/End Nautical/Astronomical Twilight phens, this collector
        // does NOT map them — those four contract fields are hardcoded empty for
        // USNO (the moon data + civil bounds are its edge, not extra stages). A
        // future regression that re-adds `sun_ts("Begin Nautical Twilight")` would
        // break this test.
        let mut data = data_of(&oneday_la());
        // Inject nautical/astro phens that USNO never actually returns.
        data["sundata"].as_array_mut().unwrap().extend([
            json!({"phen": "Begin Nautical Twilight", "time": "04:39"}),
            json!({"phen": "End Nautical Twilight", "time": "21:06"}),
            json!({"phen": "Begin Astronomical Twilight", "time": "03:58"}),
            json!({"phen": "End Astronomical Twilight", "time": "21:47"}),
        ]);
        let a = almanac_from(&data, "2026-06-10", 34.05, -118.25, "-07:00");
        // Civil twilight (which USNO really does emit) is still mapped…
        assert_eq!(a.civil_twilight_begin, "2026-06-10T05:12:00-07:00");
        // …but nautical/astro are never sourced from the response.
        assert!(a.nautical_twilight_begin.is_empty());
        assert!(a.nautical_twilight_end.is_empty());
        assert!(a.astronomical_twilight_begin.is_empty());
        assert!(a.astronomical_twilight_end.is_empty());
    }

    #[test]
    fn parse_location_validates_and_rounds() {
        let l = parse_location("34.05,-118.25").unwrap();
        assert_eq!(l.lat, 34.05);
        assert_eq!(l.lon, -118.25);
        assert!(l.place.is_empty());
        // A label (which may contain commas) is preserved.
        let l = parse_location("34.05, -118.25, Los Angeles, CA").unwrap();
        assert_eq!(l.place, "Los Angeles, CA");
        // High-precision input rounds to 4 dp (the dedupe granularity).
        let l = parse_location("34.0523456,-118.2512345").unwrap();
        assert_eq!(l.lat, 34.0523);
        assert_eq!(l.lon, -118.2512);
        // Range + parse errors are rejected with a message, never a panic.
        assert!(parse_location("").is_err());
        assert!(parse_location("34.05").is_err(), "needs both lat and lon");
        assert!(parse_location("north,-118").is_err(), "non-numeric lat");
        assert!(parse_location("91,0").is_err(), "lat out of range");
        assert!(parse_location("0,-181").is_err(), "lon out of range");
    }

    // --- mapping tests (the evidence-confirmed shape) --------------------

    #[test]
    fn maps_full_la_day_with_sun_moon_twilight_golden_hour_and_extra() {
        let a = almanac_from(&data_of(&oneday_la()), "2026-06-10", 34.05, -118.25, "-07:00");
        assert_eq!(a.date, "2026-06-10");
        assert_eq!(a.source, "usno");
        assert_eq!(a.lat, Some(34.05));
        assert_eq!(a.lon, Some(-118.25));
        // Sun events, assembled to RFC3339 local with the query offset.
        assert_eq!(a.sunrise, "2026-06-10T05:41:00-07:00");
        assert_eq!(a.sunset, "2026-06-10T20:04:00-07:00");
        assert_eq!(a.solar_noon, "2026-06-10T12:53:00-07:00", "Upper Transit → solar noon");
        assert_eq!(a.civil_twilight_begin, "2026-06-10T05:12:00-07:00");
        assert_eq!(a.civil_twilight_end, "2026-06-10T20:33:00-07:00");
        // USNO computes civil twilight only — it never emits nautical/astro
        // phens, so those four fields are always empty for this source (see
        // `usno_emits_civil_twilight_only`).
        assert!(a.nautical_twilight_begin.is_empty());
        assert!(a.nautical_twilight_end.is_empty());
        assert!(a.astronomical_twilight_begin.is_empty());
        assert!(a.astronomical_twilight_end.is_empty());
        // Moon events.
        assert_eq!(a.moonrise, "2026-06-10T02:01:00-07:00");
        assert_eq!(a.moonset, "2026-06-10T15:22:00-07:00");
        assert_eq!(a.moon_phase, "Waning Crescent", "curphase → moon_phase");
        // Computed golden hour = sunset − 1h.
        assert_eq!(a.golden_hour, "2026-06-10T19:04:00-07:00");
        // day_length not provided by USNO → omitted.
        assert!(a.day_length.is_empty());
        // fracillum + closestphase + isdst ride verbatim in extra; illumination
        // is the normalized 0–1 fraction of fracillum ("25%" → 0.25), matching
        // the contract's worked example key.
        assert_eq!(a.extra.get("fracillum"), Some(&json!("25%")));
        assert_eq!(a.extra.get("illumination"), Some(&json!(0.25)));
        assert_eq!(a.extra.get("closestphase").and_then(|c| c.get("phase")), Some(&json!("Last Quarter")));
        assert_eq!(a.extra.get("isdst"), Some(&json!(false)));
        // The whole row is schema-valid by construction (proven in spec_validation);
        // here, omit-empty drops the absent twilight stages.
        let re = serde_json::to_value(&a).unwrap();
        assert!(re.get("nautical_twilight_begin").is_none());
        assert!(re.get("day_length").is_none());
    }

    #[test]
    fn maps_arctic_midnight_sun_omits_rise_set_keeps_transit_and_moon() {
        // Under the midnight sun the body never rises/sets, so USNO emits no
        // Rise/Set/civil-twilight phens — those fields stay empty, and the
        // computed golden hour (which needs a sunset) is omitted. The sun's
        // **Upper Transit still exists** (the sun's highest point), so solar_noon
        // is populated from the live response. Moon data and date/source make a
        // valid row.
        let a = almanac_from(&data_of(&oneday_arctic()), "2026-06-21", 70.0, -150.0, "-09:00");
        assert!(a.sunrise.is_empty(), "no sunrise under midnight sun");
        assert!(a.sunset.is_empty());
        assert!(a.golden_hour.is_empty(), "no sunset → no computed golden hour");
        assert!(a.civil_twilight_begin.is_empty(), "no civil-twilight phen in a polar day");
        // Upper Transit (13:02) is present even when the sun never sets.
        assert_eq!(a.solar_noon, "2026-06-21T13:02:00-09:00", "Upper Transit → solar noon");
        // Live moondata is [Set 00:42, Rise 13:29, Upper Transit 19:08].
        assert_eq!(a.moonrise, "2026-06-21T13:29:00-09:00");
        assert_eq!(a.moonset, "2026-06-21T00:42:00-09:00");
        assert_eq!(a.moon_phase, "First Quarter");
        // fracillum "50%" → normalized illumination 0.5; raw kept too.
        assert_eq!(a.extra.get("fracillum"), Some(&json!("50%")));
        assert_eq!(a.extra.get("illumination"), Some(&json!(0.5)));
        // Re-serialized form drops the empty rise/set/golden-hour fields.
        let re = serde_json::to_value(&a).unwrap();
        assert!(re.get("sunrise").is_none() && re.get("golden_hour").is_none());
        assert!(re.get("civil_twilight_begin").is_none());
        assert_eq!(re.get("date"), Some(&json!("2026-06-21")));
        assert_eq!(re.get("source"), Some(&json!("usno")));
    }

    // --- a scripted mock API ---------------------------------------------

    /// Records each requested date and replays a canned response per date (or a
    /// default LA day shifted to that date). `errors` forces a per-date error.
    struct MockApi {
        by_date: HashMap<String, Value>,
        errors: HashMap<String, FetchError>,
        requested: RefCell<Vec<String>>,
        default: Value,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi {
                by_date: HashMap::new(),
                errors: HashMap::new(),
                requested: RefCell::new(Vec::new()),
                default: oneday_la(),
            }
        }
    }

    impl UsnoApi for MockApi {
        fn oneday(&self, date: &str, _lat: f64, _lon: f64, _tz: f64) -> Result<Value, FetchError> {
            self.requested.borrow_mut().push(date.to_string());
            if let Some(e) = self.errors.get(date) {
                return Err(match e {
                    FetchError::Api(m) => FetchError::Api(m.clone()),
                    FetchError::Other(m) => FetchError::Other(m.clone()),
                });
            }
            // Default: the LA day with its `date` field rewritten to the query.
            let mut v = self.by_date.get(date).cloned().unwrap_or_else(|| self.default.clone());
            if let Some(d) = v["properties"]["data"].as_object_mut() {
                if let Some(day) = date.rsplit('-').next().and_then(|s| s.parse::<i64>().ok()) {
                    d.insert("day".into(), json!(day));
                }
            }
            Ok(v)
        }
    }

    fn configured_vault(name: &str) -> Vault {
        let v = temp_vault(name);
        def_connect(&v, "34.05,-118.25,Los Angeles").unwrap();
        v
    }

    #[test]
    fn first_pull_writes_today_only_both_layers_and_advances_watermark() {
        let v = configured_vault("firstpull");
        let api = MockApi::new();
        // A fixed "now": June 10, 2026, 09:00 PDT (offset -7).
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();

        let out = pull_with(&v, &api, now).unwrap();
        assert_eq!(out.counts.get("days"), Some(&1), "first sync writes just today");
        assert_eq!(api.requested.borrow().as_slice(), &["2026-06-10"], "only today queried");

        // Contract almanac row, partitioned by the month of `date`.
        let body = std::fs::read_to_string(v.root().join("environment/usno/almanac/2026-06.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 1);
        assert!(body.contains("\"date\":\"2026-06-10\""));
        assert!(body.contains("\"source\":\"usno\""));
        assert!(body.contains("\"moon_phase\":\"Waning Crescent\""));
        // Times anchored to the now-offset (-07:00).
        assert!(body.contains("\"sunrise\":\"2026-06-10T05:41:00-07:00\""), "{body}");
        assert!(body.contains("\"golden_hour\":\"2026-06-10T19:04:00-07:00\""));
        // extra carries the source-specific fracillum.
        assert!(body.contains("\"fracillum\":\"25%\""));

        // Raw layer mirrors the partition, verbatim data object (keeps fields
        // the contract drops, e.g. day_of_week / closestphase).
        let raw = std::fs::read_to_string(v.root().join("environment/usno/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"day_of_week\":\"Wednesday\""), "raw keeps un-mapped fields: {raw}");
        assert!(raw.contains("\"curphase\":\"Waning Crescent\""));

        // Watermark advanced to today; cursor carries the (non-secret) location.
        let state = v.read_usno_sync();
        assert_eq!(state.last_date.as_deref(), Some("2026-06-10"));
        assert_eq!(state.location.as_ref().unwrap().place, "Los Angeles");
        assert!(state.updated.is_some());
    }

    #[test]
    fn second_pull_backfills_gap_and_dedupes_re_run() {
        let v = configured_vault("backfill");
        // Seed: a prior sync left the watermark at June 8.
        let mut st = v.read_usno_sync();
        st.last_date = Some("2026-06-08".into());
        v.write_usno_sync(&st).unwrap();

        let api = MockApi::new();
        let now = Local.with_ymd_and_hms(2026, 6, 11, 9, 0, 0).single().unwrap();
        let out = pull_with(&v, &api, now).unwrap();
        // Days 9, 10, 11 (the day after the watermark through today).
        assert_eq!(out.counts.get("days"), Some(&3));
        assert_eq!(api.requested.borrow().as_slice(), &["2026-06-09", "2026-06-10", "2026-06-11"]);
        let body = std::fs::read_to_string(v.root().join("environment/usno/almanac/2026-06.jsonl")).unwrap();
        assert_eq!(body.lines().count(), 3);
        assert_eq!(v.read_usno_sync().last_date.as_deref(), Some("2026-06-11"));

        // Re-run with the watermark reset to before the window → same dates
        // re-queried, but the (date, coords) dedupe writes nothing new and the
        // file stays byte-identical.
        let mut st = v.read_usno_sync();
        st.last_date = Some("2026-06-08".into());
        v.write_usno_sync(&st).unwrap();
        let again = pull_with(&v, &MockApi::new(), now).unwrap();
        assert_eq!(again.counts.get("days"), Some(&0), "every day already stored");
        let body2 = std::fs::read_to_string(v.root().join("environment/usno/almanac/2026-06.jsonl")).unwrap();
        assert_eq!(body, body2, "almanac file byte-identical after dedupe re-run");
    }

    #[test]
    fn backfill_is_capped_so_a_long_gap_cannot_storm_the_api() {
        let v = configured_vault("cap");
        // A watermark a year behind: a naive "day-after-watermark through today"
        // window would be 365 calls. The cap clamps it to the most-recent
        // MAX_BACKFILL_DAYS days (ending today) — the API is never stormed, and
        // the useful recent window still lands. The ancient gap days are dropped
        // (an almanac wants recent geometry, not deep history).
        let mut st = v.read_usno_sync();
        st.last_date = Some("2025-06-11".into());
        v.write_usno_sync(&st).unwrap();
        let api = MockApi::new();
        let now = Local.with_ymd_and_hms(2026, 6, 11, 9, 0, 0).single().unwrap();
        let out = pull_with(&v, &api, now).unwrap();
        // At most the cap many calls/rows — the storm guard.
        assert_eq!(api.requested.borrow().len(), MAX_BACKFILL_DAYS as usize, "call count capped");
        assert_eq!(out.counts.get("days"), Some(&(MAX_BACKFILL_DAYS as u64)));
        // The window ends today, and the oldest day queried is exactly
        // today − (cap − 1) — not the year-old watermark.
        let reqs = api.requested.borrow();
        assert_eq!(reqs.last().unwrap(), "2026-06-11", "window ends today");
        assert_eq!(reqs.first().unwrap(), "2026-05-08", "window starts today − (cap−1), gap dropped");
        assert_eq!(v.read_usno_sync().last_date.as_deref(), Some("2026-06-11"));
    }

    #[test]
    fn per_day_api_error_is_skipped_window_continues() {
        let v = configured_vault("apierr");
        let mut st = v.read_usno_sync();
        st.last_date = Some("2026-06-08".into());
        v.write_usno_sync(&st).unwrap();
        let mut api = MockApi::new();
        // June 10 errors at the API level (e.g. a bad date) — skip it, keep 9+11.
        api.errors.insert("2026-06-10".into(), FetchError::Api("bad date".into()));
        let now = Local.with_ymd_and_hms(2026, 6, 11, 9, 0, 0).single().unwrap();
        let out = pull_with(&v, &api, now).unwrap();
        assert_eq!(out.counts.get("days"), Some(&2), "9 and 11 written, 10 skipped");
        let body = std::fs::read_to_string(v.root().join("environment/usno/almanac/2026-06.jsonl")).unwrap();
        assert!(!body.contains("\"date\":\"2026-06-10\""), "the errored day is absent");
        // Watermark reached the newest written day (11), even though 10 was a gap.
        assert_eq!(v.read_usno_sync().last_date.as_deref(), Some("2026-06-11"));
    }

    #[test]
    fn transport_error_on_first_day_surfaces_without_advancing() {
        let v = configured_vault("neterr");
        let mut api = MockApi::new();
        api.errors.insert("2026-06-10".into(), FetchError::Other("connection reset".into()));
        let now = Local.with_ymd_and_hms(2026, 6, 10, 9, 0, 0).single().unwrap();
        let err = pull_with(&v, &api, now).unwrap_err().to_string();
        assert!(err.contains("USNO oneday fetch failed"), "clear error: {err}");
        // Nothing written, watermark unset → the next sync re-fetches.
        assert!(v.read_usno_sync().last_date.is_none(), "no partial advance on transport error");
        assert!(!v.root().join("environment/usno/almanac").exists());
    }

    #[test]
    fn unconfigured_pull_skips_with_clear_error() {
        let v = temp_vault("noloc"); // no location set
        let err = pull_with(&v, &MockApi::new(), Local::now()).unwrap_err().to_string();
        assert!(err.contains("no location set"), "clear, no panic: {err}");
    }

    // --- connection tests -------------------------------------------------

    #[test]
    fn connect_stores_location_as_plain_config_not_a_secret() {
        let v = temp_vault("conn");
        def_connect(&v, "34.05,-118.25,Los Angeles").unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Los Angeles");
        assert_eq!(status.accounts[0].key, "usno");

        // The coordinate lives in the PLAIN cursor (rebuildable config), and the
        // secret store was never created — nothing here is sensitive.
        let cursor = std::fs::read_to_string(v.root().join(".trove/usno-sync.json")).unwrap();
        assert!(cursor.contains("34.05"));
        assert!(!v.root().join(".trove/sync").exists(), "no secret store for a keyless config source");

        // A coordinate with no label falls back to a formatted lat/lon label.
        def_connect(&v, "40.71,-74.01").unwrap();
        assert_eq!(def_status(&v).unwrap().accounts[0].label, "40.7100, -74.0100");

        // Disconnect forgets the location (and the watermark); synced data stays.
        def_disconnect(&v, "usno").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
        assert!(v.read_usno_sync().location.is_none());
    }

    #[test]
    fn empty_location_rejected_and_pull_needs_it() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("no location"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "usno");
        assert_eq!(DEF.connection, Some("usno"));
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor deserializes to all-None (a fresh, unconfigured state).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.location.is_none() && empty.last_date.is_none());
        // An older cursor that carried only a location (no watermark yet) still
        // loads — additive evolution, prove old lines deserialize.
        let partial: SyncState =
            serde_json::from_str(r#"{"location":{"lat":34.05,"lon":-118.25}}"#).unwrap();
        assert_eq!(partial.location.as_ref().unwrap().lat, 34.05);
        assert!(partial.location.as_ref().unwrap().place.is_empty());
        assert!(partial.last_date.is_none());
    }
}
