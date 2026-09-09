//! PurpleAir — hyperlocal PM2.5 from community air-quality sensors.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/purpleair.md
//!
//! A **Periodic** cloud pull: nearby outdoor sensor readings from PurpleAir's
//! community network, polled hourly via the v1 REST API. Writes the
//! **`environment`** domain's scalar reading shape ([`EnvReading`]):
//!
//! - **contract layer** — `environment/purpleair/YYYY-MM.jsonl` — one
//!   [`EnvReading`] per metric per sensor per poll (pm25, temperature,
//!   humidity). Guids encode sensor_index + metric + hour so a re-poll of the
//!   same hour upserts in place.
//! - **raw layer** — `environment/purpleair/raw/YYYY-MM.jsonl` — the full
//!   sensor objects at full fidelity (UNCONDITIONAL).
//!
//! **Auth:** free API Read Key from develop.purpleair.com (Google SSO, no card)
//! pasted via the connect card ([`ConnectMethod::TokenPaste`]), stored in the
//! 0600 secret store under `.trove/sync/purpleair.json`.
//!
//! **Discovery:** bounding-box query (`/v1/sensors?nwlat=…&nwlng=…&selat=…&selng=…`)
//! centred on the user's location (±0.15°, ~16 km), outdoor sensors only
//! (`location_type=0`).
//!
//! **Degrades gracefully:** a location with no nearby sensors returns empty
//! results — no error, collector is inert. No TCC, no local files. Standalone.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvReading;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer reading directory; raw lines in `raw/`.
const DIR: &str = "environment/purpleair";
const RAW_DIR: &str = "environment/purpleair/raw";
/// Non-secret rebuildable cursor.
const SYNC_FILE: &str = ".trove/purpleair-sync.json";

const SERVICE: &str = "purpleair";
const SOURCE: &str = "purpleair";
const API_BASE: &str = "https://api.purpleair.com";

/// Bounding-box half-width in degrees (≈16 km, sufficient for a dense metro).
const BBOX_DELTA: f64 = 0.15;
/// Only outdoor sensors (`location_type=0`).
const LOCATION_TYPE: u8 = 0;
/// Kept short to avoid stalling the owner loop.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Hourly cadence — PurpleAir sensors update every 2 minutes but AQI readings
/// are not meaningfully different at sub-hourly resolution for vault storage.
pub const PURPLEAIR_SYNC_SECS: u64 = 3600;

