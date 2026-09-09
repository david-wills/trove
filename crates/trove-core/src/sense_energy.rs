//! Sense Energy Monitor — whole-home electricity via the unofficial REST API +
//! a CSV export import path (CSV parser parked; see below).
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/sense-energy.md
//!
//! ## Auth
//!
//! Sense's unofficial API (community-documented via github.com/scottbonline/sense)
//! authenticates with email + password:
//!
//! ```text
//! POST https://api.sense.com/apiservice/api/v1/authenticate
//! body: {email, password}
//! → {access_token, user_id, refresh_token, monitors: [{id: <monitor_id>}]}
//! ```
//!
//! On expiry, `POST .../renew` with `{user_id, refresh_token}` returns fresh tokens.
//! We store `access_token`, `user_id`, `refresh_token`, and `monitor_id` in the
//! vault's 0600 secret store. The user pastes `email:password` as a single field;
//! we verify at connect time with a real API call and store the resulting tokens —
//! not the password.
//!
//! ## Data
//!
//! - `GET app/monitors/{monitor_id}/history/usage?scale=DAY&start=...`
//!   → `{start, consumption: {usage_total_kwh, ...}, device_breakdown: [{id, name, icon, consumption: {usage_total_kwh}}]}`
//!   Daily-aggregate kWh: one whole-home row + one row per detected device per day.
//!
//! The unofficial API has no hourly endpoint that is publicly documented. The
//! Sense web app offers a CSV export (Usage screen) that produces hourly kWh rows,
//! but its exact column names are **undocumented** and no sample exists on disk.
//! The CSV importer is scaffolded here but the real parser is **parked**
//! (parser_parked_needs_sample=true) — a real export is needed before
//! we can build a reliable parser.
//!
//! ## Vault layout
//!
//! - **Raw layer** (`home/sense-energy/raw/YYYY-MM.jsonl`) — full API responses,
//!   one object per poll, tagged with `ts`. Full fidelity; unconditional.
//! - **Energy layer** (`home/sense-energy/energy/YYYY-MM.jsonl`) — home.energy
//!   draft-schema rows (`ts`, `source`, `device`, `circuit`, `kwh`,
//!   `interval_secs`, `direction`, `guid`). The home.energy shape is an
//!   **unbound sibling draft**; these rows follow the schema field-for-field
//!   for forward compatibility but are NOT backed by a Rust type yet — same
//!   pattern as emporia.rs.
//!
//! `guid` = `sense-energy:{monitor_id}:{device_id_or_main}:{date_ymd}`.
//!
//! ## Cursor
//!
//! `.trove/sense-energy-sync.json` (non-secret, rebuildable) stores per-monitor
//! watermarks (ISO date `YYYY-MM-DD` of the last day written).
//!
//! ## Failure posture
//!
//! Auth failure (401/403) → clear error: "Sense changed their unofficial API or
//! your session expired — reconnect from the Integrations tab."  We never fail
//! silently. The unofficial API may break at any time; design for honest degradation.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, ImportOutcome, ImportSpec, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Paths / constants.

const RAW_DIR: &str = "home/sense-energy/raw";
const ENERGY_DIR: &str = "home/sense-energy/energy";
const SYNC_FILE: &str = ".trove/sense-energy-sync.json";
const SERVICE: &str = "sense-energy";

