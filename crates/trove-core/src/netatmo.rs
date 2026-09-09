//! Netatmo Weather Station — cloud pull via the official Netatmo API.
//! Brief: docs/integrations/netatmo.md.
//!
//! A **Periodic** (hourly) cloud pull over two endpoints:
//!
//! - `GET /api/getstationsdata` → the user's stations, each with its modules
//!   (base station + outdoor / rain / wind / indoor extension modules), every
//!   module carrying a `dashboard_data` block with the latest readings.
//!   Full-fidelity station snapshot lands in `home/netatmo/raw/stations/YYYY-MM.jsonl`.
//!
//! - `POST /api/getmeasure` (per module per type) → historical readings for
//!   a given `[date_begin, date_end]` window at the finest available scale.
//!   Watermark per module in `.trove/netatmo-sync.json`; walks forward from
//!   the watermark until the API returns a short page (< `SCALE_LIMIT` rows)
//!   or an empty body. A batch of measurements fans out into one
//!   [`HomeReading`] per metric per timestamp.
//!
//! **Field names (exact Netatmo API casing — CamelCase):**
//!
//! | API field          | metric key            | unit    |
//! |--------------------|-----------------------|---------|
//! | `Temperature`      | `temperature`         | `C`     |
//! | `Humidity`         | `humidity`            | `percent` |
//! | `CO2`              | `co2`                 | `ppm`   |
//! | `Noise`            | `noise`               | `db`    |
//! | `Pressure`         | `pressure`            | `hpa`   |
//! | `AbsolutePressure` | `pressure_absolute`   | `hpa`   |
//! | `WindStrength`     | `wind_speed`          | `km_h`  |
//! | `WindAngle`        | `wind_direction`      | `degrees` |
//! | `GustStrength`     | `wind_gust`           | `km_h`  |
//! | `GustAngle`        | `wind_gust_direction` | `degrees` |
//! | `Rain`             | `rain`                | `mm`    |
//! | `sum_rain_1`       | `rain_hourly`         | `mm`    |
//! | `sum_rain_24`      | `rain_daily`          | `mm`    |
//!
//! **Auth:** OAuth 2.0 with standard authorization-code flow. The user
//! (or Trove's baked app) registers a free app at dev.netatmo.com for
//! `client_id` + `client_secret`. Scopes: `read_station read_homecoach`.
//! Netatmo issues a refresh token — the pull refreshes silently on expiry.
//! Redirect URI: `http://localhost:38739/callback` (INDEX #159 assigned port).
//!
//! **Vault layout:**
//! - `home/netatmo/YYYY-MM.jsonl` — contract [`HomeReading`] rows, one per
//!   metric per measurement interval.
//! - `home/netatmo/raw/YYYY-MM.jsonl` — getmeasure raw response objects (full
//!   fidelity), tagged with station/module ids and query metadata.
//! - `home/netatmo/raw/stations/YYYY-MM.jsonl` — getstationsdata snapshots
//!   (the station+module+dashboard_data objects verbatim).
//!
//! **Dedupe:** `guid` = `netatmo:{station_id}:{module_id}:{metric}:{epoch}`.
//! Cursor (non-secret) at `.trove/netatmo-sync.json` — per-module per-type
//! `date_end` watermark (epoch seconds). Rebuildable.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::{self, AppCredentials, OauthFlow, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

const SERVICE: &str = "netatmo";
const SYNC_FILE: &str = ".trove/netatmo-sync.json";
const CONTRACT_DIR: &str = "home/netatmo";
const RAW_DIR: &str = "home/netatmo/raw";
const STATIONS_RAW_DIR: &str = "home/netatmo/raw/stations";
const API_BASE: &str = "https://api.netatmo.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Hourly sync.
pub const NETATMO_SYNC_SECS: u64 = 3600;
/// `getmeasure` scale: "30min" gives readings at the native Netatmo station
/// interval (5–15 min stored at this granularity). `optimize=false` returns
/// each timestamp as a separate body key for straightforward parsing.
const SCALE: &str = "30min";
/// The API returns at most 1024 rows per getmeasure call; a short page signals
/// end-of-history or caught-up state.
const SCALE_LIMIT: usize = 1024;
/// Rate-limit guard between requests (Netatmo: 500 req/hr).
const REQ_INTERVAL: Duration = Duration::from_millis(600);

// ---------------------------------------------------------------------------
// Metric table: Netatmo API field (exact CamelCase) → (metric key, unit).
// Used for both the `type` query parameter and decoding positional value arrays.

const METRICS: &[(&str, &str, &str)] = &[
    ("Temperature", "temperature", "C"),
    ("Humidity", "humidity", "percent"),
    ("CO2", "co2", "ppm"),
    ("Noise", "noise", "db"),
    ("Pressure", "pressure", "hpa"),
    ("AbsolutePressure", "pressure_absolute", "hpa"),
    ("WindStrength", "wind_speed", "km_h"),
    ("WindAngle", "wind_direction", "degrees"),
    ("GustStrength", "wind_gust", "km_h"),
    ("GustAngle", "wind_gust_direction", "degrees"),
    ("Rain", "rain", "mm"),
    ("sum_rain_1", "rain_hourly", "mm"),
    ("sum_rain_24", "rain_daily", "mm"),
];

// ---------------------------------------------------------------------------
// OAuth provider.

