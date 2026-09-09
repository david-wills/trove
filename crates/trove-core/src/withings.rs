//! Withings — smart scales, blood-pressure monitors, sleep mat, and ScanWatch.
//! One API (`wbsapi.withings.net`) covers all devices; the same collector
//! works for a scale-only user and a full-stack user.
//!
//! # Vault layout
//!
//! - **Raw layer (unconditional, every collection):**
//!   - `health/withings/measures/raw/YYYY-MM.jsonl` — measure groups verbatim
//!   - `health/withings/sleep/raw/YYYY-MM.jsonl` — sleep series records
//!   - `health/withings/sleep_summaries/raw/YYYY-MM.jsonl` — sleep summaries
//!   - `health/withings/activity/raw/YYYY.jsonl` — daily activity records
//!   - `health/withings/heart/raw/YYYY-MM.jsonl` — heart/ECG records
//!
//! - **Contract layer (measures only):**
//!   `health/medical/withings/observations/YYYY-MM.jsonl` —
//!   [`crate::health_medical::Observation`] rows, one per measure within each
//!   measure group (a single weigh-in group yields weight + fat% + muscle mass
//!   + … as separate rows). Deduped by `guid = "<grpid>-<type>"`.
//!
//! Sleep, activity, and heart/ECG land only in the raw layer — they do not
//! map cleanly to the observation contract and belong to unbound sibling
//! drafts (home.event / health-medical.condition).
//!
//! # Windowed backfill + cursor
//!
//! Each collection uses a persisted Unix-timestamp watermark in
//! `.trove/withings-sync.json`. The first sync seeds the watermark from a
//! fixed epoch (2010-01-01). Subsequent syncs pull from the watermark (less
//! an overlap window) through now in successive 90-day windows until the
//! present is reached.
//!
//! The Withings measure API returns groups sorted by `date` descending (newest
//! first); we parse them all and advance the watermark to the max date seen.
//!
//! # Auth
//!
//! OAuth via [`crate::sync::withings`]. This module calls
//! `sync::withings::fresh_token` at the top of every sync pass and handles
//! 401 as a token-expiry signal (fall back to reconnect advice).
//!
//! # Privacy
//!
//! Body composition, blood pressure, and sleep data are medical-grade.
//! The def ships `default_on: false` with explicit acknowledgement copy.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health_medical::Observation;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SYNC_FILE: &str = ".trove/withings-sync.json";
const SERVICE: &str = "withings";
const API_BASE: &str = "https://wbsapi.withings.net";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs in the watcher loop. Hourly: readings land at
/// weigh-in / wake cadence, once per day at most.
pub const WITHINGS_SYNC_SECS: u64 = 3600;

/// Max window size for a single measures request (Unix seconds). 90 days is
/// a safe ceiling well under Withings' documented limits.
const MAX_WINDOW_SECS: i64 = 90 * 86400;

/// Overlap behind the watermark on incremental passes — Withings can
/// back-date a sync, so re-pull a few days to catch anything that came in
/// late.
const OVERLAP_SECS: i64 = 7 * 86400;

/// Earliest possible Withings measurement (first product launched in 2010;
/// we use 2010-01-01 00:00:00 UTC = 1262304000).
const WITHINGS_EPOCH: i64 = 1_262_304_000;

/// Paths for raw collections and the contract observation stream.
const MEASURES_RAW_DIR: &str = "health/withings/measures/raw";
const SLEEP_RAW_DIR: &str = "health/withings/sleep/raw";
const SLEEP_SUMMARY_RAW_DIR: &str = "health/withings/sleep_summaries/raw";
const ACTIVITY_RAW_DIR: &str = "health/withings/activity/raw";
const HEART_RAW_DIR: &str = "health/withings/heart/raw";
const OBS_DIR: &str = "health/medical/withings/observations";

// ---------------------------------------------------------------------------
// Registry face

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(OBS_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(MEASURES_RAW_DIR)))
}

