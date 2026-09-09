//! National Weather Service api.weather.gov — US weather alerts and the
//! official hourly forecast for the user's location.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/nws.md.
//!
//! A **Periodic**, keyless, no-login collector (the [`crate::weather`] shape —
//! it reuses the very same location ladder). It writes the **`environment`**
//! domain's two bound shapes:
//!
//! - **active alerts → [`EnvGeoEvent`]** (`environment/nws/events/YYYY-MM.jsonl`,
//!   month of `ts`) — the time-sensitive primary: only ~7 days of alert history
//!   exists, so live polling *is* what captures the record. One row per active
//!   watch/warning/advisory at the user's point, deduped/upserted by the alert
//!   id so a re-poll of a still-active alert never duplicates.
//! - **hourly forecast → [`EnvReading`]** (`environment/nws/YYYY-MM.jsonl`,
//!   month of `ts`) — `temperature` / `precip_probability` / `wind_speed` per
//!   forecast hour, each keyed by `nws-<metric>-<startTime>` so a re-fetch of a
//!   future hour *updates that hour in place* (the forecast for a given hour
//!   changes between polls) instead of duplicating.
//!
//! A **raw layer** (`environment/nws/raw/YYYY-MM.jsonl`) keeps the full alert
//! `feature` objects and the full forecast `period` objects verbatim — the
//! contract drops the alert geometry polygon and some forecast detail, so the
//! raw line is the source of truth for anything the normalized columns don't
//! carry.
//!
//! **Keyless, but a `User-Agent` is mandatory:** api.weather.gov rejects any
//! request without one. Every call sends `User-Agent` + `Accept:
//! application/geo+json`.
//!
//! **US-only, degrades silently:** outside the US the `/points` lookup 404s →
//! no gridpoint, so the forecast is skipped (not an error). With no location at
//! all (no CoreLocation grant, no manual location) the collector is completely
//! inert — no rows, no error — exactly like [`crate::weather`].

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::{EnvGeoEvent, EnvReading};
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

/// Contract-layer reading directory; geo-events go one level deeper in
/// `events/`, raw objects in `raw/`.
const DIR: &str = "environment/nws";
const EVENTS_DIR: &str = "environment/nws/events";
const RAW_DIR: &str = "environment/nws/raw";
/// Non-secret rebuildable cursor — last-used location (for the ladder
/// fallback) + last poll time. Deleting it costs nothing; the next pass
/// re-derives everything.
const SYNC_FILE: &str = ".trove/nws-sync.json";

const SOURCE: &str = "nws";
const API_BASE: &str = "https://api.weather.gov";
/// NWS rejects requests with no User-Agent; it asks for an identifying string.
const USER_AGENT: &str = "Trove (https://trove.app)";
/// GeoJSON is the documented content type for these endpoints.
const ACCEPT: &str = "application/geo+json";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// NWS asks for a polling floor of ~30 minutes for alerts; the load is
/// trivial and the forecast doesn't change faster than that meaningfully.
pub const NWS_SYNC_SECS: u64 = 1800;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Prefer the cursor's poll time; else the newest data partition (events
    // are the urgent arm, so check them first, then readings).
    vault
        .read_nws_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join(EVENTS_DIR)))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(DIR)))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        // A manual location works without the grant.
        required: false,
    }
}

// Periodic pass: the same pull the manual "Sync now" runs, but it never errors
// the loop — no location, a non-US point, or a network blip is just a quiet
// no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let alerts = out.counts.get("alerts").copied().unwrap_or(0);
            let readings = out.counts.get("readings").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(
                alerts > 0 || readings > 0,
                || format!("nws synced — {alerts} alerts, {readings} forecast readings"),
            ))
        }
        // Transient network/parse failure: stay silent, retry next tick.
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("nws sync skipped: {e}"))),
    }
}

// Manual "Sync now": surfaces a human headline (errors propagate to the UI).
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let alerts = out.counts.get("alerts").copied().unwrap_or(0);
    let readings = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if alerts == 0 && readings == 0 {
        "National Weather Service: nothing new (no active alerts; forecast up to date)".to_string()
    } else {
        format!("NWS synced — {alerts} active alerts, {readings} forecast readings")
    };
    Ok(out_with_headline(out, headline))
}

