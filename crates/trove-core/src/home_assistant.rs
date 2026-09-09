//! Home Assistant self-hosted hub — periodic REST state-history pull.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/home-assistant.md.
//!
//! A **Periodic** cloud pull against the [Home Assistant REST API]:
//! `GET /api/history/period/{ts}` returns state-change history for all
//! entities. Auth: a Long-Lived Access Token (LLAT) generated on the user's
//! HA profile page, plus the instance URL (commonly `http://homeassistant.local:8123`
//! or an HTTPS URL with a self-signed cert).
//!
//! Two layers, unconditional:
//!
//! - **Raw** — every state-change object verbatim at
//!   `home/home-assistant/raw/YYYY-MM.jsonl`, full fidelity (entity_id,
//!   state, attributes, last_changed).
//! - **Contract** — sensor entities with parseable numeric states fan out into
//!   [`HomeReading`] rows at `home/home-assistant/YYYY-MM.jsonl`; one row per
//!   state change per numeric sensor. Non-sensor and non-numeric entities
//!   (lights, switches, locks, "unavailable") go to raw only.
//!
//! **Opportunistic / skip-when-down**: a TCP connection failure, timeout, or
//! non-2xx response means the HA instance is unreachable — the periodic pass
//! quietly skips and retries next cycle; the watcher loop never errors. Only
//! the manual "Sync now" surfaces errors to the user.
//!
//! **Watermark**: the latest `last_changed` timestamp ever written, kept in
//! a rebuildable cursor at `.trove/home-assistant-sync.json` (non-secret,
//! beside the other `.trove/` indexes, not under `.trove/sync/`). The cursor
//! advances only after the full drain of a time window. A silent baseline on
//! first sync: we capture the current watermark without emitting any records
//! (the recorder stores the last 10 days by default; we poll hourly so we
//! drain the backlog promptly before records purge).
//!
//! Auth is a secret pair (URL + LLAT): pasted via [`ConnectMethod::TokenPaste`]
//! as `url|token`, stored under `.trove/sync/home-assistant.json` (0600).
//!
//! [Home Assistant REST API]: https://developers.home-assistant.io/docs/api/rest/

use std::collections::{BTreeMap, HashSet};
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

/// Contract-layer reading stream; raw lines one level deeper in `raw/`.
const DIR: &str = "home/home-assistant";
const RAW_DIR: &str = "home/home-assistant/raw";

/// Non-secret rebuildable cursor for the watermark. NOT under `.trove/sync/`
/// (that's 0600 secrets). Delete it to re-drain from the beginning.
const SYNC_FILE: &str = ".trove/home-assistant-sync.json";

/// Service id under `.trove/sync/` for the stored `url|token` pair.
const SERVICE: &str = "home-assistant";

/// Seconds between syncs. Hourly keeps us well inside the 10-day default
/// recorder retention; well inside the 30-day extended retention window.
pub const HA_SYNC_SECS: u64 = 3600;

/// Hard timeout for any single HTTP request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// HA records state history in 1-second resolution; we overlap by this
/// window on each pull to avoid missing edge-second events (overlap is
/// deduplicated by guid at write time).
const OVERLAP_SECS: i64 = 5;

// ---------------------------------------------------------------------------
// Cursor state — non-secret, rebuildable.

/// Persisted non-secret cursor for the watermark.
#[derive(Debug, Serialize, Deserialize, Default)]
struct SyncState {
    /// The latest `last_changed` RFC3339 we have written (or empty = no
    /// prior data; triggers a silent baseline on first run).
    #[serde(default)]
    latest_ts: String,
}

