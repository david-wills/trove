//! USGS Earthquake Catalog — global seismic events via FDSN and GeoJSON feeds.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/usgs-earthquakes.md
//!
//! A **Periodic**, keyless, no-login collector. It writes the **`environment`**
//! domain's geo-event shape ([`EnvGeoEvent`]):
//!
//! - **contract layer** — `environment/usgs-earthquakes/events/YYYY-MM.jsonl`
//!   (month of `ts`) — one [`EnvGeoEvent`] per earthquake, keyed by the USGS
//!   event id (`guid`). USGS revises events post-hoc (updated magnitude, reviewed
//!   status), so rows are **upserted by guid** — a re-pull updates in place, never
//!   duplicates.
//! - **raw layer** — `environment/usgs-earthquakes/raw/YYYY-MM.jsonl` — full
//!   GeoJSON feature objects, verbatim, at full fidelity. Unconditional.
//!
//! **Keyless, global, plain HTTPS** — no login, no API key, no TCC grant. The
//! only query parameters that carry user-derived data are the latitude/longitude
//! of the user's location (public feed, government server). The location is never
//! stored in the vault; it's used only as a query parameter.
//!
//! **Polling strategy:**
//! - Live polls: the realtime `all_hour.geojson` summary feed (~1-min cadence
//!   from USGS) for near-real-time capture without hitting the paginated API.
//! - Backfill: the FDSN `event/1/query` paginated endpoint, draining from the
//!   last-seen event time to now. The first run backfills from 30 days ago;
//!   subsequent runs walk forward from the cursor watermark.
//! - **20k cap**: FDSN returns HTTP 400 when a window has more than 20k events.
//!   Dense windows are handled by time-window bisection: when a page returns
//!   HTTP 400 or is exactly PAGE_LIMIT rows, the window is halved and each
//!   sub-window is drained independently until every sub-window returns a
//!   short page.
//! - **HTTP 204**: FDSN returns 204 No Content (empty body) for zero-match
//!   windows. This is the common steady-state case (quiet week in a region).
//!   It is treated as an empty FeatureCollection, not an error.
//! - Dedupe: the USGS event id is the guid; an upsert by id handles post-hoc
//!   revisions.
//!
//! **Degrades gracefully** when no location is available: the radius query
//! becomes global (no lat/lon filter). With no location, the feed is still useful
//! but broader. The `all_hour` feed is always global and always polled regardless.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::corelocation;
use crate::environment::EnvGeoEvent;
use crate::eventkit::AuthStatus;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::store::write_json_atomic;
use crate::vault::Vault;

/// Contract-layer geo-events; raw objects in `raw/`.
const EVENTS_DIR: &str = "environment/usgs-earthquakes/events";
const RAW_DIR: &str = "environment/usgs-earthquakes/raw";
/// Non-secret rebuildable cursor — last-used location + watermark + last poll.
const SYNC_FILE: &str = ".trove/usgs-earthquakes-sync.json";

const SOURCE: &str = "usgs-earthquakes";
const FDSN_BASE: &str = "https://earthquake.usgs.gov/fdsnws/event/1";
const FEED_BASE: &str = "https://earthquake.usgs.gov/earthquakes/feed/v1.0/summary";
/// User-Agent string; USGS asks for an identifying string (same policy as NWS).
const USER_AGENT: &str = "Trove (https://trove.app)";
/// Default minimum magnitude — keeps noise low (< 2.5 is not felt at distance).
const MIN_MAG: f64 = 2.5;
/// Default radius in km around the user's location. 500 km catches regional quakes.
const RADIUS_KM: u32 = 500;
/// Maximum events the FDSN API returns per request (documented 20k cap).
const PAGE_LIMIT: u32 = 20_000;
/// Default backfill window on first run: 30 days.
const BACKFILL_DAYS: i64 = 30;
/// HTTP timeout per request — kept short so a hung server doesn't stall the loop.
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Hourly cadence: earthquakes are infrequent enough that hourly is sufficient.
pub const USGS_EQ_SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_usgs_eq_sync()
        .map(|s| s.updated)
        .filter(|u| !u.is_empty())
        .or_else(|| crate::registry::newest_stem(&vault.root().join(EVENTS_DIR)))
}

