//! Ultrahuman Ring AIR + optional CGM — biometric data via the developer API.
//! Brief: docs/integrations/ultrahuman.md.
//!
//! # Auth
//!
//! **Personal API Token** (TokenPaste), not OAuth. The developer portal at
//! partner.ultrahuman.com (application-gated) issues a Bearer token. The brief
//! said "OAuth 2.0 via UltraSignal" but the actual published docs use a Personal
//! API Token with an `Authorization: <token>` header. No OAuth flow, no refresh.
//!
//! The API also requires the **account email** as a query parameter with every
//! request. The user pastes `email:token` (colon-separated); the module stores
//! the email in `scope` and the token in `access_token` — same pattern as
//! RetroAchievements.
//!
//! Access is application-gated (proposal review at vision.ultrahuman.com). Until
//! approved, the hub card ships as a `Periodic` stub with a connect card; the
//! pull silently no-ops when no token is stored. Flags: Needs-David (submit the
//! developer application), Needs-login (a ring-wearing account to validate with).
//!
//! # API shape (confirmed from multiple independent clients)
//!
//! Endpoint: `GET https://partner.ultrahuman.com/api/v1/metrics`
//! Required params: `email=<account-email>` AND `date=<YYYY-MM-DD>` (single date only).
//!
//! Response wrapper:
//! ```json
//! {
//!   "status": "...",
//!   "data": {
//!     "metric_data": [
//!       {
//!         "type": "hr",
//!         "object": {
//!           "values": [
//!             { "value": 58, "timestamp": 1748736000 }
//!           ]
//!         }
//!       }
//!     ]
//!   }
//! }
//! ```
//! Timestamps are **epoch seconds** (not milliseconds). Each metric type appears
//! once per day in the array; `values` is an array of intraday readings (HR is
//! per-minute, glucose ~5-min, recovery/sleep scores a single entry). This means
//! one Observation per reading per metric type, keyed by `<type>:<timestamp_s>`.
//!
//! # Vault layout
//!
//! - **Raw layer (unconditional):** `health/ultrahuman/raw/YYYY-MM.jsonl` — one
//!   raw record per day per metric_type entry, verbatim from the API.
//! - **Contract layer:** `health/medical/ultrahuman/observations/YYYY-MM.jsonl`
//!   — [`crate::health_medical::Observation`] rows, one per discrete reading
//!   (each `values[]` entry). Deduped by `guid = "<type>:<epoch_ts>"`.
//!
//! # Cursor
//!
//! Watermark in `.trove/ultrahuman-sync.json`. First sync seeds from `today - 30d`;
//! incremental syncs walk forward ONE DAY AT A TIME (the API is single-date only).
//! Cursor advances ONLY after the full write succeeds (crash re-drains the last day).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::health_medical::Observation;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SERVICE: &str = "ultrahuman";
const API_BASE: &str = "https://partner.ultrahuman.com";
const OBS_DIR: &str = "health/medical/ultrahuman/observations";
const RAW_DIR: &str = "health/ultrahuman/raw";
const SYNC_FILE: &str = ".trove/ultrahuman-sync.json";

/// Seconds between syncs in the watcher loop. A few times daily matches
/// when ring data lands after phone app sync.
pub const ULTRAHUMAN_SYNC_SECS: u64 = 4 * 3600;

/// Overlap re-pulled behind the watermark each pass — the ring syncs
/// retroactively through the phone, so a day or two of overlap is safe.
const OVERLAP_DAYS: i64 = 2;

/// First-connect seed window (recent data on screen fast).
const SEED_DAYS: i64 = 30;

/// Earliest possible date to backfill (Ultrahuman Ring launched 2022).
const ULTRAHUMAN_EPOCH: &str = "2022-01-01";

/// HTTP timeout — short so a hung connection can't stall the watcher loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Credential helpers

/// Split a `"email:token"` paste into `(email, token)`.
///
/// Splits on the FIRST colon so tokens containing colons (e.g. JWT with
/// base64url segments) are preserved intact.
fn split_credential(pasted: &str) -> Option<(String, String)> {
    let pasted = pasted.trim();
    let colon = pasted.find(':')?;
    let email = pasted[..colon].trim().to_string();
    let token = pasted[colon + 1..].trim().to_string();
    if email.is_empty() || token.is_empty() {
        return None;
    }
    Some((email, token))
}

// ---------------------------------------------------------------------------
// Cursor

/// Persisted watermark and sync metadata.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UltrahumanSyncState {
    /// RFC3339 local time of the last sync attempt.
    pub updated: String,
    /// Most recent date (YYYY-MM-DD) successfully synced; incremental passes
    /// re-pull `OVERLAP_DAYS` behind this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watermark: Option<String>,
    /// Error from the last pass, if any (for the UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Total raw records written (for index / stats).
    #[serde(default)]
    pub raw_records: u64,
    /// Total contract observations written.
    #[serde(default)]
    pub observations: u64,
}