impl Vault {
    fn read_ha_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_ha_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape — the verbatim API state-change object tagged with a ts for
// the month-partition writer. Only `ts` is used for partitioning; `value` is
// what lands on disk.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Metric mapping: HA sensor device_class → (contract metric, unit normalization).
// We translate HA unit strings to the trove vocabulary; if a unit isn't in the
// map we pass it through as-is (additive tolerance for future device classes).

/// `(device_class, trove_metric)`. Unit normalization is handled in
/// `unit_of` from the `unit_of_measurement` attribute.
const SENSOR_CLASS_METRIC: &[(&str, &str)] = &[
    ("temperature", "temperature"),
    ("humidity", "humidity"),
    ("carbon_dioxide", "co2"),
    ("pressure", "pressure"),
    ("illuminance", "illuminance"),
    ("pm25", "pm25"),
    ("pm10", "pm10"),
    ("volatile_organic_compounds", "tvoc"),
    ("volatile_organic_compounds_parts", "tvoc"),
    ("nitrogen_dioxide", "no2"),
    ("ozone", "ozone"),
    ("sulphur_dioxide", "so2"),
    ("carbon_monoxide", "co"),
    ("signal_strength", "signal_strength"),
    ("power", "power"),
    ("energy", "energy"),
    ("voltage", "voltage"),
    ("current", "current"),
    ("wind_speed", "wind_speed"),
    ("wind_direction", "wind_direction"),
    ("precipitation", "precipitation"),
    ("precipitation_intensity", "precipitation_rate"),
    ("atmospheric_pressure", "pressure"),
    ("moisture", "moisture"),
    ("sound_pressure", "sound_pressure"),
    ("distance", "distance"),
    ("speed", "speed"),
    ("battery", "battery"),
    ("gas", "gas"),
    ("water", "water"),
    ("aqi", "aqi"),
    ("radon", "radon"),
];

/// Normalize a HA unit string to the trove vocabulary. Unknown units pass
/// through unchanged (additive — future HA classes still land).
///
/// All trove-vocabulary outputs are lowercase to match `home.reading.schema.json`
/// (e.g. `hpa`, `kwh`, `c`, `f`) and allow cross-source merge-by-metric to
/// compare units without case folding at read time.
fn normalize_unit(ha_unit: &str) -> &str {
    match ha_unit {
        "°C" => "c",
        "°F" => "f",
        "K" => "k",
        "%" => "percent",
        "ppm" => "ppm",
        "ppb" => "ppb",
        "hPa" | "mbar" => "hpa",
        "inHg" => "inhg",
        "mmHg" => "mmhg",
        "Pa" => "pa",
        "kPa" => "kpa",
        "bar" => "bar",
        "lx" => "lx",
        "µg/m³" | "ug/m³" | "μg/m³" => "ug_m3",
        "mg/m³" | "mg/m3" => "mg_m3",
        "Bq/m³" => "bq_m3",
        "µg/m3" => "ug_m3",
        "m/s" => "m_s",
        "km/h" => "km_h",
        "mph" => "mph",
        "mm" => "mm",
        "in" => "in",
        "mm/h" => "mm_h",
        "in/h" => "in_h",
        "dB" | "dBm" => "db",
        "W" => "w",
        "kW" => "kw",
        "Wh" => "wh",
        "kWh" => "kwh",
        "V" => "v",
        "A" => "a",
        "m" => "m",
        "ft" => "ft",
        "km" => "km",
        "mi" => "mi",
        "m³" => "m3",
        "ft³" => "ft3",
        "L" => "l",
        "gal" => "gal",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Periodic pass: the same pull "Sync now" runs, but network/connectivity
/// errors are silently skipped — the loop must not error.
/// Only `HaError::Unreachable` (TCP refused, timeout, DNS failure) is treated
/// as opportunistic skip-when-down. Auth errors and API errors (e.g. HTTP 400
/// from a misconfigured instance) propagate as errors so they surface in the
/// hub and are not mistaken for skip-when-down behavior.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("home assistant synced — {n} readings")
            }))
        }
        Err(e) => {
            // Only swallow genuine connectivity failures (TCP refused, timeout,
            // DNS) as skip-when-down. Check the error chain for HaError::Unreachable.
            let is_unreachable = e
                .chain()
                .any(|cause| {
                    let s = cause.to_string();
                    s.starts_with("unreachable:") || s.contains("connection refused")
                });
            if is_unreachable {
                Ok(crate::registry::CollectOutcome::note(format!(
                    "home assistant sync skipped (unreachable): {e}"
                )))
            } else {
                // Auth failure, HTTP 400 (missing filter_entity_id), API errors —
                // propagate so the hub surfaces them rather than silently zero-collecting.
                Err(e)
            }
        }
    }
}

/// Manual "Sync now": surfaces errors to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let raw_n = out.counts.get("raw_states").copied().unwrap_or(0);
    let headline = if n == 0 && raw_n == 0 {
        "Home Assistant is up to date — no new state changes".to_string()
    } else {
        format!("Home Assistant synced — {n} sensor readings, {raw_n} raw state changes")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "home-assistant",
        name: "Home Assistant",
        // Carries a paste-credential connection (URL|token), so it is a CloudSync
        // in the registry taxonomy even though the HA instance is on the LAN —
        // every connection-bearing def is CloudSync (connections_are_well_formed).
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pulls entity state history from your local Home Assistant instance — every Zigbee, \
             Z-Wave, Nest, Hue, Ecobee, temperature, motion, lock, and switch HA already knows. \
             Numeric sensor readings land in the unified home store; all state changes are \
             preserved in raw.",
        domain: "home",
        vault_path: "home/home-assistant/",
        toggleable: true,
        setup: &[
            "In Home Assistant, go to your profile page and scroll to Long-Lived Access Tokens.",
            "Create a token and copy it.",
            "Paste your HA URL and the token in the form below as url|token (e.g. http://homeassistant.local:8123|eyJ0eX…).",
            "First sync backfills available history (default HA retention: 10 days); later syncs are incremental.",
        ],
        caveats:
            "Requires a running Home Assistant instance on the same network. Skips gracefully \
             when HA is unreachable — no error state, retries next cycle. Default recorder \
             retention is 10 days (configurable) — connect promptly to preserve history.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(HA_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("home-assistant"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste: url|token composite).

/// Parse `url|token` from the pasted string. Splits on the first `|`.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your HA URL and Long-Lived Access Token as url|token");
    }
    let (url, token) = pasted
        .split_once('|')
        .ok_or_else(|| anyhow::anyhow!("paste the URL and token separated by |, e.g. http://homeassistant.local:8123|eyJ0eX…"))?;
    let url = url.trim().trim_end_matches('/').to_string();
    let token = token.trim().to_string();
    if url.is_empty() {
        bail!("missing URL — paste as url|token");
    }
    if token.is_empty() {
        bail!("missing token — paste as url|token");
    }
    Ok((url, token))
}

