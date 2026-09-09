//! Dexcom CGM — continuous glucose monitor cloud sync. Catalogued in the
//! Phase 2 pass; brief: docs/integrations/dexcom.md. **First collector in the
//! `health-medical` domain** — this build binds the `observation` contract
//! (see [`crate::health_medical`] / [`crate::contracts`]).
//!
//! A **Periodic** cloud pull over the Dexcom API v3 (`api.dexcom.com`; the dev
//! sandbox is `sandbox-api.dexcom.com`). Dexcom CGMs (G6, G7, ONE, ONE+) report
//! an estimated glucose value (EGV) every ~5 minutes — ~288 readings/day. Each
//! EGV becomes a [`crate::health_medical::Observation`] under
//! `health/medical/dexcom/observations/YYYY-MM.jsonl`:
//!
//! - `guid` = the EGV `recordId` (a stable per-reading id, the dedupe key).
//! - `ts` = `systemTime` (the device's UTC clock) converted to local. Receiver-
//!   sourced records carry a naive (offset-less) UTC stamp; we treat a missing
//!   offset as UTC, so every row partitions and sorts honestly.
//! - `test` = `"Glucose"`, `code` = LOINC `2339-0` (Glucose [Mass/volume] in
//!   Blood), `value` = the integer mg/dL reading, `unit` = `"mg/dL"`.
//! - `trend` / `trendRate` / `status` / `displayTime` / device ids ride in
//!   `extra` (CGM-specific, no contract column).
//!
//! Two layers: the **raw** EGV object verbatim under
//! `health/medical/dexcom/raw/YYYY-MM.jsonl` (full fidelity, unconditional), and
//! the normalized **contract** observation rows, deduped by `guid` against
//! what's already on disk.
//!
//! ## Windowed backfill (the cursor)
//!
//! The `/egvs` endpoint takes a `startDate`/`endDate` window (ISO 8601 UTC, no
//! offset) of at most 30 days, inclusive of start and exclusive of end. We first
//! read `/dataRange` for the account's EGV span, then walk forward from the
//! persisted watermark (or the account's first EGV on a cold start) in ≤30-day
//! windows up to the account's last EGV, draining each window fully. We persist
//! the watermark (the max `systemTime` ingested, as UTC) in
//! `.trove/dexcom-sync.json` (non-secret, rebuildable) and advance it only after
//! a window's full write, so a crash re-drains that window rather than skipping.
//!
//! Coexists with the existing raw `health/` writers (garmin/oura/apple-health
//! write `health/<metric>/…`, unbound) — this writes the *medical* contract
//! subtree (`health/medical/dexcom/`) and touches neither.
//!
//! ## Auth (OAuth 2.0, a SECRET)
//!
//! Single-use OAuth (not the shared Google login): the user registers an app at
//! developer.dexcom.com (self-service is capped at 5 authorized users — a
//! Needs-David app-registration flag, recorded in the connect-card copy; BYO app
//! credentials mitigate the cap). Confidential client: the client id/secret post
//! in the token form body. Credentials resolve explicit → saved → compiled-in
//! (`TROVE_DEXCOM_CLIENT_ID` / `TROVE_DEXCOM_CLIENT_SECRET`, empty baked
//! default). The access token (and its refresh token) live 0600 under
//! `.trove/sync/`, never in the cursor or any non-secret file.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::health_medical::Observation;
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, Provider, TokenSet};
use crate::vault::Vault;

/// Contract-layer observation stream; raw EGVs nest under `raw/`. The medical
/// contract subtree, distinct from the unbound raw `health/<metric>/` writers.
const DIR: &str = "health/medical/dexcom/observations";
const RAW_DIR: &str = "health/medical/dexcom/raw";

/// Non-secret rebuildable cursor — NOT under `.trove/sync/` (that's for 0600
/// secrets). Deleting it just re-walks the account's EGV history from the start.
const SYNC_FILE: &str = ".trove/dexcom-sync.json";

/// Service id under `.trove/sync/` where the OAuth token is stored.
const SERVICE: &str = "dexcom";

/// LOINC code for "Glucose [Mass/volume] in Blood" — the analyte a CGM reports,
/// so cross-source glucose (Dexcom EGVs, a Quest fasting glucose, an Epic
/// bundle) lines up on one axis at read time.
const GLUCOSE_LOINC: &str = "2339-0";

/// The Dexcom API base. Production US; the EU/JP hosts and the sandbox are valid
/// substitutions a future build can wire — the personal pull targets US.
const API_BASE: &str = "https://api.dexcom.com";
/// Kept short so a hung connection can't stall the watcher owner loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs in the watcher loop. Hourly: EGVs trickle in at ~5-min
/// resolution and the incremental window poll is cheap when caught up.
pub const DEXCOM_SYNC_SECS: u64 = 3600;

/// Max query window the `/egvs` endpoint allows (30 days). We walk the backfill
/// in windows no larger than this.
const MAX_WINDOW_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// OAuth provider (the ticktick precedent: confidential client, fixed redirect
// port). Dexcom posts client creds in the token form body (not Basic auth).