fn out_with_headline(out: PullOutcome, headline: String) -> PullOutcome {
    PullOutcome { headline, counts: out.counts }
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. Keyless — no
/// `connection`. Rides a ~30-minute cadence; inert until a location exists.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "nws",
        name: "National Weather Service",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Captures US National Weather Service alerts — tornado warnings, \
                      flood watches, winter storms, fire-weather, and 100+ more — alongside \
                      the official hourly forecast for your location, from the keyless public \
                      api.weather.gov. No account; only coordinates ever leave this Mac.",
        domain: "environment",
        vault_path: "environment/nws/",
        toggleable: true,
        setup: &[
            "No account or key — this uses the keyless public api.weather.gov.",
            "Approve Location Services when asked, or set a location manually on the Weather tab; without a location this stays inert.",
        ],
        caveats: "US and territories only — outside the US there's no NWS gridpoint, so no rows \
                  are written (no error; the Weather collector still runs). Inert until a \
                  location exists. Alert history is only ~7 days, so timely capture matters — \
                  this is why it polls live rather than backfilling.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(NWS_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state: last-used location (ladder rung 3) + last poll time.
/// Losing it costs nothing — the next pass re-derives the location and the
/// upsert keeps re-polling idempotent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct NwsSyncState {
    /// RFC3339 local time of the last pass that got as far as a decision.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// The location last used successfully (ladder rung 3 fallback).
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    /// Non-empty while the collector is stuck (no location, network error) —
    /// for the UI banner.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_nws_sync(&self) -> Option<NwsSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_nws_sync(&self, state: &NwsSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// The three GETs this collector makes. Tests implement this against canned
/// fixtures; production hits the real api.weather.gov.
trait NwsApi {
    /// `GET /alerts/active?point=LAT,LON` → GeoJSON FeatureCollection.
    fn active_alerts(&self, lat: f64, lon: f64) -> Result<Value>;
    /// `GET /points/LAT,LON` → the point metadata (forecast URLs +
    /// relativeLocation). `Ok(None)` for a non-US point (HTTP 404) — not an
    /// error, just "no gridpoint here".
    fn points(&self, lat: f64, lon: f64) -> Result<Option<Value>>;
    /// `GET <forecastHourly URL>` → the hourly forecast.
    fn forecast_hourly(&self, url: &str) -> Result<Value>;
}

/// The live client over ureq. Base URL injected only so a test could point it
/// at a stub server if needed; the trait is the real test seam.
struct NwsClient {
    base: String,
}

impl NwsClient {
    fn new(base: String) -> Self {
        NwsClient { base }
    }

    fn req(url: &str) -> ureq::Request {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", USER_AGENT)
            .set("Accept", ACCEPT)
    }
}

impl NwsApi for NwsClient {
    fn active_alerts(&self, lat: f64, lon: f64) -> Result<Value> {
        let url = format!("{}/alerts/active?point={lat},{lon}", self.base);
        Self::req(&url)
            .call()
            .context("requesting NWS active alerts")?
            .into_json()
            .context("reading NWS alerts response")
    }

    fn points(&self, lat: f64, lon: f64) -> Result<Option<Value>> {
        let url = format!("{}/points/{lat},{lon}", self.base);
        match Self::req(&url).call() {
            Ok(resp) => Ok(Some(resp.into_json().context("reading NWS points response")?)),
            // Non-US point: no gridpoint. Skip the forecast, don't error.
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(e).context("requesting NWS gridpoint"),
        }
    }

    fn forecast_hourly(&self, url: &str) -> Result<Value> {
        Self::req(url)
            .call()
            .context("requesting NWS hourly forecast")?
            .into_json()
            .context("reading NWS forecast response")
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Best-effort RFC3339 → local-offset RFC3339. NWS already returns offset-aware
/// timestamps (`...-07:00`); we re-render them via the local timezone so the
/// vault's `ts` is consistent with every other collector. An unparseable value
/// is passed through verbatim (tolerant — better an odd string than a dropped
/// row).
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// A non-empty trimmed string from `properties[key]`, or "".
fn prop_str(props: &Value, key: &str) -> String {
    props.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Parse one `/alerts/active` FeatureCollection into (contract geo-events, raw
/// feature objects). `lat`/`lon` are the queried point — NWS alerts carry a
/// county/zone polygon, not a point, so we stamp the coordinates we asked
/// about. A feature missing both an `id` (its guid) and any usable timestamp is
/// skipped — a geo-event with no dedupe key can't be stored.
fn parse_alerts(body: &Value, lat: f64, lon: f64) -> (Vec<EnvGeoEvent>, Vec<Value>) {
    let features = match body.get("features").and_then(Value::as_array) {
        Some(f) => f.clone(),
        None => return (Vec::new(), Vec::new()),
    };
    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for feature in features {
        let props = feature.get("properties").cloned().unwrap_or(Value::Null);
        // guid: properties.id (the alert URN). Required for a geo-event.
        let guid = prop_str(&props, "id");
        let guid = if guid.is_empty() {
            // Fall back to the feature's top-level @id if properties.id is absent.
            feature.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string()
        } else {
            guid
        };
        if guid.is_empty() {
            continue; // no dedupe key → can't store
        }
        // ts: onset || effective || sent.
        let ts_raw = ["onset", "effective", "sent"]
            .iter()
            .map(|k| prop_str(&props, k))
            .find(|s| !s.is_empty());
        let Some(ts_raw) = ts_raw else {
            continue; // no usable time → can't partition
        };
        let ts = to_local(&ts_raw);

        // url = the canonical link to the event page. The FEATURE's top-level
        // `id` is the real API URL (`https://api.weather.gov/alerts/...`);
        // `properties.id` is a bare URN (identical to `guid`), not a link. Use
        // the http link when present; otherwise synthesize one from the guid so
        // `url` is never a bare URN.
        let url = {
            let top = feature.get("id").and_then(Value::as_str).unwrap_or("").trim();
            if top.starts_with("http") {
                top.to_string()
            } else {
                format!("{API_BASE}/alerts/{guid}")
            }
        };
        let expires = {
            let e = prop_str(&props, "expires");
            if e.is_empty() {
                String::new()
            } else {
                to_local(&e)
            }
        };

        rows.push(EnvGeoEvent {
            ts,
            source: SOURCE.into(),
            guid: guid.clone(),
            event_type: "alert".into(),
            magnitude: None,
            place: prop_str(&props, "areaDesc"),
            lat: Some(lat),
            lon: Some(lon),
            severity: prop_str(&props, "severity"),
            headline: prop_str(&props, "headline"),
            url,
            expires,
            // The full `properties` ride verbatim (event, urgency, certainty,
            // description, instruction, sender, …) — everything the normalized
            // columns don't carry.
            extra: props_to_map(&props),
        });
        raws.push(feature);
    }
    (rows, raws)
}

/// `properties` as an object map for `extra`; empty when it isn't an object.
fn props_to_map(props: &Value) -> Map<String, Value> {
    match props {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    }
}

/// The forecast URL + the human place label from a `/points` response.
struct PointInfo {
    forecast_hourly: String,
    place: String,
}

/// Parse `/points` → the `forecastHourly` URL and a `relativeLocation`
/// "City, ST" label. `None` when there's no `forecastHourly` (a malformed or
/// non-forecast point response — treated like "no gridpoint").
fn parse_points(body: &Value) -> Option<PointInfo> {
    let props = body.get("properties")?;
    let forecast_hourly = props.get("forecastHourly").and_then(Value::as_str)?.trim().to_string();
    if forecast_hourly.is_empty() {
        return None;
    }
    let place = props
        .get("relativeLocation")
        .and_then(|r| r.get("properties"))
        .map(|rp| {
            let city = rp.get("city").and_then(Value::as_str).unwrap_or("").trim();
            let state = rp.get("state").and_then(Value::as_str).unwrap_or("").trim();
            match (city.is_empty(), state.is_empty()) {
                (false, false) => format!("{city}, {state}"),
                (false, true) => city.to_string(),
                _ => String::new(),
            }
        })
        .unwrap_or_default();
    Some(PointInfo { forecast_hourly, place })
}

/// Parse `"10 mph"` / `"5 to 10 mph"` → (value, unit). Takes the *first*
/// number it finds (a range's low end) and the trailing unit token. `None`
/// when there's no leading number.
fn parse_wind_speed(s: &str) -> Option<(f64, String)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut chars = s.split_whitespace();
    let first = chars.next()?;
    let value: f64 = first.parse().ok()?;
    // The unit is the last whitespace token (e.g. "mph", "km/h").
    let unit = s.split_whitespace().last().unwrap_or("").to_string();
    Some((value, unit))
}

/// Parse one hourly forecast response's `periods[]` into (contract readings,
/// raw period objects). Each period emits up to three readings —
/// `temperature`, `precip_probability`, `wind_speed` — skipping any whose
/// source value is null/absent. `lat`/`lon`/`place` stamp the queried point.
/// guids are `nws-<metric>-<startTime>` so a re-fetch of the same hour upserts.
fn parse_forecast(
    body: &Value,
    lat: f64,
    lon: f64,
    place: &str,
) -> (Vec<EnvReading>, Vec<Value>) {
    let periods = match body
        .get("properties")
        .and_then(|p| p.get("periods"))
        .and_then(Value::as_array)
    {
        Some(p) => p.clone(),
        None => return (Vec::new(), Vec::new()),
    };
    let mut rows = Vec::new();
    let mut raws = Vec::new();
    for period in periods {
        let start = period.get("startTime").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if start.is_empty() {
            continue; // can't key or partition without the hour
        }
        let ts = to_local(&start);
        let short_forecast =
            period.get("shortForecast").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let wind_direction =
            period.get("windDirection").and_then(Value::as_str).unwrap_or("").trim().to_string();

        let mk_extra = || {
            let mut m = Map::new();
            m.insert("forecast".into(), Value::Bool(true));
            if !short_forecast.is_empty() {
                m.insert("short_forecast".into(), Value::String(short_forecast.clone()));
            }
            if !wind_direction.is_empty() {
                m.insert("wind_direction".into(), Value::String(wind_direction.clone()));
            }
            m
        };
        let mk = |metric: &str, value: f64, unit: String| EnvReading {
            ts: ts.clone(),
            source: SOURCE.into(),
            metric: metric.into(),
            value,
            unit,
            place: place.to_string(),
            lat: Some(lat),
            lon: Some(lon),
            station: String::new(),
            guid: Some(format!("nws-{metric}-{start}")),
            extra: mk_extra(),
        };

        let mut emitted = false;

        // temperature (value + temperatureUnit "F"/"C").
        if let Some(temp) = period.get("temperature").and_then(Value::as_f64) {
            let unit = period
                .get("temperatureUnit")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            rows.push(mk("temperature", temp, unit));
            emitted = true;
        }
        // precip_probability (probabilityOfPrecipitation.value, percent).
        if let Some(pop) = period
            .get("probabilityOfPrecipitation")
            .and_then(|p| p.get("value"))
            .and_then(Value::as_f64)
        {
            rows.push(mk("precip_probability", pop, "percent".into()));
            emitted = true;
        }
        // wind_speed (parse "10 mph" → 10 / "mph").
        if let Some(ws) = period.get("windSpeed").and_then(Value::as_str) {
            if let Some((value, unit)) = parse_wind_speed(ws) {
                rows.push(mk("wind_speed", value, unit));
                emitted = true;
            }
        }

        // Only keep a raw period when it produced at least one reading (an
        // all-null period is noise).
        if emitted {
            raws.push(period);
        }
    }
    (rows, raws)
}

// ---------------------------------------------------------------------------
// Upsert into stable month partitions (the claude_code / github idiom).

/// A raw object carrying the contract ts purely so the month-partition writer
/// files it under the right month, plus a guid for dedupe. Only `value` is
/// serialized to disk (flattened) — the raw line is the API object verbatim.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(skip)]
    guid: String,
    #[serde(flatten)]
    value: Value,
}

/// Upsert geo-events into `environment/nws/events/YYYY-MM.jsonl` by `guid`.
/// Alerts re-appear every poll until they expire, so a still-active alert must
/// update in place (its expiry/description can change), never duplicate. The
/// partition is rewritten atomically.
fn upsert_events(vault: &Vault, rows: &[EnvGeoEvent]) -> Result<u64> {
    let stream = vault.stream(EVENTS_DIR, Partition::Month);
    let mut written = 0u64;
    // Group incoming rows by partition key so we touch each file once.
    let mut by_key: std::collections::BTreeMap<String, Vec<&EnvGeoEvent>> = Default::default();
    for r in rows {
        let key = Partition::Month
            .key(&r.ts)
            .with_context(|| format!("nws alert {} has unpartitionable ts {:?}", r.guid, r.ts))?;
        by_key.entry(key.to_string()).or_default().push(r);
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

/// Upsert readings into `environment/nws/YYYY-MM.jsonl` by `guid`. The forecast
/// for a future hour is re-fetched and changes between polls, so each
/// `nws-<metric>-<hour>` row updates in place. Readings without a guid would be
/// un-upsertable — but this collector always sets one, so that can't happen.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: std::collections::BTreeMap<String, Vec<&EnvReading>> = Default::default();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!("nws reading {:?} has unpartitionable ts {:?}", r.guid, r.ts)
        })?;
        by_key.entry(key.to_string()).or_default().push(r);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<EnvReading> = stream.read(&key)?;
        for row in incoming {
            let slot = existing.iter_mut().find(|e| e.guid == row.guid && e.guid.is_some());
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

/// Upsert raw objects into `environment/nws/raw/YYYY-MM.jsonl` by guid. Full
/// fidelity — never drop what the API returned.
fn upsert_raw(vault: &Vault, lines: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: std::collections::BTreeMap<String, Vec<&RawLine>> = Default::default();
    for l in lines {
        let key = Partition::Month
            .key(&l.ts)
            .with_context(|| format!("nws raw {} has unpartitionable ts {:?}", l.guid, l.ts))?;
        by_key.entry(key.to_string()).or_default().push(l);
    }
    for (key, incoming) in by_key {
        // Existing raw lines as (guid, value) — guid lives in a synthetic
        // wrapper key we strip on read so the on-disk line stays verbatim.
        let mut existing: Vec<Value> = stream.read(&key)?;
        // Build a guid→index map over the existing raw objects so we can replace.
        for l in incoming {
            let pos = existing.iter().position(|v| raw_guid(v) == l.guid);
            match pos {
                Some(i) => existing[i] = l.value.clone(),
                None => existing.push(l.value.clone()),
            }
        }
        vault.write_snapshot(&format!("{RAW_DIR}/{key}.jsonl"), &existing)?;
    }
    Ok(())
}

/// The guid of a stored raw object: an alert feature's `properties.id` (or
/// `id`), or a forecast period's `nws-<metric>-<startTime>` is not on the raw —
/// forecast raws are deduped by `startTime` instead (one raw period per hour).
fn raw_guid(v: &Value) -> String {
    // Alert feature: properties.id || id.
    if let Some(props) = v.get("properties") {
        let p = prop_str(props, "id");
        if !p.is_empty() {
            return p;
        }
    }
    if let Some(id) = v.get("id").and_then(Value::as_str) {
        if !id.trim().is_empty() {
            return id.trim().to_string();
        }
    }
    // Forecast period: key by its startTime (one raw period per hour).
    if let Some(start) = v.get("startTime").and_then(Value::as_str) {
        if !start.trim().is_empty() {
            return format!("period-{}", start.trim());
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the point (production ladder) and sync. Inert (no rows, no error)
/// when no location is available. Non-US points skip the forecast silently.
///
/// This is the `DEF` entry: it runs the full location ladder
/// (CoreLocation → manual → cursor) and, when nothing resolves, records the
/// no-location reason on the cursor for the UI banner before returning a quiet
/// no-op. The location-independent network+write body lives in [`pull_at`] so
/// tests can drive it with an explicit point and never touch CoreLocation.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_nws_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    if point.is_none() {
        // Inert: no location yet. Record the reason for the UI, but this is not
        // an error (mirror weather.rs degradation).
        let mut state = state;
        state.updated = Local::now().to_rfc3339();
        state.error =
            "no location: grant Location Services or set a location on the Weather tab".into();
        vault.write_nws_sync(&state)?;
        return Ok(PullOutcome {
            headline: "National Weather Service: no location set".into(),
            counts: std::collections::BTreeMap::from([("alerts", 0), ("readings", 0)]),
        });
    }
    pull_at(vault, point)
}

/// The network+write body over an EXPLICIT point — the IO seam tests drive
/// directly. `None` ⇒ a clean inert no-op (no rows, no error); `Some((lat,lon))`
/// ⇒ fetch alerts+forecast and write. Hits the real api.weather.gov; the
/// location ladder is *not* consulted here (see [`pull`]).
pub fn pull_at(vault: &Vault, point: Option<(f64, f64)>) -> Result<PullOutcome> {
    let client = NwsClient::new(API_BASE.to_string());
    pull_at_with(vault, point, &client)
}

/// The pull body over an injected API + explicit point — the offline test seam.
/// `None` ⇒ inert no-op; `Some` ⇒ fetch+write through `client`.
fn pull_at_with(vault: &Vault, point: Option<(f64, f64)>, client: &impl NwsApi) -> Result<PullOutcome> {
    let Some((lat, lon)) = point else {
        // No point ⇒ inert: no network, no rows, no cursor write, no error.
        return Ok(PullOutcome {
            headline: "National Weather Service: no location set".into(),
            counts: std::collections::BTreeMap::from([("alerts", 0), ("readings", 0)]),
        });
    };
    let now = Local::now();
    let mut state = vault.read_nws_sync().unwrap_or_default();

    let mut alerts_written = 0u64;
    let mut readings_written = 0u64;
    let mut raw_lines: Vec<RawLine> = Vec::new();

    // 1. Active alerts (the time-sensitive primary).
    let alerts_body = client.active_alerts(lat, lon)?;
    let (alert_rows, alert_raws) = parse_alerts(&alerts_body, lat, lon);
    if !alert_rows.is_empty() {
        alerts_written += upsert_events(vault, &alert_rows)?;
        for (row, raw) in alert_rows.iter().zip(alert_raws.iter()) {
            raw_lines.push(RawLine { ts: row.ts.clone(), guid: row.guid.clone(), value: raw.clone() });
        }
    }

    // 2. Hourly forecast. A non-US point has no gridpoint → skip silently.
    let mut forecast_place = String::new();
    if let Some(points) = client.points(lat, lon)? {
        if let Some(info) = parse_points(&points) {
            forecast_place = info.place.clone();
            let hourly = client.forecast_hourly(&info.forecast_hourly)?;
            let (reading_rows, period_raws) = parse_forecast(&hourly, lat, lon, &info.place);
            if !reading_rows.is_empty() {
                readings_written += upsert_readings(vault, &reading_rows)?;
            }
            // Raw forecast periods: dedupe by hour (one raw per startTime).
            let mut seen_hours: HashSet<String> = HashSet::new();
            for raw in period_raws {
                let start =
                    raw.get("startTime").and_then(Value::as_str).unwrap_or("").trim().to_string();
                if start.is_empty() || !seen_hours.insert(start.clone()) {
                    continue;
                }
                raw_lines.push(RawLine {
                    ts: to_local(&start),
                    guid: format!("period-{start}"),
                    value: raw,
                });
            }
        }
    }

    // 3. Raw layer (full fidelity), upserted by the same keys.
    if !raw_lines.is_empty() {
        upsert_raw(vault, &raw_lines)?;
    }

    // 4. Advance the cursor (last-used location + poll time). Cleared error.
    state.updated = now.to_rfc3339();
    state.lat = lat;
    state.lon = lon;
    state.place = forecast_place;
    state.error = String::new();
    vault.write_nws_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{alerts_written} alerts, {readings_written} readings"),
        counts: std::collections::BTreeMap::from([
            ("alerts", alerts_written),
            ("readings", readings_written),
        ]),
    })
}

/// The location ladder, reusing exactly what [`crate::weather`] uses:
/// CoreLocation ([`corelocation::current_location`]) → the manual
/// [`Vault::weather_location`] → this collector's last-used location in its own
/// sync state. `None` → the collector is inert. We do **not** round the
/// coordinates: NWS gridpoint resolution wants the real point (it snaps to its
/// own ~2.5 km grid server-side), and the request is keyless to a government
/// API, not a per-user-tracked third party.
fn resolve_point(vault: &Vault, state: &NwsSyncState) -> Option<(f64, f64)> {
    if corelocation::auth_status() == AuthStatus::NotDetermined {
        // Fire the TCC prompt when the host carries the usage string; instant
        // when already decided (same shape as weather.rs).
        corelocation::request_access(5);
    }
    if let Some(fix) = corelocation::current_location(8) {
        return Some((fix.lat, fix.lon));
    }
    if let Some(m) = vault.weather_location() {
        return Some((m.lat, m.lon));
    }
    // Last known: 0,0 is the Atlantic off Ghana, safe as "unset".
    (state.lat != 0.0 || state.lon != 0.0).then_some((state.lat, state.lon))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-nws-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures (modeled on documented api.weather.gov responses) ----------

    /// A `/alerts/active` FeatureCollection with one active alert. Shape follows
    /// the documented GeoJSON: a `features[]` of `{id, geometry, properties}`.
    fn alerts_present() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [
                {
                    "id": "https://api.weather.gov/alerts/urn:oid:2.49.0.1.840.0.abc123",
                    "type": "Feature",
                    "geometry": { "type": "Polygon", "coordinates": [[[-119.3,34.6],[-119.0,34.6],[-119.0,34.4],[-119.3,34.4],[-119.3,34.6]]] },
                    "properties": {
                        "id": "urn:oid:2.49.0.1.840.0.abc123",
                        "areaDesc": "Ventura County Mountains",
                        "sent": "2026-06-10T10:30:00-07:00",
                        "effective": "2026-06-10T10:45:00-07:00",
                        "onset": "2026-06-10T11:00:00-07:00",
                        "expires": "2026-06-11T20:00:00-07:00",
                        "ends": "2026-06-11T20:00:00-07:00",
                        "status": "Actual",
                        "messageType": "Alert",
                        "severity": "Severe",
                        "certainty": "Likely",
                        "urgency": "Expected",
                        "event": "Red Flag Warning",
                        "headline": "Red Flag Warning issued June 10 at 11:00AM PDT until June 11 at 8:00PM PDT",
                        "description": "GUSTY WINDS AND LOW HUMIDITY...",
                        "instruction": "A Red Flag Warning means critical fire weather..."
                    }
                }
            ]
        })
    }

    /// An empty `/alerts/active` response — no active alerts at this point.
    fn alerts_empty() -> Value {
        json!({ "type": "FeatureCollection", "features": [] })
    }

    /// A `/points/LAT,LON` response: the `forecastHourly` URL + relativeLocation.
    fn points_us() -> Value {
        json!({
            "properties": {
                "gridId": "LOX",
                "gridX": 155,
                "gridY": 45,
                "forecast": "https://api.weather.gov/gridpoints/LOX/155,45/forecast",
                "forecastHourly": "https://api.weather.gov/gridpoints/LOX/155,45/forecast/hourly",
                "relativeLocation": {
                    "type": "Feature",
                    "properties": { "city": "Los Angeles", "state": "CA" }
                }
            }
        })
    }

    /// An hourly forecast with two periods. The first carries a full set; the
    /// second has a null PoP and an empty windSpeed (those metrics are skipped).
    fn forecast_hourly_v1() -> Value {
        json!({
            "properties": {
                "periods": [
                    {
                        "number": 1,
                        "startTime": "2026-06-10T11:00:00-07:00",
                        "endTime": "2026-06-10T12:00:00-07:00",
                        "isDaytime": true,
                        "temperature": 72,
                        "temperatureUnit": "F",
                        "probabilityOfPrecipitation": { "unitCode": "wmoUnit:percent", "value": 20 },
                        "windSpeed": "10 mph",
                        "windDirection": "SW",
                        "shortForecast": "Sunny"
                    },
                    {
                        "number": 2,
                        "startTime": "2026-06-10T12:00:00-07:00",
                        "endTime": "2026-06-10T13:00:00-07:00",
                        "isDaytime": true,
                        "temperature": 74,
                        "temperatureUnit": "F",
                        "probabilityOfPrecipitation": { "unitCode": "wmoUnit:percent", "value": null },
                        "windSpeed": "",
                        "windDirection": "SW",
                        "shortForecast": "Sunny"
                    }
                ]
            }
        })
    }

    /// The same hourly URL re-fetched later: hour 11:00 now reads 75°F / 35%
    /// (the forecast was revised). Proves an upsert, not a duplicate.
    fn forecast_hourly_v2() -> Value {
        json!({
            "properties": {
                "periods": [
                    {
                        "number": 1,
                        "startTime": "2026-06-10T11:00:00-07:00",
                        "endTime": "2026-06-10T12:00:00-07:00",
                        "isDaytime": true,
                        "temperature": 75,
                        "temperatureUnit": "F",
                        "probabilityOfPrecipitation": { "unitCode": "wmoUnit:percent", "value": 35 },
                        "windSpeed": "15 mph",
                        "windDirection": "W",
                        "shortForecast": "Sunny"
                    }
                ]
            }
        })
    }

    // --- A configurable stub fetcher -----------------------------------------

    struct Stub {
        alerts: Value,
        points: Option<Value>,
        forecast: Value,
    }
    impl NwsApi for Stub {
        fn active_alerts(&self, _lat: f64, _lon: f64) -> Result<Value> {
            Ok(self.alerts.clone())
        }
        fn points(&self, _lat: f64, _lon: f64) -> Result<Option<Value>> {
            Ok(self.points.clone())
        }
        fn forecast_hourly(&self, _url: &str) -> Result<Value> {
            Ok(self.forecast.clone())
        }
    }

    /// A canned US point (LA) the seeded pull tests pass EXPLICITLY to
    /// [`pull_at_with`], so no test ever calls CoreLocation / the location
    /// ladder. The production ladder is exercised only via `pull` in production.
    const SEED: (f64, f64) = (34.05, -118.24);

    // --- Pure parser tests ----------------------------------------------------

    #[test]
    fn parse_alerts_maps_the_contract_fields() {
        let (rows, raws) = parse_alerts(&alerts_present(), 34.05, -118.24);
        assert_eq!(rows.len(), 1);
        assert_eq!(raws.len(), 1);
        let a = &rows[0];
        assert_eq!(a.source, "nws");
        assert_eq!(a.event_type, "alert");
        assert_eq!(a.guid, "urn:oid:2.49.0.1.840.0.abc123", "guid = properties.id");
        // ts = onset, re-rendered to a local offset (same instant).
        assert_eq!(
            DateTime::parse_from_rfc3339(&a.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T11:00:00-07:00").unwrap().timestamp()
        );
        assert_eq!(a.severity, "Severe");
        assert_eq!(a.headline.as_str(), "Red Flag Warning issued June 10 at 11:00AM PDT until June 11 at 8:00PM PDT");
        assert_eq!(a.place, "Ventura County Mountains");
        assert_eq!(
            DateTime::parse_from_rfc3339(&a.expires).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-11T20:00:00-07:00").unwrap().timestamp()
        );
        assert_eq!(a.lat, Some(34.05));
        assert_eq!(a.lon, Some(-118.24));
        // The full properties ride in extra.
        assert_eq!(a.extra.get("event"), Some(&json!("Red Flag Warning")));
        assert_eq!(a.extra.get("urgency"), Some(&json!("Expected")));
        assert_eq!(a.extra.get("certainty"), Some(&json!("Likely")));
        assert!(a.extra.get("description").is_some());
    }

    #[test]
    fn alert_url_is_the_http_link_not_the_urn() {
        // Regression: `url` is the canonical event page (the feature's TOP-LEVEL
        // http `id`), NOT the bare `properties.id` URN (which == guid). Before
        // the fix, `url` was sourced from `properties.id`, so url == urn.
        let body = json!({
            "type": "FeatureCollection",
            "features": [{
                "id": "https://api.weather.gov/alerts/urn:oid:X",
                "type": "Feature",
                "properties": {
                    "id": "urn:oid:X",
                    "areaDesc": "Somewhere",
                    "onset": "2026-06-10T11:00:00-07:00",
                    "severity": "Severe",
                    "headline": "Test"
                }
            }]
        });
        let (rows, _) = parse_alerts(&body, 34.05, -118.24);
        assert_eq!(rows.len(), 1);
        let a = &rows[0];
        // guid stays the URN (= properties.id).
        assert_eq!(a.guid, "urn:oid:X", "guid = properties.id (the URN)");
        // url is the http event link, distinct from the guid.
        assert_eq!(
            a.url, "https://api.weather.gov/alerts/urn:oid:X",
            "url = the feature's top-level http id, not the bare URN"
        );
        assert_ne!(a.url, a.guid, "url must not be the bare URN");
        assert!(a.url.starts_with("http"), "url is never a bare urn:");
    }

    #[test]
    fn alert_url_falls_back_to_synthesized_link_never_a_bare_urn() {
        // If the feature has no usable top-level http id, synthesize the API URL
        // from the guid — `url` must never be left a bare `urn:oid:` string.
        let body = json!({
            "type": "FeatureCollection",
            "features": [{
                // No top-level `id` at all — guid must come from properties.id.
                "type": "Feature",
                "properties": {
                    "id": "urn:oid:Y",
                    "areaDesc": "Somewhere",
                    "onset": "2026-06-10T11:00:00-07:00"
                }
            }]
        });
        let (rows, _) = parse_alerts(&body, 34.05, -118.24);
        assert_eq!(rows.len(), 1);
        let a = &rows[0];
        assert_eq!(a.guid, "urn:oid:Y");
        assert_eq!(
            a.url, "https://api.weather.gov/alerts/urn:oid:Y",
            "synthesized http link from guid"
        );
        assert!(!a.url.starts_with("urn:"), "url is never a bare urn");
    }

    #[test]
    fn parse_forecast_emits_three_metrics_and_skips_nulls() {
        let (rows, raws) = parse_forecast(&forecast_hourly_v1(), 34.05, -118.24, "Los Angeles, CA");
        // Period 1: temperature + precip_probability + wind_speed = 3.
        // Period 2: temperature only (null PoP, empty windSpeed) = 1.
        assert_eq!(rows.len(), 4);
        // Two periods both produced a reading → two raw periods.
        assert_eq!(raws.len(), 2);

        let temp = rows.iter().find(|r| r.metric == "temperature").unwrap();
        assert_eq!(temp.value, 72.0);
        assert_eq!(temp.unit, "F");
        assert_eq!(temp.place, "Los Angeles, CA");
        assert_eq!(temp.lat, Some(34.05));
        assert_eq!(temp.guid.as_deref(), Some("nws-temperature-2026-06-10T11:00:00-07:00"));
        assert_eq!(temp.extra.get("forecast"), Some(&json!(true)));
        assert_eq!(temp.extra.get("short_forecast"), Some(&json!("Sunny")));
        assert_eq!(temp.extra.get("wind_direction"), Some(&json!("SW")));

        let pop = rows.iter().find(|r| r.metric == "precip_probability").unwrap();
        assert_eq!(pop.value, 20.0);
        assert_eq!(pop.unit, "percent");

        let wind = rows.iter().find(|r| r.metric == "wind_speed").unwrap();
        assert_eq!(wind.value, 10.0);
        assert_eq!(wind.unit, "mph");

        // Period 2 (12:00) emitted only temperature.
        let p2: Vec<_> = rows.iter().filter(|r| r.ts.contains("12:00")).collect();
        assert_eq!(p2.len(), 1, "null PoP + empty windSpeed skipped");
        assert_eq!(p2[0].metric, "temperature");
        assert_eq!(p2[0].value, 74.0);
    }

    #[test]
    fn wind_speed_parser() {
        assert_eq!(parse_wind_speed("10 mph"), Some((10.0, "mph".into())));
        assert_eq!(parse_wind_speed("5 to 10 mph"), Some((5.0, "mph".into())), "range → low end");
        assert_eq!(parse_wind_speed("15 km/h"), Some((15.0, "km/h".into())));
        assert_eq!(parse_wind_speed(""), None);
        assert_eq!(parse_wind_speed("calm"), None, "no leading number");
    }

    #[test]
    fn parse_points_extracts_url_and_place() {
        let info = parse_points(&points_us()).unwrap();
        assert_eq!(info.forecast_hourly, "https://api.weather.gov/gridpoints/LOX/155,45/forecast/hourly");
        assert_eq!(info.place, "Los Angeles, CA");
        // A response with no forecastHourly → None.
        assert!(parse_points(&json!({"properties": {}})).is_none());
    }

    // --- Pull / store / dedupe tests -----------------------------------------

    #[test]
    fn pull_writes_alert_geoevent_and_forecast_readings_plus_raw() {
        let v = temp_vault("pull");
        let stub = Stub {
            alerts: alerts_present(),
            points: Some(points_us()),
            forecast: forecast_hourly_v1(),
        };
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(out.counts.get("alerts"), Some(&1));
        assert_eq!(out.counts.get("readings"), Some(&4));

        // Geo-event landed in environment/nws/events/, month of ts (June).
        let events =
            std::fs::read_to_string(v.root().join("environment/nws/events/2026-06.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 1);
        assert!(events.contains("\"event_type\":\"alert\""));
        assert!(events.contains("urn:oid:2.49.0.1.840.0.abc123"));

        // Readings landed in environment/nws/, month of ts.
        let readings =
            std::fs::read_to_string(v.root().join("environment/nws/2026-06.jsonl")).unwrap();
        assert_eq!(readings.lines().count(), 4);
        assert!(readings.contains("\"metric\":\"temperature\""));
        assert!(readings.contains("\"metric\":\"precip_probability\""));
        assert!(readings.contains("\"metric\":\"wind_speed\""));
        assert!(readings.contains("\"forecast\":true"));

        // Raw layer: 1 alert feature + 2 forecast periods = 3 lines, verbatim.
        let raw = std::fs::read_to_string(v.root().join("environment/nws/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 3);
        // Verbatim alert geometry (dropped from the contract) is present in raw.
        assert!(raw.contains("\"geometry\""), "raw keeps the alert polygon");
        assert!(raw.contains("\"coordinates\""));
        // Verbatim forecast detail (endTime, isDaytime) is present in raw.
        assert!(raw.contains("\"endTime\""));
        assert!(raw.contains("\"isDaytime\""));

        // Cursor advanced with the place + cleared error.
        let state = v.read_nws_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert_eq!(state.place, "Los Angeles, CA");
        assert!(state.error.is_empty());
    }

    #[test]
    fn alert_repoll_does_not_duplicate_upserts_by_guid() {
        let v = temp_vault("alert-dedup");
        let stub = Stub {
            alerts: alerts_present(),
            points: Some(points_us()),
            forecast: forecast_hourly_v1(),
        };
        // First poll.
        pull_at_with(&v, Some(SEED), &stub).unwrap();
        let events_1 =
            std::fs::read_to_string(v.root().join("environment/nws/events/2026-06.jsonl")).unwrap();
        assert_eq!(events_1.lines().count(), 1);

        // Re-poll: the same alert is still active and comes back identically.
        let out2 = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(out2.counts.get("alerts"), Some(&1), "still seen and re-upserted");
        let events_2 =
            std::fs::read_to_string(v.root().join("environment/nws/events/2026-06.jsonl")).unwrap();
        assert_eq!(events_2.lines().count(), 1, "upsert by guid — no duplicate row");
        assert_eq!(events_1, events_2, "identical alert → byte-identical file");

        // Raw alert feature also deduped (1 alert + 2 periods, unchanged).
        let raw = std::fs::read_to_string(v.root().join("environment/nws/raw/2026-06.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 3, "raw upserts by guid too");
    }

    #[test]
    fn forecast_refetch_upserts_the_hour_not_duplicates() {
        let v = temp_vault("forecast-upsert");
        // First poll: hour 11:00 = 72°F / 20% / 10 mph (+ hour 12:00 temp).
        pull_at_with(
            &v,
            Some(SEED),
            &Stub {
                alerts: alerts_empty(),
                points: Some(points_us()),
                forecast: forecast_hourly_v1(),
            },
        )
        .unwrap();
        let r1 = std::fs::read_to_string(v.root().join("environment/nws/2026-06.jsonl")).unwrap();
        assert_eq!(r1.lines().count(), 4, "3 metrics @11:00 + 1 @12:00");

        // Second poll: the forecast for 11:00 was revised to 75°F / 35% / 15 mph.
        pull_at_with(
            &v,
            Some(SEED),
            &Stub {
                alerts: alerts_empty(),
                points: Some(points_us()),
                forecast: forecast_hourly_v2(),
            },
        )
        .unwrap();
        let rows: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        // Still exactly the same set of (metric, hour) keys — the 11:00 hour was
        // upserted, the 12:00 temp from poll 1 remains. v2 has no 12:00 period,
        // so: 3 (@11:00 upserted) + 1 (@12:00, untouched) = 4. No duplicates.
        assert_eq!(rows.len(), 4, "upsert by guid — the hour updated in place");
        let temp_11 = rows
            .iter()
            .find(|r| r.metric == "temperature" && r.ts.contains("11:00"))
            .unwrap();
        assert_eq!(temp_11.value, 75.0, "11:00 temperature updated to the new value");
        let pop_11 = rows
            .iter()
            .find(|r| r.metric == "precip_probability" && r.ts.contains("11:00"))
            .unwrap();
        assert_eq!(pop_11.value, 35.0, "11:00 precip updated");
        let wind_11 = rows
            .iter()
            .find(|r| r.metric == "wind_speed" && r.ts.contains("11:00"))
            .unwrap();
        assert_eq!(wind_11.value, 15.0, "11:00 wind updated");
        // The untouched 12:00 temperature from poll 1 is still there.
        assert!(rows.iter().any(|r| r.metric == "temperature" && r.ts.contains("12:00")));
    }

    #[test]
    fn empty_alerts_writes_no_rows_no_error() {
        let v = temp_vault("no-alerts");
        let out = pull_at_with(
            &v,
            Some(SEED),
            &Stub {
                alerts: alerts_empty(),
                points: Some(points_us()),
                forecast: forecast_hourly_v1(),
            },
        )
        .unwrap();
        assert_eq!(out.counts.get("alerts"), Some(&0), "no active alerts → no rows");
        // No events file at all.
        assert!(
            !v.root().join("environment/nws/events").exists(),
            "no events dir when there were no alerts"
        );
        // Forecast still wrote (alerts and forecast are independent arms).
        assert!(v.root().join("environment/nws/2026-06.jsonl").exists());
        let state = v.read_nws_sync().unwrap();
        assert!(state.error.is_empty(), "empty alerts is not an error");
    }

    #[test]
    fn non_us_point_skips_forecast_no_error() {
        let v = temp_vault("non-us");
        // points() returns None — the live client maps a 404 to this. Alerts
        // come back empty (NWS returns an empty collection for non-US points).
        let out = pull_at_with(
            &v,
            Some(SEED),
            &Stub { alerts: alerts_empty(), points: None, forecast: json!({}) },
        )
        .unwrap();
        assert_eq!(out.counts.get("alerts"), Some(&0));
        assert_eq!(out.counts.get("readings"), Some(&0), "no gridpoint → no forecast rows");
        // No forecast file, no error.
        assert!(!v.root().join("environment/nws/2026-06.jsonl").exists());
        let state = v.read_nws_sync().unwrap();
        assert!(state.error.is_empty(), "non-US degrade is silent, not an error");
    }

    #[test]
    fn pull_at_none_is_inert() {
        // Deterministic, CoreLocation-free: an explicit `None` point ⇒ a clean
        // no-op. No network is touched (the `None` short-circuits before any
        // request), so this is safe and fast on any machine regardless of its
        // Location Services grant. No rows, no error.
        let v = temp_vault("inert");
        let out = pull_at(&v, None).unwrap();
        assert_eq!(out.counts.get("alerts"), Some(&0));
        assert_eq!(out.counts.get("readings"), Some(&0));
        // Nothing written under environment/nws/ at all.
        assert!(!v.root().join("environment/nws/2026-06.jsonl").exists());
        assert!(!v.root().join("environment/nws/events").exists());
        assert!(!v.root().join("environment/nws/raw").exists());
        // A None point is a pure no-op — it doesn't even write the cursor (the
        // production `pull` is what records the no-location banner reason).
        assert!(v.read_nws_sync().is_none(), "no point ⇒ no cursor write");
    }

    #[test]
    fn sync_state_round_trips_and_back_compat() {
        // Empty object (a brand-new/missing cursor).
        let empty: NwsSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.lat, 0.0);
        assert!(empty.updated.is_empty());
        // Round-trip with data.
        let v = temp_vault("sync");
        v.write_nws_sync(&NwsSyncState {
            updated: "2026-06-10T11:00:00-07:00".into(),
            lat: 34.05,
            lon: -118.24,
            place: "Los Angeles, CA".into(),
            error: String::new(),
        })
        .unwrap();
        let s = v.read_nws_sync().unwrap();
        assert_eq!(s.lat, 34.05);
        assert_eq!(s.place, "Los Angeles, CA");
        assert!(s.error.is_empty());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Forward/back compat: a minimal geo-event line (only the 4 required
        // fields) and a minimal reading line (only the 4 required) must parse —
        // proves the upsert read path tolerates sparse old data.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/nws/events")).unwrap();
        std::fs::write(
            v.root().join("environment/nws/events/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T11:00:00-07:00\",\"source\":\"nws\",\"guid\":\"old-1\",\"event_type\":\"alert\"}\n",
        )
        .unwrap();
        let events: Vec<EnvGeoEvent> =
            v.stream(EVENTS_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].guid, "old-1");

        std::fs::write(
            v.root().join("environment/nws/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T11:00:00-07:00\",\"source\":\"nws\",\"metric\":\"temperature\",\"value\":70}\n",
        )
        .unwrap();
        let readings: Vec<EnvReading> = v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].value, 70.0);
        assert_eq!(readings[0].guid, None, "sparse reading has no guid");
    }
}
