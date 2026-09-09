//! Google Maps Pollen API — daily pollen forecast for 65+ countries.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/google-pollen.md
//!
//! A **Periodic** cloud pull (once daily): tree/grass/weed UPI (Universal Pollen
//! Index) and optional species breakdown from `pollen.googleapis.com/v1/forecast:lookup`.
//! Covers 65+ countries with strong US coverage.
//!
//! Auth: a Google Cloud API key pasted by the user (NOT the shared `google` OAuth
//! login — the Pollen API uses a billing-enabled Cloud project key, a different
//! auth model). The key is stored in the 0600 secret store under
//! `.trove/sync/google-pollen.json`.
//!
//! Two vault layers:
//! - **Raw** — `environment/google-pollen/raw/YYYY-MM.jsonl` — the full daily
//!   forecast objects verbatim, one per forecast day, full fidelity.
//! - **Contract** — `environment/google-pollen/YYYY-MM.jsonl` — one
//!   [`EnvReading`] per pollen type (tree/grass/weed) per day, with species
//!   detail in `extra`. Deduped by `guid = "gpollen:{YYYY-MM-DD}:{metric}:{lat},{lon}"`
//!   (upserts — a re-pull of the same forecast day updates in place).
//!
//! Location: reuses the same weather location ladder as [`crate::nws`] and
//! [`crate::airnow`]: CoreLocation → manual weather location → last-used cursor.
//!
//! Rate budget: ~1 call/day ≈ 365/year, trivially inside the 5,000/month free tier.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
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
const DIR: &str = "environment/google-pollen";
const RAW_DIR: &str = "environment/google-pollen/raw";
/// Non-secret rebuildable cursor — last location + last poll time.
const SYNC_FILE: &str = ".trove/google-pollen-sync.json";

const SERVICE: &str = "google-pollen";
const SOURCE: &str = "google-pollen";
const API_BASE: &str = "https://pollen.googleapis.com";
/// 5-day window — the API supports up to 5 days; one call per daily sync.
const FORECAST_DAYS: u32 = 5;
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Daily cadence: pollen forecasts update once daily; more polling is wasteful
/// against the 5k/month free-tier quota.
pub const GOOGLE_POLLEN_SYNC_SECS: u64 = 86_400;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_google_pollen_sync()
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
                format!("google-pollen synced — {n} pollen readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "google-pollen sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "Google Pollen: nothing new (no location, no key, or already up to date)".to_string()
    } else {
        format!("Google Pollen synced — {n} pollen readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-pollen",
        name: "Google Pollen",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Daily pollen-count forecasts (tree, grass, weed) from Google's \
                      Maps Pollen API, covering 65+ countries at local resolution. \
                      Includes species-level breakdown (oak, birch, ragweed, …) with \
                      the Universal Pollen Index (0–5 scale).",
        domain: "environment",
        vault_path: "environment/google-pollen/",
        toggleable: true,
        setup: &[
            "Enable the Pollen API in a Google Cloud project with a billing account \
             (free tier: 5,000 calls/month — ~365/year for daily polling).",
            "Create an API key in the Cloud Console (APIs & Services → Credentials) \
             restricted to the Pollen API.",
            "Paste the key in the connect card below.",
            "Approve Location Services when asked, or set a location manually on the \
             Weather tab.",
        ],
        caveats: "Requires a Google Cloud API key tied to a billing-enabled project — \
                  even the free tier (5,000 calls/month) needs a billing account. \
                  Coverage varies by region: best in the US; 65+ countries total. \
                  Species detail is only available for regions Google has plant data for.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(GOOGLE_POLLEN_SYNC_SECS),
        collect: def_collect,
    },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: Some("google-pollen"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — a Google Cloud API key).

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
            label: "Google Cloud API key (configured)".to_string(),
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
/// A distinct TokenPaste connection — NOT the shared `google` OAuth login.
/// The Pollen API uses a Cloud project API key (billing required even for
/// the free tier), which is a different credential type and lifecycle from
/// the OAuth-based Google services.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "google-pollen",
    display_name: "Google Pollen (Cloud API key)",
    methods: &[ConnectMethod::TokenPaste {
        label: "Google Cloud API key",
        help: "Create an API key in the Google Cloud Console (APIs & Services → Credentials) \
               for a project with the Pollen API enabled and a billing account attached. \
               Even the free tier (5,000 calls/month) requires a billing account.",
        placeholder: "AIzaSy…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["google-pollen"],
    setup: &[
        "Enable the Pollen API in a Google Cloud project (console.cloud.google.com).",
        "Attach a billing account to that project (required even for the free tier).",
        "Create an API key restricted to the Pollen API and paste it here.",
    ],
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state: last-used location + last poll time.
/// Losing it costs nothing — the next pass re-resolves the location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct GooglePollenSyncState {
    /// RFC3339 local time of the last pass that got as far as a decision.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// Last-used latitude (0.0 = unset).
    #[serde(default)]
    pub lat: f64,
    /// Last-used longitude (0.0 = unset).
    #[serde(default)]
    pub lon: f64,
    /// Non-empty while stuck (no location, no key, network error) — for the UI.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_google_pollen_sync(&self) -> Option<GooglePollenSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_google_pollen_sync(&self, state: &GooglePollenSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// API shapes — confirmed from the official Google Maps Pollen API reference
// at developers.google.com/maps/documentation/pollen/reference/rest/v1/forecast/lookup
//
// Field names verified against the documented REST response schema:
//  - `dailyInfo[]` — per-day array
//  - `dailyInfo[].date` — `{year, month, day}` integer object (NOT a string)
//  - `dailyInfo[].pollenTypeInfo[]` — tree/grass/weed aggregate with `code`,
//    `displayName`, `inSeason`, `indexInfo`, `healthRecommendations`
//  - `dailyInfo[].plantInfo[]` — per-species with `code`, `displayName`,
//    `inSeason`, `indexInfo`, `plantDescription`
//  - `indexInfo.code` = "UPI"; `indexInfo.value` = integer 0–5;
//    `indexInfo.category` = "None"/"Very Low"/"Low"/"Moderate"/"High"/"Very High"

/// The top-level response from `pollen.googleapis.com/v1/forecast:lookup`.
#[derive(Debug, Clone, Deserialize)]
pub struct PollenForecastResponse {
    #[serde(default, rename = "regionCode")]
    pub region_code: String,
    #[serde(default, rename = "dailyInfo")]
    pub daily_info: Vec<DailyInfo>,
    /// Pagination token — present when the API paginates a multi-day request.
    /// We request 5 days in one call which is within the single-page limit.
    #[serde(default, rename = "nextPageToken")]
    pub next_page_token: String,
}

/// One day of forecast data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DailyInfo {
    /// The calendar date as `{year, month, day}` integers.
    pub date: Option<DateParts>,
    /// Aggregate by pollen type (TREE, GRASS, WEED).
    #[serde(default, rename = "pollenTypeInfo")]
    pub pollen_type_info: Vec<PollenTypeInfo>,
    /// Per-species breakdown (BIRCH, GRAMINALES, RAGWEED, …).
    #[serde(default, rename = "plantInfo")]
    pub plant_info: Vec<PlantInfo>,
}

/// Google's date object: `{year, month, day}` as integers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateParts {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl DateParts {
    /// Render as a `YYYY-MM-DD` string. Returns `None` if invalid.
    pub fn to_date_str(&self) -> Option<String> {
        NaiveDate::from_ymd_opt(self.year, self.month, self.day)
            .map(|d| d.format("%Y-%m-%d").to_string())
    }
}

/// Aggregate pollen type entry (TREE, GRASS, or WEED).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollenTypeInfo {
    /// Enum code: `"GRASS"`, `"TREE"`, `"WEED"`.
    pub code: String,
    #[serde(default, rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "indexInfo")]
    pub index_info: Option<IndexInfo>,
    #[serde(default, rename = "healthRecommendations")]
    pub health_recommendations: Vec<String>,
    #[serde(default, rename = "inSeason")]
    pub in_season: bool,
}

