//! Nightscout — self-hosted CGM aggregator with open REST API.
//! Brief: docs/integrations/nightscout.md. **Follower** of the `health-medical`
//! domain contract bound by [`crate::dexcom`]; reuses [`crate::health_medical::Observation`].
//!
//! A **Periodic** cloud pull against the user's own Nightscout instance (REST
//! API v3). Nightscout aggregates CGM readings from Dexcom Share, FreeStyle
//! Libre, Medtronic, Eversense, and others into a single endpoint that anyone
//! who runs a self-hosted instance can query.
//!
//! Three data streams:
//! - **Entries** (`/api/v3/entries`, type `sgv`): sensor glucose values — each
//!   becomes a [`crate::health_medical::Observation`] under
//!   `health/medical/nightscout/observations/YYYY-MM.jsonl` (guid = `identifier`
//!   or `_id`; ts = `date` epoch→local; test = `"Glucose"`; code = LOINC
//!   `2339-0`; value = `sgv` in mg/dL; trend in `extra`).
//! - **Treatments** (`/api/v3/treatments`): insulin doses, carbs, notes — raw
//!   only, `health/medical/nightscout/raw/treatments/YYYY-MM.jsonl`; no bound
//!   contract for this shape.
//! - **Devicestatus** (`/api/v3/devicestatus`): pump/uploader snapshots — raw
//!   only, `health/medical/nightscout/raw/devicestatus/YYYY-MM.jsonl`.
//!
//! Two layers per stream where a contract exists: unconditional **raw** (full
//! fidelity, verbatim object) + normalized **contract** observation rows.
//!
//! ## Cursor
//!
//! Watermark per collection (`entries_through`, `treatments_through`,
//! `devicestatus_through`) — each an epoch-ms integer — stored in
//! `.trove/nightscout-sync.json` (non-secret, rebuildable). Each collection
//! pages forward with `date$gte=<cursor>&sort$asc=date&limit=1000`
//! (ascending, oldest-first) advancing the cursor to `page_max+1` after each
//! full page, until a short page signals completion, then the watermark
//! advances. A crash before the write re-drains; guid dedupe makes re-drain safe.
//!
//! ## Auth
//!
//! The user pastes `<url>:<token>` (their instance base URL + their API secret
//! or JWT token) as a single composite credential stored 0600. The token is
//! passed as the `?token=<token>` query parameter (v3 API; the `?token` path is
//! preferred over the `api-secret` header for the v3 API).

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
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
// Paths.

/// Contract observations: `health/medical/nightscout/observations/YYYY-MM.jsonl`
const OBS_DIR: &str = "health/medical/nightscout/observations";
/// Raw entries (verbatim SGV objects).
const RAW_ENTRIES_DIR: &str = "health/medical/nightscout/raw/entries";
/// Raw treatments (insulin, carbs, notes).
const RAW_TREATMENTS_DIR: &str = "health/medical/nightscout/raw/treatments";
/// Raw devicestatus (pump/uploader snapshots).
const RAW_DEVICESTATUS_DIR: &str = "health/medical/nightscout/raw/devicestatus";
/// Non-secret watermark cursor.
const SYNC_FILE: &str = ".trove/nightscout-sync.json";
/// Secret-store service id.
const SERVICE: &str = "nightscout";
/// LOINC for Glucose [Mass/volume] in Blood — same as Dexcom.
const GLUCOSE_LOINC: &str = "2339-0";
/// Default page size for the v3 collection endpoints.
const PAGE_SIZE: u64 = 1000;
/// HTTP timeout per request.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Seconds between syncs. Hourly: CGM readings trickle in at ~5-minute
/// resolution and incremental page polls are cheap when caught up.
const SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// Max `date` (epoch ms) ingested for `/entries` — the `date$gte` lower
    /// bound for the next pull.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries_through: Option<u64>,
    /// Max `date` (epoch ms) ingested for `/treatments`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    treatments_through: Option<u64>,
    /// Max `date` (epoch ms) ingested for `/devicestatus`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    devicestatus_through: Option<u64>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_nightscout_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_nightscout_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Credential parsing: `<url>:<token>`.
//
// The user pastes their Nightscout base URL followed immediately by a `:`
// and their API secret / JWT token:
//   - `https://mysite.fly.dev:mysecret`       (common: no port)
//   - `http://localhost:1337:mysecret`         (local with port)
//
// Strategy: scan the string for all colons AFTER stripping the scheme
// (`https://` / `http://`). A colon whose right-hand side is ALL digits
// (optionally followed by a `/`) is a URL port separator — skip it. The
// first colon whose right-hand side contains non-digit characters is the
// token separator.

