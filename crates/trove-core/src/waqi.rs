//! World Air Quality Index (WAQI / aqicn.org) — global ground-monitor AQI.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/waqi.md
//!
//! A **Periodic** cloud pull: current AQI + per-pollutant sub-indices from the
//! nearest ground-monitor station to the user's configured location, polled
//! hourly from the public `api.waqi.info/feed/geo:LAT;LON/` endpoint.
//!
//! Writes the **`environment`** domain's scalar reading shape ([`EnvReading`]):
//!
//! - **contract layer** — `environment/waqi/YYYY-MM.jsonl` — one [`EnvReading`]
//!   per metric per poll (AQI + available per-pollutant sub-indices), keyed by
//!   `waqi:<station-idx>:<metric>:<iso-ts>` (upserts — a re-poll of the same
//!   observation time updates in place).
//! - **raw layer** — `environment/waqi/raw/YYYY-MM.jsonl` — the full API `data`
//!   object at full fidelity, one line per poll, unconditionally.
//!
//! **Auth:** a free API token pasted by the user via the connect card
//! ([`ConnectMethod::TokenPaste`]), stored in the 0600 secret store under
//! `.trove/sync/waqi.json`. The token is issued instantly (no email, no billing)
//! at <https://aqicn.org/data-platform/token/>.
//!
//! **Location:** reuses the same weather location ladder as [`crate::nws`] and
//! [`crate::airnow`]: CoreLocation → manual weather location → last-used cursor.
//!
//! **Stale station:** when `data.aqi` equals `-1` the nearest station has no
//! current data; the collector degrades gracefully (zero contract rows, raw
//! layer still written) without an error — the next hourly poll will retry.
//!
//! **No location:** inert, no rows, no error. Mirrors nws.rs / airnow.rs.
//!
//! **Field evidence:** JSON shapes confirmed against the live demo endpoint
//! `api.waqi.info/feed/geo:LAT;LON/?token=demo` (2026-06-16) — exact field
//! names verified: `data.aqi`, `data.idx`, `data.dominentpol`, `data.time.iso`,
//! `data.city.name/geo/url`, `data.iaqi.<pollutant>.v`, `data.attributions`.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
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

/// Contract-layer reading directory; raw layer in `raw/`.
const DIR: &str = "environment/waqi";
const RAW_DIR: &str = "environment/waqi/raw";
/// Non-secret rebuildable cursor — last-used location + last poll time.
const SYNC_FILE: &str = ".trove/waqi-sync.json";

