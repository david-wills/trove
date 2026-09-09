//! Medicare Blue Button 2.0 — Part A/B/D claims (ICD-10/CPT/NDC) via FHIR R4
//! for Medicare beneficiaries.
//!
//! Brief: docs/integrations/cms-blue-button.md
//!
//! A **Periodic** cloud pull (daily — claims lag weeks anyway) from the CMS
//! fixed national endpoint `https://api.bluebutton.cms.gov/v2/fhir`.  The user
//! authorizes via their Medicare.gov credentials through a CMS-registered OAuth2
//! app (PKCE public client, free registration at bluebutton.cms.gov).  This is
//! NOT the per-org SMART on FHIR discovery flow used by `smart_on_fhir` — there
//! is exactly one national endpoint and one fixed OAuth configuration.
//!
//! ## Evidence for endpoints (confirmed live 2026-06-17)
//!
//! - Sandbox `.well-known/smart-configuration`:
//!   `https://sandbox.bluebutton.cms.gov/v1/fhir/.well-known/smart-configuration`
//!   → authorization_endpoint: `https://sandbox.bluebutton.cms.gov/v2/o/authorize`
//!   → token_endpoint:         `https://sandbox.bluebutton.cms.gov/v2/o/token`
//!
//! - Production `.well-known/smart-configuration`:
//!   `https://api.bluebutton.cms.gov/v2/fhir/.well-known/smart-configuration`
//!   → authorization_endpoint: `https://api.bluebutton.cms.gov/v2/o/authorize`
//!   → token_endpoint:         `https://api.bluebutton.cms.gov/v2/o/token`
//!
//! - Sandbox FHIR capability statement:
//!   `https://sandbox.bluebutton.cms.gov/v2/fhir/metadata`
//!   → FHIR version: 4.0.1
//!   → Resources: Patient, Coverage, ExplanationOfBenefit
//!   → Supports `_lastUpdated`, `_count`, `startIndex` pagination
//!   → Supports PKCE (`code_challenge_methods_supported`: ["S256"])
//!
//! ## Vault layout
//!
//! - **Raw (all resources):**
//!   `health/medical/cms-blue-button/raw/<ResourceType>/YYYY-MM.jsonl` —
//!   verbatim FHIR R4 JSON for every resource returned, full fidelity,
//!   unconditional.  Three types: ExplanationOfBenefit, Coverage, Patient.
//!
//! - **Contract:** None bound yet.  EOB diagnoses (ICD-10) map to the unbound
//!   `health-medical.condition` draft; Part D fills (NDC/days-supply/prescriber)
//!   map to the unbound `health-medical.medication` draft; both are
//!   `deferred-sibling-draft` until David ratifies those shapes.  Raw is the
//!   only sink for now.
//!
//! ## EOB key field evidence
//! (from BFD FHIR test fixtures at CMSgov/beneficiary-fhir-data on GitHub
//!  and CARIN BB IG examples)
//!
//! Part D (pharmacy) EOB — structural fields confirmed across BFD + CARIN BB:
//!   - `id`                                  → guid
//!   - `type.coding[].code`                  → "PDE" or "pharmacy" → claim type
//!   - `billablePeriod.start`                → service date
//!   - `item[0].servicedDate`                → fill date
//!   - `item[0].productOrService.coding[0]`  → NDC code
//!     NOTE: NDC coding system varies by BFD version; live endpoint may emit
//!     `http://hl7.org/fhir/sid/ndc` (FHIR standard) rather than the OID form.
//!     The raw collector stores verbatim — future medication contract must
//!     tolerate both and filter by `code` value, not `system` alone.
//!   - `item[0].quantity.value`              → quantity dispensed
//!   - `supportingInfo[].valueQuantity.value` (category "daysSupply"/"dayssupply") → days supply
//!     NOTE: BFD uses lowercase `dayssupply`/`refillnum`; CARIN IG uses
//!     camelCase `daysSupply`/`refillNum`.  Raw collector stores verbatim —
//!     future medication contract must match case-insensitively or check both forms.
//!   - `careTeam[0].provider.display` (role "prescribing")        → prescriber name
//!   - `total[].amount.value` (category "submitted" or "drugcost") → total drug cost
//!     NOTE: CMS BB2.0/CARIN C4BBAdjudication may use "drugcost" for the drug
//!     total rather than "submitted".  Raw stores verbatim; future contract
//!     should accept both category codes.
//!
//! Part A/B (institutional/carrier) EOB — confirmed from CARIN IG EOBInpatient1.json:
//!   - `id`                                                        → guid
//!   - `type.coding[].code`                                        → claim type
//!   - `billablePeriod.start/end`                                  → service period
//!   - `diagnosis[].diagnosisCodeableConcept.coding[].code`
//!     (system "http://hl7.org/fhir/sid/icd-10-cm")               → ICD-10 diagnosis
//!   - `item[0].servicedDate`                                      → service date
//!   - `total[].amount.value` (category "submitted")               → submitted amount
//!   - `provider.display`                                          → provider
//!
//! ## Cursor
//!
//! Per resource type: max `meta.lastUpdated` across all fetched resources,
//! persisted in `.trove/cms-blue-button-sync.json`.  Paging follows FHIR Bundle
//! `link.relation=next`.  Watermark advances per type after full drain.
//!
//! ## Privacy
//!
//! Claims data exposes diagnoses, procedures, and prescriptions — among the most
//! sensitive data in the vault.  Ships opt-in (`default_on: false`,
//! `toggleable: true`) with explicit acknowledgement.  Audience is narrow:
//! US Medicare beneficiaries (65+ or disability) only — anyone else has nothing
//! to pull (clean empty state, not an error).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::{self, AppCredentials, Provider, TokenSet};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SERVICE: &str = "cms-blue-button";