/// Parse `<nightscout-url>:<api-token>` from the pasted composite string.
/// Returns `(base_url, token)`. The base URL must start with `http://` or
/// `https://`.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your Nightscout URL and API token as url:token");
    }

    // Must start with http:// or https://
    let (scheme, without_scheme) = if let Some(rest) = pasted.strip_prefix("https://") {
        ("https://", rest)
    } else if let Some(rest) = pasted.strip_prefix("http://") {
        ("http://", rest)
    } else {
        bail!(
            "Nightscout URL must start with https:// or http:// — paste as \
             https://your-site.com:your-api-secret"
        );
    };

    // Walk colon positions in the post-scheme string. A colon is a port
    // separator when the segment immediately after it (up to the next `:` or `/`)
    // is purely ASCII digits AND 1-5 digits (valid port range) AND there is a
    // subsequent colon (meaning a token follows). The last colon whose segment
    // contains non-digit characters — or is a numeric segment with no further
    // colon — is the token separator.
    let mut token_colon: Option<usize> = None;
    let bytes = without_scheme.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b':' {
            continue;
        }
        // The segment immediately after this colon, up to the next `:` or `/`.
        let after = &without_scheme[i + 1..];
        let port_seg_len = after.bytes().take_while(|&c| c != b':' && c != b'/').count();
        let port_seg = &after[..port_seg_len];
        // Treat as a port separator only when:
        //   (a) all digits, 1-5 chars (valid port number), AND
        //   (b) a subsequent colon exists (so a token still follows).
        let looks_like_port = !port_seg.is_empty()
            && port_seg.len() <= 5
            && port_seg.bytes().all(|c| c.is_ascii_digit())
            && after[port_seg_len..].bytes().any(|c| c == b':');
        if looks_like_port {
            // This is a port colon; skip it and keep scanning.
            continue;
        }
        // Non-port colon → token separator.
        token_colon = Some(i);
        break;
    }

    let Some(pos) = token_colon else {
        bail!(
            "missing API token — paste as https://your-site.com:your-api-secret \
             (the token immediately follows the site URL, separated by a colon)"
        );
    };

    let url_part = &without_scheme[..pos];
    let token = without_scheme[pos + 1..].trim();
    if token.is_empty() {
        bail!("empty API token — paste as https://your-site.com:your-api-secret");
    }

    // Strip a trailing slash from the base URL.
    let base = format!("{scheme}{url_part}").trim_end_matches('/').to_string();

    Ok((base, token.to_string()))
}

// ---------------------------------------------------------------------------
// HTTP abstraction — injectable trait so tests run fully offline.

trait NightscoutApi {
    /// `GET /api/v3/<collection>?date$gte=<from_ms>&sort$asc=date&limit=<n>`
    /// Returns the parsed JSON body. Pagination loop calls this with increasing
    /// `from_ms` offsets (ascending order; cursor advances past the page max).
    fn list(
        &self,
        collection: &str,
        from_ms: Option<u64>,
        limit: u64,
    ) -> Result<Value, FetchError>;
}

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (401/403)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// Thin ureq-backed client. `base` has no trailing slash; `token` is passed as
/// `?token=<token>`.
struct NightscoutClient {
    base: String,
    token: String,
}

impl NightscoutClient {
    fn new(base: String, token: String) -> Self {
        NightscoutClient { base, token }
    }
}

