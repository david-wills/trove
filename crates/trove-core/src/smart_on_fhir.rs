//! Generic SMART on FHIR patient-access client covering Cerner/Oracle Health,
//! Meditech, Allscripts, athenahealth, and any other USCDI-v3-compliant EHR
//! that is not already handled by the specific Epic/Quest/Labcorp modules.
//!
//! Brief: docs/integrations/smart-on-fhir.md.
//!
//! A **Periodic** cloud pull (every 6 hours) over a user-configured FHIR R4
//! base URL. The user provides a `<FHIR-base-url>|<bearer-token>` composite
//! string from their patient portal's "Developer / API access" section.  Full
//! multi-org PKCE discovery via `.well-known/smart-configuration` is deferred
//! (Needs-David — requires a runtime-configurable OAuth flow that the current
//! static `Provider` architecture does not yet support); the composite-token
//! path covers the immediate need for any FHIR R4 provider.
//!
//! ## Vault layout
//!
//! - **Raw (all resources):**
//!   `health/medical/smart-on-fhir/raw/<ResourceType>/YYYY-MM.jsonl` —
//!   verbatim FHIR R4 JSON for every resource returned, full fidelity,
//!   unconditional.
//! - **Contract (Observations only):**
//!   `health/medical/smart-on-fhir/observations/YYYY-MM.jsonl` — one row per
//!   FHIR Observation mapped to the health-medical Observation contract,
//!   partitioned by local month of the effective date.  Non-Observation
//!   resources stay raw-only until the sibling-draft
//!   health-medical.condition / health-medical.medication contracts are
//!   ratified.
//!
//! ## Auth (composite token paste)
//!
//! The user pastes a pipe-separated string:
//!   `https://fhir.example.com/R4|ey...bearer...`
//! The FHIR base URL is stored in the non-secret config file
//! `.trove/smart-on-fhir-config.json`; the bearer token rides in the
//! `access_token` slot of the 0600 token file
//! `.trove/sync/smart-on-fhir.json`.  When the bearer token expires the user
//! reconnects.  Full per-org PKCE discovery is a Needs-David enhancement.
//!
//! ## FHIR R4 field evidence
//!
//! Observation mapping is built against the FHIR R4 Observation specification
//! (hl7.org/fhir/R4/observation.html); field names and cardinalities are
//! standard R4 — they apply to every USCDI-v3 compliant server (Epic,
//! Cerner, Quest, Labcorp, Meditech, Allscripts, athenahealth), confirmed
//! by the Epic sandbox fixtures in `epic_mychart.rs`.
//!
//! ## Cursor
//!
//! Per resource type: max `meta.lastUpdated` across all fetched resources,
//! persisted in `.trove/smart-on-fhir-sync.json` (non-secret, rebuildable).
//! The watermark advances per resource type after the full multi-page drain.
//! Paging follows FHIR Bundle `link.relation=next`.
//!
//! ## Privacy
//!
//! Clinical data is among the most sensitive data in the vault.  This
//! integration ships opt-in (`default_on: false`, `toggleable: true`) with an
//! explicit acknowledgement step in the setup copy.

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
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

const SERVICE: &str = "smart-on-fhir";

/// Contract Observation directory (partitioned by month).
const DIR: &str = "health/medical/smart-on-fhir/observations";

/// Raw FHIR NDJSON root — files are under <RAW_ROOT>/<ResourceType>/
const RAW_ROOT: &str = "health/medical/smart-on-fhir/raw";

/// Non-secret rebuildable cursor.  Delete to re-drain full history.
const SYNC_FILE: &str = ".trove/smart-on-fhir-sync.json";

/// Non-secret config storing the FHIR base URL.  Separate from the cursor so
/// the URL is easy to see and edit without regenerating watermarks.
const CONFIG_FILE: &str = ".trove/smart-on-fhir-config.json";

/// FHIR resource types polled on each sync.
const RESOURCE_TYPES: &[&str] = &[
    "Observation",
    "Condition",
    "MedicationRequest",
    "Immunization",
    "AllergyIntolerance",
    "Procedure",
];

/// Page size for FHIR search bundles.
const PAGE_SIZE: u32 = 100;

/// HTTP request timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Sync every 6 hours — clinical records change infrequently.
const SYNC_SECS: u64 = 6 * 3600;

/// Separator between FHIR base URL and bearer token in the composite paste
/// value.  A pipe character is not legal in a URL path unencoded.
const COMPOSITE_SEP: char = '|';

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
                format!("smart-on-fhir synced — {total} records")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "smart-on-fhir sync skipped: {e}"
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
        id: SERVICE,
        name: "Medical Records (SMART on FHIR)",
        kind: IntegrationKind::CloudSync,
        // Clinical records are the most sensitive data in the vault — opt-in
        // only, with explicit acknowledgement.
        default_on: false,
        description:
            "Pulls clinical records — lab results, vital signs, conditions, medications, \
             immunizations, and procedures — from any FHIR R4 / USCDI-v3 health system \
             not already covered by the dedicated Epic, Quest, or Labcorp integrations. \
             Covers Cerner/Oracle Health, Meditech, Allscripts, athenahealth, and ~40 \
             other providers.",
        domain: "health",
        vault_path: "health/medical/smart-on-fhir/",
        toggleable: true,
        setup: &[
            "Clinical records are among the most sensitive data Trove can collect — \
             enabling this opts you in explicitly.",
            "From your patient portal's developer or API-access section, copy your FHIR R4 \
             base URL and an access token.",
            "Paste them here as a single string separated by a pipe character: \
             https://fhir.example.com/R4|ey...token",
            "First sync pulls all available resources; later syncs are incremental.",
        ],
        caveats:
            "Requires your health portal to expose a FHIR R4 patient-access endpoint with \
             a bearer token you can copy.  Full SMART on FHIR PKCE (automatic login) for \
             per-org endpoint discovery is a planned enhancement.  For Epic MyChart, Quest \
             Diagnostics, or Labcorp, use the dedicated integrations instead.",
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
// Connection (composite TokenPaste: FHIR base URL + bearer token).