/// Per-species plant entry (BIRCH, OAK, GRAMINALES, RAGWEED, …).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlantInfo {
    /// Species code: `"BIRCH"`, `"OAK"`, `"GRAMINALES"`, `"RAGWEED"`, …
    pub code: String,
    #[serde(default, rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "indexInfo")]
    pub index_info: Option<IndexInfo>,
    #[serde(default, rename = "inSeason")]
    pub in_season: bool,
    /// Detailed plant description (family, season, cross-reactions, …).
    #[serde(rename = "plantDescription")]
    pub plant_description: Option<Value>,
}

/// Universal Pollen Index — identical shape for both type and plant entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexInfo {
    /// Always `"UPI"` (Universal Pollen Index).
    pub code: String,
    #[serde(default, rename = "displayName")]
    pub display_name: String,
    /// Numeric UPI value 0–5.
    pub value: Option<i64>,
    /// Human category: `"None"`, `"Very Low"`, `"Low"`, `"Moderate"`,
    /// `"High"`, `"Very High"`.
    #[serde(default)]
    pub category: String,
    /// Narrative description of the index level.
    #[serde(default, rename = "indexDescription")]
    pub index_description: String,
    /// RGB colour for display; stored verbatim in `extra` if present.
    #[serde(default)]
    pub color: Option<Value>,
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

trait PollenApi {
    fn forecast(&self, lat: f64, lon: f64, days: u32, key: &str) -> Result<Value>;
}

struct PollenClient {
    base: String,
}

impl PollenClient {
    fn new(base: String) -> Self {
        PollenClient { base }
    }
}