/// Fields we request from the API — confirmed from aiopurpleair library and
/// Home Assistant coordinator (coordinator.py, SENSOR_FIELDS_TO_RETRIEVE).
/// We ask for pm2.5 (raw PM2.5 concentration, ug/m3), temperature (F),
/// humidity (%), plus sensor identity and location fields.
const REQUESTED_FIELDS: &str =
    "sensor_index,name,latitude,longitude,last_seen,pm2.5,pm1.0,pm10.0,temperature,humidity,pressure,confidence";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_purpleair_sync()
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

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("purpleair synced — {n} readings")
            }))
        }
        Err(e) => Ok(CollectOutcome::note(format!("purpleair sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "PurpleAir: nothing new (no sensors nearby or already up to date)".to_string()
    } else {
        format!("PurpleAir synced — {n} readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "purpleair",
        name: "PurpleAir",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Polls hyperlocal PM2.5 readings from nearby PurpleAir community sensors. \
                      Higher spatial resolution than official monitors in dense urban areas.",
        domain: "environment",
        vault_path: "environment/purpleair/",
        toggleable: true,
        setup: &[
            "Get a free API Read Key at develop.purpleair.com (sign in with Google — no credit card).",
            "In the Developer Dashboard, create a project, generate a read key.",
            "Paste the key in the connect card below.",
            "Approve Location Services when asked, or set a location manually on the Weather tab.",
        ],
        caveats: "Coverage is uneven — dense in wealthy US/EU cities, sparse elsewhere. \
                  Returns no data outside coverage areas (no error). \
                  Requires Location Services or a manually set location.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(PURPLEAIR_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: Some("purpleair"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — the free API Read Key).

fn def_connect(vault: &Vault, key: &str) -> Result<()> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API key must not be empty");
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
    if let Some(_token) = vault.load_sync_token(SERVICE)? {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "PurpleAir API Read Key (configured)".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    let configured = !accounts.is_empty();
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] by the integrator.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "purpleair",
    display_name: "PurpleAir",
    methods: &[ConnectMethod::TokenPaste {
        label: "PurpleAir API Read Key",
        help: "Free key from develop.purpleair.com — sign in with Google, create a project, \
               generate a Read Key (no credit card required).",
        placeholder: "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["purpleair"],
    setup: &[
        "Go to develop.purpleair.com and sign in with Google.",
        "Create a project (any name), then click 'Generate Read Key'.",
        "Copy the Read Key and paste it here.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state: last-used location + last poll time.
/// Losing it costs nothing — the next pass re-resolves location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct PurpleAirSyncState {
    /// RFC3339 local time of the last pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Last-used latitude (0.0 = unset).
    #[serde(default)]
    pub lat: f64,
    /// Last-used longitude (0.0 = unset).
    #[serde(default)]
    pub lon: f64,
    /// How many sensors were in the most recent bounding-box result.
    #[serde(default)]
    pub sensor_count: u32,
    /// Non-empty while stuck (no location, no key, network error).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_purpleair_sync(&self) -> Option<PurpleAirSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_purpleair_sync(&self, state: &PurpleAirSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API shapes — confirmed from the official PurpleAir v1 API.
//
// PurpleAir uses a compact envelope: `fields` is an ordered list of field
// names; `data` is a list of value-tuples in the same order. The API docs
// at api.purpleair.com/v1/sensors confirm this layout (also verified in
// aiopurpleair/models/sensors.py which zips fields+data to reconstruct
// individual sensor objects).
//
// Key field names (confirmed from HA coordinator.py + aiopurpleair):
//   sensor_index  — u64 stable sensor id
//   name          — human label the owner gave the sensor
//   latitude, longitude — sensor's GPS coords
//   last_seen     — Unix timestamp (seconds) the sensor last reported
//   pm2.5         — PM2.5 concentration, ug/m3 (raw, not AQI)
//   pm1.0, pm10.0 — companion particle bands, ug/m3
//   temperature   — degrees Fahrenheit
//   humidity      — percent relative humidity
//   pressure      — hPa
//   confidence    — 0–100 data-quality score
//
// Note: the API does NOT return AQI directly — the brief's mention of
// "EPA correction" is about EPA's 2021 correction formula for PM2.5 raw
// concentrations; we store raw PM2.5 (ug/m3) and map to the pm25 metric.

/// Top-level API envelope for `/v1/sensors`.
#[derive(Debug, Deserialize)]
struct SensorsEnvelope {
    /// Ordered list of field names for every entry in `data`.
    fields: Vec<String>,
    /// Each inner list is one sensor's values in `fields` order.
    data: Vec<Vec<Value>>,
}

/// One sensor's parsed fields, assembled by zipping `fields` + `data`.
/// Some fields (pm10, pm100, pressure, raw) are stored for completeness but
/// are currently used only for raw-layer output; allow dead_code on the struct.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct SensorRow {
    /// Unique, stable sensor id — the primary key.
    pub sensor_index: u64,
    /// Human label.
    pub name: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Unix seconds the sensor last reported.
    pub last_seen: Option<i64>,
    /// PM2.5 concentration, ug/m3.
    pub pm25: Option<f64>,
    /// PM1.0 concentration, ug/m3.
    pub pm10: Option<f64>,
    /// PM10.0 concentration, ug/m3.
    pub pm100: Option<f64>,
    /// Temperature, degrees Fahrenheit (as returned by the API).
    pub temperature: Option<f64>,
    /// Relative humidity, percent.
    pub humidity: Option<f64>,
    /// Pressure, hPa.
    pub pressure: Option<f64>,
    /// Data-quality confidence score (0–100).
    pub confidence: Option<f64>,
    /// The raw object (all fields) for the raw layer.
    pub raw: Value,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

trait PurpleAirApi {
    /// `GET /v1/sensors?fields=...&location_type=0&nwlat=...&nwlng=...&selat=...&selng=...`
    fn get_sensors(&self, key: &str, nwlat: f64, nwlng: f64, selat: f64, selng: f64)
        -> Result<Value>;
}

struct PurpleAirClient {
    base: String,
}

impl PurpleAirClient {
    fn new(base: String) -> Self {
        Self { base }
    }
}

impl PurpleAirApi for PurpleAirClient {
    fn get_sensors(
        &self,
        key: &str,
        nwlat: f64,
        nwlng: f64,
        selat: f64,
        selng: f64,
    ) -> Result<Value> {
        let url = format!(
            "{}/v1/sensors?fields={}&location_type={LOCATION_TYPE}\
             &nwlat={nwlat}&nwlng={nwlng}&selat={selat}&selng={selng}",
            self.base, REQUESTED_FIELDS
        );
        ureq::get(&url)
            .set("X-API-Key", key)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("requesting PurpleAir sensors")?
            .into_json()
            .context("reading PurpleAir response")
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Parse the compact fields+data envelope into typed `SensorRow` objects.
/// Unknown or missing fields are handled gracefully (the API may return
/// additional fields not in our requested set; we ignore extras).
///
/// Returns `Err` on a structural parse failure so the caller can surface it
/// rather than silently collapsing to an empty result (which is
/// indistinguishable from a legitimately-empty bounding box).
fn parse_envelope(body: &Value) -> Result<(Vec<SensorRow>, Vec<Value>)> {
    let envelope: SensorsEnvelope = serde_json::from_value(body.clone())
        .context("PurpleAir: unexpected API envelope shape")?;

    let mut rows = Vec::new();
    let mut raws = Vec::new();

    for values in &envelope.data {
        if values.is_empty() {
            continue;
        }
        // Zip field names with values to reconstruct a named map.
        let obj: Map<String, Value> = envelope
            .fields
            .iter()
            .zip(values.iter())
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let raw = Value::Object(obj.clone());

        // sensor_index is mandatory — skip rows where it is missing or unparseable.
        let sensor_index = match obj.get("sensor_index").and_then(Value::as_u64) {
            Some(id) => id,
            None => {
                raws.push(raw);
                continue;
            }
        };

        let name = obj
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let latitude = obj.get("latitude").and_then(|v| v.as_f64());
        let longitude = obj.get("longitude").and_then(|v| v.as_f64());
        let last_seen = obj.get("last_seen").and_then(Value::as_i64);
        let pm25 = obj.get("pm2.5").and_then(|v| v.as_f64());
        let pm10 = obj.get("pm1.0").and_then(|v| v.as_f64());
        let pm100 = obj.get("pm10.0").and_then(|v| v.as_f64());
        let temperature = obj.get("temperature").and_then(|v| v.as_f64());
        let humidity = obj.get("humidity").and_then(|v| v.as_f64());
        let pressure = obj.get("pressure").and_then(|v| v.as_f64());
        let confidence = obj.get("confidence").and_then(|v| v.as_f64());

        rows.push(SensorRow {
            sensor_index,
            name,
            latitude,
            longitude,
            last_seen,
            pm25,
            pm10,
            pm100,
            temperature,
            humidity,
            pressure,
            confidence,
            raw: raw.clone(),
        });
        raws.push(raw);
    }

    Ok((rows, raws))
}

/// Build an RFC3339 timestamp string from a Unix `last_seen` timestamp.
/// Falls back to `now` if the sensor hasn't reported (null `last_seen`).
fn unix_to_rfc3339(unix_secs: Option<i64>, now: &DateTime<Local>) -> String {
    match unix_secs {
        Some(s) => {
            // Build a UTC DateTime and re-express in local time, so the
            // offset in the string reflects the machine's TZ (mirrors what
            // the poll instant would give us for a new observation).
            Utc.timestamp_opt(s, 0)
                .single()
                .map(|utc| DateTime::<Local>::from(utc).to_rfc3339())
                .unwrap_or_else(|| now.to_rfc3339())
        }
        None => now.to_rfc3339(),
    }
}

/// UTC-hour label for a Unix `last_seen` timestamp, used as the raw-layer
/// dedup dimension.  Derived from UTC (not local time) so it is stable across
/// machine-TZ changes and DST transitions.
///
/// Returns `"YYYY-MM-DDTHH"` in UTC.  Falls back to the poll-instant UTC hour
/// when `last_seen` is missing.
fn utc_hour_label(unix_secs: Option<i64>, now: &DateTime<Local>) -> String {
    let utc: DateTime<Utc> = match unix_secs {
        Some(s) => Utc.timestamp_opt(s, 0).single().unwrap_or_else(Utc::now),
        None => DateTime::<Utc>::from(*now),
    };
    // Format to hour precision in UTC: "2026-06-15T10"
    utc.format("%Y-%m-%dT%H").to_string()
}

/// Hour-truncated ISO label for contract-layer deduplication (YYYY-MM-DDTHH).
/// PurpleAir sensors typically report every 2 minutes; we dedupe within the
/// same observation hour so a re-poll of the same hour upserts in place.
///
/// NOTE: this is applied to the already-local-tz-rendered RFC3339 string from
/// unix_to_rfc3339, so it inherits any local-TZ rendering.  For the raw layer,
/// prefer `utc_hour_label` which operates on the UTC epoch directly.
fn hour_label(ts: &str) -> &str {
    // "2026-06-15T10:35:00-07:00" → "2026-06-15T10"
    // Safe because ts is always ≥13 chars when well-formed.
    if ts.len() >= 13 { &ts[..13] } else { ts }
}

/// Convert one `SensorRow` into zero, one, or several `EnvReading` entries —
/// one per metric that is non-null (pm25, temperature, humidity).
/// `guid` = `purpleair-<sensor_index>-<metric>-<YYYY-MM-DDTHH>` so a re-poll
/// of the same sensor + metric + hour upserts rather than duplicates.
fn sensor_to_readings(row: &SensorRow, now: &DateTime<Local>) -> Vec<EnvReading> {
    let ts = unix_to_rfc3339(row.last_seen, now);
    let hour = hour_label(&ts);

    // Shared extra fields: sensor identity, confidence.
    let make_extra = |metric_name: &str| -> Map<String, Value> {
        let mut extra = Map::new();
        extra.insert(
            "sensor_index".into(),
            Value::Number(serde_json::Number::from(row.sensor_index)),
        );
        if !row.name.is_empty() {
            extra.insert("sensor_name".into(), Value::String(row.name.clone()));
        }
        if let Some(c) = row.confidence {
            extra.insert(
                "confidence".into(),
                serde_json::Number::from_f64(c)
                    .map(|n| Value::Number(n))
                    .unwrap_or(Value::Null),
            );
        }
        // pm2.5 and companion bands always go into pm25 readings' extra for context.
        if metric_name != "pm25" {
            if let Some(v) = row.pm25 {
                if let Some(n) = serde_json::Number::from_f64(v) {
                    extra.insert("pm25_ug_m3".into(), Value::Number(n));
                }
            }
        }
        extra
    };

    let mut readings = Vec::new();

    // --- pm25 (ug/m3) ---
    if let Some(pm25) = row.pm25 {
        let guid = format!("purpleair-{}-pm25-{}", row.sensor_index, hour);
        readings.push(EnvReading {
            ts: ts.clone(),
            source: SOURCE.into(),
            metric: "pm25".into(),
            value: pm25,
            unit: "ug_m3".into(),
            place: row.name.clone(),
            lat: row.latitude,
            lon: row.longitude,
            station: row.sensor_index.to_string(),
            guid: Some(guid),
            extra: make_extra("pm25"),
        });
    }

    // --- temperature (Fahrenheit as returned by the API) ---
    if let Some(temp) = row.temperature {
        let guid = format!("purpleair-{}-temperature-{}", row.sensor_index, hour);
        readings.push(EnvReading {
            ts: ts.clone(),
            source: SOURCE.into(),
            metric: "temperature".into(),
            value: temp,
            unit: "F".into(),
            place: row.name.clone(),
            lat: row.latitude,
            lon: row.longitude,
            station: row.sensor_index.to_string(),
            guid: Some(guid),
            extra: make_extra("temperature"),
        });
    }

    // --- humidity (%) ---
    if let Some(hum) = row.humidity {
        let guid = format!("purpleair-{}-humidity-{}", row.sensor_index, hour);
        readings.push(EnvReading {
            ts: ts.clone(),
            source: SOURCE.into(),
            metric: "humidity".into(),
            value: hum,
            unit: "percent".into(),
            place: row.name.clone(),
            lat: row.latitude,
            lon: row.longitude,
            station: row.sensor_index.to_string(),
            guid: Some(guid),
            extra: make_extra("humidity"),
        });
    }

    readings
}

// ---------------------------------------------------------------------------
// Upsert into stable month partitions (airnow.rs pattern).

/// Upsert contract readings into `environment/purpleair/YYYY-MM.jsonl`.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvReading>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!(
                "purpleair reading {:?} has unpartitionable ts {:?}",
                r.guid, r.ts
            )
        })?;
        by_key.entry(key.to_string()).or_default().push(r);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<EnvReading> = stream.read(&key)?;
        for row in incoming {
            match existing
                .iter_mut()
                .find(|e| e.guid == row.guid && e.guid.is_some())
            {
                Some(slot) => *slot = row.clone(),
                None => existing.push(row.clone()),
            }
            written += 1;
        }
        vault.write_snapshot(&format!("{DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(written)
}

/// Upsert raw sensor objects into `environment/purpleair/raw/YYYY-MM.jsonl`.
///
/// Dedup key = `"<sensor_index>/<utc_hour>"` (mirrors airnow's
/// ParameterName+DateObserved+HourObserved key).  Using the UTC hour from
/// `last_seen` (the epoch field present in every raw object) keeps one raw
/// line per sensor per observation-hour without collapsing a month of hourly
/// polls into a single row (the bug that would occur with sensor_index alone).
/// UTC derivation also makes the key stable across machine-TZ changes and DST.
fn upsert_raw(vault: &Vault, raws: &[Value], ts: &str) -> Result<()> {
    let key = Partition::Month.key(ts).context("purpleair raw partition key")?;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut existing: Vec<Value> = stream.read(&key)?;
    for item in raws {
        let item_raw_key = raw_dedup_key(item);
        if item_raw_key.is_empty() {
            // No sensor_index — cannot deduplicate; append unconditionally.
            existing.push(item.clone());
        } else {
            match existing
                .iter_mut()
                .find(|v| raw_dedup_key(v) == item_raw_key)
            {
                Some(slot) => *slot = item.clone(),
                None => existing.push(item.clone()),
            }
        }
    }
    vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)
}

/// Compute the raw-layer dedup key for one sensor object.
/// Format: `"<sensor_index>/<YYYY-MM-DDTHH_utc>"`.
/// Returns an empty string when sensor_index is missing (upsert falls back to append).
fn raw_dedup_key(v: &Value) -> String {
    let idx = match v.get("sensor_index").and_then(Value::as_u64) {
        Some(i) if i > 0 => i,
        _ => return String::new(),
    };
    // Derive the UTC hour from last_seen (the epoch timestamp in every raw object).
    let last_seen = v.get("last_seen").and_then(Value::as_i64);
    let utc_hour = utc_hour_label(last_seen, &Local::now());
    format!("{idx}/{utc_hour}")
}

// ---------------------------------------------------------------------------
// The pull.

fn load_api_key(vault: &Vault) -> Result<String> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("PurpleAir API Read Key not configured — connect via the hub card")?;
    let key = token.access_token.trim().to_string();
    if key.is_empty() {
        bail!("PurpleAir API Read Key is empty — reconnect via the hub card");
    }
    Ok(key)
}

/// Resolve the user's location using the same ladder as [`crate::airnow`]:
/// CoreLocation → manual weather location → cursor.
fn resolve_point(vault: &Vault, state: &PurpleAirSyncState) -> Option<(f64, f64)> {
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

/// Production entry point.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_purpleair_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error =
            "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_purpleair_sync(&state)?;
        return Ok(PullOutcome {
            headline: "PurpleAir: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }
    let client = PurpleAirClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &client)
}

/// Offline-testable pull with explicit point + injected API.
pub fn pull_at(vault: &Vault, point: Option<(f64, f64)>) -> Result<PullOutcome> {
    let client = PurpleAirClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &client)
}

fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    client: &impl PurpleAirApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "PurpleAir: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    };

    let key = load_api_key(vault)?;
    let now = Local::now();
    let mut state = vault.read_purpleair_sync().unwrap_or_default();

    // Bounding box: ±BBOX_DELTA degrees around the user's point.
    let nwlat = lat + BBOX_DELTA;
    let nwlng = lon - BBOX_DELTA;
    let selat = lat - BBOX_DELTA;
    let selng = lon + BBOX_DELTA;

    let body = client.get_sensors(&key, nwlat, nwlng, selat, selng)?;
    // parse_envelope returns Err on a shape regression so it is distinguishable
    // from a legitimately-empty bounding box (which yields Ok with empty vecs).
    let (sensor_rows, raws) = parse_envelope(&body).map_err(|e| {
        // Surface the error via state before propagating so the hub card shows it.
        let mut err_state = vault.read_purpleair_sync().unwrap_or_default();
        err_state.error = format!("API shape changed: {e}");
        let _ = vault.write_purpleair_sync(&err_state);
        e
    })?;

    // Build contract readings: one EnvReading per metric per sensor.
    let readings: Vec<EnvReading> = sensor_rows
        .iter()
        .flat_map(|s| sensor_to_readings(s, &now))
        .collect();

    let readings_written = if !readings.is_empty() {
        upsert_readings(vault, &readings)?
    } else {
        0
    };

    // Raw layer: unconditional full fidelity.
    if !raws.is_empty() {
        let raw_ts = readings
            .first()
            .map(|r| r.ts.clone())
            .unwrap_or_else(|| now.to_rfc3339());
        upsert_raw(vault, &raws, &raw_ts)?;
    }

    // Advance cursor.
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.sensor_count = sensor_rows.len() as u32;
    state.error = String::new();
    vault.write_purpleair_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{readings_written} readings from {} sensors", sensor_rows.len()),
        counts: BTreeMap::from([
            ("readings", readings_written),
            ("sensors", sensor_rows.len() as u64),
        ]),
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
            .join(format!("trove-purpleair-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Inject a test API key so load_api_key doesn't bail.
    fn seed_key(vault: &Vault) {
        let token = crate::sync::oauth::TokenSet {
            access_token: "test-read-key".to_string(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: None,
        };
        vault.save_sync_token(SERVICE, &token).unwrap();
    }

    // -----------------------------------------------------------------------
    // Confirmed fixture: envelope structure verified from the official PurpleAir
    // API community documentation (community.purpleair.com/t/making-api-calls-with
    // -the-purpleair-api/180) and the aiopurpleair library model (models/sensors.py).
    // Field names confirmed from Home Assistant's SENSOR_FIELDS_TO_RETRIEVE list
    // (coordinator.py) and the HA sensor.py implementation.
    //
    // The API returns a compact envelope:
    //   { "fields": ["sensor_index", "name", ...], "data": [[101, "Sensor A", ...], ...] }
    // Each inner array contains values in the same order as `fields`.
    // pm2.5 is returned in ug/m3 (raw concentration, NOT AQI).
    // temperature is returned in °F (as-is from the API).
    // last_seen is a Unix timestamp (seconds since epoch).

    /// Two nearby outdoor sensors with full PM2.5 + temp + humidity data.
    fn fixture_two_sensors() -> Value {
        json!({
            "api_version": "V1.0.10-0.0.12",
            "time_stamp": 1750000000u64,
            "data_time_stamp": 1750000060u64,
            "fields": [
                "sensor_index", "name", "latitude", "longitude",
                "last_seen", "pm2.5", "pm1.0", "pm10.0",
                "temperature", "humidity", "pressure", "confidence"
            ],
            "data": [
                [101, "Sensor Alpha", 34.0610, -118.2430, 1750000000u64, 8.2, 5.1, 10.4, 72.0, 58.0, 1013.2, 100],
                [202, "Sensor Beta",  34.0520, -118.2350, 1749999600u64, 12.5, 7.8, 15.1, 75.0, 62.0, 1012.8, 97]
            ]
        })
    }

    /// Single sensor, only pm2.5 — temperature and humidity are null
    /// (sensor hardware sometimes omits environmental sensors).
    fn fixture_pm25_only() -> Value {
        json!({
            "api_version": "V1.0.10-0.0.12",
            "time_stamp": 1750000000u64,
            "data_time_stamp": 1750000060u64,
            "fields": ["sensor_index", "name", "latitude", "longitude", "last_seen", "pm2.5"],
            "data": [
                [303, "PM-only sensor", 34.06, -118.24, 1750000000u64, 5.5]
            ]
        })
    }

    /// Empty bounding box — no sensors in the area (non-dense region).
    fn fixture_empty() -> Value {
        json!({
            "api_version": "V1.0.10-0.0.12",
            "time_stamp": 1750000000u64,
            "data_time_stamp": 1750000000u64,
            "fields": ["sensor_index", "name", "latitude", "longitude", "last_seen", "pm2.5"],
            "data": []
        })
    }

    /// Sensor with no pm2.5 value (null) — a low-confidence or offline sensor.
    fn fixture_null_pm25() -> Value {
        json!({
            "api_version": "V1.0.10-0.0.12",
            "time_stamp": 1750000000u64,
            "data_time_stamp": 1750000000u64,
            "fields": ["sensor_index", "name", "latitude", "longitude", "last_seen", "pm2.5", "temperature"],
            "data": [
                [404, "Offline sensor", 34.06, -118.24, 1749990000u64, null, null]
            ]
        })
    }

    /// Same sensor as fixture_two_sensors sensor 101, but last_seen is one hour
    /// later (1750003600 = 1750000000 + 3600 s).  Used to verify that a second
    /// poll in a different hour produces a NEW raw row, not an overwrite.
    fn fixture_sensor_hour2() -> Value {
        json!({
            "api_version": "V1.0.10-0.0.12",
            "time_stamp": 1750003600u64,
            "data_time_stamp": 1750003660u64,
            "fields": [
                "sensor_index", "name", "latitude", "longitude",
                "last_seen", "pm2.5", "pm1.0", "pm10.0",
                "temperature", "humidity", "pressure", "confidence"
            ],
            "data": [
                [101, "Sensor Alpha", 34.0610, -118.2430, 1750003600u64, 9.1, 5.8, 11.2, 73.0, 57.0, 1013.5, 100]
            ]
        })
    }

    struct Stub {
        body: Value,
    }
    impl PurpleAirApi for Stub {
        fn get_sensors(&self, _key: &str, _nwlat: f64, _nwlng: f64, _selat: f64, _selng: f64) -> Result<Value> {
            Ok(self.body.clone())
        }
    }

    const SEED: (f64, f64) = (34.05, -118.24);

    // -----------------------------------------------------------------------
    // Parser tests.

    #[test]
    fn parse_two_sensors_yields_correct_row_count() {
        let (rows, raws) = parse_envelope(&fixture_two_sensors()).unwrap();
        // 2 sensors × 3 metrics (pm25 + temperature + humidity) = 6 EnvReading objects
        // But parse_envelope returns SensorRow objects (one per sensor), not readings.
        assert_eq!(rows.len(), 2, "two sensors parsed");
        assert_eq!(raws.len(), 2, "two raw objects");

        let alpha = rows.iter().find(|r| r.sensor_index == 101).unwrap();
        assert_eq!(alpha.name, "Sensor Alpha");
        assert_eq!(alpha.pm25, Some(8.2));
        assert_eq!(alpha.temperature, Some(72.0));
        assert_eq!(alpha.humidity, Some(58.0));
        assert_eq!(alpha.latitude, Some(34.0610));
        assert_eq!(alpha.longitude, Some(-118.2430));
        assert_eq!(alpha.confidence, Some(100.0));
    }

    #[test]
    fn parse_bad_envelope_returns_err() {
        // A completely wrong shape must not silently return empty — it must Err.
        let bad = json!({"wrong_key": [1, 2, 3]});
        assert!(
            parse_envelope(&bad).is_err(),
            "structural envelope mismatch must return Err, not empty vecs"
        );
    }

    #[test]
    fn sensor_to_readings_generates_three_readings_per_full_sensor() {
        let (rows, _) = parse_envelope(&fixture_two_sensors()).unwrap();
        let alpha = rows.iter().find(|r| r.sensor_index == 101).unwrap();
        let now = Local::now();
        let readings = sensor_to_readings(alpha, &now);
        // pm25 + temperature + humidity = 3 readings.
        assert_eq!(readings.len(), 3, "3 readings from a full sensor");
        let pm25 = readings.iter().find(|r| r.metric == "pm25").unwrap();
        assert_eq!(pm25.source, "purpleair");
        assert_eq!(pm25.value, 8.2);
        assert_eq!(pm25.unit, "ug_m3");
        assert_eq!(pm25.station, "101");
        let temp = readings.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(temp.unit, "F");
        assert_eq!(temp.value, 72.0);
        let hum = readings.iter().find(|r| r.metric == "humidity").unwrap();
        assert_eq!(hum.unit, "percent");
        assert_eq!(hum.value, 58.0);
    }

    #[test]
    fn pm25_only_sensor_yields_one_reading() {
        let (rows, _) = parse_envelope(&fixture_pm25_only()).unwrap();
        let now = Local::now();
        let readings: Vec<_> = rows.iter().flat_map(|s| sensor_to_readings(s, &now)).collect();
        // Only pm25 is present — 1 reading, not 3.
        assert_eq!(readings.len(), 1, "only pm25 reading when temp/humidity absent");
        assert_eq!(readings[0].metric, "pm25");
        assert_eq!(readings[0].value, 5.5);
    }

    #[test]
    fn null_pm25_sensor_yields_no_readings() {
        let (rows, raws) = parse_envelope(&fixture_null_pm25()).unwrap();
        // The sensor must appear in raw (full fidelity), but yield no contract readings.
        assert_eq!(rows.len(), 1, "null-pm25 sensor still parsed");
        assert_eq!(raws.len(), 1, "raw keeps null-pm25 sensor");
        let now = Local::now();
        let readings: Vec<_> = rows.iter().flat_map(|s| sensor_to_readings(s, &now)).collect();
        assert_eq!(readings.len(), 0, "no contract readings when pm25 and temperature both null");
    }

    #[test]
    fn empty_bbox_yields_no_rows() {
        let (rows, raws) = parse_envelope(&fixture_empty()).unwrap();
        assert_eq!(rows.len(), 0);
        assert_eq!(raws.len(), 0);
    }

    #[test]
    fn guid_encodes_sensor_index_metric_and_hour() {
        let (rows, _) = parse_envelope(&fixture_two_sensors()).unwrap();
        let alpha = rows.iter().find(|r| r.sensor_index == 101).unwrap();
        let now = Local::now();
        let readings = sensor_to_readings(alpha, &now);
        let pm25 = readings.iter().find(|r| r.metric == "pm25").unwrap();
        let guid = pm25.guid.as_deref().unwrap_or("");
        // Must contain sensor_index + metric.
        assert!(guid.contains("101"), "guid must contain sensor_index; got={guid}");
        assert!(guid.contains("pm25"), "guid must contain metric; got={guid}");
        // Two different sensors with the same metric must produce different guids.
        let beta = rows.iter().find(|r| r.sensor_index == 202).unwrap();
        let beta_readings = sensor_to_readings(beta, &now);
        let beta_pm25 = beta_readings.iter().find(|r| r.metric == "pm25").unwrap();
        let beta_guid = beta_pm25.guid.as_deref().unwrap_or("");
        assert_ne!(guid, beta_guid, "different sensors must produce different guids");
    }

    #[test]
    fn station_field_is_sensor_index_string() {
        let (rows, _) = parse_envelope(&fixture_two_sensors()).unwrap();
        let now = Local::now();
        for row in &rows {
            let readings = sensor_to_readings(row, &now);
            for r in readings {
                assert_eq!(
                    r.station,
                    row.sensor_index.to_string(),
                    "station field must be the sensor_index string"
                );
            }
        }
    }

    #[test]
    fn extra_carries_sensor_index_and_confidence() {
        let (rows, _) = parse_envelope(&fixture_two_sensors()).unwrap();
        let alpha = rows.iter().find(|r| r.sensor_index == 101).unwrap();
        let now = Local::now();
        let readings = sensor_to_readings(alpha, &now);
        let pm25 = readings.iter().find(|r| r.metric == "pm25").unwrap();
        assert_eq!(
            pm25.extra.get("sensor_index"),
            Some(&json!(101u64)),
            "extra must carry sensor_index"
        );
        assert_eq!(
            pm25.extra.get("confidence"),
            Some(&json!(100.0)),
            "extra must carry confidence"
        );
    }

    // -----------------------------------------------------------------------
    // Pull / store / dedupe tests.

    #[test]
    fn pull_writes_contract_and_raw_files() {
        let v = temp_vault("pull");
        seed_key(&v);
        let stub = Stub { body: fixture_two_sensors() };
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        // 2 sensors × 3 metrics = 6 readings.
        assert_eq!(
            out.counts.get("readings"),
            Some(&6),
            "expected 6 readings (2 sensors × 3 metrics)"
        );
        assert_eq!(out.counts.get("sensors"), Some(&2));

        // Contract file must exist.
        let contract_file = v.root().join("environment/purpleair");
        assert!(contract_file.exists(), "contract directory must exist");

        // Raw layer must exist.
        assert!(
            v.root().join("environment/purpleair/raw").exists(),
            "raw directory must exist"
        );

        // Cursor advanced.
        let state = v.read_purpleair_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.lat, SEED.0);
        assert_eq!(state.sensor_count, 2);
        assert!(state.error.is_empty());
    }

    #[test]
    fn repoll_same_hour_upserts_not_duplicates() {
        let v = temp_vault("upsert");
        seed_key(&v);
        // First poll.
        pull_at_with(&v, Some(SEED), &Stub { body: fixture_two_sensors() }).unwrap();

        // Second poll (same data, same hour) — must not add new rows.
        let out2 = pull_at_with(&v, Some(SEED), &Stub { body: fixture_two_sensors() }).unwrap();
        assert_eq!(out2.counts.get("readings"), Some(&6));

        // Read back all contract rows by scanning the actual written month files.
        // The fixture's last_seen = 1750000000 → 2025-06 in local TZ (UTC-7 = PDT).
        // Use that specific key rather than Local::now() to avoid cross-month mismatch.
        let dir = v.root().join(DIR);
        let total_rows: usize = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                s.ends_with(".jsonl") && !s.contains("raw")
            })
            .map(|e| {
                let content = std::fs::read_to_string(e.path()).unwrap_or_default();
                content.lines().filter(|l| !l.trim().is_empty()).count()
            })
            .sum();
        assert_eq!(total_rows, 6, "upsert — no duplicate rows after re-poll");
    }

    /// Verifies the MAJOR RAW-DEDUP FIX: two polls of the same sensor in
    /// *different* UTC hours must produce two raw rows, not one overwrite.
    ///
    /// Before the fix, `upsert_raw` keyed on sensor_index alone, so the second
    /// poll silently overwrote the first — collapsing a month of hourly history
    /// to one row per sensor.  The fix keys on sensor_index + UTC_hour(last_seen).
    #[test]
    fn raw_different_hours_produce_separate_rows() {
        let v = temp_vault("raw-hours");
        seed_key(&v);

        // Hour 1: sensor 101 last_seen = 1750000000 (UTC 2025-06-15T06:xx).
        pull_at_with(&v, Some(SEED), &Stub { body: fixture_two_sensors() }).unwrap();

        // Hour 2: same sensor 101, last_seen = 1750003600 (UTC 2025-06-15T07:xx) — next hour.
        pull_at_with(&v, Some(SEED), &Stub { body: fixture_sensor_hour2() }).unwrap();

        // The raw directory must have rows for BOTH hours — count raw lines for sensor 101.
        let raw_dir = v.root().join(RAW_DIR);
        let total_raw: usize = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .map(|e| {
                let content = std::fs::read_to_string(e.path()).unwrap_or_default();
                content
                    .lines()
                    .filter(|l| !l.trim().is_empty() && l.contains("\"sensor_index\":101"))
                    .count()
            })
            .sum();

        assert_eq!(
            total_raw, 2,
            "sensor 101 should have 2 raw rows (one per UTC hour), got {total_raw}"
        );
    }

    #[test]
    fn raw_same_hour_upserts_not_duplicates() {
        let v = temp_vault("raw-same-hr");
        seed_key(&v);

        // Poll twice with the same fixture (same sensor_index, same last_seen → same UTC hour).
        pull_at_with(&v, Some(SEED), &Stub { body: fixture_two_sensors() }).unwrap();
        pull_at_with(&v, Some(SEED), &Stub { body: fixture_two_sensors() }).unwrap();

        // Should still be 2 raw rows (one per sensor), not 4.
        let raw_dir = v.root().join(RAW_DIR);
        let total_raw: usize = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .map(|e| {
                let content = std::fs::read_to_string(e.path()).unwrap_or_default();
                content.lines().filter(|l| !l.trim().is_empty()).count()
            })
            .sum();

        assert_eq!(total_raw, 2, "same-hour re-poll must not duplicate raw rows; got {total_raw}");
    }

    #[test]
    fn empty_bbox_is_graceful() {
        let v = temp_vault("empty");
        seed_key(&v);
        let out = pull_at_with(&v, Some(SEED), &Stub { body: fixture_empty() }).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert_eq!(out.counts.get("sensors"), Some(&0));
        // No contract file written for zero readings.
        assert!(
            !v.root().join("environment/purpleair/raw").exists(),
            "no raw file when no sensors"
        );
        let state = v.read_purpleair_sync().unwrap();
        assert!(state.error.is_empty(), "empty bbox is not an error");
    }

    #[test]
    fn no_point_is_inert() {
        let v = temp_vault("inert");
        seed_key(&v);
        let out = pull_at(&v, None).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/purpleair").exists());
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("sync");
        let state = PurpleAirSyncState {
            updated: "2026-06-15T10:00:00-07:00".into(),
            lat: 34.05,
            lon: -118.24,
            sensor_count: 5,
            error: String::new(),
        };
        v.write_purpleair_sync(&state).unwrap();
        let loaded = v.read_purpleair_sync().unwrap();
        assert_eq!(loaded.lat, 34.05);
        assert_eq!(loaded.sensor_count, 5);
        assert!(loaded.error.is_empty());
    }

    #[test]
    fn connection_status_reflects_stored_key() {
        let v = temp_vault("conn-status");
        let s1 = def_status(&v).unwrap();
        assert!(!s1.configured, "no key stored yet");
        seed_key(&v);
        let s2 = def_status(&v).unwrap();
        assert!(s2.configured, "key stored → configured");
        def_disconnect(&v, "purpleair").unwrap();
        let s3 = def_status(&v).unwrap();
        assert!(!s3.configured, "disconnected → not configured");
    }

    #[test]
    fn connect_rejects_empty_key() {
        let v = temp_vault("empty-key");
        assert!(def_connect(&v, "").is_err());
        assert!(def_connect(&v, "   ").is_err());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Back-compat: a sparse reading line (only the 4 required fields)
        // must parse cleanly via the upsert read path.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/purpleair")).unwrap();
        let now = Local::now();
        let key = format!("{}", now.format("%Y-%m"));
        std::fs::write(
            v.root().join(format!("environment/purpleair/{key}.jsonl")),
            "{\"ts\":\"2026-06-15T10:00:00-07:00\",\"source\":\"purpleair\",\"metric\":\"pm25\",\"value\":8.2}\n",
        )
        .unwrap();
        let rows: Vec<EnvReading> = v.stream(DIR, Partition::Month).read(&key).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metric, "pm25");
        assert_eq!(rows[0].value, 8.2);
        assert_eq!(rows[0].guid, None, "sparse line has no guid");
    }
}