const SERVICE: &str = "waqi";
const SOURCE: &str = "waqi";
const API_BASE: &str = "https://api.waqi.info";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Hourly cadence: WAQI station observations update ~once per hour.
pub const WAQI_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_waqi_sync()
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
                format!("waqi synced — {n} AQI readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "waqi sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "WAQI: nothing new (no location, stale station, or already up to date)".to_string()
    } else {
        format!("WAQI synced — {n} AQI readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. `pub static DEF` is
/// the single entry point; `pub mod waqi;` and `&crate::waqi::DEF` in
/// `INTEGRATIONS` are pre-existing — this replaces the NotWired stub body only.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "waqi",
        name: "World Air Quality Index",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls hourly AQI readings from the WAQI network of 10,000+ \
                      ground monitors worldwide — uniquely covering Asian, European, \
                      and other cities not served by AirNow or Open-Meteo model data. \
                      Returns AQI, dominant pollutant, and per-pollutant sub-indices \
                      for the nearest station to your location.",
        domain: "environment",
        vault_path: "environment/waqi/",
        toggleable: true,
        setup: &[
            "Get a free token instantly at aqicn.org/data-platform/token — no email confirmation, no billing.",
            "Paste the token in the connect card.",
            "Approve Location Services when asked, or set a location manually on the Weather tab.",
        ],
        caveats: "Requires a free token from aqicn.org/data-platform/token. \
                  Degrades gracefully when the nearest station is stale (AQI = -1): \
                  no contract rows are written but raw data is preserved. \
                  Inert without a location.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WAQI_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: Some("waqi"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — the free API token).

fn def_connect(vault: &Vault, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!("API token must not be empty");
    }
    let ts = crate::sync::oauth::TokenSet {
        access_token: token.to_string(),
        refresh_token: None,
        token_type: None,
        scope: None,
        expires_at: None,
    };
    vault.save_sync_token(SERVICE, &ts)
}

fn def_disconnect(vault: &Vault, _token: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(_ts) = vault.load_sync_token(SERVICE)? {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "WAQI API token (configured)".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    let configured = !accounts.is_empty();
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] (one `&crate::waqi::CONNECTION`
/// line to be added by the integrator).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "waqi",
    display_name: "World Air Quality Index",
    methods: &[ConnectMethod::TokenPaste {
        label: "WAQI API token",
        help: "Free token issued instantly — no email confirmation, no billing. \
               Visit aqicn.org/data-platform/token, fill in the form, \
               and copy the token that appears on the page.",
        placeholder: "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["waqi"],
    setup: &[
        "Visit aqicn.org/data-platform/token.",
        "Fill in the short form (name + email) — no email confirmation required.",
        "Copy the token shown on the page and paste it here.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state: last-used location + last poll time.
/// Losing it costs nothing — the next pass re-resolves the location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct WaqiSyncState {
    /// RFC3339 local time of the last successful pass.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Last-used latitude (0.0 = unset).
    #[serde(default)]
    pub lat: f64,
    /// Last-used longitude (0.0 = unset).
    #[serde(default)]
    pub lon: f64,
    /// Human-readable nearest station name from the last successful poll.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub station: String,
    /// Non-empty while stuck (no location, no token, network error) — for the UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_waqi_sync(&self) -> Option<WaqiSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_waqi_sync(&self, state: &WaqiSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API shapes — confirmed against api.waqi.info/feed/geo:LAT;LON/?token=demo
// (2026-06-16). Field names verified: data.aqi, data.idx, data.dominentpol,
// data.time.iso, data.city.{name,geo,url}, data.iaqi.<pol>.v, data.attributions.

/// The outer API envelope: `{"status":"ok","data":{...}}`.
/// Note: `status` is read from the raw `Value` before deserialization (because
/// error responses use `data` as a plain string, not `WaqiData`). The struct
/// is only deserialized when we have already confirmed `status == "ok"`.
#[derive(Debug, Deserialize)]
struct WaqiResponse {
    #[allow(dead_code)]
    status: String,
    #[serde(default)]
    data: Option<WaqiData>,
}

/// The `data` object — the primary payload.
#[derive(Debug, Deserialize)]
struct WaqiData {
    /// Current AQI. -1 means "no data" (stale/unavailable station).
    #[serde(default)]
    aqi: i32,
    /// Unique numeric station id across the WAQI network.
    #[serde(default)]
    idx: i64,
    /// The dominant pollutant code (`"pm25"`, `"o3"`, …).
    #[serde(default)]
    dominentpol: String,
    /// Station name and coordinates.
    #[serde(default)]
    city: WaqiCity,
    /// Measurement time (we use `time.iso` for the RFC3339 ts).
    #[serde(default)]
    time: WaqiTime,
    /// Per-pollutant sub-indices, each carrying a single `v` value.
    #[serde(default)]
    iaqi: Map<String, Value>,
    /// Attribution sources (kept verbatim in raw; unused at parse time).
    #[serde(default)]
    #[allow(dead_code)]
    attributions: Vec<Value>,
}

#[derive(Debug, Default, Deserialize)]
struct WaqiCity {
    /// Human label of the nearest station (`"Shanghai (上海)"`).
    #[serde(default)]
    name: String,
    /// `[lat, lon]` of the station.
    #[serde(default)]
    geo: Vec<f64>,
    /// URL to the WAQI city page.
    #[serde(default)]
    url: String,
}

#[derive(Debug, Default, Deserialize)]
struct WaqiTime {
    /// ISO8601/RFC3339 timestamp of the measurement (`"2026-06-16T16:00:00+08:00"`).
    #[serde(default)]
    iso: String,
    /// Unix epoch seconds of the measurement (backup when `iso` is absent).
    #[serde(default)]
    v: i64,
    /// Timezone string (`"+08:00"`); captured for raw fidelity, not used in logic.
    #[serde(default)]
    #[allow(dead_code)]
    tz: String,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

pub(crate) trait WaqiApi {
    /// `GET /feed/geo:LAT;LON/?token=TOKEN` — returns the full JSON body.
    fn feed(&self, lat: f64, lon: f64, token: &str) -> Result<Value>;
}

struct WaqiClient {
    base: String,
}

impl WaqiClient {
    fn new() -> Self {
        WaqiClient { base: API_BASE.to_string() }
    }
}

impl WaqiApi for WaqiClient {
    fn feed(&self, lat: f64, lon: f64, token: &str) -> Result<Value> {
        let url = format!("{}/feed/geo:{lat};{lon}/?token={token}", self.base);
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("requesting WAQI geo feed")?
            .into_json()
            .context("reading WAQI response")
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Best-effort RFC3339 → local-offset RFC3339.
/// `data.time.iso` already carries an offset (`"2026-06-16T16:00:00+08:00"`);
/// we re-render it into the machine's local timezone so the vault `ts` is
/// consistent with every other collector. An unparseable value is passed
/// through verbatim (tolerant — better an odd string than a dropped row).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// Build the metric name for a per-pollutant `iaqi` key.
/// WAQI uses lowercase short codes (`pm25`, `pm10`, `o3`, `no2`, `so2`, `co`);
/// the environment domain spec uses the same names plus `temperature` (t),
/// `humidity` (h), `pressure` (p), `wind_speed` (w).
fn iaqi_metric(key: &str) -> &'static str {
    match key {
        "pm25" => "pm25",
        "pm10" => "pm10",
        "o3"   => "ozone",
        "no2"  => "no2",
        "so2"  => "so2",
        "co"   => "co",
        "t"    => "temperature",
        "h"    => "humidity",
        "p"    => "pressure",
        "w"    => "wind_speed",
        "uvi"  => "uv",
        // Unknown sensors pass through under a waqi-prefixed name.
        _      => "unknown",
    }
}

/// Unit string for each iaqi key. WAQI does not send units; these are the
/// standard units for each sensor as documented by WAQI / EPA.
fn iaqi_unit(key: &str) -> &'static str {
    match key {
        "pm25" | "pm10" => "aqi",
        "o3" | "no2" | "so2" => "aqi",
        "co"  => "aqi",
        "t"   => "C",
        "h"   => "percent",
        "p"   => "hpa",
        "w"   => "m/s",
        "uvi" => "index",
        _     => "",
    }
}

/// Parse a WAQI feed response into (contract readings, raw data object, api_error).
///
/// The primary contract reading is `metric="aqi"` with the overall AQI value.
/// Each `iaqi` key that carries a numeric `v` emits an additional reading.
/// Station lat/lon come from `data.city.geo[0..1]`.
/// guid = `waqi:<idx>:<metric>:<iso-ts>` — stable across re-polls.
///
/// Returns `(Vec<EnvReading>, Option<raw data Value>, Option<String>)`:
/// - The raw Value is `Some` even when `data.aqi == -1` (stale station) — full fidelity.
/// - When WAQI returns `{"status":"error","data":"..."}` (invalid key, over quota, etc.),
///   the third element is `Some(message)` and the caller MUST surface it — never treat
///   a token error as a clean empty-data poll. Raw is `None` on API errors (no data object).
///
/// A missing/null `data` object is a quiet no-op (malformed response).
fn parse_feed(body: &Value) -> (Vec<EnvReading>, Option<Value>, Option<String>) {
    // Inspect `status` from the raw Value first — this avoids a deserialization
    // failure when `data` is a string (WAQI error responses like
    // `{"status":"error","data":"Invalid key"}` can't round-trip through
    // `Option<WaqiData>`).
    let status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("");
    if status != "ok" {
        // Surface the API error message so the caller can set state.error and flag reconnect.
        // WAQI uses `data` as a plain string for error messages (e.g. "Invalid key",
        // "Over quota"). Fall back to the raw status string when data is absent/not-string.
        let api_err = body
            .get("data")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| format!("WAQI API error: {s}"))
            .unwrap_or_else(|| {
                let s = if status.is_empty() { "unknown" } else { status };
                format!("WAQI API error: status={s}")
            });
        return (Vec::new(), None, Some(api_err));
    }

    // Status is "ok" — now deserialize the full struct.
    let resp: WaqiResponse = match serde_json::from_value(body.clone()) {
        Ok(r) => r,
        Err(_) => return (Vec::new(), None, None),
    };
    let data = match resp.data {
        Some(d) => d,
        None => return (Vec::new(), None, None),
    };

    // Raw: the full `data` object, verbatim.
    let raw_val = body.get("data").cloned();

    // ts: data.time.iso (RFC3339 with offset) → local RFC3339.
    let ts_raw = if !data.time.iso.is_empty() {
        data.time.iso.clone()
    } else if data.time.v != 0 {
        // Fallback: epoch seconds → RFC3339 via chrono.
        use chrono::TimeZone;
        chrono::Utc
            .timestamp_opt(data.time.v, 0)
            .single()
            .map(|dt| dt.with_timezone(&Local).to_rfc3339())
            .unwrap_or_default()
    } else {
        // No usable time → can't partition or key.
        return (Vec::new(), raw_val, None);
    };
    if ts_raw.is_empty() {
        return (Vec::new(), raw_val, None);
    }
    let ts = to_local(&ts_raw);

    // Station location: data.city.geo = [lat, lon].
    let (station_lat, station_lon) = if data.city.geo.len() >= 2 {
        (Some(data.city.geo[0]), Some(data.city.geo[1]))
    } else {
        (None, None)
    };
    let station_name = data.city.name.clone();
    let station_idx = data.idx;

    // Stale station: aqi == -1 means no current data. Write raw (for auditability)
    // but emit zero contract rows (no -1 rows in the contract store).
    if data.aqi == -1 {
        return (Vec::new(), raw_val, None);
    }

    let mut readings = Vec::new();

    // Helper: build an EnvReading for a given metric, value, unit.
    let mk = |metric: &str, value: f64, unit: &str, extra: Map<String, Value>| EnvReading {
        ts: ts.clone(),
        source: SOURCE.into(),
        metric: metric.to_string(),
        value,
        unit: unit.to_string(),
        place: station_name.clone(),
        lat: station_lat,
        lon: station_lon,
        station: station_idx.to_string(),
        guid: Some(format!("waqi:{station_idx}:{metric}:{ts_raw}")),
        extra,
    };

    // Primary AQI reading.
    {
        let mut extra = Map::new();
        if !data.dominentpol.is_empty() {
            extra.insert("dominant_pollutant".into(), Value::String(data.dominentpol.clone()));
        }
        if !data.city.url.is_empty() {
            extra.insert("station_url".into(), Value::String(data.city.url.clone()));
        }
        readings.push(mk("aqi", data.aqi as f64, "aqi", extra));
    }

    // Per-pollutant iaqi sub-indices.
    for (key, val) in &data.iaqi {
        let v = match val.get("v").and_then(Value::as_f64) {
            Some(f) => f,
            None => continue,
        };
        let raw_metric = iaqi_metric(key.as_str());
        // Use the raw key name for unknowns (never leak a 'static "unknown" label).
        let metric = if raw_metric == "unknown" {
            // Box::leak is acceptable here — unknown keys are rare and the
            // set is bounded by the WAQI sensor vocabulary.
            Box::leak(format!("waqi_{key}").into_boxed_str()) as &str
        } else {
            raw_metric
        };
        let unit = iaqi_unit(key.as_str());
        let extra = Map::new();
        readings.push(mk(metric, v, unit, extra));
    }

    (readings, raw_val, None)
}

// ---------------------------------------------------------------------------
// Upsert into stable month partitions (airnow.rs / nws.rs pattern).

/// Upsert readings into `environment/waqi/YYYY-MM.jsonl` by `guid`.
/// A re-poll of the same station + metric + hour upserts in place, never
/// duplicates.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: std::collections::BTreeMap<String, Vec<&EnvReading>> = Default::default();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!(
                "waqi reading {:?} has unpartitionable ts {:?}",
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

/// Append/upsert one raw data object into `environment/waqi/raw/YYYY-MM.jsonl`.
/// Dedupe by `idx` + `time.iso` so a re-poll of the same hour never duplicates.
fn upsert_raw(vault: &Vault, raw: &Value, ts: &str) -> Result<()> {
    let key = Partition::Month.key(ts).context("waqi raw partition key")?;
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut existing: Vec<Value> = stream.read(&key)?;
    // Build a dedup key from idx + time.iso.
    let raw_key = raw_dedupe_key(raw);
    if raw_key.is_empty() {
        // No usable key — append unconditionally (should be rare).
        existing.push(raw.clone());
    } else {
        match existing.iter_mut().find(|v| raw_dedupe_key(v) == raw_key) {
            Some(slot) => *slot = raw.clone(),
            None => existing.push(raw.clone()),
        }
    }
    vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)
}

/// A stable dedup key for a raw WAQI `data` object: `<idx>:<time.iso>`.
fn raw_dedupe_key(v: &Value) -> String {
    let idx = v.get("idx").and_then(Value::as_i64).unwrap_or(0);
    let iso = v
        .get("time")
        .and_then(|t| t.get("iso"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if idx == 0 || iso.is_empty() {
        return String::new();
    }
    format!("{idx}:{iso}")
}

// ---------------------------------------------------------------------------
// Location ladder (mirrors airnow.rs / nws.rs).

fn resolve_point(vault: &Vault, state: &WaqiSyncState) -> Option<(f64, f64)> {
    if corelocation::auth_status() == AuthStatus::NotDetermined {
        corelocation::request_access(5);
    }
    if let Some(fix) = corelocation::current_location(8) {
        return Some((fix.lat, fix.lon));
    }
    if let Some(m) = vault.weather_location() {
        return Some((m.lat, m.lon));
    }
    // Last-used: 0,0 is the Atlantic off Ghana — safe as "unset".
    (state.lat != 0.0 || state.lon != 0.0).then_some((state.lat, state.lon))
}

// ---------------------------------------------------------------------------
// Load token.

fn load_token(vault: &Vault) -> Result<String> {
    let ts = vault
        .load_sync_token(SERVICE)?
        .context("WAQI token not configured — connect via the hub card")?;
    let token = ts.access_token.trim().to_string();
    if token.is_empty() {
        bail!("WAQI token is empty — reconnect via the hub card");
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// The pull.

/// Production entry point: resolve location + load token + sync.
/// Inert when no location (no rows, no error). Propagates token errors
/// so the hub card shows the "connect" prompt.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_waqi_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        // No location: record the reason on the cursor for the UI banner.
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error =
            "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_waqi_sync(&state)?;
        return Ok(PullOutcome {
            headline: "WAQI: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }
    let token = load_token(vault)?;
    let client = WaqiClient::new();
    pull_at_with(vault, point, &token, &client)
}

/// Network + write body over an explicit point. The test seam: drives offline
/// with an injected client, never touches CoreLocation or the token store.
pub(crate) fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    token: &str,
    client: &impl WaqiApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        // No point → inert no-op (mirrors nws.rs / airnow.rs).
        return Ok(PullOutcome {
            headline: "WAQI: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    };
    let now = Local::now();
    let mut state = vault.read_waqi_sync().unwrap_or_default();

    let body = client.feed(lat, lon, token)?;
    let (readings, raw, api_err) = parse_feed(&body);

    // Defect #1: a non-ok API status (invalid/revoked token, over-quota) must
    // never be treated as a clean empty-data poll. Surface the error on the
    // cursor so the hub card shows "reconnect", then propagate it as Err so
    // def_pull / def_collect report it rather than silently reporting 0 rows.
    if let Some(err_msg) = api_err {
        state.error = err_msg.clone();
        // Preserve prior station/updated; only record that the last attempt failed.
        vault.write_waqi_sync(&state)?;
        bail!("{err_msg}");
    }

    // Defect #2: derive raw_ts from the data object's time.iso when readings
    // is empty (stale station), so the raw partition key matches the
    // observation's month rather than the poll wall-clock month.
    // Fall back to now() only when the raw object itself has no usable timestamp.
    let raw_ts = readings.first().map(|r| r.ts.clone()).unwrap_or_else(|| {
        raw.as_ref()
            .and_then(|v| v.get("time"))
            .and_then(|t| t.get("iso"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| to_local(s))
            .unwrap_or_else(|| now.to_rfc3339())
    });

    // Raw layer — unconditional (full fidelity even on stale).
    if let Some(raw_obj) = &raw {
        upsert_raw(vault, raw_obj, &raw_ts)?;
    }

    let written = if readings.is_empty() {
        0u64
    } else {
        upsert_readings(vault, &readings)?
    };

    // Advance cursor.
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    // Defect #3: only overwrite station when we actually got new readings
    // (preserve last-known-good station across transient stale polls).
    if let Some(first) = readings.first() {
        state.station = first.place.clone();
    }
    // Defect #3: only clear error on a genuine success (readings written).
    // A stale poll (0 readings, status=ok, aqi=-1) is not an error, but we
    // also must not clear a prior token/network error that hasn't been resolved.
    if written > 0 {
        state.error = String::new();
    }
    vault.write_waqi_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("WAQI synced — {written} readings"),
        counts: BTreeMap::from([("readings", written)]),
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
            std::env::temp_dir().join(format!("trove-waqi-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — modeled on the confirmed live demo response shape
    // (api.waqi.info/feed/geo:LAT;LON/?token=demo, 2026-06-16).

    /// A full WAQI feed response with AQI=59 and multiple iaqi sub-indices.
    fn feed_ok() -> Value {
        json!({
            "status": "ok",
            "data": {
                "aqi": 59,
                "idx": 1437,
                "attributions": [
                    {"url": "https://sthj.sh.gov.cn/", "name": "Shanghai Environment Monitoring Center"},
                    {"url": "https://waqi.info/", "name": "World Air Quality Index Project"}
                ],
                "city": {
                    "geo": [31.2047372, 121.4489017],
                    "name": "Shanghai",
                    "url": "https://aqicn.org/city/shanghai",
                    "location": ""
                },
                "dominentpol": "pm25",
                "iaqi": {
                    "co":  {"v": 4.6},
                    "h":   {"v": 83},
                    "no2": {"v": 6},
                    "o3":  {"v": 34.6},
                    "p":   {"v": 1008},
                    "pm10":{"v": 21},
                    "pm25":{"v": 59},
                    "so2": {"v": 4.1},
                    "t":   {"v": 24},
                    "w":   {"v": 5.6}
                },
                "time": {
                    "s":   "2026-06-16 16:00:00",
                    "tz":  "+08:00",
                    "v":   1781625600,
                    "iso": "2026-06-16T16:00:00+08:00"
                },
                "forecast": {
                    "daily": {
                        "pm25": [
                            {"avg": 118, "day": "2026-06-15", "max": 160, "min": 87},
                            {"avg": 167, "day": "2026-06-16", "max": 208, "min": 141}
                        ]
                    }
                },
                "debug": {"sync": "2026-06-16T17:44:43+09:00"}
            }
        })
    }

    /// A stale-station feed: aqi == -1.
    fn feed_stale() -> Value {
        json!({
            "status": "ok",
            "data": {
                "aqi": -1,
                "idx": 9999,
                "attributions": [],
                "city": {"geo": [0.0, 0.0], "name": "Stale Station", "url": "", "location": ""},
                "dominentpol": "",
                "iaqi": {},
                "time": {
                    "s": "2026-06-16 12:00:00",
                    "tz": "+00:00",
                    "v": 1781611200,
                    "iso": "2026-06-16T12:00:00+00:00"
                }
            }
        })
    }

    /// A status != "ok" response (e.g. invalid token).
    fn feed_error() -> Value {
        json!({"status": "error", "data": "Invalid key"})
    }

    /// A response with no `time.iso` but a valid `time.v` epoch.
    fn feed_no_iso() -> Value {
        json!({
            "status": "ok",
            "data": {
                "aqi": 42,
                "idx": 777,
                "attributions": [],
                "city": {"geo": [34.05, -118.24], "name": "LA Station", "url": "", "location": ""},
                "dominentpol": "pm25",
                "iaqi": {"pm25": {"v": 42}},
                "time": {
                    "s": "2026-06-16 08:00:00",
                    "tz": "-07:00",
                    "v": 1781600400,
                    "iso": ""
                }
            }
        })
    }

    // A stub injector for offline tests.
    struct Stub(Value);
    impl WaqiApi for Stub {
        fn feed(&self, _lat: f64, _lon: f64, _token: &str) -> Result<Value> {
            Ok(self.0.clone())
        }
    }

    const SEED: (f64, f64) = (34.05, -118.24);
    const TOKEN: &str = "test-token";

    // -----------------------------------------------------------------------
    // Pure parser tests.

    #[test]
    fn parse_ok_emits_aqi_plus_iaqi_readings() {
        let (rows, raw, api_err) = parse_feed(&feed_ok());
        assert!(api_err.is_none(), "ok response must not set api_err");
        // 1 AQI + 10 iaqi keys = 11 readings.
        assert_eq!(rows.len(), 11, "aqi + 10 iaqi sub-indices");
        assert!(raw.is_some(), "raw always set on status=ok");

        let aqi_row = rows.iter().find(|r| r.metric == "aqi").unwrap();
        assert_eq!(aqi_row.value, 59.0);
        assert_eq!(aqi_row.unit, "aqi");
        assert_eq!(aqi_row.source, "waqi");
        assert_eq!(aqi_row.place, "Shanghai");
        assert_eq!(aqi_row.station, "1437");
        assert_eq!(aqi_row.lat, Some(31.2047372));
        assert_eq!(aqi_row.lon, Some(121.4489017));
        assert!(aqi_row.guid.as_deref().unwrap().starts_with("waqi:1437:aqi:"));
        assert_eq!(
            aqi_row.extra.get("dominant_pollutant"),
            Some(&json!("pm25"))
        );

        // pm25 sub-index.
        let pm25 = rows.iter().find(|r| r.metric == "pm25").unwrap();
        assert_eq!(pm25.value, 59.0);
        assert_eq!(pm25.unit, "aqi");
        assert!(pm25.guid.as_deref().unwrap().starts_with("waqi:1437:pm25:"));

        // o3 maps to "ozone".
        assert!(rows.iter().any(|r| r.metric == "ozone"), "o3 → ozone");

        // t maps to "temperature".
        let temp = rows.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(temp.value, 24.0);
        assert_eq!(temp.unit, "C");

        // h maps to "humidity".
        let hum = rows.iter().find(|r| r.metric == "humidity").unwrap();
        assert_eq!(hum.value, 83.0);
        assert_eq!(hum.unit, "percent");

        // p maps to "pressure".
        let pres = rows.iter().find(|r| r.metric == "pressure").unwrap();
        assert_eq!(pres.value, 1008.0);
        assert_eq!(pres.unit, "hpa");

        // w maps to "wind_speed".
        let wind = rows.iter().find(|r| r.metric == "wind_speed").unwrap();
        assert_eq!(wind.value, 5.6);
        assert_eq!(wind.unit, "m/s");
    }

    #[test]
    fn parse_stale_returns_no_readings_but_keeps_raw() {
        let (rows, raw, api_err) = parse_feed(&feed_stale());
        assert!(rows.is_empty(), "stale station (aqi=-1) → no contract rows");
        assert!(raw.is_some(), "raw always preserved even when stale");
        assert!(api_err.is_none(), "stale (aqi=-1) is not an API error");
    }

    #[test]
    fn parse_error_status_returns_empty_with_error_signal() {
        let (rows, raw, api_err) = parse_feed(&feed_error());
        assert!(rows.is_empty(), "non-ok status → no rows");
        assert!(raw.is_none(), "non-ok status → no raw");
        // The API error message must be surfaced so the caller can flag reconnect.
        let err = api_err.expect("parse_feed must return Some(err) on status=error");
        assert!(err.contains("Invalid key"), "error message forwarded: {err}");
    }

    #[test]
    fn pull_error_response_records_error_and_returns_err() {
        // A present-but-invalid token returns HTTP 200 with {"status":"error","data":"Invalid key"}.
        // pull_at_with must NOT treat this as a clean 0-reading poll; it must:
        // 1. set state.error so the hub card shows reconnect
        // 2. preserve the prior station (not clobber it)
        // 3. return Err so def_pull / def_collect report the problem
        let v = temp_vault("err-response");
        // Pre-seed a known station in the cursor.
        v.write_waqi_sync(&WaqiSyncState {
            updated: "2026-06-16T10:00:00+00:00".into(),
            lat: SEED.0,
            lon: SEED.1,
            station: "Shanghai".into(),
            error: String::new(),
        })
        .unwrap();

        let result = pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_error()));
        assert!(result.is_err(), "API error must propagate as Err, not Ok(0 readings)");

        // Cursor must record the error message.
        let state = v.read_waqi_sync().unwrap();
        assert!(!state.error.is_empty(), "state.error must be set on API error");
        assert!(
            state.error.contains("Invalid key"),
            "error message forwarded to cursor: {}",
            state.error
        );
        // Prior station must be preserved (not wiped to empty string).
        assert_eq!(state.station, "Shanghai", "station preserved after API error");
        // No contract or raw files should be written.
        assert!(
            !v.root().join("environment/waqi").exists(),
            "no vault files written on API error"
        );
    }

    #[test]
    fn guid_contains_idx_and_metric_and_iso_ts() {
        let (rows, _, _) = parse_feed(&feed_ok());
        for r in &rows {
            let g = r.guid.as_deref().unwrap_or("");
            assert!(g.starts_with("waqi:1437:"), "guid prefix: {g}");
            assert!(g.len() > "waqi:1437:aqi:".len(), "guid includes ts: {g}");
        }
    }

    #[test]
    fn ts_converted_to_local_rfc3339() {
        let (rows, _, _) = parse_feed(&feed_ok());
        for r in &rows {
            let dt = DateTime::parse_from_rfc3339(&r.ts);
            assert!(dt.is_ok(), "ts is valid RFC3339: {:?}", r.ts);
        }
    }

    #[test]
    fn feed_no_iso_falls_back_to_epoch() {
        // When time.iso is empty, the collector falls back to time.v (epoch).
        let (rows, _, _) = parse_feed(&feed_no_iso());
        // aqi + pm25 = 2.
        assert_eq!(rows.len(), 2, "aqi + pm25");
        let aqi = rows.iter().find(|r| r.metric == "aqi").unwrap();
        assert_eq!(aqi.value, 42.0);
        // The ts should be a valid RFC3339 derived from epoch 1781600400.
        let dt = DateTime::parse_from_rfc3339(&aqi.ts).unwrap();
        assert_eq!(dt.timestamp(), 1781600400, "epoch fallback ts");
    }

    // -----------------------------------------------------------------------
    // Pull / store / dedupe tests.

    #[test]
    fn pull_writes_contract_readings_and_raw() {
        let v = temp_vault("pull");
        let out = pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_ok())).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&11));

        // Contract layer: environment/waqi/2026-06.jsonl.
        let content =
            std::fs::read_to_string(v.root().join("environment/waqi/2026-06.jsonl")).unwrap();
        assert_eq!(content.lines().count(), 11);
        assert!(content.contains("\"metric\":\"aqi\""));
        assert!(content.contains("\"metric\":\"pm25\""));
        assert!(content.contains("\"metric\":\"ozone\""));
        assert!(content.contains("\"metric\":\"temperature\""));

        // Raw layer: environment/waqi/raw/2026-06.jsonl.
        let raw =
            std::fs::read_to_string(v.root().join("environment/waqi/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "one raw object per poll");
        // Full fidelity: forecast data preserved in raw.
        assert!(raw.contains("\"forecast\""), "forecast preserved in raw");
        assert!(raw.contains("\"debug\""), "debug block preserved in raw");

        // Cursor advanced.
        let state = v.read_waqi_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.lat, SEED.0);
        assert_eq!(state.lon, SEED.1);
        assert_eq!(state.station, "Shanghai");
        assert!(state.error.is_empty());
    }

    #[test]
    fn stale_poll_writes_raw_only_no_contract_rows() {
        let v = temp_vault("stale");
        let out = pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_stale())).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        // No contract file.
        assert!(!v.root().join("environment/waqi").exists()
            || !v.root().join("environment/waqi/2026-06.jsonl").exists(),
            "stale poll must not write contract rows");
        // Raw file must exist (even for stale).
        assert!(v.root().join("environment/waqi/raw").exists(), "raw always written");
    }

    #[test]
    fn repoll_upserts_not_duplicates() {
        let v = temp_vault("dedup");
        // First poll.
        pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_ok())).unwrap();
        let rows_1: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows_1.len(), 11);

        // Second identical poll: same guids → upsert in place, no duplicates.
        pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_ok())).unwrap();
        let rows_2: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows_2.len(), 11, "re-poll must not duplicate rows");

        // Raw also deduped.
        let raw: Vec<Value> =
            v.stream(RAW_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(raw.len(), 1, "raw deduped by idx:iso");
    }

    #[test]
    fn no_location_is_inert() {
        let v = temp_vault("no-loc");
        let out = pull_at_with(&v, None, TOKEN, &Stub(feed_ok())).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/waqi").exists());
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("sync");
        v.write_waqi_sync(&WaqiSyncState {
            updated: "2026-06-16T16:00:00+08:00".into(),
            lat: 31.2,
            lon: 121.4,
            station: "Shanghai".into(),
            error: String::new(),
        })
        .unwrap();
        let s = v.read_waqi_sync().unwrap();
        assert_eq!(s.lat, 31.2);
        assert_eq!(s.station, "Shanghai");
        assert!(s.error.is_empty());
    }

    #[test]
    fn sync_state_back_compat_empty_object() {
        // A brand-new / missing cursor deserializes safely with all defaults.
        let empty: WaqiSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.lat, 0.0);
        assert!(empty.updated.is_empty());
        assert!(empty.station.is_empty());
    }

    #[test]
    fn stale_poll_preserves_prior_station_name() {
        // Defect #3: a transient stale poll (aqi=-1) must not wipe the last-known
        // station name that was set by a previous successful poll.
        let v = temp_vault("stale-station");
        // Simulate a prior successful poll that resolved "Shanghai".
        pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_ok())).unwrap();
        let after_ok = v.read_waqi_sync().unwrap();
        assert_eq!(after_ok.station, "Shanghai", "station set after good poll");
        assert!(after_ok.error.is_empty(), "no error after good poll");

        // Now a stale poll arrives.
        pull_at_with(&v, Some(SEED), TOKEN, &Stub(feed_stale())).unwrap();
        let after_stale = v.read_waqi_sync().unwrap();
        // Station should be preserved (not wiped to empty).
        assert_eq!(
            after_stale.station, "Shanghai",
            "stale poll must not wipe last-known-good station"
        );
        // A transient stale is not an error condition.
        assert!(
            after_stale.error.is_empty(),
            "stale poll must not set error: {:?}",
            after_stale.error
        );
    }

    #[test]
    fn over_quota_error_is_surfaced_not_silently_swallowed() {
        // WAQI returns HTTP 200 with {"status":"error","data":"Over quota"} for
        // an exhausted token. This must propagate as Err, never as Ok(0 readings).
        let over_quota = json!({"status": "error", "data": "Over quota"});
        let v = temp_vault("over-quota");
        let result = pull_at_with(&v, Some(SEED), TOKEN, &Stub(over_quota));
        assert!(result.is_err(), "over-quota must propagate as Err");
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("Over quota"), "error forwarded: {err_str}");
        let state = v.read_waqi_sync().unwrap();
        assert!(state.error.contains("Over quota"), "cursor flags the quota error");
    }

    #[test]
    fn old_contract_reading_still_deserializes() {
        // Serde back-compat: a minimal sparse row (only the 4 required fields)
        // from an older build must parse without error.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/waqi")).unwrap();
        std::fs::write(
            v.root().join("environment/waqi/2026-06.jsonl"),
            "{\"ts\":\"2026-06-16T16:00:00+08:00\",\"source\":\"waqi\",\"metric\":\"aqi\",\"value\":59}\n",
        )
        .unwrap();
        let rows: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, 59.0);
        assert_eq!(rows[0].guid, None, "sparse row has no guid — tolerated");
    }
}
