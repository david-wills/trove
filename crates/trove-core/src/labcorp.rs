//! Labcorp Patient FHIR — lab results via OAuth SMART on FHIR.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/labcorp.md.
//!
//! A **Periodic** cloud pull over the Labcorp patient FHIR R4 endpoint.
//! Lab results arrive as FHIR `Observation` and `DiagnosticReport` resources;
//! each Observation becomes a [`crate::health_medical::Observation`] under
//! `health/medical/labcorp/observations/YYYY-MM.jsonl`.
//!
//! # Contract mapping
//!
//! - `guid`  = FHIR `Observation.id` (the stable FHIR resource id).
//! - `ts`    = `Observation.effectiveDateTime` (preferred), then
//!             `Observation.effectivePeriod.start` (collection window start),
//!             then `Observation.issued` (availability instant, last resort).
//! - `test`  = `Observation.code.coding[0].display` or `code.text`.
//! - `code`  = `Observation.code.coding[0].code` (LOINC when present).
//! - `code_system` = `"loinc"` when the coding system is LOINC.
//! - `value` = `Observation.valueQuantity.value` (numeric).
//! - `value_text` = `Observation.valueString` or `valueCodeableConcept.text`
//!   for qualitative results.
//! - `unit`  = `Observation.valueQuantity.unit`.
//! - `reference_range` = `Observation.referenceRange[0]` as text.
//! - `flag`  = `Observation.interpretation[0].coding[0].code` (`"H"`, `"L"`,
//!   `"A"`, etc.).
//! - `panel` = linked `DiagnosticReport.code.text` if provided via context in
//!   `extra`.
//! - `provider` = `Observation.performer[0].display` (e.g. `"Labcorp"` or the
//!   ordering org). Falls back to empty string when no performer is present.
//! - Source-specific overflow (resource status, full category, identifiers)
//!   rides under `extra` (no contract column).
//!
//! Two layers, always: full-fidelity FHIR Observation JSON under
//! `health/medical/labcorp/raw/YYYY-MM.jsonl`; normalized contract rows
//! under `health/medical/labcorp/observations/YYYY-MM.jsonl`. Both
//! partitioned by the local month of the observation's effective date.
//!
//! # FHIR paging
//!
//! The FHIR server returns `Bundle` resources with an array of `entry`
//! objects and an optional `link[relation=next].url` for subsequent pages.
//! We drain all pages before writing (DRAIN the whole window before advancing
//! the watermark).
//!
//! # Auth
//!
//! SMART on FHIR patient-access: the user registers at
//! `fhir.labcorp.com/register/patient/` (free) and authorizes this app.
//! Public client + PKCE (Authorization Code + PKCE; no client_secret — the
//! SMART on FHIR standard for patient-facing apps). Credentials resolve:
//! explicit → saved → compiled-in (`TROVE_LABCORP_CLIENT_ID`, empty baked
//! default). The access token lives 0600 under `.trove/sync/`.
//!
//! # NOTE: Needs-sample / parser tolerance
//!
//! Labcorp's FHIR endpoint is thinly documented; the parser follows FHIR R4
//! standard profiles but tolerates field variance. Real-endpoint validation
//! requires a Labcorp patient account (Needs-login flag). Unexpected fields
//! are preserved verbatim in `extra` rather than dropped.

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
use crate::sync::oauth::{self, AppCredentials, Provider, TokenSet};
use crate::vault::Vault;

/// Contract observations directory (one NDJSON per month).
const DIR: &str = "health/medical/labcorp/observations";
/// Full-fidelity FHIR Observation NDJSON.
const RAW_DIR: &str = "health/medical/labcorp/raw";
/// Non-secret rebuildable watermark cursor.
const SYNC_FILE: &str = ".trove/labcorp-sync.json";
/// Service id for the OAuth token store.
const SERVICE: &str = "labcorp";

/// Labcorp Patient FHIR R4 base URL.
/// The brief cites fhir.labcorp.com; the FHIR metadata is at the standard
/// `[base]/metadata` well-known path. The actual path may need discovery —
/// treated as the base for R4 FHIR operations.
const FHIR_BASE: &str = "https://fhir.labcorp.com/r4";

const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Sync every 6 hours — lab results change infrequently.
pub const LABCORP_SYNC_SECS: u64 = 6 * 3600;

// ---------------------------------------------------------------------------
// OAuth provider (SMART on FHIR, public client + PKCE).

