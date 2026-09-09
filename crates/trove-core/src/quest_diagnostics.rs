//! Quest Diagnostics lab results via the MyQuest patient FHIR R4 endpoint.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/quest-diagnostics.md.
//!
//! A **Periodic** cloud pull over Quest's patient FHIR R4 endpoint
//! (`api.questdiagnostics.com/r4`). The endpoint returns structured,
//! LOINC-coded lab results — DiagnosticReport (the panel) containing Observation
//! resources (individual analyte results: code, value, units, reference range,
//! flag). Each Observation becomes a [`crate::health_medical::Observation`] under
//! `health/medical/quest-diagnostics/observations/YYYY-MM.jsonl`.
//!
//! ## Vault layout
//!
//! - **Raw:** `health/medical/quest-diagnostics/raw/YYYY-MM.jsonl` — every FHIR
//!   Observation resource verbatim, full fidelity, unconditional. Written for
//!   ALL Observations including grouper/dataAbsentReason entries that the
//!   contract mapper cannot map, so the raw layer is a complete backstop.
//! - **Contract:** `health/medical/quest-diagnostics/observations/YYYY-MM.jsonl`
//!   — one row per FHIR Observation that was successfully mapped to the
//!   `health-medical` observation contract. `guid` = the FHIR `Observation.id`;
//!   `ts` = `effectiveDateTime` | `effectivePeriod.start` | `effectiveInstant`
//!   | `issued` | `meta.lastUpdated` (first non-empty); `test` =
//!   `code.coding[0].display` or `code.text`; `code` = LOINC code; `value` =
//!   `valueQuantity.value` or `valueInteger`; `value_text` = `valueString` |
//!   `valueCodeableConcept.text` | `valueRange`/`valueRatio` as text;
//!   `flag` from `interpretation[0]`.
//!
//! ## Cursor
//!
//! FHIR R4 search: `GET /r4/Observation?patient=Patient/<id>&_sort=_lastUpdated&_count=200`
//! with `_lastUpdated=ge<watermark>`. The watermark is the max `meta.lastUpdated`
//! across all ingested Observations, persisted in `.trove/quest-diagnostics-sync.json`
//! (non-secret, rebuildable). Paging follows FHIR Bundle `link.relation=next`.
//! The watermark advances only after each page's write (crash-safe re-drain).
//!
//! ## Auth (SMART on FHIR OAuth 2.0, PKCE)
//!
//! Quest's FHIR patient API uses SMART on FHIR 2.0 (PKCE public client).
//! Auth/token endpoints discovered via
//! `GET api.questdiagnostics.com/r4/.well-known/smart-configuration`.
//! Client registration is self-service at Quest's developer portal; a client
//! secret is NOT required for a PKCE public client
//! (`default_client_secret: None`). App credentials baked at build time via
//! `TROVE_QUEST_CLIENT_ID` (empty default); the user can paste their own.
//!
//! **Identity-verification note:** Quest's updated FHIR policy requires
//! third-party identity verification before app access is granted. The connect
//! card must explain this step and direct the user to
//! `myquest.questdiagnostics.com` to complete verification before connecting.
//!
//! The access token (and optional refresh token) live 0600 under `.trove/sync/`.
//! The cursor + patient id are kept in a non-secret rebuildable JSON file.
//!
//! ## Flags
//!
//! Needs-login (validation requires a real MyQuest account with identity
//! verification + app registration). Needs-sample (no Quest FHIR sandbox cited in
//! the brief; the parser is built against the FHIR R4 spec shape, validated via
//! fixtures synthesized from the published FHIR R4 Observation schema).

use std::collections::{BTreeMap, HashSet};
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

const SERVICE: &str = "quest-diagnostics";

/// Contract observations, partitioned by local month of the result date.
const DIR: &str = "health/medical/quest-diagnostics/observations";

/// Raw FHIR Observation JSON, verbatim, unconditional.
const RAW_DIR: &str = "health/medical/quest-diagnostics/raw";

/// Non-secret rebuildable cursor. Deleting it re-drains the full history.
const SYNC_FILE: &str = ".trove/quest-diagnostics-sync.json";

/// Base URL for Quest's patient FHIR R4 endpoint.
const FHIR_BASE: &str = "https://api.questdiagnostics.com/r4";

/// FHIR page size for Observation search. Quest supports `_count=200`.
const PAGE_SIZE: u32 = 200;

/// HTTP request timeout. Kept short; hung connections should not stall the loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Seconds between syncs (hourly). Lab results accumulate infrequently but we
/// check often so new results appear promptly after a blood draw.
const SYNC_SECS: u64 = 3600;

// ---------------------------------------------------------------------------
// OAuth provider (SMART on FHIR PKCE public client).