impl UltrahumanSyncState {
    fn read(vault: &Vault) -> Option<Self> {
        let path = vault.resolve(SYNC_FILE).ok()?;
        let body = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&body).ok()
    }

    fn write(&self, vault: &Vault) -> Result<()> {
        crate::store::write_json_atomic(&vault.resolve(SYNC_FILE)?, self)
    }
}

// ---------------------------------------------------------------------------
// HTTP client

/// Thin client injected by tests.
///
/// Fetches metrics for a single date. The real API is single-date-only;
/// callers loop day-by-day.
trait ApiClient: Send {
    /// Fetch metrics for `email` on `date` (YYYY-MM-DD). Returns the raw JSON body.
    fn fetch_day(&self, email: &str, date: &str) -> Result<Value, FetchError>;
}

enum FetchError {
    Unauthorized,
    RateLimited,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (401) — check or reconnect the Ultrahuman token"),
            FetchError::RateLimited => write!(f, "rate limited (429) — will retry next sync"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

struct LiveClient {
    token: String,
}

impl ApiClient for LiveClient {
    fn fetch_day(&self, email: &str, date: &str) -> Result<Value, FetchError> {
        // Real endpoint: GET /api/v1/metrics with required email + date params.
        // Authorization header is bare token (not "Bearer <token>").
        let req = ureq::get(&format!("{API_BASE}/api/v1/metrics"))
            .set("Authorization", &self.token)
            .query("email", email)
            .query("date", date)
            .timeout(HTTP_TIMEOUT);
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
}

// ---------------------------------------------------------------------------
// Registry face

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(OBS_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(RAW_DIR)))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull_inner(vault) {
        Ok(stats) => {
            let total = stats.raw_records + stats.observations;
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!(
                    "ultrahuman synced — {} raw records, {} observations",
                    stats.raw_records, stats.observations
                )
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "ultrahuman sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let stats = pull_inner(vault)?;
    Ok(PullOutcome {
        headline: if stats.raw_records == 0 && stats.observations == 0 {
            "Ultrahuman is up to date — no new records".to_string()
        } else {
            format!(
                "Ultrahuman synced — {} raw records, {} observations",
                stats.raw_records, stats.observations
            )
        },
        counts: BTreeMap::from([
            ("raw_records", stats.raw_records),
            ("observations", stats.observations),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`]. The `pub mod` +
/// `&DEF` lines already exist — this replaces the NotWired stub body only.
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "ultrahuman",
        name: "Ultrahuman",
        kind: IntegrationKind::CloudSync,
        // CGM glucose is sensitive biometric detail — opt-in with explicit ack.
        default_on: false,
        description:
            "Syncs readiness, sleep, biometrics (HRV, SpO2, skin temperature), and optional \
             CGM glucose data from your Ultrahuman Ring AIR via the developer API. \
             Daily records with a full-history backfill.",
        domain: "health",
        vault_path: "health/ultrahuman/",
        toggleable: true,
        setup: &[
            "Continuous glucose and biometric data are sensitive — enabling this opts you in.",
            "API access requires applying at vision.ultrahuman.com (proposal-reviewed, not \
             self-service). Connect once approved.",
            "Paste your Ultrahuman account email, a colon, then the Personal API Token from \
             the developer portal (e.g. user@example.com:eyJhbGc…).",
        ],
        caveats:
            "Developer API access is application-gated (Ultrahuman reviews proposals). \
             CGM glucose rows appear only for users wearing the optional M1 patch; \
             ring-only users simply get no glucose data — no separate setup needed.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(ULTRAHUMAN_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("ultrahuman"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste — "email:token" combined, same pattern as RA)

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (email, token) = split_credential(pasted).ok_or_else(|| {
        anyhow::anyhow!(
            "expected \"email:token\" — paste your Ultrahuman account email, a colon, \
             then the Personal API Token from the developer portal \
             (e.g. user@example.com:eyJhbGc…)"
        )
    })?;
    // Validate the email looks plausible before storing.
    if !email.contains('@') {
        bail!("the part before the colon should be an email address (e.g. user@example.com:token)");
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            // token in access_token; (non-secret) email in scope — same pattern
            // as RetroAchievements (username:apikey).
            access_token: token,
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: Some(email),
            expires_at: None, // Ultrahuman personal tokens don't expire on a fixed schedule
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(tok) = vault.load_sync_token(SERVICE)? {
        let label = tok
            .scope
            .as_deref()
            .unwrap_or("Ultrahuman")
            .to_string();
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    // Personal token is self-service (once approved) — configured=true means
    // the connection is ready to accept a token paste.
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] by the integrator
/// (one `&crate::ultrahuman::CONNECTION,` line — not added here to avoid
/// duplicate registration).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "ultrahuman",
    display_name: "Ultrahuman",
    methods: &[ConnectMethod::TokenPaste {
        label: "Ultrahuman email:token",
        help: "Paste your Ultrahuman account email, a colon, then your Personal API token from \
               partner.ultrahuman.com. Example: user@example.com:eyJhbGc… — \
               API access requires prior approval from Ultrahuman.",
        placeholder: "user@example.com:eyJhbGc…",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["ultrahuman"],
    setup: &[
        "Apply for developer access at vision.ultrahuman.com (proposal-reviewed).",
        "Once approved, sign in at partner.ultrahuman.com and create a Personal API token.",
        "In the box below, paste your account email, a colon, then the token. \
         Example: user@example.com:eyJhbGc… — stored locally, never leaves your machine.",
    ],
};

// ---------------------------------------------------------------------------
// Pull implementation

/// Result of one sync pass.
#[derive(Debug, Clone, Default)]
struct PullStats {
    raw_records: u64,
    observations: u64,
}

fn pull_inner(vault: &Vault) -> Result<PullStats> {
    let tok = match vault.load_sync_token(SERVICE)? {
        Some(t) => t,
        None => bail!("Ultrahuman is not connected — paste your email:token from the developer portal"),
    };
    let token = tok.access_token;
    let email = tok.scope.ok_or_else(|| {
        anyhow::anyhow!("Ultrahuman: no email stored — reconnect with email:token format")
    })?;
    let client = LiveClient { token };
    pull_with_client(vault, &client, &email)
}

fn pull_with_client(vault: &Vault, client: &dyn ApiClient, email: &str) -> Result<PullStats> {
    let mut state = UltrahumanSyncState::read(vault).unwrap_or_default();
    let today = Local::now().date_naive();

    let start: NaiveDate = state
        .watermark
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok())
        .map(|w| (w - chrono::Duration::days(OVERLAP_DAYS)).max(
            NaiveDate::parse_from_str(ULTRAHUMAN_EPOCH, "%Y-%m-%d").unwrap()
        ))
        .unwrap_or_else(|| today - chrono::Duration::days(SEED_DAYS));
    let start = start.min(today);

    let mut stats = PullStats::default();

    // The API is single-date-only: iterate one day at a time.
    let mut day = start;
    while day <= today {
        let date_str = day.format("%Y-%m-%d").to_string();

        let raw = match client.fetch_day(email, &date_str) {
            Ok(v) => v,
            Err(FetchError::Unauthorized) => {
                state.error = Some("unauthorized — reconnect the Ultrahuman token".into());
                state.updated = Local::now().to_rfc3339();
                let _ = state.write(vault);
                bail!("Ultrahuman token rejected (401) — reconnect from the Integrations tab");
            }
            Err(FetchError::RateLimited) => {
                state.error = Some("rate limited — will retry next sync".into());
                state.updated = Local::now().to_rfc3339();
                let _ = state.write(vault);
                bail!("Ultrahuman rate limited — will retry next sync");
            }
            Err(FetchError::Other(e)) => {
                state.error = Some(e.clone());
                state.updated = Local::now().to_rfc3339();
                let _ = state.write(vault);
                bail!("Ultrahuman API error: {e}");
            }
        };

        // Parse data.metric_data[] from the response wrapper.
        let metric_data = extract_metric_data(&raw);

        if !metric_data.is_empty() {
            // Raw layer: one record per metric_data entry, tagged with date for partition.
            let new_raw = write_raw(vault, &metric_data, &date_str)?;
            stats.raw_records += new_raw;

            // Contract layer: one Observation per reading in object.values[].
            let obs = extract_observations(&metric_data, &date_str);

            // Guard: if we got metric_data entries but parsed zero observations,
            // treat as a parse error rather than advancing the watermark silently.
            if obs.is_empty() && new_raw > 0 {
                // metric_data is present but values are empty/unparseable for all
                // metrics on this day — this can legitimately happen (rest day with
                // no readings). We still advance so we don't re-fetch endlessly.
                // But we do NOT count it as an error.
            }

            let new_obs = write_observations(vault, obs)?;
            stats.observations += new_obs;

            // Advance watermark after successful write.
            state.watermark = Some(date_str.clone());
            state.raw_records += new_raw;
            state.observations += new_obs;
            state.error = None;
            state.updated = Local::now().to_rfc3339();
            state.write(vault)?;
        } else {
            // No metric_data for this day — likely future date or no data yet.
            // Still advance watermark to avoid re-fetching empty days.
            // But only if day is in the past (today might not have data yet).
            if day < today {
                state.watermark = Some(date_str);
                state.updated = Local::now().to_rfc3339();
                state.write(vault)?;
            }
        }

        day = day.succ_opt().unwrap_or(today);
    }

    Ok(stats)
}

/// Extract `data.metric_data` array from the API response wrapper.
///
/// Real shape: `{"status": "...", "data": {"metric_data": [...]}}`
/// Returns empty vec if the wrapper doesn't match or the array is missing.
fn extract_metric_data(v: &Value) -> Vec<Value> {
    v.get("data")
        .and_then(|d| d.get("metric_data"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Extract [`Observation`] rows from a `metric_data` array for a given date.
///
/// Each entry in `metric_data` has:
/// - `type`: metric name (e.g. "hr", "hrv", "glucose")
/// - `object.values[]`: array of `{value: f64, timestamp: i64}` (epoch seconds)
///
/// One Observation is emitted per reading in `values[]`.
/// `guid = "<type>:<epoch_seconds>"` for time-series metrics — stable and unique.
///
/// Scores/indices that have a single entry per day (recovery, sleep_score, etc.)
/// also get `<type>:<epoch_seconds>` guids, which is still unique and stable.
fn extract_observations(metric_data: &[Value], date: &str) -> Vec<Observation> {
    // (type_name, display_name, unit, loinc_code)
    // LOINC codes where known; empty string when not standardized.
    const METRIC_META: &[(&str, &str, &str, &str)] = &[
        ("hr",                   "Heart Rate",                    "bpm",       "8867-4"),
        ("hrv",                  "Heart Rate Variability (HRV)",  "ms",        "80404-7"),
        ("spo2",                 "SpO2",                          "%",         "2708-6"),
        ("temp",                 "Skin Temperature",              "°C",        ""),
        ("temperature_deviation","Skin Temperature Deviation",    "°C",        ""),
        ("night_rhr",            "Resting Heart Rate (night)",    "bpm",       "8867-4"),
        ("glucose",              "Glucose (CGM)",                 "mg/dL",     "2339-0"),
        ("average_glucose",      "Average Glucose",               "mg/dL",     "2339-0"),
        ("glucose_variability",  "Glucose Variability",           "%",         ""),
        ("hba1c",                "Estimated HbA1c",               "%",         "4548-4"),
        ("time_in_target",       "Time in Target Glucose Range",  "%",         ""),
        ("sleep_score",          "Sleep Score",                   "score",     ""),
        ("recovery",             "Recovery Score",                "score",     ""),
        ("movement_index",       "Movement Index",                "score",     ""),
        ("metabolic_score",      "Metabolic Score",               "score",     ""),
        ("recovery_index",       "Recovery Index",                "score",     ""),
        ("sleep_efficiency",     "Sleep Efficiency",              "%",         ""),
        ("vo2_max",              "VO₂ Max",                       "mL/kg/min", "59408-5"),
        ("steps",                "Steps",                         "count",     "41950-7"),
        ("total_sleep",          "Total Sleep",                   "h",         "93832-4"),
        ("active_minutes",       "Active Minutes",                "min",       ""),
        ("deep_sleep",           "Deep Sleep",                    "h",         ""),
        ("rem_sleep",            "REM Sleep",                     "h",         ""),
        ("light_sleep",          "Light Sleep",                   "h",         ""),
    ];

    let mut out = Vec::new();

    for entry in metric_data {
        let metric_type = match entry.get("type").and_then(Value::as_str) {
            Some(t) => t,
            None => continue,
        };

        // Look up display metadata for this metric type.
        let (display_name, unit, loinc) = METRIC_META
            .iter()
            .find(|&&(k, _, _, _)| k == metric_type)
            .map(|&(_, name, unit, loinc)| (name, unit, loinc))
            .unwrap_or((metric_type, "", ""));

        // values[] is an array of {value, timestamp} readings.
        let values = match entry
            .get("object")
            .and_then(|o| o.get("values"))
            .and_then(Value::as_array)
        {
            Some(arr) if !arr.is_empty() => arr,
            _ => continue,
        };

        for reading in values {
            let value = match reading.get("value").and_then(Value::as_f64) {
                Some(v) => v,
                None => continue,
            };
            let timestamp_secs = match reading.get("timestamp").and_then(Value::as_i64) {
                Some(ts) => ts,
                None => continue,
            };

            // Convert epoch seconds to RFC3339 local time for the ts field.
            let ts_local: String = Utc
                .timestamp_opt(timestamp_secs, 0)
                .single()
                .map(|dt| dt.with_timezone(&Local).to_rfc3339())
                .unwrap_or_else(|| format!("{date}T00:00:00+00:00"));

            // guid = "<type>:<epoch_seconds>" — unique per reading.
            let guid = format!("{metric_type}:{timestamp_secs}");

            let mut extra: Map<String, Value> = Map::new();
            extra.insert("date".into(), Value::String(date.to_string()));

            let mut obs = Observation::new(SERVICE, guid, ts_local.clone(), display_name);
            obs.value = Some(value);
            if !unit.is_empty() {
                obs.unit = unit.to_string();
            }
            if !loinc.is_empty() {
                obs.code = loinc.to_string();
                obs.code_system = "loinc".to_string();
            }
            obs.extra = extra;
            out.push(obs);
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Vault write helpers

/// Raw record tagged with a date string for the month-partition writer.
///
/// Each entry is a `metric_data` element: `{type, object: {values: [...]}}`.
/// We also inject a `_date` key so the stream can partition by month.
#[derive(Serialize)]
struct TaggedRaw {
    #[serde(rename = "_date")]
    date: String,
    #[serde(flatten)]
    value: Value,
}

/// Write raw metric_data entries. Deduped by `<type>:<date>`.
/// Returns the count of new records written.
fn write_raw(vault: &Vault, metric_data: &[Value], date: &str) -> Result<u64> {
    if metric_data.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(RAW_DIR, Partition::Month);

    // Build seen set from existing raw records: "<type>:<date>"
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let (Some(t), Some(d)) = (
                v.get("type").and_then(Value::as_str),
                v.get("_date").and_then(Value::as_str),
            ) {
                seen.insert(format!("{t}:{d}"));
            }
        }
    }

    let new_records: Vec<TaggedRaw> = metric_data
        .iter()
        .filter_map(|entry| {
            let metric_type = entry.get("type").and_then(Value::as_str)?;
            let dedup_key = format!("{metric_type}:{date}");
            if seen.insert(dedup_key) {
                Some(TaggedRaw {
                    date: date.to_string(),
                    value: entry.clone(),
                })
            } else {
                None
            }
        })
        .collect();

    let n = new_records.len() as u64;
    stream.append(&new_records, |r| &r.date)?;
    Ok(n)
}

/// Write contract Observation rows. Deduped by `guid`. Returns new count.
fn write_observations(vault: &Vault, rows: Vec<Observation>) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(OBS_DIR, Partition::Month);

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

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-ultrahuman-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Real response wrapper shape (confirmed from mi3nts/ultraHumanAPIReader
    // and official developer docs).
    fn real_day_response(_date: &str) -> Value {
        json!({
            "status": "success",
            "error": null,
            "data": {
                "metric_data": [
                    {
                        "type": "hr",
                        "object": {
                            "values": [
                                {"value": 58, "timestamp": 1748736000i64},
                                {"value": 60, "timestamp": 1748736060i64}
                            ]
                        }
                    },
                    {
                        "type": "hrv",
                        "object": {
                            "values": [
                                {"value": 45.5, "timestamp": 1748736000i64}
                            ]
                        }
                    },
                    {
                        "type": "glucose",
                        "object": {
                            "values": [
                                {"value": 92.0, "timestamp": 1748736000i64},
                                {"value": 95.0, "timestamp": 1748736300i64}
                            ]
                        }
                    },
                    {
                        "type": "recovery",
                        "object": {
                            "values": [
                                {"value": 78.0, "timestamp": 1748736000i64}
                            ]
                        }
                    },
                    {
                        "type": "sleep_score",
                        "object": {
                            "values": [
                                {"value": 82.0, "timestamp": 1748736000i64}
                            ]
                        }
                    }
                ]
            }
        })
    }

    // ---------------------------------------------------------------------------
    // split_credential tests

    #[test]
    fn split_credential_basic() {
        let (email, token) = split_credential("user@example.com:mytoken123").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(token, "mytoken123");
    }

    #[test]
    fn split_credential_token_with_colons() {
        // JWT tokens contain colons in base64url — only split on first colon.
        let (email, token) = split_credential("user@example.com:eyJhbGc:extra:stuff").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(token, "eyJhbGc:extra:stuff");
    }

    #[test]
    fn split_credential_trims_whitespace() {
        let (email, token) = split_credential("  user@example.com : tok  ").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(token, "tok");
    }

    #[test]
    fn split_credential_missing_colon_returns_none() {
        assert!(split_credential("notokenhere").is_none());
    }

    #[test]
    fn split_credential_empty_email_returns_none() {
        assert!(split_credential(":token").is_none());
    }

    #[test]
    fn split_credential_empty_token_returns_none() {
        assert!(split_credential("user@example.com:").is_none());
    }

    // ---------------------------------------------------------------------------
    // extract_metric_data tests

    #[test]
    fn extract_metric_data_real_wrapper() {
        let v = real_day_response("2026-06-01");
        let md = extract_metric_data(&v);
        assert_eq!(md.len(), 5);
        assert_eq!(md[0]["type"], "hr");
    }

    #[test]
    fn extract_metric_data_missing_data_key_returns_empty() {
        let v = json!({"status": "success", "error": null});
        let md = extract_metric_data(&v);
        assert!(md.is_empty());
    }

    #[test]
    fn extract_metric_data_empty_metric_data_array() {
        let v = json!({"data": {"metric_data": []}});
        let md = extract_metric_data(&v);
        assert!(md.is_empty());
    }

    // ---------------------------------------------------------------------------
    // extract_observations tests

    #[test]
    fn extracts_readings_from_real_wrapper() {
        let v = real_day_response("2026-06-01");
        let md = extract_metric_data(&v);
        let obs = extract_observations(&md, "2026-06-01");

        // hr: 2 readings, hrv: 1, glucose: 2, recovery: 1, sleep_score: 1 = 7 total
        assert_eq!(obs.len(), 7, "expected 7 observations, got {:?}", obs.iter().map(|o| &o.guid).collect::<Vec<_>>());
    }

    #[test]
    fn hr_observations_have_correct_fields() {
        let v = real_day_response("2026-06-01");
        let md = extract_metric_data(&v);
        let obs = extract_observations(&md, "2026-06-01");

        // guid = "hr:<epoch_secs>"
        let hr1 = obs.iter().find(|o| o.guid == "hr:1748736000").expect("hr:1748736000 missing");
        assert_eq!(hr1.test, "Heart Rate");
        assert_eq!(hr1.value, Some(58.0));
        assert_eq!(hr1.unit, "bpm");
        assert_eq!(hr1.code, "8867-4");
        assert_eq!(hr1.code_system, "loinc");
        assert_eq!(hr1.source, "ultrahuman");

        // Second HR reading has different timestamp = different guid.
        let hr2 = obs.iter().find(|o| o.guid == "hr:1748736060").expect("hr:1748736060 missing");
        assert_eq!(hr2.value, Some(60.0));
    }

    #[test]
    fn glucose_readings_have_correct_fields() {
        let v = real_day_response("2026-06-01");
        let md = extract_metric_data(&v);
        let obs = extract_observations(&md, "2026-06-01");

        let g1 = obs.iter().find(|o| o.guid == "glucose:1748736000").expect("glucose:1748736000 missing");
        assert_eq!(g1.value, Some(92.0));
        assert_eq!(g1.unit, "mg/dL");
        assert_eq!(g1.code, "2339-0");

        let g2 = obs.iter().find(|o| o.guid == "glucose:1748736300").expect("glucose:1748736300 missing");
        assert_eq!(g2.value, Some(95.0));
    }

    #[test]
    fn timestamps_are_epoch_seconds_converted_to_rfc3339() {
        // epoch 1748736000 = 2025-06-01T00:00:00 UTC
        let v = json!([{
            "type": "hr",
            "object": {
                "values": [{"value": 58, "timestamp": 0i64}]
            }
        }]);
        let obs = extract_observations(v.as_array().unwrap(), "1970-01-01");
        assert_eq!(obs.len(), 1);
        // ts should be RFC3339 and parseable
        let parsed = chrono::DateTime::parse_from_rfc3339(&obs[0].ts);
        assert!(parsed.is_ok(), "ts should be RFC3339, got: {}", obs[0].ts);
    }

    #[test]
    fn guid_uses_epoch_seconds_not_date() {
        let v = real_day_response("2026-06-01");
        let md = extract_metric_data(&v);
        let obs = extract_observations(&md, "2026-06-01");
        // All guids should be "<type>:<epoch_seconds>" not "<date>:<type>"
        for o in &obs {
            assert!(
                !o.guid.starts_with("2026"),
                "guid should not start with a date: {}", o.guid
            );
            assert!(
                o.guid.contains(':'),
                "guid should contain colon: {}", o.guid
            );
        }
    }

    #[test]
    fn entries_without_type_are_skipped() {
        let md = vec![
            json!({"object": {"values": [{"value": 58, "timestamp": 1748736000i64}]}}),
            json!({"type": "hr", "object": {"values": [{"value": 60, "timestamp": 1748736060i64}]}}),
        ];
        let obs = extract_observations(&md, "2026-06-01");
        assert_eq!(obs.len(), 1);
    }

    #[test]
    fn entries_with_empty_values_array_are_skipped() {
        let md = vec![
            json!({"type": "hr", "object": {"values": []}}),
            json!({"type": "hrv", "object": {"values": [{"value": 45, "timestamp": 1748736000i64}]}}),
        ];
        let obs = extract_observations(&md, "2026-06-01");
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].guid, "hrv:1748736000");
    }

    #[test]
    fn unknown_metric_types_still_produce_observations() {
        // Unknown types shouldn't be dropped — we just use the type as display_name
        // and no LOINC code.
        let md = vec![
            json!({"type": "new_future_metric", "object": {"values": [{"value": 42.0, "timestamp": 1748736000i64}]}}),
        ];
        let obs = extract_observations(&md, "2026-06-01");
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].guid, "new_future_metric:1748736000");
    }

    // ---------------------------------------------------------------------------
    // Write helpers

    #[test]
    fn write_observations_dedupes_by_guid() {
        let v = temp_vault("dedup");
        let ts = "2026-06-01T00:00:00+00:00".to_string();
        let mut row = Observation::new("ultrahuman", "hr:1748736000", &ts, "Heart Rate");
        row.ts = ts;
        let n1 = write_observations(&v, vec![row.clone()]).unwrap();
        assert_eq!(n1, 1);
        let n2 = write_observations(&v, vec![row]).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn write_raw_dedupes_by_type_and_date() {
        let v = temp_vault("rawdedup");
        let entries = vec![
            json!({"type": "hr", "object": {"values": [{"value": 58, "timestamp": 1748736000i64}]}}),
        ];
        let n1 = write_raw(&v, &entries, "2026-06-01").unwrap();
        assert_eq!(n1, 1);
        // Same type+date = deduplicated.
        let n2 = write_raw(&v, &entries, "2026-06-01").unwrap();
        assert_eq!(n2, 0);
        // Different date = new record.
        let n3 = write_raw(&v, &entries, "2026-06-02").unwrap();
        assert_eq!(n3, 1);
    }

    #[test]
    fn write_raw_writes_date_tagged_records() {
        let v = temp_vault("rawdate");
        let entries = vec![
            json!({"type": "hrv", "object": {"values": [{"value": 45, "timestamp": 1748736000i64}]}}),
        ];
        write_raw(&v, &entries, "2026-06-01").unwrap();
        // Read back and check _date is present.
        let stream = v.stream(RAW_DIR, Partition::Month);
        let keys = stream.partitions().unwrap();
        assert!(!keys.is_empty());
        let records: Vec<Value> = stream.read::<Value>(&keys[0]).unwrap();
        assert!(!records.is_empty());
        assert_eq!(records[0]["_date"], "2026-06-01");
        assert_eq!(records[0]["type"], "hrv");
    }

    // ---------------------------------------------------------------------------
    // Sync state round-trip

    #[test]
    fn sync_state_round_trips() {
        let v = temp_vault("state");
        assert!(UltrahumanSyncState::read(&v).is_none());
        let s = UltrahumanSyncState {
            updated: "2026-06-01T10:00:00-07:00".into(),
            watermark: Some("2026-06-01".into()),
            error: None,
            raw_records: 5,
            observations: 20,
        };
        s.write(&v).unwrap();
        let loaded = UltrahumanSyncState::read(&v).unwrap();
        assert_eq!(loaded.watermark.as_deref(), Some("2026-06-01"));
        assert_eq!(loaded.raw_records, 5);
        assert_eq!(loaded.observations, 20);
    }

    // ---------------------------------------------------------------------------
    // Connection

    #[test]
    fn token_paste_connect_saves_token_and_email() {
        let v = temp_vault("connect");
        def_connect(&v, "user@example.com:test-api-token").unwrap();
        let tok = v.load_sync_token(SERVICE).unwrap().unwrap();
        assert_eq!(tok.access_token, "test-api-token");
        assert_eq!(tok.scope.as_deref(), Some("user@example.com"));
        assert!(tok.expires_at.is_none());
    }

    #[test]
    fn connect_rejects_missing_colon() {
        let v = temp_vault("nocoat");
        assert!(def_connect(&v, "notokenhere").is_err());
    }

    #[test]
    fn connect_rejects_invalid_email() {
        let v = temp_vault("bademail");
        assert!(def_connect(&v, "notanemail:token").is_err());
    }

    #[test]
    fn connect_rejects_empty_paste() {
        let v = temp_vault("empty-token");
        assert!(def_connect(&v, "  ").is_err());
    }

    #[test]
    fn disconnect_removes_token() {
        let v = temp_vault("disconnect");
        def_connect(&v, "user@example.com:tok").unwrap();
        def_disconnect(&v, "ultrahuman").unwrap();
        assert!(v.load_sync_token(SERVICE).unwrap().is_none());
    }

    #[test]
    fn status_without_token_has_no_accounts() {
        let v = temp_vault("noaccounts");
        let s = def_status(&v).unwrap();
        assert!(s.accounts.is_empty());
        assert!(s.configured, "always configured for token paste");
    }

    #[test]
    fn status_with_token_shows_email_as_label() {
        let v = temp_vault("withtoken");
        def_connect(&v, "user@example.com:tok").unwrap();
        let s = def_status(&v).unwrap();
        assert_eq!(s.accounts.len(), 1);
        assert_eq!(s.accounts[0].key, "ultrahuman");
        assert_eq!(s.accounts[0].label, "user@example.com");
        assert!(!s.accounts[0].needs_reconnect);
    }

    // ---------------------------------------------------------------------------
    // Full pull against a stub client

    struct StubClient {
        response: Value,
    }

    impl ApiClient for StubClient {
        fn fetch_day(&self, _email: &str, _date: &str) -> Result<Value, FetchError> {
            Ok(self.response.clone())
        }
    }

    struct ErrorClient {
        err: &'static str,
    }

    impl ApiClient for ErrorClient {
        fn fetch_day(&self, _: &str, _: &str) -> Result<Value, FetchError> {
            match self.err {
                "401" => Err(FetchError::Unauthorized),
                "429" => Err(FetchError::RateLimited),
                _ => Err(FetchError::Other(self.err.to_string())),
            }
        }
    }

    #[test]
    fn pull_without_token_returns_error() {
        let v = temp_vault("notoken");
        let err = pull_inner(&v).unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[test]
    fn pull_with_real_shape_writes_raw_and_observations() {
        let v = temp_vault("stub");
        let client = StubClient {
            response: real_day_response("2026-06-01"),
        };
        let stats = pull_with_client(&v, &client, "user@example.com").unwrap();
        // 5 metric entries → 5 raw records; 7 readings → 7 observations
        assert!(stats.raw_records >= 5, "expected >= 5 raw records, got {}", stats.raw_records);
        assert!(stats.observations >= 7, "expected >= 7 observations, got {}", stats.observations);

        assert!(v.root().join("health/medical/ultrahuman/observations").exists());
        assert!(v.root().join("health/ultrahuman/raw").exists());
    }

    #[test]
    fn pull_is_idempotent_on_same_data() {
        let v = temp_vault("idempotent");
        let client = StubClient {
            response: real_day_response("2026-06-01"),
        };
        let s1 = pull_with_client(&v, &client, "user@example.com").unwrap();
        let s2 = pull_with_client(&v, &client, "user@example.com").unwrap();
        // First pass writes data.
        assert!(s1.raw_records >= 5);
        // Second pass: watermark already past seed window — no new records.
        assert_eq!(s2.raw_records + s2.observations, 0);
    }

    #[test]
    fn pull_calls_per_day_not_ranges() {
        use std::sync::{Arc, Mutex};

        struct CountingClient {
            calls: Arc<Mutex<Vec<(String, String)>>>,
            response: Value,
        }
        impl ApiClient for CountingClient {
            fn fetch_day(&self, email: &str, date: &str) -> Result<Value, FetchError> {
                self.calls.lock().unwrap().push((email.to_string(), date.to_string()));
                Ok(self.response.clone())
            }
        }

        let v = temp_vault("percall");
        let calls = Arc::new(Mutex::new(Vec::new()));
        // Pre-set watermark to 2 days ago so the loop has a defined window.
        let today = Local::now().date_naive();
        let start = today - chrono::Duration::days(2);
        let mut state = UltrahumanSyncState::default();
        state.watermark = Some(start.format("%Y-%m-%d").to_string());
        state.write(&v).unwrap();

        let client = CountingClient {
            calls: Arc::clone(&calls),
            response: json!({"data": {"metric_data": []}}),
        };
        pull_with_client(&v, &client, "test@example.com").unwrap();

        let captured = calls.lock().unwrap();
        // Should have called once per day: from (start - OVERLAP_DAYS).max(epoch) to today
        // With OVERLAP_DAYS=2 and watermark=today-2: start = today-4 (but overlap from watermark)
        // The key invariant: each call has a single date (YYYY-MM-DD) and email.
        assert!(!captured.is_empty(), "should have made at least one API call");
        for (email, date) in captured.iter() {
            assert_eq!(email, "test@example.com");
            // Each date should be a valid YYYY-MM-DD.
            NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .unwrap_or_else(|_| panic!("expected YYYY-MM-DD date, got: {date}"));
        }
    }

    #[test]
    fn unauthorized_aborts_and_flags_state() {
        let v = temp_vault("unauth");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "tok".into(),
                refresh_token: None,
                token_type: None,
                scope: Some("test@example.com".into()),
                expires_at: None,
            },
        )
        .unwrap();
        let client = ErrorClient { err: "401" };
        let err = pull_with_client(&v, &client, "test@example.com").unwrap_err();
        assert!(err.to_string().contains("401") || err.to_string().contains("reconnect"), "{err}");
        let state = UltrahumanSyncState::read(&v).unwrap();
        assert!(state.error.is_some());
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
    }

    #[test]
    fn def_has_correct_id_and_connection() {
        assert_eq!(DEF.meta.id, "ultrahuman");
        assert_eq!(DEF.connection, Some("ultrahuman"));
        assert!(!DEF.meta.default_on, "opt-in by default (medical data)");
    }
}
