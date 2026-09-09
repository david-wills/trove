//! NOAA Space Weather Prediction Center — Kp index, geomagnetic storm alerts.
//! Brief: docs/integrations/noaa-swpc.md.
//!
//! **Periodic (3 h)**, fully keyless US government JSON feeds at
//! services.swpc.noaa.gov/json/.  No login, no TCC, standalone-clean (plain
//! HTTPS via ureq+rustls).
//!
//! # What it writes
//!
//! - **Kp index readings → [`crate::environment::EnvReading`]** — one row per
//!   1-minute sample (`metric = "kp"`, `value = estimated_kp`, `unit = "index"`).
//!   Written to `environment/noaa-swpc/YYYY-MM.jsonl` (month of the UTC `time_tag`
//!   converted to local time).
//! - **Geomagnetic alerts → [`crate::environment::EnvGeoEvent`]** — one row per
//!   alert message (`event_type = "alert"`).  Deduped by the serial number parsed
//!   from the message body.  Written to
//!   `environment/noaa-swpc/events/YYYY-MM.jsonl`.
//! - **Raw layer** (`environment/noaa-swpc/raw/YYYY-MM.jsonl`) — the API objects
//!   verbatim, full fidelity.  OVATION aurora grid snapshots are stored here as a
//!   sidecar (they are bulky raster arrays, not suited for per-row event storage).
//!
//! # Cursor / watermark
//!
//! A non-secret rebuildable JSON cursor (`.trove/noaa-swpc-sync.json`) records
//! the `time_tag` of the latest Kp sample ingested.  On each pull, samples
//! *after* the cursor are stored; the cursor advances only after the full write
//! succeeds so a crash cannot lose data.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::environment::{EnvGeoEvent, EnvReading};
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const DIR: &str = "environment/noaa-swpc";
const EVENTS_DIR: &str = "environment/noaa-swpc/events";
const RAW_DIR: &str = "environment/noaa-swpc/raw";
const SYNC_FILE: &str = ".trove/noaa-swpc-sync.json";
const SOURCE: &str = "noaa-swpc";

const KP_URL: &str = "https://services.swpc.noaa.gov/json/planetary_k_index_1m.json";
const ALERTS_URL: &str = "https://services.swpc.noaa.gov/products/alerts.json";
const OVATION_URL: &str = "https://services.swpc.noaa.gov/json/ovation_aurora_latest.json";

/// Poll every 3 hours — matches the Kp update cadence at SWPC.
pub const SWPC_SYNC_SECS: u64 = 10_800;

const USER_AGENT: &str = "Trove (https://trove.app)";
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

// ---------------------------------------------------------------------------
// DEF

