//! Polar fitness watches and HR monitors via the AccessLink v4 cloud API.
//! Brief: docs/integrations/polar.md. Periodic poller (hourly) + Import hook
//! for bulk-export ZIPs. Dedicated OAuth connection ("polar") — not the shared
//! Google login. No FIT parsing in this module; raw JSON sessions only.
//!
//! ## What lands in the vault
//!
//! Raw-only layout under `health/polar/` (no bound contract yet — the
//! per-metric contract layer is a future Phase-4 pioneer step):
//!
//! - `health/polar/sessions/YYYY-MM.jsonl` — training session summaries (one
//!   per session by `id`; deduped).
//! - `health/polar/sleep/YYYY-MM.jsonl` — sleep records (one per night by
//!   `sleepDate`; deduped).
//! - `health/polar/recharge/YYYY-MM.jsonl` — Nightly Recharge results (one per
//!   night by `sleepResultDate`; deduped).
//! - `health/polar/activity/YYYY-MM.jsonl` — daily activity summaries (one per
//!   day by `date`; deduped).
//! - `health/polar/imports/` — FIT/TCX/GPX/CSV files dropped by the user.
//!
//! All four API collections share one incremental cursor in
//! `.trove/polar-sync.json` (non-secret; rebuildable by rewriting from
//! output files). The cursor stores the latest `from` date used for each
//! collection — advancing only after that collection's write commits.
//!
//! ## Auth (OAuth 2.0, confidential client via Basic auth)
//!
//! Polar uses HTTP Basic auth (base64(client_id:client_secret)) for the token
//! endpoint. The provider flag `basic_auth: true` tells the oauth module to
//! issue the token request with a Basic Authorization header rather than POST
//! body params (same as Strava, different from Dexcom). Credentials from
//! `TROVE_POLAR_CLIENT_ID` / `TROVE_POLAR_CLIENT_SECRET` (empty baked
//! default). Developer registration at admin.polaraccesslink.com (self-service,
//! requires a Polar Flow account — a Needs-login flag).
//!
//! ## Scopes requested
//!
//! `activity:read sleep:read nightly_recharge:read training_sessions:read`
//! (the four data collections we poll — narrowest required set).
//!
//! ## Cursor / windowing
//!
//! The AccessLink v4 list endpoints require `from` (inclusive) and `to`
//! (exclusive) query parameters (ISO 8601 date, `YYYY-MM-DD`). We store the
//! watermark as the last date we successfully wrote data through, and query
//! [watermark, today] on each sync. Cold start: begin from 90 days ago (a
//! sensible seed window). Id-based dedupe makes repeated fetches of overlapping
//! windows safe.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    ImportOutcome, IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::{self, AppCredentials, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const API_BASE: &str = "https://www.polaraccesslink.com/v4/data";

const DIR_SESSIONS: &str = "health/polar/sessions";
const DIR_SLEEP: &str = "health/polar/sleep";
const DIR_RECHARGE: &str = "health/polar/recharge";
const DIR_ACTIVITY: &str = "health/polar/activity";

const SYNC_FILE: &str = ".trove/polar-sync.json";
const SERVICE: &str = "polar";

