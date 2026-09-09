//! NASA FIRMS wildfire / active-fire detection.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/nasa-firms.md
//!
//! A **Periodic** (daily) cloud pull: active-fire detections from NASA's
//! Fire Information for Resource Management System. Queries the FIRMS area
//! CSV endpoint for a bounding box around the user's location. Writes the
//! **`environment`** domain's `EnvGeoEvent` shape:
//!
//! - **contract layer** — `environment/nasa-firms/events/YYYY-MM.jsonl` —
//!   one [`crate::environment::EnvGeoEvent`] per detection, `event_type =
//!   "fire"`, deduped by a stable hash of (satellite, lat, lon, acq_date,
//!   acq_time).
//! - **raw layer** — `environment/nasa-firms/raw/YYYY-MM.jsonl` —
//!   the parsed CSV row objects at full fidelity (every field FIRMS returns),
//!   UNCONDITIONAL.
//!
//! **Auth:** a free MAP_KEY obtained from one-time email signup at
//! firms.modaps.eosdis.nasa.gov/api/map_key/. Pasted via the connect card
//! ([`ConnectMethod::TokenPaste`]), stored in the 0600 secret store.
//!
//! **Sensor:** VIIRS_SNPP_NRT (375 m, preferred) only. MODIS is out of scope
//! for this collector.
//!
//! **Location:** requires a configured location (lat/lon bounding box).
//! Reuses the vault's weather location (or CoreLocation) to build a ±1° box.
//! Inert when no location is set.
//!
//! **Cursor / watermark:** a non-secret cursor at `.trove/nasa-firms-sync.json`
//! tracks the most recent `acq_date` written (YYYY-MM-DD). Each pull fetches
//! `DAY_RANGE=1` for *today* (the FIRMS NRT window); on a first sync or a
//! stale watermark it fetches the last [`MAX_FETCH_DAYS`] days. The cursor
//! advances *after* writing, so a crash re-fetches.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvGeoEvent;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Contract-layer events dir; raw dir one level deeper.
const EVENTS_DIR: &str = "environment/nasa-firms/events";
const RAW_DIR: &str = "environment/nasa-firms/raw";
/// Non-secret rebuildable cursor — last date written, last location used.
const SYNC_FILE: &str = ".trove/nasa-firms-sync.json";
/// The secret store service id under `.trove/sync/`.
const SERVICE: &str = "nasa-firms";