pub static DEXCOM: Provider = Provider {
    service: SERVICE,
    display_name: "Dexcom",
    // v3 OAuth: the login page and the token exchange both live under
    // /v2/oauth2 on the API host (the path stayed v2 across the v3 API cutover).
    auth_url: "https://api.dexcom.com/v2/oauth2/login",
    token_url: "https://api.dexcom.com/v2/oauth2/token",
    // The only scope Dexcom exposes; a user can't authorize a subset.
    scopes: "offline_access",
    // NEW unique fixed port (38573-38578 are taken:
    // ticktick/oura/google/trakt/microsoft/simkl). Must match the redirect URI
    // registered in the Dexcom app.
    redirect_port: 38579,
    use_pkce: false,
    // Dexcom wants client_id/client_secret in the token request *body*, not as
    // HTTP Basic auth.
    basic_auth: false,
    // Bake credentials in at build time for a "just log in" experience:
    // TROVE_DEXCOM_CLIENT_ID / TROVE_DEXCOM_CLIENT_SECRET (empty default — app
    // registration is a Needs-David flag).
    default_client_id: option_env!("TROVE_DEXCOM_CLIENT_ID"),
    default_client_secret: option_env!("TROVE_DEXCOM_CLIENT_SECRET"),
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// Periodic pass: the same pull "Sync now" runs, but it never errors the loop —
// a missing token or a network blip is a quiet no-op until the next tick.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("dexcom synced — {total} glucose readings")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!("dexcom sync skipped: {e}"))),
    }
}

// Manual "Sync now": surfaces errors (not connected) to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "dexcom",
        name: "Dexcom",
        kind: IntegrationKind::CloudSync,
        // Continuous glucose is medical data — opt-in with explicit
        // acknowledgement (the health-medical domain rule).
        default_on: false,
        description:
            "Pulls continuous glucose readings from your Dexcom CGM (G6, G7, ONE, ONE+) \
             via the Dexcom API v3 into the unified medical store. First sync backfills your \
             history; later syncs fetch only new readings.",
        domain: "health",
        vault_path: "health/medical/dexcom/",
        toggleable: true,
        setup: &[
            "Continuous glucose is sensitive medical data — enabling this opts you in to \
             collecting it.",
            "Connect your Dexcom account on this card (you'll register a developer app first — \
             see the connection's setup steps).",
            "First sync backfills your available glucose history; later syncs are incremental.",
        ],
        caveats:
            "Dexcom's self-service API access is capped at 5 authorized users per app — fine for \
             a personal pull, a scale limit if shared broadly (bring your own app credentials to \
             mitigate). Stelo (the OTC CGM) is not covered by this API; Stelo users export from \
             Clarity instead.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(DEXCOM_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("dexcom"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (OAuth = a Dexcom login, a SECRET). The ticktick precedent:
// explicit → saved → compiled-in credential resolution; single-account.

/// [`ConnectMethod::OAuth`] adapter: forward to [`connect`] (which owns the
/// credential resolution) and drop the token — callers re-read state through the
/// status hook.
fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