fn def_last_data(vault: &Vault) -> Option<String> {
    vault
        .read_swpc_sync()
        .and_then(|s| if s.last_kp_ts.is_empty() { None } else { Some(s.last_kp_ts) })
        .or_else(|| crate::registry::newest_stem(&vault.root().join(DIR)))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let kp = out.counts.get("kp_readings").copied().unwrap_or(0);
            let al = out.counts.get("alerts").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(
                kp > 0 || al > 0,
                || format!("noaa-swpc synced — {kp} Kp readings, {al} alerts"),
            ))
        }
        Err(e) => Ok(CollectOutcome::note(format!("noaa-swpc sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let kp = out.counts.get("kp_readings").copied().unwrap_or(0);
    let al = out.counts.get("alerts").copied().unwrap_or(0);
    let headline = if kp == 0 && al == 0 {
        "NOAA Space Weather: nothing new".to_string()
    } else {
        format!("NOAA Space Weather: {kp} Kp readings, {al} alerts")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].  Keyless — no
/// connection. Polls every 3 h matching the Kp update cadence.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "noaa-swpc",
        name: "NOAA Space Weather",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Captures near-real-time geomagnetic and solar data from NOAA's Space \
                      Weather Prediction Center: the planetary Kp index (aurora visibility — \
                      Kp ≥ 5 means mid-latitude aurora), geomagnetic storm alerts, and OVATION \
                      aurora footprint snapshots. Fully keyless US government feed.",
        domain: "environment",
        vault_path: "environment/noaa-swpc/",
        toggleable: true,
        setup: &[
            "No account or key required — uses the keyless public NOAA SWPC JSON feeds.",
        ],
        caveats: "Kp index updates every 3 hours; aurora forecasts are probabilistic. \
                  OVATION grid snapshots are stored in the raw layer only.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SWPC_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Cursor

/// Non-secret, rebuildable collector state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwpcSyncState {
    /// The `time_tag` of the last Kp sample ingested (UTC naive,
    /// `YYYY-MM-DDTHH:MM:SS`). Advance-after-write only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_kp_ts: String,
    /// RFC3339 local time of the last successful pull.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated: String,
}

impl Vault {
    pub fn read_swpc_sync(&self) -> Option<SwpcSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_swpc_sync(&self, state: &SwpcSyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Time helpers

/// Parse a NOAA SWPC `time_tag` (UTC naive, `YYYY-MM-DDTHH:MM:SS`) into a
/// local-offset RFC3339 string.  Unparseable values are passed through.
fn utc_naive_to_local(tag: &str) -> String {
    NaiveDateTime::parse_from_str(tag, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|ndt| {
            let utc: DateTime<Utc> = Utc.from_utc_datetime(&ndt);
            utc.with_timezone(&Local).to_rfc3339()
        })
        .unwrap_or_else(|| tag.to_string())
}

/// Parse a NOAA alerts `issue_datetime` (UTC, space-separated, possibly with
/// fractional seconds: `2026-06-15 12:35:56.833`) into local RFC3339.
fn alert_dt_to_local(s: &str) -> String {
    // Try with fractional seconds first, then without.
    let ndt = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S"))
        .ok();
    match ndt {
        Some(ndt) => {
            let utc: DateTime<Utc> = Utc.from_utc_datetime(&ndt);
            utc.with_timezone(&Local).to_rfc3339()
        }
        None => s.to_string(),
    }
}

// ---------------------------------------------------------------------------
// HTTP layer (injectable for tests)

trait SwpcApi {
    /// `GET planetary_k_index_1m.json` → JSON array of Kp samples.
    fn kp_index(&self) -> Result<Value>;
    /// `GET products/alerts.json` → JSON array of alert objects.
    fn alerts(&self) -> Result<Value>;
    /// `GET ovation_aurora_latest.json` → the OVATION aurora grid snapshot.
    fn ovation(&self) -> Result<Value>;
}

struct SwpcClient;

impl SwpcClient {
    fn get(url: &str) -> Result<Value> {
        ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("User-Agent", USER_AGENT)
            .call()
            .with_context(|| format!("GET {url}"))?
            .into_json()
            .with_context(|| format!("reading JSON from {url}"))
    }
}

impl SwpcApi for SwpcClient {
    fn kp_index(&self) -> Result<Value> {
        Self::get(KP_URL)
    }
    fn alerts(&self) -> Result<Value> {
        Self::get(ALERTS_URL)
    }
    fn ovation(&self) -> Result<Value> {
        Self::get(OVATION_URL)
    }
}

// ---------------------------------------------------------------------------
// Kp parsing

/// One entry from `planetary_k_index_1m.json`.
#[derive(Debug, Deserialize)]
struct KpEntry {
    time_tag: String,
    #[allow(dead_code)]
    kp_index: i64,
    estimated_kp: f64,
    kp: String,
}

/// Parse the Kp JSON array into contract readings and raw values.
/// Only samples with `time_tag > watermark` are returned (new-only window).
fn parse_kp(body: &Value, watermark: &str) -> (Vec<EnvReading>, Vec<Value>) {
    let arr = match body.as_array() {
        Some(a) => a,
        None => return (Vec::new(), Vec::new()),
    };
    let mut readings = Vec::new();
    let mut raws = Vec::new();
    for entry_v in arr {
        let entry: KpEntry = match serde_json::from_value(entry_v.clone()) {
            Ok(e) => e,
            Err(_) => continue,
        };
        // Watermark: only new samples (lexicographic, UTC naive tags sort correctly).
        if entry.time_tag.as_str() <= watermark {
            continue;
        }
        let ts = utc_naive_to_local(&entry.time_tag);
        let guid = format!("noaa-swpc:kp:{}", entry.time_tag);
        let mut extra = Map::new();
        extra.insert("kp_class".into(), Value::String(entry.kp.clone()));
        readings.push(EnvReading {
            ts,
            source: SOURCE.into(),
            metric: "kp".into(),
            value: entry.estimated_kp,
            unit: "index".into(),
            place: String::new(),
            lat: None,
            lon: None,
            station: "planetary".into(),
            guid: Some(guid),
            extra,
        });
        raws.push(entry_v.clone());
    }
    (readings, raws)
}

// ---------------------------------------------------------------------------
// Alert parsing

/// Parse the SWPC serial number from the message body:
///   `Serial Number: 3701`
/// Returns a guid `noaa-swpc:alert:<serial>` or falls back to the
/// `issue_datetime` when no serial is found.
fn alert_guid(product_id: &str, message: &str, issue_dt: &str) -> String {
    for line in message.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Serial Number:") {
            let serial = rest.trim();
            if !serial.is_empty() {
                return format!("noaa-swpc:alert:{serial}");
            }
        }
    }
    // Fallback: product + issue datetime (unique enough for alerts without serials).
    format!("noaa-swpc:alert:{product_id}:{issue_dt}")
}

/// Parse a headline from the alert message body (the first non-empty line after
/// the header block, or the `Space Weather Message Code:` line).
fn alert_headline(message: &str) -> String {
    // Look for a line that is an all-caps summary (e.g. "GEOMAGNETIC STORM WATCH").
    for line in message.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with("Space Weather") || t.starts_with("Serial") || t.starts_with("Issue Time") {
            continue;
        }
        // First substantive line.
        return t.to_string();
    }
    String::new()
}

/// Parse `products/alerts.json` → (contract geo-events, raw values).
fn parse_alerts(body: &Value) -> (Vec<EnvGeoEvent>, Vec<Value>) {
    let arr = match body.as_array() {
        Some(a) => a,
        None => return (Vec::new(), Vec::new()),
    };
    let mut events = Vec::new();
    let mut raws = Vec::new();
    for entry_v in arr {
        let product_id = entry_v
            .get("product_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let issue_dt_raw = entry_v
            .get("issue_datetime")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let message = entry_v
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if issue_dt_raw.is_empty() {
            continue; // no timestamp → can't partition
        }
        let ts = alert_dt_to_local(&issue_dt_raw);
        let guid = alert_guid(&product_id, &message, &issue_dt_raw);
        let headline = alert_headline(&message);

        let mut extra = Map::new();
        extra.insert("product_id".into(), Value::String(product_id.clone()));
        extra.insert("message".into(), Value::String(message.clone()));

        events.push(EnvGeoEvent {
            ts,
            source: SOURCE.into(),
            guid,
            event_type: "alert".into(),
            magnitude: None,
            place: String::new(),
            lat: None,
            lon: None,
            severity: String::new(),
            headline,
            url: String::new(),
            expires: String::new(),
            extra,
        });
        raws.push(entry_v.clone());
    }
    (events, raws)
}

// ---------------------------------------------------------------------------
// Upsert helpers (mirror nws.rs idiom)

/// Upsert Kp readings into `environment/noaa-swpc/YYYY-MM.jsonl` by `guid`.
fn upsert_readings(vault: &Vault, rows: &[EnvReading]) -> Result<u64> {
    let stream = vault.stream(DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: std::collections::BTreeMap<String, Vec<&EnvReading>> = Default::default();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!("noaa-swpc reading {:?} has unpartitionable ts {:?}", r.guid, r.ts)
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

/// Upsert geo-events into `environment/noaa-swpc/events/YYYY-MM.jsonl` by `guid`.
fn upsert_events(vault: &Vault, rows: &[EnvGeoEvent]) -> Result<u64> {
    let stream = vault.stream(EVENTS_DIR, Partition::Month);
    let mut written = 0u64;
    let mut by_key: std::collections::BTreeMap<String, Vec<&EnvGeoEvent>> = Default::default();
    for r in rows {
        let key = Partition::Month.key(&r.ts).with_context(|| {
            format!("noaa-swpc alert {} has unpartitionable ts {:?}", r.guid, r.ts)
        })?;
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

/// Wrapper that carries the ts/guid purely for the month-partition routing;
/// the on-disk bytes are the raw API object verbatim (flattened).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(skip)]
    guid: String,
    #[serde(flatten)]
    value: Value,
}

/// Upsert raw objects into `environment/noaa-swpc/raw/YYYY-MM.jsonl`.
fn upsert_raw(vault: &Vault, lines: &[RawLine]) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let mut by_key: std::collections::BTreeMap<String, Vec<&RawLine>> = Default::default();
    for l in lines {
        let key = Partition::Month
            .key(&l.ts)
            .with_context(|| format!("noaa-swpc raw {} has unpartitionable ts {:?}", l.guid, l.ts))?;
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

/// Derive the guid of a stored raw object (alert: from the message; Kp: from `time_tag`).
fn raw_guid(v: &Value) -> String {
    // Alert object: derive from message serial.
    if let Some(msg) = v.get("message").and_then(Value::as_str) {
        if let Some(pid) = v.get("product_id").and_then(Value::as_str) {
            let idt = v.get("issue_datetime").and_then(Value::as_str).unwrap_or("");
            return alert_guid(pid, msg, idt);
        }
    }
    // Kp entry: key by time_tag.
    if let Some(tt) = v.get("time_tag").and_then(Value::as_str) {
        return format!("noaa-swpc:kp:{tt}");
    }
    // OVATION sidecar: key by observation_time if present.
    if let Some(ot) = v.get("Observation Time").and_then(Value::as_str) {
        return format!("noaa-swpc:ovation:{ot}");
    }
    String::new()
}

// ---------------------------------------------------------------------------
// The pull

/// Full pull: Kp + alerts + OVATION via the injected API client.
/// Returns counts: `kp_readings`, `alerts`.
fn pull_with(vault: &Vault, api: &impl SwpcApi) -> Result<PullOutcome> {
    let state = vault.read_swpc_sync().unwrap_or_default();
    let watermark = state.last_kp_ts.clone();
    let now = Local::now();

    let mut kp_written = 0u64;
    let mut alerts_written = 0u64;
    let mut raw_lines: Vec<RawLine> = Vec::new();
    let mut new_watermark = watermark.clone();

    // 1. Kp index (contract readings + raw).
    let kp_body = api.kp_index()?;
    let (kp_rows, kp_raws) = parse_kp(&kp_body, &watermark);
    if !kp_rows.is_empty() {
        kp_written = upsert_readings(vault, &kp_rows)?;
        // Advance watermark to the max time_tag seen.
        for row in &kp_rows {
            // guid is "noaa-swpc:kp:<time_tag>" — extract the time_tag portion.
            if let Some(g) = &row.guid {
                if let Some(tag) = g.strip_prefix("noaa-swpc:kp:") {
                    if tag > new_watermark.as_str() {
                        new_watermark = tag.to_string();
                    }
                }
            }
        }
        for (row, raw) in kp_rows.iter().zip(kp_raws.iter()) {
            raw_lines.push(RawLine {
                ts: row.ts.clone(),
                guid: row.guid.clone().unwrap_or_default(),
                value: raw.clone(),
            });
        }
    }

    // 2. Geomagnetic alerts (contract geo-events + raw). The SWPC alerts feed
    //    returns the last ~few-days window; upsert keeps them idempotent.
    let alerts_body = api.alerts()?;
    let (alert_rows, alert_raws) = parse_alerts(&alerts_body);
    if !alert_rows.is_empty() {
        alerts_written = upsert_events(vault, &alert_rows)?;
        for (row, raw) in alert_rows.iter().zip(alert_raws.iter()) {
            raw_lines.push(RawLine {
                ts: row.ts.clone(),
                guid: row.guid.clone(),
                value: raw.clone(),
            });
        }
    }

    // 3. OVATION aurora grid — raw sidecar only (too bulky for event rows).
    //    Tolerate a network failure here (it's a bonus artifact, not the primary).
    match api.ovation() {
        Ok(ovation_body) => {
            // Key the snapshot by "Observation Time" if present; fall back to now.
            let obs_time = ovation_body
                .get("Observation Time")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .unwrap_or_else(|| now.to_rfc3339());
            let obs_local = {
                // OVATION timestamps look like "2026-06-16T06:01:00Z".
                DateTime::parse_from_rfc3339(&obs_time)
                    .map(|d| d.with_timezone(&Local).to_rfc3339())
                    .unwrap_or_else(|_| now.to_rfc3339())
            };
            let ovation_guid = format!("noaa-swpc:ovation:{obs_time}");
            raw_lines.push(RawLine {
                ts: obs_local,
                guid: ovation_guid,
                value: ovation_body,
            });
        }
        Err(_) => {} // Tolerate OVATION failure — the primary Kp+alerts data still lands.
    }

    // 4. Raw layer (unconditional full fidelity).
    if !raw_lines.is_empty() {
        upsert_raw(vault, &raw_lines)?;
    }

    // 5. Advance cursor only after successful writes.
    vault.write_swpc_sync(&SwpcSyncState {
        last_kp_ts: new_watermark,
        updated: now.to_rfc3339(),
    })?;

    Ok(PullOutcome {
        headline: format!("{kp_written} Kp readings, {alerts_written} alerts"),
        counts: std::collections::BTreeMap::from([
            ("kp_readings", kp_written),
            ("alerts", alerts_written),
        ]),
    })
}

/// Production pull: hits the real SWPC endpoints.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    pull_with(vault, &SwpcClient)
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-swpc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -------------------------------------------------------------------
    // Fixtures — modeled on documented SWPC API responses.

    /// A realistic `planetary_k_index_1m.json` slice: 3 consecutive minute samples.
    fn kp_fixture() -> Value {
        json!([
            {"time_tag": "2026-06-10T03:00:00", "kp_index": 1, "estimated_kp": 1.33, "kp": "1+"},
            {"time_tag": "2026-06-10T03:01:00", "kp_index": 2, "estimated_kp": 2.00, "kp": "2o"},
            {"time_tag": "2026-06-10T03:02:00", "kp_index": 3, "estimated_kp": 2.67, "kp": "2+"}
        ])
    }

    /// A realistic `products/alerts.json` slice: 2 alerts.
    fn alerts_fixture() -> Value {
        json!([
            {
                "product_id": "K04W",
                "issue_datetime": "2026-06-10 03:15:00.000",
                "message": "Space Weather Message Code: WARK04\r\nSerial Number: 2000\r\nIssue Time: 2026 Jun 10 0315 UTC\r\n\r\nGEOMAGNETIC STORM WATCH\r\nThreshold Reached: K-index of 4 expected"
            },
            {
                "product_id": "EF3A",
                "issue_datetime": "2026-06-09 22:00:00.500",
                "message": "Space Weather Message Code: ALTEF3\r\nSerial Number: 1999\r\nIssue Time: 2026 Jun 09 2200 UTC\r\n\r\nCONTINUED ALERT: Electron 2MeV Integral Flux exceeded 1000pfu"
            }
        ])
    }

    /// A minimal OVATION snapshot (the real one is a huge lat×lon array; this
    /// exercises just the sidecar routing).
    fn ovation_fixture() -> Value {
        json!({
            "Observation Time": "2026-06-10T03:05:00Z",
            "Forecast Time": "2026-06-10T03:10:00Z",
            "Data Format": "[Longitude, Latitude, Aurora]",
            "coordinates": [[0.0, 60.0, 12], [1.0, 60.0, 8]]
        })
    }

    /// An empty alerts array — no active alerts.
    fn alerts_empty() -> Value {
        json!([])
    }

    // -------------------------------------------------------------------
    // Stub API

    struct Stub {
        kp: Value,
        alerts: Value,
        ovation: Result<Value>,
    }
    impl SwpcApi for Stub {
        fn kp_index(&self) -> Result<Value> {
            Ok(self.kp.clone())
        }
        fn alerts(&self) -> Result<Value> {
            Ok(self.alerts.clone())
        }
        fn ovation(&self) -> Result<Value> {
            self.ovation.as_ref().map(|v| v.clone()).map_err(|e| anyhow::anyhow!("{e}"))
        }
    }

    // -------------------------------------------------------------------
    // Time helper tests

    #[test]
    fn utc_naive_parses_and_converts() {
        let local = utc_naive_to_local("2026-06-10T03:00:00");
        // Must be valid RFC3339 and represent the same instant as UTC 03:00.
        let parsed = DateTime::parse_from_rfc3339(&local).expect("must be RFC3339");
        let utc_expected = Utc.with_ymd_and_hms(2026, 6, 10, 3, 0, 0).single().unwrap();
        assert_eq!(parsed.with_timezone(&Utc), utc_expected);
    }

    #[test]
    fn alert_dt_with_fractional_seconds() {
        let local = alert_dt_to_local("2026-06-15 12:35:56.833");
        let parsed = DateTime::parse_from_rfc3339(&local).expect("must be RFC3339");
        // Compare at second granularity — fractional seconds are preserved by the
        // parse but the rounded expected value still matches the same instant.
        let utc_parsed = parsed.with_timezone(&Utc);
        assert_eq!(utc_parsed.date_naive().to_string(), "2026-06-15");
        assert_eq!(utc_parsed.hour(), 12);
        assert_eq!(utc_parsed.minute(), 35);
        assert_eq!(utc_parsed.second(), 56);
    }

    #[test]
    fn alert_dt_without_fractional_seconds() {
        let local = alert_dt_to_local("2026-06-10 03:15:00");
        let parsed = DateTime::parse_from_rfc3339(&local).expect("must be RFC3339");
        let utc_expected = Utc.with_ymd_and_hms(2026, 6, 10, 3, 15, 0).single().unwrap();
        assert_eq!(parsed.with_timezone(&Utc), utc_expected);
    }

    // -------------------------------------------------------------------
    // Parser unit tests

    #[test]
    fn parse_kp_maps_contract_fields() {
        let (rows, raws) = parse_kp(&kp_fixture(), "");
        // All 3 samples should be returned (watermark is empty → all are new).
        assert_eq!(rows.len(), 3);
        assert_eq!(raws.len(), 3);

        let r0 = &rows[0];
        assert_eq!(r0.source, "noaa-swpc");
        assert_eq!(r0.metric, "kp");
        assert_eq!(r0.value, 1.33);
        assert_eq!(r0.unit, "index");
        assert_eq!(r0.station, "planetary");
        assert_eq!(r0.guid.as_deref(), Some("noaa-swpc:kp:2026-06-10T03:00:00"));
        assert_eq!(r0.extra.get("kp_class"), Some(&json!("1+")));
        // ts must be valid RFC3339.
        DateTime::parse_from_rfc3339(&r0.ts).expect("ts is RFC3339");
    }

    #[test]
    fn parse_kp_watermark_filters_old_samples() {
        // Watermark at the first sample — only the two newer ones should return.
        let (rows, _) = parse_kp(&kp_fixture(), "2026-06-10T03:00:00");
        assert_eq!(rows.len(), 2, "samples <= watermark are excluded");
        assert_eq!(rows[0].guid.as_deref(), Some("noaa-swpc:kp:2026-06-10T03:01:00"));
    }

    #[test]
    fn parse_kp_all_filtered_returns_empty() {
        let (rows, raws) = parse_kp(&kp_fixture(), "2026-06-10T09:00:00");
        assert!(rows.is_empty());
        assert!(raws.is_empty());
    }

    #[test]
    fn parse_alerts_maps_contract_fields() {
        let (events, raws) = parse_alerts(&alerts_fixture());
        assert_eq!(events.len(), 2);
        assert_eq!(raws.len(), 2);

        let a0 = &events[0];
        assert_eq!(a0.source, "noaa-swpc");
        assert_eq!(a0.event_type, "alert");
        assert_eq!(a0.guid, "noaa-swpc:alert:2000", "guid from serial number");
        // ts must be valid RFC3339.
        DateTime::parse_from_rfc3339(&a0.ts).expect("alert ts is RFC3339");
        assert!(!a0.headline.is_empty(), "headline extracted");
        assert_eq!(a0.extra.get("product_id"), Some(&json!("K04W")));
        assert!(a0.extra.get("message").is_some());

        let a1 = &events[1];
        assert_eq!(a1.guid, "noaa-swpc:alert:1999");
    }

    #[test]
    fn parse_alerts_empty_returns_empty() {
        let (events, raws) = parse_alerts(&alerts_empty());
        assert!(events.is_empty());
        assert!(raws.is_empty());
    }

    #[test]
    fn alert_guid_falls_back_when_no_serial() {
        let msg = "Space Weather Message Code: MISC\r\nSome other content";
        let guid = alert_guid("MISC", msg, "2026-06-10 01:00:00");
        // No serial → fallback includes product + issue_datetime.
        assert!(guid.contains("MISC"), "fallback guid includes product_id");
        assert!(guid.starts_with("noaa-swpc:alert:"));
    }

    // -------------------------------------------------------------------
    // Pull / store / dedupe tests

    #[test]
    fn pull_writes_kp_readings_alerts_and_raw() {
        let v = temp_vault("pull");
        let stub = Stub {
            kp: kp_fixture(),
            alerts: alerts_fixture(),
            ovation: Ok(ovation_fixture()),
        };
        let out = pull_with(&v, &stub).unwrap();
        assert_eq!(out.counts.get("kp_readings"), Some(&3));
        assert_eq!(out.counts.get("alerts"), Some(&2));

        // Kp readings in environment/noaa-swpc/YYYY-MM.jsonl.
        let readings_path = v.root().join("environment/noaa-swpc/2026-06.jsonl");
        let readings_txt = std::fs::read_to_string(&readings_path).unwrap();
        assert_eq!(readings_txt.lines().count(), 3, "3 Kp rows");
        assert!(readings_txt.contains("\"metric\":\"kp\""));
        assert!(readings_txt.contains("\"unit\":\"index\""));
        assert!(readings_txt.contains("\"kp_class\":\"1+\""));

        // Alert geo-events in environment/noaa-swpc/events/YYYY-MM.jsonl.
        let events_txt = std::fs::read_to_string(
            v.root().join("environment/noaa-swpc/events/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(events_txt.lines().count(), 2, "2 alert rows");
        assert!(events_txt.contains("\"event_type\":\"alert\""));
        assert!(events_txt.contains("noaa-swpc:alert:2000"));

        // Raw layer: 3 Kp + 2 alerts + 1 OVATION.
        let raw_txt =
            std::fs::read_to_string(v.root().join("environment/noaa-swpc/raw/2026-06.jsonl"))
                .unwrap();
        assert_eq!(raw_txt.lines().count(), 6, "3 Kp + 2 alerts + 1 OVATION raw");
        // OVATION sidecar is present — its raw line has "Observation Time" field.
        assert!(raw_txt.contains("Observation Time"), "OVATION sidecar present");

        // Cursor advanced.
        let state = v.read_swpc_sync().unwrap();
        assert_eq!(state.last_kp_ts, "2026-06-10T03:02:00", "watermark at latest sample");
        assert!(!state.updated.is_empty());
    }

    #[test]
    fn kp_repoll_deduplicates_by_guid() {
        let v = temp_vault("kp-dedup");
        let stub = Stub {
            kp: kp_fixture(),
            alerts: alerts_empty(),
            ovation: Ok(ovation_fixture()),
        };
        // First pull.
        pull_with(&v, &stub).unwrap();
        let r1 =
            std::fs::read_to_string(v.root().join("environment/noaa-swpc/2026-06.jsonl")).unwrap();
        assert_eq!(r1.lines().count(), 3);

        // Second pull with the same data — watermark should block all rows.
        pull_with(&v, &stub).unwrap();
        let r2 =
            std::fs::read_to_string(v.root().join("environment/noaa-swpc/2026-06.jsonl")).unwrap();
        assert_eq!(r2.lines().count(), 3, "no duplicate rows after re-poll");
    }

    #[test]
    fn alert_repoll_upserts_not_duplicates() {
        let v = temp_vault("alert-dedup");
        let stub = Stub {
            kp: kp_fixture(),
            alerts: alerts_fixture(),
            ovation: Ok(ovation_fixture()),
        };
        // Two polls with identical alerts.
        pull_with(&v, &stub).unwrap();
        pull_with(&v, &stub).unwrap();
        let events_txt = std::fs::read_to_string(
            v.root().join("environment/noaa-swpc/events/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(events_txt.lines().count(), 2, "upsert by guid — no duplicates");
    }

    #[test]
    fn watermark_advances_only_after_write_and_cursor_persists() {
        let v = temp_vault("watermark");
        let stub = Stub {
            kp: kp_fixture(),
            alerts: alerts_empty(),
            ovation: Ok(ovation_fixture()),
        };
        assert!(v.read_swpc_sync().is_none(), "no cursor before first pull");
        pull_with(&v, &stub).unwrap();
        let state = v.read_swpc_sync().unwrap();
        assert_eq!(state.last_kp_ts, "2026-06-10T03:02:00");

        // A second pull with the same fixture: watermark blocks all samples.
        let out2 = pull_with(&v, &stub).unwrap();
        assert_eq!(out2.counts.get("kp_readings"), Some(&0), "no new rows past watermark");
    }

    #[test]
    fn ovation_failure_is_tolerated() {
        let v = temp_vault("ovation-fail");
        let stub = Stub {
            kp: kp_fixture(),
            alerts: alerts_empty(),
            ovation: Err(anyhow::anyhow!("simulated network error")),
        };
        // Must not propagate an error — OVATION is a bonus sidecar.
        let out = pull_with(&v, &stub).unwrap();
        assert_eq!(out.counts.get("kp_readings"), Some(&3), "Kp still written");
    }

    #[test]
    fn empty_kp_and_alerts_writes_nothing() {
        let v = temp_vault("empty");
        let stub = Stub {
            kp: json!([]),
            alerts: alerts_empty(),
            ovation: Ok(ovation_fixture()),
        };
        let out = pull_with(&v, &stub).unwrap();
        assert_eq!(out.counts.get("kp_readings"), Some(&0));
        assert_eq!(out.counts.get("alerts"), Some(&0));
        assert!(!v.root().join("environment/noaa-swpc/2026-06.jsonl").exists());
        assert!(!v.root().join("environment/noaa-swpc/events").exists());
    }

    #[test]
    fn sync_state_round_trips_and_back_compat() {
        let empty: SwpcSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_kp_ts.is_empty());
        let v = temp_vault("sync");
        v.write_swpc_sync(&SwpcSyncState {
            last_kp_ts: "2026-06-10T03:02:00".into(),
            updated: "2026-06-10T03:05:00-07:00".into(),
        })
        .unwrap();
        let s = v.read_swpc_sync().unwrap();
        assert_eq!(s.last_kp_ts, "2026-06-10T03:02:00");
        assert!(!s.updated.is_empty());
    }

    #[test]
    fn old_contract_lines_still_deserialize() {
        // A minimal EnvReading (4 required fields) and EnvGeoEvent (4 required)
        // must parse without error — proves the upsert read path is back-compat.
        let v = temp_vault("compat");
        std::fs::create_dir_all(v.root().join("environment/noaa-swpc")).unwrap();
        std::fs::write(
            v.root().join("environment/noaa-swpc/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T03:00:00-07:00\",\"source\":\"noaa-swpc\",\"metric\":\"kp\",\"value\":1.33}\n",
        )
        .unwrap();
        let rows: Vec<EnvReading> =
            v.stream(DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, 1.33);
        assert!(rows[0].guid.is_none(), "sparse row has no guid");

        std::fs::create_dir_all(v.root().join("environment/noaa-swpc/events")).unwrap();
        std::fs::write(
            v.root().join("environment/noaa-swpc/events/2026-06.jsonl"),
            "{\"ts\":\"2026-06-10T03:15:00-07:00\",\"source\":\"noaa-swpc\",\"guid\":\"noaa-swpc:alert:old-1\",\"event_type\":\"alert\"}\n",
        )
        .unwrap();
        let events: Vec<EnvGeoEvent> =
            v.stream(EVENTS_DIR, Partition::Month).read("2026-06").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].guid, "noaa-swpc:alert:old-1");
    }
}
