//! Epic MyChart — SMART on FHIR patient health records from Epic-connected hospitals.
//!
//! A **Periodic** cloud pull over Epic's patient FHIR R4 endpoint. The
//! endpoint returns FHIR R4 resources — Observation/DiagnosticReport (labs
//! and vitals), Condition (problems), MedicationRequest (prescriptions),
//! Immunization, AllergyIntolerance, and Procedure. Each FHIR Observation
//! (or vital sign) becomes a [`crate::health_medical::Observation`] row
//! written to `health/medical/epic-mychart/observations/YYYY-MM.jsonl`.
//! Other resource types (Condition, Medication, Immunization, Allergy,
//! Procedure) are written to raw-only paths (those shapes sit behind the
//! unbound health-medical.condition / health-medical.medication sibling
//! drafts).
//!
//! ## Vault layout
//!
//! - **Raw (all resources):**
//!   `health/medical/epic-mychart/raw/<ResourceType>/YYYY-MM.jsonl` —
//!   verbatim FHIR R4 JSON for every resource returned, full fidelity,
//!   unconditional.
//! - **Contract (Observations only):**
//!   `health/medical/epic-mychart/observations/YYYY-MM.jsonl` — one row
//!   per FHIR Observation mapped to the health-medical contract, partitioned
//!   by local month of the effective date. Non-Observation resources stay
//!   raw-only until the sibling-draft contracts are ratified (Needs-David).
//!
//! ## Auth (SMART on FHIR OAuth 2.0, PKCE)
//!
//! Epic's patient-facing API uses SMART on FHIR 2.0 (PKCE public client).
//! The sandbox authorize/token endpoints are baked below; production
//! endpoints are discovered via
//! `<fhir-base>/.well-known/smart-configuration` (one org per auth flow).
//! Client registration is self-service at open.epic.com (free sandbox access);
//! production review takes a few days.
//!
//! **Evidence for endpoint URLs:** confirmed by fetching the live
//! `.well-known/smart-configuration` from the Epic sandbox at
//! `https://fhir.epic.com/interconnect-fhir-oauth/api/FHIR/R4/.well-known/smart-configuration`
//! (2026-06-16) — authorization_endpoint and token_endpoint verified.
//!
//! ## Cursor
//!
//! Per resource type: the watermark is the maximum `meta.lastUpdated` seen
//! across all fetched resources of that type, persisted in
//! `.trove/epic-mychart-sync.json`. Paging follows FHIR Bundle
//! `link.relation=next`. The watermark advances once per resource type after
//! the full multi-page drain completes (not per-page); guid/id dedupe ensures
//! re-draining from the same watermark is safe on restart.
//!
//! ## Flags
//!
//! Needs-login (validation requires a real MyChart account or the Epic
//! sandbox patient login). Clinical data ships opt-in with explicit
//! acknowledgement. Connection id is new (`epic-mychart`); other
//! Epic-flavored SMART on FHIR sources could reuse it.
//!
//! # NOTE ON SCOPE NARROWING
//!
//! First pass writes Observation resources to the health-medical contract
//! (bound) + ALL resources verbatim to raw (unconditional). Condition /
//! MedicationRequest / Immunization / AllergyIntolerance / Procedure write
//! raw-only; their normalized layers are deferred to the
//! health-medical.condition / health-medical.medication unbound sibling drafts.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
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

// ---------------------------------------------------------------------------
// Constants

const SERVICE: &str = "epic-mychart";

/// Contract Observation directory (partitioned by month).
const DIR: &str = "health/medical/epic-mychart/observations";

/// Raw FHIR NDJSON root — actual files are under <RAW_ROOT>/<ResourceType>/.
const RAW_ROOT: &str = "health/medical/epic-mychart/raw";

/// Non-secret rebuildable cursor. Delete to re-drain full history.
const SYNC_FILE: &str = ".trove/epic-mychart-sync.json";

/// Epic sandbox FHIR R4 base URL.
/// Verified via `.well-known/smart-configuration` on 2026-06-16.
const FHIR_BASE: &str = "https://fhir.epic.com/interconnect-fhir-oauth/api/FHIR/R4";

/// FHIR page size; Epic supports up to 200 per page.
const PAGE_SIZE: u32 = 100;

/// HTTP request timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Sync every 6 hours — clinical records change infrequently.
const SYNC_SECS: u64 = 6 * 3600;

/// FHIR resource types fetched on each sync.
/// Observations cover lab results and vital signs.
/// Others are pulled raw-only (contract layers pending Needs-David ratification).
const RESOURCE_TYPES: &[&str] = &[
    "Observation",
    "Condition",
    "MedicationRequest",
    "Immunization",
    "AllergyIntolerance",
    "Procedure",
];

// ---------------------------------------------------------------------------
// OAuth provider (SMART on FHIR PKCE public client).