/// `configured` mirrors the ticktick status hook: app credentials saved or
/// compiled in, so connecting is just a login. At most one account; a saved but
/// expired token flags `needs_reconnect` only if it carries no refresh token
/// (Dexcom issues a refresh token with `offline_access`, so the pull refreshes
/// silently and expiry is rarely surfaced).
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(DEXCOM.service)?.is_some()
        || DEXCOM.default_credentials().is_some();
    let accounts = match vault.load_sync_token(DEXCOM.service)? {
        Some(token) => vec![ConnectedAccount {
            key: DEXCOM.service.to_string(),
            label: DEXCOM.display_name.to_string(),
            connected_at: None,
            expires_at: token.expires_at,
            // A refreshable token is never "needs reconnect" just for being
            // expired — the pull refreshes it. Only a tokenless-refresh expiry
            // would, which `offline_access` avoids.
            needs_reconnect: token.expired() && token.refresh_token.is_none(),
            extra: BTreeMap::new(),
        }],
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

/// Forget the token; app credentials are kept so reconnecting is just a login.
/// Synced data and the cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`]. Single-use OAuth (NOT the
/// shared Google login). Bring-your-own-app: Dexcom requires a self-registered
/// developer app (5-user self-service cap).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "dexcom",
    display_name: "Dexcom",
    methods: &[ConnectMethod::OAuth {
        provider: &DEXCOM,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["dexcom"],
    setup: &[
        "Sign in at developer.dexcom.com and create an app (self-service access is capped at 5 \
         users, which is plenty for your own account).",
        "Set its OAuth redirect URI to http://localhost:38579/callback — must match exactly.",
        "Paste the app's Client ID and Client Secret here. They're saved, so every future \
         connect is just a login.",
    ],
};

/// Interactive connect: opens the consent page, waits for the redirect, saves
/// the token. Blocking — callers off the main thread only. App credentials
/// resolve explicit → saved → compiled-in defaults.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(DEXCOM.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(DEXCOM.service)?
            .or_else(|| DEXCOM.default_credentials())
            .context("no Dexcom app credentials — register an app at developer.dexcom.com and enter its id/secret in the Integrations tab")?,
    };
    let flow = oauth::OauthFlow::start(&DEXCOM, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(DEXCOM.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable so tests run fully offline.

/// Status-level fetch errors: 401 wants distinct handling (refresh then retry),
/// 429 is transient, everything else is a message.
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

/// The endpoints the pull needs. A trait so tests drive the mapping/persist
/// logic with fixtures, never the network.
trait DexcomApi {
    /// `GET /v3/users/self/dataRange` → the account's data spans (we read the
    /// `egvs` window). The full JSON value.
    fn data_range(&self, token: &str) -> Result<Value, FetchError>;

    /// `GET /v3/users/self/egvs?startDate=..&endDate=..` for one ≤30-day window.
    /// `start`/`end` are ISO 8601 UTC, no offset. The full JSON value.
    fn egvs(&self, token: &str, start: &str, end: &str) -> Result<Value, FetchError>;
}

/// Thin client; base URL injected (the readwise/oura/lastfm pattern).
struct DexcomClient {
    base: String,
}

impl DexcomClient {
    fn new(base: String) -> Self {
        DexcomClient { base }
    }

    fn get(&self, url: &str, token: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
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

impl DexcomApi for DexcomClient {
    fn data_range(&self, token: &str) -> Result<Value, FetchError> {
        self.get(&format!("{}/v3/users/self/dataRange", self.base), token)
    }

    fn egvs(&self, token: &str, start: &str, end: &str) -> Result<Value, FetchError> {
        let url = format!(
            "{}/v3/users/self/egvs?startDate={}&endDate={}",
            self.base,
            urlencode(start),
            urlencode(end)
        );
        self.get(&url, token)
    }
}

/// Minimal query-string encoder for the ISO timestamp window params (the ':'
/// must be escaped). Mirrors the oauth module's idiom.
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
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max EGV `systemTime` ingested, as a naive UTC ISO 8601 string
    /// (`YYYY-MM-DDTHH:MM:SS`) — the inclusive lower bound (exclusive in effect,
    /// since guid dedupe drops the boundary reading) for the next window walk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    egvs_through: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_dexcom_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_dexcom_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Raw row shape (full-fidelity EGV object). The on-disk line is the verbatim
// EGV record, tagged with the contract ts purely so the month-partition writer
// files it under the right month. Only `value` is serialized.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Pure mapping (fixture-tested).

/// A top-level string field, trimmed; "" when missing/non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Parse a Dexcom `systemTime`/`displayTime` (ISO 8601 UTC; receiver records
/// carry no offset) into a UTC [`DateTime`]. Tries RFC3339 (offset/Z) first,
/// then a naive `YYYY-MM-DDTHH:MM:SS` interpreted as UTC. `None` when neither
/// parses.
fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // Receiver-sourced: naive UTC, no offset. Seconds may or may not be present.
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// `systemTime` (UTC) → the contract `ts`, RFC3339 in the machine's local
/// offset. `None` when unparseable (the row can't be filed).
fn ts_local(system_time: &str) -> Option<String> {
    let utc = parse_utc(system_time)?;
    Some(utc.with_timezone(&Local).to_rfc3339())
}

/// Insert `k`→`v` into `extra` only when the string is non-empty (trimmed).
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// One Dexcom EGV record → a contract [`Observation`]. `None` when the record
/// has no `recordId` (can't dedup) or no usable `systemTime` (can't partition).
/// `unit` is always `"mg/dL"` — the v3 endpoint reports every EGV in mg/dL, and
/// the row asserts the mg/dL glucose LOINC — so a raw `unit` of `"unknown"`
/// (common on receiver rows) or `"mmol/L"` is normalized to mg/dL, with any
/// non-mg/dL raw unit preserved under `extra.rawUnit`.
fn observation_from(egv: &Value) -> Option<Observation> {
    let record_id = str_field(egv, "recordId");
    if record_id.is_empty() {
        return None;
    }
    let system_time = str_field(egv, "systemTime");
    let ts = ts_local(&system_time)?;

    // value: the integer mg/dL reading. Absent (null) on a record whose value is
    // out of the measuring range — those carry a `status` instead; keep the row
    // (the status is the signal) with no numeric value.
    let value = egv.get("value").and_then(Value::as_f64);
    // unit: the v3 endpoint guarantees "All glucose values are reported in units
    // of mg/dL", and `code`/`code_system` below assert the mg/dL glucose LOINC —
    // so the contract unit is always mg/dL. The raw `unit` enum is mg/dL | mmol/L
    // | unknown; receiver/older-transmitter rows commonly send "unknown" (or omit
    // it), which must NOT ride onto a LOINC-mg/dL observation verbatim or the row
    // is internally inconsistent. Normalize to mg/dL; preserve any non-mg/dL raw
    // unit in `extra.rawUnit` so its provenance survives (the raw layer already
    // keeps the verbatim object).
    let raw_unit = str_field(egv, "unit");
    let unit = "mg/dL".to_string();

    let mut extra = Map::new();
    if !raw_unit.is_empty() && raw_unit != "mg/dL" {
        extra.insert("rawUnit".into(), Value::String(raw_unit));
    }
    // CGM-specific signal with no contract column — full fidelity in extra.
    put_str(&mut extra, "trend", &str_field(egv, "trend"));
    if let Some(rate) = egv.get("trendRate").and_then(Value::as_f64) {
        extra.insert("trendRate".into(), Value::from(rate));
    }
    put_str(&mut extra, "status", &str_field(egv, "status"));
    put_str(&mut extra, "rateUnit", &str_field(egv, "rateUnit"));
    // displayTime (the device's local-as-shown clock) preserved alongside the
    // canonical systemTime-derived `ts`.
    put_str(&mut extra, "displayTime", &str_field(egv, "displayTime"));
    put_str(&mut extra, "displayDevice", &str_field(egv, "displayDevice"));
    put_str(&mut extra, "transmitterId", &str_field(egv, "transmitterId"));
    put_str(&mut extra, "transmitterGeneration", &str_field(egv, "transmitterGeneration"));

    Some(Observation {
        ts,
        source: "dexcom".into(),
        guid: record_id,
        test: "Glucose".into(),
        code: GLUCOSE_LOINC.into(),
        code_system: "loinc".into(),
        value,
        value_text: String::new(),
        unit,
        reference_range: String::new(),
        flag: String::new(),
        panel: String::new(),
        provider: String::new(),
        extra,
    })
}

/// The EGV records out of an `/egvs` (or `/dataRange`-shaped) response. v3 wraps
/// them under `records`; a bare array is tolerated defensively.
fn egv_records(v: &Value) -> Vec<Value> {
    match v.get("records") {
        Some(Value::Array(a)) => a.clone(),
        _ => match v {
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        },
    }
}

/// The account's EGV span end (`dataRange.egvs.end.systemTime`), as a naive-UTC
/// ISO string. `None` when the account has no EGV data yet.
fn egvs_end(data_range: &Value) -> Option<String> {
    let end = data_range.get("egvs")?.get("end")?;
    let s = str_field(end, "systemTime");
    (!s.is_empty()).then_some(s)
}

/// The account's EGV span start (`dataRange.egvs.start.systemTime`), as a
/// naive-UTC ISO string. `None` when the account has no EGV data yet.
fn egvs_start(data_range: &Value) -> Option<String> {
    let start = data_range.get("egvs")?.get("start")?;
    let s = str_field(start, "systemTime");
    (!s.is_empty()).then_some(s)
}

/// Format a UTC datetime as the API's query-param shape: ISO 8601, no offset,
/// second-precision (`YYYY-MM-DDTHH:MM:SS`).
fn fmt_query(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

// ---------------------------------------------------------------------------
// Write: raw + contract, deduped by guid against what's already on disk.

/// Append new contract observations + raw rows, deduped by guid. Returns the
/// number of new contract rows written. Raw lines partition by the same month as
/// their contract row.
fn write_layer(vault: &Vault, rows: Vec<(Observation, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Existing guids in the contract stream — re-runnable: a re-pull of an
    // overlapping window never duplicates (the readwise/letterboxd pattern).
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<Observation> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (obs, raw_val) in rows {
        if obs.guid.is_empty() || !seen.insert(obs.guid.clone()) {
            continue; // no id, or already stored
        }
        new_raws.push(RawLine { ts: obs.ts.clone(), value: raw_val });
        new_rows.push(obs);
    }

    contract.append(&new_rows, |r| &r.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token (refreshing if needed) and sync. Missing token ⇒ a quiet
/// skip on the periodic path (mirror readwise/todoist), a clear error on the
/// manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Dexcom is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = DexcomClient::new(API_BASE.to_string());
    pull_with(vault, &client, &token.access_token)
}

/// Refresh the access token if it's expired (Dexcom issues a refresh token with
/// `offline_access`). The refreshed token is persisted (0600). A token with no
/// refresh token that's expired bails with a reconnect message.
fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(DEXCOM.service)?
        .or_else(|| DEXCOM.default_credentials())
        .context("Dexcom token expired and no app credentials to refresh it — reconnect")?;
    match oauth::refresh_token(&DEXCOM, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(DEXCOM.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            // A dead refresh token forces a clean reconnect rather than looping.
            vault.delete_sync_token(DEXCOM.service)?;
            bail!("Dexcom token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

/// The pull body over an injected API + access token — the testable seam. Reads
/// the account's EGV span, walks forward from the watermark in ≤30-day windows,
/// draining each, and advances the watermark only after each window's write.
fn pull_with(vault: &Vault, api: &impl DexcomApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_dexcom_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    let range = api
        .data_range(token)
        .map_err(|e| fetch_err("dataRange", e))?;
    let Some(end_raw) = egvs_end(&range) else {
        // No EGV data on the account yet — nothing to do, leave the watermark.
        counts.insert("readings", 0);
        return Ok(PullOutcome {
            headline: "Dexcom synced — no glucose data available yet".to_string(),
            counts,
        });
    };
    let end = parse_utc(&end_raw).with_context(|| {
        format!("dataRange egvs.end systemTime is not a parseable timestamp: {end_raw:?}")
    })?;

    // Cold-start lower bound: the watermark, else the account's first EGV.
    let start_raw = state
        .egvs_through
        .clone()
        .or_else(|| egvs_start(&range))
        .context("dataRange has an egvs end but no start — cannot bound the backfill")?;
    let mut cursor = parse_utc(&start_raw)
        .with_context(|| format!("cursor lower bound is not a parseable timestamp: {start_raw:?}"))?;

    let mut total: u64 = 0;
    // Walk forward in ≤30-day windows. The endpoint is inclusive of startDate
    // and exclusive of endDate; guid dedupe makes the inclusive lower boundary
    // (a re-read of the watermark reading) idempotent.
    while cursor <= end {
        let window_end = (cursor + chrono::Duration::days(MAX_WINDOW_DAYS)).min(end);
        let resp = api
            .egvs(token, &fmt_query(cursor), &fmt_query(window_end))
            .map_err(|e| fetch_err("egvs", e))?;
        let records = egv_records(&resp);

        // Watermark candidate: the max systemTime across this window (a naive-UTC
        // string). Computed from the *raw* records so it advances even past rows
        // we couldn't map (a record with no recordId still moves the clock).
        let window_max = records
            .iter()
            .filter_map(|r| parse_utc(&str_field(r, "systemTime")))
            .max();

        let rows: Vec<(Observation, Value)> = records
            .iter()
            .filter_map(|r| observation_from(r).map(|o| (o, r.clone())))
            .collect();
        total += write_layer(vault, rows)?;

        // Advance the watermark only after this window's write committed — a
        // crash before here re-drains the window (guid dedupe makes that safe).
        if let Some(max) = window_max {
            let max_str = fmt_query(max);
            if state.egvs_through.as_deref().is_none_or(|cur| max_str.as_str() > cur) {
                state.egvs_through = Some(max_str);
            }
        }

        // Advance to the next window. Always step strictly past window_end so a
        // window with no new data can't loop; the exclusive-end semantics mean
        // window_end itself is re-covered by the next window's inclusive start,
        // and guid dedupe drops the overlap.
        if window_end >= end {
            break;
        }
        cursor = window_end;
    }

    counts.insert("readings", total);
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_dexcom_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("Dexcom synced — {total} new glucose readings"),
        counts,
    })
}

/// Map a [`FetchError`] at the top of an endpoint into an anyhow error with a
/// clear reconnect message for 401.
fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Dexcom rejected the token (401) on the {endpoint} endpoint — reconnect from the Integrations tab"
        ),
        FetchError::RateLimited => anyhow::anyhow!(
            "Dexcom rate limited the {endpoint} endpoint (429) — it'll retry on the next sync"
        ),
        other => anyhow::anyhow!("Dexcom {endpoint} fetch failed: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-dexcom-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- fixtures (the documented v3 OpenAPI EGV + dataRange shapes) --------

    /// One EGV record in the exact v3 shape (OpenAPI `EGVRecord`): a normal
    /// reading sourced from a phone (systemTime carries a UTC offset of Z).
    fn egv_normal() -> Value {
        json!({
            "recordId": "f6d9e2a1-0001",
            "systemTime": "2026-06-10T17:00:00Z",
            "displayTime": "2026-06-10T10:00:00",
            "transmitterId": "8GXXAA",
            "transmitterTicks": 1234567,
            "value": 112,
            "status": null,
            "trend": "flat",
            "trendRate": 0.3,
            "unit": "mg/dL",
            "rateUnit": "mg/dL/min",
            "displayDevice": "iOS",
            "transmitterGeneration": "g6"
        })
    }

    /// A receiver-sourced EGV: NO UTC offset on systemTime/displayTime (the
    /// documented receiver behavior), a falling trend, no `unit` field.
    fn egv_receiver_no_offset() -> Value {
        json!({
            "recordId": "f6d9e2a1-0002",
            "systemTime": "2026-06-10T17:05:00",
            "displayTime": "2026-06-10T17:05:00",
            "transmitterId": "8GXXAA",
            "transmitterTicks": 1234867,
            "value": 108,
            "status": null,
            "trend": "singleDown",
            "trendRate": -1.8,
            "displayDevice": "receiver",
            "transmitterGeneration": "g6"
        })
    }

    /// A real out-of-range EGV in the documented v3 shape: the `value` is PRESENT
    /// (the swagger example sends `value:39, status:"low"`; "low" = under 40,
    /// "high" = over 400) and `status` rides alongside it into `extra`. This row
    /// also carries `unit:"unknown"` — a legal enum member (`unknown|mg/dL|mmol/L`)
    /// the API sends on receiver/older-transmitter rows — to prove the contract
    /// normalizes it to mg/dL (the v3 "all values in mg/dL" invariant).
    fn egv_out_of_range() -> Value {
        json!({
            "recordId": "f6d9e2a1-0003",
            "systemTime": "2026-06-10T17:10:00Z",
            "displayTime": "2026-06-10T10:10:00",
            "value": 39,
            "status": "low",
            "trend": "singleDown",
            "trendRate": -2.0,
            "unit": "unknown",
            "displayDevice": "receiver",
            "transmitterGeneration": "g6"
        })
    }

    /// A DEFENSIVE-parsing fixture, NOT a documented Dexcom shape: a malformed
    /// record whose `value` is null (the swagger types `value` as `integer|null`,
    /// but real out-of-range rows send a numeric value + `status` — see
    /// [`egv_out_of_range`]). Proves the mapper keeps such a row (omitting the
    /// numeric value) instead of dropping or panicking on it.
    fn egv_null_value_defensive() -> Value {
        json!({
            "recordId": "f6d9e2a1-0009",
            "systemTime": "2026-06-10T17:20:00Z",
            "displayTime": "2026-06-10T10:20:00",
            "value": null,
            "status": "low",
            "trend": "notComputable",
            "trendRate": null,
            "unit": "mg/dL",
            "displayDevice": "iOS",
            "transmitterGeneration": "g6"
        })
    }

    fn egvs_response(records: Vec<Value>) -> Value {
        json!({
            "recordType": "egv",
            "recordVersion": "3.0",
            "userId": "ab12cd34",
            "records": records
        })
    }

    /// A `/dataRange` response whose EGV span brackets the fixtures above (one
    /// ≤30-day window covers them).
    fn data_range_response() -> Value {
        json!({
            "recordType": "dataRange",
            "recordVersion": "3.0",
            "userId": "ab12cd34",
            "calibrations": {"start": {"systemTime": "2026-06-01T00:00:00", "displayTime": "2026-06-01T00:00:00"}, "end": {"systemTime": "2026-06-10T17:10:00", "displayTime": "2026-06-10T10:10:00"}},
            "egvs": {
                "start": {"systemTime": "2026-06-10T17:00:00", "displayTime": "2026-06-10T10:00:00"},
                "end": {"systemTime": "2026-06-10T17:10:00", "displayTime": "2026-06-10T10:10:00"}
            },
            "events": {"start": {"systemTime": "2026-06-01T00:00:00", "displayTime": "2026-06-01T00:00:00"}, "end": {"systemTime": "2026-06-10T17:10:00", "displayTime": "2026-06-10T10:10:00"}}
        })
    }

    // --- pure mapping tests ------------------------------------------------

    #[test]
    fn maps_normal_egv_to_glucose_observation() {
        let o = observation_from(&egv_normal()).unwrap();
        assert_eq!(o.source, "dexcom");
        assert_eq!(o.guid, "f6d9e2a1-0001", "guid is the stable recordId");
        assert_eq!(o.test, "Glucose");
        assert_eq!(o.code, "2339-0", "LOINC for blood glucose");
        assert_eq!(o.code_system, "loinc");
        assert_eq!(o.value, Some(112.0));
        assert_eq!(o.unit, "mg/dL");
        // ts = systemTime (UTC) rendered in local — same instant as the source.
        assert_eq!(
            DateTime::parse_from_rfc3339(&o.ts).unwrap().timestamp(),
            DateTime::parse_from_rfc3339("2026-06-10T17:00:00Z").unwrap().timestamp(),
        );
        // CGM-specific signal rides in extra.
        assert_eq!(o.extra.get("trend"), Some(&json!("flat")));
        assert_eq!(o.extra.get("trendRate"), Some(&json!(0.3)));
        assert_eq!(o.extra.get("displayDevice"), Some(&json!("iOS")));
        assert_eq!(o.extra.get("transmitterGeneration"), Some(&json!("g6")));
        assert_eq!(o.extra.get("displayTime"), Some(&json!("2026-06-10T10:00:00")));
    }

    #[test]
    fn naive_utc_systemtime_is_interpreted_as_utc() {
        // A receiver record (no offset) must be read as UTC, not local — the
        // instant has to match the same wall-clock interpreted in UTC.
        let o = observation_from(&egv_receiver_no_offset()).unwrap();
        assert_eq!(o.guid, "f6d9e2a1-0002");
        assert_eq!(o.value, Some(108.0));
        assert_eq!(o.unit, "mg/dL", "absent unit falls back to mg/dL");
        assert_eq!(
            DateTime::parse_from_rfc3339(&o.ts).unwrap().timestamp(),
            // "2026-06-10T17:05:00" interpreted as UTC.
            Utc.with_ymd_and_hms(2026, 6, 10, 17, 5, 0).unwrap().timestamp(),
        );
        assert_eq!(o.extra.get("trend"), Some(&json!("singleDown")));
        assert_eq!(o.extra.get("trendRate"), Some(&json!(-1.8)));
    }

    #[test]
    fn out_of_range_egv_keeps_value_and_status() {
        // The REAL documented out-of-range shape: the numeric value is present
        // (swagger example: value:39, status:"low") and `status` rides into
        // extra alongside the kept value.
        let o = observation_from(&egv_out_of_range()).unwrap();
        assert_eq!(o.guid, "f6d9e2a1-0003");
        assert_eq!(o.value, Some(39.0), "out-of-range rows still carry a value");
        assert_eq!(o.extra.get("status"), Some(&json!("low")));
        assert_eq!(o.extra.get("trend"), Some(&json!("singleDown")));
        // The raw unit "unknown" is normalized to mg/dL on the contract row (the
        // v3 "all values in mg/dL" invariant + the mg/dL LOINC), and the raw
        // string is preserved under extra.rawUnit.
        assert_eq!(o.unit, "mg/dL", "unknown unit normalized to mg/dL");
        assert_eq!(o.extra.get("rawUnit"), Some(&json!("unknown")), "raw unit preserved");
        assert_eq!(o.code, "2339-0", "still the mg/dL glucose LOINC");
        let re = serde_json::to_value(&o).unwrap();
        // The integer value writes back as `39` on disk (serde collapses the
        // f64 39.0 to a bare integer in JSON text).
        assert_eq!(serde_json::to_string(&o).unwrap().contains("\"value\":39"), true, "value on disk");
        assert_eq!(re["unit"], json!("mg/dL"));
        assert!(re.get("flag").is_none() && re.get("panel").is_none());
    }

    #[test]
    fn null_value_record_is_kept_defensively_without_value() {
        // Defensive: a malformed record with value:null (not a real Dexcom
        // out-of-range shape) is kept (its status is still a signal) with no
        // numeric value, rather than dropped or panicking.
        let o = observation_from(&egv_null_value_defensive()).unwrap();
        assert_eq!(o.guid, "f6d9e2a1-0009");
        assert!(o.value.is_none(), "null value → no numeric reading");
        assert_eq!(o.extra.get("status"), Some(&json!("low")));
        // trendRate null → not stored.
        assert!(o.extra.get("trendRate").is_none(), "null trendRate omitted");
        // mg/dL unit passes through unchanged (no rawUnit stashed when it's
        // already mg/dL).
        assert_eq!(o.unit, "mg/dL");
        assert!(o.extra.get("rawUnit").is_none(), "mg/dL is not stashed as rawUnit");
        // Omit-empty: re-serialized form drops the null value and unused columns.
        let re = serde_json::to_value(&o).unwrap();
        assert!(re.get("value").is_none(), "null value omitted on disk");
        assert!(re.get("flag").is_none() && re.get("panel").is_none());
        assert_eq!(re["unit"], json!("mg/dL"));
    }

    #[test]
    fn record_without_id_or_timestamp_is_skipped() {
        // No recordId → can't dedup.
        let no_id = json!({"systemTime": "2026-06-10T17:00:00Z", "value": 100});
        assert!(observation_from(&no_id).is_none());
        // No usable systemTime → can't partition.
        let no_ts = json!({"recordId": "x", "value": 100});
        assert!(observation_from(&no_ts).is_none());
        let bad_ts = json!({"recordId": "x", "systemTime": "not-a-time", "value": 100});
        assert!(observation_from(&bad_ts).is_none());
    }

    #[test]
    fn parse_utc_handles_offset_z_and_naive() {
        assert_eq!(
            parse_utc("2026-06-10T17:00:00Z").unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 10, 17, 0, 0).unwrap()
        );
        assert_eq!(
            parse_utc("2026-06-10T17:00:00").unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 10, 17, 0, 0).unwrap()
        );
        assert_eq!(
            parse_utc("2026-06-10T17:00:00+02:00").unwrap(),
            Utc.with_ymd_and_hms(2026, 6, 10, 15, 0, 0).unwrap()
        );
        assert_eq!(parse_utc("2026-06-10T17:00").unwrap(), Utc.with_ymd_and_hms(2026, 6, 10, 17, 0, 0).unwrap());
        assert!(parse_utc("").is_none());
        assert!(parse_utc("garbage").is_none());
    }

    #[test]
    fn data_range_accessors_read_the_egvs_span() {
        let r = data_range_response();
        assert_eq!(egvs_start(&r).as_deref(), Some("2026-06-10T17:00:00"));
        assert_eq!(egvs_end(&r).as_deref(), Some("2026-06-10T17:10:00"));
        // A range with no egvs block yields None (a fresh account).
        let empty = json!({"recordType": "dataRange", "egvs": {}});
        assert!(egvs_end(&empty).is_none());
    }

    #[test]
    fn egv_records_reads_wrapper_and_bare_array() {
        assert_eq!(egv_records(&egvs_response(vec![egv_normal()])).len(), 1);
        assert_eq!(egv_records(&json!([egv_normal(), egv_normal()])).len(), 2);
        assert!(egv_records(&json!({"records": null})).is_empty());
        assert!(egv_records(&json!({})).is_empty());
    }

    // --- a scripted mock API -----------------------------------------------

    struct MockApi {
        range: Value,
        egvs_windows: RefCell<VecDeque<Result<Value, FetchError>>>,
        egvs_calls: RefCell<Vec<(String, String)>>,
    }

    impl MockApi {
        fn new(range: Value) -> Self {
            MockApi {
                range,
                egvs_windows: RefCell::new(VecDeque::new()),
                egvs_calls: RefCell::new(Vec::new()),
            }
        }
        fn window(self, records: Vec<Value>) -> Self {
            self.egvs_windows.borrow_mut().push_back(Ok(egvs_response(records)));
            self
        }
    }

    impl DexcomApi for MockApi {
        fn data_range(&self, _token: &str) -> Result<Value, FetchError> {
            Ok(self.range.clone())
        }
        fn egvs(&self, _token: &str, start: &str, end: &str) -> Result<Value, FetchError> {
            self.egvs_calls.borrow_mut().push((start.to_string(), end.to_string()));
            self.egvs_windows
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Ok(egvs_response(vec![])))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_dedupes_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(data_range_response())
            .window(vec![egv_normal(), egv_receiver_no_offset(), egv_out_of_range()]);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("readings"), Some(&3));

        // Contract observation stream, partitioned by systemTime month (June).
        let obs = std::fs::read_to_string(
            v.root().join("health/medical/dexcom/observations/2026-06.jsonl"),
        )
        .unwrap();
        assert_eq!(obs.lines().count(), 3);
        assert!(obs.contains("\"guid\":\"f6d9e2a1-0001\""));
        assert!(obs.contains("\"test\":\"Glucose\""));
        assert!(obs.contains("\"code\":\"2339-0\""), "LOINC on disk");
        assert!(obs.contains("\"value\":112"));
        // The out-of-range row is present, carries its value (39) and status, and
        // its raw "unknown" unit was normalized to mg/dL with rawUnit preserved.
        assert!(obs.contains("\"guid\":\"f6d9e2a1-0003\""));
        assert!(obs.contains("\"value\":39"));
        assert!(obs.contains("\"status\":\"low\""));
        assert!(obs.contains("\"rawUnit\":\"unknown\""), "raw unit preserved in extra");
        assert!(!obs.contains("\"unit\":\"unknown\""), "no unknown unit on a contract row");

        // Raw layer mirrors the partitioning, verbatim EGV objects (fields the
        // contract drops survive).
        let raw = std::fs::read_to_string(v.root().join("health/medical/dexcom/raw/2026-06.jsonl")).unwrap();
        assert!(raw.contains("\"transmitterTicks\":1234567"), "raw keeps fields the contract drops");
        assert!(raw.contains("\"recordId\":\"f6d9e2a1-0001\""));

        // Watermark advanced to the max systemTime (the out-of-range row at
        // 17:10), as a naive-UTC string. The cursor carries NO token.
        let state = v.read_dexcom_sync();
        assert_eq!(state.egvs_through.as_deref(), Some("2026-06-10T17:10:00"));
        assert!(state.updated.is_some());
        let cursor = std::fs::read_to_string(v.root().join(".trove/dexcom-sync.json")).unwrap();
        assert!(!cursor.contains("tok"), "token never in the cursor");

        // Re-run with the same input → guid dedupe, byte-identical file.
        let again = pull_with(
            &v,
            &MockApi::new(data_range_response()).window(vec![egv_normal(), egv_receiver_no_offset(), egv_out_of_range()]),
            "tok",
        )
        .unwrap();
        assert_eq!(again.counts.get("readings"), Some(&0), "all three already stored");
        let obs2 = std::fs::read_to_string(v.root().join("health/medical/dexcom/observations/2026-06.jsonl")).unwrap();
        assert_eq!(obs, obs2, "observation file byte-identical after re-run");
    }

    #[test]
    fn backfill_walks_multiple_30_day_windows() {
        // A 70-day EGV span forces three windows (30 + 30 + 10 days). Each
        // window returns one reading at its start; all three should land.
        let range = json!({
            "recordType": "dataRange",
            "egvs": {
                "start": {"systemTime": "2026-01-01T00:00:00"},
                "end": {"systemTime": "2026-03-12T00:00:00"}
            }
        });
        let r1 = json!({"recordId": "w1", "systemTime": "2026-01-01T00:00:00", "value": 100, "unit": "mg/dL"});
        let r2 = json!({"recordId": "w2", "systemTime": "2026-02-01T00:00:00", "value": 101, "unit": "mg/dL"});
        let r3 = json!({"recordId": "w3", "systemTime": "2026-03-05T00:00:00", "value": 102, "unit": "mg/dL"});
        let v = temp_vault("backfill");
        let api = MockApi::new(range)
            .window(vec![r1])
            .window(vec![r2])
            .window(vec![r3]);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("readings"), Some(&3), "all three windows drained");
        // Three distinct windows were requested, each ≤30 days, marching forward.
        let calls = api.egvs_calls.borrow();
        assert_eq!(calls.len(), 3, "three windows: {calls:?}");
        assert_eq!(calls[0].0, "2026-01-01T00:00:00", "first window starts at the span start");
        assert_eq!(calls[1].0, "2026-01-31T00:00:00", "second window starts where the first ended");
        assert_eq!(calls[2].0, "2026-03-02T00:00:00", "third window starts where the second ended");
        // Each window's ≤30-day cap held (start→end span never exceeds 30 days).
        for (s, e) in calls.iter() {
            let span = parse_utc(e).unwrap() - parse_utc(s).unwrap();
            assert!(span <= chrono::Duration::days(MAX_WINDOW_DAYS), "window {s}..{e} > 30 days");
        }
        // All three readings landed (timezone-independent: the local month a
        // midnight-UTC row partitions into depends on the test machine's tz, so
        // count rows across every partition rather than asserting month files).
        let obs_dir = v.stream(DIR, Partition::Month);
        let total: usize = obs_dir
            .partitions()
            .unwrap()
            .iter()
            .map(|k| obs_dir.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(total, 3, "all three windows' readings persisted");
        let state = v.read_dexcom_sync();
        assert_eq!(state.egvs_through.as_deref(), Some("2026-03-05T00:00:00"));
    }

    #[test]
    fn incremental_pull_starts_from_the_watermark() {
        // A second sync over a grown account should request a window starting at
        // the stored watermark, not the account's first EGV.
        let v = temp_vault("incremental");
        v.write_dexcom_sync(&SyncState {
            egvs_through: Some("2026-06-10T17:10:00".into()),
            updated: Some("2026-06-10T10:11:00-07:00".into()),
        })
        .unwrap();
        // Account now extends to 17:15 with one new reading at the tail.
        let range = json!({
            "recordType": "dataRange",
            "egvs": {
                "start": {"systemTime": "2026-06-10T17:00:00"},
                "end": {"systemTime": "2026-06-10T17:15:00"}
            }
        });
        let newer = json!({"recordId": "f6d9e2a1-0004", "systemTime": "2026-06-10T17:15:00Z", "value": 105, "unit": "mg/dL", "trend": "flat"});
        let api = MockApi::new(range).window(vec![newer]);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("readings"), Some(&1));
        let calls = api.egvs_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "2026-06-10T17:10:00", "window starts at the watermark, not the span start");
        let state = v.read_dexcom_sync();
        assert_eq!(state.egvs_through.as_deref(), Some("2026-06-10T17:15:00"));
    }

    #[test]
    fn empty_account_is_a_clean_noop() {
        let v = temp_vault("emptyacct");
        // dataRange with no egvs span (a connected account that's never worn a
        // sensor).
        let api = MockApi::new(json!({"recordType": "dataRange", "egvs": {}}));
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("readings"), Some(&0));
        // No window requests at all.
        assert!(api.egvs_calls.borrow().is_empty());
        // No watermark written prematurely.
        assert!(v.read_dexcom_sync().egvs_through.is_none());
    }

    #[test]
    fn window_fetch_failure_errors_without_advancing_watermark() {
        let v = temp_vault("windowfail");
        let api = MockApi::new(data_range_response());
        api.egvs_windows.borrow_mut().push_back(Err(FetchError::Other("boom".into())));
        let err = pull_with(&v, &api, "tok").unwrap_err().to_string();
        assert!(err.contains("egvs"), "error names the endpoint: {err}");
        // Nothing committed: the watermark stays unset so the next sync re-drains.
        assert!(v.read_dexcom_sync().egvs_through.is_none(), "failed window must not advance");
    }

    #[test]
    fn data_range_401_is_a_reconnect_error() {
        let v = temp_vault("range401");
        struct Failing;
        impl DexcomApi for Failing {
            fn data_range(&self, _t: &str) -> Result<Value, FetchError> {
                Err(FetchError::Unauthorized)
            }
            fn egvs(&self, _t: &str, _s: &str, _e: &str) -> Result<Value, FetchError> {
                unreachable!()
            }
        }
        let err = pull_with(&v, &Failing, "tok").unwrap_err().to_string();
        assert!(err.contains("reconnect"), "401 surfaces a reconnect message: {err}");
        assert!(err.contains("dataRange"), "names the endpoint: {err}");
    }

    // --- cursor + connection tests -----------------------------------------

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor file deserializes to all-None (a first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.egvs_through.is_none());
        assert!(empty.updated.is_none());
        // A future cursor carrying an unknown extra field still deserializes
        // (additive evolution — prove old/new lines load).
        let fwd: SyncState =
            serde_json::from_str(r#"{"egvs_through":"2026-06-10T17:10:00","future":"x"}"#).unwrap();
        assert_eq!(fwd.egvs_through.as_deref(), Some("2026-06-10T17:10:00"));
    }

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("pull-unconnected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error, no panic: {err}");
    }

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        let v = temp_vault("conn");
        // Store directly (connect needs the browser/network).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "dex_secret_abc".into(),
                refresh_token: Some("dex_refresh_xyz".into()),
                token_type: Some("Bearer".into()),
                scope: Some("offline_access".into()),
                expires_at: Some(1_900_000_000),
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Dexcom");
        assert_eq!(status.accounts[0].key, "dexcom");
        assert!(!status.accounts[0].needs_reconnect, "live token");

        // The token is NOT in any non-secret file (the cursor).
        v.write_dexcom_sync(&SyncState {
            egvs_through: Some("2026-06-10T17:10:00".into()),
            updated: Some("2026-06-15T00:00:00-07:00".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/dexcom-sync.json")).unwrap();
        assert!(!cursor.contains("dex_secret_abc"), "access token never in the cursor");
        assert!(!cursor.contains("dex_refresh_xyz"), "refresh token never in the cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("dex_secret_abc") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "secret token file must be 0600");
                }
            }
            assert!(found, "the token was stored under .trove/sync");
        }

        def_disconnect(&v, "dexcom").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn connection_uses_a_fresh_unique_redirect_port() {
        // 38573-38578 are taken (ticktick/oura/google/trakt/microsoft/simkl);
        // dexcom claims 38579.
        assert_eq!(DEXCOM.redirect_port, 38579);
        assert_eq!(DEXCOM.redirect_uri(), "http://localhost:38579/callback");
        assert_eq!(DEXCOM.scopes, "offline_access");
        assert!(CONNECTION.method("oauth").is_some());
        assert_eq!(CONNECTION.id, "dexcom");
        assert_eq!(DEF.connection, Some("dexcom"));
    }
}