pub static NETATMO: Provider = Provider {
    service: SERVICE,
    display_name: "Netatmo",
    auth_url: "https://api.netatmo.com/oauth2/authorize",
    token_url: "https://api.netatmo.com/oauth2/token",
    // read_station: weather stations. read_homecoach: HOME Coach air-quality.
    scopes: "read_station read_homecoach",
    // Assigned production port: 38580 + 159 = 38739.
    redirect_port: 38739,
    use_pkce: false,
    // Netatmo expects client_id/secret in the form body, not Basic auth.
    basic_auth: false,
    default_client_id: option_env!("TROVE_NETATMO_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_NETATMO_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Connection.

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(NETATMO.service)?.is_some()
        || NETATMO.default_credentials().is_some();
    let accounts = match vault.load_sync_token(NETATMO.service)? {
        Some(token) => vec![ConnectedAccount {
            key: NETATMO.service.to_string(),
            label: NETATMO.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // Netatmo issues a refresh token; only flag reconnect if expired
            // AND no refresh token available.
            needs_reconnect: token.expired() && token.refresh_token.is_none(),
            extra: BTreeMap::new(),
        }],
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`] — the integrator adds
/// one `&crate::netatmo::CONNECTION,` line.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "netatmo",
    display_name: "Netatmo",
    methods: &[ConnectMethod::OAuth {
        provider: &NETATMO,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["netatmo"],
    setup: &[
        "Sign in at dev.netatmo.com and create a free app (any name).",
        "Add http://localhost:38739/callback as an authorized redirect URI.",
        "Paste the Client ID and Client Secret here — saved once, \
         every future connect is just a login.",
    ],
};

/// Interactive connect: opens the Netatmo consent page, waits for the redirect,
/// saves the token. Credentials resolve: explicit → previously saved →
/// compiled-in defaults.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(NETATMO.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(NETATMO.service)?
            .or_else(|| NETATMO.default_credentials())
            .context(
                "no Netatmo app credentials — register a free app at dev.netatmo.com and \
                 enter its Client ID and Secret in the Integrations tab",
            )?,
    };
    let flow = OauthFlow::start(&NETATMO, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(NETATMO.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Registry face (DEF).

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("netatmo synced — {n} readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "netatmo sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub already
/// has `pub mod netatmo;` in `lib.rs` and `&crate::netatmo::DEF` in
/// `INTEGRATIONS` — do NOT add them again).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "netatmo",
        name: "Netatmo Weather Station",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls indoor and outdoor readings from your Netatmo personal weather station \
             and HOME Coach air-quality modules via the official API. Hourly poll with full \
             historical backfill; covers temperature, humidity, CO2, noise, pressure, wind, \
             and rain across all connected modules.",
        domain: "home",
        vault_path: "home/netatmo/",
        toggleable: true,
        setup: &[
            "Register a free app at dev.netatmo.com, then connect here.",
            "First sync backfills your station's full history; later syncs are incremental.",
        ],
        caveats:
            "Requires a free Netatmo developer app (dev.netatmo.com). \
             Historical data available for all measurements with no stated retention limit. \
             Rate limited to 500 requests/hour; a large backfill runs over several passes.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(NETATMO_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("netatmo"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Per-module per-type watermark.
/// Key: `"{station_id}:{module_id}:{api_field}"` e.g.
///   `"70:ee:50:aa:bb:cc:02:00:00:aa:bb:cc:Temperature"`.
/// Value: epoch seconds of the last successfully ingested batch's latest ts.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    watermarks: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_netatmo_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_netatmo_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Module type → getmeasure field list.
//
// Netatmo collapses module capabilities in the `data_type` array using *group
// names* ("Wind", "Rain") rather than the individual measurement sub-fields
// ("WindStrength", "WindAngle", …). The group names do NOT match METRICS keys,
// so intersecting data_type against METRICS yields zero hits for wind/rain and
// misses AbsolutePressure (which never appears in data_type at all).
//
// The correct mapping is by `type` (NAMain / NAModule1 / NAModule2 /
// NAModule3 / NAModule4 / NHC), which is always present and stable:
//
//   NAMain     — base station: Temperature + Humidity + CO2 + Noise +
//                Pressure + AbsolutePressure
//   NAModule1  — outdoor module: Temperature + Humidity
//   NAModule2  — wind gauge: WindStrength + WindAngle + GustStrength + GustAngle
//   NAModule3  — rain gauge: Rain + sum_rain_1 + sum_rain_24
//   NAModule4  — indoor extension: Temperature + Humidity + CO2 + Noise
//   NHC        — HOME Coach: Temperature + Humidity + CO2 + Noise +
//                Pressure + AbsolutePressure
//
// Evidence: exzz/netatmo-api-go, philippelt/netatmo-api-python, HA netatmo
// sensor.py — all drive field selection from module `type`, not data_type.

/// Return METRICS indices for a known Netatmo module `type` string.
///
/// Index reference (matches METRICS table order):
///   0=Temperature, 1=Humidity, 2=CO2, 3=Noise, 4=Pressure, 5=AbsolutePressure,
///   6=WindStrength, 7=WindAngle, 8=GustStrength, 9=GustAngle,
///   10=Rain, 11=sum_rain_1, 12=sum_rain_24
///
/// Unknown types return empty (graceful: new/unknown modules stored in raw
/// but not mapped to contract metrics).
fn metrics_for_module_type(module_type: &str) -> &'static [usize] {
    match module_type {
        // Base station + HOME Coach: all indoor readings + both pressure variants.
        "NAMain" | "NHC" => &[0, 1, 2, 3, 4, 5],
        // Outdoor module: temperature + humidity only.
        "NAModule1"      => &[0, 1],
        // Wind gauge: all four wind sub-fields.
        "NAModule2"      => &[6, 7, 8, 9],
        // Rain gauge: instantaneous + hourly + daily totals.
        "NAModule3"      => &[10, 11, 12],
        // Indoor extension module: temp, humidity, CO2, noise (no pressure).
        "NAModule4"      => &[0, 1, 2, 3],
        _                => &[],
    }
}

// ---------------------------------------------------------------------------
// Module descriptor — the station topology from getstationsdata.

#[derive(Clone, Debug)]
struct ModuleDesc {
    station_id: String,
    module_id: String,
    station_name: String,
    module_name: String,
    /// METRICS indices this module type reports (driven by `type` field, not `data_type`).
    types: Vec<usize>,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

trait NetatmoApi {
    fn stations_data(&self, access_token: &str) -> Result<Value>;
    fn measure(
        &self,
        access_token: &str,
        station_id: &str,
        module_id: &str,
        api_type: &str,
        date_begin: i64,
        date_end: Option<i64>,
    ) -> Result<Value>;
}

struct LiveApi;

impl NetatmoApi for LiveApi {
    fn stations_data(&self, access_token: &str) -> Result<Value> {
        let resp = ureq::get(&format!("{API_BASE}/api/getstationsdata"))
            .set("Authorization", &format!("Bearer {access_token}"))
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(describe_http_error)
            .context("netatmo getstationsdata")?;
        resp.into_json::<Value>().context("parsing getstationsdata")
    }

    fn measure(
        &self,
        access_token: &str,
        station_id: &str,
        module_id: &str,
        api_type: &str,
        date_begin: i64,
        date_end: Option<i64>,
    ) -> Result<Value> {
        let mut form: Vec<(&str, String)> = vec![
            ("device_id", station_id.to_string()),
            ("module_id", module_id.to_string()),
            ("scale", SCALE.to_string()),
            ("type", api_type.to_string()),
            ("date_begin", date_begin.to_string()),
            ("optimize", "false".to_string()),
            ("real_time", "false".to_string()),
        ];
        if let Some(de) = date_end {
            form.push(("date_end", de.to_string()));
        }
        let form_ref: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let resp = ureq::post(&format!("{API_BASE}/api/getmeasure"))
            .set("Authorization", &format!("Bearer {access_token}"))
            .timeout(HTTP_TIMEOUT)
            .send_form(&form_ref)
            .map_err(describe_http_error)
            .context("netatmo getmeasure")?;
        resp.into_json::<Value>().context("parsing getmeasure")
    }
}

fn describe_http_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {code}: {}", body.chars().take(400).collect::<String>())
        }
        other => anyhow::Error::from(other),
    }
}

// ---------------------------------------------------------------------------
// Station / module discovery.

/// Extract all module descriptors from a `getstationsdata` body. The base
/// station is itself a module (module_id == station_id) plus the `modules`
/// extension array.
fn extract_modules(body: &Value) -> Vec<ModuleDesc> {
    let devices = match body.pointer("/body/devices").and_then(Value::as_array) {
        Some(d) => d,
        None => return Vec::new(),
    };

    let mut result = Vec::new();
    for device in devices {
        let station_id = str_field(device, "_id");
        if station_id.is_empty() {
            continue;
        }
        let station_name = str_field(device, "station_name");

        // Base station: module_id == station_id.
        let base_types = type_indices(device);
        if !base_types.is_empty() {
            result.push(ModuleDesc {
                station_id: station_id.clone(),
                module_id: station_id.clone(),
                station_name: station_name.clone(),
                module_name: station_name.clone(),
                types: base_types,
            });
        }

        // Extension modules.
        if let Some(mods) = device.get("modules").and_then(Value::as_array) {
            for m in mods {
                let mid = str_field(m, "_id");
                if mid.is_empty() {
                    continue;
                }
                let module_name = {
                    let n = str_field(m, "module_name");
                    if n.is_empty() { mid.clone() } else { n }
                };
                let mtypes = type_indices(m);
                if !mtypes.is_empty() {
                    result.push(ModuleDesc {
                        station_id: station_id.clone(),
                        module_id: mid,
                        station_name: station_name.clone(),
                        module_name,
                        types: mtypes,
                    });
                }
            }
        }
    }
    result
}

/// Map a module object to METRICS indices via its `type` field.
///
/// We intentionally ignore `data_type` (which collapses sub-fields into group
/// names like "Wind"/"Rain" that do not match METRICS keys) in favour of the
/// stable `type` field (NAMain/NAModule1/NAModule2/NAModule3/NAModule4/NHC).
fn type_indices(v: &Value) -> Vec<usize> {
    let module_type = match v.get("type").and_then(Value::as_str) {
        Some(t) => t,
        None => return Vec::new(),
    };
    metrics_for_module_type(module_type).to_vec()
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Parsing getmeasure responses (optimize=false).
//
// Response shape:
//   { "status": "ok",
//     "body": { "1699000000": [21.3, 65.0], "1699001800": [21.1, 66.0], … },
//     "time_server": … }
//
// Each key is a Unix epoch-seconds string; each value is a positional array
// matching the requested `type` comma-list order.

/// Parse a getmeasure body into `(epoch_sec, [values])` pairs, sorted ascending.
fn parse_measure_body(body: &Value, type_count: usize) -> Vec<(i64, Vec<Option<f64>>)> {
    let map = match body.pointer("/body").and_then(Value::as_object) {
        Some(m) => m,
        None => return Vec::new(),
    };
    let mut rows: Vec<(i64, Vec<Option<f64>>)> = Vec::new();
    for (ts_str, vals_v) in map {
        let epoch: i64 = match ts_str.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let arr = match vals_v.as_array() {
            Some(a) => a,
            None => continue,
        };
        let values: Vec<Option<f64>> = (0..type_count)
            .map(|i| arr.get(i).and_then(Value::as_f64))
            .collect();
        rows.push((epoch, values));
    }
    rows.sort_by_key(|(t, _)| *t);
    rows
}

// ---------------------------------------------------------------------------
// Mapping one getmeasure row → HomeReadings.

fn epoch_to_ts(epoch: i64) -> Option<String> {
    Local.timestamp_opt(epoch, 0).single().map(|dt| dt.to_rfc3339())
}

/// One `(epoch, values)` pair → `HomeReading` rows, one per present metric.
/// `type_idx_list` is the METRICS-index slice matching `values` positions.
fn readings_from_measure(
    epoch: i64,
    values: &[Option<f64>],
    type_idx_list: &[usize],
    module: &ModuleDesc,
) -> Vec<HomeReading> {
    let Some(ts) = epoch_to_ts(epoch) else { return Vec::new() };
    if Partition::Month.key(&ts).is_none() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for (pos, &metric_idx) in type_idx_list.iter().enumerate() {
        let Some(value) = values.get(pos).copied().flatten() else { continue };
        let (api_field, metric, unit) = METRICS[metric_idx];
        let mut r = HomeReading::new("netatmo", metric, value, ts.clone());
        r.unit = unit.to_string();
        r.place = module.station_name.clone();
        r.device = module.module_id.clone();
        let mut extra = Map::new();
        extra.insert(
            "guid".into(),
            Value::String(format!(
                "netatmo:{}:{}:{metric}:{epoch}",
                module.station_id, module.module_id
            )),
        );
        extra.insert("api_field".into(), Value::String(api_field.to_string()));
        extra.insert("module_name".into(), Value::String(module.module_name.clone()));
        r.extra = extra;
        rows.push(r);
    }
    rows
}

// ---------------------------------------------------------------------------
// Write layer (contract + raw), deduped by guid.

fn reading_guid(v: &Value) -> String {
    v.get("extra")
        .and_then(|e| e.get("guid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Raw line: full-fidelity getmeasure response tagged with query context.
#[derive(Serialize)]
struct RawMeasureLine {
    #[serde(skip)]
    ts: String,
    station_id: String,
    module_id: String,
    module_name: String,
    api_type: String,
    scale: &'static str,
    date_begin: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    date_end: Option<i64>,
    #[serde(flatten)]
    body: Value,
}

/// Stations snapshot raw line.
#[derive(Serialize)]
struct StationsSnap {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    body: Value,
}

fn write_layers(
    vault: &Vault,
    rows: Vec<HomeReading>,
    raw_line: Option<RawMeasureLine>,
) -> Result<u64> {
    let contract = vault.stream(CONTRACT_DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids for contract dedupe.
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
            continue;
        }
        new_rows.push(row);
    }

    contract.append(&new_rows, |r| &r.ts)?;

    if let Some(rl) = raw_line {
        if Partition::Month.key(&rl.ts).is_some() {
            raw.append(&[rl], |r| &r.ts)?;
        }
    }

    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// Token resolution (refresh on expiry).

fn resolve_token(vault: &Vault) -> Result<String> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Netatmo is not connected — add your credentials in the Integrations tab")?;

    if !token.expired() {
        return Ok(token.access_token.clone());
    }
    let creds = vault
        .load_sync_app(SERVICE)?
        .or_else(|| NETATMO.default_credentials())
        .context("Netatmo credentials not found — reconnect in the Integrations tab")?;
    let refreshed = oauth::refresh_token(&NETATMO, &creds, &token)?;
    vault.save_sync_token(SERVICE, &refreshed)?;
    Ok(refreshed.access_token)
}

// ---------------------------------------------------------------------------
// The pull.

/// Top-level pull (live path).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let access_token = resolve_token(vault)?;
    pull_with(vault, &LiveApi, &access_token)
}

/// Pull body over an injectable API — the testable seam.
fn pull_with(vault: &Vault, api: &impl NetatmoApi, access_token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_netatmo_sync();
    let now_epoch = chrono::Utc::now().timestamp();

    // 1. Fetch + snapshot station topology.
    let stations_body = api.stations_data(access_token)?;
    let modules = extract_modules(&stations_body);

    if let Some(ts) = epoch_to_ts(now_epoch) {
        if Partition::Month.key(&ts).is_some() {
            let snap = vault.stream(STATIONS_RAW_DIR, Partition::Month);
            let line = StationsSnap { ts, body: stations_body };
            snap.append(&[line], |s| &s.ts)?;
        }
    }

    // 2. Per-module, per-type: walk getmeasure forward from watermark.
    let mut total: u64 = 0;
    for module in &modules {
        for &metric_idx in &module.types {
            let (api_field, _, _) = METRICS[metric_idx];
            let wm_key =
                format!("{}:{}:{}", module.station_id, module.module_id, api_field);
            let watermark = state.watermarks.get(&wm_key).copied();
            let date_begin = watermark.unwrap_or(0);

            let (written, new_wm) = drain_module(
                vault,
                api,
                access_token,
                module,
                &[metric_idx],
                date_begin,
                now_epoch,
            )?;
            total += written;

            if let Some(wm) = new_wm {
                let entry = state.watermarks.entry(wm_key).or_insert(wm);
                if wm > *entry {
                    *entry = wm;
                }
            }
            std::thread::sleep(REQ_INTERVAL);
        }
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_netatmo_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total} readings"),
        counts: BTreeMap::from([("readings", total)]),
    })
}

/// Walk getmeasure forward for one module + type combination.
/// Returns `(readings_written, new_watermark_epoch)`.
fn drain_module(
    vault: &Vault,
    api: &impl NetatmoApi,
    access_token: &str,
    module: &ModuleDesc,
    type_idx_list: &[usize],
    date_begin: i64,
    now_epoch: i64,
) -> Result<(u64, Option<i64>)> {
    let api_types: Vec<&str> = type_idx_list.iter().map(|&i| METRICS[i].0).collect();
    let type_param = api_types.join(",");

    let mut written_total: u64 = 0;
    let mut max_seen: Option<i64> = None;
    let mut cursor = date_begin;

    loop {
        let body = api.measure(
            access_token,
            &module.station_id,
            &module.module_id,
            &type_param,
            cursor,
            Some(now_epoch),
        )?;

        let rows = parse_measure_body(&body, type_idx_list.len());
        if rows.is_empty() {
            break;
        }

        let mut all_readings: Vec<HomeReading> = Vec::new();
        for (epoch, values) in &rows {
            all_readings.extend(readings_from_measure(*epoch, values, type_idx_list, module));
        }

        let latest_epoch = rows.iter().map(|(t, _)| *t).max().unwrap_or(cursor);
        let raw_ts = epoch_to_ts(latest_epoch).unwrap_or_default();

        let raw_line = if !raw_ts.is_empty() {
            Some(RawMeasureLine {
                ts: raw_ts,
                station_id: module.station_id.clone(),
                module_id: module.module_id.clone(),
                module_name: module.module_name.clone(),
                api_type: type_param.clone(),
                scale: SCALE,
                date_begin: cursor,
                date_end: Some(now_epoch),
                body: body.clone(),
            })
        } else {
            None
        };

        let written = write_layers(vault, all_readings, raw_line)?;
        written_total += written;

        if let Some(mx) = rows.iter().map(|(t, _)| *t).max() {
            max_seen = Some(max_seen.map_or(mx, |m: i64| m.max(mx)));
        }

        // Short page → end of available data.
        if rows.len() < SCALE_LIMIT {
            break;
        }

        let next_begin = latest_epoch + 1;
        if next_begin <= cursor {
            // Non-advancing cursor — safety stop.
            break;
        }
        cursor = next_begin;
        std::thread::sleep(REQ_INTERVAL);
    }

    Ok((written_total, max_seen))
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-netatmo-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — real Netatmo API shapes confirmed from:
    //   philippelt/netatmo-api-python (dashboard_data fields)
    //   home-assistant/core netatmo sensor.py (type → field mappings)
    //   exzz/netatmo-api-go (module type → measurement mapping)
    //   planetk/homebridge-netatmo mockapi (real data_type values per type)
    //   radcliff gist 043cc6be892847e1be08 (live getstationsdata dump)
    //
    // Key: `data_type` uses GROUP names ("Wind","Rain"), not sub-field names.
    //      `type` (NAMain/NAModule1/NAModule2/NAModule3) drives getmeasure field
    //      selection. AbsolutePressure is in dashboard_data but NOT in data_type.

    fn stations_body() -> Value {
        json!({
            "status": "ok",
            "body": {
                "devices": [{
                    "_id": "70:ee:50:aa:bb:cc",
                    "station_name": "Home Station",
                    "type": "NAMain",
                    // Real NAMain data_type: group names only; AbsolutePressure absent.
                    "data_type": ["Temperature","CO2","Humidity","Noise","Pressure"],
                    "dashboard_data": {
                        "time_utc": 1699000000,
                        "Temperature": 21.3,
                        "Humidity": 62,
                        "CO2": 412,
                        "Noise": 38,
                        "Pressure": 1013.2,
                        "AbsolutePressure": 1010.5,
                        "temp_trend": "stable",
                        "pressure_trend": "down"
                    },
                    "modules": [
                        {
                            "_id": "02:00:00:aa:bb:cc",
                            "module_name": "Outdoor",
                            "type": "NAModule1",
                            "data_type": ["Temperature","Humidity"],
                            "dashboard_data": {
                                "time_utc": 1699000000,
                                "Temperature": 13.7,
                                "Humidity": 78
                            }
                        },
                        {
                            "_id": "06:00:00:aa:bb:cc",
                            "module_name": "Wind Gauge",
                            "type": "NAModule2",
                            // Real NAModule2 data_type: group name "Wind", not sub-fields.
                            "data_type": ["Wind"],
                            "dashboard_data": {
                                "time_utc": 1699000000,
                                "WindStrength": 12,
                                "WindAngle": 225,
                                "GustStrength": 18,
                                "GustAngle": 220
                            }
                        },
                        {
                            "_id": "05:00:00:aa:bb:cc",
                            "module_name": "Rain Gauge",
                            "type": "NAModule3",
                            // Real NAModule3 data_type: group name "Rain" only.
                            "data_type": ["Rain"],
                            "dashboard_data": {
                                "time_utc": 1699000000,
                                "Rain": 0.5,
                                "sum_rain_1": 2.3,
                                "sum_rain_24": 14.7
                            }
                        }
                    ]
                }]
            },
            "time_server": 1699000500
        })
    }

    fn simple_stations_body() -> Value {
        json!({
            "status": "ok",
            "body": {
                "devices": [{
                    "_id": "70:ee:50:aa:bb:cc",
                    "station_name": "Test Station",
                    "type": "NAMain",
                    "data_type": ["Temperature","CO2","Humidity","Noise","Pressure"],
                    "modules": []
                }]
            }
        })
    }

    fn measure_body(pairs: &[(i64, f64)]) -> Value {
        let mut map = serde_json::Map::new();
        for (epoch, val) in pairs {
            map.insert(epoch.to_string(), json!([val]));
        }
        json!({"status":"ok","body": map})
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        stations: Value,
        measures: RefCell<std::collections::VecDeque<Value>>,
    }

    impl MockApi {
        fn new(stations: Value, measures: Vec<Value>) -> Self {
            MockApi {
                stations,
                measures: RefCell::new(measures.into_iter().collect()),
            }
        }
    }

    impl NetatmoApi for MockApi {
        fn stations_data(&self, _t: &str) -> Result<Value> {
            Ok(self.stations.clone())
        }
        fn measure(
            &self,
            _t: &str,
            _sid: &str,
            _mid: &str,
            _ty: &str,
            _db: i64,
            _de: Option<i64>,
        ) -> Result<Value> {
            Ok(self.measures.borrow_mut().pop_front().unwrap_or_else(|| {
                json!({"status":"ok","body":{}})
            }))
        }
    }

    // -----------------------------------------------------------------------
    // Tests: module extraction.

    #[test]
    fn extract_modules_parses_all_module_types() {
        let body = stations_body();
        let modules = extract_modules(&body);
        assert_eq!(modules.len(), 4, "base + 3 extension modules");

        // NAMain: Temperature, Humidity, CO2, Noise, Pressure, AbsolutePressure (6).
        let base = modules.iter().find(|m| m.module_id == "70:ee:50:aa:bb:cc").unwrap();
        assert_eq!(base.station_name, "Home Station");
        assert_eq!(base.types.len(), 6, "NAMain yields 6 metrics");
        let base_fields: Vec<&str> = base.types.iter().map(|&i| METRICS[i].0).collect();
        assert!(base_fields.contains(&"Temperature"));
        assert!(base_fields.contains(&"AbsolutePressure"),
            "AbsolutePressure included for NAMain even though absent from data_type");

        // NAModule1: Temperature, Humidity (2).
        let outdoor = modules.iter().find(|m| m.module_id == "02:00:00:aa:bb:cc").unwrap();
        assert_eq!(outdoor.module_name, "Outdoor");
        assert_eq!(outdoor.types.len(), 2);
        let outdoor_fields: Vec<&str> =
            outdoor.types.iter().map(|&i| METRICS[i].0).collect();
        assert!(outdoor_fields.contains(&"Temperature"));
        assert!(outdoor_fields.contains(&"Humidity"));

        // NAModule2: WindStrength, WindAngle, GustStrength, GustAngle (4).
        // Real data_type is ["Wind"] — type field drives the mapping.
        let wind = modules.iter().find(|m| m.module_id == "06:00:00:aa:bb:cc").unwrap();
        assert_eq!(wind.types.len(), 4, "NAModule2 yields 4 wind metrics");
        let wind_fields: Vec<&str> = wind.types.iter().map(|&i| METRICS[i].0).collect();
        assert!(wind_fields.contains(&"WindStrength"), "wind_speed collected");
        assert!(wind_fields.contains(&"WindAngle"),   "wind_direction collected");
        assert!(wind_fields.contains(&"GustStrength"), "wind_gust collected");
        assert!(wind_fields.contains(&"GustAngle"),   "wind_gust_direction collected");

        // NAModule3: Rain, sum_rain_1, sum_rain_24 (3).
        // Real data_type is ["Rain"] — type field drives the mapping.
        let rain = modules.iter().find(|m| m.module_id == "05:00:00:aa:bb:cc").unwrap();
        assert_eq!(rain.types.len(), 3, "NAModule3 yields 3 rain metrics");
        let rain_fields: Vec<&str> = rain.types.iter().map(|&i| METRICS[i].0).collect();
        assert!(rain_fields.contains(&"Rain"),        "instantaneous rain collected");
        assert!(rain_fields.contains(&"sum_rain_1"),  "rain_hourly collected");
        assert!(rain_fields.contains(&"sum_rain_24"), "rain_daily collected");
    }

    #[test]
    fn extract_modules_skips_empty_station_id() {
        let body = json!({"body":{"devices":[{"_id":"","type":"NAMain","data_type":["Temperature"]}]}});
        assert!(extract_modules(&body).is_empty());
    }

    #[test]
    fn extract_modules_skips_module_with_unknown_type() {
        // A module with no recognized `type` field yields no metrics and is skipped.
        let body = json!({
            "body": {
                "devices": [{
                    "_id": "aa:bb",
                    "station_name": "X",
                    "type": "NAUnknown",
                    "data_type": [],
                    "modules": []
                }]
            }
        });
        assert!(extract_modules(&body).is_empty(), "unknown module type → no entry");
    }

    #[test]
    fn wind_module_real_data_type_is_mapped_via_type_field() {
        // Verifies the exact bug that was fixed: real NAModule2 has data_type:["Wind"]
        // (a group name). The old code intersected against METRICS keys and found nothing.
        // The new code uses `type: "NAModule2"` to drive the mapping.
        let body = json!({
            "body": {
                "devices": [{
                    "_id": "aa:bb:cc:11:22:33",
                    "station_name": "Yard",
                    "type": "NAMain",
                    "data_type": ["Temperature"],
                    "modules": [{
                        "_id": "06:00:00:11:22:33",
                        "module_name": "Wind",
                        "type": "NAModule2",
                        "data_type": ["Wind"],
                        "dashboard_data": {
                            "WindStrength": 8, "WindAngle": 90,
                            "GustStrength": 14, "GustAngle": 85
                        }
                    }]
                }]
            }
        });
        let modules = extract_modules(&body);
        let wind = modules.iter().find(|m| m.module_id == "06:00:00:11:22:33")
            .expect("wind module must be extracted despite data_type:[\"Wind\"] group name");
        let fields: Vec<&str> = wind.types.iter().map(|&i| METRICS[i].0).collect();
        assert!(fields.contains(&"WindStrength"), "WindStrength mapped");
        assert!(fields.contains(&"GustStrength"), "GustStrength mapped");
    }

    #[test]
    fn rain_module_real_data_type_is_mapped_via_type_field() {
        // Verifies: real NAModule3 has data_type:["Rain"] only; sum_rain_1/sum_rain_24
        // must still be collected via the type-driven mapping.
        let body = json!({
            "body": {
                "devices": [{
                    "_id": "aa:bb:cc:11:22:33",
                    "station_name": "Yard",
                    "type": "NAMain",
                    "data_type": ["Temperature"],
                    "modules": [{
                        "_id": "05:00:00:11:22:33",
                        "module_name": "Rain",
                        "type": "NAModule3",
                        "data_type": ["Rain"],
                        "dashboard_data": {
                            "Rain": 0.3, "sum_rain_1": 1.1, "sum_rain_24": 8.4
                        }
                    }]
                }]
            }
        });
        let modules = extract_modules(&body);
        let rain = modules.iter().find(|m| m.module_id == "05:00:00:11:22:33")
            .expect("rain module must be extracted");
        let fields: Vec<&str> = rain.types.iter().map(|&i| METRICS[i].0).collect();
        assert!(fields.contains(&"Rain"),        "Rain mapped");
        assert!(fields.contains(&"sum_rain_1"),  "sum_rain_1 (rain_hourly) mapped");
        assert!(fields.contains(&"sum_rain_24"), "sum_rain_24 (rain_daily) mapped");
    }

    // -----------------------------------------------------------------------
    // Tests: getmeasure parsing.

    #[test]
    fn parse_measure_body_sorts_ascending_and_extracts_values() {
        let body = json!({
            "status":"ok",
            "body": {
                "1699001800": [21.0, 63.0],
                "1699000000": [21.3, 62.0]
            }
        });
        let rows = parse_measure_body(&body, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1699000000, "sorted ascending");
        assert_eq!(rows[0].1[0], Some(21.3));
        assert_eq!(rows[0].1[1], Some(62.0));
        assert_eq!(rows[1].0, 1699001800);
    }

    #[test]
    fn parse_measure_body_handles_missing_trailing_values() {
        let body = json!({"body": {"1699000000": [21.3]}});
        let rows = parse_measure_body(&body, 2);
        assert_eq!(rows[0].1[0], Some(21.3));
        assert_eq!(rows[0].1[1], None, "missing position → None");
    }

    #[test]
    fn parse_measure_body_skips_non_epoch_keys() {
        let body = json!({"body": {"not_a_number": [1.0], "1699000000": [2.0]}});
        let rows = parse_measure_body(&body, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 1699000000);
    }

    // -----------------------------------------------------------------------
    // Tests: HomeReading mapping.

    fn base_module() -> ModuleDesc {
        ModuleDesc {
            station_id: "70:ee:50:aa:bb:cc".to_string(),
            module_id: "70:ee:50:aa:bb:cc".to_string(),
            station_name: "Home Station".to_string(),
            module_name: "Home Station".to_string(),
            types: vec![0, 1], // Temperature, Humidity
        }
    }

    #[test]
    fn readings_from_measure_emits_one_per_present_metric() {
        let module = base_module();
        let rows = readings_from_measure(1699000000, &[Some(21.3), Some(62.0)], &[0, 1], &module);
        assert_eq!(rows.len(), 2);

        let temp = rows.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(temp.source, "netatmo");
        assert_eq!(temp.value, 21.3);
        assert_eq!(temp.unit, "C");
        assert_eq!(temp.place, "Home Station");
        assert_eq!(temp.device, "70:ee:50:aa:bb:cc");

        let guid = temp.extra.get("guid").and_then(Value::as_str).unwrap();
        assert!(
            guid.contains("netatmo:70:ee:50:aa:bb:cc:70:ee:50:aa:bb:cc:temperature:1699000000"),
            "guid: {guid}"
        );

        let hum = rows.iter().find(|r| r.metric == "humidity").unwrap();
        assert_eq!(hum.value, 62.0);
        assert_eq!(hum.unit, "percent");
    }

    #[test]
    fn readings_from_measure_skips_none_values() {
        let module = base_module();
        let rows = readings_from_measure(1699000000, &[None, Some(62.0)], &[0, 1], &module);
        assert_eq!(rows.len(), 1, "None temperature skipped");
        assert_eq!(rows[0].metric, "humidity");
    }

    // -----------------------------------------------------------------------
    // Tests: integration (scripted mock).

    #[test]
    fn full_pull_writes_contract_raw_and_stations_layers() {
        let v = temp_vault("fullpull");
        // simple_stations_body() is NAMain (6 metrics: Temperature, Humidity, CO2, Noise,
        // Pressure, AbsolutePressure). We supply real data for the first two metrics;
        // the remaining four return empty bodies from the MockApi default. Net: 2 readings.
        let temp_m = measure_body(&[(1699000000, 21.3)]);
        let hum_m = measure_body(&[(1699000000, 62.0)]);
        let api = MockApi::new(simple_stations_body(), vec![temp_m, hum_m]);

        let out = pull_with(&v, &api, "fake_token").unwrap();
        assert_eq!(out.counts.get("readings"), Some(&2), "2 contract rows");

        // Contract layer.
        let contract = v.stream(CONTRACT_DIR, Partition::Month);
        let mut all: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        assert_eq!(all.len(), 2);
        let temp = all.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(temp.value, 21.3);
        assert_eq!(temp.unit, "C");
        assert!(
            all.iter().all(|r| r.extra.get("guid").is_some()),
            "every row has a guid"
        );

        // Raw layer.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let raw_count: usize = raw
            .partitions()
            .unwrap()
            .iter()
            .map(|k| raw.read::<Value>(k).unwrap().len())
            .sum();
        assert!(raw_count >= 1, "raw measure lines written");

        // Stations snapshot.
        let snap = v.stream(STATIONS_RAW_DIR, Partition::Month);
        let snap_count: usize = snap
            .partitions()
            .unwrap()
            .iter()
            .map(|k| snap.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(snap_count, 1, "stations snapshot stored");

        // Watermark advanced.
        let state = v.read_netatmo_sync();
        assert!(!state.watermarks.is_empty());
        assert!(state.updated.is_some());
    }

    #[test]
    fn pull_is_idempotent_via_guid_dedupe() {
        let v = temp_vault("idempotent");
        let body = measure_body(&[(1699000000, 21.3)]);
        let empty = json!({"status":"ok","body":{}});

        let api1 = MockApi::new(simple_stations_body(), vec![body.clone(), empty.clone()]);
        let out1 = pull_with(&v, &api1, "tok").unwrap();
        assert!(out1.counts.get("readings").copied().unwrap_or(0) > 0);

        let api2 = MockApi::new(simple_stations_body(), vec![body.clone(), empty.clone()]);
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(
            out2.counts.get("readings").copied().unwrap_or(0),
            0,
            "all guids already stored"
        );
    }

    #[test]
    fn pull_empty_measures_writes_nothing() {
        let v = temp_vault("empty");
        let empty = json!({"status":"ok","body":{}});
        let api = MockApi::new(
            simple_stations_body(),
            vec![empty.clone(), empty.clone()],
        );
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 0);
    }

    #[test]
    fn sync_state_back_compat_deserialize() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermarks.is_empty());
        assert!(empty.updated.is_none());

        let partial: SyncState = serde_json::from_str(
            r#"{"watermarks":{"70:ee:50:aa:bb:cc:70:ee:50:aa:bb:cc:Temperature":1699000000}}"#,
        )
        .unwrap();
        assert_eq!(
            partial
                .watermarks
                .get("70:ee:50:aa:bb:cc:70:ee:50:aa:bb:cc:Temperature"),
            Some(&1699000000)
        );
        assert!(partial.updated.is_none(), "old cursor missing `updated` still loads");
    }

    #[test]
    fn provider_port_matches_assigned_index() {
        assert_eq!(NETATMO.redirect_port, 38739, "38580 + 159 = 38739");
    }

    #[test]
    fn connection_and_def_wired_correctly() {
        assert!(CONNECTION.method("oauth").is_some(), "OAuth method present");
        assert_eq!(CONNECTION.id, "netatmo");
        assert_eq!(DEF.connection, Some("netatmo"));
        assert_eq!(DEF.meta.id, "netatmo");
        assert_eq!(DEF.meta.domain, "home");
        assert!(!DEF.meta.default_on, "opt-in — requires OAuth setup");
    }
}