/// Raw FHIR NDJSON root — actual files under <RAW_ROOT>/<ResourceType>/
const RAW_ROOT: &str = "health/medical/cms-blue-button/raw";

/// Non-secret rebuildable cursor.  Delete to re-drain full history.
const SYNC_FILE: &str = ".trove/cms-blue-button-sync.json";

/// Production FHIR R4 base URL.
/// Confirmed via `GET https://api.bluebutton.cms.gov/v2/fhir/metadata` (2026-06-17).
const FHIR_BASE: &str = "https://api.bluebutton.cms.gov/v2/fhir";

/// FHIR resource types polled on each sync.
const RESOURCE_TYPES: &[&str] = &["ExplanationOfBenefit", "Coverage", "Patient"];

/// Page size.  BB2.0 supports `_count`; 50 is conservative for EOB (large resources).
const PAGE_SIZE: u32 = 50;

/// HTTP request timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Sync daily — Medicare claims lag weeks from the date of service.
const SYNC_SECS: u64 = 24 * 3600;

// ---------------------------------------------------------------------------
// OAuth provider (BB2.0 PKCE public client).
//
// Endpoints confirmed live 2026-06-17:
//   GET https://api.bluebutton.cms.gov/v2/fhir/.well-known/smart-configuration
//   authorization_endpoint: https://api.bluebutton.cms.gov/v2/o/authorize
//   token_endpoint:         https://api.bluebutton.cms.gov/v2/o/token
//   code_challenge_methods_supported: ["S256"]  (PKCE confirmed)
//
// Assigned production redirect port: 38580 + 236 = 38816.
// Register http://localhost:38816/callback at bluebutton.cms.gov.