/// Parse a composite paste string into `(fhir_base, bearer_token)`.
/// Trims both parts; rejects empty base or empty token.
fn parse_composite(raw: &str) -> Result<(String, String)> {
    let raw = raw.trim();
    let pos = raw.find(COMPOSITE_SEP).context(
        "Expected format: https://fhir.example.com/R4|ey...token  \
         (FHIR base URL, a pipe `|`, then the bearer token)",
    )?;
    let base = raw[..pos].trim().to_string();
    let token = raw[pos + COMPOSITE_SEP.len_utf8()..].trim().to_string();
    if base.is_empty() {
        bail!("FHIR base URL cannot be empty");
    }
    if token.is_empty() {
        bail!("bearer token cannot be empty");
    }
    Ok((base, token))
}

/// Non-secret config: the FHIR base URL.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct FhirConfig {
    /// FHIR R4 base URL (e.g. `https://fhir.example.com/R4`), without
    /// trailing slash.  Default is an empty string (unconfigured).
    #[serde(default)]
    pub fhir_base: String,
}

impl Vault {
    fn read_fhir_config(&self) -> FhirConfig {
        self.resolve(CONFIG_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_fhir_config(&self, cfg: &FhirConfig) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(CONFIG_FILE)?, cfg)
    }
}

/// Store the composite paste: FHIR base URL in the non-secret config, bearer
/// token 0600 in the sync token store.
fn def_connect(vault: &Vault, raw: &str) -> Result<()> {
    let (base, bearer) = parse_composite(raw)?;
    // Normalize the base URL: strip trailing slash.
    let base = base.trim_end_matches('/').to_string();
    vault.write_fhir_config(&FhirConfig { fhir_base: base })?;
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: bearer,
            refresh_token: None,
            token_type: Some("Bearer".into()),
            scope: Some("patient/*.read".into()),
            expires_at: None, // user re-pastes when the portal token expires
        },
    )
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    let _ = vault.delete_sync_token(SERVICE);
    // Also clear the non-secret FHIR config so a reconnect can supply a
    // different FHIR base URL.
    let _ = vault.resolve(CONFIG_FILE).map(|p| std::fs::remove_file(p));
    Ok(())
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let cfg = vault.read_fhir_config();
    let configured = !cfg.fhir_base.is_empty();
    let accounts = match vault.load_sync_token(SERVICE)? {
        Some(token) => {
            vec![ConnectedAccount {
                key: SERVICE.to_string(),
                label: if cfg.fhir_base.is_empty() {
                    "FHIR server (URL unknown)".to_string()
                } else {
                    cfg.fhir_base.clone()
                },
                connected_at: None,
                expires_at: token.expires_at,
                // No refresh token; if the token has expired_at and is past
                // it, ask the user to reconnect.
                needs_reconnect: token.expired(),
                extra: BTreeMap::new(),
            }]
        }
        None => Vec::new(),
    };
    Ok(ConnectStatus { configured, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds
/// the `&crate::smart_on_fhir::CONNECTION,` line).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Medical Records (SMART on FHIR)",
    methods: &[ConnectMethod::TokenPaste {
        label: "FHIR base URL and access token",
        help: "From your patient portal's developer or API-access section, copy your FHIR R4 \
               base URL and a bearer token.  Paste them here separated by a pipe character: \
               https://fhir.example.com/R4|ey...token",
        placeholder: "https://fhir.example.com/R4|ey...token",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["smart-on-fhir"],
    setup: &[
        "Sign in to your patient portal.  Look for a \"Developer\", \"FHIR API\", or \
         \"Data access\" section that provides a FHIR R4 base URL and an access token.",
        "Copy the FHIR R4 base URL (e.g. https://fhir.example.com/R4) and your access token.",
        "Paste them here separated by a pipe: https://fhir.example.com/R4|ey...token",
        "For Epic MyChart, Quest Diagnostics, or Labcorp, use the dedicated integration instead.",
    ],
};

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

/// FHIR operations used by the pull.  Injectable so tests run offline.
pub trait FhirApi {
    /// `GET <resource_type>?patient=<id>&_sort=_lastUpdated&_count=<n>
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

    /// `GET Patient?_format=json` — resolve the patient id.
    fn patient(&self, token: &str) -> Result<Value, FetchError>;
}

/// Thin FHIR R4 client backed by ureq.
pub struct FhirClient {
    base: String,
}

impl FhirClient {
    pub fn new(base: impl Into<String>) -> Self {
        let mut base = base.into();
        while base.ends_with('/') {
            base.pop();
        }
        FhirClient { base }
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

impl FhirApi for FhirClient {
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

/// Percent-encode a query parameter value (RFC 3986 unreserved chars pass
/// through; everything else is %-encoded).
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

/// Return the later of two optional watermark strings, comparing as UTC
/// instants.  Falls back to lexicographic comparison if one is unparseable.
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

/// Extract the effective timestamp from a FHIR resource for month-partitioning.
/// Never returns an empty string — falls back to today rather than wedging a
/// whole-page append.
fn resource_ts(res: &Value) -> String {
    for s in [
        str_field(res, "effectiveDateTime"),
        nested_str(res, &["effectivePeriod", "start"]),
        str_field(res, "onsetDateTime"),
        str_field(res, "occurrenceDateTime"),
        str_field(res, "recordedDate"),
        str_field(res, "authoredOn"),
        str_field(res, "performedDateTime"),
        str_field(res, "issued"),
        nested_str(res, &["meta", "lastUpdated"]),
    ] {
        if !s.is_empty() {
            return s;
        }
    }
    Local::now().date_naive().to_string()
}

// ---------------------------------------------------------------------------
// FHIR R4 Observation → contract Observation.
//
// Evidence basis: FHIR R4 Observation specification
// (hl7.org/fhir/R4/observation.html), confirmed against the FHIR R4
// canonical examples and the Epic sandbox fixtures in epic_mychart.rs.
// Every USCDI-v3 compliant server MUST expose these fields in the same
// standard locations.
//
//   guid        = "smart-on-fhir/" + Observation.id
//                 (namespaced so guids do not collide with epic-mychart/
//                 quest-diagnostics/labcorp Observation ids, which share the
//                 same FHIR id space per server but are unique per server)
//   ts          = effectiveDateTime → effectivePeriod.start →
//                 effectiveInstant → issued → meta.lastUpdated
//   test        = code.coding[0].display OR code.text
//   code        = LOINC code from code.coding (prefer LOINC system)
//   code_system = "loinc" when LOINC, else raw system URL
//   value       = valueQuantity.value OR valueInteger
//   value_text  = valueString | valueCodeableConcept.text | valueRange/valueRatio
//   unit        = valueQuantity.unit (UCUM)
//   reference_range = referenceRange[0].text or lo–hi synthesized
//   flag        = interpretation[0].coding[0].code
//   panel       = basedOn[0].display
//   provider    = performer[0].display
//   extra       = status, category, specimen, encounter, component, …

/// Map a FHIR R4 Observation resource to zero or more contract [`Observation`]s.
///
/// Most Observations map to exactly one row.  Multi-component Observations (e.g.
/// blood pressure with LOINC 55284-4 / 85354-9) carry NO top-level `value[x]`
/// per the FHIR R4 spec (hl7.org/fhir/R4/observation.html#component); their
/// systolic/diastolic readings live in `component[].valueQuantity`.  For these,
/// one row is emitted per component, each inheriting the parent id, ts, and top-
/// level code, with the component's own code/display, value, unit, and a guid
/// suffix of `/<component-code>` to keep guids unique and deduplicated.
///
/// Returns an empty Vec when the resource has no usable `id`, no dateable
/// instant, or no test name (ungroupable Observations land in the raw layer only).
pub fn observations_from_fhir(res: &Value) -> Vec<Observation> {
    match observation_from_fhir_inner(res) {
        None => vec![],
        Some(obs) => obs,
    }
}

/// For backward-compat callers: returns the first mapped Observation (or None).
/// Prefer `observations_from_fhir` when multiple rows per resource are possible.
pub fn observation_from_fhir(res: &Value) -> Option<Observation> {
    observations_from_fhir(res).into_iter().next()
}

fn observation_from_fhir_inner(res: &Value) -> Option<Vec<Observation>> {
    let id = str_field(res, "id");
    if id.is_empty() {
        return None;
    }

    // ts: prefer effectiveDateTime; fall back in order.
    // effectiveTiming.event[0] is also a valid R4 effective[x] choice;
    // handled here so a Timing-only Observation still gets a ts rather than
    // falling all the way back to issued / meta.lastUpdated.
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
                    // effectiveTiming.event[0] — pick first event timestamp if present.
                    let timing_event = res
                        .get("effectiveTiming")
                        .and_then(|t| t.get("event"))
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .unwrap_or("")
                        .to_string();
                    if !timing_event.is_empty() {
                        timing_event
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
        }
    };
    if ts.is_empty() {
        return None;
    }

    // Test name: code.coding[0].display, then code.text.
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

    // LOINC code: prefer a coding entry whose system URL contains "loinc".
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
                    let code = c
                        .get("code")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let system = c
                        .get("system")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
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
    } else if let Some(b) = res.get("valueBoolean").and_then(Value::as_bool) {
        // Boolean results (e.g. presence/absence flags).
        if b { "true".to_string() } else { "false".to_string() }
    } else if let Some(s) = res.get("valueDateTime").and_then(Value::as_str) {
        // DateTime results (e.g. last menstrual period date).
        s.trim().to_string()
    } else if let Some(s) = res.get("valueTime").and_then(Value::as_str) {
        // Time-of-day results.
        s.trim().to_string()
    } else if let Some(period) = res.get("valuePeriod") {
        // Period results: "start – end" or individual bound.
        let start = period.get("start").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let end = period.get("end").and_then(Value::as_str).unwrap_or("").trim().to_string();
        match (start.is_empty(), end.is_empty()) {
            (false, false) => format!("{start} – {end}"),
            (false, true) => format!("{start} –"),
            (true, false) => format!("– {end}"),
            (true, true) => String::new(),
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

    // Reference range: text first, then lo–hi synthesized.
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
    let provider_name = res
        .get("performer")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|p| p.get("display"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();

    // Multi-component Observations (e.g. blood pressure LOINC 55284-4 / 85354-9):
    // Per FHIR R4 spec (hl7.org/fhir/R4/observation.html#component), these carry
    // NO top-level value[x]; the systolic/diastolic readings live in
    // component[].valueQuantity.  When the observation has component[] AND no
    // top-level value (numeric or text), emit one contract row per component so
    // each reading is independently queryable.
    let components = res
        .get("component")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let has_top_value = value.is_some() || !value_text.is_empty();

    if !has_top_value && !components.is_empty() {
        // Emit one Observation row per component.
        let mut rows: Vec<Observation> = Vec::with_capacity(components.len());
        for comp in components {
            // Component test name: code.coding[0].display or code.text.
            let comp_test = {
                let from_coding = comp
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
                    comp.get("code")
                        .and_then(|c| c.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string()
                }
            };
            if comp_test.is_empty() {
                continue; // ungroupable component
            }

            // Component LOINC code (for guid suffix and code field).
            let comp_code = comp
                .get("code")
                .and_then(|c| c.get("coding"))
                .and_then(Value::as_array)
                .and_then(|a| {
                    // Prefer LOINC system.
                    a.iter()
                        .find(|c| {
                            c.get("system")
                                .and_then(Value::as_str)
                                .map(|s| s.to_lowercase().contains("loinc"))
                                .unwrap_or(false)
                        })
                        .or_else(|| a.first())
                })
                .and_then(|c| c.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();

            // Component value and unit from valueQuantity (most common for vitals).
            let comp_value = comp
                .get("valueQuantity")
                .and_then(|q| q.get("value"))
                .and_then(Value::as_f64)
                .or_else(|| {
                    comp.get("valueInteger").and_then(Value::as_i64).map(|i| i as f64)
                });
            let comp_unit = comp
                .get("valueQuantity")
                .and_then(|q| q.get("unit"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            // Component qualitative text (valueCodeableConcept / valueString).
            let comp_value_text = if let Some(s) =
                comp.get("valueString").and_then(Value::as_str)
            {
                s.trim().to_string()
            } else if let Some(cc) = comp.get("valueCodeableConcept") {
                cc.get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string()
            } else {
                String::new()
            };

            // Skip components that carry no measurable value — only
            // dataAbsentReason etc. are present; mirroring the
            // ungroupable-drop rule applied to the top-level observation.
            if comp_value.is_none() && comp_value_text.is_empty() {
                continue;
            }

            // Guid: parent id + component code suffix for uniqueness.
            let suffix = if comp_code.is_empty() {
                comp_test.to_lowercase().replace(' ', "-")
            } else {
                comp_code.clone()
            };
            let comp_guid = format!("smart-on-fhir/{id}/{suffix}");

            // Build per-component extra (inherits parent extra + component raw).
            let mut comp_extra = Map::new();
            for key in &["status", "category", "encounter", "dataAbsentReason", "bodySite", "method", "device", "note"] {
                if let Some(v) = res.get(*key) {
                    if v != &Value::Null && !(v.is_string() && v.as_str().unwrap_or("").is_empty()) {
                        comp_extra.insert((*key).to_string(), v.clone());
                    }
                }
            }
            // Store the raw component object for full fidelity.
            comp_extra.insert("component_raw".to_string(), comp.clone());

            rows.push(Observation {
                ts: ts.clone(),
                source: SERVICE.into(),
                guid: comp_guid,
                test: comp_test,
                code: comp_code,
                code_system: code_system.clone(),
                value: comp_value,
                value_text: comp_value_text,
                unit: comp_unit,
                reference_range: reference_range.clone(),
                flag: flag.clone(),
                panel: panel.clone(),
                provider: provider_name.clone(),
                extra: comp_extra,
            });
        }
        return Some(rows);
    }

    // Single-value Observation: build extra including any component array for
    // full raw fidelity at the contract layer.
    let mut extra = Map::new();
    for key in &[
        "status",
        "category",
        "specimen",
        "encounter",
        "component",
        "dataAbsentReason",
        "bodySite",
        "method",
        "device",
        "note",
    ] {
        if let Some(v) = res.get(*key) {
            if v != &Value::Null
                && !(v.is_string() && v.as_str().unwrap_or("").is_empty())
            {
                extra.insert((*key).to_string(), v.clone());
            }
        }
    }

    // Namespace the guid to smart-on-fhir so it does not collide with Epic/
    // Quest/Labcorp Observation ids.
    //
    // NOTE: The guid is scoped to smart-on-fhir (not per FHIR base URL) because
    // the single TokenPaste connection holds only one org at a time.  If the user
    // re-points the connection to a different FHIR server, FHIR ids from the old
    // server may collide with ids from the new server (both assigned e.g.
    // Observation/123).  To avoid silent deduplication when switching orgs the
    // user should delete the cursor file (.trove/smart-on-fhir-sync.json) and the
    // observations folder (.trove/health/medical/smart-on-fhir/) before reconnecting.
    Some(vec![Observation {
        ts,
        source: SERVICE.into(),
        guid: format!("smart-on-fhir/{id}"),
        test,
        code,
        code_system,
        value,
        value_text,
        unit,
        reference_range,
        flag,
        panel,
        provider: provider_name,
        extra,
    }])
}

// ---------------------------------------------------------------------------
// Cursor.

/// Per-resource-type watermarks and cached patient id.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SyncState {
    /// resourceType → max `meta.lastUpdated` seen (UTC RFC3339).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub watermarks: HashMap<String, String>,
    /// Cached patient id to avoid a round-trip on every sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patient_id: Option<String>,
    /// RFC3339 timestamp of the last successful full sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<String>,
}

impl Vault {
    pub fn read_smart_fhir_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn write_smart_fhir_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Full-fidelity raw row: the verbatim FHIR resource JSON.  `ts` is only for
/// month-partitioning and is not serialized.
struct RawLine {
    ts: String,
    value: Value,
}

impl Serialize for RawLine {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        self.value.serialize(ser)
    }
}

/// Write new raw resources for a given resource type, deduped by FHIR `id`.
fn write_raw(vault: &Vault, resource_type: &str, resources: Vec<Value>) -> Result<u64> {
    let raw_dir = format!("{RAW_ROOT}/{resource_type}");
    let stream = vault.stream(&raw_dir, Partition::Month);

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

/// Write new contract Observations + raw Observations, both deduped by id/guid.
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
        if !id.is_empty() && raw_seen.insert(id) {
            let ts = resource_ts(&r);
            new_raws.push(RawLine { ts, value: r.clone() });
        }
        for obs in observations_from_fhir(&r) {
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
// Patient id resolution.

/// Resolve the patient id from the FHIR Patient endpoint.  The response may
/// be a Bundle (search result) or a direct Patient resource.
fn resolve_patient_id(api: &impl FhirApi, token: &str) -> Result<String, FetchError> {
    let bundle = api.patient(token)?;
    let entry = bundle
        .get("entry")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|e| e.get("resource"));
    let patient = entry.unwrap_or(&bundle);
    let id = str_field(patient, "id");
    if id.is_empty() {
        Err(FetchError::Other(
            "FHIR server returned a Patient bundle with no id — \
             cannot query per-patient resources"
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
            "SMART on FHIR is not connected — paste a FHIR base URL and bearer token \
             in the Integrations tab",
        )?;
    let cfg = vault.read_fhir_config();
    if cfg.fhir_base.is_empty() {
        bail!(
            "SMART on FHIR config is missing the FHIR base URL — reconnect from the \
             Integrations tab"
        );
    }
    let client = FhirClient::new(&cfg.fhir_base);
    pull_with(vault, &client, &token.access_token)
}

/// Testable pull body over an injected API + access token.
///
/// 1. Resolves the patient id (cached in the cursor; refetched if missing).
/// 2. For each resource type: drains all pages from the `meta.lastUpdated`
///    watermark, writes raw (all types) + contract (Observations only),
///    advances the watermark.
/// 3. Returns a count map: `observations` + `other_raw` (sum of all
///    non-Observation new raw rows).
pub fn pull_with(vault: &Vault, api: &impl FhirApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_smart_fhir_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Resolve patient id (cached to avoid a network round-trip on every sync).
    let patient_id = if let Some(id) = &state.patient_id {
        id.clone()
    } else {
        let id = resolve_patient_id(api, token).map_err(|e| match e {
            FetchError::Unauthorized => anyhow::anyhow!(
                "SMART on FHIR rejected the token (401) — reconnect from the Integrations tab"
            ),
            other => anyhow::anyhow!("SMART on FHIR patient lookup failed: {other}"),
        })?;
        state.patient_id = Some(id.clone());
        id
    };

    let mut obs_total: u64 = 0;
    let mut other_raw_total: u64 = 0;

    for &rt in RESOURCE_TYPES {
        let watermark = state.watermarks.get(rt).map(String::as_str);
        let mut page_url: Option<String> = None;
        let mut new_watermark: Option<String> = None;
        let mut rt_raw: u64 = 0;

        loop {
            let result = api.resources(token, rt, &patient_id, watermark, page_url.as_deref());
            let bundle = match result {
                Err(FetchError::Unauthorized) => {
                    return Err(anyhow::anyhow!(
                        "SMART on FHIR rejected the token (401) on {} — reconnect",
                        rt
                    ));
                }
                Err(FetchError::NotFound) => {
                    // Some servers return 404 for resource types the patient
                    // has none of — skip gracefully.
                    break;
                }
                Err(other) => {
                    return Err(anyhow::anyhow!("SMART on FHIR {rt} fetch failed: {other}"));
                }
                Ok(b) => b,
            };

            let entries: Vec<Value> =
                bundle_entries(&bundle, rt).into_iter().cloned().collect();

            // Track max meta.lastUpdated for the watermark.
            for entry in &entries {
                if let Some(lu) = entry
                    .get("meta")
                    .and_then(|m| m.get("lastUpdated"))
                    .and_then(Value::as_str)
                {
                    new_watermark = Some(max_watermark(new_watermark.as_deref(), lu));
                }
            }

            if rt == "Observation" {
                obs_total += write_observations(vault, entries)?;
            } else {
                rt_raw += write_raw(vault, rt, entries)?;
            }

            match next_link(&bundle) {
                Some(next) => page_url = Some(next.to_string()),
                None => break,
            }
        }

        // Advance watermark after the full drain for this resource type and
        // persist partial progress so a failure on a later type doesn't lose
        // the work already done (mirrors epic_mychart's per-type write).
        if let Some(wm) = new_watermark {
            let prev = state.watermarks.get(rt).map(String::as_str);
            let advanced = max_watermark(prev, &wm);
            state.watermarks.insert(rt.to_string(), advanced);
            vault.write_smart_fhir_sync(&state)?;
        }

        other_raw_total += rt_raw;
    }

    counts.insert("observations", obs_total);
    if other_raw_total > 0 {
        counts.insert("other_raw", other_raw_total);
    }

    state.updated = Some(Local::now().to_rfc3339());
    vault.write_smart_fhir_sync(&state)?;

    let total: u64 = counts.values().sum();
    Ok(PullOutcome {
        headline: format!("SMART on FHIR synced — {total} new records"),
        counts,
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!(
            "trove-smart-fhir-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // FHIR R4 Observation fixtures — built against hl7.org/fhir/R4/observation.html
    // and the same shape confirmed by the Epic sandbox fixtures in epic_mychart.rs.
    // These field names and cardinalities are mandated by USCDI v3 / FHIR R4 for
    // every compliant server (Cerner, Meditech, Epic, Quest, Labcorp, etc.).

    /// A LOINC-coded numeric lab result (glucose): the most common Observation
    /// shape.  Fields: id, effectiveDateTime, LOINC code.coding, valueQuantity,
    /// referenceRange, interpretation (flag), basedOn (panel), performer (provider).
    fn obs_glucose() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-glucose-001",
            "meta": {"lastUpdated": "2026-06-10T10:00:00+00:00"},
            "status": "final",
            "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "laboratory"}]}],
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "15074-8", "display": "Glucose [Mass/volume] in Blood"}],
                "text": "Glucose"
            },
            "effectiveDateTime": "2026-06-10T09:30:00-07:00",
            "issued": "2026-06-10T10:00:00Z",
            "valueQuantity": {"value": 113, "unit": "mg/dL", "system": "http://unitsofmeasure.org", "code": "mg/dL"},
            "referenceRange": [{"low": {"value": 70, "unit": "mg/dL"}, "high": {"value": 99, "unit": "mg/dL"}, "text": "70-99"}],
            "interpretation": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation", "code": "H", "display": "High"}]}],
            "basedOn": [{"reference": "ServiceRequest/sr-001", "display": "Comprehensive Metabolic Panel"}],
            "performer": [{"reference": "Organization/lab-001", "display": "Quest Diagnostics"}]
        })
    }

    /// A qualitative (non-numeric) Observation: HIV antibody screen with
    /// valueCodeableConcept.text ("Non-Reactive").  LOINC 75622-1.
    fn obs_qualitative() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-hiv-001",
            "meta": {"lastUpdated": "2025-11-03T12:00:00+00:00"},
            "status": "final",
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "75622-1", "display": "HIV 1 and 2 tests - Meaningful Use set"}],
                "text": "HIV 1/2 Antibody Screen"
            },
            "effectiveDateTime": "2025-11-03",
            "valueCodeableConcept": {
                "coding": [{"system": "http://snomed.info/sct", "code": "131194007", "display": "Non-Reactive"}],
                "text": "Non-Reactive"
            }
        })
    }

    /// A vital-sign Observation (blood pressure) in the REAL FHIR R4 shape:
    /// LOINC 55284-4 panel with NO top-level value[x]; systolic (8480-6) and
    /// diastolic (8462-4) readings live in component[].valueQuantity as mandated
    /// by hl7.org/fhir/R4/observation.html#component.  Confirmed against Epic,
    /// Cerner, and the FHIR R4 canonical BP example.
    fn obs_blood_pressure() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-bp-001",
            "meta": {"lastUpdated": "2026-05-15T08:00:00+00:00"},
            "status": "final",
            "category": [{"coding": [{"system": "http://terminology.hl7.org/CodeSystem/observation-category", "code": "vital-signs"}]}],
            "code": {
                "coding": [{"system": "http://loinc.org", "code": "55284-4", "display": "Blood pressure systolic and diastolic"}],
                "text": "Blood Pressure"
            },
            "effectiveDateTime": "2026-05-15T08:00:00Z",
            // NOTE: no top-level valueQuantity — real BP observations carry readings
            // only in component[]; a top-level value[x] would violate the spec.
            "component": [
                {
                    "code": {
                        "coding": [{"system": "http://loinc.org", "code": "8480-6", "display": "Systolic blood pressure"}],
                        "text": "Systolic"
                    },
                    "valueQuantity": {"value": 120.0, "unit": "mm[Hg]", "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}
                },
                {
                    "code": {
                        "coding": [{"system": "http://loinc.org", "code": "8462-4", "display": "Diastolic blood pressure"}],
                        "text": "Diastolic"
                    },
                    "valueQuantity": {"value": 80.0, "unit": "mm[Hg]", "system": "http://unitsofmeasure.org", "code": "mm[Hg]"}
                }
            ]
        })
    }

    /// An Observation with no `id` — must be dropped by the mapper.
    fn obs_no_id() -> Value {
        json!({
            "resourceType": "Observation",
            "status": "final",
            "code": {"text": "Some test"},
            "effectiveDateTime": "2026-06-01"
        })
    }

    /// An Observation with no effective date, issued, or meta.lastUpdated —
    /// must be dropped by the mapper (no ts to partition on).
    fn obs_no_date() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-no-date",
            "status": "final",
            "code": {"text": "Some test"}
        })
    }

    /// An ungroupable Observation: has id + date but no code text/display —
    /// mapper must return None (raw layer still captures it).
    fn obs_no_name() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-no-name",
            "status": "final",
            "effectiveDateTime": "2026-06-01",
            "valueQuantity": {"value": 42.0, "unit": "unit"}
        })
    }

    /// A non-Observation resource (Condition) — raw-only.
    fn condition_resource() -> Value {
        json!({
            "resourceType": "Condition",
            "id": "cond-001",
            "meta": {"lastUpdated": "2026-04-01T00:00:00+00:00"},
            "clinicalStatus": {"coding": [{"code": "active"}]},
            "code": {
                "coding": [{"system": "http://hl7.org/fhir/sid/icd-10-cm", "code": "E11", "display": "Type 2 diabetes mellitus"}],
                "text": "Type 2 Diabetes"
            },
            "onsetDateTime": "2020-03-01",
            "subject": {"reference": "Patient/patient-001"}
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

    fn patient_bundle(patient_id: &str) -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": [{"resource": {"resourceType": "Patient", "id": patient_id}}]
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
    }

    impl FhirApi for MockApi {
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
    // Observation mapper tests.

    #[test]
    fn maps_numeric_loinc_observation() {
        let o = observation_from_fhir(&obs_glucose()).unwrap();
        assert_eq!(o.source, "smart-on-fhir");
        assert_eq!(o.guid, "smart-on-fhir/obs-glucose-001");
        assert_eq!(o.test, "Glucose [Mass/volume] in Blood");
        assert_eq!(o.code, "15074-8");
        assert_eq!(o.code_system, "loinc");
        assert_eq!(o.value, Some(113.0));
        assert_eq!(o.unit, "mg/dL");
        assert_eq!(o.reference_range, "70-99");
        assert_eq!(o.flag, "H");
        assert_eq!(o.panel, "Comprehensive Metabolic Panel");
        assert_eq!(o.provider, "Quest Diagnostics");
        assert!(!o.ts.is_empty(), "ts is set from effectiveDateTime");
        assert!(o.extra.contains_key("status"), "status in extra");
        assert!(o.extra.contains_key("category"), "category in extra");
    }

    #[test]
    fn maps_qualitative_observation_to_value_text() {
        let o = observation_from_fhir(&obs_qualitative()).unwrap();
        assert_eq!(o.guid, "smart-on-fhir/obs-hiv-001");
        assert_eq!(o.test, "HIV 1 and 2 tests - Meaningful Use set");
        assert_eq!(o.value_text, "Non-Reactive");
        assert!(o.value.is_none(), "no numeric value for a qualitative result");
        assert_eq!(o.code, "75622-1");
        assert_eq!(o.code_system, "loinc");
    }

    /// Real FHIR R4 blood pressure observations have NO top-level value[x];
    /// systolic/diastolic readings live in component[].valueQuantity only.
    /// The mapper must emit one contract row per component so both readings are
    /// captured at the contract layer (not silently lost as value=None).
    #[test]
    fn maps_vital_sign_blood_pressure_components() {
        let rows = observations_from_fhir(&obs_blood_pressure());
        // Exactly two rows: one per component (systolic + diastolic).
        assert_eq!(rows.len(), 2, "BP observation must produce 2 component rows, got {}", rows.len());

        // Both rows share the parent ts and source.
        for row in &rows {
            assert_eq!(row.source, "smart-on-fhir");
            assert!(!row.ts.is_empty(), "component row must have ts");
            // guid is suffixed with component LOINC code for uniqueness.
            assert!(
                row.guid.starts_with("smart-on-fhir/obs-bp-001/"),
                "component guid must be suffixed: {}",
                row.guid
            );
        }

        // Find systolic and diastolic rows by guid suffix.
        let systolic = rows.iter().find(|r| r.guid.contains("8480-6")).expect("systolic row missing");
        let diastolic = rows.iter().find(|r| r.guid.contains("8462-4")).expect("diastolic row missing");

        assert_eq!(systolic.test, "Systolic blood pressure");
        assert_eq!(systolic.code, "8480-6");
        assert_eq!(systolic.value, Some(120.0), "systolic value");
        assert_eq!(systolic.unit, "mm[Hg]");

        assert_eq!(diastolic.test, "Diastolic blood pressure");
        assert_eq!(diastolic.code, "8462-4");
        assert_eq!(diastolic.value, Some(80.0), "diastolic value");
        assert_eq!(diastolic.unit, "mm[Hg]");
    }

    /// Verify that observation_from_fhir (single-result compat shim) returns
    /// the first component row for a multi-component observation.
    #[test]
    fn observation_from_fhir_compat_shim_returns_first_component() {
        let first = observation_from_fhir(&obs_blood_pressure());
        assert!(first.is_some(), "compat shim must return Some for a valid BP observation");
        let row = first.unwrap();
        assert!(row.guid.starts_with("smart-on-fhir/obs-bp-001/"), "compat shim guid: {}", row.guid);
    }

    #[test]
    fn drops_observation_without_id() {
        assert!(observation_from_fhir(&obs_no_id()).is_none(), "no id → None");
    }

    #[test]
    fn drops_observation_without_any_date() {
        assert!(observation_from_fhir(&obs_no_date()).is_none(), "no date → None");
    }

    #[test]
    fn drops_observation_without_test_name() {
        assert!(observation_from_fhir(&obs_no_name()).is_none(), "no name → None");
    }

    #[test]
    fn guid_is_namespaced() {
        let o = observation_from_fhir(&obs_glucose()).unwrap();
        assert!(
            o.guid.starts_with("smart-on-fhir/"),
            "guid must be namespaced: {}",
            o.guid
        );
    }

    // -----------------------------------------------------------------------
    // pull_with integration tests.

    #[test]
    fn full_pull_writes_both_layers_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(patient_bundle("patient-001"))
            .page("Observation", vec![obs_glucose(), obs_qualitative()])
            .page("Condition", vec![condition_resource()]);

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&2));
        assert_eq!(out.counts.get("other_raw"), Some(&1));

        let obs_dir = v.stream(DIR, Partition::Month);
        let obs_total: usize = obs_dir
            .partitions()
            .unwrap()
            .iter()
            .map(|k| obs_dir.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(obs_total, 2, "two contract rows");

        let raw_obs = v.stream(&format!("{RAW_ROOT}/Observation"), Partition::Month);
        let raw_obs_total: usize = raw_obs
            .partitions()
            .unwrap()
            .iter()
            .map(|k| raw_obs.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(raw_obs_total, 2, "two raw Observation rows");

        let raw_cond = v.stream(&format!("{RAW_ROOT}/Condition"), Partition::Month);
        let raw_cond_total: usize = raw_cond
            .partitions()
            .unwrap()
            .iter()
            .map(|k| raw_cond.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(raw_cond_total, 1, "one raw Condition row");

        let state = v.read_smart_fhir_sync();
        assert_eq!(state.patient_id.as_deref(), Some("patient-001"));
        assert!(state.watermarks.contains_key("Observation"), "Observation watermark set");
        assert!(state.updated.is_some(), "updated timestamp persisted");
    }

    #[test]
    fn deduplication_prevents_double_write_on_re_pull() {
        let v = temp_vault("dedup");
        let api = MockApi::new(patient_bundle("p")).page("Observation", vec![obs_glucose()]);
        let out1 = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out1.counts.get("observations"), Some(&1));

        let api2 = MockApi::new(patient_bundle("p")).page("Observation", vec![obs_glucose()]);
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(out2.counts.get("observations"), Some(&0), "no new rows on re-pull");

        let obs_dir = v.stream(DIR, Partition::Month);
        let total: usize = obs_dir
            .partitions()
            .unwrap()
            .iter()
            .map(|k| obs_dir.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(total, 1, "still exactly one row after dedup re-pull");
    }

    #[test]
    fn ungroupable_obs_lands_in_raw_but_not_contract() {
        let v = temp_vault("ungroupable");
        let api = MockApi::new(patient_bundle("p")).page("Observation", vec![obs_no_name()]);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&0));

        let obs_dir = v.stream(DIR, Partition::Month);
        let contract_total: usize = obs_dir
            .partitions()
            .unwrap_or_default()
            .iter()
            .map(|k| obs_dir.read::<Value>(k).unwrap_or_default().len())
            .sum();
        assert_eq!(contract_total, 0, "ungroupable → no contract row");

        let raw_obs = v.stream(&format!("{RAW_ROOT}/Observation"), Partition::Month);
        let raw_total: usize = raw_obs
            .partitions()
            .unwrap()
            .iter()
            .map(|k| raw_obs.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(raw_total, 1, "ungroupable Observation lands in raw");
    }

    /// A blood pressure observation (component-only, no top-level value[x]) must
    /// produce 2 contract rows (one per component) and 1 raw row (the full resource).
    #[test]
    fn bp_component_observation_produces_two_contract_rows() {
        let v = temp_vault("bp_components");
        let api = MockApi::new(patient_bundle("p")).page("Observation", vec![obs_blood_pressure()]);
        let out = pull_with(&v, &api, "tok").unwrap();
        // 2 contract rows from the 2 components.
        assert_eq!(out.counts.get("observations"), Some(&2), "BP must produce 2 contract rows (systolic + diastolic)");

        let obs_dir = v.stream(DIR, Partition::Month);
        let contract_total: usize = obs_dir
            .partitions()
            .unwrap()
            .iter()
            .map(|k| obs_dir.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(contract_total, 2, "two contract rows written to vault");

        // Raw layer: 1 row (the full BP resource verbatim).
        let raw_obs = v.stream(&format!("{RAW_ROOT}/Observation"), Partition::Month);
        let raw_total: usize = raw_obs
            .partitions()
            .unwrap()
            .iter()
            .map(|k| raw_obs.read::<Value>(k).unwrap().len())
            .sum();
        assert_eq!(raw_total, 1, "one raw Observation row (full BP resource)");
    }

    #[test]
    fn empty_account_is_a_clean_noop() {
        let v = temp_vault("empty");
        let api = MockApi::new(patient_bundle("p"));
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&0));
        let state = v.read_smart_fhir_sync();
        assert_eq!(state.patient_id.as_deref(), Some("p"), "patient id cached");
        assert!(state.watermarks.is_empty(), "no watermarks without data");
    }

    #[test]
    fn pull_without_connection_is_a_clean_error() {
        let v = temp_vault("unconnected");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error: {err}");
    }

    // -----------------------------------------------------------------------
    // Composite token parse.

    #[test]
    fn parse_composite_splits_url_and_token() {
        let (base, tok) = parse_composite("https://fhir.example.com/R4|ey.abc.xyz").unwrap();
        assert_eq!(base, "https://fhir.example.com/R4");
        assert_eq!(tok, "ey.abc.xyz");
    }

    #[test]
    fn parse_composite_trims_whitespace() {
        let (base, tok) = parse_composite("  https://fhir.example.com/R4 | eyABC  ").unwrap();
        assert_eq!(base, "https://fhir.example.com/R4");
        assert_eq!(tok, "eyABC");
    }

    #[test]
    fn parse_composite_requires_pipe_separator() {
        let err = parse_composite("https://fhir.example.com/R4").unwrap_err();
        assert!(err.to_string().contains("pipe") || err.to_string().contains("|"), "{err}");
    }

    #[test]
    fn parse_composite_rejects_empty_token() {
        let err = parse_composite("https://fhir.example.com/R4|").unwrap_err();
        assert!(err.to_string().contains("bearer token"), "{err}");
    }

    #[test]
    fn parse_composite_rejects_empty_base() {
        let err = parse_composite("|ey.token").unwrap_err();
        assert!(err.to_string().contains("FHIR base URL"), "{err}");
    }

    // -----------------------------------------------------------------------
    // Connection store tests.

    #[test]
    fn connect_stores_fhir_base_and_bearer_token_separately() {
        let v = temp_vault("connect-store");
        def_connect(&v, "https://fhir.example.com/R4|ey.my.secret").unwrap();

        let cfg = v.read_fhir_config();
        assert_eq!(cfg.fhir_base, "https://fhir.example.com/R4");

        // Bearer token must NOT appear in the non-secret config file.
        let cfg_raw = std::fs::read_to_string(v.resolve(CONFIG_FILE).unwrap()).unwrap();
        assert!(!cfg_raw.contains("ey.my.secret"), "secret not in config file");

        let tok = v.load_sync_token(SERVICE).unwrap().unwrap();
        assert_eq!(tok.access_token, "ey.my.secret");

        let status = def_status(&v).unwrap();
        assert!(status.configured, "configured");
        assert_eq!(status.accounts.len(), 1);
        assert!(
            status.accounts[0].label.contains("fhir.example.com"),
            "label shows FHIR base URL"
        );
    }

    #[test]
    fn connect_strips_trailing_slash_from_fhir_base() {
        let v = temp_vault("trailing-slash");
        def_connect(&v, "https://fhir.example.com/R4/|ey.tok").unwrap();
        let cfg = v.read_fhir_config();
        assert_eq!(cfg.fhir_base, "https://fhir.example.com/R4", "trailing slash stripped");
    }

    #[test]
    fn disconnect_clears_token_and_config() {
        let v = temp_vault("disconnect");
        def_connect(&v, "https://fhir.example.com/R4|ey.tok").unwrap();
        def_disconnect(&v, SERVICE).unwrap();
        assert!(v.load_sync_token(SERVICE).unwrap().is_none(), "token cleared");
        let cfg = v.read_fhir_config();
        assert!(cfg.fhir_base.is_empty(), "config cleared after disconnect");
        let status = def_status(&v).unwrap();
        assert!(!status.configured, "not configured after disconnect");
        assert!(status.accounts.is_empty(), "no accounts after disconnect");
    }

    #[test]
    fn sync_state_back_compat_empty_and_partial() {
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.patient_id.is_none());
        assert!(empty.watermarks.is_empty());
        let fwd: SyncState = serde_json::from_str(
            r#"{"patient_id":"p","watermarks":{"Observation":"2026-06-10T17:10:00Z"},"future":"x"}"#,
        )
        .unwrap();
        assert_eq!(fwd.patient_id.as_deref(), Some("p"));
        assert_eq!(
            fwd.watermarks.get("Observation").map(String::as_str),
            Some("2026-06-10T17:10:00Z")
        );
    }

    #[test]
    fn connection_def_has_token_paste_method() {
        assert!(CONNECTION.method("token-paste").is_some());
        assert_eq!(CONNECTION.id, SERVICE);
        assert_eq!(DEF.connection, Some(SERVICE));
    }

    #[test]
    fn bundle_entries_extracts_resources_by_type() {
        let bundle = fhir_bundle(vec![obs_glucose(), condition_resource()]);
        let obs = bundle_entries(&bundle, "Observation");
        assert_eq!(obs.len(), 1);
        assert_eq!(str_field(obs[0], "id"), "obs-glucose-001");
        let conds = bundle_entries(&bundle, "Condition");
        assert_eq!(conds.len(), 1);
        assert_eq!(str_field(conds[0], "id"), "cond-001");
    }

    #[test]
    fn next_link_finds_the_next_page_url() {
        let bundle = fhir_bundle_with_next(vec![], "https://fhir.example.com/next?page=2");
        assert_eq!(
            next_link(&bundle),
            Some("https://fhir.example.com/next?page=2")
        );
        assert!(next_link(&fhir_bundle(vec![])).is_none());
    }

    #[test]
    fn urlencode_escapes_colons_and_plus() {
        let s = "2026-06-10T17:10:00+00:00";
        let enc = urlencode(s);
        assert!(!enc.contains(':'), "colon escaped: {enc}");
        assert!(!enc.contains('+'), "plus escaped: {enc}");
        assert!(enc.contains("2026-06-10"), "date part passes through");
    }

    #[test]
    fn resource_ts_falls_back_chain() {
        let r = json!({"effectiveDateTime": "2026-06-01", "onsetDateTime": "2026-01-01"});
        assert_eq!(resource_ts(&r), "2026-06-01", "effectiveDateTime first");
        let r2 = json!({"onsetDateTime": "2020-03-01"});
        assert_eq!(resource_ts(&r2), "2020-03-01", "onsetDateTime for Condition");
        let r3 = json!({"meta": {"lastUpdated": "2026-04-01T00:00:00+00:00"}});
        assert_eq!(resource_ts(&r3), "2026-04-01T00:00:00+00:00", "meta.lastUpdated as fallback");
    }

    #[test]
    fn fhir_config_back_compat_empty_and_round_trip() {
        let empty: FhirConfig = serde_json::from_str("{}").unwrap();
        assert!(empty.fhir_base.is_empty());
        let cfg = FhirConfig { fhir_base: "https://fhir.example.com/R4".into() };
        let s = serde_json::to_string(&cfg).unwrap();
        let back: FhirConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.fhir_base, "https://fhir.example.com/R4");
    }
}
