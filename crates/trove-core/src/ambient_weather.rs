//! Ambient Weather personal weather stations via the official REST API.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/ambient-weather.md.
//! **First collector in the `home` domain** — this build binds the home
//! reading contract (see [`crate::home`] / [`crate::contracts`]).
//!
//! A **Periodic** cloud pull over two endpoints, against `rt.ambientweather.net`
//! (the REST subdomain), authenticated by **two** keys — an `apiKey` (grants
//! access to a user's device data) and an `applicationKey` (identifies the app).
//! Both are generated on the user's ambientweather.net account page; a
//! single-field token form would fail confusingly, so the connect card asks for
//! both as one `apiKey:applicationKey` string the pull splits.
//!
//! - `GET /v1/devices` → the user's stations, each `{macAddress, info:{name,
//!   location, coords}, lastData}`. Drives discovery (which stations to walk)
//!   and supplies each device's place/coords.
//! - `GET /v1/devices/{macAddress}?endDate=&limit=288` → that device's
//!   observations, **newest-first, descending from `endDate`**, ≤288 rows/page
//!   (5- or 30-minute increments). We walk *backward* — page 1 with no
//!   `endDate` (newest), then `endDate = oldest dateutc seen − 1ms` — until a
//!   page returns nothing newer than the device's watermark (incremental) or an
//!   empty page (first-sync backfill reached the 1-year retention horizon).
//!
//! Each observation fans out into **one [`crate::home::HomeReading`] per
//! sensor metric** (tempf → `temperature`/`F`, humidity → `humidity`/`percent`,
//! windspeedmph → `wind_speed`/`mph`, …) under `home/ambient-weather/YYYY-MM.jsonl`
//! (`ts` = the row's `date`, ISO UTC → local; `guid` =
//! `ambient-weather:{mac}:{metric}:{dateutc}`, the stable per-device-metric-time
//! key; `device` = the MAC, `place` = the station name, `lat`/`lon` from the
//! device's coords). The verbatim API object rides alongside under
//! `home/ambient-weather/raw/YYYY-MM.jsonl` (full fidelity, unconditional).
//!
//! Per-device watermarks (max `dateutc` ever written) live in a rebuildable
//! cursor at `.trove/ambient-weather-sync.json` (non-secret, beside the vault's
//! other `.trove/` indexes — not under `.trove/sync/`). A device's watermark
//! advances only after its full backward drain, so a crash re-drains rather than
//! skips. **The 1-year cloud-retention deletion is the whole point of this
//! integration** — data older than a year is already gone at connect time, so
//! the first sync backfills the whole window promptly.
//!
//! Auth is a secret (two keys): pasted via [`ConnectMethod::TokenPaste`], stored
//! under `.trove/sync/` (0600), verified at connect with a real `GET /v1/devices`,
//! and never logged or written to the cursor or any non-secret file.

use std::collections::{BTreeMap, HashSet};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract-layer reading stream; raw lines go one level deeper in `raw/`.
const DIR: &str = "home/ambient-weather";
const RAW_DIR: &str = "home/ambient-weather/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-walks each device's history on the next sync.
const SYNC_FILE: &str = ".trove/ambient-weather-sync.json";

/// Service id under `.trove/sync/` where the pasted `apiKey:applicationKey`
/// pair is stored (the github/oura/lastfm PAT slot: it rides a never-expiring
/// [`TokenSet`]).
const SERVICE: &str = "ambient-weather";

/// Compiled-in `applicationKey` default. Empty by default — set
/// `TROVE_AMBIENT_WEATHER_APPLICATION_KEY` to bake one in so users paste only
/// their `apiKey`. When empty, the user must paste both keys as
/// `apiKey:applicationKey`.
const BAKED_APPLICATION_KEY: &str = "";

/// REST subdomain is `rt`, not `api` (the realtime/websocket subdomain is
/// `rt2`). Confirmed against the official apiary blueprint.
const API_BASE: &str = "https://rt.ambientweather.net";
/// The data endpoint's documented per-page max.
const PAGE_LIMIT: u32 = 288;
/// Rate limit is 1 request/second per apiKey — pace page fetches to stay under.
const REQ_INTERVAL: Duration = Duration::from_millis(1100);
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on backward pages per device per sync, so a pathological history
/// (or a buggy never-shrinking `endDate`) can't loop forever. 1 year at 5-min
/// resolution is ~366 pages of 288; 600 leaves generous headroom.
const MAX_PAGES_PER_DEVICE: u32 = 600;
/// Seconds between syncs in the watcher loop. Hourly: a station reports every
/// 5–30 min and the incremental walk is a couple of cheap requests when idle.
pub const AMBIENT_WEATHER_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Metric mapping table: source field → (contract metric, unit). One reading row
// is emitted per present field. Names are snake_case and chosen to line up with
// the environment/home reading vocabulary (`temperature`, `humidity`,
// `pressure`, `uv`, …) so an indoor sensor and an outdoor feed merge by metric.