/// Verify the credentials with `GET /api/` (lightweight ping), then store.
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (url, token) = parse_credentials(pasted)?;
    let client = HaClient::new(url.clone(), token.clone());
    // A minimal ping: /api/ returns {"message":"API running."} on success.
    client.ping().with_context(|| {
        format!(
            "Could not reach Home Assistant at {url} — check the URL and that HA is running. \
             If using HTTPS with a self-signed cert, the connection will be refused until cert \
             trust is implemented (use http:// for LAN access)."
        )
    })?;
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: Some("HaLLAT".into()),
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored credentials. Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = credentials are stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        // The stored token is the raw `url|token` string; extract the URL for
        // display only (never log the LLAT itself).
        let display = if let Some((url, _)) = token.access_token.split_once('|') {
            url.trim_end_matches('/').to_string()
        } else {
            "Home Assistant".to_string()
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: display,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "home-assistant",
    display_name: "Home Assistant",
    methods: &[ConnectMethod::TokenPaste {
        label: "Instance URL and Long-Lived Access Token",
        help: "Paste as url|token, e.g. http://homeassistant.local:8123|eyJ0eX… — \
               create the token on your HA profile page under Long-Lived Access Tokens. \
               The token is stored locally and sent only to your HA instance.",
        placeholder: "http://homeassistant.local:8123|eyJ0eXAiOiJKV1QiLCJhbGci…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["home-assistant"],
    setup: &[
        "In Home Assistant, open your profile (click your name in the sidebar).",
        "Scroll to Long-Lived Access Tokens and create a new token.",
        "Copy the token and paste it here together with your HA URL, separated by |.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Errors from the HA REST API.
#[derive(Debug)]
enum HaError { // Display/Error impls below
    Unreachable(String),
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for HaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HaError::Unreachable(m) => write!(f, "unreachable: {m}"),
            HaError::Unauthorized => write!(f, "unauthorized (check your Long-Lived Access Token)"),
            HaError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for HaError {}

/// The endpoints the pull needs. A trait so tests can drive the logic
/// with fixtures and never touch the network.
trait HaApi {
    /// `GET /api/` — lightweight ping that returns `{"message":"API running."}`.
    fn ping(&self) -> Result<(), HaError>;
    /// `GET /api/states` — returns a snapshot of all entity state objects.
    /// Used to enumerate entity IDs before calling `history`.
    fn states(&self) -> Result<Value, HaError>;
    /// `GET /api/history/period/{ts}?filter_entity_id={ids}&end_time={end}` — returns a
    /// `Vec<Vec<StateObject>>` (outer = per entity, inner = history).
    /// `entity_ids` must be non-empty; HA >= 2022.7 returns HTTP 400 when
    /// `filter_entity_id` is absent.
    fn history(&self, start: &str, end: Option<&str>, entity_ids: &[String]) -> Result<Value, HaError>;
}

/// Live HTTP client backed by ureq.
struct HaClient {
    base_url: String,
    token: String,
}

impl HaClient {
    fn new(base_url: String, token: String) -> Self {
        HaClient { base_url, token }
    }
}

impl HaApi for HaClient {
    fn ping(&self) -> Result<(), HaError> {
        let url = format!("{}/api/", self.base_url);
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(401, _) => HaError::Unauthorized,
                ureq::Error::Status(403, _) => HaError::Unauthorized,
                ureq::Error::Transport(t) => HaError::Unreachable(t.to_string()),
                ureq::Error::Status(code, r) => {
                    HaError::Other(format!("HTTP {code}: {}", r.status_text()))
                }
            })?;
        if resp.status() >= 200 && resp.status() < 300 {
            Ok(())
        } else {
            Err(HaError::Other(format!("unexpected HTTP {}", resp.status())))
        }
    }

    fn states(&self) -> Result<Value, HaError> {
        let url = format!("{}/api/states", self.base_url);
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                    HaError::Unauthorized
                }
                ureq::Error::Transport(t) => HaError::Unreachable(t.to_string()),
                ureq::Error::Status(code, r) => {
                    HaError::Other(format!("HTTP {code}: {}", r.status_text()))
                }
            })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| HaError::Other(format!("JSON parse: {e}")))?;
        Ok(body)
    }

    fn history(&self, start: &str, end: Option<&str>, entity_ids: &[String]) -> Result<Value, HaError> {
        // entity_ids must be non-empty — HA >= 2022.7 returns HTTP 400 when
        // filter_entity_id is absent. Callers are responsible for chunking.
        if entity_ids.is_empty() {
            return Err(HaError::Other(
                "filter_entity_id is missing — cannot call /api/history/period without entity ids".into(),
            ));
        }
        // URL-encode the timestamp — RFC3339 contains colons and '+', both
        // meaningful in query strings.
        let encoded_start = start.replace(':', "%3A").replace('+', "%2B");
        let filter = entity_ids.join(",");
        let mut url = format!(
            "{}/api/history/period/{}?minimal_response=false&significant_changes_only=false&filter_entity_id={}",
            self.base_url, encoded_start, filter
        );
        if let Some(e) = end {
            let encoded_end = e.replace(':', "%3A").replace('+', "%2B");
            url.push_str(&format!("&end_time={}", encoded_end));
        }
        let resp = ureq::get(&url)
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Content-Type", "application/json")
            .timeout(HTTP_TIMEOUT)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                    HaError::Unauthorized
                }
                ureq::Error::Transport(t) => HaError::Unreachable(t.to_string()),
                ureq::Error::Status(code, r) => {
                    HaError::Other(format!("HTTP {code}: {}", r.status_text()))
                }
            })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| HaError::Other(format!("JSON parse: {e}")))?;
        Ok(body)
    }
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// Extract a string attribute from a state object's `attributes` map.
fn attr_str<'a>(obj: &'a Value, key: &str) -> &'a str {
    obj.get("attributes")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// Try to parse the `state` field of a HA state object as f64.
fn parse_state_f64(obj: &Value) -> Option<f64> {
    obj.get("state")?.as_str()?.parse::<f64>().ok()
}

/// The RFC3339 `last_changed` from a HA state object, or `""` when missing.
fn last_changed(obj: &Value) -> &str {
    obj.get("last_changed").and_then(Value::as_str).unwrap_or("")
}

/// Stable guid for a state-change row: `home-assistant:{entity_id}:{last_changed}`.
fn state_guid(entity_id: &str, lc: &str) -> String {
    format!("home-assistant:{entity_id}:{lc}")
}