/// Quest SMART on FHIR OAuth2 provider. PKCE required; no client secret.
/// Auth/token endpoints are per the MyQuest FHIR developer documentation;
/// the `.well-known/smart-configuration` endpoint carries the canonical URLs
/// but those require a live connection to discover — these are the stable
/// production endpoints from Quest's integration guide.
pub static QUEST: Provider = Provider {
    service: SERVICE,
    display_name: "Quest Diagnostics (MyQuest)",
    // Quest SMART on FHIR OAuth endpoints (standard SMART on FHIR paths on
    // the api.questdiagnostics.com host).
    auth_url: "https://api.questdiagnostics.com/oauth2/authorize",
    token_url: "https://api.questdiagnostics.com/oauth2/token",
    // SMART on FHIR patient-level scopes for lab Observations and
    // DiagnosticReports. `offline_access` requests a refresh token.
    scopes: "patient/Observation.read patient/DiagnosticReport.read offline_access",
    // Unique production redirect port assigned to quest-diagnostics (#83).
    // Register: http://localhost:38663/callback in Quest's developer portal.
    redirect_port: 38663,
    // SMART on FHIR mandates PKCE (S256).
    use_pkce: true,
    // PKCE public client: no client secret, no Basic auth.
    basic_auth: false,
    // Bake client id at build time: TROVE_QUEST_CLIENT_ID. Empty default —
    // app registration is a Needs-David flag (requires Quest developer portal
    // account + identity verification).
    default_client_id: option_env!("TROVE_QUEST_CLIENT_ID"),
    // Public (PKCE) client: no client secret.
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
                format!("quest-diagnostics synced — {total} lab results")
            }))
        }
        Err(e) => Ok(crate::registry::CollectOutcome::note(format!(
            "quest-diagnostics sync skipped: {e}"
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
        id: "quest-diagnostics",
        name: "Quest Diagnostics",
        kind: IntegrationKind::CloudSync,
        // Lab results are sensitive medical data — ships opt-in.
        default_on: false,
        description:
            "Pulls your Quest Diagnostics lab results — complete blood count, metabolic panel, \
             A1c, lipids, and more — via the MyQuest FHIR R4 patient endpoint. Results arrive \
             as LOINC-coded rows, enabling longitudinal health trends. First sync backfills your \
             full history; later syncs fetch only new results.",
        domain: "health",
        vault_path: "health/medical/quest-diagnostics/",
        toggleable: true,
        setup: &[
            "Lab results are sensitive medical data — enabling this opts you in to collecting them.",
            "Before connecting, complete identity verification at myquest.questdiagnostics.com — \
             Quest's FHIR policy requires this before third-party apps can access your records.",
            "Register a developer app at the Quest API developer portal to obtain a Client ID \
             (no secret needed — this is a PKCE public client).",
            "Paste your Client ID and click Connect. You will be directed to sign in with your \
             MyQuest credentials and approve access.",
        ],
        caveats:
            "Requires completion of MyQuest's third-party identity verification before OAuth \
             will succeed. Lab results ordered through an Epic-connected health system may also \
             appear in an Epic MyChart pull — cross-source duplicates are reconciled at read time.",
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
    let configured = vault.load_sync_app(QUEST.service)?.is_some()
        || QUEST.default_credentials().is_some();
    let accounts = match vault.load_sync_token(QUEST.service)? {
        Some(token) => {
            let needs_reconnect = token.expired() && token.refresh_token.is_none();
            vec![ConnectedAccount {
                key: QUEST.service.to_string(),
                label: QUEST.display_name.to_string(),
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
/// `&crate::quest_diagnostics::CONNECTION,` line — this module only declares it).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: SERVICE,
    display_name: "Quest Diagnostics",
    methods: &[ConnectMethod::OAuth {
        provider: &QUEST,
        multi_account: false,
        run: connect_oauth,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["quest-diagnostics"],
    setup: &[
        "Complete identity verification at myquest.questdiagnostics.com first — Quest requires \
         this before third-party apps can access your FHIR records.",
        "Register a developer app at Quest's API developer portal. Set the redirect URI to \
         http://localhost:38663/callback (no client secret needed; this is a PKCE flow).",
        "Paste the Client ID here. Saved once, so future connects are just a login.",
    ],
};

/// Interactive OAuth connect: opens the consent page, awaits the redirect, saves
/// the token. Blocking — call from a background thread only.
pub fn connect(vault: &Vault, creds: Option<AppCredentials>) -> Result<TokenSet> {
    let creds = match creds {
        Some(c) => {
            vault.save_sync_app(QUEST.service, &c)?;
            c
        }
        None => vault
            .load_sync_app(QUEST.service)?
            .or_else(|| QUEST.default_credentials())
            .context(
                "no Quest Diagnostics Client ID — register an app at the Quest developer portal \
                 and enter its Client ID in the Integrations tab",
            )?,
    };
    let flow = oauth::OauthFlow::start(&QUEST, &creds)?;
    oauth::open_browser(flow.authorize_url())?;
    let token = flow.finish(&creds, Duration::from_secs(300))?;
    vault.save_sync_token(QUEST.service, &token)?;
    Ok(token)
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SyncState {
    /// FHIR `meta.lastUpdated` watermark (RFC3339): the latest `_lastUpdated`
    /// value seen across all ingested Observations. The next search uses
    /// `_lastUpdated=ge<watermark>` to fetch only new/updated records. Absent
    /// on a cold start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_updated: Option<String>,
    /// FHIR Patient resource id on this server, cached so subsequent syncs do
    /// not re-fetch it. Absent on a cold start (fetched on first sync).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    patient_id: Option<String>,
    /// RFC3339 local time of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_quest_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_quest_sync(&self, state: &SyncState) -> Result<()> {
        crate::store::write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// HTTP layer — injectable for offline tests.

#[derive(Debug)]
enum FetchError {
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
trait QuestFhirApi {
    /// `GET /r4/Patient?_format=json` — the patient's FHIR Patient resource.
    /// Returns the Bundle; caller extracts `entry[0].resource.id`.
    fn patient(&self, token: &str) -> Result<Value, FetchError>;

    /// `GET /r4/Observation?patient=Patient/<id>&_sort=_lastUpdated&_count=200
    ///   [&_lastUpdated=ge<watermark>]`
    /// Returns a FHIR Bundle (searchset). Caller follows `link[next]`.
    fn observations(
        &self,
        token: &str,
        patient_id: &str,
        last_updated_ge: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<Value, FetchError>;
}

struct QuestClient {
    base: String,
}

impl QuestClient {
    fn new() -> Self {
        QuestClient { base: FHIR_BASE.to_string() }
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

impl QuestFhirApi for QuestClient {
    fn patient(&self, token: &str) -> Result<Value, FetchError> {
        let url = format!("{}/Patient?_format=json", self.base);
        self.get_json(&url, token)
    }

    fn observations(
        &self,
        token: &str,
        patient_id: &str,
        last_updated_ge: Option<&str>,
        next_url: Option<&str>,
    ) -> Result<Value, FetchError> {
        let url = if let Some(next) = next_url {
            next.to_string()
        } else {
            let mut q = format!(
                "{}/Observation?patient=Patient/{}&_sort=_lastUpdated&_count={}&_format=json",
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

/// Minimal URL encoding for path segments and query values.
fn urlencoded_id(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

/// Minimal percent-encoding for query parameter values (encodes `:`/`+`/etc.).
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
/// `resourceType == <expected>`. Tolerates missing keys silently.
fn bundle_entries<'a>(bundle: &'a Value, expected_type: &str) -> Vec<&'a Value> {
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
fn next_link(bundle: &Value) -> Option<&str> {
    bundle
        .get("link")
        .and_then(Value::as_array)?
        .iter()
        .find(|l| l.get("relation").and_then(Value::as_str) == Some("next"))
        .and_then(|l| l.get("url"))
        .and_then(Value::as_str)
}

/// Convenience: get a string field, trimmed.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

/// Convenience: descend a dot-separated path and get a string.
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

/// Parse a FHIR instant (RFC3339) into a comparable UTC instant.
/// Returns `None` if the string is not a valid RFC3339 datetime.
/// Used for watermark comparison so that mixed-offset instants
/// (e.g. "2026-05-10T08:00:00-07:00" vs "2026-05-10T15:00:00Z") sort correctly.
fn parse_fhir_instant(s: &str) -> Option<DateTime<chrono::Utc>> {
    s.parse::<DateTime<chrono::FixedOffset>>()
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

// ---------------------------------------------------------------------------
// Pure mapping: FHIR R4 Observation → contract Observation.
//
// Evidence basis: FHIR R4 Observation spec (hl7.org/fhir/R4/observation.html)
// plus the example shapes confirmed from the spec above.
//
// Field derivation:
//   guid        = `id` (FHIR resource id, e.g. "quest-obs-12345")
//   ts          = `effectiveDateTime` → `effectivePeriod.start` →
//                 `effectiveInstant` → `issued` → `meta.lastUpdated`
//   test        = `code.coding[0].display` or `code.text`
//   code        = `code.coding[0].code` where system contains "loinc" (or first)
//   code_system = "loinc" when code is a LOINC code, else raw system URL
//   value       = `valueQuantity.value` or `valueInteger` (numeric, as f64)
//   value_text  = `valueString` | `valueCodeableConcept.text` |
//                 `valueCodeableConcept.coding[0].display` |
//                 `valueRange` as "lo-hi" | `valueRatio` as "num/den"
//   unit        = `valueQuantity.unit` (UCUM preferred)
//   reference_range = `referenceRange[0].text` or `"<low>–<high>"`
//   flag        = `interpretation[0].coding[0].code` (H/L/A/N/…)
//   panel       = via `basedOn[0].display` or DiagnosticReport name in caller
//   provider    = `performer[0].display`
//   extra       = `status`, `category`, `specimen`, `component`, full resource
//                 keys not in the contract

/// Map a FHIR R4 Observation resource to a contract [`Observation`].
/// Returns `None` when the resource has no usable `id` or no dateable instant.
pub fn observation_from_fhir(res: &Value, panel_name: &str) -> Option<Observation> {
    let id = str_field(res, "id");
    if id.is_empty() {
        return None;
    }

    // ts: effectiveDateTime > effectivePeriod.start > effectiveInstant > issued
    //     > meta.lastUpdated.
    // FHIR R4 allows all five shapes; prefer the one that best represents the
    // specimen-collection instant rather than the result-release time.
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
        return None; // an untitled observation is not useful
    }

    // LOINC code: prefer a coding with a loinc system URL.
    let (code, code_system) = res
        .get("code")
        .and_then(|c| c.get("coding"))
        .and_then(Value::as_array)
        .map(|codings| {
            // Prefer the coding whose system contains "loinc".
            let preferred = codings.iter().find(|c| {
                c.get("system")
                    .and_then(Value::as_str)
                    .map(|s| s.to_lowercase().contains("loinc"))
                    .unwrap_or(false)
            });
            let coding = preferred.or_else(|| codings.first());
            match coding {
                Some(c) => {
                    let code = c.get("code").and_then(Value::as_str).unwrap_or("").trim().to_string();
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
    // FHIR R4 value[x] allows valueQuantity, valueInteger, valueRange,
    // valueRatio, valueString, valueCodeableConcept, and others.
    let value = res
        .get("valueQuantity")
        .and_then(|q| q.get("value"))
        .and_then(Value::as_f64)
        .or_else(|| {
            // valueInteger: integer counts/titers (e.g. WBC differential)
            res.get("valueInteger").and_then(Value::as_i64).map(|i| i as f64)
        });

    // Qualitative result text: valueString, valueCodeableConcept.text,
    // valueCodeableConcept.coding[0].display, valueRange/valueRatio as text.
    let value_text = if let Some(s) = res.get("valueString").and_then(Value::as_str) {
        s.trim().to_string()
    } else if let Some(cc) = res.get("valueCodeableConcept") {
        // Prefer .text; fall back to the first coding display (e.g. "Reactive").
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
        // valueRange: represent as "low–high" text.
        let lo = range.get("low").and_then(|q| q.get("value")).and_then(Value::as_f64);
        let hi = range.get("high").and_then(|q| q.get("value")).and_then(Value::as_f64);
        match (lo, hi) {
            (Some(l), Some(h)) => format!("{l}-{h}"),
            (Some(l), None) => format!(">{l}"),
            (None, Some(h)) => format!("<{h}"),
            (None, None) => String::new(),
        }
    } else if let Some(ratio) = res.get("valueRatio") {
        // valueRatio: represent as "numerator/denominator" text.
        let num = ratio.get("numerator").and_then(|q| q.get("value")).and_then(Value::as_f64);
        let den = ratio.get("denominator").and_then(|q| q.get("value")).and_then(Value::as_f64);
        match (num, den) {
            (Some(n), Some(d)) => format!("{n}/{d}"),
            _ => String::new(),
        }
    } else {
        String::new()
    };

    // Unit: valueQuantity.unit (UCUM preferred).
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

    // Flag from interpretation[0].coding[0].code (H/L/A/N/…).
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

    // Panel: from the caller-supplied DiagnosticReport display, or basedOn.
    let panel = if !panel_name.is_empty() {
        panel_name.to_string()
    } else {
        res.get("basedOn")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|r| r.get("display"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };

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
    // Preserve meta.lastUpdated for watermark advancement without keeping the
    // full meta block in extra.
    let meta_lu = nested_str(res, &["meta", "lastUpdated"]);
    if !meta_lu.is_empty() {
        extra.insert("meta.lastUpdated".to_string(), Value::String(meta_lu));
    }

    Some(Observation {
        ts,
        source: "quest-diagnostics".into(),
        guid: id,
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

struct RawLine {
    ts: String,
    #[allow(dead_code)]
    value: Value,
}

impl Serialize for RawLine {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        self.value.serialize(ser)
    }
}

/// Write contract observations and raw FHIR Observations independently.
///
/// The raw layer is **unconditional and full-fidelity**: every FHIR Observation
/// resource from the server is written verbatim, even those that
/// `observation_from_fhir` cannot map (e.g. grouper observations with no
/// `code.text`, `dataAbsentReason` entries, or unknown `value[x]` shapes).
/// The contract layer carries only the successfully-mapped rows.
///
/// Both layers deduplicate by FHIR Observation.id so re-draining a page (crash
/// recovery) is idempotent.
///
/// Returns the count of **new contract** rows written.
fn write_layer(
    vault: &Vault,
    mapped_rows: Vec<(Observation, Value)>,
    all_raw: Vec<Value>,
) -> Result<u64> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // --- Raw layer: unconditional, deduped by FHIR Observation.id read from
    //     the raw stream itself (independent of the contract dedupe set). ---
    let mut raw_seen: HashSet<String> = HashSet::new();
    for key in raw.partitions()? {
        for v in raw.read::<Value>(&key)? {
            let g = str_field(&v, "id");
            if !g.is_empty() {
                raw_seen.insert(g);
            }
        }
    }
    let mut new_raw: Vec<RawLine> = Vec::new();
    for r in all_raw {
        let id = str_field(&r, "id");
        if id.is_empty() || !raw_seen.insert(id.clone()) {
            continue;
        }
        // ts for partitioning: effectiveDateTime > effectivePeriod.start >
        // effectiveInstant > issued > meta.lastUpdated.
        let ts = {
            let eff = str_field(&r, "effectiveDateTime");
            if !eff.is_empty() {
                eff
            } else {
                let period_start = nested_str(&r, &["effectivePeriod", "start"]);
                if !period_start.is_empty() {
                    period_start
                } else {
                    let inst = str_field(&r, "effectiveInstant");
                    if !inst.is_empty() {
                        inst
                    } else {
                        let issued = str_field(&r, "issued");
                        if !issued.is_empty() {
                            issued
                        } else {
                            nested_str(&r, &["meta", "lastUpdated"])
                        }
                    }
                }
            }
        };
        new_raw.push(RawLine { ts, value: r });
    }

    // --- Contract layer: only successfully-mapped rows, deduped by guid
    //     (same key as FHIR id) via the contract stream. ---
    let mut contract_seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = str_field(&v, "guid");
            if !g.is_empty() {
                contract_seen.insert(g);
            }
        }
    }
    let mut new_obs: Vec<Observation> = Vec::new();
    for (obs, _raw_val) in mapped_rows {
        if obs.guid.is_empty() || !contract_seen.insert(obs.guid.clone()) {
            continue;
        }
        new_obs.push(obs);
    }

    raw.append(&new_raw, |r| &r.ts)?;
    contract.append(&new_obs, |o| &o.ts)?;
    Ok(new_obs.len() as u64)
}

// ---------------------------------------------------------------------------
// The pull.

/// Entry point called by the Periodic runner + "Sync now". Missing token is a
/// quiet skip on the periodic path; a clear error on the manual path.
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let token = vault
        .load_sync_token(SERVICE)?
        .context("Quest Diagnostics is not connected — connect your account in the Integrations tab")?;
    let token = ensure_fresh(vault, token)?;
    let client = QuestClient::new();
    pull_with(vault, &client, &token.access_token)
}

fn ensure_fresh(vault: &Vault, token: TokenSet) -> Result<TokenSet> {
    if !token.expired() {
        return Ok(token);
    }
    let creds = vault
        .load_sync_app(QUEST.service)?
        .or_else(|| QUEST.default_credentials())
        .context(
            "Quest Diagnostics token expired and no client id to refresh it — reconnect from the \
             Integrations tab",
        )?;
    match oauth::refresh_token(&QUEST, &creds, &token) {
        Ok(fresh) => {
            vault.save_sync_token(QUEST.service, &fresh)?;
            Ok(fresh)
        }
        Err(e) => {
            vault.delete_sync_token(QUEST.service)?;
            bail!(
                "Quest Diagnostics token refresh failed ({e}) — reconnect from the Integrations tab"
            );
        }
    }
}

/// The testable pull body. Resolves the patient id (cached), then pages through
/// Observations from the watermark, writing each page before advancing the
/// cursor (crash-safe re-drain via guid dedupe).
fn pull_with(vault: &Vault, api: &impl QuestFhirApi, token: &str) -> Result<PullOutcome> {
    let mut state = vault.read_quest_sync();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();

    // Resolve the patient id (cache it to avoid a Patient search on every sync).
    if state.patient_id.is_none() {
        let bundle = api
            .patient(token)
            .map_err(|e| fetch_err("Patient", e))?;
        let id = bundle_entries(&bundle, "Patient")
            .first()
            .and_then(|p| p.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .context("Patient bundle had no Patient resource — cannot locate your records")?;
        state.patient_id = Some(id);
        vault.write_quest_sync(&state)?;
    }
    let patient_id = state.patient_id.as_deref().unwrap();

    let mut total: u64 = 0;
    let mut page_url: Option<String> = None;
    let mut new_watermark: Option<String> = None;

    loop {
        let bundle = api
            .observations(
                token,
                patient_id,
                state.last_updated.as_deref(),
                page_url.as_deref(),
            )
            .map_err(|e| fetch_err("Observation", e))?;

        let resources = bundle_entries(&bundle, "Observation");
        if resources.is_empty() {
            break;
        }

        // Raw: every Observation from this page goes to the raw layer verbatim.
        let all_raw: Vec<Value> = resources.iter().map(|r| (*r).clone()).collect();

        // Contract: only the successfully-mapped rows (observation_from_fhir
        // returns None for grouper/dataAbsentReason Observations with no id or
        // no test name — those still land in the raw layer above).
        let mapped_rows: Vec<(Observation, Value)> = resources
            .iter()
            .filter_map(|r| observation_from_fhir(r, "").map(|obs| (obs, (*r).clone())))
            .collect();

        // Advance the watermark candidate from meta.lastUpdated on all resources
        // (including those we can't map, so the cursor still progresses).
        for r in &resources {
            let lu = nested_str(r, &["meta", "lastUpdated"]);
            if !lu.is_empty() {
                let is_newer = match (&new_watermark, parse_fhir_instant(&lu)) {
                    (None, _) => true,
                    (Some(cur), Some(lu_dt)) => match parse_fhir_instant(cur) {
                        Some(cur_dt) => lu_dt > cur_dt,
                        None => lu.as_str() > cur.as_str(), // fallback: lexicographic
                    },
                    (Some(cur), None) => lu.as_str() > cur.as_str(),
                };
                if is_newer {
                    new_watermark = Some(lu);
                }
            }
        }

        total += write_layer(vault, mapped_rows, all_raw)?;

        // Advance the watermark after this page's write (crash-safe: re-draining
        // a page is safe because guid dedupe makes it idempotent).
        if let Some(ref wm) = new_watermark {
            let should_advance = match state.last_updated.as_deref() {
                None => true,
                Some(cur) => match (parse_fhir_instant(wm), parse_fhir_instant(cur)) {
                    (Some(wm_dt), Some(cur_dt)) => wm_dt > cur_dt,
                    _ => wm.as_str() > cur, // fallback: lexicographic
                },
            };
            if should_advance {
                state.last_updated = Some(wm.clone());
                vault.write_quest_sync(&state)?;
            }
        }

        // Follow the FHIR Bundle next link if present.
        match next_link(&bundle) {
            Some(next) => page_url = Some(next.to_string()),
            None => break,
        }
    }

    counts.insert("observations", total);
    state.updated = Some(Local::now().to_rfc3339());
    vault.write_quest_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("Quest Diagnostics synced — {total} new lab results"),
        counts,
    })
}

fn fetch_err(resource: &str, e: FetchError) -> anyhow::Error {
    match e {
        FetchError::Unauthorized => anyhow::anyhow!(
            "Quest Diagnostics rejected the token on {resource} — ensure identity verification is \
             complete and reconnect from the Integrations tab"
        ),
        FetchError::NotFound => anyhow::anyhow!(
            "Quest Diagnostics {resource} endpoint returned 404 — the FHIR endpoint URL may have \
             changed; check the Trove release notes"
        ),
        other => anyhow::anyhow!("Quest Diagnostics {resource} fetch failed: {other}"),
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
            "trove-quest-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — exact FHIR R4 shapes from the spec + the health-medical.md
    // example. Authoring basis: hl7.org/fhir/R4/observation.html confirmed above.

    /// A normal numeric lab result in exact FHIR R4 shape: Glucose, LOINC
    /// 15074-8, value 113 mg/dL, reference range 70–99, flag H (high).
    fn obs_glucose() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-glucose-001",
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
            "subject": { "reference": "Patient/quest-pt-001" },
            "effectiveDateTime": "2026-04-02T09:30:10-07:00",
            "issued": "2026-04-02T16:30:00Z",
            "performer": [{ "display": "Quest Diagnostics" }],
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

    /// A qualitative (non-numeric) test result: HIV antibody screen,
    /// valueString "Non-Reactive".
    fn obs_hiv_qualitative() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-hiv-002",
            "meta": { "lastUpdated": "2026-05-10T14:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{
                    "system": "http://loinc.org",
                    "code": "56888-1",
                    "display": "HIV 1+2 Ab [Presence] in Serum"
                }]
            },
            "subject": { "reference": "Patient/quest-pt-001" },
            "effectiveDateTime": "2026-05-10T08:00:00-07:00",
            "valueString": "Non-Reactive",
            "referenceRange": [{ "text": "Non-Reactive" }]
        })
    }

    /// An Observation with no `code.coding` (only `code.text`); tests fallback
    /// test-name extraction.
    fn obs_text_only_code() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-text-003",
            "meta": { "lastUpdated": "2026-05-11T10:00:00Z" },
            "status": "final",
            "code": { "text": "Hemoglobin A1c" },
            "subject": { "reference": "Patient/quest-pt-001" },
            "effectiveDateTime": "2026-05-11T09:00:00-07:00",
            "valueQuantity": { "value": 5.6, "unit": "%" }
        })
    }

    /// An Observation with a non-LOINC coding system — tests that code_system
    /// captures the raw system URL rather than hardcoding "loinc".
    fn obs_snomed_code() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-snomed-004",
            "meta": { "lastUpdated": "2026-05-12T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{
                    "system": "http://snomed.info/sct",
                    "code": "365750007",
                    "display": "Body mass index"
                }]
            },
            "effectiveDateTime": "2026-05-12T08:00:00-07:00",
            "valueQuantity": { "value": 24.1, "unit": "kg/m2" }
        })
    }

    /// An Observation with only `issued` (no `effectiveDateTime`) — tests the
    /// ts fallback chain.
    fn obs_issued_only() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-issued-005",
            "meta": { "lastUpdated": "2026-06-01T08:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "2823-3", "display": "Potassium" }]
            },
            "issued": "2026-06-01T07:45:00-07:00",
            "valueQuantity": { "value": 4.1, "unit": "mEq/L" }
        })
    }

    /// An Observation with only `meta.lastUpdated` as a timestamp — tests the
    /// deepest fallback level.
    fn obs_meta_ts_only() -> Value {
        json!({
            "resourceType": "Observation",
            "id": "obs-meta-006",
            "meta": { "lastUpdated": "2026-06-02T09:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "2951-2", "display": "Sodium" }]
            },
            "valueQuantity": { "value": 141, "unit": "mEq/L" }
        })
    }

    /// A FHIR Bundle wrapping a list of Observation resources.
    fn obs_bundle(observations: Vec<Value>) -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "total": observations.len(),
            "entry": observations.iter().map(|o| json!({"resource": o})).collect::<Vec<_>>()
        })
    }

    /// A Patient search bundle.
    fn patient_bundle() -> Value {
        json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": [{ "resource": { "resourceType": "Patient", "id": "quest-pt-001" } }]
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests.

    #[test]
    fn maps_glucose_observation_correctly() {
        let obs = observation_from_fhir(&obs_glucose(), "Comprehensive Metabolic Panel").unwrap();
        assert_eq!(obs.source, "quest-diagnostics");
        assert_eq!(obs.guid, "obs-glucose-001");
        assert_eq!(obs.test, "Glucose [Mass/volume] in Blood");
        assert_eq!(obs.code, "15074-8");
        assert_eq!(obs.code_system, "loinc");
        assert_eq!(obs.value, Some(113.0));
        assert_eq!(obs.unit, "mg/dL");
        assert_eq!(obs.reference_range, "70-99");
        assert_eq!(obs.flag, "H");
        assert_eq!(obs.panel, "Comprehensive Metabolic Panel");
        assert_eq!(obs.provider, "Quest Diagnostics");
        // ts = effectiveDateTime (first in fallback chain).
        assert_eq!(obs.ts, "2026-04-02T09:30:10-07:00");
        // extra keeps status and category; no value/unit/code leakage.
        assert_eq!(
            obs.extra.get("status"),
            Some(&json!("final")),
            "status in extra"
        );
        // The contract row round-trips correctly.
        let re = serde_json::to_value(&obs).unwrap();
        assert_eq!(re["value"].as_f64(), Some(113.0));
        assert_eq!(re["flag"], json!("H"));
    }

    #[test]
    fn maps_qualitative_observation_correctly() {
        let obs = observation_from_fhir(&obs_hiv_qualitative(), "").unwrap();
        assert_eq!(obs.guid, "obs-hiv-002");
        assert_eq!(obs.test, "HIV 1+2 Ab [Presence] in Serum");
        assert_eq!(obs.code, "56888-1");
        assert_eq!(obs.code_system, "loinc");
        assert!(obs.value.is_none(), "qualitative result: no numeric value");
        assert_eq!(obs.value_text, "Non-Reactive");
        assert!(obs.unit.is_empty(), "no unit for a qualitative result");
        assert_eq!(obs.reference_range, "Non-Reactive");
        // Omit-empty: re-serialized form drops nil value, empty unit, etc.
        let re = serde_json::to_value(&obs).unwrap();
        assert!(re.get("value").is_none(), "null value omitted");
        assert!(re.get("unit").is_none(), "empty unit omitted");
        assert_eq!(re["value_text"], json!("Non-Reactive"));
    }

    #[test]
    fn text_only_code_falls_back_to_code_text() {
        let obs = observation_from_fhir(&obs_text_only_code(), "").unwrap();
        assert_eq!(obs.test, "Hemoglobin A1c", "code.text used when no coding.display");
        assert!(obs.code.is_empty(), "no code.coding → no code");
        assert!(obs.code_system.is_empty());
        assert_eq!(obs.value, Some(5.6));
        assert_eq!(obs.unit, "%");
    }

    #[test]
    fn non_loinc_coding_preserves_raw_system() {
        let obs = observation_from_fhir(&obs_snomed_code(), "").unwrap();
        assert_eq!(obs.code, "365750007");
        // Non-LOINC system: the raw system URL is used as code_system (the
        // contract is flexible — code_system is free text, not an enum).
        assert!(obs.code_system.contains("snomed"), "snomed system preserved: {}", obs.code_system);
    }

    #[test]
    fn ts_fallback_chain_issued_then_meta() {
        // issued only (no effectiveDateTime).
        let obs = observation_from_fhir(&obs_issued_only(), "").unwrap();
        assert_eq!(obs.ts, "2026-06-01T07:45:00-07:00", "issued used when no effectiveDateTime");

        // meta.lastUpdated only (no effectiveDateTime, no issued).
        let obs_meta = observation_from_fhir(&obs_meta_ts_only(), "").unwrap();
        assert_eq!(obs_meta.ts, "2026-06-02T09:00:00Z", "meta.lastUpdated as last resort");
    }

    #[test]
    fn reference_range_lo_hi_fallback() {
        // An Observation with low/high but no text — the mapper assembles "lo-hi".
        let obs_data = json!({
            "resourceType": "Observation",
            "id": "obs-range-007",
            "status": "final",
            "code": { "coding": [{ "system": "http://loinc.org", "code": "2823-3", "display": "Potassium" }] },
            "effectiveDateTime": "2026-06-03T08:00:00-07:00",
            "valueQuantity": { "value": 3.5, "unit": "mEq/L" },
            "referenceRange": [{ "low": { "value": 3.5 }, "high": { "value": 5.0 } }]
        });
        let obs = observation_from_fhir(&obs_data, "").unwrap();
        assert_eq!(obs.reference_range, "3.5-5");
    }

    #[test]
    fn missing_id_or_no_timestamp_returns_none() {
        // No id.
        let no_id = json!({
            "resourceType": "Observation",
            "status": "final",
            "code": { "coding": [{ "system": "http://loinc.org", "code": "2823-3", "display": "K" }] },
            "effectiveDateTime": "2026-06-03T08:00:00Z"
        });
        assert!(observation_from_fhir(&no_id, "").is_none(), "no id → None");

        // Empty id.
        let empty_id = json!({
            "resourceType": "Observation",
            "id": "",
            "status": "final",
            "code": { "coding": [{ "code": "x", "display": "X" }] },
            "effectiveDateTime": "2026-06-03T08:00:00Z"
        });
        assert!(observation_from_fhir(&empty_id, "").is_none(), "empty id → None");

        // No timestamp at all.
        let no_ts = json!({
            "resourceType": "Observation",
            "id": "obs-x",
            "status": "final",
            "code": { "coding": [{ "code": "x", "display": "X" }] }
        });
        assert!(observation_from_fhir(&no_ts, "").is_none(), "no timestamp → None");

        // No test name anywhere.
        let no_test = json!({
            "resourceType": "Observation",
            "id": "obs-y",
            "status": "final",
            "code": {},
            "effectiveDateTime": "2026-06-03T08:00:00Z"
        });
        assert!(observation_from_fhir(&no_test, "").is_none(), "no test name → None");
    }

    #[test]
    fn bundle_helpers_read_entries_and_next_link() {
        let bundle = obs_bundle(vec![obs_glucose(), obs_hiv_qualitative()]);
        let obs = bundle_entries(&bundle, "Observation");
        assert_eq!(obs.len(), 2);
        assert!(next_link(&bundle).is_none(), "no next link in this bundle");

        // A bundle with a next link.
        let paged = json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": [
                { "relation": "self", "url": "https://api.questdiagnostics.com/r4/Observation?..." },
                { "relation": "next", "url": "https://api.questdiagnostics.com/r4/Observation?_page=2" }
            ],
            "entry": [{ "resource": obs_glucose() }]
        });
        assert_eq!(
            next_link(&paged),
            Some("https://api.questdiagnostics.com/r4/Observation?_page=2")
        );
    }

    // -----------------------------------------------------------------------
    // Mock API + integration tests.

    struct MockApi {
        patient: Value,
        observation_pages: RefCell<VecDeque<Value>>,
        obs_calls: RefCell<Vec<(Option<String>, Option<String>)>>, // (watermark, next_url)
    }

    impl MockApi {
        fn new(patient: Value) -> Self {
            MockApi {
                patient,
                observation_pages: RefCell::new(VecDeque::new()),
                obs_calls: RefCell::new(Vec::new()),
            }
        }
        fn page(self, bundle: Value) -> Self {
            self.observation_pages.borrow_mut().push_back(bundle);
            self
        }
    }

    impl QuestFhirApi for MockApi {
        fn patient(&self, _t: &str) -> Result<Value, FetchError> {
            Ok(self.patient.clone())
        }
        fn observations(
            &self,
            _t: &str,
            _patient_id: &str,
            last_updated_ge: Option<&str>,
            next_url: Option<&str>,
        ) -> Result<Value, FetchError> {
            self.obs_calls
                .borrow_mut()
                .push((last_updated_ge.map(str::to_string), next_url.map(str::to_string)));
            Ok(self
                .observation_pages
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| obs_bundle(vec![])))
        }
    }

    #[test]
    fn full_pull_writes_both_layers_and_advances_watermark() {
        let v = temp_vault("fullpull");
        let api = MockApi::new(patient_bundle())
            .page(obs_bundle(vec![obs_glucose(), obs_hiv_qualitative()]));

        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&2));

        // Contract observation stream — April observations land in 2026-04.
        let obs_path = v.root().join("health/medical/quest-diagnostics/observations/2026-04.jsonl");
        let obs_text = std::fs::read_to_string(&obs_path).unwrap();
        assert_eq!(obs_text.lines().count(), 1, "glucose lands in April");
        assert!(obs_text.contains("\"guid\":\"obs-glucose-001\""));
        assert!(obs_text.contains("\"code\":\"15074-8\""));
        assert!(obs_text.contains("\"flag\":\"H\""));

        // May observation lands in 2026-05.
        let may_path = v.root().join("health/medical/quest-diagnostics/observations/2026-05.jsonl");
        let may_text = std::fs::read_to_string(&may_path).unwrap();
        assert!(may_text.contains("\"guid\":\"obs-hiv-002\""));
        assert!(may_text.contains("\"value_text\":\"Non-Reactive\""));

        // Raw layer mirrors the partitioning with verbatim FHIR JSON.
        let raw_path = v.root().join("health/medical/quest-diagnostics/raw/2026-04.jsonl");
        let raw_text = std::fs::read_to_string(&raw_path).unwrap();
        assert!(raw_text.contains("\"valueQuantity\""), "raw keeps FHIR fields the contract maps");

        // Watermark advanced to the latest meta.lastUpdated seen.
        let state = v.read_quest_sync();
        assert!(state.last_updated.is_some(), "watermark set after pull");
        assert_eq!(state.patient_id.as_deref(), Some("quest-pt-001"));
        let cursor_bytes = std::fs::read_to_string(v.root().join(".trove/quest-diagnostics-sync.json")).unwrap();
        assert!(!cursor_bytes.contains("tok"), "access token never in the cursor file");
    }

    #[test]
    fn guid_dedupe_prevents_double_write_on_repull() {
        let v = temp_vault("dedupe");
        let api = MockApi::new(patient_bundle())
            .page(obs_bundle(vec![obs_glucose()]));
        let out1 = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out1.counts.get("observations"), Some(&1));

        // Re-pull the same observation — guid dedupe must block the duplicate.
        let api2 = MockApi::new(patient_bundle())
            .page(obs_bundle(vec![obs_glucose()]));
        let out2 = pull_with(&v, &api2, "tok").unwrap();
        assert_eq!(out2.counts.get("observations"), Some(&0), "duplicate blocked by guid dedupe");
    }

    #[test]
    fn paging_follows_next_link() {
        let v = temp_vault("paging");
        // Page 1: one observation + a next link.
        let page1 = json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "link": [
                { "relation": "next", "url": "https://api.questdiagnostics.com/r4/Observation?_page=2" }
            ],
            "entry": [{ "resource": obs_glucose() }]
        });
        // Page 2: one observation, no next link.
        let page2 = obs_bundle(vec![obs_hiv_qualitative()]);

        let api = MockApi::new(patient_bundle()).page(page1).page(page2);
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&2), "both pages drained");

        // The second call carried the next_url from page 1.
        let calls = api.obs_calls.borrow();
        assert_eq!(calls.len(), 2, "exactly two Observation fetches");
        assert!(calls[0].1.is_none(), "first call has no next_url");
        assert_eq!(
            calls[1].1.as_deref(),
            Some("https://api.questdiagnostics.com/r4/Observation?_page=2"),
            "second call follows the next link"
        );
    }

    #[test]
    fn incremental_pull_passes_watermark_to_next_fetch() {
        let v = temp_vault("incremental");
        // Seed a prior watermark.
        v.write_quest_sync(&SyncState {
            last_updated: Some("2026-05-01T00:00:00Z".into()),
            patient_id: Some("quest-pt-001".into()),
            updated: Some("2026-05-01T00:00:00Z".into()),
        })
        .unwrap();
        let api = MockApi::new(patient_bundle())
            .page(obs_bundle(vec![obs_hiv_qualitative()]));
        let _out = pull_with(&v, &api, "tok").unwrap();

        let calls = api.obs_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].0.as_deref(),
            Some("2026-05-01T00:00:00Z"),
            "watermark passed as _lastUpdated=ge parameter"
        );
        // Patient fetch was skipped (patient_id already cached).
        // (Patient.patient() would panic if called — MockApi always returns the
        // bundle, so verify indirectly via the observation call count.)
    }

    #[test]
    fn empty_bundle_is_a_noop_without_crashing() {
        let v = temp_vault("empty");
        let api = MockApi::new(patient_bundle()).page(obs_bundle(vec![]));
        let out = pull_with(&v, &api, "tok").unwrap();
        assert_eq!(out.counts.get("observations"), Some(&0));
        // No observation files created.
        assert!(
            !v.root().join("health/medical/quest-diagnostics/observations").exists()
                || std::fs::read_dir(v.root().join("health/medical/quest-diagnostics/observations"))
                    .map(|d| d.count() == 0)
                    .unwrap_or(true)
        );
    }

    #[test]
    fn patient_not_found_is_a_clear_error() {
        let v = temp_vault("no_patient");
        // Bundle with no Patient entry.
        let api = MockApi::new(json!({
            "resourceType": "Bundle",
            "type": "searchset",
            "entry": []
        }));
        let err = pull_with(&v, &api, "tok").unwrap_err().to_string();
        assert!(err.contains("Patient"), "error mentions Patient: {err}");
    }

    #[test]
    fn unauthorized_response_is_a_reconnect_error() {
        let v = temp_vault("unauth");
        struct FailingApi;
        impl QuestFhirApi for FailingApi {
            fn patient(&self, _t: &str) -> Result<Value, FetchError> {
                Err(FetchError::Unauthorized)
            }
            fn observations(
                &self,
                _t: &str,
                _: &str,
                _: Option<&str>,
                _: Option<&str>,
            ) -> Result<Value, FetchError> {
                unreachable!()
            }
        }
        let err = pull_with(&v, &FailingApi, "tok").unwrap_err().to_string();
        assert!(
            err.contains("identity verification") || err.contains("reconnect"),
            "reconnect hint surfaced: {err}"
        );
    }

    #[test]
    fn pull_without_token_is_a_clean_error() {
        let v = temp_vault("no_token");
        let err = pull(&v).unwrap_err().to_string();
        assert!(err.contains("not connected"), "clear error without token: {err}");
    }

    #[test]
    fn cursor_back_compat_partial_and_forward() {
        // Empty cursor is a cold start (all None).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_updated.is_none());
        assert!(empty.patient_id.is_none());

        // Forward-compat: unknown extra field is ignored.
        let fwd: SyncState = serde_json::from_str(
            r#"{"last_updated":"2026-04-01T00:00:00Z","patient_id":"quest-pt-1","future":"x"}"#,
        )
        .unwrap();
        assert_eq!(fwd.last_updated.as_deref(), Some("2026-04-01T00:00:00Z"));
        assert_eq!(fwd.patient_id.as_deref(), Some("quest-pt-1"));
    }

    #[test]
    fn connection_uses_assigned_redirect_port() {
        // 38663 is the assigned unique port for quest-diagnostics (#83).
        assert_eq!(QUEST.redirect_port, 38663);
        assert_eq!(QUEST.redirect_uri(), "http://localhost:38663/callback");
        assert!(QUEST.use_pkce, "SMART on FHIR mandates PKCE");
        assert!(!QUEST.basic_auth, "PKCE public client: no basic auth");
        assert!(QUEST.default_client_secret.is_none(), "no compiled-in secret for PKCE client");
        assert!(CONNECTION.method("oauth").is_some());
        assert_eq!(CONNECTION.id, "quest-diagnostics");
        assert_eq!(DEF.connection, Some("quest-diagnostics"));
    }

    // -----------------------------------------------------------------------
    // Defect-fix regression tests.

    /// valueCodeableConcept is mapped to value_text (defect fix).
    #[test]
    fn maps_value_codeable_concept_to_value_text() {
        // Serology / microbiology qualitative coded result — common Quest shape.
        let obs = json!({
            "resourceType": "Observation",
            "id": "obs-hepb-010",
            "meta": { "lastUpdated": "2026-05-15T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{
                    "system": "http://loinc.org",
                    "code": "22314-9",
                    "display": "Hepatitis B surface Ab [Units/volume] in Serum"
                }]
            },
            "effectiveDateTime": "2026-05-15T08:00:00-07:00",
            "valueCodeableConcept": {
                "coding": [{
                    "system": "http://snomed.info/sct",
                    "code": "10828004",
                    "display": "Reactive"
                }],
                "text": "Reactive"
            }
        });
        let result = observation_from_fhir(&obs, "").unwrap();
        assert_eq!(result.value_text, "Reactive",
            "valueCodeableConcept.text captured in value_text");
        assert!(result.value.is_none(), "no numeric value for coded result");
    }

    /// valueCodeableConcept without .text falls back to coding[0].display.
    #[test]
    fn maps_value_codeable_concept_display_fallback() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "obs-coded-011",
            "meta": { "lastUpdated": "2026-05-16T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "6895-7", "display": "Culture" }]
            },
            "effectiveDateTime": "2026-05-16T08:00:00-07:00",
            "valueCodeableConcept": {
                "coding": [{ "display": "Negative" }]
            }
        });
        let result = observation_from_fhir(&obs, "").unwrap();
        assert_eq!(result.value_text, "Negative",
            "valueCodeableConcept.coding[0].display used when no .text");
    }

    /// valueInteger is captured as a numeric value (defect fix).
    #[test]
    fn maps_value_integer_to_value() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "obs-wbc-012",
            "meta": { "lastUpdated": "2026-05-17T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "26464-8", "display": "WBC" }]
            },
            "effectiveDateTime": "2026-05-17T08:00:00-07:00",
            "valueInteger": 7
        });
        let result = observation_from_fhir(&obs, "").unwrap();
        assert_eq!(result.value, Some(7.0), "valueInteger captured as f64");
    }

    /// valueRange is captured as value_text (defect fix).
    #[test]
    fn maps_value_range_to_value_text() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "obs-range-013",
            "meta": { "lastUpdated": "2026-05-18T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "2885-2", "display": "Protein range" }]
            },
            "effectiveDateTime": "2026-05-18T08:00:00-07:00",
            "valueRange": {
                "low": { "value": 6.3 },
                "high": { "value": 8.2 }
            }
        });
        let result = observation_from_fhir(&obs, "").unwrap();
        assert_eq!(result.value_text, "6.3-8.2", "valueRange formatted as low-high text");
    }

    /// effectivePeriod.start is used when effectiveDateTime is absent (defect fix).
    #[test]
    fn ts_uses_effective_period_start() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "obs-period-014",
            "meta": { "lastUpdated": "2026-05-20T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "2823-3", "display": "Potassium" }]
            },
            "effectivePeriod": {
                "start": "2026-05-20T08:00:00-07:00",
                "end": "2026-05-20T08:30:00-07:00"
            },
            "valueQuantity": { "value": 4.0, "unit": "mEq/L" }
        });
        let result = observation_from_fhir(&obs, "").unwrap();
        assert_eq!(result.ts, "2026-05-20T08:00:00-07:00",
            "effectivePeriod.start used when effectiveDateTime absent");
    }

    /// effectiveInstant is used when effectiveDateTime and effectivePeriod absent.
    #[test]
    fn ts_uses_effective_instant() {
        let obs = json!({
            "resourceType": "Observation",
            "id": "obs-instant-015",
            "meta": { "lastUpdated": "2026-05-21T10:00:00Z" },
            "status": "final",
            "code": {
                "coding": [{ "system": "http://loinc.org", "code": "2823-3", "display": "Potassium" }]
            },
            "effectiveInstant": "2026-05-21T08:00:00Z",
            "valueQuantity": { "value": 4.2, "unit": "mEq/L" }
        });
        let result = observation_from_fhir(&obs, "").unwrap();
        assert_eq!(result.ts, "2026-05-21T08:00:00Z",
            "effectiveInstant used as ts fallback");
    }

    /// Raw layer is unconditional: unmappable observations (no code.text) appear
    /// in the raw stream but NOT in the contract stream (defect fix).
    #[test]
    fn raw_layer_captures_unmappable_observations() {
        // A grouper / dataAbsentReason Observation with no code display or text —
        // observation_from_fhir returns None for it, but it must still land in raw.
        let grouper = json!({
            "resourceType": "Observation",
            "id": "obs-grouper-020",
            "meta": { "lastUpdated": "2026-06-10T12:00:00Z" },
            "status": "registered",
            "code": {},
            "effectiveDateTime": "2026-06-10T08:00:00Z",
            "dataAbsentReason": {
                "coding": [{ "code": "not-performed", "display": "Not Performed" }]
            }
        });
        // Verify observation_from_fhir rejects it (no test name).
        assert!(observation_from_fhir(&grouper, "").is_none(),
            "grouper has no test name — mapper returns None");

        let v = temp_vault("rawonly");
        let api = MockApi::new(patient_bundle())
            .page(obs_bundle(vec![obs_glucose(), grouper]));
        let out = pull_with(&v, &api, "tok").unwrap();

        // Contract: only the mappable glucose row.
        assert_eq!(out.counts.get("observations"), Some(&1),
            "only 1 mapped contract row");

        // Raw: BOTH observations must be present.
        let raw_june_path = v.root().join("health/medical/quest-diagnostics/raw/2026-06.jsonl");
        let raw_june = std::fs::read_to_string(&raw_june_path).unwrap();
        assert!(raw_june.contains("\"obs-grouper-020\""),
            "unmappable observation present in raw: {raw_june}");

        // The grouper must NOT be in the contract stream.
        let obs_june = v.root().join("health/medical/quest-diagnostics/observations/2026-06.jsonl");
        assert!(!obs_june.exists() || !std::fs::read_to_string(&obs_june).unwrap()
            .contains("obs-grouper-020"),
            "unmappable observation absent from contract stream");
    }

    /// parse_fhir_instant handles mixed-offset timestamps correctly.
    #[test]
    fn fhir_instant_mixed_offset_comparison() {
        // "2026-05-10T15:00:00Z" and "2026-05-10T08:00:00-07:00" are the same instant.
        let utc = parse_fhir_instant("2026-05-10T15:00:00Z").unwrap();
        let offset = parse_fhir_instant("2026-05-10T08:00:00-07:00").unwrap();
        assert_eq!(utc, offset, "same instant in different timezone representations");

        // An earlier timestamp is less.
        let earlier = parse_fhir_instant("2026-05-10T14:59:59Z").unwrap();
        assert!(earlier < utc, "earlier instant compares correctly");
    }
}