/// `(api_field, metric, unit)`. Outdoor + console fields from the documented
/// device-data shape. Indoor temp/humidity get an `_indoor` metric suffix so
/// they don't collide with the outdoor reading at the same timestamp.
const METRICS: &[(&str, &str, &str)] = &[
    ("tempf", "temperature", "F"),
    ("humidity", "humidity", "percent"),
    ("tempinf", "temperature_indoor", "F"),
    ("humidityin", "humidity_indoor", "percent"),
    ("winddir", "wind_direction", "degrees"),
    ("windspeedmph", "wind_speed", "mph"),
    ("windgustmph", "wind_gust", "mph"),
    ("maxdailygust", "wind_gust_max_daily", "mph"),
    ("baromrelin", "pressure_relative", "inHg"),
    ("baromabsin", "pressure_absolute", "inHg"),
    ("hourlyrainin", "rain_hourly", "in"),
    ("dailyrainin", "rain_daily", "in"),
    ("weeklyrainin", "rain_weekly", "in"),
    ("monthlyrainin", "rain_monthly", "in"),
    ("yearlyrainin", "rain_yearly", "in"),
    ("eventrainin", "rain_event", "in"),
    ("totalrainin", "rain_total", "in"),
    ("uv", "uv", "index"),
    ("solarradiation", "solar_radiation", "wm2"),
    ("feelsLike", "feels_like", "F"),
    ("dewPoint", "dew_point", "F"),
    ("co2", "co2", "ppm"),
    ("pm25", "pm25", "ug_m3"),
];

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing key or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("ambient weather synced — {n} readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "ambient weather sync skipped: {e}"
        ))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Ambient Weather is up to date — no new readings".to_string()
    } else {
        format!("Ambient Weather synced — {n} readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ambient-weather",
        name: "Ambient Weather",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls weather readings from your Ambient Weather personal weather station \
             via the official REST API into the unified home store. First sync backfills \
             your station's history; later syncs fetch only what's new.",
        domain: "home",
        vault_path: "home/ambient-weather/",
        toggleable: true,
        setup: &[
            "Create an API Key and an Application Key on your ambientweather.net account page.",
            "Connect with both keys (pasted as apiKey:applicationKey) on this card.",
            "First sync backfills your station's history; later syncs are incremental.",
        ],
        caveats: "Both an API Key and an Application Key are required (generated on your \
                  ambientweather.net account page). Ambient Weather deletes cloud data older \
                  than one year — anything past that horizon is already gone at connect time, so \
                  the first sync backfills the whole window. Rate-limited to 1 request/second; a \
                  large history backfills over a few minutes.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(AMBIENT_WEATHER_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("ambient-weather"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = the apiKey:applicationKey pair, a SECRET).

/// Resolve the credential pair from a pasted string. The user pastes
/// `apiKey:applicationKey`; if an applicationKey is baked in (env → compiled),
/// pasting only the apiKey works too. Returns `(api_key, application_key)`.
/// Trims whitespace; splits on the FIRST `:` only (neither key contains one).
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your API Key and Application Key as apiKey:applicationKey");
    }
    let (api_key, app_key) = match pasted.split_once(':') {
        Some((a, b)) => (a.trim().to_string(), b.trim().to_string()),
        None => (pasted.to_string(), String::new()),
    };
    // Fall back to a baked applicationKey when the user pasted only the apiKey.
    let app_key = if app_key.is_empty() { baked_application_key() } else { app_key };
    if api_key.is_empty() {
        bail!("missing API Key — paste it as apiKey:applicationKey");
    }
    if app_key.is_empty() {
        bail!(
            "missing Application Key — paste both keys as apiKey:applicationKey \
             (both are generated on your ambientweather.net account page)"
        );
    }
    Ok((api_key, app_key))
}

/// The compiled-in / env applicationKey default. Empty when neither is set.
fn baked_application_key() -> String {
    if let Ok(k) = std::env::var("TROVE_AMBIENT_WEATHER_APPLICATION_KEY") {
        let k = k.trim();
        if !k.is_empty() {
            return k.to_string();
        }
    }
    BAKED_APPLICATION_KEY.trim().to_string()
}