impl PollenApi for PollenClient {
    fn forecast(&self, lat: f64, lon: f64, days: u32, key: &str) -> Result<Value> {
        let url = format!(
            "{}/v1/forecast:lookup?location.latitude={lat}&location.longitude={lon}&days={days}&key={key}",
            self.base
        );
        ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .call()
            .context("requesting Google Pollen forecast")?
            .into_json()
            .context("reading Google Pollen forecast response")
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// The metric name for each aggregate pollen-type code.
/// `"GRASS"` → `"pollen_grass_upi"`, etc.
fn type_metric(code: &str) -> String {
    match code {
        "GRASS" => "pollen_grass_upi".into(),
        "TREE" => "pollen_tree_upi".into(),
        "WEED" => "pollen_weed_upi".into(),
        other => format!("pollen_{}_upi", other.to_lowercase()),
    }
}

/// Parse a full forecast response into (contract readings, raw daily objects).
///
/// Each [`DailyInfo`] entry yields one [`EnvReading`] per pollen type present
/// in `pollenTypeInfo` (the aggregate types: tree/grass/weed). Species detail
/// rides in `extra.species` as a JSON object keyed by plant code → UPI value.
/// The raw layer stores the complete `DailyInfo` value verbatim.
///
/// `ts` for each reading is `{YYYY-MM-DD}T00:00:00+00:00` (UTC midnight of the
/// forecast date) — consistent, partition-safe, and clearly a forecast date not
/// an observation moment. The `guid` is `gpollen:{date}:{metric}:{lat},{lon}`
/// (round-trip stable, upsert-safe across re-polls).
pub fn parse_forecast(
    body: &Value,
    lat: f64,
    lon: f64,
) -> (Vec<EnvReading>, Vec<Value>) {
    let resp: PollenForecastResponse = match serde_json::from_value(body.clone()) {
        Ok(r) => r,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let region = resp.region_code.clone();
    let lat_s = format!("{lat:.4}");
    let lon_s = format!("{lon:.4}");
    let coord_key = format!("{lat_s},{lon_s}");

    let mut readings: Vec<EnvReading> = Vec::new();
    let mut raws: Vec<Value> = Vec::new();

    for day in &resp.daily_info {
        let date_parts = match &day.date {
            Some(d) => d,
            None => continue,
        };
        let date_str = match date_parts.to_date_str() {
            Some(s) => s,
            None => continue,
        };
        // ts = UTC midnight of the forecast date. This is a forecast day, not an
        // observation instant; UTC midnight is unambiguous and partition-safe.
        let ts = format!("{date_str}T00:00:00+00:00");

        // Build the species map for `extra`: plant_code → UPI value.
        let mut species_map: Map<String, Value> = Map::new();
        for plant in &day.plant_info {
            if let Some(info) = &plant.index_info {
                if let Some(v) = info.value {
                    species_map.insert(
                        plant.code.to_lowercase(),
                        Value::Number(v.into()),
                    );
                }
            }
        }

        // One reading per aggregate pollen type that has an index value.
        for ptype in &day.pollen_type_info {
            let index_info = match &ptype.index_info {
                Some(i) => i,
                None => continue,
            };
            let upi = match index_info.value {
                Some(v) => v,
                None => continue,
            };

            let metric = type_metric(&ptype.code);
            let guid = format!("gpollen:{date_str}:{metric}:{coord_key}");

            let mut extra: Map<String, Value> = Map::new();
            extra.insert("category".into(), Value::String(index_info.category.clone()));
            if !index_info.index_description.is_empty() {
                extra.insert(
                    "index_description".into(),
                    Value::String(index_info.index_description.clone()),
                );
            }
            extra.insert("in_season".into(), Value::Bool(ptype.in_season));
            if !ptype.health_recommendations.is_empty() {
                extra.insert(
                    "health_recommendations".into(),
                    Value::Array(
                        ptype
                            .health_recommendations
                            .iter()
                            .map(|s| Value::String(s.clone()))
                            .collect(),
                    ),
                );
            }
            if !region.is_empty() {
                extra.insert("region_code".into(), Value::String(region.clone()));
            }
            // Day-wide species map (plant code → UPI). Stored as `day_species`
            // rather than `species` to make clear this covers ALL species for the
            // day, not just those belonging to this pollen type. Google's API does
            // not expose a species-to-type mapping in the response, so we cannot
            // split per type; the raw layer has the full DayInfo with plantInfo
            // for any fine-grained cross-referencing.
            if !species_map.is_empty() {
                extra.insert(
                    "day_species".into(),
                    Value::Object(species_map.clone()),
                );
            }

            readings.push(EnvReading {
                ts: ts.clone(),
                source: SOURCE.into(),
                metric,
                value: upi as f64,
                unit: "index".into(),
                place: String::new(),
                lat: Some(lat),
                lon: Some(lon),
                station: String::new(),
                guid: Some(guid),
                extra,
            });
        }

        // Raw layer: the full DayInfo object verbatim (one line per forecast day).
        raws.push(serde_json::to_value(day).unwrap_or(Value::Null));
    }

    (readings, raws)
}

// ---------------------------------------------------------------------------
// Upsert helpers (same pattern as nws.rs / airnow.rs).

/// Upsert contract readings into `environment/google-pollen/YYYY-MM.jsonl`
/// by `guid`. A re-poll of the same forecast day updates in place, never
/// duplicates.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvReading>> = BTreeMap::new();
    for r in rows {
        let key = Partition::Month
            .key(&r.ts)
            .with_context(|| format!("google-pollen reading {:?} has unpartitionable ts {:?}", r.guid, r.ts))?;
        by_key.entry(key.to_string()).or_default().push(r);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<EnvReading> = stream.read(&key)?;
        for row in incoming {
            let slot = existing
                .iter_mut()
                .find(|e| e.guid.is_some() && e.guid == row.guid);
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

/// Raw layer helper: a wrapper that carries `ts` and `guid` for partitioning
/// but serializes as the raw API object verbatim (flatten pattern from nws.rs).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(skip)]
    guid: String,
    #[serde(flatten)]
    value: Value,
}

/// Upsert raw day objects into `environment/google-pollen/raw/YYYY-MM.jsonl`
/// by `guid` (the date key). Full fidelity — never drop what the API returned.
fn upsert_raw(vault: &Vault, lines: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: BTreeMap<String, Vec<&RawLine>> = BTreeMap::new();
    for l in lines {
        let key = Partition::Month
            .key(&l.ts)
            .with_context(|| format!("google-pollen raw {} has unpartitionable ts {:?}", l.guid, l.ts))?;
        by_key.entry(key.to_string()).or_default().push(l);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<Value> = stream.read(&key)?;
        for l in incoming {
            let pos = existing.iter().position(|v| raw_date_key(v) == l.guid);
            match pos {
                Some(i) => existing[i] = l.value.clone(),
                None => existing.push(l.value.clone()),
            }
        }
        vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(())
}

/// Extract the date key from a raw DayInfo value for deduplication.
/// Reconstructs `YYYY-MM-DD` from the `{year, month, day}` date object.
fn raw_date_key(v: &Value) -> String {
    let date = match v.get("date") {
        Some(d) => d,
        None => return String::new(),
    };
    let y = date.get("year").and_then(Value::as_i64).unwrap_or(0);
    let mo = date.get("month").and_then(Value::as_i64).unwrap_or(0);
    let d = date.get("day").and_then(Value::as_i64).unwrap_or(0);
    if y == 0 || mo == 0 || d == 0 {
        return String::new();
    }
    format!("{y:04}-{mo:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// Location ladder (identical to nws.rs / airnow.rs).

fn resolve_point(vault: &Vault, state: &GooglePollenSyncState) -> Option<(f64, f64)> {
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

/// Production entry point: resolve the location + API key, then sync.
/// Inert (no rows, no error) when no location or key is available.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_google_pollen_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error =
            "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_google_pollen_sync(&state)?;
        return Ok(PullOutcome {
            headline: "Google Pollen: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    }
    let api_key = match vault.load_sync_token(SERVICE)? {
        Some(tok) => tok.access_token,
        None => {
            return Ok(PullOutcome {
                headline: "Google Pollen: not connected (paste a Cloud API key to enable)".into(),
                counts: BTreeMap::from([("readings", 0)]),
            });
        }
    };
    let client = PollenClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &api_key, &client)
}

/// Network+write body over an EXPLICIT point and API key — the offline test
/// seam. `None` point ⇒ clean inert no-op.
pub fn pull_at(vault: &Vault, point: Option<(f64, f64)>, api_key: &str) -> Result<PullOutcome> {
    let client = PollenClient::new(API_BASE.to_string());
    pull_at_with(vault, point, api_key, &client)
}

fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    api_key: &str,
    client: &impl PollenApi,
) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        return Ok(PullOutcome {
            headline: "Google Pollen: no location set".into(),
            counts: BTreeMap::from([("readings", 0)]),
        });
    };
    let now = Local::now();
    let mut state = vault.read_google_pollen_sync().unwrap_or_default();

    let body: Value = client.forecast(lat, lon, FORECAST_DAYS, api_key)?;
    let (readings, day_values) = parse_forecast(&body, lat, lon);

    let readings_written = if !readings.is_empty() {
        upsert_readings(vault, &readings)?
    } else {
        0
    };

    // Build raw lines: one per forecast day, keyed by the day's own embedded date.
    // IMPORTANT: derive the date from each raw DayInfo object directly — do NOT
    // zip against the readings list. Days with zero contract readings (e.g. all
    // pollenTypeInfo null or empty pollenTypeInfo for out-of-season days) still
    // produce a raw line (full fidelity, never drop what the API returned).
    if !day_values.is_empty() {
        let raw_lines: Vec<RawLine> = day_values
            .iter()
            .filter_map(|raw_val| {
                let date = raw_date_key(raw_val);
                if date.is_empty() {
                    return None; // malformed day — skip rather than file under wrong slot
                }
                let ts = format!("{date}T00:00:00+00:00");
                Some(RawLine {
                    ts,
                    guid: date,
                    value: raw_val.clone(),
                })
            })
            .collect();
        if !raw_lines.is_empty() {
            upsert_raw(vault, &raw_lines)?;
        }
    }

    // Advance the cursor.
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.error = String::new();
    vault.write_google_pollen_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{readings_written} pollen readings"),
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
        let dir = std::env::temp_dir()
            .join(format!("trove-gpollen-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — authored from the documented API response schema.
    // Field names verified against developers.google.com/maps/documentation/
    // pollen/reference/rest/v1/forecast/lookup:
    //   date = {year, month, day} integers
    //   pollenTypeInfo[].code = "GRASS" | "TREE" | "WEED"
    //   pollenTypeInfo[].indexInfo.code = "UPI"
    //   pollenTypeInfo[].indexInfo.value = integer 0-5
    //   pollenTypeInfo[].indexInfo.category = "None"/"Very Low"/"Low"/etc.
    //   plantInfo[].code = "BIRCH" | "GRAMINALES" | "RAGWEED" | etc.

    /// One-day forecast with all three pollen types and two species.
    fn forecast_one_day() -> Value {
        json!({
            "regionCode": "us",
            "dailyInfo": [
                {
                    "date": {"year": 2026, "month": 6, "day": 17},
                    "pollenTypeInfo": [
                        {
                            "code": "GRASS",
                            "displayName": "Grass",
                            "inSeason": true,
                            "indexInfo": {
                                "code": "UPI",
                                "displayName": "Universal Pollen Index",
                                "value": 2,
                                "category": "Low",
                                "indexDescription": "Pollen levels are low today.",
                                "color": {"red": 1.0, "green": 0.9, "blue": 0.0}
                            },
                            "healthRecommendations": [
                                "Pollen levels are low — most allergy sufferers should be comfortable."
                            ]
                        },
                        {
                            "code": "TREE",
                            "displayName": "Tree",
                            "inSeason": true,
                            "indexInfo": {
                                "code": "UPI",
                                "displayName": "Universal Pollen Index",
                                "value": 3,
                                "category": "Moderate",
                                "indexDescription": "Moderate tree pollen.",
                                "color": {"red": 1.0, "green": 0.5, "blue": 0.0}
                            },
                            "healthRecommendations": []
                        },
                        {
                            "code": "WEED",
                            "displayName": "Weed",
                            "inSeason": false,
                            "indexInfo": {
                                "code": "UPI",
                                "displayName": "Universal Pollen Index",
                                "value": 0,
                                "category": "None",
                                "indexDescription": "No weed pollen.",
                                "color": {"red": 0.0, "green": 0.8, "blue": 0.0}
                            },
                            "healthRecommendations": []
                        }
                    ],
                    "plantInfo": [
                        {
                            "code": "GRAMINALES",
                            "displayName": "Grass",
                            "inSeason": true,
                            "indexInfo": {
                                "code": "UPI",
                                "value": 2,
                                "category": "Low"
                            }
                        },
                        {
                            "code": "OAK",
                            "displayName": "Oak",
                            "inSeason": true,
                            "indexInfo": {
                                "code": "UPI",
                                "value": 3,
                                "category": "Moderate"
                            }
                        }
                    ]
                }
            ]
        })
    }

    /// Multi-day forecast (3 days) — proves partitioning and cursor advance.
    fn forecast_three_days() -> Value {
        json!({
            "regionCode": "us",
            "dailyInfo": [
                {
                    "date": {"year": 2026, "month": 6, "day": 17},
                    "pollenTypeInfo": [
                        {"code": "GRASS", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 1, "category": "Very Low"}},
                        {"code": "TREE", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 4, "category": "High"}},
                        {"code": "WEED", "inSeason": false,
                         "indexInfo": {"code": "UPI", "value": 0, "category": "None"}}
                    ],
                    "plantInfo": []
                },
                {
                    "date": {"year": 2026, "month": 6, "day": 18},
                    "pollenTypeInfo": [
                        {"code": "GRASS", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 2, "category": "Low"}},
                        {"code": "TREE", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 3, "category": "Moderate"}},
                        {"code": "WEED", "inSeason": false,
                         "indexInfo": {"code": "UPI", "value": 0, "category": "None"}}
                    ],
                    "plantInfo": []
                },
                {
                    "date": {"year": 2026, "month": 7, "day": 1},
                    "pollenTypeInfo": [
                        {"code": "TREE", "inSeason": false,
                         "indexInfo": {"code": "UPI", "value": 0, "category": "None"}}
                    ],
                    "plantInfo": []
                }
            ]
        })
    }

    /// Empty forecast response (no dailyInfo, happens outside coverage).
    fn forecast_empty() -> Value {
        json!({"regionCode": "xx", "dailyInfo": []})
    }

    /// Day with no indexInfo on a type — that type must be skipped.
    fn forecast_missing_index() -> Value {
        json!({
            "regionCode": "us",
            "dailyInfo": [{
                "date": {"year": 2026, "month": 6, "day": 17},
                "pollenTypeInfo": [
                    {"code": "GRASS", "inSeason": true, "indexInfo": null},
                    {"code": "TREE", "inSeason": true,
                     "indexInfo": {"code": "UPI", "value": 2, "category": "Low"}}
                ],
                "plantInfo": []
            }]
        })
    }

    // Stub API client for offline tests.
    struct Stub { body: Value }
    impl PollenApi for Stub {
        fn forecast(&self, _lat: f64, _lon: f64, _days: u32, _key: &str) -> Result<Value> {
            Ok(self.body.clone())
        }
    }

    const SEED: (f64, f64) = (34.05, -118.24);
    const API_KEY: &str = "test-key";

    // -----------------------------------------------------------------------
    // Pure parser tests.

    #[test]
    fn parse_one_day_yields_three_readings_and_raw() {
        let (readings, raws) = parse_forecast(&forecast_one_day(), 34.05, -118.24);
        // Three pollen types: GRASS, TREE, WEED.
        assert_eq!(readings.len(), 3, "one reading per pollen type");
        assert_eq!(raws.len(), 1, "one raw object per forecast day");

        let grass = readings.iter().find(|r| r.metric == "pollen_grass_upi").unwrap();
        assert_eq!(grass.value, 2.0);
        assert_eq!(grass.unit, "index");
        assert_eq!(grass.source, "google-pollen");
        assert_eq!(grass.lat, Some(34.05));
        assert_eq!(grass.lon, Some(-118.24));
        assert_eq!(grass.ts, "2026-06-17T00:00:00+00:00");
        // guid is stable and includes the date, metric, and coords.
        let guid = grass.guid.as_deref().unwrap();
        assert!(guid.starts_with("gpollen:2026-06-17:pollen_grass_upi:"), "guid = {guid}");
        // extra carries category, in_season, health_recommendations, and species.
        assert_eq!(grass.extra.get("category"), Some(&json!("Low")));
        assert_eq!(grass.extra.get("in_season"), Some(&json!(true)));
        assert!(grass.extra.get("health_recommendations").is_some());
        // day_species is day-wide (all plants for the day), not type-scoped.
        assert!(grass.extra.get("day_species").is_some(), "day_species detail in extra");
        let species = grass.extra.get("day_species").unwrap();
        assert!(species.get("graminales").is_some(), "GRAMINALES in day_species");
        assert!(species.get("oak").is_some(), "OAK in day_species");
        assert_eq!(species.get("graminales"), Some(&json!(2)));

        let tree = readings.iter().find(|r| r.metric == "pollen_tree_upi").unwrap();
        assert_eq!(tree.value, 3.0);
        assert_eq!(tree.extra.get("category"), Some(&json!("Moderate")));

        let weed = readings.iter().find(|r| r.metric == "pollen_weed_upi").unwrap();
        assert_eq!(weed.value, 0.0);
        assert_eq!(weed.extra.get("in_season"), Some(&json!(false)));
    }

    #[test]
    fn parse_three_days_yields_nine_readings_and_three_raws() {
        // 2 days × 3 types + 1 day × 1 type = 7 readings; 3 raw objects.
        let (readings, raws) = parse_forecast(&forecast_three_days(), 34.05, -118.24);
        assert_eq!(readings.len(), 7, "2 full days + 1 partial day");
        assert_eq!(raws.len(), 3, "one raw per day");

        // Day spanning a month boundary (June→July) must partition correctly.
        let july = readings.iter().find(|r| r.ts.starts_with("2026-07")).unwrap();
        assert_eq!(july.metric, "pollen_tree_upi");
        assert_eq!(july.value, 0.0);
    }

    #[test]
    fn parse_empty_forecast_yields_no_readings() {
        let (readings, raws) = parse_forecast(&forecast_empty(), 34.05, -118.24);
        assert_eq!(readings.len(), 0);
        assert_eq!(raws.len(), 0);
    }

    #[test]
    fn type_without_index_info_is_skipped() {
        let (readings, _) = parse_forecast(&forecast_missing_index(), 34.05, -118.24);
        // GRASS has null indexInfo → skipped; TREE has value 2 → 1 reading.
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].metric, "pollen_tree_upi");
    }

    #[test]
    fn guid_is_stable_and_unique_per_date_and_metric() {
        let (readings, _) = parse_forecast(&forecast_one_day(), 34.05, -118.24);
        let guids: Vec<_> = readings.iter().map(|r| r.guid.as_deref().unwrap()).collect();
        let unique: std::collections::HashSet<_> = guids.iter().collect();
        assert_eq!(guids.len(), unique.len(), "all guids are unique");
        // Every guid starts with "gpollen:" and embeds the date.
        for g in &guids {
            assert!(g.starts_with("gpollen:2026-06-17:"), "guid = {g}");
        }
    }

    #[test]
    fn date_parts_to_date_str() {
        assert_eq!(
            DateParts { year: 2026, month: 6, day: 17 }.to_date_str(),
            Some("2026-06-17".into())
        );
        assert_eq!(
            DateParts { year: 2026, month: 2, day: 29 }.to_date_str(),
            None, // 2026 is not a leap year
        );
        assert_eq!(
            DateParts { year: 2024, month: 2, day: 29 }.to_date_str(),
            Some("2024-02-29".into()) // 2024 is a leap year
        );
    }

    #[test]
    fn type_metric_names() {
        assert_eq!(type_metric("GRASS"), "pollen_grass_upi");
        assert_eq!(type_metric("TREE"), "pollen_tree_upi");
        assert_eq!(type_metric("WEED"), "pollen_weed_upi");
        assert_eq!(type_metric("MOLD"), "pollen_mold_upi"); // future-proof
    }

    // -----------------------------------------------------------------------
    // Pull / store / dedupe tests.

    #[test]
    fn pull_writes_readings_and_raw_layer() {
        let v = temp_vault("pull");
        let stub = Stub { body: forecast_one_day() };
        let out = pull_at_with(&v, Some(SEED), API_KEY, &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&3));

        // Contract layer: environment/google-pollen/2026-06.jsonl — 3 readings.
        let contract_path = v.root().join("environment/google-pollen/2026-06.jsonl");
        assert!(contract_path.exists(), "contract file written");
        let lines: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|r| r.metric == "pollen_grass_upi"));
        assert!(lines.iter().any(|r| r.metric == "pollen_tree_upi"));
        assert!(lines.iter().any(|r| r.metric == "pollen_weed_upi"));

        // Raw layer: environment/google-pollen/raw/2026-06.jsonl — 1 day object.
        let raw_path = v.root().join("environment/google-pollen/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw file written");
        let raw_body = std::fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw_body.lines().count(), 1, "one raw line per forecast day");
        // Verbatim structure: date object, pollenTypeInfo, plantInfo present.
        assert!(raw_body.contains("\"date\""), "raw keeps date object");
        assert!(raw_body.contains("pollenTypeInfo"), "raw keeps pollenTypeInfo");
        assert!(raw_body.contains("plantInfo"), "raw keeps plantInfo");

        // Cursor advanced.
        let state = v.read_google_pollen_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.lat, 34.05);
        assert_eq!(state.lon, -118.24);
        assert!(state.error.is_empty());
    }

    #[test]
    fn repoll_does_not_duplicate_upserts_by_guid() {
        let v = temp_vault("dedup");
        let stub = Stub { body: forecast_one_day() };
        // First poll.
        pull_at_with(&v, Some(SEED), API_KEY, &stub).unwrap();
        let after_1: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(after_1.len(), 3);

        // Re-poll with the same data.
        pull_at_with(&v, Some(SEED), API_KEY, &stub).unwrap();
        let after_2: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(after_2.len(), 3, "upsert by guid — no duplicate rows");
    }

    #[test]
    fn updated_forecast_upserts_in_place() {
        let v = temp_vault("upsert");
        // First poll: grass = 2.
        let stub1 = Stub { body: forecast_one_day() };
        pull_at_with(&v, Some(SEED), API_KEY, &stub1).unwrap();

        // Second poll: grass index revised to 4.
        let revised = json!({
            "regionCode": "us",
            "dailyInfo": [{
                "date": {"year": 2026, "month": 6, "day": 17},
                "pollenTypeInfo": [
                    {"code": "GRASS", "inSeason": true,
                     "indexInfo": {"code": "UPI", "value": 4, "category": "High"}},
                    {"code": "TREE", "inSeason": true,
                     "indexInfo": {"code": "UPI", "value": 3, "category": "Moderate"}},
                    {"code": "WEED", "inSeason": false,
                     "indexInfo": {"code": "UPI", "value": 0, "category": "None"}}
                ],
                "plantInfo": []
            }]
        });
        let stub2 = Stub { body: revised };
        pull_at_with(&v, Some(SEED), API_KEY, &stub2).unwrap();

        let rows: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 3, "still 3 rows — upsert, not append");
        let grass = rows.iter().find(|r| r.metric == "pollen_grass_upi").unwrap();
        assert_eq!(grass.value, 4.0, "grass updated to new value");
        assert_eq!(
            grass.extra.get("category"),
            Some(&json!("High")),
            "category updated"
        );
    }

    #[test]
    fn multi_month_forecast_partitions_correctly() {
        let v = temp_vault("multimonth");
        let stub = Stub { body: forecast_three_days() };
        pull_at_with(&v, Some(SEED), API_KEY, &stub).unwrap();

        let june: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        let july: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-07").unwrap();
        // June 17 (3 types) + June 18 (3 types) = 6; July 1 (1 type) = 1.
        assert_eq!(june.len(), 6, "June readings");
        assert_eq!(july.len(), 1, "July reading in separate partition");
    }

    #[test]
    fn empty_response_writes_no_rows_no_error() {
        let v = temp_vault("empty");
        let stub = Stub { body: forecast_empty() };
        let out = pull_at_with(&v, Some(SEED), API_KEY, &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/google-pollen/2026-06.jsonl").exists());
        assert!(!v.root().join("environment/google-pollen/raw").exists());
    }

    #[test]
    fn no_point_is_inert() {
        let v = temp_vault("inert");
        let stub = Stub { body: forecast_one_day() };
        let out = pull_at_with(&v, None, API_KEY, &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        assert!(!v.root().join("environment/google-pollen").exists());
    }

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("cursor");
        let state = GooglePollenSyncState {
            updated: "2026-06-17T00:00:00+00:00".into(),
            lat: 34.05,
            lon: -118.24,
            error: String::new(),
        };
        v.write_google_pollen_sync(&state).unwrap();
        let loaded = v.read_google_pollen_sync().unwrap();
        assert_eq!(loaded.lat, 34.05);
        assert_eq!(loaded.lon, -118.24);
        assert_eq!(loaded.updated, "2026-06-17T00:00:00+00:00");
        assert!(loaded.error.is_empty());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Back-compat: sparse reading (only the 4 required fields) must parse —
        // proves the upsert read path tolerates old minimal data.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/google-pollen")).unwrap();
        std::fs::write(
            v.root().join("environment/google-pollen/2026-06.jsonl"),
            "{\"ts\":\"2026-06-17T00:00:00+00:00\",\"source\":\"google-pollen\",\"metric\":\"pollen_tree_upi\",\"value\":2}\n",
        ).unwrap();
        let rows: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, 2.0);
        assert_eq!(rows[0].guid, None, "sparse row has no guid");
    }

    #[test]
    fn raw_date_key_extracts_date_from_day_info() {
        let day = json!({"date": {"year": 2026, "month": 6, "day": 17}, "pollenTypeInfo": []});
        assert_eq!(raw_date_key(&day), "2026-06-17");
        // Missing date.
        assert_eq!(raw_date_key(&json!({})), "");
        // Partial date.
        assert_eq!(raw_date_key(&json!({"date": {"year": 2026, "month": 6}})), "");
    }

    /// Regression test for raw-layer desync bug:
    /// When a middle forecast day has no contract readings (empty pollenTypeInfo
    /// or all indexInfo null), the raw layer must still contain a line for that
    /// day — and each raw line's embedded date must match its position.
    ///
    /// Input: days A (2 types), B (empty pollenTypeInfo), C (1 type).
    /// Expected raw: 3 lines — 2026-06-17, 2026-06-18, 2026-06-19.
    /// Expected contract: 3 readings (A's 2 + C's 1).
    #[test]
    fn raw_layer_includes_zero_reading_days_no_desync() {
        let body = json!({
            "regionCode": "us",
            "dailyInfo": [
                {
                    "date": {"year": 2026, "month": 6, "day": 17},
                    "pollenTypeInfo": [
                        {"code": "GRASS", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 2, "category": "Low"}},
                        {"code": "TREE", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 3, "category": "Moderate"}}
                    ],
                    "plantInfo": []
                },
                {
                    // Day B: out of season — empty pollenTypeInfo (no readings produced).
                    "date": {"year": 2026, "month": 6, "day": 18},
                    "pollenTypeInfo": [],
                    "plantInfo": []
                },
                {
                    "date": {"year": 2026, "month": 6, "day": 19},
                    "pollenTypeInfo": [
                        {"code": "TREE", "inSeason": false,
                         "indexInfo": {"code": "UPI", "value": 0, "category": "None"}}
                    ],
                    "plantInfo": []
                }
            ]
        });

        // ---- parse_forecast layer ----
        let (readings, raws) = parse_forecast(&body, 34.05, -118.24);
        // Day A: 2 types → 2 readings; Day B: 0 types → 0 readings; Day C: 1 type → 1 reading.
        assert_eq!(readings.len(), 3, "3 contract readings across 3 days (day B contributes 0)");
        // All 3 raw day objects present, regardless of contract-layer output.
        assert_eq!(raws.len(), 3, "raw layer: all 3 forecast days present");
        // Each raw's embedded date matches its slot — verify via raw_date_key.
        assert_eq!(raw_date_key(&raws[0]), "2026-06-17", "raw[0] date = day A");
        assert_eq!(raw_date_key(&raws[1]), "2026-06-18", "raw[1] date = day B (zero readings)");
        assert_eq!(raw_date_key(&raws[2]), "2026-06-19", "raw[2] date = day C");

        // ---- pull_at_with round-trip (vault) ----
        let v = temp_vault("raw_desync");
        let stub = Stub { body: body.clone() };
        let out = pull_at_with(&v, Some(SEED), API_KEY, &stub).unwrap();
        assert_eq!(out.counts.get("readings"), Some(&3), "3 contract readings written");

        // Raw vault file must contain 3 lines.
        let raw_path = v.root().join("environment/google-pollen/raw/2026-06.jsonl");
        assert!(raw_path.exists(), "raw file written");
        let raw_body = std::fs::read_to_string(&raw_path).unwrap();
        let raw_lines_written: Vec<Value> = raw_body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(raw_lines_written.len(), 3, "3 raw lines in vault (one per day, including zero-reading day B)");

        // Each raw line's embedded date matches its expected slot.
        let dates: Vec<String> = raw_lines_written.iter().map(raw_date_key).collect();
        assert!(dates.contains(&"2026-06-17".to_string()), "day A in raw: {dates:?}");
        assert!(dates.contains(&"2026-06-18".to_string()), "day B in raw (zero readings): {dates:?}");
        assert!(dates.contains(&"2026-06-19".to_string()), "day C in raw: {dates:?}");
    }

    /// Variant of the desync regression: middle day has pollenTypeInfo present
    /// but all entries have indexInfo: null (also produces zero contract readings).
    #[test]
    fn raw_layer_includes_null_index_days_no_desync() {
        let body = json!({
            "regionCode": "us",
            "dailyInfo": [
                {
                    "date": {"year": 2026, "month": 6, "day": 17},
                    "pollenTypeInfo": [
                        {"code": "GRASS", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 1, "category": "Very Low"}}
                    ],
                    "plantInfo": []
                },
                {
                    // Day B: all indexInfo null — produces zero contract readings.
                    "date": {"year": 2026, "month": 6, "day": 18},
                    "pollenTypeInfo": [
                        {"code": "GRASS", "inSeason": true, "indexInfo": null},
                        {"code": "TREE", "inSeason": true, "indexInfo": null}
                    ],
                    "plantInfo": []
                },
                {
                    "date": {"year": 2026, "month": 6, "day": 19},
                    "pollenTypeInfo": [
                        {"code": "WEED", "inSeason": true,
                         "indexInfo": {"code": "UPI", "value": 2, "category": "Low"}}
                    ],
                    "plantInfo": []
                }
            ]
        });

        let (readings, raws) = parse_forecast(&body, 34.05, -118.24);
        assert_eq!(readings.len(), 2, "2 contract readings (day A + day C; day B skipped)");
        assert_eq!(raws.len(), 3, "3 raw objects (all days, including null-index day B)");
        assert_eq!(raw_date_key(&raws[0]), "2026-06-17");
        assert_eq!(raw_date_key(&raws[1]), "2026-06-18", "null-index day B raw preserved");
        assert_eq!(raw_date_key(&raws[2]), "2026-06-19");
    }
}