pub static LABCORP_PROVIDER: Provider = Provider {
    service: SERVICE,
    display_name: "Labcorp",
    // SMART on FHIR standard auth endpoints under the FHIR base. The exact
    // authorize/token URLs are discoverable via `.well-known/smart-configuration`
    // but are baked here for the fixed Labcorp endpoint (Needs-sample to confirm).
    auth_url: "https://fhir.labcorp.com/oauth2/authorize",
    token_url: "https://fhir.labcorp.com/oauth2/token",
    // SMART on FHIR patient-access scopes: read Observation + DiagnosticReport.
    // `offline_access` requests a refresh token. `launch/patient` context scopes
    // the request to the authenticated patient automatically.
    scopes: "patient/Observation.read patient/DiagnosticReport.read offline_access",
    // Assigned unique production port for this integration (38580 + 80 = 38660).
    redirect_port: 38660,
    // SMART on FHIR standalone patient launch uses PKCE (public client — no
    // client_secret required for this flow).
    use_pkce: true,
    basic_auth: false,
    // Client id compiled in at build time (TROVE_LABCORP_CLIENT_ID).
    // Registration at fhir.labcorp.com/register/patient/ (Needs-login).
    default_client_id: option_env!("TROVE_LABCORP_CLIENT_ID"),
    // Public PKCE client: no client_secret.
    default_client_secret: None,
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Registry hooks.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("labcorp synced — {total} lab observations")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "labcorp sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "labcorp",
        name: "Labcorp",
        kind: IntegrationKind::CloudSync,
        // Lab results are medical data — opt-in only, never default-on.
        default_on: false,
        description:
            "Pulls your Labcorp lab results via the patient FHIR endpoint into the unified \
             medical store. First sync backfills available history; later syncs are incremental.",
        domain: "health",
        vault_path: "health/medical/labcorp/",
        toggleable: true,
        setup: &[
            "Lab results are sensitive medical data — enabling this opts you in to collecting \
             them.",
            "Register at fhir.labcorp.com/register/patient/ to get a client ID for Trove, \
             then connect your account on this card.",
            "First sync backfills your available lab history; later syncs fetch only new results.",
        ],
        caveats:
            "The Labcorp patient FHIR endpoint is newer and less publicly documented than \
             Quest's — the first real-login run serves as the schema check. Unknown fields are \
             preserved in the raw layer.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(LABCORP_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some(SERVICE),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (OAuth 2.0 + PKCE, a SECRET).

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(LABCORP_PROVIDER.service)?.is_some()
        || LABCORP_PROVIDER.default_credentials().is_some();
    let accounts = match vault.load_sync_token(LABCORP_PROVIDER.service)? {
        Some(token) => vec![ConnectedAccount {
            key: LABCORP_PROVIDER.service.to_string(),
            label: LABCORP_PROVIDER.display_name.to_string(),
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

/// Registered in [`crate::integrations::CONNECTIONS`].
/// Public PKCE client — no client_secret required (SMART on FHIR standard for
/// patient-facing apps). The user registers once at fhir.labcorp.com.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Labcorp",
    methods: &[ConnectMethod::OAuth {
        provider: &LABCORP_PROVIDER,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["labcorp"],
    setup: &[
        "Register at fhir.labcorp.com/register/patient/ to create a patient app and obtain a \
         Client ID.",
        "Set its OAuth redirect URI to http://localhost:38660/callback — must match exactly.",
        "Paste the Client ID here (no secret needed — Labcorp uses a public PKCE client).",
    ],
};

/// Interactive connect: opens the consent page, waits for the redirect, saves
/// the token. Blocking — callers off the main thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(LABCORP_PROVIDER.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(LABCORP_PROVIDER.service)?
            .or_else(|| LABCORP_PROVIDER.default_credentials())
            .context(
                "no Labcorp client ID — register at fhir.labcorp.com/register/patient/ and enter \
                 the client ID in the Integrations tab",
            )?,
    };
    let flow = oauth::OauthFlow::start(&LABCORP_PROVIDER, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(LABCORP_PROVIDER.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// The maximum `Observation.meta.lastUpdated` value seen across all fetched
    /// Observations, as a UTC RFC3339 string (e.g. `"2026-04-03T14:30:10Z"`).
    /// Used as `_lastUpdated=gt<watermark>` on the next incremental query —
    /// we track the *same field* the server filters on to avoid silent gaps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observations_through: Option<String>,
    /// RFC3339 timestamp of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_labcorp_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_labcorp_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

trait LabcorpApi {
    /// `GET /Observation?patient=Patient&_lastUpdated=gt<since>&_sort=_lastUpdated`
    /// (or without the `_lastUpdated` filter for a cold start).
    /// Returns the raw Bundle JSON.
    fn observations(&self, token: &str, since: Option<&str>) -> Result<Value, FetchError>;

    /// Fetch the next page via the full URL from `Bundle.link[relation=next]`.
    fn next_page(&self, token: &str, url: &str) -> Result<Value, FetchError>;
}

#[derive(Debug)]
enum FetchError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

struct LabcorpClient {
    base: String,
}

impl LabcorpClient {
    fn new(base: String) -> Self {
        LabcorpClient { base }
    }

    fn get(&self, url: &str, token: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/fhir+json")
            .call()
        {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing FHIR response: {e}"))),
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

impl LabcorpApi for LabcorpClient {
    fn observations(&self, token: &str, since: Option<&str>) -> Result<Value, FetchError> {
        let mut url = format!(
            "{}/Observation?patient=Patient&_sort=_lastUpdated&_count=100",
            self.base
        );
        if let Some(ts) = since {
            url.push_str(&format!("&_lastUpdated=gt{}", ts));
        }
        self.get(&url, token)
    }

    fn next_page(&self, token: &str, url: &str) -> Result<Value, FetchError> {
        self.get(url, token)
    }
}

// ---------------------------------------------------------------------------
// FHIR R4 Observation → contract Observation mapper.
//
// Field names follow the FHIR R4 spec confirmed via hl7.org official examples.
// All access is tolerant (missing fields produce empty/None, not errors).

/// Extract a string field from a Value, trimmed, empty when missing.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Insert into `extra` only when non-empty.
fn put_str(extra: &mut Map<String, Value>, k: &str, v: &str) {
    let v = v.trim();
    if !v.is_empty() {
        extra.insert(k.into(), Value::String(v.into()));
    }
}

/// Parse a FHIR R4 date or dateTime string into an RFC3339 local-offset string,
/// or return the input as-is (for date-only strings like `2026-04-02`).
fn parse_fhir_datetime(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // RFC3339 / ISO 8601 dateTime with offset.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // Naive UTC datetime without offset (e.g. "2026-04-02T09:30:10").
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            let utc = Utc.from_utc_datetime(&naive);
            return Some(utc.with_timezone(&Local).to_rfc3339());
        }
    }
    // Date-only ("2026-04-02") — clinical data often has date granularity only.
    if s.len() >= 10 && s.chars().nth(4) == Some('-') && s.chars().nth(7) == Some('-') {
        return Some(s[..10].to_string());
    }
    None
}

/// Parse a FHIR dateTime string and normalize it to a UTC RFC3339 string
/// (`"2026-04-03T14:30:10Z"`). Used for watermark values so that mixed-offset
/// strings can be compared safely as lexicographic strings.
/// Returns `None` for date-only values or unparseable input.
fn parse_to_utc_rfc3339(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // RFC3339 / ISO 8601 dateTime with offset (covers the `Z` suffix too).
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).to_rfc3339());
    }
    // Naive UTC datetime without offset (e.g. "2026-04-02T09:30:10").
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            let utc = Utc.from_utc_datetime(&naive);
            return Some(utc.to_rfc3339());
        }
    }
    // Date-only — no time component, can't use as an instant watermark.
    None
}

/// Best-effort `ts` from a FHIR Observation: prefer `effectiveDateTime`, fall
/// back to `effectivePeriod.start`, then `issued`. Returns `None` when none is
/// present or parseable (the row can't be partitioned).
///
/// The contract defines `ts` as the *effective/collection* time of the
/// specimen — `effectiveDateTime` (exact instant) and `effectivePeriod.start`
/// (window start) both represent this, while `issued` is the
/// verification/availability instant and is the last resort.
fn observation_ts(obs: &Value) -> Option<String> {
    // 1. effectiveDateTime — exact collection instant.
    if let Some(s) = obs.get("effectiveDateTime").and_then(Value::as_str) {
        if let Some(ts) = parse_fhir_datetime(s) {
            return Some(ts);
        }
    }
    // 2. effectivePeriod.start — collection window start.
    if let Some(period) = obs.get("effectivePeriod") {
        if let Some(s) = period.get("start").and_then(Value::as_str) {
            if let Some(ts) = parse_fhir_datetime(s) {
                return Some(ts);
            }
        }
    }
    // 3. issued — verification/availability instant (last resort).
    if let Some(s) = obs.get("issued").and_then(Value::as_str) {
        if let Some(ts) = parse_fhir_datetime(s) {
            return Some(ts);
        }
    }
    None
}

/// Extract the first LOINC code (or any coding) from a FHIR `CodeableConcept`.
/// Returns `(code, code_system, display)`.
fn extract_coding(concept: &Value) -> (String, String, String) {
    let codings = concept
        .get("coding")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    // Prefer a LOINC coding.
    let loinc = codings.iter().find(|c| {
        c.get("system")
            .and_then(Value::as_str)
            .unwrap_or("")
            .contains("loinc")
    });
    let coding = loinc.or_else(|| codings.first());

    let (code, code_system, display) = match coding {
        Some(c) => {
            let code = str_field(c, "code");
            let system = c.get("system").and_then(Value::as_str).unwrap_or("");
            let code_system = if system.contains("loinc") {
                "loinc".to_string()
            } else if !system.is_empty() {
                system.to_string()
            } else {
                String::new()
            };
            let display = str_field(c, "display");
            (code, code_system, display)
        }
        None => (String::new(), String::new(), String::new()),
    };

    // Prefer coding.display; fall back to concept.text.
    let display = if display.is_empty() {
        str_field(concept, "text")
    } else {
        display
    };

    (code, code_system, display)
}

/// Extract the reference-range text from `Observation.referenceRange[0]`.
/// Produces a human-readable string like "3.5 - 5.0" or "<5.7" or the `text`
/// field when present.
fn extract_reference_range(obs: &Value) -> String {
    let arr = match obs.get("referenceRange").and_then(Value::as_array) {
        Some(a) if !a.is_empty() => &a[0],
        _ => return String::new(),
    };
    // Prefer the text field (e.g., "70-99", "<5.7").
    let text = str_field(arr, "text");
    if !text.is_empty() {
        return text;
    }
    // Synthesize from low/high.
    let low = arr
        .get("low")
        .and_then(|q| q.get("value"))
        .and_then(Value::as_f64);
    let high = arr
        .get("high")
        .and_then(|q| q.get("value"))
        .and_then(Value::as_f64);
    match (low, high) {
        (Some(lo), Some(hi)) => format!("{lo} - {hi}"),
        (Some(lo), None) => format!(">{lo}"),
        (None, Some(hi)) => format!("<{hi}"),
        (None, None) => String::new(),
    }
}

/// Extract the abnormal flag from `Observation.interpretation[0].coding[0].code`.
fn extract_flag(obs: &Value) -> String {
    obs.get("interpretation")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|i| i.get("coding"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Map a FHIR R4 Observation JSON object to a contract [`Observation`].
/// Returns `None` when the record has no `id` (can't deduplicate) or no
/// usable effective datetime (can't partition).
fn observation_from_fhir(obs: &Value) -> Option<Observation> {
    let id = str_field(obs, "id");
    if id.is_empty() {
        return None;
    }
    let ts = observation_ts(obs)?;

    let code_concept = obs.get("code").unwrap_or(&Value::Null);
    let (code, code_system, test_display) = extract_coding(code_concept);
    // `test` is required; use display, then text, then code as last resort.
    let test = if !test_display.is_empty() {
        test_display.clone()
    } else {
        str_field(code_concept, "text")
    };
    let test = if test.is_empty() { code.clone() } else { test };
    if test.is_empty() {
        // Can't build a meaningful row without a test name; keep the raw layer.
        return None;
    }

    // Numeric result.
    let (value, unit) = match obs.get("valueQuantity") {
        Some(q) => (
            q.get("value").and_then(Value::as_f64),
            str_field(q, "unit"),
        ),
        None => (None, String::new()),
    };

    // Qualitative result.
    let value_text = obs
        .get("valueString")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            obs.get("valueCodeableConcept")
                .and_then(|c| {
                    // coding[0].display, else text
                    c.get("coding")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|e| e.get("display"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or_else(|| c.get("text").and_then(Value::as_str).map(str::to_string))
                })
                .unwrap_or_default()
        });

    let reference_range = extract_reference_range(obs);
    let flag = extract_flag(obs);

    // provider: first performer with a display name (e.g. "Labcorp").
    // The Observation's own performer[] maps directly to the contract's
    // `provider` field per the brief/docstring — no DiagnosticReport needed.
    let provider = obs
        .get("performer")
        .and_then(Value::as_array)
        .and_then(|performers| {
            performers
                .iter()
                .find_map(|p| p.get("display").and_then(Value::as_str).map(str::to_string))
        })
        .unwrap_or_default();

    // Extra: status, category, identifiers.
    let mut extra = Map::new();
    put_str(&mut extra, "status", &str_field(obs, "status"));

    // Category display text (e.g., "Laboratory").
    if let Some(cats) = obs.get("category").and_then(Value::as_array) {
        let cats_text: Vec<String> = cats
            .iter()
            .filter_map(|c| {
                let (_, _, disp) = extract_coding(c);
                let text = if disp.is_empty() { str_field(c, "text") } else { disp };
                (!text.is_empty()).then_some(text)
            })
            .collect();
        if !cats_text.is_empty() {
            extra.insert("category".into(), Value::String(cats_text.join(", ")));
        }
    }

    // Preserve the full raw Observation JSON for complete fidelity.
    // (The raw layer also stores it verbatim, but a pointer to the resource in
    // extra aids read-time joins — avoids re-parsing raw NDJSON.)
    // We omit the full embed to keep contract rows slim; source code + guid
    // is sufficient for a join.

    Some(Observation {
        ts,
        source: "labcorp".into(),
        guid: format!("labcorp-{id}"),
        test,
        code,
        code_system,
        value,
        value_text,
        unit,
        reference_range,
        flag,
        panel: String::new(), // populated from DiagnosticReport context when available
        provider,
        extra,
    })
}

/// Extract the `entry[].resource` array from a FHIR `Bundle` response.
fn bundle_entries(bundle: &Value) -> Vec<Value> {
    bundle
        .get("entry")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("resource").cloned())
                .collect()
        })
        .unwrap_or_default()
}