/// Verify the pasted keys with `GET /v1/devices` (a 200 with a device array on
/// success), then store the pasted pair (0600). A 401 bails with a clear
/// message; the keys are never logged.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (api_key, app_key) = parse_credentials(pasted)?;
    let client = AmbientClient::new(API_BASE.to_string(), api_key.clone(), app_key.clone());
    match client.devices() {
        Ok(_) => {}
        Err(FetchError::Unauthorized) => bail!(
            "Ambient Weather rejected the keys (401) — re-copy your API Key and Application Key \
             from ambientweather.net/account"
        ),
        Err(e) => bail!("Ambient Weather auth check failed: {e}"),
    }
    // Store the pasted pair verbatim (NOT the parsed halves) so a baked
    // applicationKey can still be picked up later if the user pasted only the
    // apiKey. The keys go ONLY through the secret store (0600). Never the cursor.
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: Some("AmbientKeys".into()),
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored keys. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = the key pair is stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if vault.load_sync_token(SERVICE)?.is_some() {
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "Ambient Weather".to_string(),
            connected_at: None, // the secret store doesn't record it
            expires_at: None,   // the keys don't expire
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // No bring-your-own-app step: the user self-services both keys.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single method: paste the
/// two keys as `apiKey:applicationKey`. Both are required (Ambient Weather needs
/// both on every call); a baked applicationKey lets a packaged build accept just
/// the apiKey, but the help copy asks for both so the default path always works.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "ambient-weather",
    display_name: "Ambient Weather",
    methods: &[ConnectMethod::TokenPaste {
        label: "API Key and Application Key",
        help: "Paste both keys as apiKey:applicationKey — create them on your \
               ambientweather.net account page. Both are required; they're stored locally and \
               sent only to Ambient Weather.",
        placeholder: "abcd…apiKey:wxyz…applicationKey",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["ambient-weather"],
    setup: &[
        "Sign in to ambientweather.net and open your account page.",
        "Create an API Key (grants access to your devices) and an Application Key.",
        "Paste them here as apiKey:applicationKey — both are required.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors: 401 wants distinct handling at connect, 429 is
/// the rate limit (transient), everything else is a message.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The two endpoints the pull needs. A trait so tests drive the mapping/cursor
/// logic with fixtures, never the network.
trait AmbientApi {
    /// `GET /v1/devices` → the device array.
    fn devices(&self) -> Result<Vec<Value>, FetchError>;
    /// `GET /v1/devices/{mac}` → one descending page of ≤`limit` observations,
    /// ending at `end_date` (epoch ms, exclusive upper bound) when given.
    fn device_data(
        &self,
        mac: &str,
        end_date: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Value>, FetchError>;
}

/// Thin client; base URL injected (the github/oura/lastfm pattern).
struct AmbientClient {
    base: String,
    api_key: String,
    app_key: String,
}

impl AmbientClient {
    fn new(base: String, api_key: String, app_key: String) -> Self {
        AmbientClient { base, api_key, app_key }
    }

    /// Shared GET → a JSON array, mapping status codes onto [`FetchError`].
    fn get_array(&self, url: &str, extra: &[(&str, String)]) -> Result<Vec<Value>, FetchError> {
        let mut req = ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .query("apiKey", &self.api_key)
            .query("applicationKey", &self.app_key);
        for (k, v) in extra {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                match v {
                    Value::Array(a) => Ok(a),
                    // Defensive: a bare object (or anything else) → empty page.
                    _ => Ok(Vec::new()),
                }
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
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

impl AmbientApi for AmbientClient {
    fn devices(&self) -> Result<Vec<Value>, FetchError> {
        self.get_array(&format!("{}/v1/devices", self.base), &[])
    }

    fn device_data(
        &self,
        mac: &str,
        end_date: Option<i64>,
        limit: u32,
    ) -> Result<Vec<Value>, FetchError> {
        let mut extra: Vec<(&str, String)> = vec![("limit", limit.to_string())];
        if let Some(ed) = end_date {
            extra.push(("endDate", ed.to_string()));
        }
        // The MAC contains ':' — encode it into the path segment.
        let mac_enc = mac.replace(':', "%3A");
        self.get_array(&format!("{}/v1/devices/{mac_enc}", self.base), &extra)
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Per-device (MAC → max `dateutc` ever written, epoch ms) watermark. The
    /// next incremental walk stops descending once it passes this.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    watermarks: BTreeMap<String, i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_ambient_weather_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ambient_weather_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity API object). The on-disk line is the verbatim
// observation object, tagged with the contract ts purely so the month-partition
// writer files it under the right month. Only `value` is serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A station's identity, pulled once from the device-list entry and stamped
/// onto every reading it produces.
#[derive(Clone, Default)]
struct DeviceInfo {
    mac: String,
    name: String,
    lat: Option<f64>,
    lon: Option<f64>,
}

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Pull the [`DeviceInfo`] out of one `/v1/devices` entry. `info.coords.coords`
/// nests `{lat, lon}` (the documented shape); `info.name` / `info.location`
/// give the place label. `None` when there's no MAC (can't key anything).
fn device_info(d: &Value) -> Option<DeviceInfo> {
    let mac = str_field(d, "macAddress");
    if mac.is_empty() {
        return None;
    }
    let info = d.get("info");
    let name = info
        .map(|i| {
            let n = str_field(i, "name");
            if n.is_empty() { str_field(i, "location") } else { n }
        })
        .unwrap_or_default();
    // info.coords.coords.{lat,lon}
    let coords = info.and_then(|i| i.get("coords")).and_then(|c| c.get("coords"));
    let lat = coords.and_then(|c| c.get("lat")).and_then(Value::as_f64);
    let lon = coords.and_then(|c| c.get("lon")).and_then(Value::as_f64);
    Some(DeviceInfo { mac, name, lat, lon })
}

/// The observation's `dateutc` (epoch ms) — the dedupe/watermark key. `None`
/// when absent or non-numeric (can't key or partition the row).
fn dateutc_of(o: &Value) -> Option<i64> {
    o.get("dateutc").and_then(Value::as_i64)
}

/// The observation's local `ts` (RFC3339). Prefer the ISO `date` (UTC) → local;
/// fall back to `dateutc` (epoch ms) → local. `None` when neither is usable.
fn obs_ts(o: &Value) -> Option<String> {
    if let Some(date) = o.get("date").and_then(Value::as_str) {
        if let Ok(t) = DateTime::parse_from_rfc3339(date.trim()) {
            return Some(t.with_timezone(&Local).to_rfc3339());
        }
    }
    let ms = dateutc_of(o)?;
    Some(DateTime::from_timestamp_millis(ms)?.with_timezone(&Local).to_rfc3339())
}

/// One observation → its `HomeReading` rows, one per present numeric metric.
/// `dev` stamps device/place/coords; `dateutc` keys the stable per-metric guid.
/// An observation with no usable timestamp yields nothing (can't partition).
fn readings_from(o: &Value, dev: &DeviceInfo) -> Vec<HomeReading> {
    let Some(ts) = obs_ts(o) else {
        return Vec::new();
    };
    // The partition writer needs a month key; a ts that can't yield one would
    // fail the whole append, so drop the row here instead.
    if Partition::Month.key(&ts).is_none() {
        return Vec::new();
    }
    let Some(dateutc) = dateutc_of(o) else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    for (field, metric, unit) in METRICS {
        let Some(value) = o.get(*field).and_then(Value::as_f64) else {
            continue;
        };
        let mut r = HomeReading::new("ambient-weather", *metric, value, ts.clone());
        if !unit.is_empty() {
            r.unit = (*unit).to_string();
        }
        r.place = dev.name.clone();
        r.device = dev.mac.clone();
        r.lat = dev.lat;
        r.lon = dev.lon;
        // Stable, source-unique dedupe key. `home.reading` has no `guid` column
        // (readings have a natural key), so it rides in `extra` — the dedupe key
        // the re-runnable writer skips on, matching the environment-reading idiom
        // of keying on device + metric + ts.
        let mut extra = Map::new();
        extra.insert(
            "guid".into(),
            Value::String(format!("ambient-weather:{}:{metric}:{dateutc}", dev.mac)),
        );
        r.extra = extra;
        rows.push(r);
    }
    rows
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by the per-reading guid (in extra) against
// what's already on disk.

/// The dedupe key carried in a reading's `extra.guid`. Empty when absent.
fn reading_guid(v: &Value) -> String {
    v.get("extra")
        .and_then(|e| e.get("guid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Append new contract + raw rows for one device's drain, deduped by the
/// per-reading guid. Returns the number of new contract rows written. Raw lines
/// (one per observation) partition by the same month as the observation's ts.
fn write_layer(
    vault: &Vault,
    rows: Vec<HomeReading>,
    raws: Vec<(String, Value)>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing reading guids in the contract stream — re-runnable: a re-pull of
    // an overlapping window never duplicates (the readwise/lastfm pattern).
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = reading_guid(&v);
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<HomeReading> = Vec::new();
    for row in rows {
        let guid = row
            .extra
            .get("guid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if guid.is_empty() || !seen.insert(guid) {
            continue; // no key, or already stored
        }
        new_rows.push(row);
    }

    // Raw lines are per-observation (not per-reading). Dedupe them on
    // device+dateutc so a re-walk doesn't duplicate the raw object either.
    let mut new_raws: Vec<RawLine> = Vec::new();
    let mut seen_raw: HashSet<String> = HashSet::new();
    // Existing raw observations keyed by their dateutc+macAddress.
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
            if let Some(k) = raw_key(&v) {
                seen_raw.insert(k);
            }
        }
    }
    for (ts, raw_val) in raws {
        if let Some(k) = raw_key(&raw_val) {
            if !seen_raw.insert(k) {
                continue;
            }
        }
        new_raws.push(RawLine { ts, value: raw_val });
    }

    contract.append(&new_rows, |r| &r.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

/// A raw observation's dedupe key: `{macAddress}:{dateutc}`. `None` when it has
/// no dateutc (un-keyable — kept, never deduped).
fn raw_key(v: &Value) -> Option<String> {
    let ms = dateutc_of(v)?;
    let mac = str_field(v, "macAddress");
    Some(format!("{mac}:{ms}"))
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the credentials and sync. Missing keys ⇒ a quiet skip on the
/// periodic path (mirror lastfm/readwise), a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Ambient Weather is not connected — add your API and Application keys in the Integrations tab")?;
    let (api_key, app_key) = parse_credentials(&pasted)?;
    let client = AmbientClient::new(API_BASE.to_string(), api_key, app_key);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam. For each device:
/// walk its history backward (descending `endDate`) until a page returns
/// nothing new, then advance the device watermark only after the full drain.
fn pull_with(vault: &Vault, api: &impl AmbientApi) -> Result<PullOutcome> {
    let mut state = vault.read_ambient_weather_sync();

    let devices = api.devices().map_err(|e| fetch_err("devices", e))?;
    let infos: Vec<(DeviceInfo, Value)> = devices
        .into_iter()
        .filter_map(|d| device_info(&d).map(|i| (i, d)))
        .collect();

    let mut total_written: u64 = 0;
    // Store the device-list objects verbatim under raw/ too (full fidelity of
    // the discovery response), keyed by the device's lastData ts when present.
    let mut device_raws: Vec<RawLine> = Vec::new();
    for (info, raw_dev) in &infos {
        // Best-effort ts for partitioning the device snapshot: its lastData.
        if let Some(ts) = raw_dev
            .get("lastData")
            .and_then(obs_ts_owned)
            .filter(|t| Partition::Month.key(t).is_some())
        {
            device_raws.push(RawLine { ts, value: raw_dev.clone() });
        }
        let _ = info;
    }

    for (info, _raw_dev) in &infos {
        let prior = state.watermarks.get(&info.mac).copied();
        let (rows, raws, new_watermark) = drain_device(api, info, prior)?;
        let written = write_layer(vault, rows, raws)?;
        total_written += written;
        // Advance the device watermark only after the full backward drain, and
        // only forward — a crash mid-drain re-walks rather than skipping a gap.
        if let Some(w) = new_watermark {
            let entry = state.watermarks.entry(info.mac.clone()).or_insert(w);
            if w > *entry {
                *entry = w;
            }
        }
    }

    // Device-list snapshots into raw/ (deduped only by the contract guids, which
    // these don't carry — so append unconditionally; they're a discovery audit
    // trail, low-volume). Partition by their own lastData month.
    if !device_raws.is_empty() {
        let raw = vault.stream(&format!("{RAW_DIR}/devices"), Partition::Month);
        raw.append(&device_raws, |r| &r.ts)?;
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_ambient_weather_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_written} readings"),
        counts: BTreeMap::from([("readings", total_written)]),
    })
}

/// `obs_ts` on an owned borrowed value (helper for `Option::and_then`).
fn obs_ts_owned(v: &Value) -> Option<String> {
    obs_ts(v)
}

/// Walk one device's history backward and map every observation strictly newer
/// than `prior` (the device watermark, epoch ms) into readings. Returns
/// `(contract rows, (ts, raw observation) pairs, new max dateutc seen)`.
///
/// Paging: page 1 has no `endDate` (newest first); each subsequent page sets
/// `endDate = oldest dateutc on the last page − 1` (ms), descending. Stops when
/// a page is empty, when every row on a page is ≤ `prior` (incremental caught
/// up), or at [`MAX_PAGES_PER_DEVICE`] (safety bound). A page that doesn't
/// shrink `endDate` (all rows un-keyable, or a server that ignores `endDate`)
/// also stops the loop, so it can't spin forever.
fn drain_device(
    api: &impl AmbientApi,
    info: &DeviceInfo,
    prior: Option<i64>,
) -> Result<(Vec<HomeReading>, Vec<(String, Value)>, Option<i64>)> {
    let mut rows: Vec<HomeReading> = Vec::new();
    let mut raws: Vec<(String, Value)> = Vec::new();
    let mut max_seen: Option<i64> = prior;
    let mut end_date: Option<i64> = None;
    let mut last_end: Option<i64> = None;

    for page_n in 0..MAX_PAGES_PER_DEVICE {
        let page = match api.device_data(&info.mac, end_date, PAGE_LIMIT) {
            Ok(p) => p,
            Err(FetchError::RateLimited) => {
                // Back off once and retry the same page; the watcher picks up
                // any remainder next tick.
                thread::sleep(Duration::from_secs(2));
                api.device_data(&info.mac, end_date, PAGE_LIMIT)
                    .map_err(|e| fetch_err("device data", e))?
            }
            Err(e) => return Err(fetch_err("device data", e)),
        };
        if page.is_empty() {
            break; // backfill reached the retention horizon (or no data)
        }

        let mut page_min: Option<i64> = None;
        let mut any_new = false;
        for o in &page {
            let Some(ms) = dateutc_of(o) else { continue };
            page_min = Some(page_min.map_or(ms, |m: i64| m.min(ms)));
            max_seen = Some(max_seen.map_or(ms, |m| m.max(ms)));
            // Strictly newer than the watermark → a new observation.
            if prior.is_none_or(|p| ms > p) {
                any_new = true;
                for r in readings_from(o, info) {
                    rows.push(r);
                }
                if let Some(ts) = obs_ts(o) {
                    // History rows from /v1/devices/{mac}/data carry no
                    // `macAddress` of their own (only device-list lastData /
                    // realtime events do); stamp it on so the raw object is
                    // self-describing and its dedupe key is unique across
                    // devices that report the same instant. Don't clobber a mac
                    // the payload already carries.
                    let mut raw_obj = o.clone();
                    if let Value::Object(map) = &mut raw_obj {
                        map.entry("macAddress")
                            .or_insert_with(|| Value::String(info.mac.clone()));
                    }
                    raws.push((ts, raw_obj));
                }
            }
        }

        // Incremental: once a whole page is at/below the watermark, we've caught
        // up — older pages are entirely already-stored, so stop descending.
        if prior.is_some() && !any_new {
            break;
        }

        // Descend: next page ends just before this page's oldest row.
        let Some(pm) = page_min else { break };
        let next_end = pm - 1;
        // Guard against a non-shrinking cursor (server ignored endDate, or a
        // single repeated timestamp) — would otherwise loop until the page cap.
        if last_end == Some(next_end) {
            break;
        }
        // A short page (fewer than the limit) is the last page of history.
        if (page.len() as u32) < PAGE_LIMIT {
            // Still process this page (done above), then stop.
            let _ = page_n;
            break;
        }
        last_end = Some(next_end);
        end_date = Some(next_end);
        thread::sleep(REQ_INTERVAL); // 1 req/s rate limit
    }

    Ok((rows, raws, max_seen))
}

/// Map a [`FetchError`] at the top of an endpoint into an anyhow error with a
/// clear reconnect message for 401.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Ambient Weather rejected the keys (401) on the {endpoint} endpoint — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Ambient Weather rate limited the {endpoint} endpoint (429) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Ambient Weather {endpoint} fetch failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-ambient-weather-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the documented apiary device-list + device-data shapes) ---

    /// A `/v1/devices` entry: one station with name, location, nested coords,
    /// and a lastData block. Modeled field-for-field on the official apiary
    /// blueprint (rt.ambientweather.net), with coords filled to a real place.
    fn device_entry() -> Value {
        json!({
            "macAddress": "00:0E:C6:00:00:00",
            "info": {
                "name": "My Weather Station",
                "location": "Home",
                "coords": {
                    "coords": { "lat": 34.05, "lon": -118.25 },
                    "geo": { "type": "Point", "coordinates": [-118.25, 34.05] }
                }
            },
            "lastData": observation(1515436500000, "2018-01-08T18:35:00.000Z")
        })
    }

    /// One observation row at a given epoch-ms / ISO date. The exact field set
    /// (and casing — feelsLike/dewPoint are camelCase) from the blueprint.
    fn observation(dateutc: i64, date: &str) -> Value {
        json!({
            "dateutc": dateutc,
            "date": date,
            "winddir": 58,
            "windspeedmph": 0.9,
            "windgustmph": 4,
            "maxdailygust": 5,
            "windgustdir": 61,
            "winddir_avg2m": 63,
            "windspdmph_avg2m": 0.9,
            "winddir_avg10m": 58,
            "windspdmph_avg10m": 0.9,
            "tempf": 66.9,
            "humidity": 30,
            "baromrelin": 30.05,
            "baromabsin": 28.71,
            "tempinf": 74.1,
            "humidityin": 30,
            "hourlyrainin": 0,
            "dailyrainin": 0,
            "monthlyrainin": 0,
            "yearlyrainin": 0,
            "feelsLike": 66.9,
            "dewPoint": 34.45380707462477
        })
    }

    // --- pure mapping tests ----------------------------------------------

    #[test]
    fn device_info_reads_mac_name_and_nested_coords() {
        let d = device_entry();
        let info = device_info(&d).unwrap();
        assert_eq!(info.mac, "00:0E:C6:00:00:00");
        assert_eq!(info.name, "My Weather Station", "info.name preferred over location");
        assert_eq!(info.lat, Some(34.05), "info.coords.coords.lat");
        assert_eq!(info.lon, Some(-118.25), "info.coords.coords.lon");
        // A device with no MAC yields nothing (can't key it).
        assert!(device_info(&json!({"info": {"name": "x"}})).is_none());
        // location is the name fallback.
        let no_name = json!({"macAddress": "AA", "info": {"location": "Cabin"}});
        assert_eq!(device_info(&no_name).unwrap().name, "Cabin");
    }

    #[test]
    fn observation_fans_out_into_one_reading_per_metric() {
        let dev = device_info(&device_entry()).unwrap();
        let o = observation(1515436500000, "2018-01-08T18:35:00.000Z");
        let rows = readings_from(&o, &dev);

        // One row per mapped field present (the wind-avg fields are NOT mapped).
        let by_metric: BTreeMap<&str, &HomeReading> =
            rows.iter().map(|r| (r.metric.as_str(), r)).collect();
        assert!(by_metric.contains_key("temperature"));
        assert!(by_metric.contains_key("temperature_indoor"));
        assert!(by_metric.contains_key("humidity"));
        assert!(by_metric.contains_key("wind_speed"));
        assert!(by_metric.contains_key("pressure_relative"));
        assert!(by_metric.contains_key("feels_like"));
        assert!(by_metric.contains_key("dew_point"));
        // Un-mapped overflow fields produce no rows.
        assert!(!by_metric.contains_key("windspdmph_avg2m"));
        assert!(!by_metric.contains_key("winddir_avg10m"));

        let temp = by_metric["temperature"];
        assert_eq!(temp.source, "ambient-weather");
        assert_eq!(temp.value, 66.9);
        assert_eq!(temp.unit, "F");
        assert_eq!(temp.place, "My Weather Station");
        assert_eq!(temp.device, "00:0E:C6:00:00:00");
        assert_eq!(temp.lat, Some(34.05));
        assert_eq!(temp.lon, Some(-118.25));
        // ts = the ISO `date` in local time (same instant as the source UTC).
        assert_eq!(
            DateTime::parse_from_rfc3339(&temp.ts).unwrap().timestamp_millis(),
            1515436500000
        );
        // Stable per-device-metric-time guid in extra (the dedupe key).
        assert_eq!(
            temp.extra.get("guid").and_then(Value::as_str),
            Some("ambient-weather:00:0E:C6:00:00:00:temperature:1515436500000")
        );
        // Indoor temp gets a distinct metric so it doesn't collide with outdoor.
        assert_eq!(by_metric["temperature_indoor"].value, 74.1);
        assert_eq!(by_metric["temperature_indoor"].unit, "F");
    }

    #[test]
    fn observation_without_a_timestamp_yields_no_rows() {
        let dev = device_info(&device_entry()).unwrap();
        // No date and no dateutc → can't partition → no rows (not a panic).
        let o = json!({"tempf": 70.0, "humidity": 45});
        assert!(readings_from(&o, &dev).is_empty());
    }

    #[test]
    fn parse_credentials_splits_and_validates() {
        let (api, app) = parse_credentials("AAA:BBB").unwrap();
        assert_eq!(api, "AAA");
        assert_eq!(app, "BBB");
        // Whitespace trimmed.
        let (api, app) = parse_credentials("  AAA : BBB  ").unwrap();
        assert_eq!((api.as_str(), app.as_str()), ("AAA", "BBB"));
        // Empty → error.
        assert!(parse_credentials("   ").is_err());
        // Only the apiKey, no baked applicationKey in this env → error naming
        // the Application Key.
        let err = parse_credentials("AAA").unwrap_err().to_string();
        assert!(err.contains("Application Key"), "{err}");
    }

    // --- a scripted mock API ---------------------------------------------

    /// Serves a fixed device list, and per-MAC pages keyed by `endDate`. Pages
    /// are returned in the order queued; `device_data` pops the next one.
    struct MockApi {
        devices: Vec<Value>,
        pages: RefCell<std::collections::VecDeque<Vec<Value>>>,
        end_dates: RefCell<Vec<Option<i64>>>,
    }

    impl MockApi {
        fn new(devices: Vec<Value>, pages: Vec<Vec<Value>>) -> Self {
            MockApi {
                devices,
                pages: RefCell::new(pages.into_iter().collect()),
                end_dates: RefCell::new(Vec::new()),
            }
        }
    }

    impl AmbientApi for MockApi {
        fn devices(&self) -> Result<Vec<Value>, FetchError> {
            Ok(self.devices.clone())
        }
        fn device_data(
            &self,
            _mac: &str,
            end_date: Option<i64>,
            _limit: u32,
        ) -> Result<Vec<Value>, FetchError> {
            self.end_dates.borrow_mut().push(end_date);
            Ok(self.pages.borrow_mut().pop_front().unwrap_or_default())
        }
    }

    #[test]
    fn full_pull_writes_both_layers_dedupes_and_advances_watermark() {
        let v = temp_vault("fullpull");
        // Two observations in different months (Jan 2018, Dec 2017), returned as
        // one short page (fewer than the 288 limit → the last page of history).
        let jan = observation(1515436500000, "2018-01-08T18:35:00.000Z");
        let dec = observation(1513000000000, "2017-12-11T12:26:40.000Z");
        let api = MockApi::new(vec![device_entry()], vec![vec![jan, dec]]);

        let out = pull_with(&v, &api).unwrap();
        let n = out.counts.get("readings").copied().unwrap();
        assert!(n > 0, "readings written");

        // Contract layer, partitioned by the observation's LOCAL month. With a
        // negative UTC offset (the CI/dev tz here), the 2018-01-08T18:35Z row may
        // fall on the 8th locally; assert by reading whichever Jan-2018 file
        // exists and checking the guids.
        let contract = v.stream(DIR, Partition::Month);
        let mut all: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        // Both observations' temperature readings landed.
        let temps: Vec<&HomeReading> = all.iter().filter(|r| r.metric == "temperature").collect();
        assert_eq!(temps.len(), 2, "one temperature reading per observation");
        // Every row carries a stable guid in extra.
        assert!(all.iter().all(|r| r.extra.get("guid").is_some()));

        // Raw layer mirrors the partitioning under raw/, verbatim observations.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let mut raw_rows: Vec<Value> = Vec::new();
        for key in raw.partitions().unwrap() {
            raw_rows.extend(raw.read::<Value>(&key).unwrap());
        }
        assert_eq!(raw_rows.len(), 2, "one raw object per observation");
        assert!(
            raw_rows.iter().any(|r| r.get("dewPoint").is_some()),
            "raw keeps fields the contract maps (camelCase dewPoint preserved)"
        );
        assert!(
            raw_rows.iter().all(|r| str_field(r, "macAddress") == "00:0E:C6:00:00:00"),
            "history rows are stamped with their device MAC for self-describing raw"
        );

        // The device-list snapshot landed under raw/devices/.
        let dev_raw = v.stream(&format!("{RAW_DIR}/devices"), Partition::Month);
        let dev_count: usize =
            dev_raw.partitions().unwrap().iter().map(|k| dev_raw.read::<Value>(k).unwrap().len()).sum();
        assert_eq!(dev_count, 1, "the device entry stored verbatim");

        // Per-device watermark advanced to the max dateutc seen.
        let state = v.read_ambient_weather_sync();
        assert_eq!(state.watermarks.get("00:0E:C6:00:00:00"), Some(&1515436500000));
        assert!(state.updated.is_some());

        // The cursor carries NO secret (it only holds MAC→ms watermarks).
        let cursor = std::fs::read_to_string(v.root().join(".trove/ambient-weather-sync.json")).unwrap();
        assert!(!cursor.contains("apiKey") && !cursor.contains("AAA"));

        // Re-run with the same input → guid dedupe, byte-identical contract file.
        let snapshot: BTreeMap<String, String> = contract
            .partitions()
            .unwrap()
            .into_iter()
            .map(|k| {
                let body = std::fs::read_to_string(v.root().join(format!("{DIR}/{k}.jsonl"))).unwrap();
                (k, body)
            })
            .collect();
        let api2 = MockApi::new(
            vec![device_entry()],
            vec![vec![observation(1515436500000, "2018-01-08T18:35:00.000Z"),
                      observation(1513000000000, "2017-12-11T12:26:40.000Z")]],
        );
        let again = pull_with(&v, &api2).unwrap();
        assert_eq!(again.counts.get("readings"), Some(&0), "all guids already stored");
        for (k, before) in &snapshot {
            let after = std::fs::read_to_string(v.root().join(format!("{DIR}/{k}.jsonl"))).unwrap();
            assert_eq!(before, &after, "contract file {k} byte-identical after re-run");
        }
    }

    #[test]
    fn incremental_pull_stops_at_the_watermark() {
        let v = temp_vault("incremental");
        // Seed a watermark so the device is "caught up" to 1515436500000.
        v.write_ambient_weather_sync(&SyncState {
            watermarks: BTreeMap::from([("00:0E:C6:00:00:00".to_string(), 1515436500000)]),
            updated: Some("2026-06-15T00:00:00-07:00".into()),
        })
        .unwrap();

        // A page whose only row is exactly the watermark → nothing strictly
        // newer, so no rows written and the loop stops without descending.
        let api = MockApi::new(
            vec![device_entry()],
            vec![vec![observation(1515436500000, "2018-01-08T18:35:00.000Z")]],
        );
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0), "watermark row is not new");
        // Only one page was requested (no backward descent past the watermark).
        assert_eq!(api.end_dates.borrow().len(), 1, "stopped after the caught-up page");

        // Now a newer observation arrives → it alone is written.
        let api2 = MockApi::new(
            vec![device_entry()],
            vec![vec![observation(1515440000000, "2018-01-08T19:33:20.000Z"),
                      observation(1515436500000, "2018-01-08T18:35:00.000Z")]],
        );
        let out2 = pull_with(&v, &api2).unwrap();
        assert!(out2.counts.get("readings").copied().unwrap() > 0, "the newer row is written");
        let state = v.read_ambient_weather_sync();
        assert_eq!(state.watermarks.get("00:0E:C6:00:00:00"), Some(&1515440000000), "watermark advanced");
    }

    #[test]
    fn backfill_walks_backward_until_a_short_page() {
        let v = temp_vault("backfill");
        // First run, no watermark. A single short page (2 rows < 288) is the
        // whole history → one request, both rows backfilled.
        let api = MockApi::new(
            vec![device_entry()],
            vec![vec![observation(1515436500000, "2018-01-08T18:35:00.000Z"),
                      observation(1515430000000, "2018-01-08T16:46:40.000Z")]],
        );
        let out = pull_with(&v, &api).unwrap();
        assert!(out.counts.get("readings").copied().unwrap() > 0);
        // A short page ends the walk after one request (no endless descent).
        assert_eq!(api.end_dates.borrow().len(), 1);
        assert_eq!(api.end_dates.borrow()[0], None, "first page has no endDate (newest)");
    }

    #[test]
    fn drain_stops_on_empty_page() {
        // A device that returns an empty first page (no data / retention horizon)
        // drains to nothing and sets no watermark — without erroring.
        let info = device_info(&device_entry()).unwrap();
        let api = MockApi::new(vec![device_entry()], vec![vec![]]);
        let (rows, raws, wm) = drain_device(&api, &info, None).unwrap();
        assert!(rows.is_empty() && raws.is_empty());
        assert_eq!(wm, None, "no observations → no watermark");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor file deserializes to all-default (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermarks.is_empty());
        assert!(empty.updated.is_none());
        // An older cursor with only watermarks (no `updated`) still loads
        // (additive evolution — prove old lines deserialize).
        let partial: SyncState =
            serde_json::from_str(r#"{"watermarks":{"AA:BB":1515436500000}}"#).unwrap();
        assert_eq!(partial.watermarks.get("AA:BB"), Some(&1515436500000));
        assert!(partial.updated.is_none());
    }

    // --- connection tests -------------------------------------------------

    #[test]
    fn connection_stores_keys_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        // Store directly (def_connect needs the network for /v1/devices).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "secret_api:secret_app".into(),
                refresh_token: None,
                token_type: Some("AmbientKeys".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Ambient Weather");
        assert_eq!(status.accounts[0].key, "ambient-weather");

        // The keys are NOT in any non-secret file (the cursor).
        v.write_ambient_weather_sync(&SyncState {
            watermarks: BTreeMap::from([("AA".to_string(), 1)]),
            updated: Some("2026-06-15T00:00:00-07:00".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/ambient-weather-sync.json")).unwrap();
        assert!(!cursor.contains("secret_api"), "keys never in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("secret_api") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret key file must be 0600");
                }
            }
            assert!(found, "the keys were stored under .trove/sync");
        }

        def_disconnect(&v, "ambient-weather").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn empty_keys_rejected_and_pull_needs_connection() {
        let v = temp_vault("empty");
        assert!(def_connect(&v, "   ").is_err());
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "ambient-weather");
        assert_eq!(DEF.connection, Some("ambient-weather"));
    }
}