/// Seconds between syncs. Hourly: Polar data lands after the device syncs
/// through the Flow app (minutes to hours of lag).
pub const POLAR_SYNC_SECS: u64 = 3600;

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How far back a cold-start sync walks (Polar's calendar endpoint limit is
/// 90 days; we use the same for all collections as a sensible seed window).
const COLD_START_DAYS: i64 = 90;

// ---------------------------------------------------------------------------
// OAuth provider

/// Polar uses HTTP Basic auth at the token endpoint (client_id:client_secret
/// base64-encoded in the Authorization header), not POST body params.
pub static POLAR: Provider = Provider {
    service: SERVICE,
    display_name: "Polar",
    auth_url: "https://auth.polar.com/oauth/authorize",
    token_url: "https://auth.polar.com/oauth/token",
    // Narrowest set of scopes for the four collections we poll.
    scopes: "activity:read sleep:read nightly_recharge:read training_sessions:read",
    // Production redirect port: 38580 + 255 = 38835.
    redirect_port: 38835,
    use_pkce: false,
    // Polar token endpoint requires Basic auth (base64(id:secret)), not body.
    basic_auth: true,
    default_client_id: option_env!("TROVE_POLAR_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_POLAR_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Cursor

/// Sync state persisted in `.trove/polar-sync.json`. One watermark per
/// collection (the ISO date used as the `from` param of the last successful
/// request — the next sync queries `[watermark, today]`).
/// Non-secret; rebuildable by scanning output files.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// `from` date last used for the sessions collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sessions_through: Option<String>,
    /// `from` date last used for the sleep collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sleep_through: Option<String>,
    /// `from` date last used for the nightly recharge collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recharge_through: Option<String>,
    /// `from` date last used for the daily activity collection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activity_through: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_polar_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_polar_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable trait so tests run fully offline.

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

/// The four collection endpoints the pull needs.
/// `from` and `to` are ISO 8601 dates (`YYYY-MM-DD`); `from` is inclusive,
/// `to` is exclusive. Both are required by the v4 API.
trait PolarApi {
    /// `GET /v4/data/training-sessions/list?from=&to=` — list of sessions.
    fn training_sessions(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError>;
    /// `GET /v4/data/sleeps?from=&to=` — list of sleep records.
    fn sleeps(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError>;
    /// `GET /v4/data/nightly-recharge-results?from=&to=` — recovery.
    fn recharge(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError>;
    /// `GET /v4/data/activity/list?from=&to=` — daily activity.
    fn activity(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError>;
}

struct PolarClient {
    base: String,
}

impl PolarClient {
    fn new(base: String) -> Self {
        PolarClient { base }
    }

    fn get(&self, path: &str, token: &str, from: &str, to: &str) -> Result<Value, FetchError> {
        let url = format!("{}{}?from={}&to={}", self.base, path, from, to);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/json")
            .call()
        {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing response: {e}"))),
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

impl PolarApi for PolarClient {
    fn training_sessions(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError> {
        self.get("/training-sessions/list", token, from, to)
    }

    fn sleeps(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError> {
        self.get("/sleeps", token, from, to)
    }

    fn recharge(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError> {
        self.get("/nightly-recharge-results", token, from, to)
    }

    fn activity(&self, token: &str, from: &str, to: &str) -> Result<Value, FetchError> {
        self.get("/activity/list", token, from, to)
    }
}

// ---------------------------------------------------------------------------
// Response parsing helpers

/// Extract an array from a response. Polar v4 wraps collections under
/// endpoint-specific top-level keys:
/// - training sessions: `trainingSessions`
/// - sleep: `nightSleeps`
/// - recharge: `nightlyRechargeResults`
/// - activity: `activities` → `activityDays` (two-level nesting)
///
/// `keys` is a path: each element descends one level. A two-element path like
/// `&["activities", "activityDays"]` walks `v["activities"]["activityDays"]`.
/// A single-element path like `&["trainingSessions"]` looks up one level.
/// Falls back to bare JSON array for defensive coverage.
fn extract_array(v: &Value, keys: &[&str]) -> Vec<Value> {
    // Bare array short-circuit: if the root value is already an array, return
    // it directly without attempting key descent (defensive, handles raw exports).
    if let Value::Array(arr) = v {
        return arr.clone();
    }
    // Walk the key path one level at a time.
    let mut cur = v;
    for &key in keys {
        match cur.get(key) {
            Some(Value::Array(arr)) => return arr.clone(),
            Some(next) => cur = next,
            None => return Vec::new(),
        }
    }
    Vec::new()
}

/// Pull a string field from a JSON object. Returns "" when missing or not a string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Guess a date string (`YYYY-MM-DD`) from a record for partitioning by month.
/// Sessions have `startTime` (ISO 8601 datetime), sleep/recharge have a date
/// field, activity has `date`. Returns `None` when nothing parseable is found.
fn record_date(v: &Value, date_keys: &[&str]) -> Option<String> {
    for &key in date_keys {
        let s = str_field(v, key);
        if s.len() >= 10 && s.chars().nth(4) == Some('-') {
            // Accept the first 10 chars as YYYY-MM-DD.
            return Some(s[..10].to_string());
        }
    }
    None
}

/// Build a fake RFC3339 ts for the store partition from a date string
/// (`YYYY-MM-DD`). Appended as `T00:00:00+00:00` so `Partition::Month` can
/// parse the `YYYY-MM` prefix (7 chars).
fn date_to_ts(date: &str) -> String {
    format!("{date}T00:00:00+00:00")
}

// ---------------------------------------------------------------------------
// Write layer

/// A raw line tagged with a ts for the store partition helper.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Dedup + append to a JSONL stream partitioned by month.
/// `id_key` is the unique-id field in each record (for dedupe).
/// `date_keys` are tried in order to extract the record's date for partitioning.
/// Returns the count of newly written records.
fn write_raw_collection(
    vault: &Vault,
    dir: &str,
    records: Vec<Value>,
    id_key: &str,
    date_keys: &[&str],
) -> Result<u64> {
    let stream = vault.stream(dir, Partition::Month);

    // Collect existing ids.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            let id = match v.get(id_key) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => continue,
            };
            if !id.is_empty() {
                seen.insert(id);
            }
        }
    }

    let mut new_lines: Vec<RawLine> = Vec::new();
    for r in records {
        let id = match r.get(id_key) {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => continue, // no id → skip
        };
        if id.is_empty() || !seen.insert(id) {
            continue; // already stored
        }
        let date = record_date(&r, date_keys).unwrap_or_else(|| {
            // Fallback: today's date — partition is approximate but at least
            // the record lands in the vault.
            Local::now().format("%Y-%m-%d").to_string()
        });
        new_lines.push(RawLine { ts: date_to_ts(&date), value: r });
    }

    stream.append(&new_lines, |l| &l.ts)?;
    Ok(new_lines.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull

/// Cold-start date: `COLD_START_DAYS` days ago as `YYYY-MM-DD`.
fn cold_start_date() -> String {
    let d = (Utc::now() - chrono::Duration::days(COLD_START_DAYS)).date_naive();
    d.format("%Y-%m-%d").to_string()
}

/// Resolve a fresh access token (refresh if expired). Mirrors dexcom.rs.
fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(POLAR.service)?
        .or_else(|| POLAR.default_credentials())
        .context("Polar token expired and no app credentials to refresh it — reconnect")?;
    match oauth::refresh_token(&POLAR, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(POLAR.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(POLAR.service)?;
            bail!("Polar token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

/// Map a [`FetchError`] into an anyhow error with a reconnect hint on 401.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Polar rejected the token (401) on the {endpoint} endpoint — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Polar rate limited the {endpoint} endpoint (429) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Polar {endpoint} fetch failed: {other}"),
    }
}

/// The testable pull body — `api` is injectable so tests stay fully offline.
///
/// Each collection queries the range `[from, today]` (from=watermark or cold-
/// start date; to=today exclusive, i.e. tomorrow). The watermark is only
/// advanced after the fetch+write succeeds, so a partial sync never silently
/// swallows records.
///
/// Training sessions: the v4 item's unique id lives at `identifier.id` (a
/// nested object), not a top-level `id` field. We extract it before dedup.
fn pull_with(vault: &Vault, api: &impl PolarApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_polar_sync();
    // `to` is exclusive — use tomorrow so today's data is included.
    let today = Local::now().format("%Y-%m-%d").to_string();
    let tomorrow = (Local::now() + chrono::Duration::days(1)).format("%Y-%m-%d").to_string();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Sessions — wrapper: trainingSessions (single-level).
    // Each item's unique id is nested at item["identifier"]["id"].
    // We flatten it to a top-level `id` field before writing so dedup works.
    {
        let from = state.sessions_through.clone().unwrap_or_else(cold_start_date);
        let resp = api
            .training_sessions(token, &from, &tomorrow)
            .map_err(|e| fetch_err("training-sessions", e))?;
        let raw_records = extract_array(&resp, &["trainingSessions"]);
        // Promote identifier.id -> id so the dedup id_key ("id") resolves.
        let records: Vec<Value> = raw_records
            .into_iter()
            .map(|mut item| {
                if item.get("id").is_none() {
                    if let Some(id_val) = item
                        .get("identifier")
                        .and_then(|idf| idf.get("id"))
                        .and_then(Value::as_str)
                        .map(|s| Value::String(s.to_string()))
                    {
                        item["id"] = id_val;
                    }
                }
                item
            })
            .collect();
        let n = write_raw_collection(vault, DIR_SESSIONS, records, "id", &["startTime", "date"])?;
        counts.insert("sessions", n);
        state.sessions_through = Some(today.clone());
    }

    // Sleep — wrapper: nightSleeps (single-level).
    {
        let from = state.sleep_through.clone().unwrap_or_else(cold_start_date);
        let resp = api.sleeps(token, &from, &tomorrow).map_err(|e| fetch_err("sleeps", e))?;
        let records = extract_array(&resp, &["nightSleeps"]);
        let n = write_raw_collection(vault, DIR_SLEEP, records, "sleepDate", &["sleepDate"])?;
        counts.insert("sleep", n);
        state.sleep_through = Some(today.clone());
    }

    // Nightly Recharge — wrapper: nightlyRechargeResults (single-level).
    {
        let from = state.recharge_through.clone().unwrap_or_else(cold_start_date);
        let resp =
            api.recharge(token, &from, &tomorrow).map_err(|e| fetch_err("nightly-recharge", e))?;
        let records = extract_array(&resp, &["nightlyRechargeResults"]);
        let n = write_raw_collection(
            vault,
            DIR_RECHARGE,
            records,
            "sleepResultDate",
            &["sleepResultDate"],
        )?;
        counts.insert("recharge", n);
        state.recharge_through = Some(today.clone());
    }

    // Daily Activity — wrapper: activities -> activityDays (two-level nesting).
    {
        let from = state.activity_through.clone().unwrap_or_else(cold_start_date);
        let resp =
            api.activity(token, &from, &tomorrow).map_err(|e| fetch_err("activity", e))?;
        // v4 response: {"activities": {"activityDays": [...]}}
        let records = extract_array(&resp, &["activities", "activityDays"]);
        let n = write_raw_collection(vault, DIR_ACTIVITY, records, "date", &["date"])?;
        counts.insert("activity", n);
        state.activity_through = Some(today.clone());
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_polar_sync(&state)?;

    let total: u64 = counts.values().sum();
    let headline = if total == 0 {
        "Polar is up to date — no new records".to_string()
    } else {
        format!("Polar synced — {total} new records")
    };
    Ok(PullOutcome { headline, counts })
}

/// Public pull entry point: resolve + refresh token, then run.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Polar is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = PolarClient::new(API_BASE.to_string());
    pull_with(vault, &client, &token.access_token)
}

// ---------------------------------------------------------------------------
// Import hook (FIT/TCX/GPX/CSV file drops — raw copy into imports/)

/// Copy a dropped file into `health/polar/imports/<filename>` verbatim.
/// FIT/TCX/GPX session files are kept in original format for future parsing.
/// Copy a dropped file (FIT/TCX/GPX/CSV/ZIP session export) verbatim into
/// `health/polar/imports/<filename>`. Exposed as `pub(crate)` so the hub can
/// wire it via the generic `run_import` command; the primary `DEF` behavior is
/// `Periodic` (the OAuth pull), mirroring the WHOOP pattern.
#[allow(dead_code)] // future hub import box will call this
pub(crate) fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(crate::health::ImportProgress),
) -> Result<ImportOutcome> {
    use std::fs;

    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("import path has no filename")?;

    progress(crate::health::ImportProgress { records: 0, percent: 0.0 });

    let dest_dir = vault.resolve("health/polar/imports")?;
    fs::create_dir_all(&dest_dir)?;
    let dest = dest_dir.join(filename);
    fs::copy(path, &dest).with_context(|| format!("copying {filename} to vault"))?;

    progress(crate::health::ImportProgress { records: 1, percent: 100.0 });

    let mut counts = BTreeMap::new();
    counts.insert("files", 1u64);
    Ok(ImportOutcome { headline: format!("{filename} copied to health/polar/imports/"), counts })
}

// ---------------------------------------------------------------------------
// Connection (OAuth)

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(POLAR.service)?.is_some()
        || POLAR.default_credentials().is_some();
    let accounts = match vault.load_sync_token(POLAR.service)? {
        Some(token) => vec![ConnectedAccount {
            key: POLAR.service.to_string(),
            label: POLAR.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
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

/// Interactive connect: opens the consent page, waits for the redirect, saves
/// the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(POLAR.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(POLAR.service)?
            .or_else(|| POLAR.default_credentials())
            .context(
                "no Polar app credentials — register an app at admin.polaraccesslink.com and \
                 enter its id/secret in the Integrations tab",
            )?,
    };
    let flow = oauth::OauthFlow::start(&POLAR, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(POLAR.service, &token)?;
    Ok(token)
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "polar",
    display_name: "Polar",
    methods: &[ConnectMethod::OAuth {
        provider: &POLAR,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["polar"],
    setup: &[
        "Register a developer application at admin.polaraccesslink.com (requires a Polar Flow \
         account — no commercial approval).",
        "Set its OAuth redirect URI to http://localhost:38835/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved locally, so every \
         future connect is just a login.",
    ],
};

// ---------------------------------------------------------------------------
// DEF hooks

fn def_last_data(vault: &Vault) -> Option<String> {
    // Report the most recent partition in any of the four collections.
    [DIR_SESSIONS, DIR_SLEEP, DIR_RECHARGE, DIR_ACTIVITY]
        .iter()
        .filter_map(|&dir| crate::registry::newest_stem(&vault.root().join(dir)))
        .max()
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("polar synced — {total} new records")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("polar sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "polar",
        name: "Polar",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Pull training sessions, heart-rate, sleep, Nightly Recharge (ANS recovery), and \
             daily activity from Polar Flow via AccessLink v4. Nightly Recharge and Training Load \
             scores are API-only and absent from bulk exports.",
        domain: "health",
        vault_path: "health/polar/",
        toggleable: true,
        setup: &[
            "Connect your Polar account via the Polar connection card — you'll register a free \
             developer app at admin.polaraccesslink.com first.",
            "First sync seeds the last 90 days; later syncs are incremental (hourly).",
            "Optional: drop individual FIT/TCX/GPX/CSV session files or the bulk-export ZIP via \
             the import box for historical backfill.",
        ],
        caveats:
            "Nightly Recharge, Training Load Pro, and Cardio Load scores are API-only (not in \
             the bulk export ZIP). Developer registration requires a Polar Flow account; the app \
             client ID and secret must be registered at admin.polaraccesslink.com.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(POLAR_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("polar"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-polar-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures derived from the AccessLink v4 swagger schema (polar-api-v4)
    //
    // Training session items: id is at item["identifier"]["id"] (a nested
    // TrainingSessionReference object); pull_with promotes it to top-level
    // "id" before writing.  Response wrapper: {"trainingSessions": [...]}.
    //
    // Activity response is double-nested: {"activities": {"activityDays": [...]}}.
    // Sleep and recharge are single-level.

    /// One training session (v4 `trainingsessionTrainingSession` shape).
    /// The unique id is under `identifier.id` — not a top-level `id` field.
    fn session_1() -> Value {
        json!({
            "identifier": {"id": "polar-session-001"},
            "startTime": "2026-06-10T07:30:00+02:00",
            "stopTime": "2026-06-10T08:45:00+02:00",
            "durationMillis": 4500000,
            "name": "Morning Run",
            "sport": {"id": "RUNNING"},
            "deviceInfo": {"deviceId": "abc123"}
        })
    }

    /// A second training session on the next day.
    fn session_2() -> Value {
        json!({
            "identifier": {"id": "polar-session-002"},
            "startTime": "2026-06-11T07:00:00+02:00",
            "stopTime": "2026-06-11T07:50:00+02:00",
            "durationMillis": 3000000,
            "name": "Cycling",
            "sport": {"id": "CYCLING"},
            "deviceInfo": {"deviceId": "abc123"}
        })
    }

    /// Sleep record (v4 `sleepListSleepsResponse` -> `nightSleeps` item).
    fn sleep_1() -> Value {
        json!({
            "sleepDate": "2026-06-10",
            "sleepResult": {
                "hypnogram": {
                    "sleepStart": "2026-06-09T23:12:33.435+02:00",
                    "sleepEnd": "2026-06-10T07:12:33.435+02:00"
                },
                "sleepCycles": [
                    {"secondsFromSleepStart": 0, "sleepDepthAtCycleStart": 2}
                ]
            },
            "sleepEvaluation": {
                "analysis": {
                    "efficiencyPercent": 91,
                    "continuityIndex": 4
                }
            }
        })
    }

    /// Nightly Recharge record (v4 `nightlyrechargeResult` shape).
    fn recharge_1() -> Value {
        json!({
            "sleepResultDate": "2026-06-10",
            "ansStatus": -15.7,
            "recoveryIndicator": 3,
            "meanNightlyRecoveryRri": 58.3,
            "meanNightlyRecoveryRespirationInterval": 4200.0,
            "hrvSamples": []
        })
    }

    /// Daily activity record (v4 `activityActivityDay` shape).
    fn activity_1() -> Value {
        json!({
            "date": "2026-06-10",
            "activitiesPerDevice": [
                {
                    "deviceReference": {"deviceId": "abc123"},
                    "activitySamples": [
                        {"stepSamples": {"startTime": "07:00:00", "steps": [120, 80, 95]}}
                    ]
                }
            ]
        })
    }

    /// v4 response: {"trainingSessions": [...]}
    fn sessions_response(items: Vec<Value>) -> Value {
        json!({"trainingSessions": items})
    }

    /// v4 response: {"nightSleeps": [...]}
    fn sleeps_response(items: Vec<Value>) -> Value {
        json!({"nightSleeps": items})
    }

    /// v4 response: {"nightlyRechargeResults": [...]}  (single-level)
    fn recharge_response(items: Vec<Value>) -> Value {
        json!({"nightlyRechargeResults": items})
    }

    /// v4 response: {"activities": {"activityDays": [...]}}  (two-level)
    fn activity_response(items: Vec<Value>) -> Value {
        json!({"activities": {"activityDays": items}})
    }

    // -----------------------------------------------------------------------
    // Mock API

    struct MockApi {
        sessions: RefCell<Vec<Value>>,
        sleep: RefCell<Vec<Value>>,
        recharge: RefCell<Vec<Value>>,
        activity: RefCell<Vec<Value>>,
    }

    impl MockApi {
        fn new(
            sessions: Vec<Value>,
            sleep: Vec<Value>,
            recharge: Vec<Value>,
            activity: Vec<Value>,
        ) -> Self {
            MockApi {
                sessions: RefCell::new(sessions),
                sleep: RefCell::new(sleep),
                recharge: RefCell::new(recharge),
                activity: RefCell::new(activity),
            }
        }
    }

    impl PolarApi for MockApi {
        fn training_sessions(&self, _t: &str, _from: &str, _to: &str) -> Result<Value, FetchError> {
            Ok(sessions_response(self.sessions.borrow().clone()))
        }
        fn sleeps(&self, _t: &str, _from: &str, _to: &str) -> Result<Value, FetchError> {
            Ok(sleeps_response(self.sleep.borrow().clone()))
        }
        fn recharge(&self, _t: &str, _from: &str, _to: &str) -> Result<Value, FetchError> {
            Ok(recharge_response(self.recharge.borrow().clone()))
        }
        fn activity(&self, _t: &str, _from: &str, _to: &str) -> Result<Value, FetchError> {
            Ok(activity_response(self.activity.borrow().clone()))
        }
    }

    // -----------------------------------------------------------------------
    // extract_array

    #[test]
    fn extract_array_finds_single_level_key() {
        // Training sessions: {"trainingSessions": [...]}  (single-level)
        let r = sessions_response(vec![session_1()]);
        let arr = extract_array(&r, &["trainingSessions"]);
        assert_eq!(arr.len(), 1);
        // id is at identifier.id in the raw item (before pull_with promotes it).
        assert_eq!(arr[0]["identifier"]["id"], "polar-session-001");
    }

    #[test]
    fn extract_array_finds_two_level_path() {
        // Activity: {"activities": {"activityDays": [...]}}  (two-level)
        let r = activity_response(vec![activity_1()]);
        let arr = extract_array(&r, &["activities", "activityDays"]);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["date"], "2026-06-10");
    }

    #[test]
    fn extract_array_bare_array_fallback() {
        let r = json!([session_1(), session_2()]);
        let arr = extract_array(&r, &["trainingSessions"]);
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn extract_array_missing_key_returns_empty() {
        let r = json!({"foo": "bar"});
        assert!(extract_array(&r, &["trainingSessions"]).is_empty());
    }

    #[test]
    fn extract_array_recharge_single_level() {
        // Recharge is single-level — NOT double-nested.
        let r = recharge_response(vec![recharge_1()]);
        let arr = extract_array(&r, &["nightlyRechargeResults"]);
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["sleepResultDate"], "2026-06-10");
    }

    // -----------------------------------------------------------------------
    // record_date

    #[test]
    fn record_date_reads_starttime_prefix() {
        let s = session_1();
        assert_eq!(record_date(&s, &["startTime"]).as_deref(), Some("2026-06-10"));
    }

    #[test]
    fn record_date_reads_date_field() {
        let a = activity_1();
        assert_eq!(record_date(&a, &["date"]).as_deref(), Some("2026-06-10"));
    }

    #[test]
    fn record_date_reads_sleep_date() {
        let sl = sleep_1();
        assert_eq!(record_date(&sl, &["sleepDate"]).as_deref(), Some("2026-06-10"));
    }

    #[test]
    fn record_date_reads_recharge_date() {
        let r = recharge_1();
        assert_eq!(record_date(&r, &["sleepResultDate"]).as_deref(), Some("2026-06-10"));
    }

    // -----------------------------------------------------------------------
    // Full pull: writes + dedupes all four collections

    #[test]
    fn full_pull_writes_all_four_collections() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(
            vec![session_1(), session_2()],
            vec![sleep_1()],
            vec![recharge_1()],
            vec![activity_1()],
        );

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("sessions"), Some(&2));
        assert_eq!(out.counts.get("sleep"), Some(&1));
        assert_eq!(out.counts.get("recharge"), Some(&1));
        assert_eq!(out.counts.get("activity"), Some(&1));

        // Sessions partitions exist.
        let sess_dir = v.stream(DIR_SESSIONS, Partition::Month);
        let partitions = sess_dir.partitions().unwrap();
        assert!(!partitions.is_empty(), "sessions partition written");
        let sess_rows: Vec<Value> = partitions
            .iter()
            .flat_map(|k| sess_dir.read::<Value>(k).unwrap())
            .collect();
        assert_eq!(sess_rows.len(), 2);
        assert!(sess_rows.iter().any(|r| r["id"] == "polar-session-001"));
        assert!(sess_rows.iter().any(|r| r["id"] == "polar-session-002"));

        // Sleep partition.
        let sleep_dir = v.stream(DIR_SLEEP, Partition::Month);
        let sleep_rows: Vec<Value> = sleep_dir
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| sleep_dir.read::<Value>(k).unwrap())
            .collect();
        assert_eq!(sleep_rows.len(), 1);
        assert_eq!(sleep_rows[0]["sleepDate"], "2026-06-10");

        // Recharge partition.
        let rch_dir = v.stream(DIR_RECHARGE, Partition::Month);
        let rch_rows: Vec<Value> = rch_dir
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| rch_dir.read::<Value>(k).unwrap())
            .collect();
        assert_eq!(rch_rows.len(), 1);
        assert_eq!(rch_rows[0]["ansStatus"], -15.7);
        assert_eq!(rch_rows[0]["recoveryIndicator"], 3);

        // Activity partition.
        let act_dir = v.stream(DIR_ACTIVITY, Partition::Month);
        let act_rows: Vec<Value> = act_dir
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| act_dir.read::<Value>(k).unwrap())
            .collect();
        assert_eq!(act_rows.len(), 1);
        assert_eq!(act_rows[0]["date"], "2026-06-10");

        // Cursor written, no token in it.
        let state = v.read_polar_sync();
        assert!(state.sessions_through.is_some());
        assert!(state.sleep_through.is_some());
        assert!(state.recharge_through.is_some());
        assert!(state.activity_through.is_some());
        assert!(state.updated.is_some());

        let cursor = std::fs::read_to_string(v.root().join(".trove/polar-sync.json")).unwrap();
        assert!(!cursor.contains("tok"), "token never in the cursor");
    }

    #[test]
    fn second_pull_deduplicates_records() {
        let v = temp_vault("dedup");
        let out1 = pull_with(
            &v,
            &MockApi::new(vec![session_1()], vec![sleep_1()], vec![recharge_1()], vec![activity_1()]),
            "tok",
        )
        .unwrap();
        assert_eq!(out1.counts.get("sessions"), Some(&1));

        // Second pull with the same data → zero new records.
        let out2 = pull_with(
            &v,
            &MockApi::new(vec![session_1()], vec![sleep_1()], vec![recharge_1()], vec![activity_1()]),
            "tok",
        )
        .unwrap();
        assert_eq!(out2.counts.get("sessions"), Some(&0), "session already stored");
        assert_eq!(out2.counts.get("sleep"), Some(&0), "sleep already stored");
        assert_eq!(out2.counts.get("recharge"), Some(&0), "recharge already stored");
        assert_eq!(out2.counts.get("activity"), Some(&0), "activity already stored");

        // No duplicate rows.
        let sess_dir = v.stream(DIR_SESSIONS, Partition::Month);
        let rows: Vec<Value> = sess_dir
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| sess_dir.read::<Value>(k).unwrap())
            .collect();
        assert_eq!(rows.len(), 1, "no duplicate rows");
    }

    #[test]
    fn empty_response_is_a_noop() {
        let v = temp_vault("empty");
        let api = MockApi::new(vec![], vec![], vec![], vec![]);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.values().sum::<u64>(), 0);
        assert!(out.headline.contains("up to date"));
    }

    #[test]
    fn record_without_id_is_skipped() {
        // A training session with no `identifier.id` cannot be deduped → skipped.
        // pull_with tries to promote identifier.id → id; if absent, the record
        // is dropped by write_raw_collection (no id_key to dedup by).
        let v = temp_vault("noid");
        let no_id = json!({"startTime": "2026-06-10T08:00:00+02:00", "sport": {"id": "RUNNING"}});
        let api = MockApi::new(vec![no_id], vec![], vec![], vec![]);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("sessions"), Some(&0), "keyless record skipped");
    }

    #[test]
    fn pull_without_token_errors_cleanly() {
        let v = temp_vault("notok");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        // Empty cursor deserializes to all-None (first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.sessions_through.is_none());
        assert!(empty.updated.is_none());
        // Forward-compat: unknown future field is tolerated.
        let fwd: SyncState = serde_json::from_str(
            r#"{"sessions_through":"2026-06-10","future_field":"x"}"#,
        )
        .unwrap();
        assert_eq!(fwd.sessions_through.as_deref(), Some("2026-06-10"));
    }

    #[test]
    fn connection_and_def_are_wired() {
        assert_eq!(DEF.connection, Some("polar"));
        assert_eq!(CONNECTION.id, "polar");
        assert_eq!(DEF.meta.id, "polar");
        // Production redirect port: 38580 + 255 = 38835.
        assert_eq!(POLAR.redirect_port, 38835, "38580 + 255 = 38835");
        assert_eq!(POLAR.redirect_uri(), "http://localhost:38835/callback");
        assert!(CONNECTION.method("oauth").is_some());
        // Polar token endpoint uses HTTP Basic auth (not POST body).
        assert!(POLAR.basic_auth, "Polar uses HTTP Basic auth at the token endpoint");
    }

    #[test]
    fn connection_stores_token_and_status_reflects_it() {
        let v = temp_vault("conn");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "polar_access_abc".into(),
                refresh_token: Some("polar_refresh_xyz".into()),
                token_type: Some("Bearer".into()),
                scope: Some("activity:read sleep:read".into()),
                expires_at: Some(1_900_000_000),
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Polar");
        assert!(!status.accounts[0].needs_reconnect, "live token");

        // Token not in cursor.
        v.write_polar_sync(&SyncState {
            sessions_through: Some("2026-06-10".into()),
            updated: Some("2026-06-10T00:00:00+00:00".into()),
            ..Default::default()
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/polar-sync.json")).unwrap();
        assert!(!cursor.contains("polar_access_abc"), "token never in cursor");
        assert!(!cursor.contains("polar_refresh_xyz"), "refresh token never in cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("polar_access_abc") {
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
        }

        def_disconnect(&v, "polar").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn fetch_error_401_contains_reconnect_hint() {
        let err = fetch_err("training-sessions", FetchError::Unauthorized);
        let msg = err.to_string();
        assert!(msg.contains("reconnect"), "401 error carries reconnect hint: {msg}");
        assert!(msg.contains("training-sessions"), "names the endpoint: {msg}");
    }

    #[test]
    fn fetch_error_429_contains_retry_hint() {
        let err = fetch_err("sleeps", FetchError::RateLimited);
        let msg = err.to_string();
        assert!(msg.contains("rate limited"), "{msg}");
        assert!(msg.contains("sleeps"), "{msg}");
    }
}
