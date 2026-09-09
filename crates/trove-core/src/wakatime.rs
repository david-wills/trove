//! WakaTime — cloud coding-activity tracker via the WakaTime REST API.
//! Also supports Wakapi (self-hosted, WakaTime-compatible endpoint).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/wakatime.md
//!
//! A **Periodic** cloud pull (daily cadence): one call to
//! `GET /api/v1/users/current/summaries` per day-window fetches per-day
//! totals (project / language / editor / OS breakdowns, total_seconds) and
//! writes them to `activity/wakatime/YYYY-MM.jsonl` — one row per
//! (day, project) pair. This is RAW-ONLY: the `activity/` root is a
//! raw-only domain; no bound contract applies.
//!
//! ## Deduplication / watermark
//!
//! The cursor (`.trove/wakatime-sync.json`) holds `last_date` — the latest
//! date successfully written, as a `YYYY-MM-DD` string. Each pull fetches
//! the window `[last_date, today]` (inclusive, so today re-pulls on every
//! run because the current day's total changes as code gets written). An
//! existing row is overwritten only when re-pulled (upsert by guid); a
//! crash before the cursor advances re-drains the same window next time.
//!
//! ## Auth
//!
//! API key as `Authorization: Basic base64(<api_key>)` — WakaTime's
//! documented Basic-auth form (the key is the only field; no username).
//! Stored under `.trove/sync/` (0600). A configurable base URL lets Wakapi
//! users point at their own instance.
//!
//! ## guid
//!
//! FNV-1a 64-bit hash over `date || ":" || project_name` encoded as 16-char
//! lowercase hex — stable, deterministic, never a reassignable DB rowid.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Where raw rows land.
const DIR: &str = "activity/wakatime";
/// Non-secret rebuildable cursor — not under `.trove/sync/` (that's for 0600 secrets).
const SYNC_FILE: &str = ".trove/wakatime-sync.json";
/// Secret-store service id for the stored API key + optional base URL.
const SERVICE: &str = "wakatime";
/// Default WakaTime cloud API base.
const DEFAULT_API_BASE: &str = "https://wakatime.com";
/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Daily cadence in seconds (86400 = 24 h).
pub const WAKATIME_SYNC_SECS: u64 = 86_400;
/// Number of days per backfill window.  Small enough that a free-tier account's
/// 14-day retention still returns at least one non-empty window, yet large enough
/// that a paid account completes a years-long backfill in a reasonable number of
/// API calls.
const BACKFILL_WINDOW_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                let r = out.counts.get("rows").copied().unwrap_or(0);
                format!("wakatime synced — {r} activity rows")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "wakatime sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let r = out.counts.get("rows").copied().unwrap_or(0);
    Ok(PullOutcome {
        headline: format!("WakaTime synced — {r} activity rows"),
        counts: out.counts,
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "wakatime",
        name: "WakaTime",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Pulls your WakaTime coding stats — languages, projects, editors, \
                      and daily totals — via the WakaTime REST API. Also supports \
                      self-hosted Wakapi instances.",
        domain: "activity",
        vault_path: "activity/wakatime/",
        toggleable: true,
        setup: &[
            "Open wakatime.com/settings/account and copy your Secret API Key.",
            "Paste it in the connect card below. Wakapi users can also set a custom base URL.",
            "First sync backfills the available history (free tier limited to ~2 weeks); later syncs are incremental.",
        ],
        caveats: "Requires an existing WakaTime account and an editor plugin already installed; \
                  data only goes back to when the plugin was first installed. Free accounts have \
                  limited history depth (~14 days); a paid plan unlocks the full range. The \
                  current day's total changes as you code and is re-pulled on each sync.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(WAKATIME_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("wakatime"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection — TokenPaste: paste the API key; optional second paste for the
// Wakapi base URL. We represent both in a single `TokenSet`:
//   access_token = the API key
//   scope        = optional base URL (overrides DEFAULT_API_BASE when set)
//
// The composite field separator is "\n" so a bare key (no newline) always
// works, and a Wakapi user appends "\n<url>" after the key.

fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!(
            "empty API key — copy your Secret API Key from wakatime.com/settings/account \
             (or your Wakapi instance settings)"
        );
    }
    let (api_key, base_url) = split_cred(raw);
    if api_key.is_empty() {
        bail!("API key is blank — paste it on the first line");
    }
    // Verify with a lightweight /users/current probe.
    let client = WakaClient::new(base_url.clone(), api_key.clone());
    match client.verify() {
        Ok(()) => {}
        Err(FetchError::Unauthorized) => bail!(
            "WakaTime rejected the key (401) — copy it fresh from \
             wakatime.com/settings/account"
        ),
        Err(e) => bail!("WakaTime auth check failed: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: api_key,
            refresh_token: None,
            token_type: None,
            scope: if base_url == DEFAULT_API_BASE {
                None
            } else {
                Some(base_url)
            },
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(ts) = vault.load_sync_token(SERVICE)? {
        let base = ts.scope.as_deref().unwrap_or(DEFAULT_API_BASE);
        let mut extra = BTreeMap::new();
        if base != DEFAULT_API_BASE {
            extra.insert("base_url", base.to_string());
        }
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: "WakaTime".to_string(),
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra,
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
///
/// Single TokenPaste field: the Secret API Key (mandatory) followed optionally
/// by a newline and a Wakapi base URL. A bare key works for the hosted service;
/// Wakapi users paste `<key>\n<https://my.wakapi.host>`.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "wakatime",
    display_name: "WakaTime",
    methods: &[ConnectMethod::TokenPaste {
        label: "WakaTime Secret API Key",
        help: "Copy your Secret API Key from wakatime.com/settings/account. \
               Wakapi users: paste the key on line 1, your instance URL on line 2 \
               (e.g. https://my.wakapi.host).",
        placeholder: "waka_sec_…  (Wakapi: add your instance URL on a second line)",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["wakatime"],
    setup: &[
        "Sign in to wakatime.com and go to Settings → Account.",
        "Copy your Secret API Key and paste it in the connect card.",
        "Wakapi users: paste the key on line 1, instance URL on line 2.",
    ],
};

// ---------------------------------------------------------------------------
// Credential helpers.

/// Split the pasted field into (api_key, base_url). A bare key on one line
/// yields (key, DEFAULT_API_BASE). A two-line paste yields (first_line, second_line).
fn split_cred(raw: &str) -> (String, String) {
    let mut lines = raw.splitn(2, '\n');
    let key = lines.next().unwrap_or("").trim().to_string();
    let url = lines
        .next()
        .map(|u| u.trim())
        .filter(|u| !u.is_empty())
        .unwrap_or(DEFAULT_API_BASE)
        .to_string();
    (key, url)
}

/// Reconstruct (api_key, base_url) from a stored TokenSet.
fn cred_from_token(ts: TokenSet) -> (String, String) {
    let base = ts.scope.unwrap_or_else(|| DEFAULT_API_BASE.to_string());
    (ts.access_token, base)
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    RateLimited,
    /// HTTP 402 or any "upgrade required" paywall response — free-tier history
    /// wall.  Treated as a graceful stop, not a hard error.
    PaymentRequired,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::RateLimited => write!(f, "rate limited (HTTP 429)"),
            FetchError::PaymentRequired => {
                write!(f, "payment required (HTTP 402) — history past the free-tier limit")
            }
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The endpoints the pull needs. A trait so tests can inject fixtures without
/// touching the network.
trait WakaApi {
    /// `GET /api/v1/users/current/summaries?start=YYYY-MM-DD&end=YYYY-MM-DD`
    fn summaries(&self, start: &str, end: &str) -> Result<Value, FetchError>;
}

struct WakaClient {
    base: String,
    api_key: String,
}

impl WakaClient {
    fn new(base: String, api_key: String) -> Self {
        WakaClient { base, api_key }
    }

    /// WakaTime Basic auth: `base64(<api_key>)` — the key alone, no colon/password.
    fn auth_header(&self) -> String {
        format!("Basic {}", STANDARD.encode(&self.api_key))
    }

    /// Lightweight probe: `GET /api/v1/users/current` — 200 = valid key.
    fn verify(&self) -> Result<(), FetchError> {
        let url = format!("{}/api/v1/users/current", self.base);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &self.auth_header())
            .call()
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(429, _)) => Err(FetchError::RateLimited),
            Err(ureq::Error::Status(code, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                Err(FetchError::Other(format!(
                    "HTTP {code}: {}",
                    body.chars().take(200).collect::<String>()
                )))
            }
            Err(e) => Err(FetchError::Other(e.to_string())),
        }
    }
}

impl WakaApi for WakaClient {
    fn summaries(&self, start: &str, end: &str) -> Result<Value, FetchError> {
        let url = format!("{}/api/v1/users/current/summaries", self.base);
        match ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &self.auth_header())
            .query("start", start)
            .query("end", end)
            .call()
        {
            Ok(resp) => resp
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing summaries: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            // 402 = payment required / free-tier history wall — graceful stop.
            Err(ureq::Error::Status(402, _)) => Err(FetchError::PaymentRequired),
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
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Latest date written successfully as `YYYY-MM-DD`. None = first sync
    /// (backfill from 2013-01-01, the earliest WakaTime launched).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_date: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_wakatime_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_wakatime_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Row shape and pure mapping logic.

/// One on-disk row: one (day, project) pair from a summaries response.
#[derive(Serialize, Deserialize, Debug)]
struct ActivityRow {
    /// Stable dedupe key: FNV-1a 64-bit hex over "date:project" (16 hex chars).
    guid: String,
    /// The day this row covers, as `YYYY-MM-DD`.
    date: String,
    /// First day of the month (`YYYY-MM-01`) — the partition timestamp the
    /// store appends under (`YYYY-MM.jsonl`). We use the ISO month prefix as
    /// the ts so `Partition::Month` slices it correctly.
    #[serde(rename = "ts")]
    month_ts: String,
    /// Project name as returned by the API. `"<<UNTRACKED>>"` for any
    /// cross-day totals that have no project name (the rare `projects: []`
    /// case).
    project: String,
    /// Total seconds for this (day, project) from `grand_total.total_seconds`
    /// when there are no projects, or from the project item's `total_seconds`.
    total_seconds: f64,
    /// Human-readable duration text from the API (`"2 hrs 30 mins"`).
    #[serde(skip_serializing_if = "String::is_empty")]
    text: String,
    /// Language breakdown for this project on this day, as returned by the
    /// API (`[{"name": "Rust", "total_seconds": 3600, ...}]`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    languages: Vec<Value>,
    /// Editor breakdown (same shape as languages).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    editors: Vec<Value>,
    /// OS breakdown (same shape as languages).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    operating_systems: Vec<Value>,
    /// Any extra top-level fields from the day object for full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    extra: Map<String, Value>,
}

/// Deterministic guid = lowercase hex of sha256(`<date>:<project>`).
/// Uses a simple iterated-byte hash so we don't need a crypto dep.
/// This is NOT a security hash — just a stable identifier.
fn row_guid(date: &str, project: &str) -> String {
    // FNV-1a 64-bit over "<date>:<project>" then encode as 16-char hex.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in date.bytes().chain(b":".iter().copied()).chain(project.bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

/// `YYYY-MM-DD` → `YYYY-MM-01` (the month timestamp the partition key needs).
fn month_ts(date: &str) -> String {
    if date.len() >= 7 {
        format!("{}-01", &date[..7])
    } else {
        date.to_string()
    }
}

/// Extract a string field from a JSON object, trimmed; "" when absent or non-string.
fn str_f(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Extract a float field; 0.0 when absent.
fn f64_f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Parse one day item from `data[]` into a vec of [`ActivityRow`] — one per
/// project (or one untracked row when there are no projects).
fn rows_from_day(day: &Value) -> Vec<ActivityRow> {
    // Extract `range.date` (YYYY-MM-DD).
    let date = day
        .get("range")
        .and_then(|r| r.get("date"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if date.is_empty() {
        return Vec::new();
    }

    let languages: Vec<Value> = day
        .get("languages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let editors: Vec<Value> = day
        .get("editors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let operating_systems: Vec<Value> = day
        .get("operating_systems")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Extra: generic passthrough of top-level day keys not already mapped into
    // typed fields.  This preserves categories/dependencies/machines/branches/
    // entities (and any future API additions) without an exhaustive allowlist.
    // Keys that are already extracted into typed fields are excluded to avoid
    // duplication.
    const TYPED_KEYS: &[&str] = &[
        "languages",
        "editors",
        "operating_systems",
        "projects",
        "range",
    ];
    let mut extra = Map::new();
    if let Some(obj) = day.as_object() {
        for (k, v) in obj {
            if !TYPED_KEYS.contains(&k.as_str()) {
                extra.insert(k.clone(), v.clone());
            }
        }
    }

    let projects: Vec<&Value> = day
        .get("projects")
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default();

    if projects.is_empty() {
        // No project breakdown — emit one "untracked" row for the day total,
        // but only when there was actual activity (skip zero-second days that
        // flood the vault with empty rows on a multi-year backfill).
        let total_seconds = day
            .get("grand_total")
            .and_then(|g| g.get("total_seconds"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        if total_seconds == 0.0 {
            return Vec::new();
        }
        let text = day
            .get("grand_total")
            .and_then(|g| g.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let project = "<<UNTRACKED>>".to_string();
        let guid = row_guid(&date, &project);
        return vec![ActivityRow {
            guid,
            date: date.clone(),
            month_ts: month_ts(&date),
            project,
            total_seconds,
            text,
            languages,
            editors,
            operating_systems,
            extra,
        }];
    }

    projects
        .into_iter()
        .filter_map(|p| {
            let project = str_f(p, "name");
            if project.is_empty() {
                return None;
            }
            let total_seconds = f64_f(p, "total_seconds");
            let text = str_f(p, "text");
            let guid = row_guid(&date, &project);
            // Per-project language breakdown isn't available in the summaries
            // API (languages are per-day totals, not per-project) — store the
            // day-level breakdowns on every project row so a reader can still
            // correlate coding language with the day.
            Some(ActivityRow {
                guid,
                date: date.clone(),
                month_ts: month_ts(&date),
                project,
                total_seconds,
                text,
                languages: languages.clone(),
                editors: editors.clone(),
                operating_systems: operating_systems.clone(),
                extra: extra.clone(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Write: upsert by guid (re-pull today upserts current totals).

/// Upsert rows into `activity/wakatime/YYYY-MM.jsonl` keyed by guid. An
/// existing row with the same guid is replaced (today's totals change as code
/// gets written); rows for other guids are appended.
fn upsert_rows(vault: &Vault, rows: Vec<ActivityRow>) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }

    // Group by month partition key (month_ts → "YYYY-MM").
    let mut by_month: BTreeMap<String, Vec<ActivityRow>> = BTreeMap::new();
    for row in rows {
        // Partition::Month keys on the first 7 chars of the ts (YYYY-MM).
        let key = row.month_ts[..7.min(row.month_ts.len())].to_string();
        by_month.entry(key).or_default().push(row);
    }

    let stream = vault.stream(DIR, Partition::Month);
    let mut written: u64 = 0;

    for (month_key, new_rows) in by_month {
        // Read existing rows for this month.
        let mut existing: Vec<ActivityRow> = stream.read::<ActivityRow>(&month_key).unwrap_or_default();

        // Build a map of guid → index for fast upsert.
        let mut guid_index: BTreeMap<String, usize> =
            existing.iter().enumerate().map(|(i, r)| (r.guid.clone(), i)).collect();

        let mut appended = 0u64;
        for row in new_rows {
            if let Some(&idx) = guid_index.get(&row.guid) {
                // Replace in place.
                existing[idx] = row;
            } else {
                guid_index.insert(row.guid.clone(), existing.len());
                existing.push(row);
                appended += 1;
            }
        }
        written += appended;

        // Re-write the whole month file with updated rows.
        // Use write_snapshot (full replace) since we maintain the in-memory set.
        let path_str = format!("{}/{}.jsonl", DIR, month_key);
        let path = vault.resolve(&path_str)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for row in &existing {
            out.push_str(&serde_json::to_string(row)?);
            out.push('\n');
        }
        crate::store::write_atomic(
            &path,
            out.as_bytes(),
        )?;
    }

    Ok(written)
}

// ---------------------------------------------------------------------------
// The pull.

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let ts = vault
        .load_sync_token(SERVICE)?
        .context("WakaTime is not connected — add your API key in the Integrations tab")?;
    let (api_key, base_url) = cred_from_token(ts);
    if api_key.trim().is_empty() {
        bail!("WakaTime API key is blank — reconnect from the Integrations tab");
    }
    let client = WakaClient::new(base_url, api_key);
    pull_with(vault, &client)
}

/// The testable pull body — the API is injected so tests run offline.
///
/// ## Windowed backfill strategy
///
/// On the first sync (`last_date` is None) we walk backward from today in
/// `BACKFILL_WINDOW_DAYS`-day chunks until either:
///   1. A window returns zero data rows (we've passed the history floor), or
///   2. We've fetched a 402 Payment Required (free-tier history wall).
///
/// After a successful initial sweep the cursor (`last_date`) is set to the
/// earliest date from which we got data, so subsequent incremental syncs only
/// re-request recent windows.
///
/// On every sync the `[last_date, today]` window is always re-fetched so the
/// current day's running total gets updated.
fn pull_with(vault: &Vault, api: &impl WakaApi) -> Result<PullOutcome> {
    let mut state = vault.read_wakatime_sync();
    let mut total_written: u64 = 0;

    let today_str = Local::now().format("%Y-%m-%d").to_string();
    let today = NaiveDate::parse_from_str(&today_str, "%Y-%m-%d")
        .unwrap_or_else(|_| Local::now().date_naive());

    // --- Incremental window: [last_date, today] (always, even after backfill) ---
    // This re-checks today's partial total and any new days since last run.
    let incr_start = state
        .last_date
        .clone()
        .unwrap_or_else(|| "2013-01-01".to_string());

    match api.summaries(&incr_start, &today_str) {
        Ok(resp) => {
            let data = resp.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            if !data.is_empty() {
                let rows: Vec<ActivityRow> = data.iter().flat_map(rows_from_day).collect();
                let new_last: Option<String> = rows.iter().map(|r| r.date.clone()).max();
                let w = upsert_rows(vault, rows)?;
                total_written += w;
                if let Some(nd) = new_last {
                    if state.last_date.as_deref().is_none_or(|cur| nd.as_str() >= cur) {
                        state.last_date = Some(nd);
                    }
                }
            } else if state.last_date.is_none() {
                // First sync: empty means the API returned nothing for the full
                // range.  Advance the floor to today so we don't re-scan forever.
                state.last_date = Some(today_str.clone());
            }
        }
        Err(FetchError::Unauthorized) => {
            bail!("WakaTime rejected the key (401) — reconnect from the Integrations tab");
        }
        Err(FetchError::RateLimited) => {
            bail!("WakaTime rate-limited the summaries request — it'll retry on the next sync");
        }
        Err(FetchError::PaymentRequired) => {
            // Free-tier wall on the large window.  Fall through to the
            // day-by-day approach below (or accept 0 rows if already seeded).
        }
        Err(FetchError::Other(msg)) => {
            bail!("WakaTime summaries fetch failed: {msg}");
        }
    }

    // --- Backward backfill: walk month-sized windows into history ---
    // Only runs when there is no confirmed floor in the cursor, which means
    // either the first sync or the incremental window returned empty/402.
    // We walk backward until two consecutive empty windows or a 402.
    if state.last_date.is_none() {
        let earliest_target =
            NaiveDate::parse_from_str("2013-01-01", "%Y-%m-%d").unwrap();
        let mut window_end = today;
        let mut consecutive_empty = 0u32;

        loop {
            let window_start = (window_end
                - ChronoDuration::days(BACKFILL_WINDOW_DAYS - 1))
            .max(earliest_target);

            let ws = window_start.format("%Y-%m-%d").to_string();
            let we = window_end.format("%Y-%m-%d").to_string();

            match api.summaries(&ws, &we) {
                Ok(resp) => {
                    let data =
                        resp.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
                    if data.is_empty() {
                        consecutive_empty += 1;
                        // Two consecutive empty windows = we've passed the floor.
                        if consecutive_empty >= 2 {
                            // Anchor the floor so subsequent syncs don't re-scan.
                            if state.last_date.is_none() {
                                state.last_date = Some(we.clone());
                            }
                            break;
                        }
                    } else {
                        consecutive_empty = 0;
                        let rows: Vec<ActivityRow> =
                            data.iter().flat_map(rows_from_day).collect();
                        // Track the earliest date we got data for as the new floor.
                        let earliest: Option<String> =
                            rows.iter().map(|r| r.date.clone()).min();
                        let w = upsert_rows(vault, rows)?;
                        total_written += w;
                        if let Some(ed) = earliest {
                            // Keep the earliest confirmed date as last_date so the
                            // next incremental sync starts from there.
                            if state.last_date.as_deref().is_none_or(|cur| ed.as_str() < cur) {
                                state.last_date = Some(ed);
                            }
                        }
                    }
                }
                Err(FetchError::PaymentRequired) => {
                    // Free-tier history wall — stop walking, anchor the floor.
                    if state.last_date.is_none() {
                        state.last_date = Some(we.clone());
                    }
                    break;
                }
                Err(FetchError::Unauthorized) => {
                    bail!("WakaTime rejected the key (401) — reconnect from the Integrations tab");
                }
                Err(FetchError::RateLimited) => {
                    // Rate-limited mid-backfill — save progress and abort this pass.
                    break;
                }
                Err(FetchError::Other(msg)) => {
                    // Non-fatal mid-backfill error — save progress and stop.
                    eprintln!("wakatime: backfill window {ws}..{we} error: {msg}; stopping early");
                    break;
                }
            }

            if window_start <= earliest_target {
                // Reached the absolute floor.
                if state.last_date.is_none() {
                    state.last_date = Some(earliest_target.format("%Y-%m-%d").to_string());
                }
                break;
            }
            // Step one window further into the past.
            window_end = window_start - ChronoDuration::days(1);
        }
    }

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    counts.insert("rows", total_written);
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_wakatime_sync(&state)?;

    Ok(PullOutcome {
        headline: if total_written == 0 {
            "WakaTime is up to date — no new rows".to_string()
        } else {
            format!("WakaTime synced — {total_written} activity rows")
        },
        counts,
    })
}

// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-wakatime-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixture: minimal valid summaries response ---

    fn summaries_resp() -> Value {
        json!({
            "data": [
                {
                    "grand_total": {
                        "digital": "3:30",
                        "hours": 3,
                        "minutes": 30,
                        "text": "3 hrs 30 mins",
                        "total_seconds": 12600.0
                    },
                    "projects": [
                        {
                            "name": "trove",
                            "total_seconds": 9000.0,
                            "percent": 71.4,
                            "digital": "2:30",
                            "text": "2 hrs 30 mins",
                            "hours": 2,
                            "minutes": 30
                        },
                        {
                            "name": "dotfiles",
                            "total_seconds": 3600.0,
                            "percent": 28.6,
                            "digital": "1:00",
                            "text": "1 hr",
                            "hours": 1,
                            "minutes": 0
                        }
                    ],
                    "languages": [
                        {
                            "name": "Rust",
                            "total_seconds": 9000.0,
                            "percent": 71.4,
                            "text": "2 hrs 30 mins"
                        },
                        {
                            "name": "Bash",
                            "total_seconds": 3600.0,
                            "percent": 28.6,
                            "text": "1 hr"
                        }
                    ],
                    "editors": [
                        {
                            "name": "Neovim",
                            "total_seconds": 12600.0,
                            "percent": 100.0,
                            "text": "3 hrs 30 mins"
                        }
                    ],
                    "operating_systems": [
                        {
                            "name": "Mac",
                            "total_seconds": 12600.0,
                            "percent": 100.0,
                            "text": "3 hrs 30 mins"
                        }
                    ],
                    "range": {
                        "date": "2026-06-15",
                        "start": "2026-06-15T00:00:00Z",
                        "end": "2026-06-15T23:59:59Z",
                        "text": "Sun Jun 15, 2026",
                        "timezone": "America/Los_Angeles"
                    }
                },
                {
                    "grand_total": {
                        "digital": "1:00",
                        "hours": 1,
                        "minutes": 0,
                        "text": "1 hr",
                        "total_seconds": 3600.0
                    },
                    "projects": [],
                    "languages": [],
                    "editors": [],
                    "operating_systems": [],
                    "range": {
                        "date": "2026-06-14",
                        "start": "2026-06-14T00:00:00Z",
                        "end": "2026-06-14T23:59:59Z",
                        "text": "Sat Jun 14, 2026",
                        "timezone": "America/Los_Angeles"
                    }
                }
            ],
            "start": "2026-06-14T00:00:00Z",
            "end": "2026-06-15T23:59:59Z",
            "cumulative_total": {
                "seconds": 16200.0,
                "text": "4 hrs 30 mins"
            }
        })
    }

    // --- mock API ---

    struct MockApi {
        responses: std::cell::RefCell<Vec<Result<Value, FetchError>>>,
        calls: std::cell::RefCell<Vec<(String, String)>>,
    }

    impl MockApi {
        fn single(resp: Value) -> Self {
            MockApi {
                responses: std::cell::RefCell::new(vec![Ok(resp)]),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
        fn error(e: FetchError) -> Self {
            MockApi {
                responses: std::cell::RefCell::new(vec![Err(e)]),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
        /// Sequence of responses consumed in order (first → last).
        /// After all are consumed, subsequent calls return empty data.
        fn seq(resps: Vec<Result<Value, FetchError>>) -> Self {
            // Vec::pop() returns from the end, so reverse to serve in order.
            let mut r = resps;
            r.reverse();
            MockApi {
                responses: std::cell::RefCell::new(r),
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl WakaApi for MockApi {
        fn summaries(&self, start: &str, end: &str) -> Result<Value, FetchError> {
            self.calls.borrow_mut().push((start.to_string(), end.to_string()));
            self.responses
                .borrow_mut()
                .pop()
                .unwrap_or(Ok(json!({"data": []})))
        }
    }

    // --- pure mapping tests ---

    #[test]
    fn row_guid_is_deterministic_and_unique() {
        let g1 = row_guid("2026-06-15", "trove");
        let g2 = row_guid("2026-06-15", "trove");
        let g3 = row_guid("2026-06-15", "dotfiles");
        let g4 = row_guid("2026-06-16", "trove");
        assert_eq!(g1, g2, "same inputs = same guid");
        assert_ne!(g1, g3, "different project = different guid");
        assert_ne!(g1, g4, "different date = different guid");
        assert_eq!(g1.len(), 16, "16-char hex");
    }

    #[test]
    fn month_ts_extracts_year_month_with_01() {
        assert_eq!(month_ts("2026-06-15"), "2026-06-01");
        assert_eq!(month_ts("2025-12-31"), "2025-12-01");
    }

    #[test]
    fn rows_from_day_emits_one_row_per_project() {
        let data = summaries_resp();
        let day = &data["data"][0];
        let rows = rows_from_day(day);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].project, "trove");
        assert_eq!(rows[0].date, "2026-06-15");
        assert_eq!(rows[0].total_seconds, 9000.0);
        assert_eq!(rows[0].text, "2 hrs 30 mins");
        assert_eq!(rows[0].month_ts, "2026-06-01");
        assert!(!rows[0].languages.is_empty(), "day-level language breakdown carried through");
        assert!(!rows[0].editors.is_empty());

        assert_eq!(rows[1].project, "dotfiles");
        assert_eq!(rows[1].total_seconds, 3600.0);
        // guids differ
        assert_ne!(rows[0].guid, rows[1].guid);
    }

    #[test]
    fn rows_from_day_untracked_when_no_projects() {
        let data = summaries_resp();
        let day = &data["data"][1];
        let rows = rows_from_day(day);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].project, "<<UNTRACKED>>");
        assert_eq!(rows[0].date, "2026-06-14");
        assert_eq!(rows[0].total_seconds, 3600.0);
    }

    #[test]
    fn rows_from_day_skips_day_with_no_date() {
        let day = json!({"projects": [], "range": {}});
        let rows = rows_from_day(&day);
        assert!(rows.is_empty(), "no date = no rows");
    }

    // --- integration pull tests ---

    #[test]
    fn full_pull_writes_rows_and_advances_cursor() {
        let v = temp_vault("fullpull");
        let api = MockApi::single(summaries_resp());
        let out = pull_with(&v, &api).unwrap();
        // 2 project rows (Jun 15) + 1 untracked row (Jun 14) = 3 new rows.
        assert_eq!(out.counts.get("rows"), Some(&3));

        // Jun rows written.
        let path = v.root().join("activity/wakatime/2026-06.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(content.contains("\"project\":\"trove\""));
        assert!(content.contains("\"project\":\"dotfiles\""));
        assert!(content.contains("\"project\":\"<<UNTRACKED>>\""));
        assert!(content.contains("\"total_seconds\":9000.0"));
        // language/editor breakdowns carried through.
        assert!(content.contains("\"Rust\""));
        assert!(content.contains("\"Neovim\""));

        // Cursor advanced to the newest date.
        let state = v.read_wakatime_sync();
        assert_eq!(state.last_date.as_deref(), Some("2026-06-15"));
        assert!(state.updated.is_some());

        // No secret in the cursor file.
        let cursor_str =
            std::fs::read_to_string(v.root().join(".trove/wakatime-sync.json")).unwrap();
        assert!(!cursor_str.contains("waka_sec"), "secret never in cursor");
    }

    #[test]
    fn re_pull_same_day_upserts_not_duplicates() {
        let v = temp_vault("upsert");
        let api1 = MockApi::single(summaries_resp());
        pull_with(&v, &api1).unwrap();

        // Second pull of the same data: all rows have same guids → upserted
        // in-place, not duplicated. New-row count = 0.
        let api2 = MockApi::single(summaries_resp());
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("rows"), Some(&0), "no new guids → 0 new rows");

        let path = v.root().join("activity/wakatime/2026-06.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().count(), 3, "still 3 rows, not 6");
    }

    #[test]
    fn empty_response_does_not_panic_and_leaves_cursor_intact() {
        let v = temp_vault("empty");
        // Pre-seed a cursor.
        v.write_wakatime_sync(&SyncState {
            last_date: Some("2026-06-10".into()),
            updated: None,
        })
        .unwrap();
        let api = MockApi::single(json!({"data": []}));
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("rows"), Some(&0));
        // Cursor last_date not regressed.
        assert_eq!(
            v.read_wakatime_sync().last_date.as_deref(),
            Some("2026-06-10")
        );
    }

    #[test]
    fn fetch_error_does_not_advance_cursor() {
        let v = temp_vault("fetcherr");
        v.write_wakatime_sync(&SyncState {
            last_date: Some("2026-06-01".into()),
            updated: None,
        })
        .unwrap();
        let api = MockApi::error(FetchError::Other("boom".into()));
        let err = pull_with(&v, &api).unwrap_err().to_string();
        assert!(err.contains("summaries") || err.contains("failed"), "clear error: {err}");
        // Cursor unchanged.
        assert_eq!(
            v.read_wakatime_sync().last_date.as_deref(),
            Some("2026-06-01"),
            "failed fetch must not advance cursor"
        );
    }

    #[test]
    fn auth_header_is_basic_base64_key_only() {
        let c = WakaClient::new(DEFAULT_API_BASE.into(), "my_secret_key".into());
        let h = c.auth_header();
        assert!(h.starts_with("Basic "), "Basic auth prefix");
        let b64 = h.strip_prefix("Basic ").unwrap();
        let decoded = String::from_utf8(STANDARD.decode(b64).unwrap()).unwrap();
        // WakaTime Basic auth: the key alone, no colon/password.
        assert_eq!(decoded, "my_secret_key", "key only, no colon/api_token suffix");
    }

    #[test]
    fn split_cred_bare_key_uses_default_base() {
        let (key, base) = split_cred("waka_sec_abc123");
        assert_eq!(key, "waka_sec_abc123");
        assert_eq!(base, DEFAULT_API_BASE);
    }

    #[test]
    fn split_cred_two_lines_gives_custom_base() {
        let (key, base) = split_cred("waka_sec_abc\nhttps://my.wakapi.host");
        assert_eq!(key, "waka_sec_abc");
        assert_eq!(base, "https://my.wakapi.host");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_date.is_none());
        let partial: SyncState = serde_json::from_str(r#"{"last_date":"2026-05-01"}"#).unwrap();
        assert_eq!(partial.last_date.as_deref(), Some("2026-05-01"));
        assert!(partial.updated.is_none());
    }

    #[test]
    fn connection_exposes_token_paste_and_def_references_it() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "wakatime");
        assert_eq!(DEF.connection, Some("wakatime"));
    }

    #[test]
    fn first_sync_uses_backfill_start_2013() {
        let v = temp_vault("backfill");
        let api = MockApi::single(json!({"data": []}));
        pull_with(&v, &api).unwrap();
        // The call to summaries should have used "2013-01-01" as start.
        let calls = api.calls.borrow();
        assert_eq!(calls[0].0, "2013-01-01", "backfill starts at WakaTime launch");
    }

    // --- defect-regression tests ---

    /// Defect 2: 402 PaymentRequired on the incremental window must NOT hard-fail;
    /// pull must succeed with 0 rows and the cursor must be preserved.
    #[test]
    fn payment_required_on_incremental_does_not_hard_fail() {
        let v = temp_vault("pay402");
        v.write_wakatime_sync(&SyncState {
            last_date: Some("2026-06-01".into()),
            updated: None,
        })
        .unwrap();
        // 402 on the incremental call — backfill loop is skipped (last_date is Some).
        let api = MockApi::error(FetchError::PaymentRequired);
        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("rows"), Some(&0), "graceful 0 rows on 402");
        // Cursor not regressed.
        assert_eq!(
            v.read_wakatime_sync().last_date.as_deref(),
            Some("2026-06-01"),
            "cursor preserved on 402"
        );
    }

    /// Defect 1: first sync that returns data must advance the cursor so the
    /// next sync doesn't re-scan from 2013-01-01 again.
    #[test]
    fn first_sync_with_data_sets_cursor_so_next_sync_is_incremental() {
        let v = temp_vault("cursor_advance");
        // First pull: returns data for Jun 15.
        let api1 = MockApi::single(summaries_resp());
        pull_with(&v, &api1).unwrap();
        let state_after_first = v.read_wakatime_sync();
        assert!(
            state_after_first.last_date.is_some(),
            "cursor must be set after first pull with data"
        );
        // Second pull: should start from last_date, not 2013-01-01.
        let api2 = MockApi::single(json!({"data": []}));
        pull_with(&v, &api2).unwrap();
        let calls = api2.calls.borrow();
        assert!(
            calls[0].0 != "2013-01-01",
            "second sync must not re-scan from 2013; got start={:?}",
            calls[0].0
        );
    }

    /// Defect 4: a day with no projects AND total_seconds == 0 must not emit a row.
    #[test]
    fn zero_activity_day_emits_no_row() {
        let day = json!({
            "grand_total": {
                "digital": "0:00",
                "hours": 0,
                "minutes": 0,
                "text": "0 secs",
                "total_seconds": 0.0
            },
            "projects": [],
            "languages": [],
            "editors": [],
            "operating_systems": [],
            "range": {
                "date": "2026-06-13",
                "start": "2026-06-13T00:00:00Z",
                "end": "2026-06-13T23:59:59Z",
                "text": "Fri Jun 13, 2026",
                "timezone": "America/Los_Angeles"
            }
        });
        let rows = rows_from_day(&day);
        assert!(rows.is_empty(), "zero-second day with no projects must emit no rows");
    }

    /// Defect 3: extra must contain categories/dependencies/machines/branches/entities
    /// and grand_total — not just grand_total.
    #[test]
    fn extra_contains_all_top_level_breakdown_arrays() {
        let day = json!({
            "grand_total": {"total_seconds": 3600.0, "text": "1 hr"},
            "projects": [{"name": "myproj", "total_seconds": 3600.0, "text": "1 hr"}],
            "languages": [],
            "editors": [],
            "operating_systems": [],
            "categories": [{"name": "Coding", "total_seconds": 3600.0}],
            "dependencies": [{"name": "serde", "total_seconds": 3600.0}],
            "machines": [{"name": "MacBook", "total_seconds": 3600.0}],
            "branches": [{"name": "main", "total_seconds": 3600.0}],
            "entities": [{"name": "wakatime.rs", "total_seconds": 3600.0}],
            "range": {
                "date": "2026-06-15",
                "start": "2026-06-15T00:00:00Z",
                "end": "2026-06-15T23:59:59Z",
                "text": "Sun Jun 15, 2026",
                "timezone": "America/Los_Angeles"
            }
        });
        let rows = rows_from_day(&day);
        assert_eq!(rows.len(), 1);
        let extra = &rows[0].extra;
        assert!(extra.contains_key("grand_total"), "grand_total in extra");
        assert!(extra.contains_key("categories"), "categories in extra");
        assert!(extra.contains_key("dependencies"), "dependencies in extra");
        assert!(extra.contains_key("machines"), "machines in extra");
        assert!(extra.contains_key("branches"), "branches in extra");
        assert!(extra.contains_key("entities"), "entities in extra");
        // typed fields must NOT be duplicated in extra.
        assert!(!extra.contains_key("projects"), "projects (typed) not in extra");
        assert!(!extra.contains_key("languages"), "languages (typed) not in extra");
        assert!(!extra.contains_key("range"), "range (typed) not in extra");
    }

    /// Defect 5 regression: guid is exactly 16 hex chars (FNV-1a 64-bit).
    #[test]
    fn guid_is_fnv64_16_hex_chars() {
        let g = row_guid("2026-06-15", "myproject");
        assert_eq!(g.len(), 16, "FNV-1a 64-bit = 16 hex chars");
        assert!(g.chars().all(|c| c.is_ascii_hexdigit()), "all hex digits");
    }

    /// Backfill loop: when first sync returns data, subsequent pull should NOT
    /// re-run the backfill loop (last_date is set).
    #[test]
    fn backfill_loop_skipped_when_cursor_already_set() {
        let v = temp_vault("no_backfill_loop");
        // First pull with real data sets the cursor.
        let api1 = MockApi::single(summaries_resp());
        pull_with(&v, &api1).unwrap();
        let call_count_first = api1.calls.borrow().len();
        // One incremental call; backfill loop skipped (cursor set).
        assert_eq!(call_count_first, 1, "first sync with data = 1 call");

        // Second pull: only incremental call, no backfill.
        let api2 = MockApi::single(json!({"data": []}));
        pull_with(&v, &api2).unwrap();
        assert_eq!(api2.calls.borrow().len(), 1, "second sync = 1 call, no backfill loop");
    }
}