const SOURCE: &str = "nasa-firms";
/// Default VIIRS sensor: 375 m near-real-time.
const SENSOR: &str = "VIIRS_SNPP_NRT";
const API_BASE: &str = "https://firms.modaps.eosdis.nasa.gov";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Bounding box half-width in degrees (±1° ≈ ±111 km at the equator).
const BBOX_DELTA: f64 = 1.0;
/// Days to request on a first sync or large gap. The FIRMS /area/csv endpoint
/// documents DAY_RANGE as "1 .. 5 — number of days to query at one time"
/// (the 10-day option applies only to different endpoint variants). Capped at 5.
const MAX_FETCH_DAYS: u32 = 5;
/// Daily cadence; the NRT layer refreshes every few hours, daily is enough
/// for a personal ambient-fire record.
pub const NASA_FIRMS_SYNC_SECS: u64 = 24 * 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_nasa_firms_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join(EVENTS_DIR)))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        required: false, // manual location also works
    }
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("detections").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("nasa-firms synced — {n} fire detections")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "nasa-firms sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("detections").copied().unwrap_or(0);
    let headline = if n == 0 {
        "NASA FIRMS: nothing new (no active fire detections in your area)".to_string()
    } else {
        format!("NASA FIRMS synced — {n} fire detections")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "nasa-firms",
        name: "NASA FIRMS Wildfire",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls active-fire detections from NASA's FIRMS satellite system \
                      (VIIRS 375 m resolution) for a radius around your location. \
                      Updates several times daily as satellites pass overhead.",
        domain: "environment",
        vault_path: "environment/nasa-firms/",
        toggleable: true,
        setup: &[
            "Register for a free MAP_KEY at firms.modaps.eosdis.nasa.gov/api/map_key/ \
             (one-time email signup).",
            "Paste the key in the connect card.",
            "Approve Location Services when asked, or set a location manually on the \
             Weather tab; without a location this stays inert.",
        ],
        caveats: "Requires a free MAP_KEY from NASA FIRMS (email signup). Inert without \
                  a location. VIIRS SNPP NRT only (375 m); rate-limited to 5,000 \
                  transactions per 10 minutes (trivially fine for a daily pull).",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::daily(NASA_FIRMS_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: Some("nasa-firms"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — the free MAP_KEY).

fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("MAP_KEY must not be empty");
    }
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
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "NASA FIRMS MAP_KEY (configured)".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    let configured = !accounts.is_empty();
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// MAP_KEY obtained from the free NASA FIRMS signup.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "nasa-firms",
    display_name: "NASA FIRMS Wildfire",
    methods: &[ConnectMethod::TokenPaste {
        label: "NASA FIRMS MAP_KEY",
        help: "Free key from one-time email signup at firms.modaps.eosdis.nasa.gov/api/map_key/ — \
               register, verify your email, and your MAP_KEY appears on the confirmation page.",
        placeholder: "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["nasa-firms"],
    setup: &[
        "Register at firms.modaps.eosdis.nasa.gov/api/map_key/ (free, email-only signup).",
        "Verify your email; your MAP_KEY is shown on the confirmation page.",
        "Paste it here and connect.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state. Losing it costs nothing — the next pass re-derives the
/// location and the upsert keeps re-polling idempotent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NasaFirmsSyncState {
    /// RFC3339 local time of the last successful pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Most recent acq_date written, as `YYYY-MM-DD`. The next pull fetches
    /// from this date onward.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_date: String,
    /// Last-used latitude (0.0 = unset).
    #[serde(default)]
    pub lat: f64,
    /// Last-used longitude (0.0 = unset).
    #[serde(default)]
    pub lon: f64,
    /// Non-empty while stuck (no key, no location, network error) — for the UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_nasa_firms_sync(&self) -> Option<NasaFirmsSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_nasa_firms_sync(&self, state: &NasaFirmsSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// CSV parsing — pure, fixture-tested.
//
// VIIRS_SNPP_NRT CSV header (from the official FIRMS attribute table):
//   latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,
//   instrument,confidence,version,bright_ti5,frp,daynight
//
// Field units:
//   latitude / longitude : decimal degrees
//   bright_ti4 / bright_ti5 : Kelvin (I-4 / I-5 channel brightness temp)
//   scan / track          : m (pixel dimensions at 375 m)
//   acq_date              : YYYY-MM-DD
//   acq_time              : HHMM (UTC, zero-padded to 4 digits)
//   satellite             : "N" (Suomi-NPP), "N20", "N21"
//   instrument            : "VIIRS"
//   confidence            : "low" | "nominal" | "high"
//   version               : e.g. "2.0NRT"
//   bright_ti5            : Kelvin
//   frp                   : Megawatts
//   daynight              : "D" | "N"

/// One parsed VIIRS detection row.
#[derive(Debug, Clone)]
struct DetectionRow {
    latitude: f64,
    longitude: f64,
    bright_ti4: Option<f64>,
    scan: Option<f64>,
    track: Option<f64>,
    acq_date: String,  // YYYY-MM-DD
    acq_time: String,  // HHMM (4 digits, UTC)
    satellite: String,
    instrument: String,
    confidence: String,
    version: String,
    bright_ti5: Option<f64>,
    frp: Option<f64>,
    daynight: String,
    /// Any columns beyond the known 14 — preserved verbatim for full-fidelity
    /// raw output. Key = column name, Value = raw string from the CSV cell.
    extra_raw: Vec<(String, String)>,
}

/// The set of known VIIRS column names (lowercase). Any column not in this
/// set is captured verbatim in `DetectionRow::extra_raw` for full fidelity.
const KNOWN_COLS: &[&str] = &[
    "latitude", "longitude", "bright_ti4", "scan", "track", "acq_date",
    "acq_time", "satellite", "instrument", "confidence", "version",
    "bright_ti5", "frp", "daynight",
];

/// Parse a FIRMS CSV response (header + data rows). Known columns are parsed
/// into typed fields; unknown columns are preserved verbatim in `extra_raw`
/// for full-fidelity raw output. Empty or header-only → empty vec.
fn parse_csv(body: &str) -> Vec<DetectionRow> {
    let mut lines = body.lines();
    let header_line = match lines.next() {
        Some(h) => h.trim(),
        None => return Vec::new(),
    };
    // If the response was empty or contained only whitespace.
    if header_line.is_empty() {
        return Vec::new();
    }
    // Build column-name → index map from the header.
    let cols: Vec<&str> = header_line.split(',').map(|s| s.trim()).collect();
    let idx = |name: &str| cols.iter().position(|&c| c.eq_ignore_ascii_case(name));
    let get = |row: &[&str], name: &str| -> String {
        idx(name)
            .and_then(|i| row.get(i))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let get_f64 = |row: &[&str], name: &str| -> Option<f64> {
        let s = get(row, name);
        if s.is_empty() {
            None
        } else {
            s.parse::<f64>().ok()
        }
    };
    // Pre-compute extra (unknown) column indices once per parse.
    let extra_col_indices: Vec<(usize, String)> = cols
        .iter()
        .enumerate()
        .filter(|(_, name)| {
            !KNOWN_COLS.iter().any(|k| k.eq_ignore_ascii_case(name))
        })
        .map(|(i, name)| (i, name.to_string()))
        .collect();

    let mut out = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let latitude = match get_f64(&fields, "latitude") {
            Some(v) => v,
            None => continue, // lat is required
        };
        let longitude = match get_f64(&fields, "longitude") {
            Some(v) => v,
            None => continue, // lon is required
        };
        let acq_date = get(&fields, "acq_date");
        if acq_date.is_empty() {
            continue; // can't ts-partition without a date
        }
        let acq_time = get(&fields, "acq_time");
        let satellite = get(&fields, "satellite");
        // Capture any unknown/future columns verbatim.
        let extra_raw: Vec<(String, String)> = extra_col_indices
            .iter()
            .map(|(i, name)| {
                let val = fields.get(*i).map(|v| v.trim().to_string()).unwrap_or_default();
                (name.clone(), val)
            })
            .collect();
        out.push(DetectionRow {
            latitude,
            longitude,
            bright_ti4: get_f64(&fields, "bright_ti4"),
            scan: get_f64(&fields, "scan"),
            track: get_f64(&fields, "track"),
            acq_date,
            acq_time: acq_time.clone(),
            satellite: satellite.clone(),
            instrument: get(&fields, "instrument"),
            confidence: get(&fields, "confidence"),
            version: get(&fields, "version"),
            bright_ti5: get_f64(&fields, "bright_ti5"),
            frp: get_f64(&fields, "frp"),
            daynight: get(&fields, "daynight"),
            extra_raw,
        });
    }
    out
}

/// Build a stable guid for a detection. Uses satellite + lat + lon + acq_date
/// + acq_time as the natural key (no server-assigned id).
fn detection_guid(row: &DetectionRow) -> String {
    // Format: "nasa-firms:<satellite>:<lat6>:<lon6>:<acq_date>:<acq_time>"
    // Six decimal places ≈ 0.11 m — more than enough to uniquely identify a
    // 375 m pixel. acq_time is already HHMM (UTC).
    format!(
        "nasa-firms:{sat}:{lat:.6}:{lon:.6}:{date}:{time}",
        sat = row.satellite,
        lat = row.latitude,
        lon = row.longitude,
        date = row.acq_date,
        time = row.acq_time
    )
}

/// Build an RFC3339 UTC timestamp from `acq_date` (YYYY-MM-DD) and
/// `acq_time` (HHMM, 4-digit UTC). Returns the best-effort string: if
/// `acq_time` is missing or unparseable the timestamp is midnight UTC on
/// `acq_date`.
fn acq_ts(acq_date: &str, acq_time: &str) -> String {
    // acq_time is HHMM: "0130" → hour=1, min=30.
    let (hour, min) = if acq_time.len() == 4 {
        let h: u32 = acq_time[..2].parse().unwrap_or(0);
        let m: u32 = acq_time[2..].parse().unwrap_or(0);
        (h.min(23), m.min(59))
    } else {
        (0, 0)
    };
    // Parse the date string and compose a full UTC datetime.
    if let Ok(d) = NaiveDate::parse_from_str(acq_date, "%Y-%m-%d") {
        if let Some(dt) = d.and_hms_opt(hour, min, 0) {
            let utc = DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc);
            return utc.to_rfc3339();
        }
    }
    // Fallback: just the date at midnight.
    format!("{acq_date}T00:00:00+00:00")
}

/// Build an [`EnvGeoEvent`] from a parsed detection row.
fn row_to_event(row: &DetectionRow) -> EnvGeoEvent {
    let guid = detection_guid(row);
    let ts = acq_ts(&row.acq_date, &row.acq_time);
    let place = format!(
        "{:.4}°, {:.4}° ({} {})",
        row.latitude,
        row.longitude,
        row.satellite,
        row.daynight
    );
    // Extra: all source-specific fields the normalized columns don't carry.
    let mut extra = Map::new();
    if let Some(v) = row.bright_ti4 {
        extra.insert("bright_ti4_k".into(), Value::from(v));
    }
    if let Some(v) = row.bright_ti5 {
        extra.insert("bright_ti5_k".into(), Value::from(v));
    }
    if let Some(v) = row.frp {
        extra.insert("frp_mw".into(), Value::from(v));
    }
    if let Some(v) = row.scan {
        extra.insert("scan_m".into(), Value::from(v));
    }
    if let Some(v) = row.track {
        extra.insert("track_m".into(), Value::from(v));
    }
    if !row.confidence.is_empty() {
        extra.insert("confidence".into(), Value::from(row.confidence.clone()));
    }
    if !row.instrument.is_empty() {
        extra.insert("instrument".into(), Value::from(row.instrument.clone()));
    }
    if !row.version.is_empty() {
        extra.insert("version".into(), Value::from(row.version.clone()));
    }
    if !row.daynight.is_empty() {
        extra.insert("daynight".into(), Value::from(row.daynight.clone()));
    }
    if !row.acq_time.is_empty() {
        extra.insert("acq_time_utc".into(), Value::from(row.acq_time.clone()));
    }
    if !row.satellite.is_empty() {
        extra.insert("satellite".into(), Value::from(row.satellite.clone()));
    }
    // FRP (fire radiative power) as the magnitude proxy — represents fire
    // intensity in Megawatts.
    let magnitude = row.frp;
    EnvGeoEvent {
        ts,
        source: SOURCE.into(),
        guid,
        event_type: "fire".into(),
        magnitude,
        place,
        lat: Some(row.latitude),
        lon: Some(row.longitude),
        severity: String::new(),
        headline: format!(
            "Active fire — {sensor} ({conf} confidence, {dn})",
            sensor = SENSOR,
            conf = if row.confidence.is_empty() { "unknown" } else { &row.confidence },
            dn = match row.daynight.as_str() {
                "D" => "daytime",
                "N" => "nighttime",
                _ => "unknown time",
            }
        ),
        url: String::new(),
        expires: String::new(),
        extra,
    }
}

/// Build the raw JSON object for a detection row (full fidelity).
/// All 14 known fields are emitted; any unknown/future columns captured in
/// `extra_raw` are appended verbatim so nothing FIRMS returns is ever dropped.
fn row_to_raw(row: &DetectionRow) -> Value {
    let mut m = Map::new();
    m.insert("latitude".into(), Value::from(row.latitude));
    m.insert("longitude".into(), Value::from(row.longitude));
    if let Some(v) = row.bright_ti4 {
        m.insert("bright_ti4".into(), Value::from(v));
    }
    if let Some(v) = row.scan {
        m.insert("scan".into(), Value::from(v));
    }
    if let Some(v) = row.track {
        m.insert("track".into(), Value::from(v));
    }
    m.insert("acq_date".into(), Value::from(row.acq_date.clone()));
    m.insert("acq_time".into(), Value::from(row.acq_time.clone()));
    m.insert("satellite".into(), Value::from(row.satellite.clone()));
    m.insert("instrument".into(), Value::from(row.instrument.clone()));
    m.insert("confidence".into(), Value::from(row.confidence.clone()));
    m.insert("version".into(), Value::from(row.version.clone()));
    if let Some(v) = row.bright_ti5 {
        m.insert("bright_ti5".into(), Value::from(v));
    }
    if let Some(v) = row.frp {
        m.insert("frp".into(), Value::from(v));
    }
    m.insert("daynight".into(), Value::from(row.daynight.clone()));
    // Append any unknown/future columns verbatim (true full-fidelity raw layer).
    for (name, val) in &row.extra_raw {
        m.insert(name.clone(), Value::from(val.clone()));
    }
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// Upsert helpers (same pattern as nws.rs).

/// Upsert geo-events into `environment/nasa-firms/events/YYYY-MM.jsonl` by
/// `guid`. Detections for the same satellite pass may re-appear on a re-pull
/// (the NRT layer updates; a detection may shift slightly or be revised).
fn upsert_events(vault: &Vault, rows: &[EnvGeoEvent]) -> Result<u64> {
    let stream = vault.stream(EVENTS_DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvGeoEvent>> = Default::default();
    for r in rows {
        let key = Partition::Month
            .key(&r.ts)
            .with_context(|| {
                format!(
                    "nasa-firms event {} has unpartitionable ts {:?}",
                    r.guid, r.ts
                )
            })?
            .to_string();
        by_key.entry(key).or_default().push(r);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<EnvGeoEvent> = stream.read(&key)?;
        for row in incoming {
            match existing.iter_mut().find(|e| e.guid == row.guid) {
                Some(slot) => *slot = row.clone(),
                None => existing.push(row.clone()),
            }
            written += 1;
        }
        vault.write_snapshot(&format!("{EVENTS_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(written)
}

/// Upsert raw objects into `environment/nasa-firms/raw/YYYY-MM.jsonl` by
/// `guid` (read from the object's constructed key).
fn upsert_raw(vault: &Vault, lines: &[(String, String, Value)]) -> Result<()> {
    // Each tuple: (ts, guid, value).
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: BTreeMap<String, Vec<(String, Value)>> = Default::default();
    for (ts, guid, value) in lines {
        let key = Partition::Month
            .key(ts)
            .with_context(|| {
                format!("nasa-firms raw {guid} has unpartitionable ts {ts:?}")
            })?
            .to_string();
        by_key.entry(key).or_default().push((guid.clone(), value.clone()));
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<Value> = stream.read(&key)?;
        for (guid, value) in incoming {
            // Find by the guid field inside the raw JSON (we stamped it).
            let pos = existing.iter().position(|v| {
                v.get("_guid").and_then(Value::as_str).unwrap_or("") == guid
            });
            // Build a value with a synthetic `_guid` key for dedupe; the rest
            // is the original verbatim row.
            let mut obj = match value {
                Value::Object(m) => m,
                _ => Map::new(),
            };
            obj.insert("_guid".into(), Value::from(guid));
            let wrapped = Value::Object(obj);
            match pos {
                Some(i) => existing[i] = wrapped,
                None => existing.push(wrapped),
            }
        }
        vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

pub(crate) trait FirmsApi {
    /// GET the area CSV: MAP_KEY, sensor, BBOX (w,s,e,n), day_range,
    /// optional start_date (YYYY-MM-DD). Returns the raw CSV body.
    fn area_csv(
        &self,
        map_key: &str,
        sensor: &str,
        bbox: &str,
        day_range: u32,
        date: Option<&str>,
    ) -> Result<String>;
}

struct FirmsClient {
    base: String,
}

impl FirmsClient {
    fn new(base: String) -> Self {
        FirmsClient { base }
    }
}

impl FirmsApi for FirmsClient {
    fn area_csv(
        &self,
        map_key: &str,
        sensor: &str,
        bbox: &str,
        day_range: u32,
        date: Option<&str>,
    ) -> Result<String> {
        let url = if let Some(d) = date {
            format!(
                "{}/api/area/csv/{}/{}/{}/{}/{}",
                self.base, map_key, sensor, bbox, day_range, d
            )
        } else {
            format!(
                "{}/api/area/csv/{}/{}/{}/{}",
                self.base, map_key, sensor, bbox, day_range
            )
        };
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("requesting NASA FIRMS area CSV")?
            .into_string()
            .context("reading NASA FIRMS CSV response")
    }
}

// ---------------------------------------------------------------------------
// Location resolution.

/// Bounding box string for the FIRMS API: "west,south,east,north".
fn bbox(lat: f64, lon: f64) -> String {
    let west = lon - BBOX_DELTA;
    let east = lon + BBOX_DELTA;
    let south = lat - BBOX_DELTA;
    let north = lat + BBOX_DELTA;
    // Clamp to valid ranges.
    let west = west.max(-180.0);
    let east = east.min(180.0);
    let south = south.max(-90.0);
    let north = north.min(90.0);
    format!("{west:.4},{south:.4},{east:.4},{north:.4}")
}

/// CoreLocation → manual weather location → cursor fallback. `None` = inert.
fn resolve_point(vault: &Vault, state: &NasaFirmsSyncState) -> Option<(f64, f64)> {
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
// The pull.

/// Pull with the production client and production API base.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_nasa_firms_sync().unwrap_or_default();
    // Inert if no key.
    let map_key = match vault.load_sync_token(SERVICE)? {
        Some(t) => t.access_token,
        None => {
            return Ok(PullOutcome {
                headline: "NASA FIRMS: not connected (paste your MAP_KEY to connect)".into(),
                counts: BTreeMap::from([("detections", 0)]),
            });
        }
    };
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut s = state;
        s.updated = Local::now().to_rfc3339();
        s.error = "no location: grant Location Services or set a location on the Weather tab"
            .into();
        vault.write_nasa_firms_sync(&s)?;
        return Ok(PullOutcome {
            headline: "NASA FIRMS: no location set".into(),
            counts: BTreeMap::from([("detections", 0)]),
        });
    }
    let client = FirmsClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &map_key, &client)
}

/// The network+write body over an injected API + explicit point. `None` point
/// → clean inert no-op. This is the offline test seam.
pub(crate) fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    map_key: &str,
    client: &impl FirmsApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "NASA FIRMS: no location set".into(),
            counts: BTreeMap::from([("detections", 0)]),
        });
    };

    let bbox_str = bbox(lat, lon);
    let now = Local::now();
    let mut state = vault.read_nasa_firms_sync().unwrap_or_default();

    // Determine fetch window. FIRMS /area/csv DAY_RANGE is 1–5 (area endpoint
    // cap). Without start_date the server counts back from today inclusively,
    // so day_range=1 means today only, day_range=5 means today and the 4 days
    // before it.  With start_date the window is [start_date .. start_date +
    // day_range - 1]; to reach today we must set day_range = days_since + 1.
    //
    // We always omit start_date and let the server anchor on today, so:
    //   - first sync / large gap → MAX_FETCH_DAYS (no start_date)
    //   - same day → 1
    //   - gap of N days → (N + 1).min(MAX_FETCH_DAYS), no start_date
    //     (the +1 ensures today is included; min cap keeps us within 1–5).
    let today_str = now.format("%Y-%m-%d").to_string();
    let (day_range, start_date): (u32, Option<String>) = if state.last_date.is_empty() {
        // First sync: fetch last MAX_FETCH_DAYS days (server counts back from today).
        (MAX_FETCH_DAYS, None)
    } else {
        // Compute days elapsed since last_date (last_date → today exclusive).
        let days_elapsed = NaiveDate::parse_from_str(&state.last_date, "%Y-%m-%d")
            .ok()
            .and_then(|last| {
                NaiveDate::parse_from_str(&today_str, "%Y-%m-%d")
                    .ok()
                    .map(|today| (today - last).num_days())
            })
            .unwrap_or(MAX_FETCH_DAYS as i64);
        if days_elapsed <= 0 {
            // Same day — re-fetch today (upsert handles dedup).
            (1, None)
        } else {
            // Gap of N days: request (N+1) to include both last_date and today.
            // Omit start_date so the server anchors on today inclusively.
            // Cap at MAX_FETCH_DAYS (area endpoint max = 5).
            let needed = (days_elapsed + 1).min(MAX_FETCH_DAYS as i64) as u32;
            (needed, None)
        }
    };

    let csv_body = client.area_csv(map_key, SENSOR, &bbox_str, day_range, start_date.as_deref())?;

    let rows = parse_csv(&csv_body);
    let n = rows.len() as u64;

    if !rows.is_empty() {
        // Contract layer: geo-events.
        let events: Vec<EnvGeoEvent> = rows.iter().map(row_to_event).collect();
        let written = upsert_events(vault, &events)?;

        // Raw layer (unconditional).
        let raw_lines: Vec<(String, String, Value)> = rows
            .iter()
            .map(|r| {
                let ts = acq_ts(&r.acq_date, &r.acq_time);
                let guid = detection_guid(r);
                let raw = row_to_raw(r);
                (ts, guid, raw)
            })
            .collect();
        upsert_raw(vault, &raw_lines)?;

        // Advance cursor watermark to the most recent acq_date written.
        let max_date = rows
            .iter()
            .filter(|r| !r.acq_date.is_empty())
            .map(|r| r.acq_date.as_str())
            .max()
            .unwrap_or(today_str.as_str())
            .to_string();
        state.last_date = max_date;
        let _ = written; // count available if needed
    }

    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.error = String::new();
    vault.write_nasa_firms_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("NASA FIRMS synced — {n} fire detections"),
        counts: BTreeMap::from([("detections", n)]),
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
            .join(format!("trove-nasa-firms-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -------------------------------------------------------------------------
    // Fixture CSV bodies (modeled on the documented FIRMS VIIRS field names).

    /// A populated VIIRS_SNPP_NRT CSV with two detection rows.
    fn viirs_csv_two_detections() -> String {
        [
            "latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,instrument,confidence,version,bright_ti5,frp,daynight",
            "34.1201,-119.2348,335.2,375,375,2026-06-10,1430,N,VIIRS,nominal,2.0NRT,296.8,12.5,D",
            "34.0891,-119.1954,342.7,375,375,2026-06-10,1432,N,VIIRS,high,2.0NRT,301.1,18.3,D",
        ].join("\n")
    }

    /// An empty VIIRS response — no detections in the bounding box.
    fn viirs_csv_empty() -> String {
        "latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,instrument,confidence,version,bright_ti5,frp,daynight\n".to_string()
    }

    /// A CSV with one detection plus an unknown future column (forward-compat).
    fn viirs_csv_extra_column() -> String {
        [
            "latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,instrument,confidence,version,bright_ti5,frp,daynight,future_field",
            "34.1201,-119.2348,335.2,375,375,2026-06-10,1430,N,VIIRS,nominal,2.0NRT,296.8,12.5,D,ignored_value",
        ].join("\n")
    }

    /// Night detection with low confidence.
    fn viirs_csv_night() -> String {
        [
            "latitude,longitude,bright_ti4,scan,track,acq_date,acq_time,satellite,instrument,confidence,version,bright_ti5,frp,daynight",
            "34.1201,-119.2348,315.4,375,375,2026-06-11,0220,N20,VIIRS,low,2.0NRT,289.3,4.1,N",
        ].join("\n")
    }

    // -------------------------------------------------------------------------
    // Stub API.

    struct Stub {
        csv: String,
    }

    impl FirmsApi for Stub {
        fn area_csv(
            &self,
            _map_key: &str,
            _sensor: &str,
            _bbox: &str,
            _day_range: u32,
            _date: Option<&str>,
        ) -> Result<String> {
            Ok(self.csv.clone())
        }
    }

    /// Spy stub: records the day_range and start_date from the most recent call.
    struct SpyStub {
        csv: String,
        day_range: std::cell::Cell<u32>,
        start_date: std::cell::RefCell<Option<String>>,
    }

    impl SpyStub {
        fn new(csv: String) -> Self {
            SpyStub {
                csv,
                day_range: std::cell::Cell::new(0),
                start_date: std::cell::RefCell::new(None),
            }
        }
    }

    impl FirmsApi for SpyStub {
        fn area_csv(
            &self,
            _map_key: &str,
            _sensor: &str,
            _bbox: &str,
            day_range: u32,
            date: Option<&str>,
        ) -> Result<String> {
            self.day_range.set(day_range);
            *self.start_date.borrow_mut() = date.map(|s| s.to_string());
            Ok(self.csv.clone())
        }
    }

    const SEED: (f64, f64) = (34.05, -119.20);
    const KEY: &str = "test-map-key";

    // -------------------------------------------------------------------------
    // Pure parser tests.

    #[test]
    fn parse_csv_extracts_all_viirs_fields() {
        let rows = parse_csv(&viirs_csv_two_detections());
        assert_eq!(rows.len(), 2, "two data rows parsed");

        let r = &rows[0];
        assert_eq!(r.latitude, 34.1201);
        assert_eq!(r.longitude, -119.2348);
        assert_eq!(r.bright_ti4, Some(335.2));
        assert_eq!(r.scan, Some(375.0));
        assert_eq!(r.track, Some(375.0));
        assert_eq!(r.acq_date, "2026-06-10");
        assert_eq!(r.acq_time, "1430");
        assert_eq!(r.satellite, "N");
        assert_eq!(r.instrument, "VIIRS");
        assert_eq!(r.confidence, "nominal");
        assert_eq!(r.version, "2.0NRT");
        assert_eq!(r.bright_ti5, Some(296.8));
        assert_eq!(r.frp, Some(12.5));
        assert_eq!(r.daynight, "D");

        let r2 = &rows[1];
        assert_eq!(r2.confidence, "high");
        assert_eq!(r2.frp, Some(18.3));
    }

    #[test]
    fn parse_csv_empty_returns_empty_vec() {
        assert!(parse_csv(&viirs_csv_empty()).is_empty());
        assert!(parse_csv("").is_empty());
        assert!(parse_csv("  \n  \n").is_empty());
    }

    #[test]
    fn parse_csv_preserves_unknown_future_column_in_extra_raw() {
        let rows = parse_csv(&viirs_csv_extra_column());
        assert_eq!(rows.len(), 1, "row is parsed");
        assert_eq!(rows[0].confidence, "nominal");
        // Unknown column is captured verbatim in extra_raw (not dropped).
        let extra: std::collections::HashMap<_, _> =
            rows[0].extra_raw.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            extra.get("future_field"),
            Some(&"ignored_value"),
            "unknown column preserved in extra_raw"
        );
    }

    #[test]
    fn row_to_raw_includes_unknown_future_column() {
        let rows = parse_csv(&viirs_csv_extra_column());
        let raw = row_to_raw(&rows[0]);
        // Known fields still present.
        assert_eq!(raw.get("latitude"), Some(&json!(34.1201)));
        // Unknown field is also present in the raw layer.
        assert_eq!(
            raw.get("future_field"),
            Some(&json!("ignored_value")),
            "future_field appears in raw output"
        );
    }

    #[test]
    fn detection_guid_is_stable_and_unique() {
        let rows = parse_csv(&viirs_csv_two_detections());
        let g0 = detection_guid(&rows[0]);
        let g1 = detection_guid(&rows[1]);
        assert_ne!(g0, g1, "different lat/lon → different guid");
        // Re-running on the same row produces the same guid.
        assert_eq!(g0, detection_guid(&rows[0]), "guid is deterministic");
        // Should not contain whitespace.
        assert!(!g0.contains(' '));
        assert!(g0.starts_with("nasa-firms:"));
    }

    #[test]
    fn acq_ts_parses_hhmm_and_date() {
        // "1430" UTC on 2026-06-10 → "2026-06-10T14:30:00+00:00"
        let ts = acq_ts("2026-06-10", "1430");
        assert!(ts.starts_with("2026-06-10T14:30:00"), "ts={ts}");
        // Midnight fallback when time is missing.
        let ts_none = acq_ts("2026-06-10", "");
        assert!(ts_none.contains("2026-06-10"), "ts_none={ts_none}");
    }

    #[test]
    fn row_to_event_maps_contract_fields() {
        let rows = parse_csv(&viirs_csv_two_detections());
        let ev = row_to_event(&rows[0]);
        assert_eq!(ev.source, "nasa-firms");
        assert_eq!(ev.event_type, "fire");
        assert_eq!(ev.lat, Some(34.1201));
        assert_eq!(ev.lon, Some(-119.2348));
        assert_eq!(ev.magnitude, Some(12.5), "magnitude = frp");
        assert!(!ev.guid.is_empty());
        assert!(!ev.ts.is_empty());
        // Extra carries sensor-specific fields.
        assert_eq!(ev.extra.get("confidence"), Some(&json!("nominal")));
        assert_eq!(ev.extra.get("frp_mw"), Some(&json!(12.5)));
        assert_eq!(ev.extra.get("daynight"), Some(&json!("D")));
        assert_eq!(ev.extra.get("satellite"), Some(&json!("N")));
    }

    #[test]
    fn row_to_raw_captures_all_fields() {
        let rows = parse_csv(&viirs_csv_two_detections());
        let raw = row_to_raw(&rows[0]);
        assert_eq!(raw.get("latitude"), Some(&json!(34.1201)));
        assert_eq!(raw.get("longitude"), Some(&json!(-119.2348)));
        assert_eq!(raw.get("bright_ti4"), Some(&json!(335.2)));
        assert_eq!(raw.get("acq_date"), Some(&json!("2026-06-10")));
        assert_eq!(raw.get("acq_time"), Some(&json!("1430")));
        assert_eq!(raw.get("satellite"), Some(&json!("N")));
        assert_eq!(raw.get("frp"), Some(&json!(12.5)));
        assert_eq!(raw.get("daynight"), Some(&json!("D")));
    }

    #[test]
    fn bbox_builds_correct_string() {
        let s = bbox(34.05, -119.20);
        // west,south,east,north
        let parts: Vec<f64> = s.split(',').map(|p| p.parse().unwrap()).collect();
        assert_eq!(parts.len(), 4);
        let (west, south, east, north) = (parts[0], parts[1], parts[2], parts[3]);
        assert!((west - (-120.20)).abs() < 0.001, "west={west}");
        assert!((south - (33.05)).abs() < 0.001, "south={south}");
        assert!((east - (-118.20)).abs() < 0.001, "east={east}");
        assert!((north - (35.05)).abs() < 0.001, "north={north}");
    }

    #[test]
    fn bbox_clamps_at_poles_and_antimeridian() {
        // Near the south pole.
        let s = bbox(-89.5, 0.0);
        let parts: Vec<f64> = s.split(',').map(|p| p.parse().unwrap()).collect();
        let south = parts[1];
        assert!(south >= -90.0, "south clamped: {south}");

        // Near the antimeridian.
        let s2 = bbox(0.0, 179.5);
        let parts2: Vec<f64> = s2.split(',').map(|p| p.parse().unwrap()).collect();
        let east = parts2[2];
        assert!(east <= 180.0, "east clamped: {east}");
    }

    // -------------------------------------------------------------------------
    // Pull / store tests.

    #[test]
    fn pull_writes_events_and_raw_for_two_detections() {
        let v = temp_vault("pull-two");
        v.save_sync_token(SERVICE, &crate::sync::oauth::TokenSet {
            access_token: KEY.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        }).unwrap();
        let stub = Stub { csv: viirs_csv_two_detections() };
        let out = pull_at_with(&v, Some(SEED), KEY, &stub).unwrap();
        assert_eq!(out.counts.get("detections"), Some(&2), "two detections written");

        // Events file.
        let events_path = v.root().join("environment/nasa-firms/events/2026-06.jsonl");
        assert!(events_path.exists(), "events file created");
        let events_body = std::fs::read_to_string(&events_path).unwrap();
        assert_eq!(events_body.lines().count(), 2, "two event rows");
        assert!(events_body.contains("\"event_type\":\"fire\""));
        assert!(events_body.contains("\"source\":\"nasa-firms\""));

        // Raw file.
        let raw_path = v.root().join("environment/nasa-firms/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw file created");
        let raw_body = std::fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw_body.lines().count(), 2, "two raw rows");
        assert!(raw_body.contains("\"bright_ti4\""));
        assert!(raw_body.contains("\"frp\""));

        // Cursor updated.
        let state = v.read_nasa_firms_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.last_date, "2026-06-10");
        assert!(state.error.is_empty());
    }

    #[test]
    fn pull_empty_csv_writes_nothing_updates_cursor() {
        let v = temp_vault("pull-empty");
        v.save_sync_token(SERVICE, &crate::sync::oauth::TokenSet {
            access_token: KEY.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        }).unwrap();
        let stub = Stub { csv: viirs_csv_empty() };
        let out = pull_at_with(&v, Some(SEED), KEY, &stub).unwrap();
        assert_eq!(out.counts.get("detections"), Some(&0));
        // No events file.
        assert!(!v.root().join("environment/nasa-firms/events").exists());
        // No raw file.
        assert!(!v.root().join("environment/nasa-firms/raw").exists());
        // Cursor still updated (to mark the poll ran).
        let state = v.read_nasa_firms_sync().unwrap();
        assert!(!state.updated.is_empty());
    }

    #[test]
    fn pull_deduplicates_same_detection_on_repoll() {
        let v = temp_vault("dedup");
        v.save_sync_token(SERVICE, &crate::sync::oauth::TokenSet {
            access_token: KEY.to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        }).unwrap();
        let stub = Stub { csv: viirs_csv_two_detections() };
        // First poll.
        pull_at_with(&v, Some(SEED), KEY, &stub).unwrap();
        let count1 = std::fs::read_to_string(
            v.root().join("environment/nasa-firms/events/2026-06.jsonl"),
        )
        .unwrap()
        .lines()
        .count();
        assert_eq!(count1, 2);

        // Re-poll: same CSV → same guids → upsert, no duplicate.
        pull_at_with(&v, Some(SEED), KEY, &stub).unwrap();
        let count2 = std::fs::read_to_string(
            v.root().join("environment/nasa-firms/events/2026-06.jsonl"),
        )
        .unwrap()
        .lines()
        .count();
        assert_eq!(count2, 2, "re-poll upserts, never duplicates");
    }

    #[test]
    fn pull_none_point_is_inert() {
        let v = temp_vault("inert");
        let stub = Stub { csv: viirs_csv_two_detections() };
        let out = pull_at_with(&v, None, KEY, &stub).unwrap();
        assert_eq!(out.counts.get("detections"), Some(&0));
        assert!(!v.root().join("environment/nasa-firms/events").exists());
    }

    #[test]
    fn night_detection_event_type_and_headline() {
        let rows = parse_csv(&viirs_csv_night());
        assert_eq!(rows.len(), 1);
        let ev = row_to_event(&rows[0]);
        assert_eq!(ev.event_type, "fire");
        assert!(ev.headline.contains("nighttime"), "headline={}", ev.headline);
        assert_eq!(ev.extra.get("daynight"), Some(&json!("N")));
        assert_eq!(ev.extra.get("satellite"), Some(&json!("N20")));
    }

    #[test]
    fn sync_state_round_trips_and_back_compat() {
        // Empty object = brand-new cursor.
        let empty: NasaFirmsSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.lat, 0.0);
        assert!(empty.last_date.is_empty());
        // Round-trip with data.
        let v = temp_vault("sync");
        v.write_nasa_firms_sync(&NasaFirmsSyncState {
            updated: "2026-06-10T14:30:00+00:00".into(),
            last_date: "2026-06-10".into(),
            lat: 34.05,
            lon: -119.20,
            error: String::new(),
        })
        .unwrap();
        let s = v.read_nasa_firms_sync().unwrap();
        assert_eq!(s.last_date, "2026-06-10");
        assert_eq!(s.lat, 34.05);
        assert!(s.error.is_empty());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Back-compat: a minimal geo-event (4 required fields) must parse.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/nasa-firms/events")).unwrap();
        std::fs::write(
            v.root().join("environment/nasa-firms/events/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T14:30:00+00:00\",\"source\":\"nasa-firms\",\"guid\":\"old-1\",\"event_type\":\"fire\"}\n",
        )
        .unwrap();
        let events: Vec<EnvGeoEvent> =
            v.stream(EVENTS_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].guid, "old-1");
        assert_eq!(events[0].event_type, "fire");
    }

    // -------------------------------------------------------------------------
    // Window / day_range correctness tests (defect fixes).

    #[test]
    fn first_sync_uses_max_fetch_days_at_most_5() {
        // First sync (no last_date) must not exceed MAX_FETCH_DAYS=5.
        let v = temp_vault("window-first");
        let spy = SpyStub::new(viirs_csv_empty());
        pull_at_with(&v, Some(SEED), KEY, &spy).unwrap();
        let dr = spy.day_range.get();
        assert!(
            dr <= MAX_FETCH_DAYS,
            "first sync day_range={dr} must be <= MAX_FETCH_DAYS={MAX_FETCH_DAYS}"
        );
        assert!(
            spy.start_date.borrow().is_none(),
            "first sync should not pass start_date (server anchors on today)"
        );
    }

    #[test]
    fn gap_fetch_includes_today_no_start_date() {
        // Simulate a 5-day-old watermark. The window must cover today too.
        // With the fix: day_range = (5+1).min(5) = 5 (no start_date).
        let v = temp_vault("window-gap");
        // Inject a 5-day-old watermark.
        let today = chrono::Local::now().naive_local().date();
        let old_date = today - chrono::Duration::days(5);
        let old_str = old_date.format("%Y-%m-%d").to_string();
        v.write_nasa_firms_sync(&NasaFirmsSyncState {
            last_date: old_str,
            ..Default::default()
        })
        .unwrap();
        let spy = SpyStub::new(viirs_csv_empty());
        pull_at_with(&v, Some(SEED), KEY, &spy).unwrap();
        let dr = spy.day_range.get();
        assert!(
            dr <= MAX_FETCH_DAYS,
            "gap day_range={dr} must not exceed MAX_FETCH_DAYS={MAX_FETCH_DAYS}"
        );
        assert!(
            dr >= 1,
            "gap day_range={dr} must be at least 1"
        );
        // After the fix, start_date must NOT be passed (we let the server anchor on today).
        assert!(
            spy.start_date.borrow().is_none(),
            "gap fetch must not pass start_date; got {:?}",
            spy.start_date.borrow()
        );
    }

    #[test]
    fn same_day_repoll_uses_day_range_1() {
        // If last_date == today, we re-fetch today with day_range=1.
        let v = temp_vault("window-same-day");
        let today_str = chrono::Local::now().format("%Y-%m-%d").to_string();
        v.write_nasa_firms_sync(&NasaFirmsSyncState {
            last_date: today_str,
            ..Default::default()
        })
        .unwrap();
        let spy = SpyStub::new(viirs_csv_empty());
        pull_at_with(&v, Some(SEED), KEY, &spy).unwrap();
        assert_eq!(spy.day_range.get(), 1, "same-day repoll → day_range=1");
        assert!(spy.start_date.borrow().is_none(), "same-day should not pass start_date");
    }

    #[test]
    fn large_gap_capped_at_max_fetch_days() {
        // A 30-day gap must be capped at MAX_FETCH_DAYS (5).
        let v = temp_vault("window-large-gap");
        let today = chrono::Local::now().naive_local().date();
        let old_date = today - chrono::Duration::days(30);
        v.write_nasa_firms_sync(&NasaFirmsSyncState {
            last_date: old_date.format("%Y-%m-%d").to_string(),
            ..Default::default()
        })
        .unwrap();
        let spy = SpyStub::new(viirs_csv_empty());
        pull_at_with(&v, Some(SEED), KEY, &spy).unwrap();
        assert_eq!(
            spy.day_range.get(),
            MAX_FETCH_DAYS,
            "large gap must be capped at MAX_FETCH_DAYS={MAX_FETCH_DAYS}"
        );
        assert!(
            spy.start_date.borrow().is_none(),
            "large gap must not pass start_date"
        );
    }
}