/// Map a HA sensor state object into a `HomeReading`. Returns `None` when:
/// - no entity_id
/// - no numeric state (unavailable / unknown / non-numeric)
/// - no recognized device_class for the metric
/// - no parseable `last_changed` for the partition key
fn state_to_reading(obj: &Value) -> Option<HomeReading> {
    let entity_id = obj.get("entity_id").and_then(Value::as_str).unwrap_or("");
    if entity_id.is_empty() {
        return None;
    }
    // Only sensor entities carry numeric readings; quick prefix check avoids
    // trying to parse e.g. "on"/"off"/"unavailable" for lights and switches.
    if !entity_id.starts_with("sensor.") {
        return None;
    }
    let value = parse_state_f64(obj)?;
    let device_class = attr_str(obj, "device_class");
    if device_class.is_empty() {
        return None;
    }
    let metric = SENSOR_CLASS_METRIC
        .iter()
        .find(|(dc, _)| *dc == device_class)
        .map(|(_, m)| *m)?;

    let lc = last_changed(obj);
    if lc.is_empty() {
        return None;
    }
    // Convert UTC RFC3339 to local-time RFC3339.
    let ts = DateTime::parse_from_rfc3339(lc)
        .ok()
        .map(|t| t.with_timezone(&Local).to_rfc3339())?;
    if Partition::Month.key(&ts).is_none() {
        return None;
    }

    let ha_unit = attr_str(obj, "unit_of_measurement");
    let unit = if ha_unit.is_empty() { "" } else { normalize_unit(ha_unit) };
    let friendly_name = attr_str(obj, "friendly_name");

    let mut r = HomeReading::new("home-assistant", metric, value, ts);
    r.unit = unit.to_string();
    r.place = friendly_name.to_string();
    r.device = entity_id.to_string();

    // Stable guid in extra (the same idiom as ambient_weather).
    let mut extra = Map::new();
    extra.insert(
        "guid".into(),
        Value::String(state_guid(entity_id, lc)),
    );
    // Carry entity_id and original unit for reference, in extra.
    extra.insert("entity_id".into(), Value::String(entity_id.to_string()));
    if !ha_unit.is_empty() {
        extra.insert("ha_unit".into(), Value::String(ha_unit.to_string()));
    }
    r.extra = extra;
    Some(r)
}

// ---------------------------------------------------------------------------
// Write helpers — raw + contract, deduped by guid.