/// CMS Blue Button 2.0 OAuth2 provider.
pub static BB2_PROVIDER: Provider = Provider {
    service: SERVICE,
    display_name: "Medicare Blue Button",
    auth_url: "https://api.bluebutton.cms.gov/v2/o/authorize",
    token_url: "https://api.bluebutton.cms.gov/v2/o/token",
    // Patient-access scopes confirmed from the BB2.0 `.well-known/smart-configuration`.
    // profile = beneficiary identity; patient/Patient.read, /Coverage.read,
    // /ExplanationOfBenefit.read = the three resource types; offline_access = refresh token.
    scopes: "patient/Patient.read patient/Coverage.read patient/ExplanationOfBenefit.read \
             profile offline_access",
    // Assigned production redirect port for cms-blue-button (#236).
    // 38580 + 236 = 38816.
    redirect_port: 38816,
    // BB2.0 supports PKCE (S256) as confirmed by the smart-configuration.
    use_pkce: true,
    // PKCE public client: client_id in form body, no HTTP Basic auth.
    basic_auth: false,
    // Baked at build time: TROVE_CMS_BLUE_BUTTON_CLIENT_ID.
    // Empty default — register a free app at bluebutton.cms.gov (reviewed by CMS).
    default_client_id: option_env!("TROVE_CMS_BLUE_BUTTON_CLIENT_ID"),
    // Public PKCE client: no secret.
    default_client_secret: None,
    extra_auth_params: &[],
};

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Use the EOB partition as the "most recent data" indicator.
    crate::registry::newest_stem(
        &vault.root().join(format!("{RAW_ROOT}/ExplanationOfBenefit")),
    )
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let total: u64 = out.counts.values().sum();
            Ok(crate::registry::CollectOutcome::note_if(total > 0, || {
                format!("cms-blue-button synced — {total} claims")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "cms-blue-button sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub `&crate::cms_blue_button::DEF`
/// line already exists — this body REPLACES the NotWired stub).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: SERVICE,
        name: "Medicare Blue Button",
        kind: IntegrationKind::CloudSync,
        // Claims data (diagnoses, procedures, prescriptions) is highly sensitive — opt-in only.
        default_on: false,
        description:
            "Pulls your Medicare Part A (inpatient), Part B (physician/outpatient), and Part D \
             (prescription drug) claims via the CMS Blue Button 2.0 FHIR R4 API.  Available to \
             US Medicare beneficiaries only (age 65+ or disability).  Claims show what was \
             billed, diagnosed, and filled — ICD-10 diagnoses, CPT/HCPCS procedures, and NDC \
             drug codes.",
        domain: "health",
        vault_path: "health/medical/cms-blue-button/",
        toggleable: true,
        setup: &[
            "This integration is available to US Medicare beneficiaries only.  Anyone without a \
             Medicare account will see an empty result — not an error.",
            "Claims data exposes diagnoses, procedures, and prescriptions.  Enabling this opts \
             you in explicitly.",
            "Register a free developer app at bluebutton.cms.gov (reviewed by CMS).  Set the \
             OAuth redirect URI to http://localhost:38816/callback (PKCE public client — no \
             client secret required).",
            "Paste your Client ID and click Connect.  You will be redirected to sign in with \
             your Medicare.gov credentials.",
        ],
        caveats:
            "US Medicare beneficiaries only.  Claims lag several weeks from the date of \
             service.  This is billing data, not clinical notes or imaging — it \
             complements Epic MyChart for a fuller medical picture.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some(SERVICE),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection (BB2.0 OAuth PKCE).

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(BB2_PROVIDER.service)?.is_some()
        || BB2_PROVIDER.default_credentials().is_some();
    let accounts = match vault.load_sync_token(BB2_PROVIDER.service)? {
        Some(token) => {
            let needs_reconnect = token.expired() && token.refresh_token.is_none();
            vec![ConnectedAccount {
                key: BB2_PROVIDER.service.to_string(),
                label: "Medicare Blue Button".to_string(),
                connected_at: None,
                expires_at: token.expires_at,
                needs_reconnect,
                extra: BTreeMap::new(),
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds the
/// `&crate::cms_blue_button::CONNECTION,` line — this module only declares it).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Medicare Blue Button",
    methods: &[ConnectMethod::OAuth {
        provider: &BB2_PROVIDER,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["cms-blue-button"],
    setup: &[
        "Register a free developer app at bluebutton.cms.gov (reviewed by CMS; free sandbox \
         access is instant, production review takes a few days).  Set the redirect URI to \
         http://localhost:38816/callback (no client secret — this is a PKCE public client).",
        "Paste your Client ID here.  On Connect, your browser will open the Medicare.gov \
         authorization page.",
        "After authorizing, Trove syncs your Part A (inpatient), Part B (physician), and Part D \
         (prescription) claims.  Anyone without a Medicare account will see an empty result.",
    ],
};

/// Interactive OAuth connect: opens the consent page, waits for the redirect, saves the token.
/// Blocking — call from a background thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(BB2_PROVIDER.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(BB2_PROVIDER.service)?
            .or_else(|| BB2_PROVIDER.default_credentials())
            .context(
                "no Medicare Blue Button Client ID — register a free app at bluebutton.cms.gov \
                 and enter its Client ID in the Integrations tab",
            )?,
    };
    let flow = oauth::OauthFlow::start(&BB2_PROVIDER, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(BB2_PROVIDER.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Cursor.

/// Per-resource-type watermarks.  Keyed by FHIR resourceType.
/// Each value is the max `meta.lastUpdated` seen, as UTC RFC3339.
/// Delete this file to re-drain the full history.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncState {
    /// resourceType → max `meta.lastUpdated` (UTC RFC3339).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub watermarks: HashMap<String, String>,
    /// RFC3339 timestamp of the last successful full sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl Vault {
    pub fn read_bb2_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn write_bb2_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

#[derive(Debug)]
pub enum FetchError {
    Unauthorized,
    NotFound,
    Other(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Unauthorized => write!(f, "unauthorized (HTTP 401/403)"),
            FetchError::NotFound => write!(f, "not found (HTTP 404)"),
            FetchError::Other(m) => write!(f, "{m}"),
        }
    }
}

/// FHIR operations the pull needs.  Injectable so tests run offline.
pub trait Bb2Api {
    /// `GET /R4/<ResourceType>?patient=<id>&_sort=_lastUpdated&_count=<n>
    ///   [&_lastUpdated=ge<watermark>]`
    /// OR follow a `next` bundle link directly.
    fn resources(
        &self,
        token: &str,
        resource_type: &str,
        patient_id: &str,
        last_updated_ge: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<Value, FetchError>;

    /// `GET /R4/Patient?_format=json` — resolve the patient id.
    fn patient(&self, token: &str) -> Result<Value, FetchError>;
}

/// Thin BB2.0 FHIR client backed by ureq.
pub struct Bb2Client;

impl Bb2Client {
    fn get_json(&self, url: &str, token: &str) -> Result<Value, FetchError> {
        match ureq::get(url)
            .timeout(HTTP_TIMEOUT)
            .set("Authorization", &format!("Bearer {token}"))
            .set("Accept", "application/fhir+json")
            .call()
        {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| FetchError::Other(format!("parsing FHIR JSON: {e}"))),
            Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Unauthorized),
            Err(ureq::Error::Status(404, _)) => Err(FetchError::NotFound),
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

impl Bb2Api for Bb2Client {
    fn patient(&self, token: &str) -> Result<Value, FetchError> {
        let url = format!("{FHIR_BASE}/Patient?_format=json");
        self.get_json(&url, token)
    }

    fn resources(
        &self,
        token: &str,
        resource_type: &str,
        patient_id: &str,
        last_updated_ge: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<Value, FetchError> {
        let url = if let Some(next) = next_url {
            next.to_string()
        } else {
            let mut q = format!(
                "{FHIR_BASE}/{resource_type}?patient={}&_sort=_lastUpdated&_count={}&_format=json",
                urlencode(patient_id),
                PAGE_SIZE,
            );
            if let Some(ge) = last_updated_ge {
                q.push_str("&_lastUpdated=ge");
                q.push_str(&urlencode(ge));
            }
            q
        };
        self.get_json(&url, token)
    }
}

/// Percent-encode a query parameter value (RFC 3986 unreserved pass through).
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
// FHIR Bundle navigation helpers.

/// All `entry[].resource` objects in a FHIR Bundle where
/// `resourceType == expected_type`.
pub fn bundle_entries<'a>(bundle: &'a Value, expected_type: &str) -> Vec<&'a Value> {
    bundle
        .get("entry")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("resource"))
                .filter(|r| {
                    r.get("resourceType").and_then(Value::as_str).unwrap_or("") == expected_type
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `url` of the `link` with `relation == "next"` in a FHIR Bundle.
pub fn next_link(bundle: &Value) -> Option<&str> {
    bundle
        .get("link")
        .and_then(Value::as_array)?
        .iter()
        .find(|l| l.get("relation").and_then(Value::as_str) == Some("next"))
        .and_then(|l| l.get("url"))
        .and_then(Value::as_str)
}

// ---------------------------------------------------------------------------
// Helpers.

/// Top-level string field, trimmed; empty when missing or non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Descend a path of keys and return the leaf as a trimmed string.
fn nested_str(v: &Value, path: &[&str]) -> String {
    let mut cur = v;
    for &key in path {
        match cur.get(key) {
            Some(next) => cur = next,
            None => return String::new(),
        }
    }
    cur.as_str().unwrap_or("").trim().to_string()
}

/// Parse a FHIR instant (RFC3339/ISO8601 with offset) to UTC DateTime.
fn parse_fhir_instant(s: &str) -> Option<DateTime<chrono::Utc>> {
    s.parse::<chrono::DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Return the later of two optional watermark strings, comparing as UTC instants.
/// Falls back to lexicographic comparison if one is unparseable.
fn max_watermark(a: Option<&str>, b: &str) -> String {
    match a {
        None => b.to_string(),
        Some(cur) => match (parse_fhir_instant(cur), parse_fhir_instant(b)) {
            (Some(cur_dt), Some(b_dt)) => {
                if b_dt > cur_dt { b.to_string() } else { cur.to_string() }
            }
            _ => {
                if b > cur { b.to_string() } else { cur.to_string() }
            }
        },
    }
}

/// Best-effort service date from an EOB or other FHIR resource.
/// Falls back to meta.lastUpdated rather than losing the partition key.
///
/// EOB field order:
///   1. `item[0].servicedDate` — most precise (fill date for Part D, line date for B)
///   2. `billablePeriod.start`  — claim service period start
///   3. `billablePeriod.end`
///   4. `meta.lastUpdated`      — last resort
fn resource_ts(res: &Value) -> String {
    // item[0].servicedDate
    let item_date = res
        .get("item")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|i| i.get("servicedDate"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !item_date.is_empty() {
        return item_date;
    }

    for s in [
        nested_str(res, &["billablePeriod", "start"]),
        nested_str(res, &["billablePeriod", "end"]),
        nested_str(res, &["meta", "lastUpdated"]),
    ] {
        if !s.is_empty() {
            return s;
        }
    }
    Local::now().date_naive().to_string()
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Full-fidelity raw row: the verbatim FHIR resource JSON.
/// `ts` is only for month-partitioning and is not serialized.
struct RawLine {
    ts: String,
    value: Value,
}

impl serde::Serialize for RawLine {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        self.value.serialize(ser)
    }
}

/// Write new raw resources for a given resource type, deduped by FHIR `id`.
///
/// Returns the number of new rows written.
fn write_raw(vault: &Vault, resource_type: &str, resources: Vec<Value>) -> Result<u64> {
    let raw_dir = format!("{RAW_ROOT}/{resource_type}");
    let stream = vault.stream(&raw_dir, Partition::Month);

    // Load existing ids to deduplicate.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            let g = str_field(&v, "id");
            if !g.is_empty() {
                seen.insert(g);
            }
        }
    }

    let mut new_rows: Vec<RawLine> = Vec::new();
    for r in resources {
        let id = str_field(&r, "id");
        if id.is_empty() || !seen.insert(id) {
            continue;
        }
        let ts = resource_ts(&r);
        new_rows.push(RawLine { ts, value: r });
    }
    let count = new_rows.len() as u64;
    stream.append(&new_rows, |r| &r.ts)?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Patient id resolution.

/// Resolve the beneficiary patient id.  The BB2.0 Patient endpoint returns
/// a Bundle with a single Patient entry for the authorized beneficiary.
fn resolve_patient_id(api: &impl Bb2Api, token: &str) -> Result<String, FetchError> {
    let bundle = api.patient(token)?;
    // Bundle (searchset) wraps the Patient; also accept a direct Patient resource.
    let entry = bundle
        .get("entry")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|e| e.get("resource"));
    let patient = entry.unwrap_or(&bundle);
    let id = str_field(patient, "id");
    if id.is_empty() {
        Err(FetchError::Other(
            "Blue Button Patient endpoint returned no id — cannot query per-beneficiary resources"
                .into(),
        ))
    } else {
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// The pull.

/// Entry point called by the Periodic runner + "Sync now".
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context(
            "Medicare Blue Button is not connected — log in via your Medicare.gov credentials \
             in the Integrations tab",
        )?;

    // Refresh if needed (BB2.0 issues refresh tokens with `offline_access` scope).
    let token = ensure_fresh(vault, token)?;

    pull_with(vault, &Bb2Client, &token.access_token)
}

fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(BB2_PROVIDER.service)?
        .or_else(|| BB2_PROVIDER.default_credentials())
        .context(
            "Medicare Blue Button token expired and no client id to refresh it — reconnect \
             from the Integrations tab",
        )?;
    match oauth::refresh_token(&BB2_PROVIDER, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(BB2_PROVIDER.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(BB2_PROVIDER.service)?;
            anyhow::bail!(
                "Medicare Blue Button token refresh failed ({e}) — reconnect from the \
                 Integrations tab"
            );
        }
    }
}

/// Testable pull body over an injected API + access token.
///
/// 1. Resolves the patient id (re-fetched fresh — BB2.0 has a single beneficiary
///    per token; caching is not necessary given the daily cadence).
/// 2. For each resource type: drains all pages from the `meta.lastUpdated`
///    watermark, writes raw (all types), advances the watermark.
/// 3. Returns a count map of new raw rows per resource type.
pub fn pull_with(vault: &Vault, api: &impl Bb2Api, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_bb2_sync();

    // Resolve patient id.
    let patient_id = resolve_patient_id(api, token).map_err(|e| match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Medicare Blue Button rejected the token — reconnect from the Integrations tab"
        ),
        other => anyhow::anyhow!("Medicare Blue Button patient lookup failed: {other}"),
    })?;

    let mut total: u64 = 0;

    for &rt in RESOURCE_TYPES {
        let watermark = state.watermarks.get(rt).map(String::as_str);
        let mut page_url: Option<String> = None;
        let mut new_watermark: Option<String> = None;

        loop {
            let result =
                api.resources(token, rt, &patient_id, watermark, page_url.as_deref());
            let bundle = match result {
                Err(FetchError::Unauthorized) => {
                    return Err(anyhow::anyhow!(
                        "Medicare Blue Button rejected the token (401) on {} — reconnect",
                        rt
                    ));
                }
                // BB2.0 may return 404 for resource types a beneficiary has no data for.
                Err(FetchError::NotFound) => break,
                Err(other) => {
                    return Err(anyhow::anyhow!(
                        "Medicare Blue Button {} fetch failed: {other}",
                        rt
                    ));
                }
                Ok(b) => b,
            };

            let entries: Vec<Value> =
                bundle_entries(&bundle, rt).into_iter().cloned().collect();

            // Track max meta.lastUpdated across this page for the watermark.
            for entry in &entries {
                if let Some(lu) = entry
                    .get("meta")
                    .and_then(|m| m.get("lastUpdated"))
                    .and_then(Value::as_str)
                {
                    new_watermark = Some(max_watermark(new_watermark.as_deref(), lu));
                }
            }

            let n = write_raw(vault, rt, entries)?;
            total += n;

            match next_link(&bundle) {
                Some(next) => page_url = Some(next.to_string()),
                None => break,
            }
        }

        // Advance the watermark for this resource type after the full drain.
        if let Some(wm) = new_watermark {
            let prev = state.watermarks.get(rt).map(String::as_str);
            let advanced = max_watermark(prev, &wm);
            state.watermarks.insert(rt.to_string(), advanced);
            // Persist partial progress so a failure on a later type doesn't lose the work.
            vault.write_bb2_sync(&state)?;
        }

        // Per-type count is already accumulated in `total`.
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_bb2_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("Medicare Blue Button synced — {total} new claims/records"),
        counts: {
            let mut m = BTreeMap::new();
            m.insert("raw", total);
            m
        },
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-cms-bb-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — field shapes confirmed from:
    //  - BFD test endpoint-responses/v2/eobReadPde.json (Part D pharmacy EOB)
    //  - CARIN IG EOBInpatient1.json (Part A institutional EOB)
    //  - BB2.0 FHIR metadata endpoint (Patient/Coverage/EOB resources confirmed)

    /// Part D (pharmacy) ExplanationOfBenefit.
    ///
    /// Field evidence (structural shape; exact coding system/category values vary by
    /// BFD version — see module-level "EOB key field evidence" NOTE blocks):
    ///   id                                                            → "pde-89"
    ///   type.coding[].code                                            → "PDE" (pharmacy)
    ///   billablePeriod.start                                          → service start
    ///   item[0].servicedDate                                          → "2015-05-12" (fill date)
    ///   item[0].productOrService.coding[0].code                       → NDC "000000000"
    ///   item[0].productOrService.coding[0].system                     → NDC OID form used here;
    ///                                                                    live endpoint may use
    ///                                                                    http://hl7.org/fhir/sid/ndc
    ///   item[0].quantity.value                                        → 60.0
    ///   supportingInfo[].valueQuantity (category "daysSupply")        → 30
    ///     (BFD sandbox uses lowercase "dayssupply"; CARIN IG uses "daysSupply")
    ///   careTeam[0].provider.display (role "prescribing")             → "DR. ROBERT BISBEE MD"
    ///   total[0].amount.value (category "submitted")                  → 550.0 USD
    ///     (live BFD/CARIN may use "drugcost" instead of "submitted")
    fn eob_part_d() -> Value {
        json!({
            "resourceType": "ExplanationOfBenefit",
            "id": "pde-89",
            "meta": {"lastUpdated": "2015-06-01T10:00:00+00:00"},
            "type": {
                "coding": [
                    {"system": "http://terminology.hl7.org/CodeSystem/claim-type", "code": "pharmacy"},
                    {"system": "https://bluebutton.cms.gov/resources/codesystem/eob-type", "code": "PDE"}
                ]
            },
            "billablePeriod": {"start": "2015-05-12", "end": "2015-05-12"},
            "patient": {"reference": "Patient/123"},
            "item": [{
                "sequence": 1,
                "servicedDate": "2015-05-12",
                "productOrService": {
                    "coding": [{
                        "system": "urn:oid:2.16.840.1.113883.6.69",
                        "code": "000000000",
                        "display": "Test Drug"
                    }]
                },
                "quantity": {"value": 60.0, "unit": "EA"}
            }],
            "supportingInfo": [
                {
                    "sequence": 1,
                    "category": {
                        "coding": [{"system": "http://hl7.org/fhir/us/carin-bb/CodeSystem/C4BBSupportingInfoType",
                                    "code": "refillNum"}]
                    },
                    "valueQuantity": {"value": 3}
                },
                {
                    "sequence": 2,
                    "category": {
                        "coding": [{"system": "http://hl7.org/fhir/us/carin-bb/CodeSystem/C4BBSupportingInfoType",
                                    "code": "daysSupply"}]
                    },
                    "valueQuantity": {"value": 30}
                }
            ],
            "careTeam": [{
                "sequence": 1,
                "provider": {"display": "DR. ROBERT BISBEE MD", "identifier": {"value": "1750384806"}},
                "role": {"coding": [{"code": "prescribing"}]}
            }],
            "total": [
                {
                    "category": {"coding": [{"code": "submitted"}]},
                    "amount": {"value": 550.0, "currency": "USD"}
                }
            ]
        })
    }

    /// Part A (inpatient institutional) ExplanationOfBenefit.
    ///
    /// Field evidence from CARIN IG EOBInpatient1.json:
    ///   id                                                              → "EOBInpatient1"
    ///   type.coding[0].code                                             → "institutional"
    ///   billablePeriod.start/end                                        → service period
    ///   diagnosis[0].diagnosisCodeableConcept.coding[0].code
    ///     (system hl7.org/fhir/sid/icd-10-cm)                          → "S06.0X1A"
    ///   item[0].servicedDate                                            → "2019-11-02"
    ///   total[0].category "submitted".amount.value                      → 2650
    ///   provider.display                                                → "XXX Health Plan"
    fn eob_part_a() -> Value {
        json!({
            "resourceType": "ExplanationOfBenefit",
            "id": "EOBInpatient1",
            "meta": {"lastUpdated": "2019-12-01T00:00:00+00:00"},
            "type": {
                "coding": [{"system": "http://terminology.hl7.org/CodeSystem/claim-type",
                             "code": "institutional"}]
            },
            "billablePeriod": {"start": "2019-01-01", "end": "2019-10-31"},
            "patient": {"reference": "Patient/123"},
            "provider": {"reference": "Organization/ProviderOrg1", "display": "XXX Health Plan"},
            "diagnosis": [{
                "sequence": 1,
                "diagnosisCodeableConcept": {
                    "coding": [{
                        "system": "http://hl7.org/fhir/sid/icd-10-cm",
                        "code": "S06.0X1A",
                        "display": "Concussion with loss of consciousness"
                    }]
                },
                "type": [{"coding": [{"code": "principal"}]}]
            }],
            "item": [{
                "sequence": 1,
                "servicedDate": "2019-11-02",
                "productOrService": {
                    "coding": [{
                        "system": "http://terminology.hl7.org/CodeSystem/data-absent-reason",
                        "code": "not-applicable"
                    }]
                }
            }],
            "total": [
                {
                    "category": {"coding": [{"code": "submitted"}]},
                    "amount": {"value": 2650.0, "currency": "USD"}
                },
                {
                    "category": {"coding": [{"code": "paidtoprovider"}]},
                    "amount": {"value": 620.0, "currency": "USD"}
                }
            ]
        })
    }

    /// Coverage resource (plan information).
    fn coverage_resource() -> Value {
        json!({
            "resourceType": "Coverage",
            "id": "coverage-part-a",
            "meta": {"lastUpdated": "2020-01-01T00:00:00+00:00"},
            "status": "active",
            "type": {"coding": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ActCode",
                                  "code": "HIP", "display": "health insurance plan policy"}]},
            "beneficiary": {"reference": "Patient/123"},
            "period": {"start": "2015-01-01"}
        })
    }

    /// Patient resource (beneficiary demographics — minimal).
    fn patient_resource() -> Value {
        json!({
            "resourceType": "Patient",
            "id": "patient-123",
            "meta": {"lastUpdated": "2015-01-01T00:00:00+00:00"},
            "name": [{"family": "DOE", "given": ["JANE"]}],
            "birthDate": "1950-01-01",
            "gender": "female"
        })
    }

    fn patient_bundle(patient_id: &str) -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": [{"resource": {"resourceType": "Patient", "id": patient_id}}]
        })
    }

    fn fhir_bundle(resources: Vec<Value>) -> Value {
        let entries: Vec<Value> =
            resources.into_iter().map(|r| json!({"resource": r})).collect();
        json!({"resourceType": "Bundle", "type": "searchset", "entry": entries})
    }

    fn fhir_bundle_with_next(resources: Vec<Value>, next_url: &str) -> Value {
        let entries: Vec<Value> =
            resources.into_iter().map(|r| json!({"resource": r})).collect();
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": [{"relation": "next", "url": next_url}],
            "entry": entries
        })
    }

    // -----------------------------------------------------------------------
    // Mock API.

    struct MockApi {
        patient: Value,
        responses: RefCell<VecDeque<(String, Result<Value, FetchError>)>>,
    }

    impl MockApi {
        fn new(patient: Value) -> Self {
            MockApi { patient, responses: RefCell::new(VecDeque::new()) }
        }

        fn page(self, rt: &str, resources: Vec<Value>) -> Self {
            self.responses
                .borrow_mut()
                .push_back((rt.to_string(), Ok(fhir_bundle(resources))));
            self
        }

        fn page_with_next(self, rt: &str, resources: Vec<Value>, next_url: &str) -> Self {
            self.responses.borrow_mut().push_back((
                rt.to_string(),
                Ok(fhir_bundle_with_next(resources, next_url)),
            ));
            self
        }
    }

    impl Bb2Api for MockApi {
        fn patient(&self, _token: &str) -> Result<Value, FetchError> {
            Ok(self.patient.clone())
        }

        fn resources(
            &self,
            _token: &str,
            resource_type: &str,
            _patient_id: &str,
            _last_updated_ge: Option<&str>,
            _next_url: Option<&str>,
        ) -> Result<Value, FetchError> {
            let mut queue = self.responses.borrow_mut();
            for i in 0..queue.len() {
                if queue[i].0 == resource_type {
                    return queue.remove(i).unwrap().1;
                }
            }
            Ok(fhir_bundle(vec![]))
        }
    }

    // -----------------------------------------------------------------------
    // Pull tests.

    #[test]
    fn full_pull_writes_all_resource_types_to_raw() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(patient_bundle("bene-001"))
            .page("ExplanationOfBenefit", vec![eob_part_d(), eob_part_a()])
            .page("Coverage", vec![coverage_resource()])
            .page("Patient", vec![patient_resource()]);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(*out.counts.get("raw").unwrap_or(&0), 4u64, "4 raw rows total");

        // Verify each resource type folder was written.
        let eob_raw = v.stream(&format!("{RAW_ROOT}/ExplanationOfBenefit"), Partition::Month);
        let eob_count: usize = eob_raw
            .partitions()
            .unwrap()
            .iter()
            .map(|k| eob_raw.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(eob_count, 2, "two EOB rows (Part D + Part A)");

        let cov_raw = v.stream(&format!("{RAW_ROOT}/Coverage"), Partition::Month);
        let cov_count: usize = cov_raw
            .partitions()
            .unwrap()
            .iter()
            .map(|k| cov_raw.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(cov_count, 1, "one Coverage row");

        // Watermarks set.
        let state = v.read_bb2_sync();
        assert!(state.watermarks.contains_key("ExplanationOfBenefit"), "EOB watermark set");
        assert!(state.updated.is_some(), "updated timestamp persisted");
    }

    #[test]
    fn deduplication_prevents_double_write_on_re_pull() {
        let v = temp_vault("dedup");
        let api = MockApi::new(patient_bundle("bene-001"))
            .page("ExplanationOfBenefit", vec![eob_part_d()]);
        let out1 = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(*out1.counts.get("raw").unwrap_or(&0), 1u64, "first pull: 1 row");

        // Re-pull the same EOB.
        let api2 = MockApi::new(patient_bundle("bene-001"))
            .page("ExplanationOfBenefit", vec![eob_part_d()]);
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(*out2.counts.get("raw").unwrap_or(&0), 0u64, "no new rows on re-pull");

        let eob_raw = v.stream(&format!("{RAW_ROOT}/ExplanationOfBenefit"), Partition::Month);
        let total: usize = eob_raw
            .partitions()
            .unwrap()
            .iter()
            .map(|k| eob_raw.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(total, 1, "still exactly one row after dedup re-pull");
    }

    #[test]
    fn empty_account_is_clean_noop() {
        // A beneficiary with no claims (or a non-beneficiary) produces zero rows.
        let v = temp_vault("empty");
        let api = MockApi::new(patient_bundle("bene-empty"));
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(*out.counts.get("raw").unwrap_or(&0), 0u64, "no rows for empty account");
        let state = v.read_bb2_sync();
        assert!(state.watermarks.is_empty(), "no watermarks without data");
    }

    #[test]
    fn pull_without_connection_returns_clear_error() {
        let v = temp_vault("unconnected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(
            err.contains("not connected") || err.contains("Blue Button"),
            "clear error: {err}"
        );
    }

    #[test]
    fn resource_ts_prefers_item_serviceddate_then_billable_period() {
        // Part D: item[0].servicedDate takes priority.
        let pde = eob_part_d();
        assert_eq!(resource_ts(&pde), "2015-05-12", "Part D uses servicedDate");

        // Remove item to fall back to billablePeriod.start.
        let mut no_item = eob_part_a().clone();
        no_item.as_object_mut().unwrap().remove("item");
        assert_eq!(resource_ts(&no_item), "2019-01-01", "no item → billablePeriod.start");
    }

    #[test]
    fn bundle_entries_extracts_by_resource_type() {
        let bundle = fhir_bundle(vec![eob_part_d(), coverage_resource()]);
        let eobs = bundle_entries(&bundle, "ExplanationOfBenefit");
        assert_eq!(eobs.len(), 1);
        assert_eq!(str_field(eobs[0], "id"), "pde-89");
        let covs = bundle_entries(&bundle, "Coverage");
        assert_eq!(covs.len(), 1);
        assert_eq!(str_field(covs[0], "id"), "coverage-part-a");
    }

    #[test]
    fn next_link_finds_next_page_url() {
        let bundle = fhir_bundle_with_next(vec![], "https://api.bluebutton.cms.gov/v2/fhir/next");
        assert_eq!(
            next_link(&bundle),
            Some("https://api.bluebutton.cms.gov/v2/fhir/next")
        );
        assert!(next_link(&fhir_bundle(vec![])).is_none());
    }

    #[test]
    fn watermark_advances_only_after_drain() {
        let v = temp_vault("watermark");
        // Two pages; both should be drained before watermark advances.
        let api = MockApi::new(patient_bundle("p"))
            .page_with_next(
                "ExplanationOfBenefit",
                vec![eob_part_d()],
                "https://api.bluebutton.cms.gov/page2",
            )
            .page("ExplanationOfBenefit", vec![eob_part_a()]);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(*out.counts.get("raw").unwrap_or(&0), 2u64, "both pages drained");

        let state = v.read_bb2_sync();
        assert!(state.watermarks.contains_key("ExplanationOfBenefit"));
        // Watermark should be the max of the two lastUpdated values.
        let wm = state.watermarks.get("ExplanationOfBenefit").unwrap();
        assert!(
            wm.as_str() >= "2019-12-01",
            "watermark advanced to max lastUpdated: {wm}"
        );
    }

    #[test]
    fn eob_raw_preserves_ndc_and_diagnosis_codes() {
        // Verify that raw storage preserves all clinical codes at full fidelity.
        let v = temp_vault("raw_fidelity");
        let api = MockApi::new(patient_bundle("bene-001"))
            .page("ExplanationOfBenefit", vec![eob_part_d(), eob_part_a()]);
        pull_with(&v, &api, "tok").unwrap();

        let eob_raw = v.stream(&format!("{RAW_ROOT}/ExplanationOfBenefit"), Partition::Month);
        let rows: Vec<Value> = eob_raw
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|k| eob_raw.read::<Value>(k).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);

        // Part D row: NDC code preserved.
        let pde = rows.iter().find(|r| str_field(r, "id") == "pde-89").unwrap();
        let ndc = pde
            .get("item").and_then(Value::as_array).and_then(|a| a.first())
            .and_then(|i| i.get("productOrService"))
            .and_then(|p| p.get("coding")).and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("code")).and_then(Value::as_str);
        assert_eq!(ndc, Some("000000000"), "NDC preserved in raw");

        // Part A row: ICD-10 diagnosis preserved.
        let ipa = rows.iter().find(|r| str_field(r, "id") == "EOBInpatient1").unwrap();
        let icd = ipa
            .get("diagnosis").and_then(Value::as_array).and_then(|a| a.first())
            .and_then(|d| d.get("diagnosisCodeableConcept"))
            .and_then(|c| c.get("coding")).and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("code")).and_then(Value::as_str);
        assert_eq!(icd, Some("S06.0X1A"), "ICD-10 code preserved in raw");
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermarks.is_empty());
        assert!(empty.updated.is_none());

        let fwd: SyncState = serde_json::from_str(
            r#"{"watermarks":{"ExplanationOfBenefit":"2026-01-01T00:00:00Z"},"future_field":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            fwd.watermarks.get("ExplanationOfBenefit").map(String::as_str),
            Some("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn connection_def_has_oauth_method_with_correct_port() {
        assert_eq!(CONNECTION.id, SERVICE);
        assert!(CONNECTION.method("oauth").is_some(), "OAuth method registered");
        assert_eq!(BB2_PROVIDER.redirect_port, 38816, "38580 + 236 = 38816");
        assert!(BB2_PROVIDER.use_pkce, "PKCE required by BB2.0");
        assert!(BB2_PROVIDER.default_client_secret.is_none(), "public client — no secret");
    }
}