impl NightscoutApi for NightscoutClient {
    fn list(
        &self,
        collection: &str,
        from_ms: Option<u64>,
        limit: u64,
    ) -> Result<Value, FetchError> {
        // Ascending sort: oldest-first so that advancing date$gte past the
        // page max fetches the next (older→newer) page correctly.
        let mut url = format!(
            "{}/api/v3/{}?sort$asc=date&limit={}&token={}",
            self.base, collection, limit, self.token
        );
        if let Some(ms) = from_ms {
            url.push_str(&format!("&date$gte={ms}"));
        }
        match ureq::get(&url).timeout(HTTP_TIMEOUT).call() {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parse error: {e}"))),
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
// Field helpers.

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Extract the document `date` field as epoch milliseconds.
/// Nightscout v3 accepts ms, seconds, or ISO 8601; but documents we read back
/// will have their `date` stored as ms (the native format). We accept ms or
/// seconds, returning ms.
fn date_ms(v: &Value) -> Option<u64> {
    let n = v.get("date")?.as_f64()?;
    // Epoch-seconds are < ~2_000_000_000 (year 2033); ms would be > 1e12.
    // Everything stored in NS is already in ms; be defensive.
    let ms = if n > 1e11 { n as u64 } else { (n * 1000.0) as u64 };
    if ms == 0 { None } else { Some(ms) }
}

/// Convert epoch ms to a local RFC3339 string for vault partitioning.
fn ts_local(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let nsecs = ((ms % 1000) * 1_000_000) as u32;
    let utc = Utc.timestamp_opt(secs, nsecs).single().unwrap_or_else(Utc::now);
    utc.with_timezone(&Local).to_rfc3339()
}

/// Stable guid: `identifier` (v3 UUID) preferred; fall back to `_id`
/// (MongoDB ObjectId); fall back to `date` as string when both absent.
fn doc_guid(v: &Value) -> String {
    let id = str_field(v, "identifier");
    if !id.is_empty() {
        return id;
    }
    let mongo = str_field(v, "_id");
    if !mongo.is_empty() {
        return mongo;
    }
    // Last resort: epoch ms as string — unique if the source is well-formed.
    v.get("date")
        .and_then(Value::as_f64)
        .map(|f| format!("date-{f}"))
        .unwrap_or_default()
}

/// Extract records from a v3 response: wraps under `result` (array), or falls
/// back to a bare array.
fn v3_records(body: &Value) -> Vec<Value> {
    match body.get("result") {
        Some(Value::Array(a)) => a.clone(),
        _ => match body {
            Value::Array(a) => a.clone(),
            _ => Vec::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// Raw line wrapper: partitioned by the contract ts, verbatim content.

#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// SGV entry → Observation.

/// One Nightscout SGV entry → a contract [`Observation`]. `None` when the
/// entry has no usable `date` (can't partition) or no meaningful id (can't
/// dedup). Non-SGV types (mbg, cal) map as generic readings with `test` set
/// to `type` field value so they're not silently dropped.
fn observation_from_entry(entry: &Value) -> Option<(Observation, String, u64)> {
    let ms = date_ms(entry)?;
    let guid = doc_guid(entry);
    if guid.is_empty() {
        return None;
    }
    let ts_str = ts_local(ms);

    let entry_type = str_field(entry, "type");
    let is_sgv = entry_type.eq_ignore_ascii_case("sgv");

    let (test, code, code_system, value, unit) = if is_sgv {
        let sgv = entry.get("sgv").and_then(Value::as_f64);
        // Nightscout stores SGV in mg/dL; the `units` field on entries is rare
        // and refers to the preferred display unit, not the stored unit. The
        // raw sensor value is always mg/dL in storage.
        (
            "Glucose".to_string(),
            GLUCOSE_LOINC.to_string(),
            "loinc".to_string(),
            sgv,
            "mg/dL".to_string(),
        )
    } else {
        // mbg, cal, or unknown: keep as a raw reading with the type as the
        // test name, no LOINC code, no numeric value normalization.
        let mbg_val = entry.get("mbg").and_then(Value::as_f64);
        let raw_val = entry.get("cal").or_else(|| entry.get("glucose")).and_then(Value::as_f64);
        let v = mbg_val.or(raw_val);
        let u = if v.is_some() { "mg/dL".to_string() } else { String::new() };
        (
            if entry_type.is_empty() { "entry".to_string() } else { entry_type.clone() },
            String::new(),
            String::new(),
            v,
            u,
        )
    };

    let mut extra = Map::new();
    // Trend / direction from the CGM — CGM-specific, lives in extra.
    let direction = str_field(entry, "direction");
    if !direction.is_empty() {
        extra.insert("direction".into(), Value::String(direction));
    }
    if let Some(noise) = entry.get("noise").and_then(Value::as_f64) {
        extra.insert("noise".into(), Value::from(noise));
    }
    let device = str_field(entry, "device");
    if !device.is_empty() {
        extra.insert("device".into(), Value::String(device));
    }
    let app = str_field(entry, "app");
    if !app.is_empty() {
        extra.insert("app".into(), Value::String(app));
    }
    if !entry_type.is_empty() {
        extra.insert("entryType".into(), Value::String(entry_type));
    }

    let obs = Observation {
        ts: ts_str.clone(),
        source: "nightscout".into(),
        guid: guid.clone(),
        test,
        code,
        code_system,
        value,
        value_text: String::new(),
        unit,
        reference_range: String::new(),
        flag: String::new(),
        panel: String::new(),
        provider: String::new(),
        extra,
    };
    Some((obs, ts_str, ms))
}

// ---------------------------------------------------------------------------
// Generic page drain: collect all documents for a collection since a cursor.
// Returns `(records, max_ms)`.

fn drain_collection(
    api: &impl NightscoutApi,
    collection: &str,
    from_ms: Option<u64>,
) -> Result<(Vec<Value>, u64)> {
    let mut all: Vec<Value> = Vec::new();
    let mut max_ms: u64 = from_ms.unwrap_or(0);
    let mut cursor = from_ms;

    loop {
        let body = api
            .list(collection, cursor, PAGE_SIZE)
            .map_err(|e| fetch_err(collection, e))?;
        let records = v3_records(&body);
        let page_len = records.len();

        // Advance max_ms and cursor to the maximum date in this page.
        let page_max = records.iter().filter_map(|r| date_ms(r)).max();
        if let Some(pm) = page_max {
            if pm > max_ms {
                max_ms = pm;
            }
        }

        all.extend(records);

        // A short page signals the last page (< PAGE_SIZE docs).
        if page_len < PAGE_SIZE as usize {
            break;
        }

        // Advance the cursor to just past the last seen timestamp so the next
        // page doesn't re-include the boundary. +1 ms to make it strictly past.
        cursor = Some(max_ms + 1);
    }

    Ok((all, max_ms))
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Append new observations + raw entry lines, deduped by guid.
fn write_entries(vault: &Vault, rows: Vec<(Observation, Value)>) -> Result<u64> {
    let obs_stream = vault.stream(OBS_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_ENTRIES_DIR, Partition::Month);

    // Load existing guids from the observation stream.
    let mut seen: HashSet<String> = HashSet::new();
    for key in obs_stream.partitions()? {
        for v in obs_stream.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_obs: Vec<Observation> = Vec::new();
    let mut new_raw: Vec<RawLine> = Vec::new();
    for (obs, raw_val) in rows {
        if obs.guid.is_empty() || !seen.insert(obs.guid.clone()) {
            continue;
        }
        new_raw.push(RawLine { ts: obs.ts.clone(), value: raw_val });
        new_obs.push(obs);
    }

    obs_stream.append(&new_obs, |r| &r.ts)?;
    raw_stream.append(&new_raw, |r| &r.ts)?;
    Ok(new_obs.len() as u64)
}

/// Append raw treatment documents, deduped by guid.
fn write_raw_collection(vault: &Vault, dir: &str, rows: Vec<(String, Value)>) -> Result<u64> {
    let stream = vault.stream(dir, Partition::Month);

    // Load existing guids.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            let g = doc_guid(&v);
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<RawLine> = Vec::new();
    for (guid, val) in rows {
        if guid.is_empty() || !seen.insert(guid) {
            continue;
        }
        let ts = date_ms(&val).map(ts_local).unwrap_or_else(|| Local::now().to_rfc3339());
        new_rows.push(RawLine { ts, value: val });
    }

    stream.append(&new_rows, |r| &r.ts)?;
    Ok(new_rows.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials from the stored token (access_token = composite
/// `url:token` as stored by `def_connect`). Returns `(base_url, token)`.
fn resolve_credentials(vault: &Vault) -> Result<(String, String)> {
    let tok = vault
        .load_sync_token(SERVICE)?
        .context("Nightscout is not connected — paste your instance URL and API token in the Integrations tab")?;
    parse_credentials(&tok.access_token)
}

pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let (base, token) = resolve_credentials(vault)?;
    let client = NightscoutClient::new(base, token);
    pull_with(vault, &client)
}

fn pull_with(vault: &Vault, api: &impl NightscoutApi) -> Result<PullOutcome> {
    let mut state = vault.read_nightscout_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // --- Entries (SGV glucose readings → Observation contract) --------------
    let (entries, entries_max) =
        drain_collection(api, "entries", state.entries_through.map(|m| m + 1))?;

    let entry_rows: Vec<(Observation, Value)> = entries
        .iter()
        .filter_map(|e| observation_from_entry(e).map(|(obs, _, _)| (obs, e.clone())))
        .collect();

    let new_entries = write_entries(vault, entry_rows)?;
    counts.insert("glucose_readings", new_entries);

    // Advance entries watermark only after write.
    if entries_max > state.entries_through.unwrap_or(0) {
        state.entries_through = Some(entries_max);
    }

    // --- Treatments (raw only) ----------------------------------------------
    let (treatments, treatments_max) =
        drain_collection(api, "treatments", state.treatments_through.map(|m| m + 1))?;

    let treatment_rows: Vec<(String, Value)> =
        treatments.into_iter().map(|v| (doc_guid(&v), v)).collect();

    let new_treatments = write_raw_collection(vault, RAW_TREATMENTS_DIR, treatment_rows)?;
    counts.insert("treatments", new_treatments);

    if treatments_max > state.treatments_through.unwrap_or(0) {
        state.treatments_through = Some(treatments_max);
    }

    // --- Devicestatus (raw only) --------------------------------------------
    let (devicestatus, devicestatus_max) =
        drain_collection(api, "devicestatus", state.devicestatus_through.map(|m| m + 1))?;

    let devicestatus_rows: Vec<(String, Value)> =
        devicestatus.into_iter().map(|v| (doc_guid(&v), v)).collect();

    let new_devicestatus =
        write_raw_collection(vault, RAW_DEVICESTATUS_DIR, devicestatus_rows)?;
    counts.insert("devicestatus", new_devicestatus);

    if devicestatus_max > state.devicestatus_through.unwrap_or(0) {
        state.devicestatus_through = Some(devicestatus_max);
    }

    // Persist cursor after all writes.
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_nightscout_sync(&state)?;

    Ok(PullOutcome {
        headline: format!(
            "Nightscout synced — {new_entries} glucose readings, {new_treatments} treatments, \
             {new_devicestatus} devicestatus"
        ),
        counts,
    })
}

fn fetch_err(collection: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Nightscout rejected the token (401/403) on /{collection} — reconnect from the \
             Integrations tab and verify your API secret or JWT token"
        ),
        other => anyhow::anyhow!("Nightscout /{collection} fetch failed: {other}"),
    }
}

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(OBS_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                out.headline.clone()
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "nightscout sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "nightscout",
        name: "Nightscout",
        kind: IntegrationKind::CloudSync,
        // Continuous glucose is sensitive medical data — opt-in.
        default_on: false,
        description:
            "Pulls continuous glucose readings, treatments, and device status from your \
             self-hosted Nightscout instance — a single endpoint that aggregates Dexcom, \
             FreeStyle Libre, Medtronic, and Eversense CGM data into one feed.",
        domain: "health",
        vault_path: "health/medical/nightscout/",
        toggleable: true,
        setup: &[
            "Continuous glucose is sensitive medical data — enabling this opts you in to \
             collecting it.",
            "Paste your Nightscout URL and API token (see connection setup steps).",
            "First sync backfills your available glucose history; later syncs are incremental.",
        ],
        caveats:
            "Requires a self-hosted Nightscout instance; primarily used by people managing \
             Type 1 diabetes. The Nightscout server itself is not a dependency Trove introduces — \
             Trove reads from your existing instance and degrades gracefully when it's unreachable.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("nightscout"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (TokenPaste = composite `<url>:<token>`, a SECRET).

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (base, token) = parse_credentials(pasted)?;
    // Validate by hitting /api/v3/status (the public server status endpoint;
    // returns 200 with server info even without auth, but we test connectivity).
    let status_url = format!("{}/api/v3/status?token={}", base, token);
    match ureq::get(&status_url).timeout(HTTP_TIMEOUT).call() {
        Ok(_) => {}
        Err(ureq::Error::Status(401 | 403, _)) => bail!(
            "Nightscout rejected the token (401/403) — verify your API secret or JWT token \
             at your-site.com/api-docs/"
        ),
        Err(e) => bail!("Could not reach your Nightscout instance: {e}"),
    }
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: Some("NightscoutToken".into()),
            scope: None,
            expires_at: None,
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(tok) = vault.load_sync_token(SERVICE)? {
        // Display only the base URL (not the secret token) in the UI.
        let label = parse_credentials(&tok.access_token)
            .map(|(url, _)| url)
            .unwrap_or_else(|_| "Nightscout".to_string());
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

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "nightscout",
    display_name: "Nightscout",
    methods: &[ConnectMethod::TokenPaste {
        label: "Nightscout URL and API token",
        help: "Paste your instance URL and API secret as url:token — for example \
               https://mysite.fly.dev:mysecrettoken. The token is your Nightscout \
               API_SECRET (the plain string, not its hash) or a JWT access token. \
               It's stored locally and sent only to your Nightscout instance.",
        placeholder: "https://mysite.fly.dev:my-api-secret",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["nightscout"],
    setup: &[
        "Open your Nightscout instance and find your API_SECRET (set when you deployed it).",
        "Paste your full URL and API_SECRET here as https://your-site.com:your-api-secret.",
        "Alternatively, generate a JWT token via your Nightscout admin panel and paste that instead.",
    ],
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-nightscout-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures: exact Nightscout v3 API shapes from the swagger docs.

    /// A normal SGV entry in the v3 response shape.
    fn entry_sgv_normal() -> Value {
        json!({
            "identifier": "53409478-105f-11e9-ab14-d663bd873d93",
            "date": 1748606400000u64,   // 2025-05-30T12:00:00Z
            "utcOffset": 0,
            "srvCreated": 1748606401000u64,
            "type": "sgv",
            "sgv": 112,
            "direction": "Flat",
            "noise": 1,
            "device": "xDrip-G5-Native",
            "app": "xDrip4iOS"
        })
    }

    /// A second SGV entry with falling trend.
    fn entry_sgv_falling() -> Value {
        json!({
            "identifier": "53409478-105f-11e9-ab14-d663bd873d94",
            "date": 1748606700000u64,   // 2025-05-30T12:05:00Z
            "_id": "mongo-abc123",
            "type": "sgv",
            "sgv": 105,
            "direction": "SingleDown",
            "noise": 1,
            "device": "xDrip-G5-Native",
            "app": "xDrip4iOS"
        })
    }

    /// An entry with NO identifier — falls back to _id.
    fn entry_mongo_id_only() -> Value {
        json!({
            "_id": "5e89c1f2d3e4f5a6b7c8d9e0",
            "date": 1748606460000u64,
            "type": "sgv",
            "sgv": 98,
            "direction": "Flat"
        })
    }

    /// A treatment record (raw only).
    fn treatment_meal() -> Value {
        json!({
            "identifier": "treat-uuid-001",
            "date": 1748606800000u64,
            "eventType": "Meal Bolus",
            "carbs": 45,
            "insulin": 4.5,
            "created_at": "2025-05-30T12:06:40.000Z",
            "enteredBy": "loop",
            "notes": "Lunch"
        })
    }

    /// A devicestatus record (raw only).
    fn devicestatus_pump() -> Value {
        json!({
            "identifier": "devstat-uuid-001",
            "date": 1748606900000u64,
            "device": "openaps://nspump",
            "created_at": "2025-05-30T12:08:20.000Z",
            "pump": {
                "clock": "2025-05-30T12:08:10Z",
                "battery": {"status": "normal", "voltage": 1.52},
                "reservoir": 147.0,
                "status": {"status": "normal", "suspended": false}
            },
            "uploader": {"battery": 72}
        })
    }

    fn v3_response(records: Vec<Value>) -> Value {
        json!({ "status": 200, "result": records })
    }

    // -----------------------------------------------------------------------
    // Credential parsing.

    #[test]
    fn parse_credentials_splits_url_and_token() {
        let (url, tok) = parse_credentials("https://mysite.fly.dev:mysecret").unwrap();
        assert_eq!(url, "https://mysite.fly.dev");
        assert_eq!(tok, "mysecret");
    }

    #[test]
    fn parse_credentials_strips_trailing_slash() {
        // A URL with a path component and trailing slash — the slash is stripped.
        let (url, tok) = parse_credentials("https://mysite.fly.dev/:tok").unwrap();
        // url_part = "mysite.fly.dev/", base = "https://mysite.fly.dev/" trimmed = "https://mysite.fly.dev"
        assert_eq!(url, "https://mysite.fly.dev");
        assert_eq!(tok, "tok");
    }

    #[test]
    fn parse_credentials_http_scheme() {
        let (url, tok) = parse_credentials("http://localhost:1337:mytoken").unwrap();
        assert_eq!(url, "http://localhost:1337");
        assert_eq!(tok, "mytoken");
    }

    #[test]
    fn parse_credentials_missing_token_errors() {
        assert!(parse_credentials("https://mysite.fly.dev").is_err());
        assert!(parse_credentials("").is_err());
        assert!(parse_credentials("mysite.fly.dev:tok").is_err(), "no scheme → error");
    }

    // -----------------------------------------------------------------------
    // Observation mapping from SGV entries.

    #[test]
    fn maps_sgv_entry_to_glucose_observation() {
        let (obs, _, ms) = observation_from_entry(&entry_sgv_normal()).unwrap();
        assert_eq!(obs.source, "nightscout");
        assert_eq!(obs.guid, "53409478-105f-11e9-ab14-d663bd873d93");
        assert_eq!(obs.test, "Glucose");
        assert_eq!(obs.code, "2339-0");
        assert_eq!(obs.code_system, "loinc");
        assert_eq!(obs.value, Some(112.0));
        assert_eq!(obs.unit, "mg/dL");
        assert_eq!(obs.extra.get("direction"), Some(&json!("Flat")));
        assert_eq!(obs.extra.get("device"), Some(&json!("xDrip-G5-Native")));
        assert_eq!(obs.extra.get("app"), Some(&json!("xDrip4iOS")));
        // ts is the epoch-ms converted to local RFC3339.
        let ts_dt = DateTime::parse_from_rfc3339(&obs.ts).unwrap();
        assert_eq!(ts_dt.timestamp(), 1748606400);
        assert_eq!(ms, 1748606400000u64);
    }

    #[test]
    fn prefers_identifier_over_mongo_id() {
        // entry_sgv_falling has both identifier and _id; identifier wins.
        let (obs, _, _) = observation_from_entry(&entry_sgv_falling()).unwrap();
        assert_eq!(obs.guid, "53409478-105f-11e9-ab14-d663bd873d94");
    }

    #[test]
    fn falls_back_to_mongo_id() {
        let (obs, _, _) = observation_from_entry(&entry_mongo_id_only()).unwrap();
        assert_eq!(obs.guid, "5e89c1f2d3e4f5a6b7c8d9e0");
    }

    #[test]
    fn entry_without_date_returns_none() {
        let bad = json!({"identifier": "abc", "type": "sgv", "sgv": 100});
        assert!(observation_from_entry(&bad).is_none());
    }

    #[test]
    fn entry_without_any_id_falls_back_to_date_guid() {
        // An entry with no identifier and no _id uses the date as the guid
        // (date-<epoch_ms>) so it is not silently dropped. This is a last-resort
        // fallback for old NS entries that predate the v3 UUID identifier field.
        let entry = json!({"date": 1748606400000u64, "type": "sgv", "sgv": 100});
        let (obs, _, _) = observation_from_entry(&entry).unwrap();
        assert!(obs.guid.starts_with("date-"), "fallback guid starts with 'date-': {}", obs.guid);
        assert_eq!(obs.value, Some(100.0));
    }

    #[test]
    fn entry_without_date_or_id_returns_none() {
        // No date at all → can't partition → None.
        let bad_no_date = json!({"type": "sgv", "sgv": 100});
        assert!(observation_from_entry(&bad_no_date).is_none());
        // No date AND no identifier → guid is empty string → None.
        let bad_no_guid = json!({"type": "sgv"});
        assert!(observation_from_entry(&bad_no_guid).is_none());
    }

    #[test]
    fn v3_records_extracts_result_array() {
        let r = v3_response(vec![entry_sgv_normal()]);
        let records = v3_records(&r);
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn v3_records_tolerates_bare_array() {
        let arr = json!([entry_sgv_normal(), entry_sgv_falling()]);
        assert_eq!(v3_records(&arr).len(), 2);
    }

    // -----------------------------------------------------------------------
    // Mock API + full pull.

    struct MockApi {
        entries: Vec<Value>,
        treatments: Vec<Value>,
        devicestatus: Vec<Value>,
        calls: std::cell::RefCell<Vec<(String, Option<u64>)>>,
    }

    impl MockApi {
        fn new(
            entries: Vec<Value>,
            treatments: Vec<Value>,
            devicestatus: Vec<Value>,
        ) -> Self {
            MockApi {
                entries,
                treatments,
                devicestatus,
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl NightscoutApi for MockApi {
        fn list(
            &self,
            collection: &str,
            from_ms: Option<u64>,
            limit: u64,
        ) -> Result<Value, FetchError> {
            self.calls
                .borrow_mut()
                .push((collection.to_string(), from_ms));
            let source: &[Value] = match collection {
                "entries" => &self.entries,
                "treatments" => &self.treatments,
                "devicestatus" => &self.devicestatus,
                _ => &[],
            };
            // Honour date$gte filter (same as the real API).
            let mut filtered: Vec<Value> = source
                .iter()
                .filter(|e| from_ms.map(|f| date_ms(e).unwrap_or(0) >= f).unwrap_or(true))
                .cloned()
                .collect();
            // Honour sort$asc=date (ascending, oldest first — matching the real query).
            filtered.sort_by_key(|e| date_ms(e).unwrap_or(0));
            // Honour limit: truncate to at most `limit` records per page.
            filtered.truncate(limit as usize);
            Ok(v3_response(filtered))
        }
    }

    #[test]
    fn full_pull_writes_observations_and_raw_layers() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(
            vec![entry_sgv_normal(), entry_sgv_falling()],
            vec![treatment_meal()],
            vec![devicestatus_pump()],
        );

        let out = pull_with(&v, &api).unwrap();
        assert_eq!(out.counts.get("glucose_readings"), Some(&2));
        assert_eq!(out.counts.get("treatments"), Some(&1));
        assert_eq!(out.counts.get("devicestatus"), Some(&1));

        // Contract observation file exists and has 2 rows.
        let obs_dir = v.root().join("health/medical/nightscout/observations");
        let mut obs_lines = 0usize;
        for entry in std::fs::read_dir(&obs_dir).unwrap().flatten() {
            let body = std::fs::read_to_string(entry.path()).unwrap();
            obs_lines += body.lines().count();
            assert!(body.contains("\"guid\":\"53409478-105f-11e9-ab14-d663bd873d93\""));
            assert!(body.contains("\"test\":\"Glucose\""));
            assert!(body.contains("\"code\":\"2339-0\""));
            assert!(body.contains("\"unit\":\"mg/dL\""));
        }
        assert_eq!(obs_lines, 2);

        // Raw entries exist.
        let raw_entries_dir = v.root().join("health/medical/nightscout/raw/entries");
        let raw_obs_lines: usize = std::fs::read_dir(&raw_entries_dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap().lines().count())
            .sum();
        assert_eq!(raw_obs_lines, 2);

        // Raw treatments exist.
        let raw_tx_dir = v.root().join("health/medical/nightscout/raw/treatments");
        let tx_body = std::fs::read_dir(&raw_tx_dir)
            .unwrap()
            .flatten()
            .next()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .unwrap_or_default();
        assert!(tx_body.contains("\"identifier\":\"treat-uuid-001\""));
        assert!(tx_body.contains("\"carbs\":45"));

        // Raw devicestatus exists.
        let raw_ds_dir = v.root().join("health/medical/nightscout/raw/devicestatus");
        let ds_body = std::fs::read_dir(&raw_ds_dir)
            .unwrap()
            .flatten()
            .next()
            .map(|e| std::fs::read_to_string(e.path()).unwrap())
            .unwrap_or_default();
        assert!(ds_body.contains("\"reservoir\":147.0"));

        // Watermark advanced.
        let state = v.read_nightscout_sync();
        assert!(state.entries_through.is_some());
        assert!(state.treatments_through.is_some());
        assert!(state.devicestatus_through.is_some());
        assert!(state.updated.is_some());
        // Token never in cursor.
        let cursor_text =
            std::fs::read_to_string(v.root().join(".trove/nightscout-sync.json")).unwrap();
        assert!(!cursor_text.contains("mysecret"), "no token in cursor");
    }

    #[test]
    fn deduplication_on_re_pull() {
        let v = temp_vault("dedup");
        let api = MockApi::new(
            vec![entry_sgv_normal()],
            vec![],
            vec![],
        );
        let out1 = pull_with(&v, &api).unwrap();
        assert_eq!(out1.counts.get("glucose_readings"), Some(&1));

        // Second pull with the same entry — cursor has advanced past it, so
        // mock returns empty (simulates date$gte filter). Nothing new written.
        let api2 = MockApi::new(vec![], vec![], vec![]);
        let out2 = pull_with(&v, &api2).unwrap();
        assert_eq!(out2.counts.get("glucose_readings"), Some(&0));

        // Observation file still has exactly 1 row.
        let obs_dir = v.root().join("health/medical/nightscout/observations");
        let total_lines: usize = std::fs::read_dir(&obs_dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap().lines().count())
            .sum();
        assert_eq!(total_lines, 1);
    }

    #[test]
    fn pull_without_token_returns_clear_error() {
        let v = temp_vault("notoken");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn cursor_serde_back_compat() {
        // Empty JSON deserializes to all-None.
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.entries_through.is_none());
        assert!(empty.updated.is_none());

        // Future unknown fields tolerated (additive).
        let fwd: SyncState = serde_json::from_str(
            r#"{"entries_through":1748606400000,"unknown_future":"x"}"#,
        )
        .unwrap();
        assert_eq!(fwd.entries_through, Some(1748606400000u64));
    }

    #[test]
    fn connection_def_shape_is_correct() {
        assert_eq!(CONNECTION.id, "nightscout");
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(DEF.connection, Some("nightscout"));
        // Opt-in (medical data).
        assert!(!DEF.meta.default_on);
    }

    #[test]
    fn connection_stores_token_and_status_shows_url_not_secret() {
        let v = temp_vault("conn-status");
        // Store credentials directly (def_connect needs network).
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "https://mysite.fly.dev:supersecret".into(),
                refresh_token: None,
                token_type: Some("NightscoutToken".into()),
                scope: None,
                expires_at: None,
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "https://mysite.fly.dev");
        assert!(
            !status.accounts[0].label.contains("supersecret"),
            "token never shown in label"
        );
        assert!(!status.accounts[0].needs_reconnect);

        def_disconnect(&v, "nightscout").unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn date_ms_handles_ms_and_seconds() {
        // Large number → ms.
        assert_eq!(date_ms(&json!({"date": 1748606400000u64})), Some(1748606400000u64));
        // Epoch seconds → scaled.
        assert_eq!(date_ms(&json!({"date": 1748606400})), Some(1748606400000u64));
        // Missing.
        assert_eq!(date_ms(&json!({})), None);
        // Zero.
        assert_eq!(date_ms(&json!({"date": 0})), None);
    }

    // -----------------------------------------------------------------------
    // Pagination regression: >PAGE_SIZE records must all be collected.
    //
    // This test exercises the multi-page drain path that was broken when
    // sort$desc=date was paired with a lower-bound cursor advance.  With
    // ascending sort and date$gte lower-bound advancement, 2500 records spread
    // across 3 pages (1000 + 1000 + 500) must all reach the vault.

    #[test]
    fn multi_page_drain_collects_all_records_past_page_size() {
        // Build 2500 synthetic SGV entries with ms timestamps 1 ms apart,
        // starting at epoch 1_700_000_000_000 ms (2023-11-14).
        let base_ms: u64 = 1_700_000_000_000;
        let total: usize = 2500;
        let entries: Vec<Value> = (0..total)
            .map(|i| {
                let ms = base_ms + i as u64;
                json!({
                    "identifier": format!("test-guid-{i:06}"),
                    "date": ms,
                    "type": "sgv",
                    "sgv": 100
                })
            })
            .collect();

        let v = temp_vault("multipage");
        let api = MockApi::new(entries, vec![], vec![]);
        let out = pull_with(&v, &api).unwrap();

        // All 2500 entries must be written.
        assert_eq!(
            out.counts.get("glucose_readings"),
            Some(&(total as u64)),
            "expected all {total} entries but got {:?}",
            out.counts.get("glucose_readings")
        );

        // The observation files must collectively contain exactly 2500 lines.
        let obs_dir = v.root().join("health/medical/nightscout/observations");
        let total_lines: usize = std::fs::read_dir(&obs_dir)
            .unwrap()
            .flatten()
            .map(|e| std::fs::read_to_string(e.path()).unwrap().lines().count())
            .sum();
        assert_eq!(total_lines, total, "observation file line count mismatch");

        // Watermark must be the maximum ms seen.
        let state = v.read_nightscout_sync();
        assert_eq!(
            state.entries_through,
            Some(base_ms + total as u64 - 1),
            "watermark should be the max date"
        );

        // The mock must have been called at least 3 times for entries
        // (pages: 0..999, 1000..1999, 2000..2499) — confirms multi-page path.
        let calls = api.calls.borrow();
        let entry_calls: Vec<_> = calls.iter().filter(|(c, _)| c == "entries").collect();
        assert!(
            entry_calls.len() >= 3,
            "expected >=3 entry fetches for 2500 records but got {}",
            entry_calls.len()
        );
        // First call: no cursor (cold start).
        assert_eq!(entry_calls[0].1, None, "first call must have no cursor");
        // Second call: cursor advances past page 1 max.
        let second_cursor = entry_calls[1].1.expect("second call must have a cursor");
        assert!(second_cursor > base_ms, "cursor must advance past first page");
    }

    // -----------------------------------------------------------------------
    // Credential parser: numeric-only token must not be misclassified as port.

    #[test]
    fn parse_credentials_numeric_token_not_rejected() {
        // A numeric-only token must be treated as the token, not a port.
        // `https://mysite.fly.dev:123456` — 123456 is the API_SECRET.
        let (url, tok) = parse_credentials("https://mysite.fly.dev:123456").unwrap();
        assert_eq!(url, "https://mysite.fly.dev");
        assert_eq!(tok, "123456");

        // Port + numeric token: `http://localhost:1337:99999`
        let (url2, tok2) = parse_credentials("http://localhost:1337:99999").unwrap();
        assert_eq!(url2, "http://localhost:1337");
        assert_eq!(tok2, "99999");
    }
}