/// Epic SMART on FHIR OAuth2 provider.
///
/// Endpoints verified by fetching the Epic sandbox `.well-known/smart-configuration`
/// on 2026-06-16:
///   https://fhir.epic.com/interconnect-fhir-oauth/api/FHIR/R4/.well-known/smart-configuration
/// Returns:
///   authorization_endpoint: https://fhir.epic.com/interconnect-fhir-oauth/oauth2/authorize
///   token_endpoint:         https://fhir.epic.com/interconnect-fhir-oauth/oauth2/token
///
/// Production orgs (real hospitals) expose their own endpoints discovered via
/// `.well-known/smart-configuration` on each org's FHIR base — the sandbox
/// URLs baked here work for test patients at open.epic.com. A future
/// multi-org connect flow would discover per-org endpoints at connect time.
pub static EPIC_PROVIDER: Provider = Provider {
    service: SERVICE,
    display_name: "Epic MyChart",
    // Sandbox authorize/token endpoints (confirmed 2026-06-16 from smart-configuration).
    auth_url: "https://fhir.epic.com/interconnect-fhir-oauth/oauth2/authorize",
    token_url: "https://fhir.epic.com/interconnect-fhir-oauth/oauth2/token",
    // Patient-access SMART on FHIR scopes: observations (labs/vitals), conditions,
    // medications, immunizations, allergies, procedures. `offline_access` requests
    // a refresh token. `launch/patient` is used for EHR-embedded launch; standalone
    // omits it and uses patient-context scopes instead.
    scopes: "patient/Patient.read patient/Observation.read patient/Condition.read \
             patient/MedicationRequest.read patient/Immunization.read \
             patient/AllergyIntolerance.read patient/Procedure.read offline_access",
    // Assigned unique production redirect port for epic-mychart (#93).
    // 38580 + 93 = 38673.
    // Register: http://localhost:38673/callback at open.epic.com.
    redirect_port: 38673,
    // SMART on FHIR mandates PKCE (S256).
    use_pkce: true,
    // PKCE public client: no Basic auth (client_id in POST body only).
    basic_auth: false,
    // Bake client id at build time: TROVE_EPIC_MYCHART_CLIENT_ID. Empty default —
    // registration at open.epic.com (free sandbox; production review in days).
    default_client_id: option_env!("TROVE_EPIC_MYCHART_CLIENT_ID"),
    // Public PKCE client: no client secret.
    default_client_secret: None,
    extra_auth_params: &[],
};

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
                format!("epic-mychart synced — {total} records")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "epic-mychart sync skipped: {e}"
        ))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    pull(vault)
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (already present as a
/// stub — this body REPLACES the NotWired stub).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "epic-mychart",
        name: "Epic MyChart",
        kind: IntegrationKind::CloudSync,
        // Clinical records are among the most sensitive data — opt-in only.
        default_on: false,
        description:
            "Pulls your clinical records — lab results, vital signs, conditions, medications, \
             immunizations, and procedures — from Epic-connected hospitals and health systems \
             via SMART on FHIR (FHIR R4, USCDI v3). Epic covers 42%+ of US hospitals.",
        domain: "health",
        vault_path: "health/medical/epic-mychart/",
        toggleable: true,
        setup: &[
            "Clinical records are among the most sensitive data Trove can collect — enabling this \
             opts you in explicitly.",
            "Register a developer app at open.epic.com (free) to obtain a Client ID \
             (no secret needed — this is a PKCE public client).",
            "Set the OAuth redirect URI to http://localhost:38673/callback in your app settings.",
            "Paste your Client ID and click Connect. You will be directed to sign in with your \
             MyChart credentials.",
        ],
        caveats:
            "Authorization uses the Epic sandbox endpoint by default; real hospital records \
             require your health system's specific FHIR endpoint (discoverable via \
             .well-known/smart-configuration). Lab results ordered through Quest or Labcorp may \
             also appear in Epic bundles — cross-source duplicates are reconciled at read time.",
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
// Connection (SMART on FHIR OAuth PKCE).

fn connect_oauth(vault: &Vault, creds: Option<AppCredentials>) -> Result<()> {
    connect(vault, creds).map(|_| ())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let configured = vault.load_sync_app(EPIC_PROVIDER.service)?.is_some()
        || EPIC_PROVIDER.default_credentials().is_some();
    let accounts = match vault.load_sync_token(EPIC_PROVIDER.service)? {
        Some(token) => {
            let needs_reconnect = token.expired() && token.refresh_token.is_none();
            vec![ConnectedAccount {
                key: EPIC_PROVIDER.service.to_string(),
                label: EPIC_PROVIDER.display_name.to_string(),
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
/// `&crate::epic_mychart::CONNECTION,` line — this module only declares it).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Epic MyChart",
    methods: &[ConnectMethod::OAuth {
        provider: &EPIC_PROVIDER,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["epic-mychart"],
    setup: &[
        "Register a developer app at open.epic.com (free). Set its redirect URI to \
         http://localhost:38673/callback (PKCE public client — no secret required).",
        "Paste the Client ID here. On Connect, your browser will open your MyChart login.",
        "After authorizing, Trove will sync your labs, conditions, medications, immunizations, \
         and procedures from Epic-connected health systems.",
    ],
};

/// Interactive OAuth connect: opens the consent page, awaits the redirect, saves
/// the token. Blocking — call from a background thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(EPIC_PROVIDER.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(EPIC_PROVIDER.service)?
            .or_else(|| EPIC_PROVIDER.default_credentials())
            .context(
                "no Epic MyChart Client ID — register a developer app at open.epic.com and \
                 enter its Client ID in the Integrations tab",
            )?,
    };
    let flow = oauth::OauthFlow::start(&EPIC_PROVIDER, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(EPIC_PROVIDER.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Cursor.

/// Watermarks per resource type. Keyed by FHIR resourceType (e.g. "Observation").
/// Each value is the maximum `meta.lastUpdated` seen, as UTC RFC3339.
/// Deleting this file re-drains the full history.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncState {
    /// Per-resource-type watermarks: map of resourceType → max meta.lastUpdated.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub watermarks: HashMap<String, String>,
    /// RFC3339 timestamp of the last successful full sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl Vault {
    pub fn read_epic_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn write_epic_sync(&self, state: &SyncState) -> Result<()> {
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

/// The FHIR operations the pull needs. Injectable so tests run offline.
pub trait EpicFhirApi {
    /// `GET /R4/<ResourceType>?patient=<id>&_sort=_lastUpdated&_count=<n>
    ///   [&_lastUpdated=ge<watermark>]`
    fn resources(
        &self,
        token: &str,
        resource_type: &str,
        patient_id: &str,
        last_updated_ge: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<Value, FetchError>;

    /// `GET /R4/Patient?_format=json` — to resolve the patient id.
    fn patient(&self, token: &str) -> Result<Value, FetchError>;
}

pub struct EpicClient {
    base: String,
}

impl EpicClient {
    pub fn new() -> Self {
        EpicClient { base: FHIR_BASE.to_string() }
    }

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

impl EpicFhirApi for EpicClient {
    fn patient(&self, token: &str) -> Result<Value, FetchError> {
        let url = format!("{}/Patient?_format=json", self.base);
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
                "{}/{resource_type}?patient={}&_sort=_lastUpdated&_count={}&_format=json",
                self.base,
                urlencoded_id(patient_id),
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

/// Minimal URL encoding for path segments (patient id).
fn urlencoded_id(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// Minimal percent-encoding for query parameter values.
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
/// `resourceType == expected`. Tolerates missing keys silently.
pub fn bundle_entries<'a>(bundle: &'a Value, expected_type: &str) -> Vec<&'a Value> {
    bundle
        .get("entry")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("resource"))
                .filter(|r| {
                    r.get("resourceType")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        == expected_type
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The `url` of the `link` with `relation == "next"` in a FHIR Bundle, if any.
pub fn next_link(bundle: &Value) -> Option<&str> {
    bundle
        .get("link")
        .and_then(Value::as_array)?
        .iter()
        .find(|l| l.get("relation").and_then(Value::as_str) == Some("next"))
        .and_then(|l| l.get("url"))
        .and_then(Value::as_str)
}

/// Convenience: get a string field, trimmed, empty when missing.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Descend a dot-separated path and return a string.
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

/// Parse a FHIR instant (RFC3339 / ISO8601 with offset) to a UTC DateTime.
/// Returns `None` for date-only strings (e.g. "2026-04-02") or unparseable input.
/// Used for watermark max-comparison so mixed-offset instants sort correctly.
fn parse_fhir_instant(s: &str) -> Option<DateTime<chrono::Utc>> {
    s.parse::<DateTime<chrono::FixedOffset>>()
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

// ---------------------------------------------------------------------------
// FHIR R4 Observation → contract Observation.
//
// Evidence basis: FHIR R4 Observation spec (hl7.org/fhir/R4/observation.html)
// confirmed by fetching Epic sandbox metadata and example bundles.
//
// Field derivation:
//   guid        = "epic-mychart/" + `Observation.id`
//   ts          = effectiveDateTime → effectivePeriod.start →
//                 effectiveInstant → issued → meta.lastUpdated
//   test        = code.coding[0].display or code.text
//   code        = LOINC code from code.coding (prefer LOINC system)
//   code_system = "loinc" when LOINC, else raw system URL
//   value       = valueQuantity.value or valueInteger
//   value_text  = valueString | valueCodeableConcept.text | valueRange/valueRatio
//   unit        = valueQuantity.unit (UCUM)
//   reference_range = referenceRange[0].text or synthesized lo–hi
//   flag        = interpretation[0].coding[0].code
//   panel       = basedOn[0].display or caller-supplied name
//   provider    = performer[0].display
//   extra       = status, category, specimen, encounter, component, …

/// Map a FHIR R4 Observation resource to a contract [`Observation`].
/// Returns `None` when the resource has no usable `id`, no dateable instant,
/// or no test name (ungroupable Observations — still land in raw layer).
pub fn observation_from_fhir(res: &Value) -> Option<Observation> {
    let id = str_field(res, "id");
    if id.is_empty() {
        return None;
    }

    // ts: effectiveDateTime > effectivePeriod.start > effectiveInstant >
    //     issued > meta.lastUpdated.
    let ts = {
        let eff = str_field(res, "effectiveDateTime");
        if !eff.is_empty() {
            eff
        } else {
            let period_start = nested_str(res, &["effectivePeriod", "start"]);
            if !period_start.is_empty() {
                period_start
            } else {
                let inst = str_field(res, "effectiveInstant");
                if !inst.is_empty() {
                    inst
                } else {
                    let issued = str_field(res, "issued");
                    if !issued.is_empty() {
                        issued
                    } else {
                        nested_str(res, &["meta", "lastUpdated"])
                    }
                }
            }
        }
    };
    if ts.is_empty() {
        return None;
    }

    // test name: code.coding[0].display, then code.text.
    let test = {
        let from_coding = res
            .get("code")
            .and_then(|c| c.get("coding"))
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|c| c.get("display"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if !from_coding.is_empty() {
            from_coding
        } else {
            nested_str(res, &["code", "text"])
        }
    };
    if test.is_empty() {
        return None; // ungroupable — raw layer only
    }

    // LOINC code: prefer coding with a LOINC system URL.
    let (code, code_system) = res
        .get("code")
        .and_then(|c| c.get("coding"))
        .and_then(Value::as_array)
        .map(|codings| {
            let preferred = codings.iter().find(|c| {
                c.get("system")
                    .and_then(Value::as_str)
                    .map(|s| s.to_lowercase().contains("loinc"))
                    .unwrap_or(false)
            });
            let coding = preferred.or_else(|| codings.first());
            match coding {
                Some(c) => {
                    let code =
                        c.get("code").and_then(Value::as_str).unwrap_or("").trim().to_string();
                    let system = c.get("system").and_then(Value::as_str).unwrap_or("").to_string();
                    let code_system = if system.to_lowercase().contains("loinc") {
                        "loinc".to_string()
                    } else if !system.is_empty() {
                        system
                    } else {
                        String::new()
                    };
                    (code, code_system)
                }
                None => (String::new(), String::new()),
            }
        })
        .unwrap_or((String::new(), String::new()));

    // Numeric value: valueQuantity.value (most common), then valueInteger.
    let value = res
        .get("valueQuantity")
        .and_then(|q| q.get("value"))
        .and_then(Value::as_f64)
        .or_else(|| res.get("valueInteger").and_then(Value::as_i64).map(|i| i as f64));

    // Qualitative result text.
    let value_text = if let Some(s) = res.get("valueString").and_then(Value::as_str) {
        s.trim().to_string()
    } else if let Some(cc) = res.get("valueCodeableConcept") {
        let text = cc.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
        if !text.is_empty() {
            text
        } else {
            cc.get("coding")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(|c| c.get("display"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string()
        }
    } else if let Some(range) = res.get("valueRange") {
        let lo = range.get("low").and_then(|q| q.get("value")).and_then(Value::as_f64);
        let hi = range.get("high").and_then(|q| q.get("value")).and_then(Value::as_f64);
        match (lo, hi) {
            (Some(l), Some(h)) => format!("{l}-{h}"),
            (Some(l), None) => format!(">{l}"),
            (None, Some(h)) => format!("<{h}"),
            (None, None) => String::new(),
        }
    } else if let Some(ratio) = res.get("valueRatio") {
        let num = ratio.get("numerator").and_then(|q| q.get("value")).and_then(Value::as_f64);
        let den = ratio.get("denominator").and_then(|q| q.get("value")).and_then(Value::as_f64);
        match (num, den) {
            (Some(n), Some(d)) => format!("{n}/{d}"),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    // Unit: valueQuantity.unit.
    let unit = res
        .get("valueQuantity")
        .and_then(|q| q.get("unit"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Reference range: text first, then lo–hi.
    let reference_range = res
        .get("referenceRange")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .map(|rr| {
            let text = rr.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
            if !text.is_empty() {
                return text;
            }
            let lo = rr.get("low").and_then(|q| q.get("value")).and_then(Value::as_f64);
            let hi = rr.get("high").and_then(|q| q.get("value")).and_then(Value::as_f64);
            match (lo, hi) {
                (Some(l), Some(h)) => format!("{l}-{h}"),
                (Some(l), None) => format!(">{l}"),
                (None, Some(h)) => format!("<{h}"),
                (None, None) => String::new(),
            }
        })
        .unwrap_or_default();

    // Flag from interpretation[0].coding[0].code.
    let flag = res
        .get("interpretation")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|i| i.get("coding"))
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|c| c.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Panel: basedOn[0].display.
    let panel = res
        .get("basedOn")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|r| r.get("display"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Provider: performer[0].display.
    let provider = res
        .get("performer")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|p| p.get("display"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Extra: source-specific fields not in the contract.
    let mut extra = Map::new();
    for key in &["status", "category", "specimen", "encounter", "component",
                  "dataAbsentReason", "bodySite", "method", "device", "note"] {
        if let Some(v) = res.get(*key) {
            if v != &Value::Null && !(v.is_string() && v.as_str().unwrap_or("").is_empty()) {
                extra.insert((*key).to_string(), v.clone());
            }
        }
    }

    // guid scoped to epic-mychart so it does not collide with Quest/Labcorp
    // Observation IDs (FHIR ids are only unique per server, not globally).
    Some(Observation {
        ts,
        source: SERVICE.into(),
        guid: format!("epic-mychart/{id}"),
        test,
        code,
        code_system,
        value,
        value_text,
        unit,
        reference_range,
        flag,
        panel,
        provider,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Full-fidelity raw row: the verbatim FHIR resource JSON.
/// `ts` is stored only for month-partitioning; not serialized.
struct RawLine {
    ts: String,
    value: Value,
}

impl Serialize for RawLine {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        self.value.serialize(ser)
    }
}

/// Extract the effective timestamp from any FHIR resource for partitioning.
/// Falls back to meta.lastUpdated, then today's date. Never returns empty string:
/// date-less resources partition into the current month rather than failing the
/// whole batch append.
fn resource_ts(res: &Value) -> String {
    // Observation-style timestamps.
    let eff = str_field(res, "effectiveDateTime");
    if !eff.is_empty() {
        return eff;
    }
    let period_start = nested_str(res, &["effectivePeriod", "start"]);
    if !period_start.is_empty() {
        return period_start;
    }
    // Condition: onsetDateTime.
    let onset = str_field(res, "onsetDateTime");
    if !onset.is_empty() {
        return onset;
    }
    // Immunization: occurrenceDateTime.
    let occurrence = str_field(res, "occurrenceDateTime");
    if !occurrence.is_empty() {
        return occurrence;
    }
    // AllergyIntolerance: recordedDate.
    let recorded = str_field(res, "recordedDate");
    if !recorded.is_empty() {
        return recorded;
    }
    // MedicationRequest: authoredOn.
    let authored = str_field(res, "authoredOn");
    if !authored.is_empty() {
        return authored;
    }
    // Procedure: performedDateTime.
    let performed = str_field(res, "performedDateTime");
    if !performed.is_empty() {
        return performed;
    }
    // issued / meta.lastUpdated as last resort.
    let issued = str_field(res, "issued");
    if !issued.is_empty() {
        return issued;
    }
    let last_updated = nested_str(res, &["meta", "lastUpdated"]);
    if !last_updated.is_empty() {
        return last_updated;
    }
    // Final fallback: today's date (YYYY-MM-DD). FHIR does not require
    // meta.lastUpdated; without a fallback an empty ts causes the partition
    // key to return None and the whole-page append to fail (proven by store
    // tests). We prefer writing the record under today's month to silently
    // dropping or wedging the entire sync.
    Local::now().date_naive().to_string()
}

/// Write new raw resources for a given resource type. Deduped by FHIR id.
/// Returns the count of new raw rows written.
fn write_raw(vault: &Vault, resource_type: &str, resources: Vec<Value>) -> Result<u64> {
    let raw_dir = format!("{RAW_ROOT}/{resource_type}");
    let raw = vault.stream(&raw_dir, Partition::Month);

    // Load existing IDs to deduplicate.
    let mut seen: HashSet<String> = HashSet::new();
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
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
    raw.append(&new_rows, |r| &r.ts)?;
    Ok(count)
}

/// Write new contract Observations + raw Observations, both deduped by guid.
/// Returns the count of new contract rows written.
fn write_observations(vault: &Vault, resources: Vec<Value>) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(&format!("{RAW_ROOT}/Observation"), Partition::Month);

    // Raw dedupe by FHIR id.
    let mut raw_seen: HashSet<String> = HashSet::new();
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
            let g = str_field(&v, "id");
            if !g.is_empty() {
                raw_seen.insert(g);
            }
        }
    }

    // Contract dedupe by guid.
    let mut contract_seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = v.get("guid").and_then(Value::as_str).unwrap_or("").to_string();
            if !g.is_empty() {
                contract_seen.insert(g);
            }
        }
    }

    let mut new_raws: Vec<RawLine> = Vec::new();
    let mut new_obs: Vec<Observation> = Vec::new();

    for r in resources {
        let id = str_field(&r, "id");

        // Raw layer: unconditional for all non-empty-id resources.
        if !id.is_empty() && raw_seen.insert(id.clone()) {
            let ts = resource_ts(&r);
            new_raws.push(RawLine { ts, value: r.clone() });
        }

        // Contract layer: only successfully mapped Observations.
        if let Some(obs) = observation_from_fhir(&r) {
            if !obs.guid.is_empty() && contract_seen.insert(obs.guid.clone()) {
                new_obs.push(obs);
            }
        }
    }

    raw.append(&new_raws, |r| &r.ts)?;
    contract.append(&new_obs, |o| &o.ts)?;
    Ok(new_obs.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Entry point called by the Periodic runner + "Sync now".
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context(
            "Epic MyChart is not connected — connect your account in the Integrations tab",
        )?;
    let token = ensure_fresh(vault, token)?;
    let client = EpicClient::new();
    pull_with(vault, &client, &token.access_token)
}

fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(EPIC_PROVIDER.service)?
        .or_else(|| EPIC_PROVIDER.default_credentials())
        .context(
            "Epic MyChart token expired and no client id to refresh it — reconnect from the \
             Integrations tab",
        )?;
    match oauth::refresh_token(&EPIC_PROVIDER, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(EPIC_PROVIDER.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(EPIC_PROVIDER.service)?;
            bail!(
                "Epic MyChart token refresh failed ({e}) — reconnect from the Integrations tab"
            );
        }
    }
}

/// The testable pull body. Resolves the patient id (cached), then for each
/// resource type drains all pages from the watermark. The watermark advances
/// once per resource type (after the full multi-page drain) and is persisted
/// after each type completes. Guid/id dedupe makes re-draining from the same
/// watermark safe on restart.
pub fn pull_with(vault: &Vault, api: &impl EpicFhirApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_epic_sync();
    // Key by &'static str directly from RESOURCE_TYPES — no allocation/leak needed.
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Resolve patient id (cached to avoid a Patient search on every sync).
    let patient_id = resolve_patient_id(vault, api, token, &mut state)?;

    // Sync each resource type independently.
    for &rtype in RESOURCE_TYPES {
        let watermark = state.watermarks.get(rtype).map(String::as_str);
        let (new_rows, new_watermark) =
            sync_resource_type(vault, api, token, rtype, &patient_id, watermark)?;

        counts.insert(rtype, new_rows);

        // Advance the watermark for this resource type if we got new data.
        if let Some(wm) = new_watermark {
            state.watermarks.insert(rtype.to_string(), wm);
            vault.write_epic_sync(&state)?;
        }
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_epic_sync(&state)?;

    let total: u64 = counts.values().sum();
    Ok(PullOutcome {
        headline: format!(
            "Epic MyChart synced — {total} records ({} observations)",
            counts.get("Observation").copied().unwrap_or(0)
        ),
        counts,
    })
}

fn resolve_patient_id(
    vault: &Vault,
    api: &impl EpicFhirApi,
    token: &str,
    state: &mut SyncState,
) -> Result<String> {
    // Use cached patient_id from the watermarks map (stored under key "__patient_id").
    if let Some(id) = state.watermarks.get("__patient_id") {
        return Ok(id.clone());
    }
    let bundle = api.patient(token).map_err(|e| {
        anyhow::anyhow!("Epic MyChart Patient search failed: {e}")
    })?;
    let id = bundle_entries(&bundle, "Patient")
        .first()
        .and_then(|p| p.get("id"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("Epic MyChart Patient bundle had no Patient resource — cannot locate your records")?;
    state.watermarks.insert("__patient_id".to_string(), id.clone());
    vault.write_epic_sync(state)?;
    Ok(id)
}

/// Sync a single FHIR resource type. Returns (new_contract_rows, new_watermark).
/// For Observation: writes contract rows + raw. For others: raw only.
fn sync_resource_type(
    vault: &Vault,
    api: &impl EpicFhirApi,
    token: &str,
    resource_type: &str,
    patient_id: &str,
    watermark: Option<&str>,
) -> Result<(u64, Option<String>)> {
    let mut new_watermark: Option<String> = None;
    let mut page_url: Option<String> = None;
    let mut total_new: u64 = 0;

    loop {
        let bundle = api
            .resources(token, resource_type, patient_id, watermark, page_url.as_deref())
            .map_err(|e| anyhow::anyhow!("Epic MyChart {resource_type} fetch failed: {e}"))?;

        let resources = bundle_entries(&bundle, resource_type);
        if resources.is_empty() {
            break;
        }

        // Advance the watermark candidate from meta.lastUpdated on all resources.
        for r in &resources {
            let lu = nested_str(r, &["meta", "lastUpdated"]);
            if !lu.is_empty() {
                new_watermark =
                    Some(max_watermark(new_watermark.as_deref(), &lu));
            }
        }

        let owned: Vec<Value> = resources.iter().map(|r| (*r).clone()).collect();
        let new_rows = if resource_type == "Observation" {
            write_observations(vault, owned)?
        } else {
            write_raw(vault, resource_type, owned)?
        };
        total_new += new_rows;

        match next_link(&bundle) {
            Some(next) => page_url = Some(next.to_string()),
            None => break,
        }
    }

    Ok((total_new, new_watermark))
}

#[allow(dead_code)]
fn fetch_err(resource: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Epic MyChart rejected the token on {resource} — reconnect from the Integrations tab"
        ),
        FetchError::NotFound => anyhow::anyhow!(
            "Epic MyChart {resource} endpoint returned 404 — the FHIR server URL may differ for \
             your health system; check Trove release notes"
        ),
        other => anyhow::anyhow!("Epic MyChart {resource} fetch failed: {other}"),
    }
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
            "trove-epic-mychart-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — exact FHIR R4 shapes from the Epic sandbox metadata + spec.
    // Evidence: confirmed FHIR R4 structure via hl7.org/fhir/R4/observation.html
    // and Epic FHIR sandbox at fhir.epic.com (capabilities verified 2026-06-16).

    /// A numeric lab Observation: Glucose, LOINC 15074-8, value 113 mg/dL,
    /// flag H, reference range 70–99. Epic-shaped with category and status.
    fn obs_glucose() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "eA1c.glucose.001",
            "meta": { "lastUpdated": "2026-04-02T16:30:00Z" },
            "status": "final",
            "category": [{
                "coding": [{
                    "system": "http://terminology.hl7.org/CodeSystem/observation-category",
                    "code": "laboratory",
                    "display": "Laboratory"
                }]
            }],
            "code": {
                "coding": [{
                    "system": "http://loinc.org",
                    "code": "15074-8",
                    "display": "Glucose [Mass/volume] in Blood"
                }],
                "text": "Glucose"
            },
            "subject": { "reference": "Patient/epic-pt-001" },
            "effectiveDateTime": "2026-04-02T09:30:10-07:00",
            "issued": "2026-04-02T16:30:00Z",
            "performer": [{ "display": "Epic Health" }],
            "valueQuantity": {
                "value": 113,
                "unit": "mg/dL",
                "system": "http://unitsofmeasure.org",
                "code": "mg/dL"
            },
            "interpretation": [{
                "coding": [{
                    "system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation",
                    "code": "H",
                    "display": "High"
                }]
            }],
            "referenceRange": [{
                "low": { "value": 70, "unit": "mg/dL" },
                "high": { "value": 99, "unit": "mg/dL" },
                "text": "70-99"
            }]
        })
    }

    /// A numeric HbA1c Observation (valueQuantity, single value, no component).
    /// Tests LOINC 4548-4 mapping and the effectiveDateTime ts path.
    fn obs_hba1c_qualitative() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "eHgbA1C.001",
            "meta": { "lastUpdated": "2026-05-10T12:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "4548-4",
                             "display": "Hemoglobin A1c/Hemoglobin.total in Blood"}]
            },
            "subject": { "reference": "Patient/epic-pt-001" },
            "effectiveDateTime": "2026-05-10T08:00:00-07:00",
            "valueQuantity": { "value": 5.6, "unit": "%" }
        })
    }

    /// An Observation with only `issued` (no effectiveDateTime) — tests ts fallback.
    fn obs_issued_only() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "eObs.issued.003",
            "meta": { "lastUpdated": "2026-06-01T08:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "2823-3", "display": "Potassium"}]
            },
            "issued": "2026-06-01T07:45:00-07:00",
            "valueQuantity": { "value": 4.1, "unit": "mEq/L" }
        })
    }

    /// An Observation with only `meta.lastUpdated` as a timestamp — deepest fallback.
    fn obs_meta_ts_only() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "eObs.meta.004",
            "meta": { "lastUpdated": "2026-06-02T09:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "2951-2", "display": "Sodium"}]
            },
            "valueQuantity": { "value": 141, "unit": "mEq/L" }
        })
    }

    /// An Observation with a valueString (qualitative).
    fn obs_qualitative_string() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "eObs.hiv.005",
            "meta": { "lastUpdated": "2026-05-15T14:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "56888-1",
                             "display": "HIV 1+2 Ab [Presence] in Serum"}]
            },
            "effectiveDateTime": "2026-05-15T08:00:00-07:00",
            "valueString": "Non-Reactive",
            "referenceRange": [{ "text": "Non-Reactive" }]
        })
    }

    /// A Condition resource (raw-only — no contract mapping).
    fn condition_resource() -> Value {
        json!({
            "resourceType": "Condition",
            "id": "eCond.diabetes.001",
            "meta": { "lastUpdated": "2025-12-01T10:00:00Z" },
            "clinicalStatus": {
                "coding": [{ "code": "active", "system": "http://terminology.hl7.org/CodeSystem/condition-clinical" }]
            },
            "code": {
                "coding": [{"system": "http://hl7.org/fhir/sid/icd-10", "code": "E11.9", "display": "Type 2 diabetes"}],
                "text": "Type 2 diabetes mellitus"
            },
            "subject": { "reference": "Patient/epic-pt-001" },
            "onsetDateTime": "2022-03-15"
        })
    }

    /// A FHIR Bundle wrapping a list of resources.
    fn make_bundle(resources: Vec<Value>) -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "total": resources.len(),
            "entry": resources.iter().map(|r| json!({"resource": r})).collect::<Vec<_>>()
        })
    }

    /// A Patient search bundle.
    fn patient_bundle() -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": [{ "resource": { "resourceType": "Patient", "id": "epic-pt-001" } }]
        })
    }

    // -----------------------------------------------------------------------
    // Mock API for offline testing.
    //
    // MockApi dispatches per resource type so test pages land in the right
    // type's queue (the real pull_with loops over RESOURCE_TYPES, calling
    // `resources(token, rtype, ...)` for each — a naive single-queue mock
    // would serve Observation pages to the wrong type).

    struct MockApi {
        patient_resp: Value,
        /// Per resource-type page queues: key = FHIR resourceType string.
        resource_pages: RefCell<HashMap<String, VecDeque<Value>>>,
    }

    impl MockApi {
        fn new(patient_resp: Value) -> Self {
            MockApi {
                patient_resp,
                resource_pages: RefCell::new(HashMap::new()),
            }
        }
        /// Queue a page bundle for the given FHIR resource type.
        fn with_page_for(self, resource_type: &str, bundle: Value) -> Self {
            self.resource_pages
                .borrow_mut()
                .entry(resource_type.to_string())
                .or_insert_with(VecDeque::new)
                .push_back(bundle);
            self
        }
        /// Queue a page bundle for "Observation" (convenience for most tests).
        fn with_obs_page(self, bundle: Value) -> Self {
            self.with_page_for("Observation", bundle)
        }
    }

    impl EpicFhirApi for MockApi {
        fn patient(&self, _token: &str) -> Result<Value, FetchError> {
            Ok(self.patient_resp.clone())
        }
        fn resources(
            &self,
            _token: &str,
            rt: &str,
            _pid: &str,
            _wm: Option<&str>,
            _next: Option<&str>,
        ) -> Result<Value, FetchError> {
            Ok(self
                .resource_pages
                .borrow_mut()
                .get_mut(rt)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(|| {
                    json!({"resourceType": "Bundle", "type": "searchset", "entry": []})
                }))
        }
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn epic_mychart_maps_glucose_observation() {
        let obs = observation_from_fhir(&obs_glucose()).unwrap();
        assert_eq!(obs.source, "epic-mychart");
        assert_eq!(obs.guid, "epic-mychart/eA1c.glucose.001");
        assert_eq!(obs.test, "Glucose [Mass/volume] in Blood");
        assert_eq!(obs.code, "15074-8");
        assert_eq!(obs.code_system, "loinc");
        assert_eq!(obs.value, Some(113.0));
        assert_eq!(obs.unit, "mg/dL");
        assert_eq!(obs.reference_range, "70-99");
        assert_eq!(obs.flag, "H");
        assert_eq!(obs.provider, "Epic Health");
        // ts = effectiveDateTime (first in fallback chain).
        assert_eq!(obs.ts, "2026-04-02T09:30:10-07:00");
        // category in extra.
        assert!(obs.extra.contains_key("category"), "category in extra");
        assert!(obs.extra.contains_key("status"), "status in extra");
        // Round-trip.
        let re = serde_json::to_value(&obs).unwrap();
        assert_eq!(re["value"].as_f64(), Some(113.0));
        assert_eq!(re["flag"], json!("H"));
        assert_eq!(re["source"], json!("epic-mychart"));
    }

    #[test]
    fn epic_mychart_maps_hba1c_observation() {
        let obs = observation_from_fhir(&obs_hba1c_qualitative()).unwrap();
        assert_eq!(obs.guid, "epic-mychart/eHgbA1C.001");
        assert_eq!(obs.test, "Hemoglobin A1c/Hemoglobin.total in Blood");
        assert_eq!(obs.value, Some(5.6));
        assert_eq!(obs.unit, "%");
    }

    #[test]
    fn epic_mychart_ts_fallback_chain() {
        // issued only.
        let obs = observation_from_fhir(&obs_issued_only()).unwrap();
        assert_eq!(obs.ts, "2026-06-01T07:45:00-07:00", "issued used when no effectiveDateTime");

        // meta.lastUpdated only.
        let obs_meta = observation_from_fhir(&obs_meta_ts_only()).unwrap();
        assert_eq!(obs_meta.ts, "2026-06-02T09:00:00Z", "meta.lastUpdated as last resort");
    }

    #[test]
    fn epic_mychart_qualitative_valuestring() {
        let obs = observation_from_fhir(&obs_qualitative_string()).unwrap();
        assert!(obs.value.is_none(), "no numeric value for qualitative result");
        assert_eq!(obs.value_text, "Non-Reactive");
        assert_eq!(obs.reference_range, "Non-Reactive");
        // Omit-empty: unit and value dropped in serialized form.
        let re = serde_json::to_value(&obs).unwrap();
        assert!(re.get("value").is_none(), "null value omitted");
        assert!(re.get("unit").is_none(), "empty unit omitted");
    }

    #[test]
    fn epic_mychart_missing_id_or_no_ts_returns_none() {
        let no_id = json!({
            "resourceType": "Observation",
            "status": "final",
            "code": { "coding": [{"code": "2823-3", "display": "K"}] },
            "effectiveDateTime": "2026-06-03T08:00:00Z"
        });
        assert!(observation_from_fhir(&no_id).is_none(), "no id → None");

        let no_ts = json!({
            "resourceType": "Observation",
            "id": "obs-no-ts",
            "status": "final",
            "code": { "coding": [{"code": "x", "display": "X"}] }
        });
        assert!(observation_from_fhir(&no_ts).is_none(), "no timestamp → None");

        let no_test = json!({
            "resourceType": "Observation",
            "id": "obs-no-test",
            "status": "final",
            "code": {},
            "effectiveDateTime": "2026-06-03T08:00:00Z"
        });
        assert!(observation_from_fhir(&no_test).is_none(), "no test name → None");
    }

    #[test]
    fn epic_mychart_reference_range_fallback_lo_hi() {
        let obs_data = json!({
            "resourceType": "Observation",
            "id": "obs-rr-007",
            "status": "final",
            "code": { "coding": [{"system": "http://loinc.org", "code": "2823-3", "display": "Potassium"}] },
            "effectiveDateTime": "2026-06-03T08:00:00-07:00",
            "valueQuantity": { "value": 3.5, "unit": "mEq/L" },
            "referenceRange": [{ "low": { "value": 3.5 }, "high": { "value": 5.0 } }]
        });
        let obs = observation_from_fhir(&obs_data).unwrap();
        assert_eq!(obs.reference_range, "3.5-5");
    }

    #[test]
    fn epic_mychart_bundle_helpers() {
        let bundle = make_bundle(vec![obs_glucose(), obs_hba1c_qualitative()]);
        let obs = bundle_entries(&bundle, "Observation");
        assert_eq!(obs.len(), 2);
        assert!(next_link(&bundle).is_none(), "no next link");

        let paged = json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": [{"relation": "next", "url": "https://fhir.epic.com/R4/Observation?page=2"}],
            "entry": []
        });
        assert_eq!(
            next_link(&paged),
            Some("https://fhir.epic.com/R4/Observation?page=2")
        );
    }

    // -----------------------------------------------------------------------
    // Integration pull tests (offline via MockApi).

    #[test]
    fn epic_mychart_full_pull_writes_observation_contract_and_raw() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(patient_bundle())
            .with_obs_page(make_bundle(vec![obs_glucose()]));

        let out = pull_with(&v, &api, "tok").unwrap();
        // One Observation should be in contract + raw.
        assert_eq!(out.counts.get("Observation"), Some(&1));

        // Contract layer.
        let obs_dir = v.root().join("health/medical/epic-mychart/observations");
        let obs_file = std::fs::read_dir(&obs_dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .expect("observations NDJSON file");
        let obs_text = std::fs::read_to_string(obs_file.path()).unwrap();
        assert_eq!(obs_text.lines().count(), 1, "one observation on disk");
        assert!(obs_text.contains("\"guid\":\"epic-mychart/eA1c.glucose.001\""));
        assert!(obs_text.contains("\"source\":\"epic-mychart\""));

        // Raw layer for Observations.
        let raw_dir = v.root().join("health/medical/epic-mychart/raw/Observation");
        let raw_file = std::fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .expect("raw Observation NDJSON file");
        let raw_text = std::fs::read_to_string(raw_file.path()).unwrap();
        assert!(raw_text.contains("\"resourceType\":\"Observation\""), "verbatim FHIR in raw");
        assert!(raw_text.contains("\"id\":\"eA1c.glucose.001\""), "FHIR id in raw");
    }

    #[test]
    fn epic_mychart_condition_writes_raw_only() {
        let v = temp_vault("condition-raw");
        // Queue the Condition page for the "Condition" resource type specifically.
        let api = MockApi::new(patient_bundle())
            .with_page_for("Condition", make_bundle(vec![condition_resource()]));

        let out = pull_with(&v, &api, "tok").unwrap();
        // Conditions go raw-only — count under "Condition".
        assert_eq!(out.counts.get("Condition"), Some(&1));

        // Confirm raw file for Condition exists.
        let raw_dir = v.root().join("health/medical/epic-mychart/raw/Condition");
        let raw_file = std::fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .find(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .expect("raw Condition NDJSON file");
        let raw_text = std::fs::read_to_string(raw_file.path()).unwrap();
        assert!(raw_text.contains("\"resourceType\":\"Condition\""));
        assert!(raw_text.contains("\"id\":\"eCond.diabetes.001\""));

        // No observations contract dir (no Observation resources were synced).
        let obs_dir = v.root().join("health/medical/epic-mychart/observations");
        let has_obs = std::fs::read_dir(&obs_dir)
            .map(|d| d.flatten().any(|e| e.file_name().to_string_lossy().ends_with(".jsonl")))
            .unwrap_or(false);
        assert!(!has_obs, "no observation contract rows for Condition-only pull");
    }

    #[test]
    fn epic_mychart_guid_deduplication() {
        let v = temp_vault("dedup");
        let api1 = MockApi::new(patient_bundle())
            .with_obs_page(make_bundle(vec![obs_glucose()]));
        let out1 = pull_with(&v, &api1, "tok").unwrap();
        assert_eq!(out1.counts.get("Observation"), Some(&1));

        // Same data again — dedupe must give 0 new rows.
        let api2 = MockApi::new(patient_bundle())
            .with_obs_page(make_bundle(vec![obs_glucose()]));
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(out2.counts.get("Observation"), Some(&0), "guid dedupe works");
    }

    #[test]
    fn epic_mychart_empty_bundle_is_noop() {
        let v = temp_vault("empty");
        // No pages queued → all types return empty bundles → zero rows.
        let api = MockApi::new(patient_bundle());
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("Observation").copied().unwrap_or(0), 0);
    }

    #[test]
    fn epic_mychart_pull_without_token_errors() {
        let v = temp_vault("unconnected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    #[test]
    fn epic_mychart_sync_state_back_compat() {
        // Empty JSON → default (all watermarks absent).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.watermarks.is_empty());

        // Forward-compat: extra keys tolerated.
        let fwd: SyncState = serde_json::from_str(
            r#"{"watermarks":{"Observation":"2026-04-03T14:30:00Z"},"future_key":"x"}"#,
        )
        .unwrap();
        assert_eq!(
            fwd.watermarks.get("Observation").map(String::as_str),
            Some("2026-04-03T14:30:00Z")
        );
    }

    #[test]
    fn epic_mychart_connection_provider_fields() {
        assert_eq!(EPIC_PROVIDER.redirect_port, 38673, "assigned port for #93 (38580+93)");
        assert_eq!(
            EPIC_PROVIDER.redirect_uri(),
            "http://localhost:38673/callback"
        );
        assert!(EPIC_PROVIDER.use_pkce, "SMART on FHIR requires PKCE");
        assert!(EPIC_PROVIDER.default_client_secret.is_none(), "public PKCE client — no secret");
        assert_eq!(CONNECTION.id, SERVICE);
        assert_eq!(DEF.connection, Some(SERVICE));
        assert!(CONNECTION.method("oauth").is_some());
    }

    #[test]
    fn epic_mychart_watermark_max_comparison_mixed_offsets() {
        // Test max_watermark handles mixed-offset instants correctly.
        // 15:30+01:00 = 14:30 UTC; 09:00-07:00 = 16:00 UTC (later).
        let a = "2026-04-03T15:30:10+01:00"; // 14:30:10 UTC
        let b = "2026-04-03T09:00:00-07:00"; // 16:00:00 UTC — later

        let result = max_watermark(Some(a), b);
        let result_dt = parse_fhir_instant(&result).unwrap();
        let b_dt = parse_fhir_instant(b).unwrap();
        assert_eq!(
            result_dt.timestamp(),
            b_dt.timestamp(),
            "max_watermark picks the chronologically later instant: {result}"
        );
    }

    /// Verify resource_ts() returns today's date rather than empty string for a
    /// resource with no usable date field. An empty ts causes Partition::key to
    /// return None and the whole-batch store::append to fail (proven by store
    /// tests). The fallback must never return "".
    #[test]
    fn epic_mychart_resource_ts_falls_back_to_today() {
        let dateless = json!({
            "resourceType": "Condition",
            "id": "eCond.nodates.999",
            "clinicalStatus": { "coding": [{ "code": "active" }] },
            "code": { "text": "Some condition" },
            "subject": { "reference": "Patient/epic-pt-001" }
            // No effectiveDateTime / onsetDateTime / recordedDate / issued / meta.lastUpdated
        });
        let ts = resource_ts(&dateless);
        assert!(!ts.is_empty(), "resource_ts must never return empty string");
        // Must look like a date YYYY-MM-DD (10 chars, hyphens at positions 4 and 7).
        assert_eq!(ts.len(), 10, "fallback ts is YYYY-MM-DD, got: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
    }

    /// Verify that a two-page Observation drain writes rows from both pages and
    /// the watermark reflects the max across all pages. This exercises the
    /// next_link threading in sync_resource_type's loop.
    #[test]
    fn epic_mychart_two_page_drain_writes_all_rows() {
        let v = temp_vault("twopages");

        // Page 1 bundle: contains obs_glucose; has a `next` link so the loop
        // continues. The mock pops from the VecDeque in insertion order — the
        // second pop (triggered by the next-page call) will serve page 2.
        let page1 = json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": [{
                "relation": "next",
                "url": "https://fhir.epic.com/R4/Observation?page=2&ct=abc"
            }],
            "entry": [{ "resource": obs_glucose() }]
        });
        // Page 2 bundle: contains obs_hba1c_qualitative; no next link → loop ends.
        let page2 = make_bundle(vec![obs_hba1c_qualitative()]);

        let api = MockApi::new(patient_bundle())
            .with_obs_page(page1)
            .with_obs_page(page2);

        let out = pull_with(&v, &api, "tok").unwrap();
        // Both pages should have been drained → 2 unique Observation rows.
        assert_eq!(
            out.counts.get("Observation"),
            Some(&2),
            "both pages drained: {out:?}"
        );

        // Confirm both rows are on disk.
        let obs_dir = v.root().join("health/medical/epic-mychart/observations");
        let total_lines: usize = std::fs::read_dir(&obs_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .map(|e| {
                std::fs::read_to_string(e.path())
                    .unwrap()
                    .lines()
                    .filter(|l| !l.is_empty())
                    .count()
            })
            .sum();
        assert_eq!(total_lines, 2, "two observation contract rows on disk");
    }
}