fn def_collect(
    vault: &Vault,
    _now: DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    match collect_withings_inner(vault) {
        Ok(stats) => Ok(crate::registry::CollectOutcome::note_if(
            stats.records > 0,
            || format!("withings synced — {} records", stats.records),
        )),
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "withings sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let stats = collect_withings_inner(vault)?;
    Ok(PullOutcome {
        headline: if stats.records == 0 {
            "Withings is up to date — no new records".to_string()
        } else {
            format!(
                "Withings synced — {} records ({} measures, {} observations)",
                stats.records, stats.raw_measures, stats.observations
            )
        },
        counts: BTreeMap::from([
            ("records", stats.records),
            ("observations", stats.observations),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. The pub mod + INTEGRATIONS
/// &DEF lines already exist (stub replacement).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "withings",
        name: "Withings",
        kind: IntegrationKind::CloudSync,
        // Body composition, BP, sleep — medical-grade. Opt-in with
        // explicit acknowledgement.
        default_on: false,
        description:
            "Syncs body composition, blood pressure, sleep, activity, and heart data \
             from all Withings devices — scales, ScanWatch, sleep mat, and BP monitors \
             — via one API. Hourly poll; full history on first connect.",
        domain: "health",
        vault_path: "health/withings/",
        toggleable: true,
        setup: &[
            "Body composition, blood pressure, and sleep are sensitive health data — \
             enabling this opts you in.",
            "Connect your Withings account on this card (you'll register a developer \
             app first — see the connection's setup steps).",
            "First sync backfills your full history; later syncs are incremental.",
        ],
        caveats:
            "Refresh tokens are valid for 1 year; you'll be prompted to reconnect \
             before they lapse. Scale-only users get measures; sleep mat / ScanWatch / \
             BP monitor data appears only if you own those devices.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WITHINGS_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("withings"),
    pull: Some(def_pull),
};

// Also expose the CONNECTION (owned here, not in a sub-module, for
// single-file authorship). The integrator adds the CONNECTIONS line.
pub use crate::sync::withings::CONNECTION;

// ---------------------------------------------------------------------------
// Sync stats

#[derive(Debug, Default, Clone)]
pub struct WithingsSyncStats {
    /// Total raw + contract records written or updated.
    pub records: u64,
    /// Raw measure groups written.
    pub raw_measures: u64,
    /// Contract observation rows written (new, deduped against disk).
    pub observations: u64,
    /// Raw sleep series records written.
    pub raw_sleep: u64,
    /// Raw sleep summary records written.
    pub raw_sleep_summaries: u64,
    /// Raw activity records written.
    pub raw_activity: u64,
    /// Raw heart/ECG records written.
    pub raw_heart: u64,
}

// ---------------------------------------------------------------------------
// Cursor

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct WithingsSyncState {
    /// Measures watermark: Unix timestamp of the newest measure group date
    /// persisted. Advance only after the full write so a crash re-drains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measures_through: Option<i64>,
    /// Sleep watermark: Unix timestamp of the latest sleep enddate persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_through: Option<i64>,
    /// Sleep summary watermark.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sleep_summary_through: Option<i64>,
    /// Activity watermark: Unix timestamp of the last activity day persisted
    /// (YYYY-MM-DD → midnight UTC of that date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_through: Option<i64>,
    /// Heart watermark: Unix timestamp of the latest ECG record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heart_through: Option<i64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl Vault {
    pub fn read_withings_sync(&self) -> WithingsSyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_withings_sync(&self, state: &WithingsSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP error classification

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

// ---------------------------------------------------------------------------
// HTTP client (injectable for tests)

/// The API calls the pull needs. A trait so tests drive the logic with
/// fixtures, never the network (the dexcom pattern).
///
/// Every endpoint accepts an `offset` parameter for pagination: when the
/// server sets `body.more != 0` in the response, the caller must re-request
/// the **same** window with `offset = body.offset` to fetch the next chunk.
/// Only once `body.more == 0` is the window fully drained.
trait WithingsApi {
    /// POST `action=getmeas` to `/measure` — body measure groups.
    /// `offset` = 0 for the first page; subsequent pages use the value from
    /// `body.offset` in the previous response.
    fn getmeas(
        &self,
        token: &str,
        startdate: i64,
        enddate: i64,
        offset: i64,
    ) -> Result<Value, FetchError>;

    /// POST `action=get` to `/v2/sleep` — intranight sleep series.
    fn sleep_get(
        &self,
        token: &str,
        startdate: i64,
        enddate: i64,
        offset: i64,
    ) -> Result<Value, FetchError>;

    /// POST `action=getsummary` to `/v2/sleep` — nightly sleep summaries.
    fn sleep_getsummary(
        &self,
        token: &str,
        startdateymd: &str,
        enddateymd: &str,
        offset: i64,
    ) -> Result<Value, FetchError>;

    /// POST `action=getactivity` to `/v2/measure` — daily activity.
    fn getactivity(
        &self,
        token: &str,
        startdateymd: &str,
        enddateymd: &str,
        offset: i64,
    ) -> Result<Value, FetchError>;

    /// POST `action=list` to `/v2/heart` — heart/ECG records.
    fn heart_list(
        &self,
        token: &str,
        startdate: i64,
        enddate: i64,
        offset: i64,
    ) -> Result<Value, FetchError>;
}

struct WithingsClient {
    base: String,
}

impl WithingsClient {
    fn post_form(
        &self,
        path: &str,
        token: &str,
        params: &[(&str, String)],
    ) -> Result<Value, FetchError> {
        let mut body = format!("access_token={}", token);
        for (k, v) in params {
            body.push('&');
            body.push_str(k);
            body.push('=');
            body.push_str(&urlencode(v));
        }
        match ureq::post(&format!("{}{path}", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
        {
            Ok(resp) => {
                let v: Value = resp
                    .into_json()
                    .map_err(|e| FetchError::Other(format!("parsing response: {e}")))?;
                // Withings wraps every response: {"status": 0, "body": {...}}.
                // status != 0 is an API-level error (401 for expired tokens
                // comes as status=401 in the body, not an HTTP 401).
                let status = v.get("status").and_then(Value::as_i64).unwrap_or(0);
                match status {
                    0 => Ok(v),
                    401 => Err(FetchError::Unauthorized),
                    429 => Err(FetchError::RateLimited),
                    other => {
                        let msg = v
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string();
                        Err(FetchError::Other(format!("API status {other}: {msg}")))
                    }
                }
            }
            Err(ureq::Error::Status(401, _)) => Err(FetchError::Unauthorized),
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

impl WithingsApi for WithingsClient {
    fn getmeas(
        &self,
        token: &str,
        startdate: i64,
        enddate: i64,
        offset: i64,
    ) -> Result<Value, FetchError> {
        let mut params = vec![
            ("action", "getmeas".into()),
            ("startdate", startdate.to_string()),
            ("enddate", enddate.to_string()),
            ("category", "1".into()), // real measurements only (not goals)
        ];
        if offset > 0 {
            params.push(("offset", offset.to_string()));
        }
        self.post_form("/measure", token, &params)
    }

    fn sleep_get(
        &self,
        token: &str,
        startdate: i64,
        enddate: i64,
        offset: i64,
    ) -> Result<Value, FetchError> {
        let mut params = vec![
            ("action", "get".into()),
            ("startdate", startdate.to_string()),
            ("enddate", enddate.to_string()),
            ("data_fields", "hr,rr,snoring".into()),
        ];
        if offset > 0 {
            params.push(("offset", offset.to_string()));
        }
        self.post_form("/v2/sleep", token, &params)
    }

    fn sleep_getsummary(
        &self,
        token: &str,
        startdateymd: &str,
        enddateymd: &str,
        offset: i64,
    ) -> Result<Value, FetchError> {
        let mut params = vec![
            ("action", "getsummary".into()),
            ("startdateymd", startdateymd.into()),
            ("enddateymd", enddateymd.into()),
            (
                "data_fields",
                concat!(
                    "breathing_disturbances_intensity,deepsleepduration,",
                    "durationtosleep,durationtowakeup,hr_average,hr_max,hr_min,",
                    "lightsleepduration,remsleepduration,rr_average,rr_max,rr_min,",
                    "sleep_score,snoring,snoringepisodecount,wakeupcount,wakeupduration"
                )
                .into(),
            ),
        ];
        if offset > 0 {
            params.push(("offset", offset.to_string()));
        }
        self.post_form("/v2/sleep", token, &params)
    }

    fn getactivity(
        &self,
        token: &str,
        startdateymd: &str,
        enddateymd: &str,
        offset: i64,
    ) -> Result<Value, FetchError> {
        let mut params = vec![
            ("action", "getactivity".into()),
            ("startdateymd", startdateymd.into()),
            ("enddateymd", enddateymd.into()),
            (
                "data_fields",
                "steps,distance,elevation,soft,moderate,intense,active,\
                 calories,totalcalories,hr_average,hr_min,hr_max"
                    .into(),
            ),
        ];
        if offset > 0 {
            params.push(("offset", offset.to_string()));
        }
        self.post_form("/v2/measure", token, &params)
    }

    fn heart_list(
        &self,
        token: &str,
        startdate: i64,
        enddate: i64,
        offset: i64,
    ) -> Result<Value, FetchError> {
        let mut params = vec![
            ("action", "list".into()),
            ("startdate", startdate.to_string()),
            ("enddate", enddate.to_string()),
        ];
        if offset > 0 {
            params.push(("offset", offset.to_string()));
        }
        self.post_form("/v2/heart", token, &params)
    }
}

/// Minimal percent-encode for form values (the oauth module idiom).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Measure type → test name (LOINC where defined, display name otherwise).
//
// Sources: python_withings_api MeasureType enum + LOINC NLM browser.

struct MeasureType {
    name: &'static str,
    /// LOINC code, if one exists for this measurement.
    loinc: Option<&'static str>,
    /// UCUM unit string.
    unit: &'static str,
}

fn measure_type_info(type_code: i64) -> MeasureType {
    match type_code {
        1 => MeasureType { name: "Weight", loinc: Some("29463-7"), unit: "kg" },
        4 => MeasureType { name: "Height", loinc: Some("8302-2"), unit: "m" },
        5 => MeasureType { name: "Fat Free Mass", loinc: None, unit: "kg" },
        6 => MeasureType { name: "Fat Ratio", loinc: Some("41982-0"), unit: "%" },
        8 => MeasureType { name: "Fat Mass", loinc: None, unit: "kg" },
        9 => MeasureType { name: "Diastolic Blood Pressure", loinc: Some("8462-4"), unit: "mm[Hg]" },
        10 => MeasureType { name: "Systolic Blood Pressure", loinc: Some("8480-6"), unit: "mm[Hg]" },
        11 => MeasureType { name: "Heart Rate", loinc: Some("8867-4"), unit: "/min" },
        12 => MeasureType { name: "Temperature", loinc: Some("8310-5"), unit: "degC" },
        54 => MeasureType { name: "SpO2", loinc: Some("59408-5"), unit: "%" },
        71 => MeasureType { name: "Body Temperature", loinc: Some("8310-5"), unit: "degC" },
        73 => MeasureType { name: "Skin Temperature", loinc: None, unit: "degC" },
        76 => MeasureType { name: "Muscle Mass", loinc: None, unit: "kg" },
        77 => MeasureType { name: "Hydration", loinc: None, unit: "kg" },
        88 => MeasureType { name: "Bone Mass", loinc: None, unit: "kg" },
        91 => MeasureType { name: "Pulse Wave Velocity", loinc: None, unit: "m/s" },
        123 => MeasureType { name: "VO2 Max", loinc: Some("60842-2"), unit: "mL/min/kg" },
        135 => MeasureType { name: "QRS Interval", loinc: None, unit: "ms" },
        136 => MeasureType { name: "PR Interval", loinc: None, unit: "ms" },
        138 => MeasureType { name: "QT Interval", loinc: None, unit: "ms" },
        139 => MeasureType { name: "Atrial Fibrillation", loinc: None, unit: "" },
        _ => MeasureType {
            name: "Unknown Measure",
            loinc: None,
            unit: "",
        },
    }
}

/// Withings returns values as integers with a 10^`unit` scale factor.
/// E.g., value=7500 unit=-2 → 75.00 kg.
fn scale_value(value: i64, unit_exp: i64) -> f64 {
    (value as f64) * 10f64.powi(unit_exp as i32)
}

// ---------------------------------------------------------------------------
// Pure mapping functions (fixture-tested)

/// Convert a Unix timestamp to local RFC3339. Returns `None` on overflow.
fn unix_to_local_rfc3339(ts: i64) -> Option<String> {
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|dt| dt.with_timezone(&Local).to_rfc3339())
}

/// Map one measure group into a list of [`Observation`] rows (one per
/// measure in the group). Returns an empty vec if the group's `date` field
/// is missing or its `measures` array is absent/empty.
///
/// `grpid` is the stable group identifier used as the base of the `guid`.
/// The full guid is `"<grpid>-<type>"` so different measure types in the
/// same group get distinct observation rows without collisions.
fn observations_from_group(group: &Value) -> Vec<(Observation, String)> {
    let grpid = match group.get("grpid").and_then(Value::as_i64) {
        Some(id) => id,
        None => return Vec::new(),
    };
    // `date` is the Unix timestamp of the measurement.
    let date_ts = match group.get("date").and_then(Value::as_i64) {
        Some(ts) => ts,
        None => return Vec::new(),
    };
    let ts = match unix_to_local_rfc3339(date_ts) {
        Some(t) => t,
        None => return Vec::new(),
    };

    let measures = match group.get("measures").and_then(Value::as_array) {
        Some(m) => m,
        None => return Vec::new(),
    };

    let mut rows = Vec::new();
    for m in measures {
        let type_code = match m.get("type").and_then(Value::as_i64) {
            Some(t) => t,
            None => continue,
        };
        let raw_value = match m.get("value").and_then(Value::as_i64) {
            Some(v) => v,
            None => continue,
        };
        let unit_exp = m.get("unit").and_then(Value::as_i64).unwrap_or(0);

        let info = measure_type_info(type_code);
        let numeric_value = scale_value(raw_value, unit_exp);
        let guid = format!("{grpid}-{type_code}");

        let mut extra = Map::new();
        // Preserve the raw API type code so callers can re-derive the name.
        extra.insert("measureType".into(), Value::from(type_code));
        // Device hash (not the real device id — Withings hash-anonymizes it).
        if let Some(d) = group.get("hash_deviceid").and_then(Value::as_str) {
            if !d.is_empty() {
                extra.insert("deviceId".into(), Value::String(d.to_string()));
            }
        }
        // attrib describes the data quality / attribution.
        if let Some(a) = group.get("attrib").and_then(Value::as_i64) {
            extra.insert("attrib".into(), Value::from(a));
        }
        // category: 1=real measure, 2=user objective.
        if let Some(c) = group.get("category").and_then(Value::as_i64) {
            extra.insert("category".into(), Value::from(c));
        }

        let obs = Observation {
            ts: ts.clone(),
            source: "withings".to_string(),
            guid: guid.clone(),
            test: info.name.to_string(),
            code: info.loinc.unwrap_or("").to_string(),
            code_system: if info.loinc.is_some() { "loinc".into() } else { String::new() },
            value: Some(numeric_value),
            value_text: String::new(),
            unit: info.unit.to_string(),
            reference_range: String::new(),
            flag: String::new(),
            panel: String::new(),
            provider: String::new(),
            extra,
        };
        rows.push((obs, guid));
    }
    rows
}

/// Extract `body.measuregrps` from a getmeas response; tolerates missing.
fn measure_groups(v: &Value) -> Vec<Value> {
    v.get("body")
        .and_then(|b| b.get("measuregrps"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Extract `body.series` from a sleep-get response (intranight series).
fn sleep_series(v: &Value) -> Vec<Value> {
    v.get("body")
        .and_then(|b| b.get("series"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Extract `body.series` from a sleep-getsummary response (nightly records).
fn sleep_summary_series(v: &Value) -> Vec<Value> {
    // getsummary also uses "series" as the key for the nightly rows.
    v.get("body")
        .and_then(|b| b.get("series"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Extract `body.activities` from a getactivity response.
fn activity_records(v: &Value) -> Vec<Value> {
    v.get("body")
        .and_then(|b| b.get("activities"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Extract `body.series` from a heart-list response.
fn heart_series(v: &Value) -> Vec<Value> {
    v.get("body")
        .and_then(|b| b.get("series"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Read pagination cursor from a Withings response body.
///
/// Returns `(more, offset)` where `more != 0` means another page is available
/// at `offset`. When fields are absent the response is treated as the final
/// page (`more=0, offset=0`).
fn pagination(v: &Value) -> (i64, i64) {
    let body = match v.get("body") {
        Some(b) => b,
        None => return (0, 0),
    };
    let more = body.get("more").and_then(Value::as_i64).unwrap_or(0);
    let offset = body.get("offset").and_then(Value::as_i64).unwrap_or(0);
    (more, offset)
}

// ---------------------------------------------------------------------------
// Write helpers

/// Append-dedup contract observations. Returns the count of new rows
/// written. Does not touch existing rows (append-only per the contract).
fn write_observations(vault: &Vault, rows: Vec<Observation>) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(OBS_DIR, Partition::Month);

    // Build the set of existing guids — skip any we already have.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                seen.insert(g.to_string());
            }
        }
    }

    let new_rows: Vec<Observation> =
        rows.into_iter().filter(|o| seen.insert(o.guid.clone())).collect();
    let n = new_rows.len() as u64;
    stream.append(&new_rows, |r| &r.ts)?;
    Ok(n)
}

/// Write raw measure groups, deduped by `grpid`. Returns the count of new
/// groups written. Raw files are month-partitioned by the group's `date`.
fn write_raw_measures(vault: &Vault, groups: Vec<Value>) -> Result<u64> {
    if groups.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(MEASURES_RAW_DIR, Partition::Month);

    let mut seen_ids: HashSet<i64> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(id) = v.get("grpid").and_then(Value::as_i64) {
                seen_ids.insert(id);
            }
        }
    }

    // Tag each group with a `_ts` field (the group's `date` as RFC3339) so
    // the month-partition writer can file it under the right month.
    let mut new_groups: Vec<TaggedRaw> = Vec::new();
    for g in groups {
        let grpid = match g.get("grpid").and_then(Value::as_i64) {
            Some(id) => id,
            None => continue,
        };
        if seen_ids.contains(&grpid) {
            continue;
        }
        let date_ts = g.get("date").and_then(Value::as_i64).unwrap_or(0);
        let ts = unix_to_local_rfc3339(date_ts).unwrap_or_else(|| "1970-01-01T00:00:00+00:00".into());
        seen_ids.insert(grpid);
        new_groups.push(TaggedRaw { ts, value: g });
    }

    let n = new_groups.len() as u64;
    stream.append(&new_groups, |r| &r.ts)?;
    Ok(n)
}

/// Write raw sleep series records (intranight), keyed by `startdate`.
fn write_raw_sleep(vault: &Vault, records: Vec<Value>) -> Result<u64> {
    if records.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(SLEEP_RAW_DIR, Partition::Month);

    let mut seen: HashSet<i64> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(ts) = v.get("startdate").and_then(Value::as_i64) {
                seen.insert(ts);
            }
        }
    }

    let mut new_recs: Vec<TaggedRaw> = Vec::new();
    for r in records {
        let startdate = r.get("startdate").and_then(Value::as_i64).unwrap_or(0);
        if seen.contains(&startdate) {
            continue;
        }
        let ts = unix_to_local_rfc3339(startdate).unwrap_or_else(|| "1970-01-01T00:00:00+00:00".into());
        seen.insert(startdate);
        new_recs.push(TaggedRaw { ts, value: r });
    }

    let n = new_recs.len() as u64;
    stream.append(&new_recs, |r| &r.ts)?;
    Ok(n)
}

/// Write raw sleep summary records (nightly), keyed by `id` (Withings
/// assigns a stable id to each sleep session).
fn write_raw_sleep_summaries(vault: &Vault, records: Vec<Value>) -> Result<u64> {
    if records.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(SLEEP_SUMMARY_RAW_DIR, Partition::Month);

    let mut seen: HashSet<i64> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(id) = v.get("id").and_then(Value::as_i64) {
                seen.insert(id);
            }
        }
    }

    let mut new_recs: Vec<TaggedRaw> = Vec::new();
    for r in records {
        let id = r.get("id").and_then(Value::as_i64).unwrap_or(0);
        // startdate for partitioning.
        let startdate = r.get("startdate").and_then(Value::as_i64).unwrap_or(0);
        if seen.contains(&id) {
            continue;
        }
        let ts = unix_to_local_rfc3339(startdate).unwrap_or_else(|| "1970-01-01T00:00:00+00:00".into());
        seen.insert(id);
        new_recs.push(TaggedRaw { ts, value: r });
    }

    let n = new_recs.len() as u64;
    stream.append(&new_recs, |r| &r.ts)?;
    Ok(n)
}

/// Write raw daily activity records, keyed by `date` (YYYY-MM-DD string).
/// Activity is daily-granular; partition by month.
fn write_raw_activity(vault: &Vault, records: Vec<Value>) -> Result<u64> {
    if records.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(ACTIVITY_RAW_DIR, Partition::Month);

    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(d) = v.get("date").and_then(Value::as_str) {
                seen.insert(d.to_string());
            }
        }
    }

    let mut new_recs: Vec<TaggedRaw> = Vec::new();
    for r in records {
        let date = match r.get("date").and_then(Value::as_str) {
            Some(d) => d.to_string(),
            None => continue,
        };
        if seen.contains(&date) {
            continue;
        }
        // Use midnight UTC of the date as the partition ts.
        let ts = format!("{date}T00:00:00+00:00");
        seen.insert(date);
        new_recs.push(TaggedRaw { ts, value: r });
    }

    let n = new_recs.len() as u64;
    stream.append(&new_recs, |r| &r.ts)?;
    Ok(n)
}

/// Write raw heart/ECG records, keyed by `ecg.signalid` or `timestamp`
/// (Withings ECG records carry a stable `signalid` inside the `ecg` object).
fn write_raw_heart(vault: &Vault, records: Vec<Value>) -> Result<u64> {
    if records.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(HEART_RAW_DIR, Partition::Month);

    let mut seen: BTreeSet<String> = BTreeSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(key) = heart_key(&v) {
                seen.insert(key);
            }
        }
    }

    let mut new_recs: Vec<TaggedRaw> = Vec::new();
    for r in records {
        let key = match heart_key(&r) {
            Some(k) => k,
            None => continue,
        };
        if seen.contains(&key) {
            continue;
        }
        let timestamp = r.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
        let ts = unix_to_local_rfc3339(timestamp).unwrap_or_else(|| "1970-01-01T00:00:00+00:00".into());
        seen.insert(key);
        new_recs.push(TaggedRaw { ts, value: r });
    }

    let n = new_recs.len() as u64;
    stream.append(&new_recs, |r| &r.ts)?;
    Ok(n)
}

fn heart_key(v: &Value) -> Option<String> {
    // Prefer ecg.signalid; fall back to timestamp as a string.
    if let Some(id) = v
        .get("ecg")
        .and_then(|e| e.get("signalid"))
        .and_then(Value::as_i64)
    {
        return Some(format!("ecg-{id}"));
    }
    v.get("timestamp")
        .and_then(Value::as_i64)
        .map(|t| format!("ts-{t}"))
}

/// A raw record tagged with a sort/partition timestamp. The `ts` field is
/// only used by the partition writer and is NOT serialized to disk (the
/// `value` is written verbatim).
struct TaggedRaw {
    ts: String,
    value: Value,
}

impl Serialize for TaggedRaw {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.value.serialize(s)
    }
}

// ---------------------------------------------------------------------------
// Window helpers

/// Returns the `(startdate, enddate)` window for an incremental fetch given
/// the current watermark (`through_ts`). `now` is in Unix seconds.
fn incremental_window(through_ts: Option<i64>, now: i64) -> (i64, i64) {
    let start = match through_ts {
        Some(t) => t.saturating_sub(OVERLAP_SECS).max(WITHINGS_EPOCH),
        None => WITHINGS_EPOCH,
    };
    // Cap at MAX_WINDOW_SECS per pass.
    let end = now.min(start + MAX_WINDOW_SECS);
    (start, end)
}

/// Convert a Unix `startdate` / `enddate` window to `YYYY-MM-DD` strings
/// for the date-based activity/sleep-summary endpoints.
fn window_to_ymd(start: i64, end: i64) -> (String, String) {
    let fmt = |ts: i64| {
        Utc.timestamp_opt(ts, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "1970-01-01".into())
    };
    (fmt(start), fmt(end))
}

// ---------------------------------------------------------------------------
// The pull

/// Top-level sync: refresh the token if needed, then pull. Called by both
/// the periodic watcher-loop collect and the manual "Sync now" pull.
fn collect_withings_inner(vault: &Vault) -> Result<WithingsSyncStats> {
    // fresh_token loads, refreshes (if expired), and persists the rotated
    // token before returning. It errors if not connected.
    let token = crate::sync::withings::fresh_token(vault)?;
    let client = WithingsClient { base: API_BASE.to_string() };
    pull_with(vault, &client, &token.access_token)
}

impl Vault {
    /// Hourly periodic pass: silent no-op when no token. Budgetable in the
    /// future (mirrors oura's budgeted loop); for now, runs to completion.
    pub fn collect_withings(&self, _budget: Option<u32>) -> Result<WithingsSyncStats> {
        if self.load_sync_token(SERVICE)?.is_none() {
            return Ok(WithingsSyncStats::default());
        }
        collect_withings_inner(self)
    }
}

/// The pull body over an injected API + access token (the testable seam).
///
/// For each stream the logic is:
///
/// 1. **Window loop** — iterate successive 90-day windows from the current
///    watermark to `now`.  The watermark advances to `end` after each window
///    is fully drained (not just to `max_date`) so that empty windows (no
///    data in that period) still move the cursor forward.  This ensures a
///    cold-start account whose data begins in 2018/2022/etc. walks through
///    the empty 2010–2017 windows quickly and reaches the user's real data.
///
/// 2. **Pagination loop** — within each window, loop while `body.more != 0`,
///    re-requesting the same `(start, end)` with the returned `body.offset`
///    to fetch the next chunk.  Only after `more == 0` is the window fully
///    drained; then the watermark is advanced and persisted.
fn pull_with(vault: &Vault, api: &impl WithingsApi, token: &str) -> Result<WithingsSyncStats> {
    let mut state = vault.read_withings_sync();
    let now = Utc::now().timestamp();
    let mut stats = WithingsSyncStats::default();

    // --- Measures -----------------------------------------------------------
    {
        let mut watermark = state.measures_through;
        loop {
            let (start, end) = incremental_window(watermark, now);
            // Drain all pages for this window.
            let mut offset = 0i64;
            loop {
                let resp = api
                    .getmeas(token, start, end, offset)
                    .map_err(|e| api_err("getmeas", e))?;
                let groups = measure_groups(&resp);

                // Observation contract rows.
                let obs_rows: Vec<Observation> = groups
                    .iter()
                    .flat_map(|g| observations_from_group(g).into_iter().map(|(o, _)| o))
                    .collect();
                let new_obs = write_observations(vault, obs_rows)?;

                // Raw layer.
                let new_raw = write_raw_measures(vault, groups)?;

                stats.raw_measures += new_raw;
                stats.observations += new_obs;
                stats.records += new_raw + new_obs;

                let (more, next_offset) = pagination(&resp);
                if more == 0 {
                    break;
                }
                offset = next_offset;
            }
            // Advance watermark to `end` so empty windows still progress.
            watermark = Some(watermark.map_or(end, |cur| cur.max(end)));
            state.measures_through = watermark;
            vault.write_withings_sync(&state)?;
            if end >= now {
                break;
            }
        }
    }

    // --- Sleep series (intranight) ------------------------------------------
    {
        let mut watermark = state.sleep_through;
        loop {
            let (start, end) = incremental_window(watermark, now);
            let mut offset = 0i64;
            loop {
                let resp = api
                    .sleep_get(token, start, end, offset)
                    .map_err(|e| api_err("sleep/get", e))?;
                let records = sleep_series(&resp);
                let n = write_raw_sleep(vault, records)?;
                stats.raw_sleep += n;
                stats.records += n;
                let (more, next_offset) = pagination(&resp);
                if more == 0 {
                    break;
                }
                offset = next_offset;
            }
            watermark = Some(watermark.map_or(end, |cur| cur.max(end)));
            state.sleep_through = watermark;
            vault.write_withings_sync(&state)?;
            if end >= now {
                break;
            }
        }
    }

    // --- Sleep summaries (nightly) ------------------------------------------
    {
        let mut watermark = state.sleep_summary_through;
        loop {
            let (start, end) = incremental_window(watermark, now);
            let (start_ymd, end_ymd) = window_to_ymd(start, end);
            let mut offset = 0i64;
            loop {
                let resp = api
                    .sleep_getsummary(token, &start_ymd, &end_ymd, offset)
                    .map_err(|e| api_err("sleep/getsummary", e))?;
                let records = sleep_summary_series(&resp);
                let n = write_raw_sleep_summaries(vault, records)?;
                stats.raw_sleep_summaries += n;
                stats.records += n;
                let (more, next_offset) = pagination(&resp);
                if more == 0 {
                    break;
                }
                offset = next_offset;
            }
            watermark = Some(watermark.map_or(end, |cur| cur.max(end)));
            state.sleep_summary_through = watermark;
            vault.write_withings_sync(&state)?;
            if end >= now {
                break;
            }
        }
    }

    // --- Activity -----------------------------------------------------------
    {
        let mut watermark = state.activity_through;
        loop {
            let (start, end) = incremental_window(watermark, now);
            let (start_ymd, end_ymd) = window_to_ymd(start, end);
            let mut offset = 0i64;
            loop {
                let resp = api
                    .getactivity(token, &start_ymd, &end_ymd, offset)
                    .map_err(|e| api_err("measure/getactivity", e))?;
                let records = activity_records(&resp);
                let n = write_raw_activity(vault, records)?;
                stats.raw_activity += n;
                stats.records += n;
                let (more, next_offset) = pagination(&resp);
                if more == 0 {
                    break;
                }
                offset = next_offset;
            }
            watermark = Some(watermark.map_or(end, |cur| cur.max(end)));
            state.activity_through = watermark;
            vault.write_withings_sync(&state)?;
            if end >= now {
                break;
            }
        }
    }

    // --- Heart / ECG --------------------------------------------------------
    {
        let mut watermark = state.heart_through;
        loop {
            let (start, end) = incremental_window(watermark, now);
            let mut offset = 0i64;
            loop {
                let resp = api
                    .heart_list(token, start, end, offset)
                    .map_err(|e| api_err("heart/list", e))?;
                let records = heart_series(&resp);
                let n = write_raw_heart(vault, records)?;
                stats.raw_heart += n;
                stats.records += n;
                let (more, next_offset) = pagination(&resp);
                if more == 0 {
                    break;
                }
                offset = next_offset;
            }
            watermark = Some(watermark.map_or(end, |cur| cur.max(end)));
            state.heart_through = watermark;
            vault.write_withings_sync(&state)?;
            if end >= now {
                break;
            }
        }
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_withings_sync(&state)?;

    Ok(stats)
}

fn api_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow!(
            "Withings rejected the token on {endpoint} (401) — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow!(
            "Withings rate limited {endpoint} (429) — will retry on next sync"
        ),
        FetchError::Other(m) => anyhow!("Withings {endpoint}: {m}"),
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-withings-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixture factories --------------------------------------------------

    /// One measure group in the exact Withings API shape: weight + fat_ratio
    /// from a Body+ scale.
    fn measure_group_weight_fat() -> Value {
        json!({
            "grpid": 100001,
            "attrib": 0,
            "date": 1_718_000_000i64, // 2024-06-10 ~12:53 UTC
            "created": 1_718_000_100i64,
            "modified": 1_718_000_100i64,
            "category": 1,
            "deviceid": null,
            "hash_deviceid": "abc123",
            "measures": [
                {"value": 7500, "type": 1, "unit": -2},  // 75.00 kg
                {"value": 1920, "type": 6, "unit": -2}   // 19.20 %
            ]
        })
    }

    /// A blood pressure measure group (systolic + diastolic + HR).
    fn measure_group_bp() -> Value {
        json!({
            "grpid": 100002,
            "attrib": 0,
            "date": 1_718_100_000i64,
            "created": 1_718_100_100i64,
            "category": 1,
            "hash_deviceid": "bpm01",
            "measures": [
                {"value": 120, "type": 10, "unit": 0},  // 120 mm[Hg] systolic
                {"value": 80, "type": 9, "unit": 0},    // 80 mm[Hg] diastolic
                {"value": 65, "type": 11, "unit": 0}    // 65 /min HR
            ]
        })
    }

    /// A getmeas response wrapping both groups.
    fn getmeas_response(groups: Vec<Value>) -> Value {
        json!({
            "status": 0,
            "body": {
                "updatetime": 1_718_100_200i64,
                "timezone": "America/New_York",
                "measuregrps": groups,
                "more": 0,
                "offset": 0
            }
        })
    }

    /// One sleep intranight series record.
    fn sleep_series_record() -> Value {
        json!({
            "startdate": 1_718_050_000i64,
            "enddate": 1_718_070_000i64,
            "state": 3,  // REM
            "model": 32,
            "data": {"hr": {"1718050000": 58}, "rr": {}, "snoring": {}}
        })
    }

    fn sleep_get_response(records: Vec<Value>) -> Value {
        json!({
            "status": 0,
            "body": {
                "series": records,
                "model": 32,
                "model_id": 64
            }
        })
    }

    /// One nightly sleep summary record.
    fn sleep_summary_record() -> Value {
        json!({
            "id": 5001i64,
            "startdate": 1_718_020_000i64,
            "enddate": 1_718_070_000i64,
            "model": 32,
            "model_id": 64,
            "timezone": "America/New_York",
            "data": {
                "deepsleepduration": 5400,
                "lightsleepduration": 10800,
                "remsleepduration": 5400,
                "wakeupduration": 900,
                "wakeupcount": 2,
                "sleep_score": 82,
                "hr_average": 58,
                "rr_average": 14,
                "snoring": 0,
                "snoringepisodecount": 0
            }
        })
    }

    fn sleep_getsummary_response(records: Vec<Value>) -> Value {
        json!({
            "status": 0,
            "body": {
                "series": records,
                "more": 0,
                "offset": 0
            }
        })
    }

    /// One daily activity record.
    fn activity_record() -> Value {
        json!({
            "date": "2024-06-10",
            "timezone": "America/New_York",
            "deviceid": null,
            "brand": 11,
            "is_tracker": true,
            "steps": 8500,
            "distance": 6800.0,
            "elevation": 25.0,
            "soft": 1200,
            "moderate": 600,
            "intense": 300,
            "active": 900,
            "calories": 350.0,
            "totalcalories": 2100.0,
            "hr_average": 72,
            "hr_min": 52,
            "hr_max": 145
        })
    }

    fn getactivity_response(records: Vec<Value>) -> Value {
        json!({
            "status": 0,
            "body": {
                "activities": records,
                "more": 0,
                "offset": 0
            }
        })
    }

    /// One heart/ECG record (Body Cardio or ScanWatch).
    fn heart_record() -> Value {
        json!({
            "timestamp": 1_718_200_000i64,
            "heart_rate": 62,
            "wearposition": 0,
            "ecg": {
                "signalid": 9001i64,
                "afib": 0
            }
        })
    }

    fn heart_list_response(records: Vec<Value>) -> Value {
        json!({
            "status": 0,
            "body": {
                "series": records,
                "more": 0,
                "offset": 0
            }
        })
    }

    // --- mock API -----------------------------------------------------------

    struct MockApi {
        measures: Value,
        sleep: Value,
        sleep_summary: Value,
        activity: Value,
        heart: Value,
    }

    impl MockApi {
        fn all_empty() -> Self {
            MockApi {
                measures: getmeas_response(vec![]),
                sleep: sleep_get_response(vec![]),
                sleep_summary: sleep_getsummary_response(vec![]),
                activity: getactivity_response(vec![]),
                heart: heart_list_response(vec![]),
            }
        }
    }

    impl WithingsApi for MockApi {
        fn getmeas(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            Ok(self.measures.clone())
        }
        fn sleep_get(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            Ok(self.sleep.clone())
        }
        fn sleep_getsummary(
            &self,
            _t: &str,
            _s: &str,
            _e: &str,
            _off: i64,
        ) -> Result<Value, FetchError> {
            Ok(self.sleep_summary.clone())
        }
        fn getactivity(&self, _t: &str, _s: &str, _e: &str, _off: i64) -> Result<Value, FetchError> {
            Ok(self.activity.clone())
        }
        fn heart_list(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            Ok(self.heart.clone())
        }
    }

    struct FailingApi {
        endpoint: &'static str,
    }

    impl FailingApi {
        fn unauthorized(ep: &'static str) -> Self {
            FailingApi { endpoint: ep }
        }
    }

    impl WithingsApi for FailingApi {
        fn getmeas(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            if self.endpoint == "getmeas" {
                Err(FetchError::Unauthorized)
            } else {
                Ok(getmeas_response(vec![]))
            }
        }
        fn sleep_get(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            Ok(sleep_get_response(vec![]))
        }
        fn sleep_getsummary(
            &self,
            _t: &str,
            _s: &str,
            _e: &str,
            _off: i64,
        ) -> Result<Value, FetchError> {
            Ok(sleep_getsummary_response(vec![]))
        }
        fn getactivity(
            &self,
            _t: &str,
            _s: &str,
            _e: &str,
            _off: i64,
        ) -> Result<Value, FetchError> {
            Ok(getactivity_response(vec![]))
        }
        fn heart_list(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            Ok(heart_list_response(vec![]))
        }
    }

    // --- pure mapping tests -------------------------------------------------

    #[test]
    fn maps_weight_group_to_two_observations() {
        let group = measure_group_weight_fat();
        let rows = observations_from_group(&group);
        assert_eq!(rows.len(), 2, "weight + fat_ratio");
        let (weight_obs, _) = rows.iter().find(|(o, _)| o.test == "Weight").unwrap();
        assert_eq!(weight_obs.source, "withings");
        assert_eq!(weight_obs.guid, "100001-1");
        assert!((weight_obs.value.unwrap() - 75.0).abs() < 1e-9, "75.00 kg");
        assert_eq!(weight_obs.unit, "kg");
        assert_eq!(weight_obs.code, "29463-7", "weight LOINC");
        assert_eq!(weight_obs.code_system, "loinc");

        let (fat_obs, _) = rows.iter().find(|(o, _)| o.test == "Fat Ratio").unwrap();
        assert_eq!(fat_obs.guid, "100001-6");
        assert!((fat_obs.value.unwrap() - 19.20).abs() < 1e-9, "19.20 %");
        assert_eq!(fat_obs.unit, "%");
        assert_eq!(fat_obs.code, "41982-0", "fat ratio LOINC");
    }

    #[test]
    fn maps_bp_group_to_three_observations() {
        let group = measure_group_bp();
        let rows = observations_from_group(&group);
        assert_eq!(rows.len(), 3, "systolic + diastolic + HR");

        let (sys, _) =
            rows.iter().find(|(o, _)| o.test == "Systolic Blood Pressure").unwrap();
        assert_eq!(sys.guid, "100002-10");
        assert!((sys.value.unwrap() - 120.0).abs() < 1e-9);
        assert_eq!(sys.unit, "mm[Hg]");
        assert_eq!(sys.code, "8480-6");

        let (dia, _) =
            rows.iter().find(|(o, _)| o.test == "Diastolic Blood Pressure").unwrap();
        assert_eq!(dia.guid, "100002-9");
        assert!((dia.value.unwrap() - 80.0).abs() < 1e-9);
        assert_eq!(dia.code, "8462-4");

        let (hr, _) = rows.iter().find(|(o, _)| o.test == "Heart Rate").unwrap();
        assert_eq!(hr.guid, "100002-11");
        assert!((hr.value.unwrap() - 65.0).abs() < 1e-9);
        assert_eq!(hr.unit, "/min");
        assert_eq!(hr.code, "8867-4");
    }

    #[test]
    fn scale_value_applies_exponent_correctly() {
        assert!((scale_value(7500, -2) - 75.0).abs() < 1e-9);
        assert!((scale_value(1920, -2) - 19.20).abs() < 1e-9);
        assert!((scale_value(120, 0) - 120.0).abs() < 1e-9);
        assert!((scale_value(1, -3) - 0.001).abs() < 1e-12);
    }

    #[test]
    fn group_without_grpid_returns_empty() {
        let no_id = json!({"date": 1_718_000_000i64, "measures": [{"value": 100, "type": 1, "unit": 0}]});
        assert!(observations_from_group(&no_id).is_empty());
    }

    #[test]
    fn group_without_date_returns_empty() {
        let no_date = json!({"grpid": 42i64, "measures": [{"value": 100, "type": 1, "unit": 0}]});
        assert!(observations_from_group(&no_date).is_empty());
    }

    #[test]
    fn group_without_measures_returns_empty() {
        let no_measures = json!({"grpid": 42i64, "date": 1_718_000_000i64, "measures": []});
        assert!(observations_from_group(&no_measures).is_empty());
    }

    #[test]
    fn measure_groups_parser_tolerates_missing_body() {
        assert!(measure_groups(&json!({})).is_empty());
        assert!(measure_groups(&json!({"body": {}})).is_empty());
        assert!(measure_groups(&json!({"body": {"measuregrps": null}})).is_empty());
    }

    #[test]
    fn incremental_window_overlaps_behind_watermark() {
        let now = 1_718_100_000i64;
        let through = now - 3600; // 1 hour ago
        let (start, end) = incremental_window(Some(through), now);
        assert!(start < through, "start is before the watermark (overlap)");
        assert_eq!(end, now);
        // Without a watermark: should start at WITHINGS_EPOCH.
        let (cold_start, _) = incremental_window(None, now);
        assert_eq!(cold_start, WITHINGS_EPOCH);
    }

    #[test]
    fn window_to_ymd_produces_correct_dates() {
        // 2024-06-10T00:00:00Z = 1717977600
        let (start, end) = window_to_ymd(1_717_977_600, 1_720_569_600);
        assert_eq!(start, "2024-06-10");
        assert_eq!(end, "2024-07-10");
    }

    #[test]
    fn unix_to_local_rfc3339_and_date_parse_correctly() {
        // 1718000000 = 2024-06-10T07:13:20Z
        let rfc3339 = unix_to_local_rfc3339(1_718_000_000).unwrap();
        // The timestamp round-trips: parsing it gives the same Unix second.
        let parsed = DateTime::parse_from_rfc3339(&rfc3339).unwrap().timestamp();
        assert_eq!(parsed, 1_718_000_000);
    }

    // --- integration: pull_with tests ---------------------------------------

    #[test]
    fn full_pull_writes_all_layers() {
        let v = temp_vault("fullpull");
        let api = MockApi {
            measures: getmeas_response(vec![measure_group_weight_fat(), measure_group_bp()]),
            sleep: sleep_get_response(vec![sleep_series_record()]),
            sleep_summary: sleep_getsummary_response(vec![sleep_summary_record()]),
            activity: getactivity_response(vec![activity_record()]),
            heart: heart_list_response(vec![heart_record()]),
        };

        let stats = pull_with(&v, &api, "tok").unwrap();

        // Observations: 2 groups → 2 + 3 = 5 measures → 5 observations.
        assert_eq!(stats.observations, 5);
        // Raw measures: 2 groups.
        assert_eq!(stats.raw_measures, 2);
        assert_eq!(stats.raw_sleep, 1);
        assert_eq!(stats.raw_sleep_summaries, 1);
        assert_eq!(stats.raw_activity, 1);
        assert_eq!(stats.raw_heart, 1);

        // Contract observations file exists and has 5 rows.
        let obs_dir = v.root().join(OBS_DIR);
        let total_obs: usize = std::fs::read_dir(&obs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .flat_map(|s| s.lines().map(str::to_string).collect::<Vec<_>>())
            .filter(|l| !l.trim().is_empty())
            .count();
        assert_eq!(total_obs, 5);

        // Weight observation fields.
        let obs_str = std::fs::read_dir(&obs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .collect::<String>();
        assert!(obs_str.contains("\"guid\":\"100001-1\""));
        assert!(obs_str.contains("\"test\":\"Weight\""));
        assert!(obs_str.contains("\"code\":\"29463-7\""));
        assert!(obs_str.contains("\"value\":75.0"));
        assert!(obs_str.contains("\"unit\":\"kg\""));

        // BP systolic.
        assert!(obs_str.contains("\"test\":\"Systolic Blood Pressure\""));
        assert!(obs_str.contains("\"code\":\"8480-6\""));

        // Cursor advanced.
        let state = v.read_withings_sync();
        assert!(state.measures_through.is_some());
        assert!(state.sleep_through.is_some());
        assert!(state.activity_through.is_some());
        assert!(state.heart_through.is_some());
        assert!(state.updated.is_some());
    }

    #[test]
    fn idempotent_pull_writes_zero_on_replay() {
        let v = temp_vault("idempotent");
        let api = MockApi {
            measures: getmeas_response(vec![measure_group_weight_fat()]),
            sleep: sleep_get_response(vec![sleep_series_record()]),
            sleep_summary: sleep_getsummary_response(vec![sleep_summary_record()]),
            activity: getactivity_response(vec![activity_record()]),
            heart: heart_list_response(vec![heart_record()]),
        };

        let first = pull_with(&v, &api, "tok").unwrap();
        assert!(first.observations > 0);

        // Second pass with identical data → 0 new records.
        let api2 = MockApi {
            measures: getmeas_response(vec![measure_group_weight_fat()]),
            sleep: sleep_get_response(vec![sleep_series_record()]),
            sleep_summary: sleep_getsummary_response(vec![sleep_summary_record()]),
            activity: getactivity_response(vec![activity_record()]),
            heart: heart_list_response(vec![heart_record()]),
        };
        let second = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(second.observations, 0);
        assert_eq!(second.raw_measures, 0);
        assert_eq!(second.raw_sleep, 0);
        assert_eq!(second.raw_activity, 0);
        assert_eq!(second.raw_heart, 0);
    }

    #[test]
    fn empty_collections_are_a_noop() {
        let v = temp_vault("empty");
        let api = MockApi::all_empty();
        let stats = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(stats.records, 0);
        // No raw directories created.
        assert!(!v.root().join(OBS_DIR).exists());
    }

    #[test]
    fn measure_api_401_surfaces_reconnect_message() {
        let v = temp_vault("meas401");
        let api = FailingApi::unauthorized("getmeas");
        let err = pull_with(&v, &api, "bad").unwrap_err().to_string();
        assert!(err.contains("reconnect"), "reconnect message: {err}");
        assert!(err.contains("getmeas"), "names the endpoint: {err}");
    }

    #[test]
    fn collect_without_token_is_silent_noop() {
        let v = temp_vault("notoken");
        let stats = v.collect_withings(None).unwrap();
        assert_eq!(stats.records, 0);
    }

    #[test]
    fn cursor_round_trips_back_compat() {
        // An empty cursor deserializes to all-None (cold start).
        let empty: WithingsSyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.measures_through.is_none());
        assert!(empty.activity_through.is_none());
        // A cursor with an unknown future field still deserializes.
        let fwd: WithingsSyncState = serde_json::from_str(
            r#"{"measures_through":1718000000,"future_field":"x"}"#,
        )
        .unwrap();
        assert_eq!(fwd.measures_through, Some(1_718_000_000));
    }

    #[test]
    fn def_connection_is_withings() {
        assert_eq!(DEF.connection, Some("withings"));
        assert_eq!(DEF.meta.id, "withings");
        // Medical data: opt-in only.
        assert!(!DEF.meta.default_on, "medical data must be opt-in");
    }

    #[test]
    fn heart_key_prefers_signalid_over_timestamp() {
        let with_ecg = json!({"timestamp": 1000, "ecg": {"signalid": 9001}});
        assert_eq!(heart_key(&with_ecg).unwrap(), "ecg-9001");
        let without_ecg = json!({"timestamp": 2000});
        assert_eq!(heart_key(&without_ecg).unwrap(), "ts-2000");
        let neither = json!({});
        assert!(heart_key(&neither).is_none());
    }

    // --- pagination helpers -------------------------------------------------

    #[test]
    fn pagination_reads_more_and_offset() {
        let resp_more = json!({"status": 0, "body": {"measuregrps": [], "more": 1, "offset": 50}});
        let (more, off) = pagination(&resp_more);
        assert_eq!(more, 1);
        assert_eq!(off, 50);

        let resp_done = json!({"status": 0, "body": {"measuregrps": [], "more": 0, "offset": 0}});
        let (more2, _) = pagination(&resp_done);
        assert_eq!(more2, 0);

        // Missing body → treated as final page.
        let (more3, _) = pagination(&json!({}));
        assert_eq!(more3, 0);
    }

    // --- A mock that pages: first call returns more=1 with one group,
    // second call (same window, offset != 0) returns more=0 with another group.

    struct PagingMockApi {
        /// Responses keyed by call index (0-based) for the measures endpoint.
        /// Other endpoints always return empty.
        measure_pages: std::sync::Mutex<Vec<Value>>,
        call_count: std::sync::Mutex<usize>,
    }

    impl PagingMockApi {
        fn new(pages: Vec<Value>) -> Self {
            PagingMockApi {
                measure_pages: std::sync::Mutex::new(pages),
                call_count: std::sync::Mutex::new(0),
            }
        }
    }

    impl WithingsApi for PagingMockApi {
        fn getmeas(
            &self,
            _t: &str,
            _s: i64,
            _e: i64,
            _off: i64,
        ) -> Result<Value, FetchError> {
            let mut idx = self.call_count.lock().unwrap();
            let pages = self.measure_pages.lock().unwrap();
            let resp = pages.get(*idx).cloned().unwrap_or_else(|| getmeas_response(vec![]));
            *idx += 1;
            Ok(resp)
        }
        fn sleep_get(&self, _t: &str, _s: i64, _e: i64, _off: i64) -> Result<Value, FetchError> {
            Ok(sleep_get_response(vec![]))
        }
        fn sleep_getsummary(
            &self,
            _t: &str,
            _s: &str,
            _e: &str,
            _off: i64,
        ) -> Result<Value, FetchError> {
            Ok(sleep_getsummary_response(vec![]))
        }
        fn getactivity(
            &self,
            _t: &str,
            _s: &str,
            _e: &str,
            _off: i64,
        ) -> Result<Value, FetchError> {
            Ok(getactivity_response(vec![]))
        }
        fn heart_list(
            &self,
            _t: &str,
            _s: i64,
            _e: i64,
            _off: i64,
        ) -> Result<Value, FetchError> {
            Ok(heart_list_response(vec![]))
        }
    }

    #[test]
    fn pagination_fetches_all_pages_within_a_window() {
        // Page 1: has group 100001, more=1, offset=1
        let page1 = json!({
            "status": 0,
            "body": {
                "measuregrps": [measure_group_weight_fat()],
                "more": 1,
                "offset": 1,
                "updatetime": 1_718_100_200i64,
                "timezone": "America/New_York"
            }
        });
        // Page 2: has group 100002, more=0 (final page)
        let page2 = json!({
            "status": 0,
            "body": {
                "measuregrps": [measure_group_bp()],
                "more": 0,
                "offset": 0,
                "updatetime": 1_718_100_200i64,
                "timezone": "America/New_York"
            }
        });

        let v = temp_vault("pagination");
        let api = PagingMockApi::new(vec![page1, page2]);
        let stats = pull_with(&v, &api, "tok").unwrap();

        // Should have collected both groups across the two pages:
        // group 100001 = 2 measures (weight + fat), group 100002 = 3 (BP + HR)
        assert_eq!(stats.observations, 5, "all 5 observations from both pages");
        assert_eq!(stats.raw_measures, 2, "2 raw measure groups from both pages");
    }

    #[test]
    fn cold_start_cursor_advances_through_empty_windows() {
        // An account with no data on the API; all empty responses.
        // The cursor must advance past WITHINGS_EPOCH and eventually reach now.
        let v = temp_vault("coldstart_advance");
        let api = MockApi::all_empty();
        let stats = pull_with(&v, &api, "tok").unwrap();
        // No data written.
        assert_eq!(stats.records, 0);
        // But all cursors must be set to at least `now` (≈current time, not epoch).
        let state = v.read_withings_sync();
        let now = Utc::now().timestamp();
        let epoch_plus_window = WITHINGS_EPOCH + MAX_WINDOW_SECS;
        assert!(
            state.measures_through.unwrap_or(0) > epoch_plus_window,
            "measures cursor advanced past the first epoch window (was {:?})",
            state.measures_through
        );
        // All five cursors advanced.
        assert!(state.measures_through.is_some());
        assert!(state.sleep_through.is_some());
        assert!(state.sleep_summary_through.is_some());
        assert!(state.activity_through.is_some());
        assert!(state.heart_through.is_some());
        // Each cursor is at or near now (within 2× the max window size to allow
        // for the last partial window).
        let tolerance = 2 * MAX_WINDOW_SECS;
        assert!(
            (state.measures_through.unwrap() - now).abs() < tolerance,
            "measures cursor is near now"
        );
    }

    #[test]
    fn incremental_pass_after_recent_watermark_single_window() {
        // If the watermark is recent (within one MAX_WINDOW), only one window
        // is needed to reach now.
        let v = temp_vault("incremental_single");
        let now = Utc::now().timestamp();
        // Seed the state as if we already synced up to 1 hour ago.
        let recent = now - 3600;
        let mut state = WithingsSyncState::default();
        state.measures_through = Some(recent);
        state.sleep_through = Some(recent);
        state.sleep_summary_through = Some(recent);
        state.activity_through = Some(recent);
        state.heart_through = Some(recent);
        v.write_withings_sync(&state).unwrap();

        let api = MockApi::all_empty();
        let stats = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(stats.records, 0);
        let after = v.read_withings_sync();
        // Cursor advanced to at least now.
        assert!(after.measures_through.unwrap_or(0) >= recent);
    }
}