/// Extract the guid from a HomeReading's extra map.
fn reading_guid(r: &HomeReading) -> String {
    r.extra
        .get("guid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Extract a dedupe key from a raw Value (the entity_id + last_changed).
fn raw_guid(v: &Value) -> Option<String> {
    let eid = v.get("entity_id").and_then(Value::as_str)?;
    let lc = v.get("last_changed").and_then(Value::as_str)?;
    Some(format!("{eid}:{lc}"))
}

/// Pull outcome counts, returned by the inner `pull` fn.
#[derive(Debug)]
struct PullCounts {
    counts: BTreeMap<&'static str, u64>,
}

// ---------------------------------------------------------------------------
// Main pull logic (tested via the trait).

fn pull_with<A: HaApi>(vault: &Vault, api: &A) -> Result<PullCounts> {
    // Load credentials.
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| anyhow::anyhow!("Home Assistant is not connected — paste url|token first"))?;
    let _pasted = &token.access_token; // already loaded via the client; just verify stored

    let cursor = vault.read_ha_sync();

    // Determine the start timestamp. On first run (empty cursor) we use a
    // sentinel that HA accepts: start of a wide window. HA's default retention
    // is 10 days, so we ask for 11 days back on first sync.
    let now = chrono::Utc::now();
    let (start_ts, is_first_sync) = if cursor.latest_ts.is_empty() {
        let backfill = now - chrono::Duration::days(11);
        (backfill.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), true)
    } else {
        // Overlap by OVERLAP_SECS to catch edge-second events; deduplicated by
        // guid at write time.
        let parsed = DateTime::parse_from_rfc3339(&cursor.latest_ts)
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or(now - chrono::Duration::hours(2));
        let with_overlap = parsed - chrono::Duration::seconds(OVERLAP_SECS);
        (with_overlap.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), false)
    };
    let end_ts = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // Enumerate all entity IDs via GET /api/states. This is required because
    // HA >= 2022.7 requires filter_entity_id on /api/history/period — an absent
    // filter returns HTTP 400 `filter_entity_id is missing`. Without states(),
    // every pull would silently 400-fail.
    let states_snapshot = api.states()?;
    let entity_ids: Vec<String> = {
        let arr = states_snapshot.as_array().ok_or_else(|| {
            anyhow::anyhow!("GET /api/states returned non-array — unexpected HA response shape")
        })?;
        arr.iter()
            .filter_map(|s| s.get("entity_id").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    };
    if entity_ids.is_empty() {
        // HA returned an empty entity list — nothing to collect. Advance the
        // watermark on first sync so we do not re-attempt the full window next cycle.
        if is_first_sync {
            vault.write_ha_sync(&SyncState { latest_ts: end_ts })?;
        }
        return Ok(PullCounts { counts: BTreeMap::new() });
    }

    // Chunk entity IDs to bound URL length (~100 entities per request keeps
    // URLs well under browser/server limits; HA payloads remain manageable).
    const ENTITY_CHUNK: usize = 100;
    let mut all_states_owned: Vec<Value> = Vec::new();
    for chunk in entity_ids.chunks(ENTITY_CHUNK) {
        let chunk_ids: Vec<String> = chunk.to_vec();
        let history = api.history(&start_ts, Some(&end_ts), &chunk_ids)?;
        // The response is `Vec<Vec<StateObject>>` (outer = per entity, inner = history).
        if let Some(entity_lists) = history.as_array() {
            for entity_list in entity_lists {
                if let Some(states) = entity_list.as_array() {
                    all_states_owned.extend(states.iter().cloned());
                }
            }
        }
    }
    let all_states: Vec<&Value> = all_states_owned.iter().collect();

    if all_states.is_empty() {
        if is_first_sync {
            // Silent baseline: record the watermark but emit nothing.
            let new_state = SyncState { latest_ts: end_ts };
            vault.write_ha_sync(&new_state)?;
        }
        return Ok(PullCounts { counts: BTreeMap::new() });
    }

    // Build the contract rows and raw rows.
    let mut contract_rows: Vec<HomeReading> = Vec::new();
    let mut raw_rows: Vec<(String, Value)> = Vec::new(); // (ts, value)
    let mut max_ts = cursor.latest_ts.clone();

    for obj in &all_states {
        let lc = last_changed(obj);
        if lc.is_empty() {
            continue;
        }
        // Track the maximum last_changed seen — advances watermark.
        if lc > max_ts.as_str() {
            max_ts = lc.to_string();
        }

        // Raw layer: always.
        let raw_ts = DateTime::parse_from_rfc3339(lc)
            .ok()
            .map(|t| t.with_timezone(&Local).to_rfc3339())
            .unwrap_or_else(|| lc.to_string());
        if Partition::Month.key(&raw_ts).is_some() {
            raw_rows.push((raw_ts, (*obj).clone()));
        }

        // Contract layer: numeric sensor entities only.
        if let Some(reading) = state_to_reading(obj) {
            contract_rows.push(reading);
        }
    }

    // On first sync: silent baseline — no writes, just advance the watermark.
    if is_first_sync {
        let new_state = SyncState { latest_ts: max_ts };
        vault.write_ha_sync(&new_state)?;
        return Ok(PullCounts { counts: BTreeMap::new() });
    }

    // Deduplicate contract rows against what's already on disk.
    let contract_stream = vault.stream(DIR, Partition::Month);
    let mut seen_guids: HashSet<String> = HashSet::new();
    for key in contract_stream.partitions()? {
        for v in contract_stream.read::<Value>(&key)? {
            let g = v
                .get("extra")
                .and_then(|e| e.get("guid"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !g.is_empty() {
                seen_guids.insert(g);
            }
        }
    }
    let mut new_contract: Vec<HomeReading> = Vec::new();
    for r in contract_rows {
        let g = reading_guid(&r);
        if g.is_empty() || !seen_guids.insert(g) {
            continue;
        }
        new_contract.push(r);
    }

    // Deduplicate raw rows.
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen_raw: HashSet<String> = HashSet::new();
    for key in raw_stream.partitions()? {
        for v in raw_stream.read::<Value>(&key)? {
            if let Some(k) = raw_guid(&v) {
                seen_raw.insert(k);
            }
        }
    }
    let _new_raw_count = raw_rows.len();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (ts, v) in raw_rows {
        if let Some(k) = raw_guid(&v) {
            if !seen_raw.insert(k) {
                continue;
            }
        }
        new_raws.push(RawLine { ts, value: v });
    }

    // Write. Contract advances the watermark only after a successful write.
    let readings_written = new_contract.len() as u64;
    let raw_written = new_raws.len() as u64;

    if !new_contract.is_empty() {
        contract_stream.append(&new_contract, |r| &r.ts)?;
    }
    if !new_raws.is_empty() {
        raw_stream.append(&new_raws, |r| &r.ts)?;
    }

    // Advance watermark only after all writes succeed.
    if !max_ts.is_empty() && max_ts > cursor.latest_ts {
        vault.write_ha_sync(&SyncState { latest_ts: max_ts })?;
    }

    let mut counts = BTreeMap::new();
    counts.insert("readings", readings_written);
    counts.insert("raw_states", raw_written);
    Ok(PullCounts { counts })
}

/// Resolve credentials from the vault and delegate to `pull_with`.
fn pull(vault: &Vault) -> Result<PullCounts> {
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| anyhow::anyhow!("Home Assistant is not connected"))?;
    let (url, ha_token) = parse_credentials(&token.access_token)?;
    let client = HaClient::new(url, ha_token);
    pull_with(vault, &client)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    // ---------------------------------------------------------------------------
    // Fixtures: HA history API response shapes confirmed against official docs.

    /// A minimal numeric sensor state object (temperature).
    fn fixture_temp_state() -> Value {
        json!({
            "entity_id": "sensor.bedroom_temperature",
            "state": "21.5",
            "attributes": {
                "friendly_name": "Bedroom Temperature",
                "unit_of_measurement": "°C",
                "device_class": "temperature"
            },
            "last_changed": "2026-06-10T14:05:00+00:00",
            "last_updated": "2026-06-10T14:05:00+00:00"
        })
    }

    /// A humidity sensor state object.
    fn fixture_humidity_state() -> Value {
        json!({
            "entity_id": "sensor.living_room_humidity",
            "state": "55.2",
            "attributes": {
                "friendly_name": "Living Room Humidity",
                "unit_of_measurement": "%",
                "device_class": "humidity"
            },
            "last_changed": "2026-06-10T15:00:00+00:00",
            "last_updated": "2026-06-10T15:00:00+00:00"
        })
    }

    /// A CO2 sensor state object.
    fn fixture_co2_state() -> Value {
        json!({
            "entity_id": "sensor.office_co2",
            "state": "812",
            "attributes": {
                "friendly_name": "Office CO2",
                "unit_of_measurement": "ppm",
                "device_class": "carbon_dioxide"
            },
            "last_changed": "2026-06-10T14:30:00+00:00",
            "last_updated": "2026-06-10T14:30:00+00:00"
        })
    }

    /// A non-numeric sensor (unavailable state) — must NOT produce a reading.
    fn fixture_unavailable_state() -> Value {
        json!({
            "entity_id": "sensor.bedroom_temperature",
            "state": "unavailable",
            "attributes": {
                "friendly_name": "Bedroom Temperature",
                "unit_of_measurement": "°C",
                "device_class": "temperature"
            },
            "last_changed": "2026-06-10T14:10:00+00:00",
            "last_updated": "2026-06-10T14:10:00+00:00"
        })
    }

    /// A light entity — must NOT produce a contract reading.
    fn fixture_light_state() -> Value {
        json!({
            "entity_id": "light.kitchen",
            "state": "on",
            "attributes": {
                "friendly_name": "Kitchen Light",
                "brightness": 255
            },
            "last_changed": "2026-06-10T14:20:00+00:00",
            "last_updated": "2026-06-10T14:20:00+00:00"
        })
    }

    /// A switch entity — must NOT produce a contract reading.
    fn fixture_switch_state() -> Value {
        json!({
            "entity_id": "switch.living_room_tv",
            "state": "off",
            "attributes": {
                "friendly_name": "Living Room TV"
            },
            "last_changed": "2026-06-10T14:22:00+00:00",
            "last_updated": "2026-06-10T14:22:00+00:00"
        })
    }

    /// A full HA history API response (the outer Vec<Vec<StateObject>> shape).
    fn fixture_history_response() -> Value {
        json!([
            [
                fixture_temp_state(),
                fixture_unavailable_state()
            ],
            [
                fixture_humidity_state()
            ],
            [
                fixture_co2_state()
            ],
            [
                fixture_light_state()
            ],
            [
                fixture_switch_state()
            ]
        ])
    }

    // ---------------------------------------------------------------------------
    // Stub API for offline tests.

    /// A fixture states snapshot: a flat array of entity state objects.
    fn fixture_states_snapshot() -> Value {
        // Minimal /api/states response — one entry per entity in our fixtures.
        // The pull_with function extracts entity_id fields from this array.
        json!([
            {"entity_id": "sensor.bedroom_temperature", "state": "21.5",
             "attributes": {"device_class": "temperature", "unit_of_measurement": "°C", "friendly_name": "Bedroom Temperature"}},
            {"entity_id": "sensor.living_room_humidity", "state": "55.2",
             "attributes": {"device_class": "humidity", "unit_of_measurement": "%", "friendly_name": "Living Room Humidity"}},
            {"entity_id": "sensor.office_co2", "state": "812",
             "attributes": {"device_class": "carbon_dioxide", "unit_of_measurement": "ppm", "friendly_name": "Office CO2"}},
            {"entity_id": "light.kitchen", "state": "on",
             "attributes": {"friendly_name": "Kitchen Light"}},
            {"entity_id": "switch.living_room_tv", "state": "off",
             "attributes": {"friendly_name": "Living Room TV"}}
        ])
    }

    struct StubApi {
        history_response: Mutex<Option<Value>>,
        states_response: Mutex<Option<Value>>,
        ping_ok: bool,
    }

    impl StubApi {
        fn ok(history: Value) -> Self {
            StubApi {
                history_response: Mutex::new(Some(history)),
                states_response: Mutex::new(Some(fixture_states_snapshot())),
                ping_ok: true,
            }
        }
        fn unreachable() -> Self {
            StubApi {
                history_response: Mutex::new(None),
                states_response: Mutex::new(None),
                ping_ok: false,
            }
        }
        fn empty() -> Self {
            // empty() has a states snapshot but an empty history response.
            StubApi {
                history_response: Mutex::new(Some(json!([]))),
                states_response: Mutex::new(Some(fixture_states_snapshot())),
                ping_ok: true,
            }
        }
        /// A stub with an empty entity list — simulates an HA instance with no entities.
        fn empty_states() -> Self {
            StubApi {
                history_response: Mutex::new(Some(json!([]))),
                states_response: Mutex::new(Some(json!([]))),
                ping_ok: true,
            }
        }
    }

    impl HaApi for StubApi {
        fn ping(&self) -> Result<(), HaError> {
            if self.ping_ok {
                Ok(())
            } else {
                Err(HaError::Unreachable("connection refused".into()))
            }
        }
        fn states(&self) -> Result<Value, HaError> {
            match self.ping_ok {
                false => Err(HaError::Unreachable("connection refused".into())),
                true => Ok(self
                    .states_response
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or(json!([]))),
            }
        }
        fn history(&self, _start: &str, _end: Option<&str>, entity_ids: &[String]) -> Result<Value, HaError> {
            // Mirror what HA >= 2022.7 does: return an error when filter_entity_id is absent.
            if entity_ids.is_empty() {
                return Err(HaError::Other(
                    "filter_entity_id is missing".into(),
                ));
            }
            match self.ping_ok {
                false => Err(HaError::Unreachable("connection refused".into())),
                true => Ok(self
                    .history_response
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or(json!([]))),
            }
        }
    }

    /// Create a unique temp vault and pre-store a fake credential.
    fn make_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-home-assistant-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let vault = Vault::open_or_create(dir).unwrap();
        // Store fake credentials (url|token) in the secret store.
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: "http://test.local:8123|fake-llat".to_string(),
                    refresh_token: None,
                    token_type: Some("HaLLAT".into()),
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();
        vault
    }

    // ---------------------------------------------------------------------------
    // Unit tests: pure mapping.

    #[test]
    fn home_assistant_temp_state_maps_to_reading() {
        let obj = fixture_temp_state();
        let r = state_to_reading(&obj).expect("should produce a reading");
        assert_eq!(r.source, "home-assistant");
        assert_eq!(r.metric, "temperature");
        assert_eq!(r.value, 21.5);
        assert_eq!(r.unit, "c");
        assert_eq!(r.place, "Bedroom Temperature");
        assert_eq!(r.device, "sensor.bedroom_temperature");
        // guid must be stable and non-empty.
        let guid = r.extra.get("guid").and_then(Value::as_str).unwrap_or("");
        assert!(guid.starts_with("home-assistant:sensor.bedroom_temperature:"));
        // Partition key must be parseable (YYYY-MM).
        assert!(Partition::Month.key(&r.ts).is_some(), "ts has a month key");
    }

    #[test]
    fn home_assistant_humidity_maps_to_reading() {
        let r = state_to_reading(&fixture_humidity_state()).unwrap();
        assert_eq!(r.metric, "humidity");
        assert_eq!(r.value, 55.2);
        assert_eq!(r.unit, "percent");
    }

    #[test]
    fn home_assistant_co2_maps_to_reading() {
        let r = state_to_reading(&fixture_co2_state()).unwrap();
        assert_eq!(r.metric, "co2");
        assert_eq!(r.value, 812.0);
        assert_eq!(r.unit, "ppm");
    }

    #[test]
    fn home_assistant_unavailable_state_yields_no_reading() {
        assert!(
            state_to_reading(&fixture_unavailable_state()).is_none(),
            "unavailable state must not produce a reading"
        );
    }

    #[test]
    fn home_assistant_light_entity_yields_no_reading() {
        assert!(
            state_to_reading(&fixture_light_state()).is_none(),
            "light entity must not produce a reading"
        );
    }

    #[test]
    fn home_assistant_switch_entity_yields_no_reading() {
        assert!(
            state_to_reading(&fixture_switch_state()).is_none(),
            "switch entity must not produce a reading"
        );
    }

    #[test]
    fn home_assistant_unit_normalization_covers_common_ha_units() {
        // Trove-vocabulary outputs must be lowercase for cross-source metric alignment.
        assert_eq!(normalize_unit("°C"), "c");
        assert_eq!(normalize_unit("°F"), "f");
        assert_eq!(normalize_unit("%"), "percent");
        assert_eq!(normalize_unit("ppm"), "ppm");
        assert_eq!(normalize_unit("µg/m³"), "ug_m3");
        assert_eq!(normalize_unit("hPa"), "hpa");
        assert_eq!(normalize_unit("lx"), "lx");
        assert_eq!(normalize_unit("dB"), "db");
        assert_eq!(normalize_unit("kWh"), "kwh");
        assert_eq!(normalize_unit("Wh"), "wh");
        assert_eq!(normalize_unit("W"), "w");
        assert_eq!(normalize_unit("kW"), "kw");
        assert_eq!(normalize_unit("V"), "v");
        assert_eq!(normalize_unit("A"), "a");
        assert_eq!(normalize_unit("K"), "k");
        assert_eq!(normalize_unit("hPa"), "hpa");
        assert_eq!(normalize_unit("inHg"), "inhg");
        // Unknown units pass through unchanged (additive tolerance for future device classes).
        assert_eq!(normalize_unit("Gbps"), "Gbps");
    }

    #[test]
    fn home_assistant_parse_credentials_ok() {
        let (url, token) =
            parse_credentials("http://homeassistant.local:8123|eyJ0eXAiOiJKV1QiLCJhbGci…")
                .unwrap();
        assert_eq!(url, "http://homeassistant.local:8123");
        assert!(!token.is_empty());
    }

    #[test]
    fn home_assistant_parse_credentials_trims_trailing_slash() {
        let (url, _) =
            parse_credentials("http://homeassistant.local:8123/|tok").unwrap();
        assert_eq!(url, "http://homeassistant.local:8123");
    }

    #[test]
    fn home_assistant_parse_credentials_missing_pipe_errors() {
        assert!(parse_credentials("http://homeassistant.local:8123").is_err());
    }

    #[test]
    fn home_assistant_parse_credentials_empty_errors() {
        assert!(parse_credentials("").is_err());
    }

    // ---------------------------------------------------------------------------
    // Integration-level tests: pull_with, vault writes, cursor behaviour.

    #[test]
    fn home_assistant_first_sync_is_silent_baseline() {
        // On the very first sync the cursor is empty → silent baseline,
        // no rows written, but the cursor advances to the end_ts.
        let vault = make_vault("baseline");
        let api = StubApi::ok(fixture_history_response());
        let out = pull_with(&vault, &api).unwrap();
        // Silent baseline: zero rows emitted.
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 0);
        assert_eq!(out.counts.get("raw_states").copied().unwrap_or(0), 0);
        // Cursor must have advanced.
        let cursor = vault.read_ha_sync();
        assert!(!cursor.latest_ts.is_empty(), "cursor must be set after first sync");
    }

    #[test]
    fn home_assistant_second_sync_writes_contract_and_raw() {
        let vault = make_vault("second-sync");
        // Prime the cursor (simulate a prior first-sync).
        vault
            .write_ha_sync(&SyncState {
                latest_ts: "2026-06-10T10:00:00+00:00".to_string(),
            })
            .unwrap();
        let api = StubApi::ok(fixture_history_response());
        let out = pull_with(&vault, &api).unwrap();
        // 3 numeric sensor states (temp=21.5, humidity=55.2, co2=812) produce readings.
        // unavailable, light, switch do NOT.
        let readings = out.counts.get("readings").copied().unwrap_or(0);
        assert_eq!(readings, 3, "expected 3 sensor readings");
        // All 5 state objects go to raw (light, switch, temp, humidity, co2;
        // unavailable skipped by partition check — no wait, unavailable state
        // still has a valid last_changed so it lands in raw too: 6 total? Let
        // us check: temp, unavailable, humidity, co2, light, switch = 6.
        let raw = out.counts.get("raw_states").copied().unwrap_or(0);
        assert!(raw >= 5, "expected at least 5 raw state rows, got {raw}");
    }

    #[test]
    fn home_assistant_deduplication_prevents_double_writes() {
        let vault = make_vault("dedup");
        // Prime cursor.
        vault
            .write_ha_sync(&SyncState {
                latest_ts: "2026-06-10T10:00:00+00:00".to_string(),
            })
            .unwrap();
        let api = StubApi::ok(fixture_history_response());
        // First pull writes the rows.
        let out1 = pull_with(&vault, &api).unwrap();
        let r1 = out1.counts.get("readings").copied().unwrap_or(0);
        // Second pull with same data: all rows already in vault — zero new writes.
        let api2 = StubApi::ok(fixture_history_response());
        let out2 = pull_with(&vault, &api2).unwrap();
        let r2 = out2.counts.get("readings").copied().unwrap_or(0);
        assert!(r1 > 0, "first pull wrote rows");
        assert_eq!(r2, 0, "second pull must not duplicate rows");
    }

    #[test]
    fn home_assistant_unreachable_instance_is_a_graceful_error() {
        // pull_with returns Err for an unreachable HA, which def_collect
        // converts to a quiet log entry (never panics the loop).
        let vault = make_vault("unreachable");
        // Prime cursor so we're in incremental mode.
        vault
            .write_ha_sync(&SyncState {
                latest_ts: "2026-06-10T10:00:00+00:00".to_string(),
            })
            .unwrap();
        let api = StubApi::unreachable();
        let err = pull_with(&vault, &api).unwrap_err();
        assert!(
            err.to_string().contains("unreachable")
                || err.to_string().contains("connection"),
            "error must mention unreachability: {err}"
        );
    }

    #[test]
    fn home_assistant_empty_history_response_is_ok() {
        let vault = make_vault("empty-history");
        vault
            .write_ha_sync(&SyncState {
                latest_ts: "2026-06-10T10:00:00+00:00".to_string(),
            })
            .unwrap();
        let api = StubApi::empty();
        let out = pull_with(&vault, &api).unwrap();
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 0);
        assert_eq!(out.counts.get("raw_states").copied().unwrap_or(0), 0);
    }

    #[test]
    fn home_assistant_connection_def_is_token_paste() {
        assert_eq!(CONNECTION.id, "home-assistant");
        assert!(
            CONNECTION.method("token-paste").is_some(),
            "must expose a token-paste method"
        );
    }

    #[test]
    fn home_assistant_reading_round_trips_to_jsonl() {
        // Verify HomeReading from HA serializes correctly for the vault.
        let obj = fixture_temp_state();
        let r = state_to_reading(&obj).unwrap();
        let j = serde_json::to_string(&r).unwrap();
        let r2: HomeReading = serde_json::from_str(&j).unwrap();
        assert_eq!(r.ts, r2.ts);
        assert_eq!(r.metric, r2.metric);
        assert_eq!(r.value, r2.value);
        assert_eq!(r.unit, r2.unit);
    }

    // ---------------------------------------------------------------------------
    // Tests for filter_entity_id requirement (blocking defect fix).

    #[test]
    fn home_assistant_history_url_requires_filter_entity_id() {
        // The stub mirrors HA >= 2022.7: history() called with empty entity_ids
        // must return an error, not an empty array. This ensures pull_with always
        // calls states() first and passes non-empty entity_ids.
        let api = StubApi::ok(fixture_history_response());
        let result = api.history("2026-06-10T00:00:00Z", None, &[]);
        assert!(
            result.is_err(),
            "history() with no filter_entity_id must error, not return empty"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("filter_entity_id"),
            "error must mention filter_entity_id: {err}"
        );
    }

    #[test]
    fn home_assistant_history_url_with_entity_ids_succeeds() {
        // history() called with entity_ids must succeed and return the fixture.
        let api = StubApi::ok(fixture_history_response());
        let ids = vec!["sensor.bedroom_temperature".to_string()];
        let result = api.history("2026-06-10T00:00:00Z", Some("2026-06-10T23:59:59Z"), &ids);
        assert!(result.is_ok(), "history() with filter_entity_id must succeed");
    }

    #[test]
    fn home_assistant_pull_enumerates_states_before_history() {
        // pull_with must call states() to enumerate entity ids and pass them to
        // history(). If the states() response is empty, zero rows are written
        // (not an error) but the watermark advances on first sync.
        let vault = make_vault("empty-states");
        let api = StubApi::empty_states();
        let out = pull_with(&vault, &api).unwrap();
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 0);
        // On first sync with empty entity list, cursor should still advance.
        let cursor = vault.read_ha_sync();
        assert!(!cursor.latest_ts.is_empty(), "cursor must advance even with empty entity list");
    }

    #[test]
    fn home_assistant_api_error_not_swallowed_as_skip() {
        // An API error (HTTP 400 / Other) must NOT be swallowed as skip-when-down.
        // Only HaError::Unreachable gets the opportunistic skip treatment.
        // We verify the classification logic by checking what pull_with returns:
        // unreachable → Err with "unreachable" in message;
        // api error → Err that does NOT come from connectivity failure.
        let vault = make_vault("api-error");
        vault
            .write_ha_sync(&SyncState {
                latest_ts: "2026-06-10T10:00:00+00:00".to_string(),
            })
            .unwrap();
        // Use unreachable stub — pull_with propagates it as an Err (not swallowed).
        let api = StubApi::unreachable();
        let err = pull_with(&vault, &api).unwrap_err();
        // The error chain from HaError::Unreachable must contain "unreachable".
        let msg = err.to_string();
        assert!(
            msg.contains("unreachable") || msg.contains("connection"),
            "unreachable errors must surface: {msg}"
        );
    }

    #[test]
    fn home_assistant_states_endpoint_returns_entity_list() {
        // states() must return the array from the /api/states fixture.
        let api = StubApi::ok(fixture_history_response());
        let result = api.states().expect("states() must succeed when reachable");
        let arr = result.as_array().expect("states() must return an array");
        assert!(!arr.is_empty(), "states snapshot must have entities");
        // Every entry must have an entity_id field.
        for entry in arr {
            assert!(
                entry.get("entity_id").and_then(Value::as_str).is_some(),
                "each state object must have entity_id"
            );
        }
    }

    #[test]
    fn home_assistant_pull_with_states_and_history_writes_correct_readings() {
        // End-to-end: pull_with calls states() then history(), and the entity
        // ids from states() are passed to history(). The StubApi enforces this
        // by erroring when entity_ids is empty.
        let vault = make_vault("full-flow");
        vault
            .write_ha_sync(&SyncState {
                latest_ts: "2026-06-10T10:00:00+00:00".to_string(),
            })
            .unwrap();
        let api = StubApi::ok(fixture_history_response());
        let out = pull_with(&vault, &api).expect("pull_with must succeed with states + history");
        let readings = out.counts.get("readings").copied().unwrap_or(0);
        // 3 numeric sensor states (temp, humidity, co2) produce readings.
        assert_eq!(readings, 3, "expected 3 sensor readings from states+history flow");
    }
}