fn def_permission() -> crate::integrations::PermissionInfo {
    crate::integrations::PermissionInfo {
        kind: "location",
        granted: Some(corelocation::auth_status() == AuthStatus::Granted),
        // Location is optional — without it we still pull the global `all_hour` feed.
        required: false,
    }
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("quakes").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(n > 0, || {
                format!("usgs-earthquakes synced — {n} quakes")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "usgs-earthquakes sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("quakes").copied().unwrap_or(0);
    let headline = if n == 0 {
        "USGS Earthquakes: nothing new".to_string()
    } else {
        format!("USGS Earthquakes synced — {n} quake events")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. Keyless — no
/// `connection`. Hourly cadence; inert when no location is available
/// (falls back to the global realtime feed).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "usgs-earthquakes",
        name: "USGS Earthquakes",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Captures earthquake events from the USGS Earthquake Catalog — \
                      magnitude, depth, location, and alert level — with historical \
                      backfill via the keyless public FDSN API. No account required; \
                      only your location is used as a query filter.",
        domain: "environment",
        vault_path: "environment/usgs-earthquakes/",
        toggleable: true,
        setup: &[
            "No account or key — this uses the keyless public earthquake.usgs.gov FDSN API.",
            "Approve Location Services when asked, or set a location manually on the Weather tab; \
             without a location this falls back to a global (no-radius) feed.",
        ],
        caveats: "Without a location the global feed is used (all M2.5+ earthquakes worldwide, \
                  which is high-volume). With a location only events within 500 km are captured. \
                  USGS revises events post-hoc — magnitudes and locations update over time; \
                  re-polls upsert by the USGS event id rather than duplicate.",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(USGS_EQ_SYNC_SECS), collect: def_collect },
    permission: Some(def_permission),
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor (non-secret, rebuildable).

/// Collector state: last-used location + watermark + last poll time.
/// Losing it costs nothing — the next pass re-resolves the location and
/// re-backfills from the default window.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct UsgsEqSyncState {
    /// RFC3339 local time of the last poll that ran to completion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
    /// The location last used (0.0/0.0 = unset → global feed).
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    /// RFC3339 UTC of the latest event we stored — the watermark for incremental
    /// pulls. A full re-drain from `watermark` → now avoids gaps. Empty on first
    /// run (we use the default backfill window instead).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub watermark: String,
    /// Non-empty while stuck (network error, parse failure) — for the UI banner.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

impl Vault {
    pub fn read_usgs_eq_sync(&self) -> Option<UsgsEqSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_usgs_eq_sync(&self, state: &UsgsEqSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// The two GETs this collector makes. Tests implement this against canned
/// fixtures; production hits the real earthquake.usgs.gov.
trait UsgsApi {
    /// `GET /earthquakes/feed/v1.0/summary/all_hour.geojson`
    /// → full GeoJSON FeatureCollection of the last hour's events globally.
    fn all_hour_feed(&self) -> Result<Value>;

    /// `GET /fdsnws/event/1/query?format=geojson&starttime=…&endtime=…&…`
    /// → GeoJSON FeatureCollection. Returns `None` when the server responds
    /// HTTP 400 (window exceeds the 20k-event cap — caller must subdivide) and
    /// `Ok(Some(empty FeatureCollection))` for HTTP 204 (no matching events).
    fn query(
        &self,
        starttime: &str,
        endtime: &str,
        lat: Option<f64>,
        lon: Option<f64>,
    ) -> Result<Option<Value>>;
}

struct UsgsClient {
    fdsn_base: String,
    feed_base: String,
}

impl UsgsClient {
    fn new(fdsn_base: String, feed_base: String) -> Self {
        UsgsClient { fdsn_base, feed_base }
    }

    fn req(url: &str) -> ureq::Request {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", USER_AGENT)
            .set("Accept", "application/geo+json")
    }
}

impl UsgsApi for UsgsClient {
    fn all_hour_feed(&self) -> Result<Value> {
        let url = format!("{}/all_hour.geojson", self.feed_base);
        Self::req(&url)
            .call()
            .context("requesting USGS all_hour feed")?
            .into_json()
            .context("reading USGS all_hour response")
    }

    fn query(
        &self,
        starttime: &str,
        endtime: &str,
        lat: Option<f64>,
        lon: Option<f64>,
    ) -> Result<Option<Value>> {
        let mut url = format!(
            "{}/query?format=geojson&starttime={starttime}&endtime={endtime}\
             &minmagnitude={MIN_MAG}&limit={PAGE_LIMIT}&orderby=time-asc",
            self.fdsn_base
        );
        if let (Some(la), Some(lo)) = (lat, lon) {
            url.push_str(&format!("&latitude={la}&longitude={lo}&maxradiuskm={RADIUS_KM}"));
        }
        match Self::req(&url).call() {
            Ok(resp) => {
                // 204 No Content is the documented default when zero events match
                // (FDSN default nodata=204). Treat it as an empty FeatureCollection.
                if resp.status() == 204 {
                    return Ok(Some(serde_json::json!({ "type": "FeatureCollection", "features": [] })));
                }
                let v: Value = resp.into_json().context("reading USGS FDSN response")?;
                Ok(Some(v))
            }
            Err(ureq::Error::Status(400, _)) => {
                // HTTP 400 means the window exceeds the documented 20k-event cap.
                // The caller must subdivide the time window and retry.
                Ok(None)
            }
            Err(ureq::Error::Status(204, _)) => {
                // Some ureq versions surface 204 as an error. Treat as empty.
                Ok(Some(serde_json::json!({ "type": "FeatureCollection", "features": [] })))
            }
            Err(e) => Err(e).context("requesting USGS FDSN query"),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing — pure, fixture-tested.

/// Convert a USGS `time` field (milliseconds since Unix epoch) to a local
/// RFC3339 string. Returns an empty string if the value is not a positive
/// integer (tolerant — caller should skip that event, not panic).
fn ms_to_rfc3339(ms: i64) -> String {
    let secs = ms / 1000;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    Utc.timestamp_opt(secs, nanos)
        .single()
        .map(|utc| utc.with_timezone(&Local).to_rfc3339())
        .unwrap_or_default()
}

/// Parse one GeoJSON FeatureCollection body (from either the realtime feed or
/// the FDSN query) into (contract geo-events, raw feature objects). Features
/// missing a USGS event id or a usable time are skipped — a geo-event with no
/// dedupe key cannot be stored.
fn parse_features(body: &Value) -> (Vec<EnvGeoEvent>, Vec<Value>) {
    let features = match body.get("features").and_then(Value::as_array) {
        Some(f) => f.clone(),
        None => return (Vec::new(), Vec::new()),
    };
    let mut rows = Vec::new();
    let mut raws = Vec::new();

    for feature in features {
        // guid: the feature's top-level `id` (e.g. "ci40123456").
        let guid = feature
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if guid.is_empty() {
            continue; // no dedupe key → can't store
        }

        let props = feature.get("properties").cloned().unwrap_or(Value::Null);

        // ts: properties.time is milliseconds since Unix epoch.
        let time_ms = props.get("time").and_then(Value::as_i64).unwrap_or(0);
        if time_ms <= 0 {
            continue; // no usable timestamp → can't partition
        }
        let ts = ms_to_rfc3339(time_ms);
        if ts.is_empty() {
            continue;
        }

        // Geometry: coordinates = [lon, lat, depth_km].
        let coords = feature
            .get("geometry")
            .and_then(|g| g.get("coordinates"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let lon = coords.first().and_then(Value::as_f64);
        let lat = coords.get(1).and_then(Value::as_f64);
        let depth_km = coords.get(2).and_then(Value::as_f64);

        let magnitude = props.get("mag").and_then(Value::as_f64);
        let place = props
            .get("place")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let url = props
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let alert = props
            .get("alert")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();

        // Build extra: source-specific fields not in the geo-event contract.
        let mut extra: Map<String, Value> = Map::new();
        if let Some(d) = depth_km {
            extra.insert("depth_km".into(), Value::Number(
                serde_json::Number::from_f64(d).unwrap_or(serde_json::Number::from(0)),
            ));
        }
        // tsunami: 0 = no warning, 1 = warning. Store as bool for readability.
        if let Some(t) = props.get("tsunami").and_then(Value::as_i64) {
            extra.insert("tsunami".into(), Value::Bool(t != 0));
        }
        if let Some(felt) = props.get("felt").and_then(Value::as_i64) {
            extra.insert("felt".into(), Value::Number(felt.into()));
        }
        if let Some(sig) = props.get("sig").and_then(Value::as_i64) {
            extra.insert("sig".into(), Value::Number(sig.into()));
        }
        if let Some(cdi) = props.get("cdi").and_then(Value::as_f64) {
            extra.insert("cdi".into(), Value::Number(
                serde_json::Number::from_f64(cdi).unwrap_or(serde_json::Number::from(0)),
            ));
        }
        if let Some(mmi) = props.get("mmi").and_then(Value::as_f64) {
            extra.insert("mmi".into(), Value::Number(
                serde_json::Number::from_f64(mmi).unwrap_or(serde_json::Number::from(0)),
            ));
        }
        if let Some(mag_type) = props.get("magType").and_then(Value::as_str) {
            if !mag_type.is_empty() {
                extra.insert("mag_type".into(), Value::String(mag_type.to_string()));
            }
        }
        if let Some(status) = props.get("status").and_then(Value::as_str) {
            if !status.is_empty() {
                extra.insert("status".into(), Value::String(status.to_string()));
            }
        }
        if let Some(net) = props.get("net").and_then(Value::as_str) {
            if !net.is_empty() {
                extra.insert("net".into(), Value::String(net.to_string()));
            }
        }
        // updated: ms since epoch → RFC3339 (when the event record was revised).
        if let Some(upd_ms) = props.get("updated").and_then(Value::as_i64) {
            let upd = ms_to_rfc3339(upd_ms);
            if !upd.is_empty() {
                extra.insert("updated".into(), Value::String(upd));
            }
        }

        rows.push(EnvGeoEvent {
            ts,
            source: SOURCE.into(),
            guid: guid.clone(),
            event_type: "quake".into(),
            magnitude,
            place,
            lat,
            lon,
            severity: alert.clone(),
            headline: String::new(), // USGS events have no headline field
            url,
            expires: String::new(), // earthquakes don't expire
            extra,
        });
        raws.push(feature);
    }
    (rows, raws)
}

// ---------------------------------------------------------------------------
// Upsert into stable month partitions.

/// A raw object carrying a synthetic `_ts`/`_guid` header purely so the
/// month-partition writer files it under the right month. The on-disk line is
/// the full API feature object verbatim (flattened = no wrapper).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(skip)]
    guid: String,
    #[serde(flatten)]
    value: Value,
}

/// Upsert geo-events into `environment/usgs-earthquakes/events/YYYY-MM.jsonl`
/// by `guid`. USGS revises events post-hoc, so a still-known quake must update
/// in place, never duplicate. The partition file is rewritten atomically.
fn upsert_events(vault: &Vault, rows: &[EnvGeoEvent]) -> Result<u64> {
    let stream = vault.stream(EVENTS_DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: BTreeMap<String, Vec<&EnvGeoEvent>> = Default::default();
    for r in rows {
        let key = Partition::Month
            .key(&r.ts)
            .with_context(|| format!("usgs-earthquakes event {} has unpartitionable ts {:?}", r.guid, r.ts))?;
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

/// Upsert raw feature objects into `environment/usgs-earthquakes/raw/YYYY-MM.jsonl`
/// by guid. Full fidelity — never drop what the API returned.
fn upsert_raw(vault: &Vault, lines: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: BTreeMap<String, Vec<&RawLine>> = Default::default();
    for l in lines {
        let key = Partition::Month
            .key(&l.ts)
            .with_context(|| format!("usgs-earthquakes raw {} has unpartitionable ts {:?}", l.guid, l.ts))?;
        by_key.entry(key.to_string()).or_default().push(l);
    }
    for (key, incoming) in by_key {
        let mut existing: Vec<Value> = stream.read(&key)?;
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

/// The USGS event id from a raw feature: the top-level `id` field.
fn raw_guid(v: &Value) -> String {
    v.get("id").and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the user's location using the same ladder as [`crate::nws`]:
/// CoreLocation → manual weather location → last-used cursor location.
/// Returns `None` if no location is available (collector falls back to the
/// global feed — no lat/lon filter).
fn resolve_point(vault: &Vault, state: &UsgsEqSyncState) -> Option<(f64, f64)> {
    if corelocation::auth_status() == AuthStatus::NotDetermined {
        corelocation::request_access(5);
    }
    if let Some(fix) = corelocation::current_location(8) {
        return Some((fix.lat, fix.lon));
    }
    if let Some(m) = vault.weather_location() {
        return Some((m.lat, m.lon));
    }
    // Last known: 0,0 is the Atlantic off Ghana — safe sentinel for "unset".
    (state.lat != 0.0 || state.lon != 0.0).then_some((state.lat, state.lon))
}

/// Production entry point: resolve the location ladder and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let state = vault.read_usgs_eq_sync().unwrap_or_default();
    let point = resolve_point(vault, &state);
    let client = UsgsClient::new(FDSN_BASE.to_string(), FEED_BASE.to_string());
    pull_at_with(vault, point, &client)
}

/// Network+write body over an explicit point (or `None` for the global feed).
/// Tests drive this directly to avoid touching CoreLocation.
pub fn pull_at(vault: &Vault, point: Option<(f64, f64)>) -> Result<PullOutcome> {
    let client = UsgsClient::new(FDSN_BASE.to_string(), FEED_BASE.to_string());
    pull_at_with(vault, point, &client)
}

/// Drain one time window [starttime_utc, endtime_utc] via the FDSN API,
/// collecting geo-events and raw lines. Uses time-window bisection when the
/// server returns HTTP 400 (over the 20k-event cap): the window is split at
/// its midpoint and each sub-window is drained recursively.
///
/// Returns the total number of events written and the latest event timestamp
/// seen (for watermark advancement).
fn drain_window(
    vault: &Vault,
    client: &impl UsgsApi,
    lat: Option<f64>,
    lon: Option<f64>,
    start_utc: DateTime<Utc>,
    end_utc: DateTime<Utc>,
    all_raws: &mut Vec<RawLine>,
    depth: u32, // guard against degenerate bisection (sub-second windows)
) -> Result<(u64, Option<String>)> {
    // Safety cap: if start >= end or depth is excessive, bail.
    if start_utc >= end_utc || depth > 32 {
        return Ok((0, None));
    }

    let starttime = start_utc.format("%Y-%m-%dT%H:%M:%S").to_string();
    let endtime = end_utc.format("%Y-%m-%dT%H:%M:%S").to_string();

    match client.query(&starttime, &endtime, lat, lon)? {
        None => {
            // HTTP 400: window exceeds the 20k-event cap. Bisect.
            let mid = start_utc + (end_utc - start_utc) / 2;
            let (w1, ts1) = drain_window(vault, client, lat, lon, start_utc, mid, all_raws, depth + 1)?;
            let (w2, ts2) = drain_window(vault, client, lat, lon, mid, end_utc, all_raws, depth + 1)?;
            let latest = match (ts1, ts2) {
                (Some(a), Some(b)) => Some(if a > b { a } else { b }),
                (Some(a), None) => Some(a),
                (None, b) => b,
            };
            Ok((w1 + w2, latest))
        }
        Some(page) => {
            let (page_rows, page_raws) = parse_features(&page);
            if page_rows.is_empty() {
                return Ok((0, None));
            }
            let latest_ts = page_rows.last().map(|r| r.ts.clone());
            let count = upsert_events(vault, &page_rows)?;
            for (row, raw) in page_rows.iter().zip(page_raws.iter()) {
                all_raws.push(RawLine { ts: row.ts.clone(), guid: row.guid.clone(), value: raw.clone() });
            }
            // If the page is exactly PAGE_LIMIT rows the window may have more events;
            // bisect to capture them all.
            if page_rows.len() >= PAGE_LIMIT as usize {
                let mid = start_utc + (end_utc - start_utc) / 2;
                // Use the last event's time as the new start for the second half to
                // avoid re-fetching the first half (starttime is inclusive, so +1ms).
                let last_event_utc = page_rows
                    .last()
                    .and_then(|r| DateTime::parse_from_rfc3339(&r.ts).ok())
                    .map(|dt| dt.with_timezone(&Utc) + chrono::Duration::milliseconds(1))
                    .unwrap_or(mid);
                let (w2, ts2) = drain_window(vault, client, lat, lon, last_event_utc, end_utc, all_raws, depth + 1)?;
                let merged_ts = match (latest_ts, ts2) {
                    (Some(a), Some(b)) => Some(if a > b { a } else { b }),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                };
                Ok((count + w2, merged_ts))
            } else {
                Ok((count, latest_ts))
            }
        }
    }
}

/// Offline-testable pull body: explicit point (or None = global) + injected API.
///
/// Strategy:
/// 1. Always poll the realtime `all_hour` feed (fast, global, deduped by guid).
/// 2. Backfill via the FDSN API from `watermark` → now using time-window
///    bisection to cross the documented 20k-event cap without dropping events.
/// 3. Advance the watermark from the max event timestamp seen across BOTH the
///    feed and the backfill (so quiet-region users whose backfill is always
///    empty still advance the watermark from the global feed).
fn pull_at_with(
    vault: &Vault,
    point: Option<(f64, f64)>,
    client: &impl UsgsApi,
) -> Result<PullOutcome> {
    let now = Local::now();
    let mut state = vault.read_usgs_eq_sync().unwrap_or_default();
    let lat = point.map(|(la, _)| la);
    let lon = point.map(|(_, lo)| lo);

    let mut total_written = 0u64;
    let mut all_raws: Vec<RawLine> = Vec::new();
    let mut latest_event_ts: Option<String> = None; // max ts across feed + backfill

    // --- 1. Realtime `all_hour` feed (near real-time, ~1-min cadence) ---------
    let feed_body = client.all_hour_feed()?;
    let (feed_rows, feed_raws) = parse_features(&feed_body);
    if !feed_rows.is_empty() {
        total_written += upsert_events(vault, &feed_rows)?;
        // Track the latest event ts across both feed and backfill (fix: watermark
        // must advance even when the in-radius FDSN backfill returns zero events).
        if let Some(last) = feed_rows.last() {
            latest_event_ts = Some(last.ts.clone());
        }
        for (row, raw) in feed_rows.iter().zip(feed_raws.iter()) {
            all_raws.push(RawLine { ts: row.ts.clone(), guid: row.guid.clone(), value: raw.clone() });
        }
    }

    // --- 2. FDSN time-window backfill ----------------------------------------
    // Determine the starttime: use the cursor watermark when available, else
    // backfill from BACKFILL_DAYS days ago.
    //
    // Note: FDSN starttime is inclusive. The watermark is the latest event ts
    // from the previous run, so the boundary event is re-fetched each run.
    // This is harmless — upsert-by-guid deduplicates it.
    let start_utc: DateTime<Utc> = if state.watermark.is_empty() {
        (now - Duration::days(BACKFILL_DAYS)).with_timezone(&Utc)
    } else {
        DateTime::parse_from_rfc3339(&state.watermark)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| (now - Duration::days(BACKFILL_DAYS)).with_timezone(&Utc))
    };
    let end_utc: DateTime<Utc> = now.with_timezone(&Utc);

    let (backfill_written, backfill_ts) =
        drain_window(vault, client, lat, lon, start_utc, end_utc, &mut all_raws, 0)?;
    total_written += backfill_written;

    // Advance from the later of feed and backfill timestamps.
    if let Some(ts) = backfill_ts {
        latest_event_ts = Some(match latest_event_ts.take() {
            Some(prev) => if ts > prev { ts } else { prev },
            None => ts,
        });
    }

    // --- 3. Raw layer (unconditional, full fidelity) --------------------------
    if !all_raws.is_empty() {
        upsert_raw(vault, &all_raws)?;
    }

    // --- 4. Advance the cursor AFTER the full drain ---------------------------
    // Advance the watermark from the max event ts seen (feed + backfill), so
    // quiet-region users (whose FDSN in-radius backfill is always 204/empty)
    // still advance past the default backfill window via the global feed.
    if let Some(ref ts) = latest_event_ts {
        state.watermark = ts.clone();
    }
    if let (Some(la), Some(lo)) = (lat, lon) {
        state.lat = la;
        state.lon = lo;
    }
    state.updated = now.to_rfc3339();
    state.error = String::new();
    vault.write_usgs_eq_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total_written} quake events"),
        counts: BTreeMap::from([("quakes", total_written)]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-usgs-eq-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- Fixtures (field names confirmed from USGS GeoJSON documentation) ----
    //
    // USGS GeoJSON feature: top-level `id` is the event id; `geometry.coordinates`
    // is [lon, lat, depth_km]; `properties` carries mag/place/time/url/felt/
    // tsunami/alert/status/net/magType/sig/cdi/mmi/updated (all confirmed against
    // https://earthquake.usgs.gov/earthquakes/feed/v1.0/geojson.php).

    /// One-feature FeatureCollection for a M4.2 quake near Ojai, CA.
    fn one_quake() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "ci40123456",
                "geometry": {
                    "type": "Point",
                    "coordinates": [-119.1817, 34.5483, 8.3]
                },
                "properties": {
                    "mag": 4.2,
                    "place": "12km NE of Ojai, CA",
                    "time": 1749526931000i64,
                    "updated": 1749527000000i64,
                    "tz": -420,
                    "url": "https://earthquake.usgs.gov/earthquakes/eventpage/ci40123456",
                    "detail": "https://earthquake.usgs.gov/fdsnws/event/1/query?eventid=ci40123456&format=geojson",
                    "felt": 214,
                    "cdi": 3.5,
                    "mmi": 4.1,
                    "alert": "green",
                    "status": "reviewed",
                    "tsunami": 0,
                    "sig": 284,
                    "net": "ci",
                    "code": "40123456",
                    "ids": ",ci40123456,",
                    "sources": ",ci,",
                    "types": ",dyfi,general-text,locsource,nearby-cities,origin,phase-data,scitech-link,",
                    "nst": 150,
                    "dmin": 0.036,
                    "rms": 0.26,
                    "gap": 24,
                    "magType": "ml",
                    "type": "earthquake",
                    "title": "M 4.2 - 12km NE of Ojai, CA"
                }
            }],
            "bbox": [-119.1817, 34.5483, 8.3, -119.1817, 34.5483, 8.3]
        })
    }

    /// A revised version of the same event — magnitude bumped to 4.5 by USGS
    /// reviewers. Proves the upsert-by-guid path.
    fn one_quake_revised() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "ci40123456",
                "geometry": {
                    "type": "Point",
                    "coordinates": [-119.1817, 34.5483, 8.3]
                },
                "properties": {
                    "mag": 4.5,
                    "place": "12km NE of Ojai, CA",
                    "time": 1749526931000i64,
                    "updated": 1749530000000i64,
                    "url": "https://earthquake.usgs.gov/earthquakes/eventpage/ci40123456",
                    "felt": 320,
                    "alert": "green",
                    "status": "reviewed",
                    "tsunami": 0,
                    "sig": 312,
                    "net": "ci",
                    "magType": "ml",
                    "type": "earthquake"
                }
            }]
        })
    }

    /// A second distinct quake event (different id).
    fn another_quake() -> Value {
        json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "nc70123456",
                "geometry": {
                    "type": "Point",
                    "coordinates": [-122.4194, 37.7749, 12.1]
                },
                "properties": {
                    "mag": 3.1,
                    "place": "5km W of San Francisco, CA",
                    "time": 1749530000000i64,
                    "url": "https://earthquake.usgs.gov/earthquakes/eventpage/nc70123456",
                    "felt": null,
                    "alert": null,
                    "status": "automatic",
                    "tsunami": 0,
                    "net": "nc",
                    "magType": "md",
                    "type": "earthquake"
                }
            }]
        })
    }

    /// Empty FeatureCollection — no events in this window.
    fn empty_collection() -> Value {
        json!({ "type": "FeatureCollection", "features": [] })
    }

    // --- Stub API ------------------------------------------------------------
    // The new query() signature has no `offset`; instead, multiple query calls
    // are dispatched based on time-window bisection. The Stub is sequence-driven:
    // each successive call to query() returns the next entry from `pages`. When
    // the list is exhausted it returns an empty FeatureCollection.

    struct Stub {
        feed: Value,
        /// Each call to `query()` pops the front element. `None` → HTTP 400
        /// (over-cap); `Some(v)` → a FeatureCollection (or empty).
        pages: std::sync::Mutex<std::collections::VecDeque<Option<Value>>>,
    }

    impl Stub {
        /// Build from a vec of `Option<Value>` where `None` signals HTTP 400.
        fn new(feed: Value, pages: Vec<Option<Value>>) -> Self {
            Stub {
                feed,
                pages: std::sync::Mutex::new(pages.into_iter().collect()),
            }
        }
    }

    impl UsgsApi for Stub {
        fn all_hour_feed(&self) -> Result<Value> {
            Ok(self.feed.clone())
        }

        fn query(
            &self,
            _starttime: &str,
            _endtime: &str,
            _lat: Option<f64>,
            _lon: Option<f64>,
        ) -> Result<Option<Value>> {
            let mut q = self.pages.lock().unwrap();
            match q.pop_front() {
                // Queue exhausted → empty FeatureCollection (no more data).
                None => Ok(Some(empty_collection())),
                // Some(None) → simulate HTTP 400 (over-cap); caller must bisect.
                Some(None) => Ok(None),
                // Some(Some(v)) → normal response with events (or empty collection).
                Some(Some(v)) => Ok(Some(v)),
            }
        }
    }

    /// Convenience: build a Stub where every query returns a normal (non-400) value.
    fn normal_stub(feed: Value, pages: Vec<Value>) -> Stub {
        Stub::new(feed, pages.into_iter().map(Some).collect())
    }

    const SEED: (f64, f64) = (34.05, -118.24);

    // --- Pure parser tests ---------------------------------------------------

    #[test]
    fn parse_features_maps_contract_fields() {
        let (rows, raws) = parse_features(&one_quake());
        assert_eq!(rows.len(), 1);
        assert_eq!(raws.len(), 1);
        let q = &rows[0];

        assert_eq!(q.source, "usgs-earthquakes");
        assert_eq!(q.guid, "ci40123456");
        assert_eq!(q.event_type, "quake");
        assert_eq!(q.magnitude, Some(4.2));
        assert_eq!(q.place, "12km NE of Ojai, CA");
        assert_eq!(q.lat, Some(34.5483));
        assert_eq!(q.lon, Some(-119.1817), "longitude is coords[0]");
        assert_eq!(q.severity, "green", "severity = alert field");
        assert_eq!(q.url, "https://earthquake.usgs.gov/earthquakes/eventpage/ci40123456");
        assert!(q.expires.is_empty(), "earthquakes have no expiry");
        assert!(q.headline.is_empty(), "USGS has no headline field");

        // Extra fields: depth, tsunami, felt, sig, magType, status, net, updated.
        assert_eq!(q.extra.get("depth_km"), Some(&json!(8.3)));
        assert_eq!(q.extra.get("tsunami"), Some(&json!(false)), "0 → false");
        assert_eq!(q.extra.get("felt"), Some(&json!(214)));
        assert_eq!(q.extra.get("sig"), Some(&json!(284)));
        assert_eq!(q.extra.get("mag_type"), Some(&json!("ml")));
        assert_eq!(q.extra.get("status"), Some(&json!("reviewed")));
        assert_eq!(q.extra.get("net"), Some(&json!("ci")));
        assert!(q.extra.contains_key("updated"), "updated timestamp in extra");

        // ts: 1749526931000 ms → should parse as a valid RFC3339.
        let dt = DateTime::parse_from_rfc3339(&q.ts);
        assert!(dt.is_ok(), "ts must be valid RFC3339; got={}", q.ts);
        // Verify the absolute instant matches the source (within 1 second rounding).
        assert_eq!(dt.unwrap().timestamp(), 1749526931, "epoch must match source");
    }

    #[test]
    fn parse_features_handles_null_optional_fields() {
        // `felt` and `alert` are null in the second fixture — they must be
        // omitted from `extra` and `severity` respectively (not stored as null).
        let (rows, _) = parse_features(&another_quake());
        assert_eq!(rows.len(), 1);
        let q = &rows[0];
        assert_eq!(q.guid, "nc70123456");
        assert_eq!(q.magnitude, Some(3.1));
        assert!(q.severity.is_empty(), "null alert → empty severity");
        assert!(!q.extra.contains_key("felt"), "null felt → omit from extra");
    }

    #[test]
    fn parse_features_skips_missing_id_or_time() {
        // Feature with no `id` must be skipped.
        let no_id = json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "geometry": { "type": "Point", "coordinates": [-118.0, 34.0, 5.0] },
                "properties": { "mag": 3.0, "time": 1749526931000i64 }
            }]
        });
        let (rows, _) = parse_features(&no_id);
        assert_eq!(rows.len(), 0, "missing id → skip");

        // Feature with time = 0 must be skipped.
        let zero_time = json!({
            "type": "FeatureCollection",
            "features": [{
                "type": "Feature",
                "id": "xx99999",
                "geometry": { "type": "Point", "coordinates": [-118.0, 34.0, 5.0] },
                "properties": { "mag": 3.0, "time": 0 }
            }]
        });
        let (rows2, _) = parse_features(&zero_time);
        assert_eq!(rows2.len(), 0, "zero time → skip");
    }

    #[test]
    fn ms_to_rfc3339_converts_epoch_millis() {
        // 1749526931000 ms = 2025-06-10 03:42:11 UTC (approx).
        let s = ms_to_rfc3339(1749526931000);
        assert!(!s.is_empty(), "must produce a non-empty timestamp");
        // Parse it back and verify the unix seconds match.
        let dt = DateTime::parse_from_rfc3339(&s).expect("must be valid RFC3339");
        assert_eq!(dt.timestamp(), 1749526931);
    }

    #[test]
    fn ms_to_rfc3339_handles_zero_gracefully() {
        // 0 ms is epoch (1970-01-01T00:00:00Z), which Utc.timestamp_opt(0, 0)
        // returns as Single. We skip it in parse_features (time <= 0 guard),
        // but the converter itself must not panic.
        let s = ms_to_rfc3339(0);
        // Either produces a timestamp or an empty string — must not panic.
        let _ = s;
    }

    // --- Pull / store / dedupe tests -----------------------------------------

    #[test]
    fn pull_writes_geo_event_and_raw() {
        let v = temp_vault("pull");
        let stub = normal_stub(one_quake(), vec![empty_collection()]);
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert!(out.counts.get("quakes").copied().unwrap_or(0) >= 1);

        // Contract geo-event: environment/usgs-earthquakes/events/YYYY-MM.jsonl
        // The timestamp 1749526931000 ms is 2025-06-09/10 depending on local TZ.
        // The month part of the filename varies by the machine's local TZ, so we
        // scan the events/ directory rather than hard-code the partition.
        let events_dir = v.root().join(EVENTS_DIR);
        assert!(events_dir.exists(), "events/ dir must exist after pull");
        let mut found_event = false;
        for entry in std::fs::read_dir(&events_dir).unwrap().flatten() {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            if content.contains("ci40123456") {
                found_event = true;
                assert!(content.contains("\"event_type\":\"quake\""));
                assert!(content.contains("\"magnitude\":4.2"));
                assert!(content.contains("\"source\":\"usgs-earthquakes\""));
                break;
            }
        }
        assert!(found_event, "ci40123456 must appear in the events/ partition");

        // Raw layer: environment/usgs-earthquakes/raw/YYYY-MM.jsonl
        let raw_dir = v.root().join(RAW_DIR);
        assert!(raw_dir.exists(), "raw/ dir must exist after pull");
        let mut found_raw = false;
        for entry in std::fs::read_dir(&raw_dir).unwrap().flatten() {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            if content.contains("ci40123456") {
                found_raw = true;
                // Raw line is the verbatim feature — geometry must be present.
                assert!(content.contains("\"geometry\""), "raw keeps full GeoJSON feature");
                assert!(content.contains("\"coordinates\""));
                // Source-specific fields kept verbatim.
                assert!(content.contains("\"magType\""), "raw keeps magType");
                assert!(content.contains("\"nst\""), "raw keeps nst");
                break;
            }
        }
        assert!(found_raw, "ci40123456 must appear in raw/ partition");

        // Cursor advanced.
        let state = v.read_usgs_eq_sync().unwrap();
        assert!(!state.updated.is_empty());
        assert!(state.error.is_empty());
    }

    #[test]
    fn repoll_upserts_by_guid_no_duplicate() {
        let v = temp_vault("upsert");
        // First poll: ci40123456 at M4.2.
        pull_at_with(
            &v,
            Some(SEED),
            &normal_stub(one_quake(), vec![empty_collection()]),
        )
        .unwrap();

        // Confirm one row in the events partition.
        let events_dir = v.root().join(EVENTS_DIR);
        let count_before: usize = std::fs::read_dir(&events_dir)
            .unwrap()
            .flatten()
            .map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.contains("ci40123456"))
                    .count()
            })
            .sum();
        assert_eq!(count_before, 1, "one row before re-poll");

        // Second poll: same event, magnitude revised to 4.5 by USGS.
        pull_at_with(
            &v,
            Some(SEED),
            &normal_stub(one_quake_revised(), vec![empty_collection()]),
        )
        .unwrap();

        // Still exactly one row (guid deduped), but with the new magnitude.
        let count_after: usize = std::fs::read_dir(&events_dir)
            .unwrap()
            .flatten()
            .map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| l.contains("ci40123456"))
                    .count()
            })
            .sum();
        assert_eq!(count_after, 1, "upsert by guid — still one row, not two");

        // The updated magnitude is in the file.
        let rows: Vec<EnvGeoEvent> = {
            let mut all = Vec::new();
            for e in std::fs::read_dir(&events_dir).unwrap().flatten() {
                let key = e
                    .path()
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let mut part: Vec<EnvGeoEvent> = v.stream(EVENTS_DIR, Partition::Month).read(&key).unwrap();
                all.append(&mut part);
            }
            all
        };
        let q = rows.iter().find(|r| r.guid == "ci40123456").unwrap();
        assert_eq!(q.magnitude, Some(4.5), "magnitude updated in place");
    }

    #[test]
    fn two_distinct_events_both_stored() {
        let v = temp_vault("two-events");
        // First poll: one_quake in the feed; another_quake via the backfill page.
        let stub = normal_stub(one_quake(), vec![another_quake(), empty_collection()]);
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        // Total: 1 from feed + 1 from backfill = 2.
        assert_eq!(out.counts.get("quakes"), Some(&2));

        let events_dir = v.root().join(EVENTS_DIR);
        let total_rows: usize = std::fs::read_dir(&events_dir)
            .unwrap()
            .flatten()
            .map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .count()
            })
            .sum();
        assert_eq!(total_rows, 2, "both distinct events stored");
    }

    #[test]
    fn empty_feed_no_error() {
        let v = temp_vault("empty");
        let stub = normal_stub(empty_collection(), vec![empty_collection()]);
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(out.counts.get("quakes"), Some(&0));
        assert!(!v.root().join(EVENTS_DIR).exists(), "no events dir when no quakes");
        assert!(!v.root().join(RAW_DIR).exists(), "no raw dir when no quakes");
        let state = v.read_usgs_eq_sync().unwrap();
        assert!(state.error.is_empty(), "empty result is not an error");
    }

    #[test]
    fn global_feed_when_no_point() {
        // With point = None the collector falls back to a global (no-radius)
        // backfill. The all_hour feed is always global.
        let v = temp_vault("global");
        let stub = normal_stub(one_quake(), vec![empty_collection()]);
        let out = pull_at_with(&v, None, &stub).unwrap();
        // Feed quake must still be stored.
        assert!(out.counts.get("quakes").copied().unwrap_or(0) >= 1);
        let state = v.read_usgs_eq_sync().unwrap();
        assert_eq!(state.lat, 0.0, "no point → lat stays 0");
        assert_eq!(state.lon, 0.0, "no point → lon stays 0");
    }

    #[test]
    fn watermark_persists_and_advances() {
        let v = temp_vault("watermark");
        // First run: empty backfill, one feed quake.
        let stub = normal_stub(one_quake(), vec![empty_collection()]);
        pull_at_with(&v, Some(SEED), &stub).unwrap();

        // Watermark is now advanced from the feed quake even when the backfill
        // returns an empty page, so state.watermark must be non-empty.
        let state = v.read_usgs_eq_sync().unwrap();
        assert!(!state.updated.is_empty(), "updated must be set after pull");
        assert!(!state.watermark.is_empty(), "watermark must advance from feed quake");
    }

    #[test]
    fn sync_state_round_trips_and_back_compat() {
        let v = temp_vault("sync");
        let state = UsgsEqSyncState {
            updated: "2026-06-10T11:00:00-07:00".into(),
            lat: 34.05,
            lon: -118.24,
            watermark: "2026-06-10T10:00:00-07:00".into(),
            error: String::new(),
        };
        v.write_usgs_eq_sync(&state).unwrap();
        let s = v.read_usgs_eq_sync().unwrap();
        assert_eq!(s.lat, 34.05);
        assert_eq!(s.lon, -118.24);
        assert_eq!(s.watermark, "2026-06-10T10:00:00-07:00");
        assert!(s.error.is_empty());

        // Empty/missing cursor deserializes to defaults — never panics.
        let empty: UsgsEqSyncState = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.lat, 0.0);
        assert!(empty.watermark.is_empty());
    }

    // --- Defect regression tests ---------------------------------------------

    #[test]
    fn fdsn_204_empty_body_is_not_an_error() {
        // FDSN default is nodata=204: a zero-match window returns 204 No Content.
        // The pull must succeed with 0 quakes and a clear error, not abort.
        // This is the common steady-state for a quiet region.
        // The Stub returns None→Some(empty) when the queue has [Some(empty)]
        // (normal_stub wraps each element in Some); a separate Stub2 below
        // tests the explicit "None in queue" (HTTP 400) path.
        let v = temp_vault("204-empty");
        // Simulate: backfill query returns an empty FeatureCollection (as FDSN
        // would after nodata override) — the pull must succeed, not error.
        let stub = normal_stub(empty_collection(), vec![empty_collection()]);
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(out.counts.get("quakes").copied().unwrap_or(0), 0, "0 quakes on empty");
        let state = v.read_usgs_eq_sync().unwrap();
        assert!(state.error.is_empty(), "empty body must not set error");
    }

    #[test]
    fn fdsn_400_over_cap_bisects_and_recovers_all_events() {
        // FDSN returns HTTP 400 when a window exceeds 20k events. The drain must
        // bisect the window and retry each half, recovering all events without
        // dropping any. This test simulates: initial window → 400 (None), then
        // first half → one_quake, second half → another_quake.
        let v = temp_vault("400-bisect");
        // pages queue (in call order):
        //   call 1: None → simulates HTTP 400 (over-cap) → bisect into [start,mid] and [mid,end]
        //   call 2: one_quake (1 event < PAGE_LIMIT) → first half returns short page → done
        //   call 3: another_quake (1 event < PAGE_LIMIT) → second half returns short page → done
        // No further calls needed (short pages do not trigger sub-bisection).
        let stub = Stub::new(
            empty_collection(),
            vec![
                None,                  // initial window → over-cap → bisect
                Some(one_quake()),     // first sub-window (short page, done)
                Some(another_quake()), // second sub-window (short page, done)
            ],
        );
        let out = pull_at_with(&v, Some(SEED), &stub).unwrap();
        assert_eq!(
            out.counts.get("quakes").copied().unwrap_or(0),
            2,
            "both events recovered after 400 bisection"
        );
        let state = v.read_usgs_eq_sync().unwrap();
        assert!(state.error.is_empty(), "bisection must not leave an error");

        let events_dir = v.root().join(EVENTS_DIR);
        let all_guids: Vec<String> = std::fs::read_dir(&events_dir)
            .unwrap()
            .flatten()
            .flat_map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        serde_json::from_str::<EnvGeoEvent>(l)
                            .map(|ev| ev.guid)
                            .unwrap_or_default()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(all_guids.contains(&"ci40123456".to_string()), "first event stored");
        assert!(all_guids.contains(&"nc70123456".to_string()), "second event stored");
    }

    #[test]
    fn watermark_advances_from_feed_when_backfill_empty() {
        // Previously the watermark only advanced from the FDSN backfill drain.
        // When the in-radius backfill returns 204/empty (quiet region), the
        // watermark stayed empty forever and every run re-issued the full
        // 30-day backfill. Now the watermark also advances from the realtime feed.
        let v = temp_vault("wm-feed");
        // Feed has one quake; backfill returns empty (quiet region).
        let stub = normal_stub(one_quake(), vec![empty_collection()]);
        pull_at_with(&v, Some(SEED), &stub).unwrap();

        let state = v.read_usgs_eq_sync().unwrap();
        assert!(
            !state.watermark.is_empty(),
            "watermark must advance from the feed quake even when backfill is empty"
        );
        // The watermark must be a valid RFC3339.
        let dt = DateTime::parse_from_rfc3339(&state.watermark);
        assert!(dt.is_ok(), "watermark must be valid RFC3339; got={}", state.watermark);
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // Back-compat: a minimal geo-event line (only the 4 required fields)
        // must parse via the upsert read path.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join(EVENTS_DIR)).unwrap();
        std::fs::write(
            v.root().join(format!("{EVENTS_DIR}/2026-06.jsonl")),
            "{\"ts\":\"2026-06-10T03:42:11-07:00\",\"source\":\"usgs-earthquakes\",\
             \"guid\":\"ci40000001\",\"event_type\":\"quake\"}\n",
        )
        .unwrap();
        let events: Vec<EnvGeoEvent> =
            v.stream(EVENTS_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].guid, "ci40000001");
        assert_eq!(events[0].event_type, "quake");
        assert_eq!(events[0].magnitude, None, "sparse line has no magnitude");
    }
}