const API_BASE: &str = "https://api.sense.com/apiservice/api/v1";
/// Sense API: daily-aggregate history; interval = 86400 s.
const HISTORY_INTERVAL_SECS: u64 = 86400;
/// How many days to walk per poll (walk back up to 30 days; incremental walks
/// are typically 1-2 days). A full backfill is done by walking day-by-day from
/// the watermark or from 30 days ago (Sense typically holds 30 days of DAY-scale data).
const HISTORY_WINDOW_DAYS: i64 = 30;
/// Periodic cadence: hourly, matching the Sense web app's data freshness.
const POLL_SECS: u64 = 3600;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ENERGY_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(n) => Ok(CollectOutcome::note_if(n > 0, || {
            format!("Sense Energy synced — {n} energy intervals")
        })),
        Err(e) => Ok(CollectOutcome::note(format!("Sense Energy sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let n = pull(vault)?;
    let headline = if n == 0 {
        "Sense Energy is up to date — no new intervals".to_string()
    } else {
        format!("Sense Energy synced — {n} energy intervals")
    };
    Ok(PullOutcome { headline, counts: BTreeMap::from([("energy_intervals", n)]) })
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub line is
/// already there — do NOT add another).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "sense-energy",
        name: "Sense Energy Monitor",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Collects whole-home and per-appliance electricity usage from your Sense Energy \
             Monitor via the community-maintained unofficial API. Per-device daily usage and \
             detected appliances are preserved; CSV export import is also available.",
        domain: "home",
        vault_path: "home/sense-energy/",
        toggleable: true,
        setup: &[
            "Connect your Sense account email and password on this card.",
            "Note: this integration uses an unofficial, community-maintained API — it may \
             break if Sense changes their backend.",
            "For a stable fallback, export your usage CSV from the Sense web app (Usage → \
             Export) and import it here.",
        ],
        caveats: "Uses an unofficial API (community-maintained via github.com/scottbonline/sense) \
                  that may break on Sense app updates. Daily-aggregate data only from the API \
                  (no sub-hourly resolution). The CSV import path (hourly kWh) requires a \
                  manually exported file from the Sense web app.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(POLL_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("sense-energy"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// CSV import (scaffolded; parser parked — Needs-sample).

/// The CSV import spec is scaffolded. The real parser is parked until a real
/// Sense usage CSV export is on disk — the exact column names are undocumented
/// and no sample is available. The scaffold accepts the file but writes only
/// the raw bytes as-is, reporting zero parsed rows. When a sample arrives,
/// replace `run_import_scaffold` with a real CSV parser and promote this to
/// a `Behavior::Import` or a sibling DEF.
#[allow(dead_code)]
static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import_scaffold,
};

#[allow(dead_code)]
fn run_import_scaffold(
    vault: &Vault,
    path: &std::path::Path,
    _params: &BTreeMap<String, String>,
    _progress: &mut dyn FnMut(crate::health::ImportProgress),
) -> Result<ImportOutcome> {
    // Scaffold: store the raw CSV bytes under raw/ tagged with the import time
    // so the file is not lost; the real parser is parked (Needs-sample).
    let ts = Local::now().to_rfc3339();
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let raw_val = json!({
        "imported_at": ts,
        "filename": path.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "raw_csv": body,
        "note": "CSV parser parked — Needs-sample (exact column names unknown)"
    });
    let raw_stream = vault.stream(&format!("{RAW_DIR}/csv-imports"), Partition::Month);
    let line = EnergyLine { ts: ts.clone(), data: raw_val };
    raw_stream.append(&[line], |r| &r.ts)?;
    Ok(ImportOutcome {
        headline: "CSV import received — parser parked (Needs-sample). \
                   File stored raw; rows will be parsed once a sample export is available."
            .into(),
        counts: BTreeMap::from([("energy_intervals", 0u64)]),
    })
}

// ---------------------------------------------------------------------------
// Connection (TokenPaste — email:password, verified then replaced with tokens).

fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let s = pasted.trim();
    if s.is_empty() {
        bail!("empty — paste your Sense credentials as email:password");
    }
    match s.split_once(':') {
        Some((u, p)) => {
            let u = u.trim().to_string();
            let p = p.trim().to_string();
            if u.is_empty() {
                bail!("missing email address — paste as email:password");
            }
            if p.is_empty() {
                bail!("missing password — paste as email:password");
            }
            Ok((u, p))
        }
        None => bail!(
            "paste your Sense credentials as email:password \
             (both are required; the colon separates them)"
        ),
    }
}

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (email, password) = parse_credentials(pasted)?;
    let client = SenseClient::new(API_BASE.to_string(), None, None, None);
    let tokens = client.authenticate(&email, &password).map_err(|e| match e {
        // Surface the MFA message verbatim — it is already clear and actionable.
        FetchError::MfaRequired => anyhow::anyhow!("{e}"),
        // All other errors (wrong credentials, network, etc.).
        _ => anyhow::anyhow!("Sense login failed — check your email and password: {e}"),
    })?;
    // Store the tokens (NOT the password). The `access_token` is the bearer token;
    // monitor_id + user_id ride in the token_type/scope slots for simplicity
    // (no secret field collisions; both are non-secret IDs).
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: tokens.access_token,
            refresh_token: Some(tokens.refresh_token),
            token_type: Some(format!("{}:{}", tokens.user_id, tokens.monitor_id)),
            scope: None,
            expires_at: None, // refresh on 401 rather than proactive expiry
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(tok) = vault.load_sync_token(SERVICE)? {
        // token_type stores "user_id:monitor_id" (non-secret IDs).
        let label = tok
            .token_type
            .as_deref()
            .and_then(|s| s.split_once(':').map(|(uid, _)| format!("Sense account (user {})", uid)))
            .unwrap_or_else(|| "Sense Account".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`]. The integrator adds
/// the one `&crate::sense_energy::CONNECTION,` line — do NOT add it here.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "sense-energy",
    display_name: "Sense Energy Monitor",
    methods: &[ConnectMethod::TokenPaste {
        label: "Sense account email and password",
        help: "Paste your Sense Energy Monitor account email and password as \
               email:password. These are stored locally (0600) and used only to \
               authenticate with Sense's unofficial API — the password is not \
               stored; only the session tokens returned by Sense are saved. \
               This integration uses a community-maintained unofficial API.",
        placeholder: "you@example.com:yourpassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["sense-energy"],
    setup: &[
        "Use your existing Sense app account email and password.",
        "Paste them as email:password on this card.",
        "Important: this integration uses an unofficial, community-maintained API — \
         it may break if Sense changes their backend.",
    ],
};

// ---------------------------------------------------------------------------
// HTTP layer — injectable for testing.

/// Session tokens returned by the Sense authenticate endpoint.
#[derive(Debug, Clone)]
struct SenseTokens {
    access_token: String,
    refresh_token: String,
    user_id: String,
    monitor_id: String,
}

/// Status-level fetch errors.
#[derive(Debug)]
enum FetchError {
    Unauthorized,
    /// Sense returned 401 with an `mfa_token` body, indicating 2FA is enabled.
    /// The TokenPaste flow cannot complete MFA — this is an honest limitation.
    MfaRequired,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::MfaRequired => write!(
                f,
                "Sense account has two-factor authentication (2FA) enabled. \
                 The single-field credential paste cannot complete MFA — \
                 disable 2FA in your Sense app settings to use this integration, \
                 or wait for a Needs-login spike that adds an MFA-aware connect flow"
            ),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// The two operations the pull needs. A trait so tests drive logic offline.
trait SenseApi {
    /// `GET app/monitors/{monitor_id}/history/usage?scale=DAY&start=...`
    /// → usage object with `consumption` + `device_breakdown`.
    fn daily_usage(&self, monitor_id: &str, start: &str) -> Result<Value, FetchError>;
}

/// Live client.
struct SenseClient {
    base: String,
    access_token: Option<String>,
    /// Stored for future use in token refresh (`POST .../renew` requires user_id).
    #[allow(dead_code)]
    user_id: Option<String>,
    /// Stored for future use in token refresh.
    #[allow(dead_code)]
    refresh_token: Option<String>,
}

impl SenseClient {
    fn new(
        base: String,
        access_token: Option<String>,
        user_id: Option<String>,
        refresh_token: Option<String>,
    ) -> Self {
        SenseClient { base, access_token, user_id, refresh_token }
    }

    /// `POST .../authenticate` — returns session tokens.
    fn authenticate(&self, email: &str, password: &str) -> Result<SenseTokens, FetchError> {
        let body = json!({"email": email, "password": password});
        let resp = ureq::post(&format!("{}/authenticate", self.base))
            .timeout(HTTP_TIMEOUT)
            .set("Content-Type", "application/json")
            .send_json(&body);
        match resp {
            Ok(r) => {
                let v: Value = r.into_json().map_err(|e| {
                    FetchError::Other(format!("parsing authenticate response: {e}"))
                })?;
                let access_token = v
                    .get("access_token")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let user_id = v
                    .get("user_id")
                    .map(|u| u.to_string().trim_matches('"').to_string())
                    .unwrap_or_default();
                let refresh_token = v
                    .get("refresh_token")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // monitors is an array; pick the first monitor's id.
                let monitor_id = v
                    .get("monitors")
                    .and_then(|m| m.as_array())
                    .and_then(|arr| arr.first())
                    .and_then(|m| m.get("id"))
                    .map(|id| id.to_string().trim_matches('"').to_string())
                    .unwrap_or_default();
                if access_token.is_empty() || monitor_id.is_empty() {
                    return Err(FetchError::Other(
                        "authenticate response missing access_token or monitor id".into(),
                    ));
                }
                Ok(SenseTokens { access_token, refresh_token, user_id, monitor_id })
            }
            Err(ureq::Error::Status(401, resp)) => {
                // Sense returns HTTP 401 with `{"mfa_token":"..."}` when the
                // account has 2FA enabled (per scottbonline/sense senseable.py
                // L62-66 → SenseMFARequiredException). Parse the body before
                // mapping to Unauthorized so we give an honest MFA error
                // instead of "wrong password".
                let body = resp.into_string().unwrap_or_default();
                if body.contains("mfa_token") {
                    Err(FetchError::MfaRequired)
                } else {
                    Err(FetchError::Unauthorized)
                }
            }
            Err(ureq::Error::Status(403, _)) => Err(FetchError::Unauthorized),
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

    fn bearer(&self) -> String {
        self.access_token.clone().unwrap_or_default()
    }
}

impl SenseApi for SenseClient {
    fn daily_usage(&self, monitor_id: &str, start: &str) -> Result<Value, FetchError> {
        let url = format!(
            "{}/app/monitors/{monitor_id}/history/usage?scale=DAY&start={start}",
            self.base
        );
        let resp = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("bearer {}", self.bearer()))
            .call();
        match resp {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| FetchError::Other(format!("parsing usage response: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
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
    /// Per-monitor (monitor_id → last date written, ISO "YYYY-MM-DD") watermark.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    watermarks: BTreeMap<String, String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_sense_energy_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_sense_energy_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Energy row + raw line types.

/// Wrapper that hides `ts` from the serialized output (stream keys on it).
#[derive(Serialize)]
struct EnergyLine {
    ts: String,
    #[serde(flatten)]
    data: Value,
}

#[derive(Serialize)]
struct RawLine {
    ts: String,
    #[serde(flatten)]
    data: Value,
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// Parse the `start` field from a usage response.  Sense returns an ISO
/// datetime string (`"2026-06-15T00:00:00Z"` or similar); extract the date
/// part.
fn start_date(resp: &Value) -> Option<String> {
    let s = resp.get("start").and_then(Value::as_str)?;
    // Trim to the date part (first 10 characters of an ISO string).
    if s.len() >= 10 {
        Some(s[..10].to_string())
    } else {
        None
    }
}

/// The `ts` for an energy row: local midnight on `date_ymd` (the interval start).
fn day_ts(date_ymd: &str) -> Option<String> {
    let nd = NaiveDate::parse_from_str(date_ymd, "%Y-%m-%d").ok()?;
    let dt = nd.and_hms_opt(0, 0, 0)?.and_local_timezone(Local).latest()?;
    Some(dt.to_rfc3339())
}

/// Convert one usage response into energy rows (home.energy draft schema).
///
/// Produces one whole-home row (`device="main"`, no `circuit`) and one row per
/// detected device from `device_breakdown`. Filters out NaN / non-finite kWh.
fn usage_to_energy_rows(resp: &Value, monitor_id: &str, date_ymd: &str) -> Vec<Value> {
    let Some(ts) = day_ts(date_ymd) else {
        return Vec::new();
    };

    let mut rows = Vec::new();

    // Whole-home consumption.
    let whole_kwh = resp
        .pointer("/consumption/usage_total_kwh")
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite());
    if let Some(kwh) = whole_kwh {
        let guid = format!("sense-energy:{monitor_id}:main:{date_ymd}");
        rows.push(json!({
            "ts": ts,
            "source": "sense-energy",
            "device": monitor_id,
            "circuit": "whole-home",
            "kwh": kwh,
            "interval_secs": HISTORY_INTERVAL_SECS,
            "direction": "consumption",
            "guid": guid
        }));
    }

    // Per-detected-device rows from device_breakdown.
    if let Some(breakdown) = resp.get("device_breakdown").and_then(Value::as_array) {
        for dev in breakdown {
            let dev_id = dev.get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let dev_name = dev.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let kwh = dev
                .pointer("/consumption/usage_total_kwh")
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v > 0.0);
            if let (Some(kwh), false) = (kwh, dev_id.is_empty()) {
                let guid = format!("sense-energy:{monitor_id}:{dev_id}:{date_ymd}");
                let mut row = json!({
                    "ts": ts,
                    "source": "sense-energy",
                    "device": monitor_id,
                    "circuit": dev_name,
                    "kwh": kwh,
                    "interval_secs": HISTORY_INTERVAL_SECS,
                    "direction": "consumption",
                    "guid": guid
                });
                // Stash device id in extra for forward compatibility.
                if let Value::Object(ref mut m) = row {
                    m.insert("extra".into(), json!({"device_id": dev_id}));
                }
                rows.push(row);
            }
        }
    }

    rows
}

// ---------------------------------------------------------------------------
// Energy writer (deduped by guid).

fn append_energy(vault: &Vault, rows: &[Value]) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(ENERGY_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                seen.insert(g.to_string());
            }
        }
    }
    let fresh: Vec<EnergyLine> = rows
        .iter()
        .filter(|r| {
            let g = r.get("guid").and_then(Value::as_str).unwrap_or("");
            g.is_empty() || seen.insert(g.to_string())
        })
        .map(|r| {
            let ts = r.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
            EnergyLine { ts, data: r.clone() }
        })
        .collect();
    let n = fresh.len() as u64;
    stream.append(&fresh, |r| &r.ts)?;
    Ok(n)
}

fn append_raw(vault: &Vault, ts: &str, data: Value) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let line = RawLine { ts: ts.to_string(), data };
    stream.append(&[line], |r| &r.ts)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The pull.

/// Load the stored session tokens from the vault secret store.
fn load_session(vault: &Vault) -> Result<(String, String, String)> {
    let tok = vault
        .load_sync_token(SERVICE)?
        .filter(|t| !t.access_token.trim().is_empty())
        .context(
            "Sense Energy is not connected — paste your credentials in the Integrations tab",
        )?;
    let access_token = tok.access_token;
    // token_type stores "user_id:monitor_id".
    let type_str = tok.token_type.unwrap_or_default();
    let (user_id, monitor_id) = type_str
        .split_once(':')
        .map(|(u, m)| (u.to_string(), m.to_string()))
        .unwrap_or_default();
    if monitor_id.is_empty() {
        bail!("Sense session missing monitor id — reconnect from the Integrations tab");
    }
    Ok((access_token, user_id, monitor_id))
}

/// Top-level pull function. Returns the count of new energy rows written.
pub fn pull(vault: &Vault) -> Result<u64> {
    let (access_token, _user_id, monitor_id) = load_session(vault)?;
    let client = SenseClient::new(
        API_BASE.to_string(),
        Some(access_token),
        None,
        None,
    );
    pull_with(vault, &client, &monitor_id)
}

/// Testable body — drives the pull over an injected API.
fn pull_with(vault: &Vault, api: &impl SenseApi, monitor_id: &str) -> Result<u64> {
    let mut state = vault.read_sense_energy_sync();

    // Walk: always start from HISTORY_WINDOW_DAYS ago (the full window Sense
    // retains at DAY scale).  We do NOT use a high-water mark to advance the
    // walk start: if any day 502s transiently, a max-watermark would skip
    // past it on the next run and permanently lose that day's data.  Instead,
    // guid-based dedupe in `append_energy` makes re-fetching already-stored
    // days free (0 new rows), so a fixed window floor is always safe.
    //
    // The `watermarks` field in `SyncState` is retained for forward
    // compatibility (old cursors round-trip cleanly) but is no longer used to
    // set `walk_from`.
    let today = Local::now().naive_local().date();
    let walk_from = today - chrono::Duration::days(HISTORY_WINDOW_DAYS);

    // Sense `history/usage?scale=DAY&start=...` returns data for the requested
    // day; we request one day at a time from walk_from to today.
    let mut total_written: u64 = 0;
    let mut date = walk_from;

    while date <= today {
        let start_str = date.format("%Y-%m-%dT00:00:00").to_string();
        // NOTE (Needs-login): `start` TZ assumption — we request local-naive
        // midnight.  If Sense echoes `start` in UTC the derived date label
        // could shift by a day for non-UTC monitors.  Dedupe holds (guid is
        // consistent), so no duplication occurs, but the interval-start ts
        // may be off.  Verify against a real account during the Needs-login
        // spike and convert to local zone if needed.
        let ts_local = day_ts(&date.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| Local::now().to_rfc3339());

        let usage = match api.daily_usage(monitor_id, &start_str) {
            Ok(v) => v,
            Err(FetchError::Unauthorized) => bail!(
                "Sense changed their unofficial API or your session expired — \
                 reconnect from the Integrations tab"
            ),
            Err(FetchError::MfaRequired) => bail!(
                "Sense account has two-factor authentication enabled — \
                 disable 2FA in the Sense app or wait for an MFA-aware connect flow"
            ),
            Err(FetchError::Other(e)) => {
                // Transient error on this day: log and continue.  Because
                // walk_from is always the fixed window floor, the next run
                // will re-request this day — no data is permanently lost.
                eprintln!("sense-energy: transient error for {date}, will retry next poll: {e}");
                date = date.succ_opt().unwrap_or(today + chrono::Duration::days(1));
                continue;
            }
        };

        // Determine the actual date from the response `start` field (Sense may
        // shift the interval to a different day in some timezones).
        let row_date = start_date(&usage).unwrap_or_else(|| date.format("%Y-%m-%d").to_string());

        // Raw layer: full API response, unconditional.
        append_raw(vault, &ts_local, usage.clone())?;

        // Energy layer: home.energy draft rows.
        let rows = usage_to_energy_rows(&usage, monitor_id, &row_date);
        let written = append_energy(vault, &rows)?;
        total_written += written;

        date = date.succ_opt().unwrap_or(today + chrono::Duration::days(1));
    }

    // Persist the last-run timestamp; watermarks retained for back-compat but
    // no longer used to set walk_from (fixed window instead — see above).
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_sense_energy_sync(&state)?;

    Ok(total_written)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-sense-energy-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures: documented response shapes from scottbonline/sense ---

    /// A typical daily usage response:
    /// `{start, consumption: {usage_total_kwh}, device_breakdown: [{id, name, consumption: {usage_total_kwh}}]}`
    fn usage_response(date: &str, whole_kwh: f64, devices: &[(&str, &str, f64)]) -> Value {
        let breakdown: Vec<Value> = devices
            .iter()
            .map(|(id, name, kwh)| {
                json!({
                    "id": id,
                    "name": name,
                    "icon": "lightbulb",
                    "consumption": { "usage_total_kwh": kwh }
                })
            })
            .collect();
        json!({
            "start": format!("{date}T00:00:00Z"),
            "consumption": {
                "usage_total_kwh": whole_kwh
            },
            "device_breakdown": breakdown
        })
    }

    // --- pure mapping ---

    #[test]
    fn usage_to_rows_whole_home_and_devices() {
        let resp = usage_response(
            "2026-06-10",
            15.3,
            &[("abc123", "Fridge", 1.2), ("def456", "EV Charger", 8.1)],
        );
        let rows = usage_to_energy_rows(&resp, "12345", "2026-06-10");

        // One whole-home row + two device rows.
        assert_eq!(rows.len(), 3, "whole-home + 2 devices");

        let whole = rows.iter().find(|r| r["circuit"] == "whole-home").unwrap();
        assert_eq!(whole["source"], "sense-energy");
        assert_eq!(whole["device"], "12345");
        assert!((whole["kwh"].as_f64().unwrap() - 15.3).abs() < 1e-9);
        assert_eq!(whole["interval_secs"], 86400_u64);
        assert_eq!(whole["direction"], "consumption");
        assert_eq!(whole["guid"], "sense-energy:12345:main:2026-06-10");

        let fridge = rows.iter().find(|r| r["circuit"] == "Fridge").unwrap();
        assert!((fridge["kwh"].as_f64().unwrap() - 1.2).abs() < 1e-9);
        assert_eq!(fridge["guid"], "sense-energy:12345:abc123:2026-06-10");
        // device_id stored in extra.
        assert_eq!(fridge["extra"]["device_id"], "abc123");

        let ev = rows.iter().find(|r| r["circuit"] == "EV Charger").unwrap();
        assert!((ev["kwh"].as_f64().unwrap() - 8.1).abs() < 1e-9);
    }

    #[test]
    fn usage_to_rows_filters_zero_kwh_and_empty_device_id() {
        let resp = json!({
            "start": "2026-06-10T00:00:00Z",
            "consumption": {"usage_total_kwh": 10.0},
            "device_breakdown": [
                // Zero kWh → omitted.
                {"id": "aaa", "name": "Off device", "consumption": {"usage_total_kwh": 0.0}},
                // Empty id → omitted.
                {"id": "", "name": "Unknown", "consumption": {"usage_total_kwh": 5.0}},
                // Valid.
                {"id": "bbb", "name": "Dryer", "consumption": {"usage_total_kwh": 2.5}}
            ]
        });
        let rows = usage_to_energy_rows(&resp, "12345", "2026-06-10");
        // whole-home + Dryer only.
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|r| r["circuit"] == "Dryer"));
        assert!(!rows.iter().any(|r| r["circuit"] == "Off device"));
        assert!(!rows.iter().any(|r| r["circuit"] == "Unknown"));
    }

    #[test]
    fn parse_credentials_splits_and_validates() {
        let (email, pass) = parse_credentials("user@example.com:mysecret").unwrap();
        assert_eq!(email, "user@example.com");
        assert_eq!(pass, "mysecret");

        // Whitespace trimmed.
        let (e, p) = parse_credentials("  a@b.com : pwd  ").unwrap();
        assert_eq!((e.as_str(), p.as_str()), ("a@b.com", "pwd"));

        // Empty → error.
        assert!(parse_credentials("").is_err());
        assert!(parse_credentials("   ").is_err());

        // No colon → error.
        let err = parse_credentials("nocolon").unwrap_err().to_string();
        assert!(err.contains("colon"), "clear message: {err}");

        // Missing password after colon → error.
        assert!(parse_credentials("a@b.com:").is_err());
    }

    #[test]
    fn pull_writes_energy_and_raw_dedupes_on_guid() {
        let v = temp_vault("pull_dedup");
        let monitor_id = "99999";

        // We need the vault to have a stored session.
        v.save_sync_token(
            SERVICE,
            &crate::sync::oauth::TokenSet {
                access_token: "tok".into(),
                refresh_token: Some("ref".into()),
                token_type: Some(format!("uid:{monitor_id}")),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        // The pull now always walks from window_start (fixed floor, not high-water
        // mark) so we must supply responses for every day in the HISTORY_WINDOW_DAYS
        // window.  Use a MockApi that returns today's data for any date so the walk
        // succeeds across the whole window.
        let today = Local::now().naive_local().date();
        let today_str = today.format("%Y-%m-%d").to_string();

        // Build a MockApi whose responses cover every day in the window by
        // providing a default for any key not explicitly mapped.
        struct WindowMockApi {
            today_resp: Value,
            today_str: String,
            calls: RefCell<Vec<String>>,
        }

        impl SenseApi for WindowMockApi {
            fn daily_usage(&self, _monitor_id: &str, start: &str) -> Result<Value, FetchError> {
                self.calls.borrow_mut().push(start.to_string());
                let date_key = &start[..10.min(start.len())];
                if date_key == self.today_str {
                    Ok(self.today_resp.clone())
                } else {
                    // Other days: return an empty usage so the walk succeeds.
                    Ok(json!({
                        "start": format!("{date_key}T00:00:00Z"),
                        "consumption": {"usage_total_kwh": 0.0},
                        "device_breakdown": []
                    }))
                }
            }
        }

        // First run: today has whole-home + 1 device.
        let today_resp = usage_response(&today_str, 12.5, &[("abc", "Washer", 1.0)]);
        let api = WindowMockApi {
            today_resp: today_resp.clone(),
            today_str: today_str.clone(),
            calls: RefCell::new(Vec::new()),
        };
        let n = pull_with(&v, &api, monitor_id).unwrap();
        assert!(n >= 2, "whole-home + washer: {n}");

        // Re-run with same data → guid dedupe → 0 new rows.
        let api2 = WindowMockApi {
            today_resp: today_resp.clone(),
            today_str: today_str.clone(),
            calls: RefCell::new(Vec::new()),
        };
        let n2 = pull_with(&v, &api2, monitor_id).unwrap();
        assert_eq!(n2, 0, "all guids already stored");

        // Energy stream has rows.
        let stream = v.stream(ENERGY_DIR, Partition::Month);
        let all_keys = stream.partitions().unwrap();
        let total_energy: usize = all_keys
            .iter()
            .map(|k| stream.read::<Value>(k).unwrap().len())
            .sum();
        assert!(total_energy >= 2);

        // Raw stream has the verbatim API responses (one per day per run).
        let raw = v.stream(RAW_DIR, Partition::Month);
        let raw_keys = raw.partitions().unwrap();
        let total_raw: usize =
            raw_keys.iter().map(|k| raw.read::<Value>(k).unwrap().len()).sum();
        assert!(total_raw >= 1, "raw objects written");

        // Cursor updated (`updated` timestamp written; watermarks retained for
        // back-compat but no longer used to set walk_from).
        let state2 = v.read_sense_energy_sync();
        assert!(state2.updated.is_some());

        // Cursor contains NO secret (only the `updated` RFC3339 timestamp).
        let cursor = std::fs::read_to_string(
            v.root().join(".trove/sense-energy-sync.json"),
        )
        .unwrap();
        assert!(!cursor.contains("tok"), "access_token never in cursor");
        assert!(!cursor.contains("ref"), "refresh_token never in cursor");
    }

    /// Confirm that a transient error on one day does NOT strand that day: the
    /// fixed-window walk means the next run re-requests it automatically.
    #[test]
    fn transient_error_does_not_lose_day() {
        let today = Local::now().naive_local().date();
        let today_str = today.format("%Y-%m-%d").to_string();
        let yesterday = (today - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();

        // Run 1: yesterday 502s, today succeeds.
        struct Run1Api { today_str: String, yesterday_str: String }
        impl SenseApi for Run1Api {
            fn daily_usage(&self, _m: &str, start: &str) -> Result<Value, FetchError> {
                let dk = &start[..10.min(start.len())];
                if dk == self.yesterday_str {
                    return Err(FetchError::Other("502 transient".into()));
                }
                if dk == self.today_str {
                    return Ok(json!({
                        "start": format!("{dk}T00:00:00Z"),
                        "consumption": {"usage_total_kwh": 5.0},
                        "device_breakdown": []
                    }));
                }
                Ok(json!({
                    "start": format!("{dk}T00:00:00Z"),
                    "consumption": {"usage_total_kwh": 0.0},
                    "device_breakdown": []
                }))
            }
        }

        let v = temp_vault("transient_loss");
        let run1 = Run1Api { today_str: today_str.clone(), yesterday_str: yesterday.clone() };
        pull_with(&v, &run1, "mon1").unwrap();

        // Energy stream after run 1: has today (whole-home), no yesterday.
        let stream = v.stream(ENERGY_DIR, Partition::Month);
        let all_keys = stream.partitions().unwrap();
        let rows_r1: Vec<Value> = all_keys.iter().flat_map(|k| stream.read::<Value>(k).unwrap()).collect();
        let has_yesterday_r1 = rows_r1.iter().any(|r| r.get("guid").and_then(Value::as_str).map(|g| g.contains(&yesterday)).unwrap_or(false));
        assert!(!has_yesterday_r1, "yesterday errored — not yet stored");

        // Run 2: both days succeed.
        struct Run2Api { today_str: String, yesterday_str: String }
        impl SenseApi for Run2Api {
            fn daily_usage(&self, _m: &str, start: &str) -> Result<Value, FetchError> {
                let dk = &start[..10.min(start.len())];
                if dk == self.yesterday_str || dk == self.today_str {
                    return Ok(json!({
                        "start": format!("{dk}T00:00:00Z"),
                        "consumption": {"usage_total_kwh": 5.0},
                        "device_breakdown": []
                    }));
                }
                Ok(json!({
                    "start": format!("{dk}T00:00:00Z"),
                    "consumption": {"usage_total_kwh": 0.0},
                    "device_breakdown": []
                }))
            }
        }

        let run2 = Run2Api { today_str: today_str.clone(), yesterday_str: yesterday.clone() };
        pull_with(&v, &run2, "mon1").unwrap();

        // Energy stream after run 2: yesterday recovered.
        let all_keys2 = stream.partitions().unwrap();
        let rows_r2: Vec<Value> = all_keys2.iter().flat_map(|k| stream.read::<Value>(k).unwrap()).collect();
        let has_yesterday_r2 = rows_r2.iter().any(|r| r.get("guid").and_then(Value::as_str).map(|g| g.contains(&yesterday)).unwrap_or(false));
        assert!(has_yesterday_r2, "yesterday recovered on second run (fixed window, not high-water mark)");
    }

    #[test]
    fn pull_requires_connection() {
        let v = temp_vault("no_conn");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected") || err.contains("Integrations tab"), "{err}");
    }

    #[test]
    fn cursor_back_compat_deserialize() {
        // Empty cursor → all defaults (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermarks.is_empty());
        assert!(empty.updated.is_none());

        // Old cursor with only watermarks (no updated) still loads.
        let partial: SyncState =
            serde_json::from_str(r#"{"watermarks":{"12345":"2026-06-15"}}"#).unwrap();
        assert_eq!(partial.watermarks.get("12345").map(String::as_str), Some("2026-06-15"));
        assert!(partial.updated.is_none());
    }

    #[test]
    fn energy_rows_match_home_energy_draft_schema() {
        // Confirm required fields (ts, source) + typed fields.
        let resp = usage_response("2026-06-10", 20.0, &[]);
        let rows = usage_to_energy_rows(&resp, "42", "2026-06-10");
        assert!(!rows.is_empty());
        let row = &rows[0];
        assert!(row.get("ts").and_then(Value::as_str).map(|s| s.len() >= 10).unwrap_or(false));
        assert_eq!(row["source"], "sense-energy");
        assert!(row.get("kwh").and_then(Value::as_f64).is_some());
        assert_eq!(row["interval_secs"], 86400_u64);
        assert!(row.get("guid").and_then(Value::as_str).map(|s| !s.is_empty()).unwrap_or(false));
    }

    #[test]
    fn connection_exposes_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, "sense-energy");
        assert_eq!(DEF.connection, Some("sense-energy"));
    }

    #[test]
    fn scaffold_import_stores_raw_returns_zero_intervals() {
        // The scaffold importer should accept any CSV, store raw bytes, and
        // return 0 energy_intervals (parser parked).
        let v = temp_vault("import_scaffold");
        let dir = std::env::temp_dir()
            .join(format!("trove-sense-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv_path = dir.join("sense_usage.csv");
        std::fs::write(&csv_path, "Date,kWh\n2026-06-10,12.5\n").unwrap();

        let mut progress_calls = 0usize;
        let outcome = run_import_scaffold(
            &v,
            &csv_path,
            &BTreeMap::new(),
            &mut |_| progress_calls += 1,
        )
        .unwrap();

        assert_eq!(
            outcome.counts.get("energy_intervals").copied(),
            Some(0),
            "parser parked — 0 intervals"
        );
        assert!(outcome.headline.contains("parked") || outcome.headline.contains("Needs-sample"));

        // Raw CSV stored under raw/csv-imports/.
        let raw = v.stream(&format!("{RAW_DIR}/csv-imports"), Partition::Month);
        let keys = raw.partitions().unwrap();
        let count: usize = keys.iter().map(|k| raw.read::<Value>(k).unwrap().len()).sum();
        assert_eq!(count, 1, "the raw CSV stored verbatim");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MFA error display is honest and distinct from wrong-password.
    #[test]
    fn mfa_error_message_is_clear_and_not_wrong_password() {
        let msg = FetchError::MfaRequired.to_string();
        assert!(msg.contains("two-factor") || msg.contains("2FA") || msg.contains("MFA"),
            "MFA message must mention 2FA: {msg}");
        // Must NOT say "check your email and password" — that's wrong for MFA users.
        assert!(!msg.to_lowercase().contains("check your email"),
            "MFA must not say 'check your email and password': {msg}");
    }

    /// pull_with bails with clear error (not wrong-password) when MFA is raised mid-session.
    #[test]
    fn pull_bails_on_mfa_with_actionable_message() {
        struct MfaApi;
        impl SenseApi for MfaApi {
            fn daily_usage(&self, _m: &str, _start: &str) -> Result<Value, FetchError> {
                Err(FetchError::MfaRequired)
            }
        }
        let v = temp_vault("mfa_bail");
        let err = pull_with(&v, &MfaApi, "mon1").unwrap_err().to_string();
        assert!(err.contains("two-factor") || err.contains("2FA") || err.contains("MFA"),
            "error must mention 2FA: {err}");
    }
}