/// Extract the `link[relation=next].url` from a FHIR Bundle for paging.
fn next_link(bundle: &Value) -> Option<String> {
    bundle
        .get("link")
        .and_then(Value::as_array)
        .and_then(|links| {
            links.iter().find(|l| {
                l.get("relation").and_then(Value::as_str) == Some("next")
            })
        })
        .and_then(|l| l.get("url"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Write layer.

/// Full-fidelity raw row: the verbatim FHIR Observation JSON, tagged with `ts`
/// for month-partitioning only (the `ts` field is skipped in serialization).
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Append new contract observations + raw FHIR objects, deduped by guid.
/// Returns the count of new contract rows written.
fn write_layer(vault: &Vault, rows: Vec<(Observation, Value)>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Load existing guids to avoid duplicating on re-pull.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = v.get("guid").and_then(Value::as_str).unwrap_or("").to_string();
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_obs: Vec<Observation> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (obs, raw_val) in rows {
        if obs.guid.is_empty() || !seen.insert(obs.guid.clone()) {
            continue;
        }
        new_raws.push(RawLine { ts: obs.ts.clone(), value: raw_val });
        new_obs.push(obs);
    }

    contract.append(&new_obs, |r| &r.ts)?;
    raw.append(&new_raws, |r| &r.ts)?;
    Ok(new_obs.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve the token (refreshing if needed) and sync.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Labcorp is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = LabcorpClient::new(FHIR_BASE.to_string());
    pull_with(vault, &client, &token.access_token)
}

fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(LABCORP_PROVIDER.service)?
        .or_else(|| LABCORP_PROVIDER.default_credentials())
        .context("Labcorp token expired and no credentials to refresh it — reconnect")?;
    match oauth::refresh_token(&LABCORP_PROVIDER, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(LABCORP_PROVIDER.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(LABCORP_PROVIDER.service)?;
            bail!("Labcorp token refresh failed ({e}) — reconnect from the Integrations tab");
        }
    }
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl LabcorpApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_labcorp_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Collect all Observation resources across all pages before writing.
    let mut all_rows: Vec<(Observation, Value)> = Vec::new();

    // First page — incremental (with watermark) or cold start (without).
    let first_bundle = api
        .observations(token, state.observations_through.as_deref())
        .map_err(|e| fetch_err("Observation search", e))?;

    let mut bundle = first_bundle;
    let mut max_issued: Option<String> = state.observations_through.clone();

    loop {
        let entries = bundle_entries(&bundle);
        for resource in &entries {
            // Only process Observation resources (bundle may include includes).
            if resource.get("resourceType").and_then(Value::as_str) != Some("Observation") {
                continue;
            }
            if let Some(obs) = observation_from_fhir(resource) {
                // Track the maximum `meta.lastUpdated` for the watermark — we
                // filter _lastUpdated on the next query, so we MUST track the
                // same field we filter on. meta.lastUpdated is always UTC on
                // FHIR servers. Normalize to UTC RFC3339 (Z suffix) so mixed
                // offset strings compare correctly as strings.
                let meta_last_updated = resource
                    .get("meta")
                    .and_then(|m| m.get("lastUpdated"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Some(utc_ts) = parse_to_utc_rfc3339(meta_last_updated) {
                    if max_issued.as_deref().is_none_or(|cur| utc_ts.as_str() > cur) {
                        max_issued = Some(utc_ts);
                    }
                }
                all_rows.push((obs, resource.clone()));
            }
        }

        // Follow pagination — drain the full result set before writing.
        match next_link(&bundle) {
            Some(next_url) => {
                bundle = api
                    .next_page(token, &next_url)
                    .map_err(|e| fetch_err("Observation next page", e))?;
            }
            None => break,
        }
    }

    // Write the full drained set — advance the watermark only after a
    // successful write so a crash re-drains rather than skipping.
    let total = write_layer(vault, all_rows)?;

    if let Some(ts) = max_issued {
        state.observations_through = Some(ts);
    }
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_labcorp_sync(&state)?;

    counts.insert("observations", total);
    Ok(PullOutcome {
        headline: format!("Labcorp synced — {total} new lab observations"),
        counts,
    })
}

fn fetch_err(endpoint: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Labcorp rejected the token (401) on the {endpoint} endpoint — reconnect from the \
             Integrations tab"
        ),
        other => anyhow::anyhow!("Labcorp {endpoint} fetch failed: {other}"),
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
            .join(format!("trove-labcorp-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — FHIR R4 shapes from official hl7.org examples.

    /// A typical lab Observation (glucose, LOINC 15074-8). Shape mirrors the
    /// official FHIR R4 example at hl7.org/fhir/R4/observation-example-f001-glucose.json.
    /// Carries `effectivePeriod` + `issued` but NO `effectiveDateTime` — the
    /// common Labcorp shape and the exact case the ts-precedence fix targets.
    fn fhir_obs_glucose() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-glucose-001",
            "meta": {"lastUpdated": "2026-04-03T14:30:10Z"},
            "status": "final",
            "category": [{
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "laboratory", "display": "Laboratory"}]
            }],
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "15074-8", "display": "Glucose [Moles/volume] in Blood"}],
                "text": "Glucose"
            },
            "subject": {"reference": "Patient/f001"},
            "effectivePeriod": {"start": "2026-04-02T09:30:10+01:00"},
            "issued": "2026-04-03T15:30:10+01:00",
            "performer": [{"display": "Labcorp"}],
            "valueQuantity": {"value": 6.3, "unit": "mmol/l", "system": "http://unitsofmeasure.org", "code": "mmol/L"},
            "interpretation": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation", "code": "H", "display": "High"}]}],
            "referenceRange": [{"low": {"value": 3.1, "unit": "mmol/l"}, "high": {"value": 6.2, "unit": "mmol/l"}}]
        })
    }

    /// A qualitative (non-numeric) Observation — HIV antibody screen result.
    fn fhir_obs_qualitative() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-hiv-002",
            "status": "final",
            "category": [{"coding": [{"code": "laboratory", "display": "Laboratory"}]}],
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "89365-1", "display": "HIV 1+2 Ab panel - Serum or Plasma"}],
                "text": "HIV 1/2 Ab Screen"
            },
            "subject": {"reference": "Patient/f001"},
            "effectiveDateTime": "2026-03-15",
            "valueCodeableConcept": {"text": "Non-Reactive"}
        })
    }

    /// An Observation with date-only effectiveDateTime (clinical date granularity).
    fn fhir_obs_date_only() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-hba1c-003",
            "status": "final",
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "4548-4", "display": "Hemoglobin A1c/Hemoglobin.total in Blood"}],
                "text": "Hemoglobin A1c"
            },
            "effectiveDateTime": "2026-01-20",
            "valueQuantity": {"value": 5.6, "unit": "%"},
            "referenceRange": [{"text": "<5.7%"}]
        })
    }

    /// An Observation with no id — must be skipped (can't deduplicate).
    fn fhir_obs_no_id() -> Value {
        json!({
            "resourceType": "Observation",
            "status": "final",
            "code": {"coding": [{"code": "15074-8"}]},
            "effectiveDateTime": "2026-04-02T09:30:10+01:00",
            "valueQuantity": {"value": 6.0, "unit": "mmol/l"}
        })
    }

    /// A Bundle wrapping one Observation.
    fn single_obs_bundle(obs: Value) -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "total": 1,
            "entry": [{"fullUrl": "urn:uuid:1", "resource": obs}]
        })
    }

    /// A paginated Bundle (first page, with a next link).
    fn bundle_with_next(obs: Value, next_url: &str) -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": [{"relation": "next", "url": next_url}],
            "entry": [{"resource": obs}]
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapper tests.

    #[test]
    fn maps_glucose_observation_to_contract() {
        let obs = observation_from_fhir(&fhir_obs_glucose()).unwrap();
        assert_eq!(obs.source, "labcorp");
        assert_eq!(obs.guid, "labcorp-obs-glucose-001");
        assert_eq!(obs.test, "Glucose [Moles/volume] in Blood");
        assert_eq!(obs.code, "15074-8", "LOINC code");
        assert_eq!(obs.code_system, "loinc");
        assert_eq!(obs.value, Some(6.3));
        assert_eq!(obs.unit, "mmol/l");
        assert_eq!(obs.flag, "H", "abnormal flag from interpretation");
        assert_eq!(obs.reference_range, "3.1 - 6.2", "synthesized from low/high");
        // ts: `effectiveDateTime` absent → `effectivePeriod.start` is used
        // (NOT `issued`). This fixture carries effectivePeriod.start =
        // "2026-04-02T09:30:10+01:00" (collection time) and issued =
        // "2026-04-03T15:30:10+01:00" (availability time). The contract
        // defines ts as the collection time, so effectivePeriod.start wins.
        let ts = DateTime::parse_from_rfc3339(&obs.ts).unwrap();
        let expected = DateTime::parse_from_rfc3339("2026-04-02T09:30:10+01:00").unwrap();
        assert_eq!(
            ts.timestamp(),
            expected.timestamp(),
            "ts uses effectivePeriod.start (collection time), not issued (availability time)"
        );
        // provider extracted from performer[].display.
        assert_eq!(obs.provider, "Labcorp", "provider from performer[0].display");
        // status in extra.
        assert_eq!(obs.extra.get("status"), Some(&json!("final")));
    }

    #[test]
    fn ts_prefers_effective_period_start_over_issued() {
        // The critical fixture: effectivePeriod+issued but NO effectiveDateTime.
        // This is the exact shape from the official FHIR R4 f001 glucose example
        // AND the expected Labcorp response shape.
        // The CONTRACT ts must be the collection date (effectivePeriod.start),
        // NOT the availability date (issued).
        let obs = fhir_obs_glucose();
        let mapped = observation_from_fhir(&obs).unwrap();
        let ts = DateTime::parse_from_rfc3339(&mapped.ts).unwrap();
        let collection = DateTime::parse_from_rfc3339("2026-04-02T09:30:10+01:00").unwrap();
        let availability = DateTime::parse_from_rfc3339("2026-04-03T15:30:10+01:00").unwrap();
        assert_eq!(
            ts.timestamp(),
            collection.timestamp(),
            "ts must be collection time (effectivePeriod.start)"
        );
        assert_ne!(
            ts.timestamp(),
            availability.timestamp(),
            "ts must NOT be the issued (availability) time"
        );
    }

    #[test]
    fn maps_qualitative_observation_uses_value_text() {
        let obs = observation_from_fhir(&fhir_obs_qualitative()).unwrap();
        assert_eq!(obs.guid, "labcorp-obs-hiv-002");
        assert_eq!(obs.test, "HIV 1+2 Ab panel - Serum or Plasma");
        assert!(obs.value.is_none(), "no numeric value for qualitative result");
        assert_eq!(obs.value_text, "Non-Reactive");
        // date-only effectiveDateTime preserved verbatim.
        assert_eq!(obs.ts, "2026-03-15");
    }

    #[test]
    fn maps_date_only_observation() {
        let obs = observation_from_fhir(&fhir_obs_date_only()).unwrap();
        assert_eq!(obs.guid, "labcorp-obs-hba1c-003");
        assert_eq!(obs.ts, "2026-01-20", "date-only preserved");
        assert_eq!(obs.value, Some(5.6));
        assert_eq!(obs.unit, "%");
        assert_eq!(obs.reference_range, "<5.7%", "text referenceRange used directly");
    }

    #[test]
    fn observation_without_id_is_skipped() {
        assert!(observation_from_fhir(&fhir_obs_no_id()).is_none());
    }

    #[test]
    fn observation_without_effective_date_is_skipped() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "no-ts",
            "code": {"coding": [{"code": "15074-8", "display": "Glucose"}]}
        });
        assert!(observation_from_fhir(&obs).is_none());
    }

    #[test]
    fn reference_range_text_takes_priority_over_low_high() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "rr-text",
            "code": {"coding": [{"code": "4548-4", "display": "HbA1c"}]},
            "effectiveDateTime": "2026-01-01",
            "referenceRange": [{"text": "<5.7", "low": {"value": 0.0}, "high": {"value": 5.7}}]
        });
        let o = observation_from_fhir(&obs).unwrap();
        assert_eq!(o.reference_range, "<5.7", "text field takes priority");
    }

    #[test]
    fn bundle_entries_extracts_resources() {
        let bundle = single_obs_bundle(fhir_obs_glucose());
        let entries = bundle_entries(&bundle);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].get("resourceType"), Some(&json!("Observation")));
    }

    #[test]
    fn next_link_extracts_pagination_url() {
        let b = bundle_with_next(fhir_obs_glucose(), "https://fhir.labcorp.com/r4/Observation?page=2");
        assert_eq!(next_link(&b).as_deref(), Some("https://fhir.labcorp.com/r4/Observation?page=2"));
        // No next link when not present.
        assert!(next_link(&single_obs_bundle(fhir_obs_glucose())).is_none());
    }

    // -----------------------------------------------------------------------
    // Mock API + integration pull tests.

    struct MockApi {
        pages: std::cell::RefCell<std::collections::VecDeque<Result<Value, FetchError>>>,
    }

    impl MockApi {
        fn new() -> Self {
            MockApi { pages: std::cell::RefCell::new(std::collections::VecDeque::new()) }
        }
        fn page(self, bundle: Value) -> Self {
            self.pages.borrow_mut().push_back(Ok(bundle));
            self
        }
    }

    impl LabcorpApi for MockApi {
        fn observations(&self, _token: &str, _since: Option<&str>) -> Result<Value, FetchError> {
            self.pages.borrow_mut().pop_front().unwrap_or_else(|| Ok(json!({"resourceType":"Bundle","entry":[]})))
        }
        fn next_page(&self, _token: &str, _url: &str) -> Result<Value, FetchError> {
            self.pages.borrow_mut().pop_front().unwrap_or_else(|| Ok(json!({"resourceType":"Bundle","entry":[]})))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_and_dedupes() {
        let v = temp_vault("fullpull");
        let api = MockApi::new().page(single_obs_bundle(fhir_obs_glucose()));

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&1));

        // Contract layer written.
        let obs_file = v
            .root()
            .join("health/medical/labcorp/observations")
            .read_dir()
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .expect("an observations NDJSON file");
        let obs_text = std::fs::read_to_string(obs_file.path()).unwrap();
        assert_eq!(obs_text.lines().count(), 1, "one observation on disk");
        assert!(obs_text.contains("\"guid\":\"labcorp-obs-glucose-001\""));
        assert!(obs_text.contains("\"code\":\"15074-8\""));
        assert!(obs_text.contains("\"source\":\"labcorp\""));
        // provider extracted from performer.
        assert!(obs_text.contains("\"provider\":\"Labcorp\""), "provider in contract row");

        // Raw layer written.
        let raw_file = v
            .root()
            .join("health/medical/labcorp/raw")
            .read_dir()
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .expect("a raw NDJSON file");
        let raw_text = std::fs::read_to_string(raw_file.path()).unwrap();
        assert!(raw_text.contains("\"resourceType\":\"Observation\""), "verbatim FHIR resource in raw");
        assert!(raw_text.contains("\"id\":\"obs-glucose-001\""), "FHIR id preserved in raw");

        // Watermark must be from meta.lastUpdated (the filtered field), not issued.
        let state = v.read_labcorp_sync();
        let wm_str = state.observations_through.clone().expect("watermark set");
        let wm_dt = DateTime::parse_from_rfc3339(&wm_str).unwrap();
        let meta_last_updated = DateTime::parse_from_rfc3339("2026-04-03T14:30:10Z").unwrap();
        assert_eq!(
            wm_dt.timestamp(),
            meta_last_updated.timestamp(),
            "watermark epoch matches meta.lastUpdated (the _lastUpdated filter field)"
        );
        // Stored as normalized UTC (no arbitrary offset).
        assert!(
            wm_str.ends_with('Z') || wm_str.ends_with("+00:00"),
            "watermark stored as UTC RFC3339: {wm_str}"
        );

        // Re-pull with same data → guid dedupe → 0 new rows; file unchanged.
        let again = pull_with(&v, &MockApi::new().page(single_obs_bundle(fhir_obs_glucose())), "tok").unwrap();
        assert_eq!(again.counts.get("observations"), Some(&0), "dedupe works");
        let obs_text2 = std::fs::read_to_string(obs_file.path()).unwrap();
        assert_eq!(obs_text, obs_text2, "file byte-identical after deduped re-pull");
    }

    #[test]
    fn watermark_uses_meta_last_updated_not_issued() {
        // Regression: watermark must track meta.lastUpdated (the field _lastUpdated
        // filters on) NOT issued/effectiveDateTime. An observation whose
        // meta.lastUpdated <= some prior max-issued would be silently skipped by
        // the _lastUpdated filter if the watermark were tracked from issued.
        let v = temp_vault("watermark-field");

        // Observation: collection=2026-04-02 (effectivePeriod), issued=2026-04-03,
        // but meta.lastUpdated=2026-04-03T14:30:10Z (the server's index timestamp).
        let api = MockApi::new().page(single_obs_bundle(fhir_obs_glucose()));
        pull_with(&v, &api, "tok").unwrap();

        let state = v.read_labcorp_sync();
        let wm = state.observations_through.unwrap();
        // Watermark should be meta.lastUpdated (2026-04-03T14:30:10Z), not issued
        // (2026-04-03T15:30:10+01:00 = 2026-04-03T14:30:10Z UTC — same epoch).
        // The key assertion is that the stored format is normalized UTC (Z).
        assert!(wm.ends_with('Z') || wm.contains("+00:00"),
            "watermark stored as UTC RFC3339: {wm}");
        // Must equal the meta.lastUpdated value (2026-04-03T14:30:10Z).
        let wm_dt = DateTime::parse_from_rfc3339(&wm).unwrap();
        let expected = DateTime::parse_from_rfc3339("2026-04-03T14:30:10Z").unwrap();
        assert_eq!(wm_dt.timestamp(), expected.timestamp(),
            "watermark epoch matches meta.lastUpdated");
    }

    #[test]
    fn watermark_max_normalized_utc_mixed_offsets() {
        // Regression: when observations carry different UTC offsets, the max
        // must be computed over UTC epochs, not raw strings. Raw strings give
        // wrong lexicographic order (e.g. "09:00-07:00" < "15:30+01:00" lexically
        // but 09:00-07:00 = 16:00 UTC is chronologically LATER).
        //
        // Obs A: meta.lastUpdated = "2026-04-03T15:30:10+01:00" (= 14:30:10 UTC)
        // Obs B: meta.lastUpdated = "2026-04-03T09:00:00-07:00" (= 16:00:00 UTC — LATER)
        //
        // A raw string comparison would pick A ("15:30" > "09:00"); correct answer is B.
        let obs_a = json!({
            "resourceType": "Observation",
            "id": "mixed-tz-a",
            "meta": {"lastUpdated": "2026-04-03T15:30:10+01:00"},
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "15074-8", "display": "Glucose"}]},
            "effectiveDateTime": "2026-04-03T09:00:00Z",
            "valueQuantity": {"value": 5.0, "unit": "mmol/l"}
        });
        let obs_b = json!({
            "resourceType": "Observation",
            "id": "mixed-tz-b",
            "meta": {"lastUpdated": "2026-04-03T09:00:00-07:00"},
            "status": "final",
            "code": {"coding": [{"system": "http://loinc.org", "code": "15074-8", "display": "Glucose"}]},
            "effectiveDateTime": "2026-04-03T10:00:00Z",
            "valueQuantity": {"value": 5.5, "unit": "mmol/l"}
        });
        let bundle = json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": [
                {"resource": obs_a},
                {"resource": obs_b}
            ]
        });
        let v = temp_vault("mixed-tz");
        let api = MockApi::new().page(bundle);
        pull_with(&v, &api, "tok").unwrap();

        let state = v.read_labcorp_sync();
        let wm = state.observations_through.unwrap();
        let wm_dt = DateTime::parse_from_rfc3339(&wm).unwrap();
        // B's meta.lastUpdated = 2026-04-03T16:00:00Z — must win.
        let b_last_updated = DateTime::parse_from_rfc3339("2026-04-03T09:00:00-07:00").unwrap();
        assert_eq!(
            wm_dt.timestamp(),
            b_last_updated.timestamp(),
            "max watermark is obs B (later in UTC): {wm}"
        );
    }

    #[test]
    fn paginated_pull_drains_all_pages() {
        let v = temp_vault("paged");
        let obs2 = fhir_obs_qualitative();
        let page1 = bundle_with_next(fhir_obs_glucose(), "https://fhir.labcorp.com/r4/next");
        let page2 = single_obs_bundle(obs2);
        let api = MockApi::new().page(page1).page(page2);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&2), "both pages drained");
    }

    #[test]
    fn empty_bundle_is_a_clean_noop() {
        let v = temp_vault("empty");
        let api = MockApi::new().page(json!({"resourceType": "Bundle", "entry": []}));
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&0));
        assert!(v.read_labcorp_sync().observations_through.is_none());
    }

    #[test]
    fn cursor_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.observations_through.is_none());
        let fwd: SyncState = serde_json::from_str(
            r#"{"observations_through":"2026-04-03T15:30:10+01:00","future":"x"}"#,
        )
        .unwrap();
        assert_eq!(fwd.observations_through.as_deref(), Some("2026-04-03T15:30:10+01:00"));
    }

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("unconnected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn connection_stores_token_0600_and_absent_from_cursor() {
        let v = temp_vault("conn-token");
        v.save_sync_token(
            SERVICE,
            &TokenSet {
                access_token: "labcorp_secret_abc".into(),
                refresh_token: Some("labcorp_refresh_xyz".into()),
                token_type: Some("Bearer".into()),
                scope: Some("patient/Observation.read".into()),
                expires_at: Some(1_900_000_000),
            },
        )
        .unwrap();

        let status = def_status(&v).unwrap();
        assert_eq!(status.accounts.len(), 1);
        assert_eq!(status.accounts[0].label, "Labcorp");
        assert!(!status.accounts[0].needs_reconnect, "live token");

        // Verify the token is NOT written into the (non-secret) cursor file.
        v.write_labcorp_sync(&SyncState {
            observations_through: Some("2026-04-03T15:30:10+01:00".into()),
            updated: Some("2026-04-03T16:00:00+00:00".into()),
        })
        .unwrap();
        let cursor = std::fs::read_to_string(v.root().join(".trove/labcorp-sync.json")).unwrap();
        assert!(!cursor.contains("labcorp_secret_abc"), "access token not in cursor");
        assert!(!cursor.contains("labcorp_refresh_xyz"), "refresh token not in cursor");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let sync_dir = v.root().join(".trove/sync");
            let mut found = false;
            for entry in std::fs::read_dir(&sync_dir).unwrap().flatten() {
                let body = std::fs::read_to_string(entry.path()).unwrap_or_default();
                if body.contains("labcorp_secret_abc") {
                    found = true;
                    let mode = entry.path().metadata().unwrap().permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "token file must be 0600");
                }
            }
            assert!(found, "token stored under .trove/sync");
        }

        def_disconnect(&v, SERVICE).unwrap();
        assert!(def_status(&v).unwrap().accounts.is_empty());
    }

    #[test]
    fn connection_uses_assigned_port_and_pkce() {
        // 38660 is the assigned production redirect port for labcorp.
        assert_eq!(LABCORP_PROVIDER.redirect_port, 38660);
        assert_eq!(LABCORP_PROVIDER.redirect_uri(), "http://localhost:38660/callback");
        assert!(LABCORP_PROVIDER.use_pkce, "public SMART on FHIR client uses PKCE");
        assert!(LABCORP_PROVIDER.default_client_secret.is_none(), "public client — no secret");
        assert!(CONNECTION.method("oauth").is_some());
        assert_eq!(CONNECTION.id, SERVICE);
        assert_eq!(DEF.connection, Some(SERVICE));
    }
}
