//! Fitbit / Google Health API — activity, sleep, heart rate, and biometric
//! data pulled via the Google Health API (the successor to the legacy Fitbit
//! Web API, which shuts down September 2026). Never the legacy Fitbit API or
//! Google Fit REST API — both retire in 2026.
//!
//! ## Layout under `health/fitbit/`
//!
//! - `<stream>.jsonl` — daily summaries, one record per day, sorted by date.
//!   Rewritten whole on change (a year of daily records is ~365 lines).
//! - `heartrate/YYYY-MM.jsonl` — intraday heart rate samples, ~5-min cadence.
//!   Month-partitioned (Oura pattern); only touched months are rewritten.
//! - `index.md` — human-readable summary table, regenerated each sync.
//!
//! ## API surface
//!
//! Base URL: `https://health.googleapis.com/v4`
//!
//! **Daily data types** (one record per day, keyed by `date`):
//! - `steps` — interval-based: `count` int64 over the day
//! - `distance` — interval-based: meters
//! - `floors` — interval-based: count
//! - `active-zone-minutes` — interval-based: per-zone minutes (fat burn / cardio / peak)
//! - `active-energy-burned` — interval-based: kcal
//! - `daily-resting-heart-rate` — `beatsPerMinute` int64
//! - `daily-heart-rate-variability` — `averageHeartRateVariabilityMilliseconds`
//! - `sleep` — session: stages, durations
//!
//! **Intraday** (keyed by ISO-8601 sample time):
//! - `heart-rate` — `beatsPerMinute` int64, sampled every ~5 min
//!
//! ## Cursor and backfill
//!
//! Each daily collection has a watermark (latest day pulled) persisted in
//! `.trove/fitbit-sync.json`. Backfill walks backward in 30-day windows until
//! consecutive empty windows indicate the account start. Intraday heart rate
//! has its own watermark and walks in 7-day windows.
//!
//! ## Auth
//!
//! Owned by [`crate::sync::fitbit`]. The pull calls
//! [`crate::sync::fitbit::fresh_token`] to get a live access token; a silent
//! no-op when no token is provisioned (budgeted watcher pass).
//!
//! ## Raw layer
//!
//! Full fidelity: every data point the API returns is written verbatim to
//! `health/fitbit/<stream>.jsonl` (or `health/fitbit/heartrate/YYYY-MM.jsonl`
//! for intraday). No contract binding — `health/` is a raw domain.
//!
//! brief: docs/integrations/fitbit.md

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use chrono::{Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, IntegrationDef, PullOutcome};
use crate::vault::Vault;

/// Seconds between Fitbit syncs in the watcher loop. Hourly: data lands after
/// each Fitbit sync from the phone app.
pub const FITBIT_SYNC_SECS: u64 = 3600;

const SYNC_FILE: &str = ".trove/fitbit-sync.json";
const API_BASE: &str = "https://health.googleapis.com";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Days re-pulled behind the watermark on each incremental pass. Fitbit may
/// recalculate recent daily summaries (e.g., after a late phone sync).
const OVERLAP_DAYS_DAILY: i64 = 3;
/// Overlap for intraday (heart rate) — 1 day covers typical late sync.
const OVERLAP_DAYS_INTRADAY: i64 = 1;
/// Seed window (first connect): pull recent data fast before backfilling.
const SEED_DAYS: i64 = 30;
/// Backfill window walking backward for daily collections.
const BACKFILL_DAYS_DAILY: i64 = 30;
/// Backfill window for intraday (heart rate: high volume, smaller windows).
const BACKFILL_DAYS_INTRADAY: i64 = 7;
/// Consecutive empty windows to consider the account start reached.
const BACKFILL_EMPTY_STOP: u32 = 3;
/// Hard floor — Fitbit was founded 2007, nothing precedes this.
const FITBIT_EPOCH: &str = "2007-01-01";

/// Per-pass request budget. `None` = unlimited (manual pull).
struct Budget(Option<u32>);

impl Budget {
    fn take(&mut self) -> bool {
        match &mut self.0 {
            None => true,
            Some(0) => false,
            Some(n) => {
                *n -= 1;
                true
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Registry hooks — the periodic pass and manual pull.

fn def_collect(
    vault: &Vault,
    _now: chrono::DateTime<Local>,
) -> Result<crate::registry::CollectOutcome> {
    let s = vault.collect_fitbit(Some(FITBIT_LOOP_BUDGET))?;
    Ok(crate::registry::CollectOutcome::note_if(s.records > 0, || {
        format!(
            "fitbit synced — {} records across {} streams",
            s.records, s.streams
        )
    }))
}

fn def_last_data(vault: &Vault) -> Option<String> {
    vault.read_fitbit_sync().map(|s| s.updated).filter(|u| !u.is_empty())
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let s = vault.fitbit_pull()?;
    let headline = if s.records == 0 {
        "Fitbit is up to date — no new records".to_string()
    } else {
        format!("Fitbit synced — {} records across {} streams", s.records, s.streams)
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("records", s.records),
            ("streams", u64::from(s.streams)),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "fitbit",
        name: "Fitbit",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Syncs activity, sleep, heart rate, and biometric data from your Fitbit \
             device via the Google Health API (successor to the Fitbit Web API; the Fitbit Web API \
             retires Sept 2026). Hourly pull with full-history backfill on first connect.",
        domain: "health",
        vault_path: "health/fitbit/",
        toggleable: true,
        setup: &[
            "Connect your Fitbit account via the Google Health API on this card (you'll \
             need a Google Cloud project — see the connection's setup steps).",
            "First sync backfills your available history; later syncs are incremental.",
        ],
        caveats:
            "Requires a Google Cloud project with the Health API enabled. Above 100 authorized \
             users, Google requires a CASA security review — fine for personal use, a gate for \
             wider distribution. Built against the Google Health API only — never the legacy \
             Fitbit Web API or Google Fit REST API (both retiring in 2026).",
    },
    behavior: Behavior::Periodic { cadence: Cadence::every_on_run(FITBIT_SYNC_SECS), collect: def_collect },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("fitbit"),
    pull: Some(def_pull),
};

/// Request budget for one watcher-loop pass. Covers incremental syncs (~20
/// requests); caps how long a backfill can occupy the owner loop.
pub const FITBIT_LOOP_BUDGET: u32 = 100;

// ---------------------------------------------------------------------------
// State structs (sync cursor, persisted in .trove/fitbit-sync.json).

/// Per-stream sync progress, persisted per stream in the sync file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FitbitStreamState {
    /// Newest day (YYYY-MM-DD) pulled successfully.
    pub watermark: Option<String>,
    /// Earliest day the backfill has walked back to; next window ends here.
    pub backfill_cursor: Option<String>,
    pub backfill_done: bool,
    /// Consecutive empty backfill windows seen.
    #[serde(default)]
    pub empty_windows: u32,
    /// Total records written for this stream (informational).
    pub records: u64,
}

/// Overall sync state for Fitbit; keyed by stream name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FitbitSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Error message from the last failed sync, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub streams: BTreeMap<String, FitbitStreamState>,
}

/// Result of one sync pass, for logging/the UI notice.
#[derive(Debug, Clone, Default, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct FitbitSyncStats {
    /// Streams that gained or changed records this pass.
    pub streams: u32,
    /// Records written (new + updated) this pass.
    pub records: u64,
    /// All streams have finished backfilling.
    pub backfill_done: bool,
}

// ---------------------------------------------------------------------------
// API client.

/// Status-level fetch errors that need distinct handling.
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

/// A page of data points returned by the API.
struct Page {
    data: Vec<Value>,
    next_page_token: Option<String>,
}

/// Thin Google Health API v4 client. The base URL is injected for
/// testability.
struct FitbitClient {
    base: String,
    token: String,
}

impl FitbitClient {
    fn get(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> Result<Value, FetchError> {
        let mut req = ureq::get(&format!("{}{path}", self.base))
            .set("Authorization", &format!("Bearer {}", self.token))
            .timeout(HTTP_TIMEOUT);
        for (k, v) in params {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
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

    /// List data points for a data type with a date filter.
    ///
    /// Filter syntax (Google Health API verbatim from primary docs):
    ///
    ///   Interval types  → `{camelType}.interval.civil_start_time >= "YYYY-MM-DD"
    ///                       AND {camelType}.interval.civil_start_time < "YYYY-MM-DD"`
    ///   Daily-summary   → `{camelType}.date >= "YYYY-MM-DD"
    ///                       AND {camelType}.date < "YYYY-MM-DD"`
    ///
    /// Both filters use camelCase data-type prefix (e.g. `steps`, `activeZoneMinutes`,
    /// `dailyHeartRateVariability`). The `filter_prefix` comes from the Stream definition.
    fn fetch_daily(
        &self,
        data_type: &str,
        filter_prefix: &str,
        key_field: KeyField,
        start: &str,
        end: &str,
        page_token: Option<&str>,
    ) -> Result<Page, FetchError> {
        let filter = match key_field {
            KeyField::IntervalCivilDate => {
                // Interval types: filter on civil_start_time (ISO 8601 date string).
                // Use half-open [start, end+1) by convention with < for the upper bound.
                format!(
                    "{filter_prefix}.interval.civil_start_time >= \"{start}\" \
                     AND {filter_prefix}.interval.civil_start_time < \"{end}\""
                )
            }
            KeyField::DailyDate => {
                // Daily-summary types: filter on .date (ISO 8601 date string).
                format!(
                    "{filter_prefix}.date >= \"{start}\" \
                     AND {filter_prefix}.date < \"{end}\""
                )
            }
            KeyField::SampleTime => {
                // Not used for daily — fall back to date filter should never happen.
                format!(
                    "{filter_prefix}.date >= \"{start}\" \
                     AND {filter_prefix}.date < \"{end}\""
                )
            }
        };
        let parent = format!("/v4/users/me/dataTypes/{data_type}/dataPoints");
        let mut params: Vec<(&str, &str)> = vec![("filter", &filter)];
        let pt_owned;
        if let Some(pt) = page_token {
            pt_owned = pt.to_string();
            params.push(("pageToken", &pt_owned));
        }
        let v = self.get(&parent, &params)?;
        Ok(Page {
            data: v
                .get("dataPoints")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            next_page_token: v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }

    /// Fetch intraday heart-rate samples using the list endpoint with a
    /// timestamp range filter.
    fn fetch_intraday_hr(
        &self,
        start_date: &str,
        end_date: &str,
        page_token: Option<&str>,
    ) -> Result<Page, FetchError> {
        // Heart rate: camelCase data-type prefix per primary docs
        // (e.g. `weight.sample_time.physical_time` — so heart-rate → `heartRate`).
        let filter = format!(
            "heartRate.sample_time.physical_time >= \"{start_date}T00:00:00Z\" \
             AND heartRate.sample_time.physical_time < \"{end_date}T23:59:59Z\""
        );
        let mut params: Vec<(&str, &str)> = vec![("filter", &filter)];
        let pt_owned;
        if let Some(pt) = page_token {
            pt_owned = pt.to_string();
            params.push(("pageToken", &pt_owned));
        }
        let v = self.get("/v4/users/me/dataTypes/heart-rate/dataPoints", &params)?;
        Ok(Page {
            data: v
                .get("dataPoints")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            next_page_token: v
                .get("nextPageToken")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
}

// ---------------------------------------------------------------------------
// Stream definitions — which data types to poll and how to store them.

/// Which field in the data point holds the date/time key for upsert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyField {
    /// Interval types (steps, distance, floors, AZM, energy, sleep).
    /// Key is derived from `interval.civilStartTime.date.{year,month,day}`
    /// → reconstructed as YYYY-MM-DD string.
    IntervalCivilDate,
    /// Daily-summary types (daily-resting-heart-rate, daily-heart-rate-variability).
    /// Key is derived from the top-level `date` object `{year,month,day}`
    /// → reconstructed as YYYY-MM-DD string.
    DailyDate,
    /// ISO-8601 timestamp from `heartRate.sampleTime.physicalTime` (intraday).
    SampleTime,
}

/// One Fitbit data type stream.
struct Stream {
    /// File stem under `health/fitbit/` and the sync-state key.
    name: &'static str,
    /// Google Health API data type path (kebab-case as used in the URL).
    api_type: &'static str,
    /// camelCase prefix used in the API filter expression (matches the union
    /// field name in the DataPoint response). Examples: `steps`, `activeZoneMinutes`,
    /// `dailyRestingHeartRate`.
    filter_prefix: &'static str,
    key_field: KeyField,
    /// Month-partitioned files (intraday heart rate).
    monthly: bool,
}

const fn interval_stream(
    name: &'static str,
    api_type: &'static str,
    filter_prefix: &'static str,
) -> Stream {
    Stream { name, api_type, filter_prefix, key_field: KeyField::IntervalCivilDate, monthly: false }
}

const fn daily_summary_stream(
    name: &'static str,
    api_type: &'static str,
    filter_prefix: &'static str,
) -> Stream {
    Stream { name, api_type, filter_prefix, key_field: KeyField::DailyDate, monthly: false }
}

/// Every daily data type the Google Health API exposes for Fitbit devices.
/// Sensors not present on the user's device return empty pages — no special
/// casing needed.
///
/// Stream kind note (from primary API spec):
///   Interval types  — steps, distance, floors, active-zone-minutes,
///                     active-energy-burned, sleep — carry `interval.civilStartTime`
///                     (a Date object {year,month,day}) as the temporal anchor; no
///                     `date` string field exists in the DataPoint.
///   Daily-summary   — daily-resting-heart-rate, daily-heart-rate-variability —
///                     carry a `date` object {year,month,day} at the top level of
///                     the union field; still a Date object, not a plain string.
static DAILY_STREAMS: &[Stream] = &[
    interval_stream("steps",                "steps",                "steps"),
    interval_stream("distance",             "distance",             "distance"),
    interval_stream("floors",               "floors",               "floors"),
    interval_stream("active-zone-minutes",  "active-zone-minutes",  "activeZoneMinutes"),
    interval_stream("active-energy-burned", "active-energy-burned", "activeEnergyBurned"),
    daily_summary_stream("resting-heart-rate",    "daily-resting-heart-rate",    "dailyRestingHeartRate"),
    daily_summary_stream("heart-rate-variability","daily-heart-rate-variability", "dailyHeartRateVariability"),
    interval_stream("sleep",                "sleep",                "sleep"),
];

static INTRADAY_STREAM: Stream = Stream {
    name: "heartrate",
    api_type: "heart-rate",
    filter_prefix: "heartRate",
    key_field: KeyField::SampleTime,
    monthly: true,
};

// ---------------------------------------------------------------------------
// Upsert logic — same pattern as oura.rs.

/// Reconstruct a YYYY-MM-DD string from a Google Health API `Date` object
/// `{"year": 2026, "month": 6, "day": 1}`.
fn date_obj_to_string(d: &Value) -> Option<String> {
    let year = d.get("year").and_then(Value::as_u64)?;
    let month = d.get("month").and_then(Value::as_u64)?;
    let day = d.get("day").and_then(Value::as_u64)?;
    Some(format!("{year:04}-{month:02}-{day:02}"))
}

/// Extract the upsert key from a data point, per the stream's key field.
///
/// Key shapes per primary API docs:
///
/// `IntervalCivilDate` — interval types (steps, distance, floors, AZM, energy, sleep):
///   The DataPoint union field (e.g. `record["steps"]`) contains an `interval`
///   object whose `civilStartTime.date` is a Date object `{year, month, day}`.
///   There is NO top-level `date` string field.
///
/// `DailyDate` — daily-summary types (dailyRestingHeartRate, dailyHeartRateVariability):
///   The DataPoint union field contains a `date` object `{year, month, day}`.
///   Again NOT a plain string — it is a Date object.
///
/// `SampleTime` — intraday heart rate:
///   `record["heartRate"]["sampleTime"]["physicalTime"]` is an RFC3339 string.
fn record_key(key_field: KeyField, record: &Value) -> Option<String> {
    match key_field {
        KeyField::IntervalCivilDate => {
            // Walk all top-level union fields looking for one that has
            // interval.civilStartTime.date = {year, month, day}.
            for (_field, val) in record.as_object()? {
                if let Some(civil_start) = val
                    .get("interval")
                    .and_then(|iv| iv.get("civilStartTime"))
                {
                    if let Some(date_obj) = civil_start.get("date") {
                        if let Some(s) = date_obj_to_string(date_obj) {
                            return Some(s);
                        }
                    }
                }
            }
            None
        }
        KeyField::DailyDate => {
            // Walk all top-level union fields looking for one that has a
            // `date` object {year, month, day}.
            for (_field, val) in record.as_object()? {
                if let Some(date_obj) = val.get("date") {
                    if let Some(s) = date_obj_to_string(date_obj) {
                        return Some(s);
                    }
                }
            }
            None
        }
        KeyField::SampleTime => {
            // Intraday heart rate: heartRate.sampleTime.physicalTime (RFC3339).
            record
                .get("heartRate")
                .and_then(|hr| hr.get("sampleTime"))
                .and_then(|st| st.get("physicalTime"))
                .and_then(Value::as_str)
                .map(str::to_string)
                // Flat path fallback for any future intraday types.
                .or_else(|| {
                    record
                        .get("sampleTime")
                        .and_then(|st| st.get("physicalTime"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        }
    }
}

/// Merge fetched records into the previous set by key.
/// Returns (merged, new_count, updated_count, total_keyed).
///
/// `total_keyed` is the number of fetched records that had a valid upsert key,
/// regardless of whether they were new, updated, or unchanged duplicates.
/// Callers use this to distinguish "API sent records but parse found no keys"
/// (shape mismatch) from "all records were already present and unchanged".
fn merge_records(
    prev: Vec<Value>,
    fetched: Vec<Value>,
    key_field: KeyField,
) -> (Vec<Value>, u64, u64, u64) {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    for r in prev {
        if let Some(k) = record_key(key_field, &r) {
            map.insert(k, r);
        }
    }
    let (mut new, mut updated, mut keyed) = (0u64, 0u64, 0u64);
    for r in fetched {
        let Some(k) = record_key(key_field, &r) else { continue };
        keyed += 1;
        match map.get(&k) {
            None => {
                new += 1;
                map.insert(k, r);
            }
            Some(old) if *old != r => {
                updated += 1;
                map.insert(k, r);
            }
            _ => {}
        }
    }
    (map.into_values().collect(), new, updated, keyed)
}

// ---------------------------------------------------------------------------
// Window helpers.

fn parse_day(s: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
}

fn incremental_range(watermark: Option<&str>, today: NaiveDate, overlap: i64) -> (String, String) {
    let start = watermark
        .and_then(parse_day)
        .map(|w| w - chrono::Duration::days(overlap))
        .unwrap_or_else(|| today - chrono::Duration::days(SEED_DAYS))
        .min(today);
    (start.to_string(), today.to_string())
}

fn next_backfill_window(cursor: &str, window_days: i64) -> Option<(String, String)> {
    let end = parse_day(cursor)?;
    let start = end - chrono::Duration::days(window_days);
    Some((start.to_string(), end.to_string()))
}

// ---------------------------------------------------------------------------
// Vault impl: the full sync pass.

impl Vault {
    /// One Fitbit sync pass: refresh the token if needed, bring every daily
    /// stream up to today, pull intraday heart rate, then continue the
    /// historical backfill. A silent no-op when no token is provisioned.
    ///
    /// `budget` limits the number of HTTP requests for a watcher-loop pass
    /// (`None` = unlimited, the manual "Sync now" path).
    pub fn collect_fitbit(&self, budget: Option<u32>) -> Result<FitbitSyncStats> {
        if self.load_sync_token("fitbit")?.is_none() {
            return Ok(FitbitSyncStats::default());
        }
        let mut state = self.read_fitbit_sync().unwrap_or_default();
        let token = match crate::sync::fitbit::fresh_token(self) {
            Ok(t) => t,
            Err(e) => {
                state.updated = Local::now().to_rfc3339();
                state.error = Some(format!("{e:#}"));
                self.write_fitbit_sync(&state)?;
                return Err(e);
            }
        };
        let client = FitbitClient {
            base: API_BASE.to_string(),
            token: token.access_token,
        };
        let result = self.fitbit_sync_pass(&client, &mut state, Budget(budget));
        state.updated = Local::now().to_rfc3339();
        state.error = result.as_ref().err().map(|e| format!("{e:#}"));
        self.write_fitbit_sync(&state)?;
        self.write_fitbit_index(&state)?;
        result
    }

    fn fitbit_sync_pass(
        &self,
        client: &FitbitClient,
        state: &mut FitbitSyncState,
        mut budget: Budget,
    ) -> Result<FitbitSyncStats> {
        let today = Local::now().date_naive();
        let mut stats = FitbitSyncStats::default();
        let mut touched: BTreeSet<&str> = BTreeSet::new();
        let mut soft_errors: Vec<String> = Vec::new();

        // Incremental pass for each daily stream.
        for stream in DAILY_STREAMS {
            let watermark = state
                .streams
                .get(stream.name)
                .and_then(|s| s.watermark.clone());
            let (start, end) =
                incremental_range(watermark.as_deref(), today, OVERLAP_DAYS_DAILY);
            match self.pull_daily_window(client, stream, &start, &end, &mut budget) {
                Ok(Some((new, updated))) => {
                    let ss = state.streams.entry(stream.name.to_string()).or_default();
                    if ss.backfill_cursor.is_none() && !ss.backfill_done {
                        ss.backfill_cursor = Some(start.clone());
                    }
                    ss.watermark = Some(end.clone());
                    ss.records += new;
                    stats.records += new + updated;
                    if new + updated > 0 {
                        touched.insert(stream.name);
                    }
                }
                Ok(None) => {
                    // Budget exhausted.
                    stats.streams = touched.len() as u32;
                    return Ok(stats);
                }
                Err(FetchError::Unauthorized) => {
                    soft_errors.push(format!(
                        "{}: unauthorized (401) — check the account's granted scopes",
                        stream.name
                    ));
                }
                Err(FetchError::RateLimited) => {
                    return Err(anyhow!(
                        "Fitbit rate limited the sync ({}) — it will resume next pass",
                        stream.name
                    ));
                }
                Err(FetchError::Other(msg)) => {
                    soft_errors.push(format!("{}: {msg}", stream.name));
                }
            }
        }

        // Incremental pass for intraday heart rate.
        {
            let stream = &INTRADAY_STREAM;
            let watermark = state.streams.get(stream.name).and_then(|s| s.watermark.clone());
            let (start, end) =
                incremental_range(watermark.as_deref(), today, OVERLAP_DAYS_INTRADAY);
            match self.pull_intraday_window(client, &start, &end, &mut budget) {
                Ok(Some((new, updated))) => {
                    let ss = state.streams.entry(stream.name.to_string()).or_default();
                    if ss.backfill_cursor.is_none() && !ss.backfill_done {
                        ss.backfill_cursor = Some(start.clone());
                    }
                    ss.watermark = Some(end.clone());
                    ss.records += new;
                    stats.records += new + updated;
                    if new + updated > 0 {
                        touched.insert(stream.name);
                    }
                }
                Ok(None) => {
                    stats.streams = touched.len() as u32;
                    return Ok(stats);
                }
                Err(FetchError::Unauthorized) => {
                    soft_errors.push(
                        "heartrate: unauthorized (401) — check the account's granted scopes"
                            .to_string(),
                    );
                }
                Err(FetchError::RateLimited) => {
                    return Err(anyhow!(
                        "Fitbit rate limited the sync (heartrate) — it will resume next pass"
                    ));
                }
                Err(FetchError::Other(msg)) => {
                    soft_errors.push(format!("heartrate: {msg}"));
                }
            }
        }
        self.write_fitbit_sync(state)?;

        // Backfill: walk history backward, round-robin per stream.
        loop {
            let mut progressed = false;

            for stream in DAILY_STREAMS {
                let ss = state.streams.entry(stream.name.to_string()).or_default();
                if ss.backfill_done {
                    continue;
                }
                let Some(cursor) = ss.backfill_cursor.clone() else { continue };
                let window = next_backfill_window(&cursor, BACKFILL_DAYS_DAILY)
                    .filter(|_| cursor.as_str() > FITBIT_EPOCH);
                let Some((start, end)) = window else {
                    ss.backfill_done = true;
                    continue;
                };
                match self.pull_daily_window(client, stream, &start, &end, &mut budget) {
                    Ok(Some((new, updated))) => {
                        let ss = state.streams.entry(stream.name.to_string()).or_default();
                        let fetched = new + updated;
                        if fetched == 0 {
                            ss.empty_windows += 1;
                            if ss.empty_windows >= BACKFILL_EMPTY_STOP {
                                ss.backfill_done = true;
                            }
                        } else {
                            ss.empty_windows = 0;
                        }
                        ss.backfill_cursor = Some(start);
                        ss.records += new;
                        stats.records += new + updated;
                        if new + updated > 0 {
                            touched.insert(stream.name);
                        }
                        progressed = true;
                        self.write_fitbit_sync(state)?;
                    }
                    Ok(None) => {
                        stats.streams = touched.len() as u32;
                        return Ok(stats);
                    }
                    Err(FetchError::Unauthorized) => {
                        soft_errors.push(format!(
                            "{}: unauthorized (401) backfill — skipping",
                            stream.name
                        ));
                        state.streams.entry(stream.name.to_string()).or_default().backfill_done = true;
                    }
                    Err(FetchError::RateLimited) => {
                        return Err(anyhow!(
                            "Fitbit rate limited the sync backfill ({}) — resuming next pass",
                            stream.name
                        ));
                    }
                    Err(FetchError::Other(msg)) => {
                        soft_errors.push(format!("{}: {msg}", stream.name));
                    }
                }
            }

            // Backfill for intraday heart rate.
            {
                let ss = state.streams.entry(INTRADAY_STREAM.name.to_string()).or_default();
                if !ss.backfill_done {
                    if let Some(cursor) = ss.backfill_cursor.clone() {
                        let window = next_backfill_window(&cursor, BACKFILL_DAYS_INTRADAY)
                            .filter(|_| cursor.as_str() > FITBIT_EPOCH);
                        if let Some((start, end)) = window {
                            match self.pull_intraday_window(client, &start, &end, &mut budget) {
                                Ok(Some((new, updated))) => {
                                    let ss = state
                                        .streams
                                        .entry(INTRADAY_STREAM.name.to_string())
                                        .or_default();
                                    let fetched = new + updated;
                                    if fetched == 0 {
                                        ss.empty_windows += 1;
                                        if ss.empty_windows >= BACKFILL_EMPTY_STOP {
                                            ss.backfill_done = true;
                                        }
                                    } else {
                                        ss.empty_windows = 0;
                                    }
                                    ss.backfill_cursor = Some(start);
                                    ss.records += new;
                                    stats.records += new + updated;
                                    if new + updated > 0 {
                                        touched.insert(INTRADAY_STREAM.name);
                                    }
                                    progressed = true;
                                    self.write_fitbit_sync(state)?;
                                }
                                Ok(None) => {
                                    stats.streams = touched.len() as u32;
                                    return Ok(stats);
                                }
                                Err(e) => {
                                    soft_errors.push(format!("heartrate backfill: {e}"));
                                    state
                                        .streams
                                        .entry(INTRADAY_STREAM.name.to_string())
                                        .or_default()
                                        .backfill_done = true;
                                }
                            }
                        } else {
                            state
                                .streams
                                .entry(INTRADAY_STREAM.name.to_string())
                                .or_default()
                                .backfill_done = true;
                        }
                    }
                }
            }

            if !progressed {
                break;
            }
        }

        stats.streams = touched.len() as u32;
        let all_streams: Vec<&str> = DAILY_STREAMS
            .iter()
            .map(|s| s.name)
            .chain(std::iter::once(INTRADAY_STREAM.name))
            .collect();
        stats.backfill_done = all_streams
            .iter()
            .all(|name| state.streams.get(*name).is_some_and(|s| s.backfill_done));

        if !soft_errors.is_empty() {
            soft_errors.truncate(3);
            bail!("fitbit sync hit errors: {}", soft_errors.join("; "));
        }
        Ok(stats)
    }

    /// Fetch one date window for a daily stream, paging through results, and
    /// upsert into the stream's JSONL file. Returns `Some((new, updated))` on
    /// success, `None` when the budget is exhausted (cursor not advanced).
    ///
    /// Defect guard: if the API returned data points but none could be keyed
    /// (parse/shape mismatch), this is surfaced as an error rather than silently
    /// advancing the cursor or counting toward empty_windows.
    fn pull_daily_window(
        &self,
        client: &FitbitClient,
        stream: &Stream,
        start: &str,
        end: &str,
        budget: &mut Budget,
    ) -> Result<Option<(u64, u64)>, FetchError> {
        let mut raw_count = 0usize;
        let mut records = Vec::new();
        let mut next: Option<String> = None;
        loop {
            if !budget.take() {
                return Ok(None);
            }
            let page =
                client.fetch_daily(stream.api_type, stream.filter_prefix, stream.key_field, start, end, next.as_deref())?;
            raw_count += page.data.len();
            records.extend(page.data);
            next = page.next_page_token;
            if next.is_none() {
                break;
            }
        }
        let (new, updated, keyed) = self
            .apply_fitbit_records(stream, records)
            .map_err(|e| FetchError::Other(format!("{e:#}")))?;
        // Guard: API returned data points but NONE could be keyed → shape mismatch,
        // not an empty date range. Distinct from "all records were unchanged duplicates"
        // (keyed > 0 but new = 0 and updated = 0).
        if raw_count > 0 && keyed == 0 {
            return Err(FetchError::Other(format!(
                "{}: API returned {raw_count} data point(s) but none could be keyed \
                 (shape mismatch — check key_field vs actual API response)",
                stream.name
            )));
        }
        Ok(Some((new, updated)))
    }

    /// Fetch one date window of intraday heart-rate samples.
    fn pull_intraday_window(
        &self,
        client: &FitbitClient,
        start: &str,
        end: &str,
        budget: &mut Budget,
    ) -> Result<Option<(u64, u64)>, FetchError> {
        let stream = &INTRADAY_STREAM;
        let mut raw_count = 0usize;
        let mut records = Vec::new();
        let mut next: Option<String> = None;
        loop {
            if !budget.take() {
                return Ok(None);
            }
            let page = client.fetch_intraday_hr(start, end, next.as_deref())?;
            raw_count += page.data.len();
            records.extend(page.data);
            next = page.next_page_token;
            if next.is_none() {
                break;
            }
        }
        let (new, updated, keyed) = self
            .apply_fitbit_records(stream, records)
            .map_err(|e| FetchError::Other(format!("{e:#}")))?;
        if raw_count > 0 && keyed == 0 {
            return Err(FetchError::Other(format!(
                "heartrate: API returned {raw_count} data point(s) but none could be keyed \
                 (shape mismatch — check SampleTime key path)"
            )));
        }
        Ok(Some((new, updated)))
    }

    /// Upsert fetched records into the stream's vault file(s).
    /// Returns `(new, updated, total_keyed)`. `total_keyed` counts every
    /// fetched record that had a valid key — including unchanged duplicates —
    /// so callers can detect shape-mismatch (keyed=0 despite raw data arriving).
    fn apply_fitbit_records(
        &self,
        stream: &Stream,
        fetched: Vec<Value>,
    ) -> Result<(u64, u64, u64)> {
        if fetched.is_empty() {
            return Ok((0, 0, 0));
        }
        let (mut new, mut updated, mut keyed) = (0u64, 0u64, 0u64);
        if stream.monthly {
            // Partition intraday records by month (from the sample timestamp).
            let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
            for r in fetched {
                let Some(k) = record_key(stream.key_field, &r) else { continue };
                if k.len() < 7 {
                    continue;
                }
                keyed += 1;
                by_month.entry(k[..7].to_string()).or_default().push(r);
            }
            for (month, recs) in by_month {
                let rel = format!("health/fitbit/{}/{month}.jsonl", stream.name);
                let prev = self.load_fitbit_records(&rel)?;
                let (merged, n, u, _) = merge_records(prev, recs, stream.key_field);
                self.write_snapshot(&rel, &merged)?;
                new += n;
                updated += u;
            }
        } else {
            let rel = format!("health/fitbit/{}.jsonl", stream.name);
            let prev = self.load_fitbit_records(&rel)?;
            let (merged, n, u, k) = merge_records(prev, fetched, stream.key_field);
            self.write_snapshot(&rel, &merged)?;
            new = n;
            updated = u;
            keyed = k;
        }
        Ok((new, updated, keyed))
    }

    pub(crate) fn load_fitbit_records(&self, rel: &str) -> Result<Vec<Value>> {
        let path = self.resolve(rel)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let body = fs::read_to_string(&path)?;
        Ok(body
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    /// Read the persisted sync state, if any.
    pub fn read_fitbit_sync(&self) -> Option<FitbitSyncState> {
        let path = self.resolve(SYNC_FILE).ok()?;
        let body = fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write_fitbit_sync(&self, state: &FitbitSyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }

    fn write_fitbit_index(&self, state: &FitbitSyncState) -> Result<()> {
        let mut md = format!(
            "# Fitbit\n\nLast sync: {}\n\n\
             | Stream | Records | Synced through | History |\n|---|---|---|---|\n",
            state.updated
        );
        let all_streams: Vec<(&str, &str)> = DAILY_STREAMS
            .iter()
            .map(|s| (s.name, s.name))
            .chain(std::iter::once((INTRADAY_STREAM.name, "heartrate (intraday)")))
            .collect();
        for (name, label) in all_streams {
            let s = state.streams.get(name).cloned().unwrap_or_default();
            let history = if s.backfill_done {
                "complete".to_string()
            } else {
                s.backfill_cursor
                    .as_deref()
                    .map(|cur| format!("backfilled to {cur}"))
                    .unwrap_or_else(|| "—".to_string())
            };
            md.push_str(&format!(
                "| {label} | {} | {} | {history} |\n",
                s.records,
                s.watermark.as_deref().unwrap_or("—"),
            ));
        }
        crate::store::write_atomic(&self.resolve("health/fitbit/index.md")?, md.as_bytes())
    }
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-fitbit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn daily_stream_ref(name: &str) -> &'static Stream {
        DAILY_STREAMS.iter().find(|s| s.name == name).unwrap()
    }

    // -----------------------------------------------------------------------
    // Helpers: real-shaped fixtures per primary API docs.

    /// Steps DataPoint: interval type, NO date string.
    /// Keyed by interval.civilStartTime.date {year, month, day}.
    fn steps_point(y: i64, m: i64, d: i64, count: &str) -> Value {
        json!({
            "steps": {
                "interval": {
                    "startTime": format!("{y:04}-{m:02}-{d:02}T00:00:00Z"),
                    "endTime":   format!("{y:04}-{m:02}-{d:02}T23:59:59Z"),
                    "civilStartTime": {
                        "date": {"year": y, "month": m, "day": d},
                        "time": {"hours": 0, "minutes": 0, "seconds": 0}
                    }
                },
                "count": count
            }
        })
    }

    /// DailyRestingHeartRate DataPoint: daily-summary type, date is a Date
    /// object {year, month, day}, NOT a string.
    fn resting_hr_point(y: i64, m: i64, d: i64, bpm: &str) -> Value {
        json!({
            "dailyRestingHeartRate": {
                "date": {"year": y, "month": m, "day": d},
                "beatsPerMinute": bpm
            }
        })
    }

    // -----------------------------------------------------------------------
    // record_key extraction

    #[test]
    fn record_key_interval_type_extracts_civil_start_date() {
        // Real steps DataPoint: no `date` string; key from interval.civilStartTime.date.
        let r = steps_point(2026, 6, 1, "8000");
        let k = record_key(KeyField::IntervalCivilDate, &r);
        assert_eq!(k.as_deref(), Some("2026-06-01"));
    }

    #[test]
    fn record_key_daily_summary_type_extracts_date_object() {
        // Real dailyRestingHeartRate DataPoint: date is a {year,month,day} object.
        let r = resting_hr_point(2026, 6, 2, "58");
        let k = record_key(KeyField::DailyDate, &r);
        assert_eq!(k.as_deref(), Some("2026-06-02"));
    }

    #[test]
    fn record_key_old_string_date_does_not_match_interval() {
        // Verify that a fabricated {date: "..."} string (old incorrect fixture)
        // does NOT key under IntervalCivilDate — guards against regression.
        let r = json!({"steps": {"date": "2026-06-01", "count": "8000"}});
        let k = record_key(KeyField::IntervalCivilDate, &r);
        assert!(k.is_none(), "string date field must NOT key interval type");
    }

    #[test]
    fn record_key_extracts_sample_time_for_intraday() {
        let r = json!({
            "heartRate": {
                "sampleTime": {
                    "physicalTime": "2026-06-01T08:00:00Z",
                    "utcOffset": "+00:00"
                },
                "beatsPerMinute": "72"
            }
        });
        let k = record_key(KeyField::SampleTime, &r);
        assert_eq!(k.as_deref(), Some("2026-06-01T08:00:00Z"));
    }

    #[test]
    fn record_key_returns_none_for_missing_key() {
        let r = json!({"count": "8000"});
        assert!(record_key(KeyField::IntervalCivilDate, &r).is_none());
        assert!(record_key(KeyField::DailyDate, &r).is_none());
    }

    #[test]
    fn date_obj_to_string_formats_correctly() {
        let d = json!({"year": 2026, "month": 6, "day": 1});
        assert_eq!(date_obj_to_string(&d).as_deref(), Some("2026-06-01"));
        let d2 = json!({"year": 2026, "month": 12, "day": 31});
        assert_eq!(date_obj_to_string(&d2).as_deref(), Some("2026-12-31"));
    }

    // -----------------------------------------------------------------------
    // merge_records logic

    #[test]
    fn merge_adds_new_and_counts_correctly() {
        let prev = vec![steps_point(2026, 6, 1, "5000")];
        let fetched = vec![
            steps_point(2026, 6, 1, "5500"), // updated (count changed)
            steps_point(2026, 6, 2, "8000"), // new
        ];
        let (merged, new, updated, keyed) =
            merge_records(prev, fetched, KeyField::IntervalCivilDate);
        assert_eq!(merged.len(), 2);
        assert_eq!((new, updated, keyed), (1, 1, 2));
        // Sorted by key — 2026-06-01 before 2026-06-02.
        assert_eq!(
            merged[0]["steps"]["count"].as_str(),
            Some("5500"),
            "updated record"
        );
    }

    #[test]
    fn merge_identical_record_is_neither_new_nor_updated() {
        let r = steps_point(2026, 6, 1, "5000");
        let (merged, new, updated, keyed) =
            merge_records(vec![r.clone()], vec![r], KeyField::IntervalCivilDate);
        assert_eq!(merged.len(), 1);
        assert_eq!((new, updated, keyed), (0, 0, 1));
    }

    #[test]
    fn merge_keyless_records_are_skipped() {
        let prev: Vec<Value> = vec![];
        let fetched = vec![json!({"count": "5000"})]; // no date field
        let (merged, new, updated, keyed) =
            merge_records(prev, fetched, KeyField::IntervalCivilDate);
        assert!(merged.is_empty());
        assert_eq!((new, updated, keyed), (0, 0, 0));
    }

    // -----------------------------------------------------------------------
    // Window calculation

    #[test]
    fn incremental_range_overlaps_behind_watermark() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 16).unwrap();
        let (start, end) = incremental_range(Some("2026-06-15"), today, OVERLAP_DAYS_DAILY);
        // 3-day overlap means start = 2026-06-12
        assert_eq!(start, "2026-06-12");
        assert_eq!(end, "2026-06-16");
    }

    #[test]
    fn incremental_range_seeds_without_watermark() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 16).unwrap();
        let (start, end) = incremental_range(None, today, OVERLAP_DAYS_DAILY);
        // Seed = 30 days back
        assert_eq!(start, "2026-05-17");
        assert_eq!(end, "2026-06-16");
    }

    #[test]
    fn backfill_window_walks_backward_30_days() {
        let (start, end) = next_backfill_window("2026-05-17", BACKFILL_DAYS_DAILY).unwrap();
        assert_eq!((start.as_str(), end.as_str()), ("2026-04-17", "2026-05-17"));
    }

    #[test]
    fn backfill_window_none_on_corrupt_cursor() {
        assert!(next_backfill_window("not-a-date", BACKFILL_DAYS_DAILY).is_none());
    }

    // -----------------------------------------------------------------------
    // Vault store round-trips

    #[test]
    fn daily_records_round_trip() {
        let v = temp_vault("daily-roundtrip");
        let stream = daily_stream_ref("steps");
        // Real-shaped fixtures: interval type, date is in civilStartTime.date.
        let fetched = vec![
            steps_point(2026, 6, 2, "9000"),
            steps_point(2026, 6, 1, "7000"),
        ];
        let (new, updated, _keyed) = v.apply_fitbit_records(stream, fetched).unwrap();
        assert_eq!((new, updated), (2, 0));
        let stored = v.load_fitbit_records("health/fitbit/steps.jsonl").unwrap();
        assert_eq!(stored.len(), 2);
        // Sorted by key: 2026-06-01 first.
        assert_eq!(stored[0]["steps"]["interval"]["civilStartTime"]["date"]["day"], 1);
        assert_eq!(stored[1]["steps"]["interval"]["civilStartTime"]["date"]["day"], 2);
    }

    #[test]
    fn intraday_hr_partitions_by_month() {
        let v = temp_vault("hr-monthly");
        let fetched = vec![
            json!({"heartRate": {"sampleTime": {"physicalTime": "2026-05-31T23:55:00Z"}, "beatsPerMinute": "60"}}),
            json!({"heartRate": {"sampleTime": {"physicalTime": "2026-06-01T00:05:00Z"}, "beatsPerMinute": "62"}}),
        ];
        let (new, updated, _keyed) = v
            .apply_fitbit_records(&INTRADAY_STREAM, fetched)
            .unwrap();
        assert_eq!((new, updated), (2, 0));
        assert!(v.root().join("health/fitbit/heartrate/2026-05.jsonl").exists());
        assert!(v.root().join("health/fitbit/heartrate/2026-06.jsonl").exists());
    }

    #[test]
    fn sync_state_persists_and_reloads() {
        let v = temp_vault("state");
        assert!(v.read_fitbit_sync().is_none());
        let mut state = FitbitSyncState {
            updated: "2026-06-16T10:00:00-07:00".into(),
            ..Default::default()
        };
        state.streams.insert(
            "steps".into(),
            FitbitStreamState {
                watermark: Some("2026-06-16".into()),
                records: 365,
                ..Default::default()
            },
        );
        v.write_fitbit_sync(&state).unwrap();
        let loaded = v.read_fitbit_sync().unwrap();
        assert_eq!(loaded.streams["steps"].records, 365);
        assert_eq!(loaded.streams["steps"].watermark.as_deref(), Some("2026-06-16"));
    }

    #[test]
    fn collect_without_token_is_silent_noop() {
        let v = temp_vault("notoken");
        let stats = v.collect_fitbit(Some(10)).unwrap();
        assert_eq!(stats.records, 0);
        assert!(!v.root().join("health/fitbit").exists());
    }

    #[test]
    fn manual_pull_without_token_is_error() {
        let v = temp_vault("pull-notoken");
        let err = v.fitbit_pull().unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn index_lists_every_stream() {
        let v = temp_vault("index");
        let mut state = FitbitSyncState::default();
        state.updated = "2026-06-16T10:00:00-07:00".into();
        state.streams.insert(
            "steps".into(),
            FitbitStreamState { watermark: Some("2026-06-16".into()), records: 180, backfill_done: true, ..Default::default() },
        );
        v.write_fitbit_index(&state).unwrap();
        let md = fs::read_to_string(v.root().join("health/fitbit/index.md")).unwrap();
        for s in DAILY_STREAMS {
            assert!(md.contains(s.name), "index missing {}", s.name);
        }
        assert!(md.contains("heartrate"), "index missing heartrate");
        assert!(md.contains("| steps | 180 | 2026-06-16 | complete |"));
    }

    // -----------------------------------------------------------------------
    // Full sync pass against a local stub server.

    /// Minimal HTTP/1.1 stub: maps request target to (status, JSON body).
    fn stub_server(route: impl Fn(&str) -> (u16, String) + Send + 'static) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut head = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            head.extend_from_slice(&buf[..n]);
                            if head.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let req = String::from_utf8_lossy(&head);
                let target = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or_default()
                    .to_string();
                let (status, body) = route(&target);
                let reason = if status == 200 { "OK" } else { "Error" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\n\
                     Content-Type: application/json\r\n\
                     Content-Length: {}\r\n\
                     Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        base
    }

    fn empty_page() -> (u16, String) {
        (200, json!({"dataPoints": [], "nextPageToken": null}).to_string())
    }

    #[test]
    fn sync_pass_writes_steps_and_resting_hr() {
        // Real-shaped fixtures: steps is interval (civilStartTime.date object);
        // dailyRestingHeartRate is daily-summary (date object, not string).
        let steps_dp = steps_point(2026, 6, 16, "9200");
        let rhr_dp = resting_hr_point(2026, 6, 16, "58");
        let steps_body = json!({"dataPoints": [steps_dp], "nextPageToken": null}).to_string();
        let rhr_body = json!({"dataPoints": [rhr_dp], "nextPageToken": null}).to_string();
        let v = temp_vault("pass-steps-hr");
        let base = stub_server(move |target| {
            if target.contains("/steps/") {
                return (200, steps_body.clone());
            }
            if target.contains("daily-resting-heart-rate") {
                return (200, rhr_body.clone());
            }
            empty_page()
        });
        let client = FitbitClient { base, token: "tok".into() };
        let mut state = FitbitSyncState::default();
        let stats = v.fitbit_sync_pass(&client, &mut state, Budget(None)).unwrap();
        assert!(stats.records >= 2, "expected ≥2 records, got {}", stats.records);
        assert!(v.root().join("health/fitbit/steps.jsonl").exists());
        assert!(v.root().join("health/fitbit/resting-heart-rate.jsonl").exists());
        assert!(state.streams["steps"].watermark.is_some());
        assert!(state.streams["resting-heart-rate"].watermark.is_some());
    }

    #[test]
    fn sync_pass_unauthorized_stream_does_not_block_others() {
        // A 401 on one stream (scope not granted) must not abort the whole pass.
        // Use real-shaped steps fixture.
        let steps_dp = steps_point(2026, 6, 16, "6000");
        let steps_body = json!({"dataPoints": [steps_dp], "nextPageToken": null}).to_string();
        let v = temp_vault("pass-401");
        let base = stub_server(move |target| {
            if target.contains("sleep") {
                return (401, json!({"error": "no scope"}).to_string());
            }
            if target.contains("/steps/") {
                return (200, steps_body.clone());
            }
            empty_page()
        });
        let client = FitbitClient { base, token: "tok".into() };
        let mut state = FitbitSyncState::default();
        // Soft errors don't make the pass fail — they're collected and then
        // the pass returns an Err wrapping all of them.
        let result = v.fitbit_sync_pass(&client, &mut state, Budget(None));
        // Sleep errored, but steps should still have been written.
        assert!(v.root().join("health/fitbit/steps.jsonl").exists());
        // The overall result is Err (soft errors from the 401 on sleep).
        assert!(result.is_err(), "soft errors should be surfaced");
        assert!(result.unwrap_err().to_string().contains("sleep"));
    }

    #[test]
    fn stream_catalog_names_are_valid_file_stems() {
        for s in DAILY_STREAMS {
            assert!(
                s.name.chars().all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit()),
                "bad file stem: {}",
                s.name
            );
        }
    }
}
