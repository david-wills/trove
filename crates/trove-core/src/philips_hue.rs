//! Philips Hue smart lighting — local bridge CLIP v2 API (LAN, zero cloud dependency).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/philips-hue.md
//!
//! A **Periodic** LAN poll against the Hue CLIP v2 REST API running on the
//! bridge at `https://<bridge-ip>/clip/v2/resource/{type}`. Auth is a one-time
//! physical link-button press that mints a username token; the token rides in
//! the `hue-application-key` header on every subsequent request.
//!
//! **Two layers, unconditional:**
//!
//! - **Raw** — full bridge state snapshots at `home/philips-hue/raw/YYYY-MM.jsonl`
//!   (lights, motion sensors, temperature sensors, rooms/zones, grouped lights).
//!   Full fidelity, every field the bridge returned.
//! - **Contract** — numeric sensors fan out into [`crate::home::HomeReading`]
//!   rows at `home/philips-hue/YYYY-MM.jsonl` (temperature only; motion is an
//!   event, not a reading). Motion sensors (presence=true) fan out into plain
//!   `Value` event rows at `home/philips-hue/events/YYYY-MM.jsonl` per the
//!   `home.event` schema (`ts`, `source`, `device`, `event="motion"`, `guid`).
//!
//! **No history endpoint.** The bridge exposes current state only; Trove builds
//! the history by polling. Gaps while the app or daemon is asleep are honest
//! — never invented.
//!
//! **Watermark:** the bridge has no event cursor; we watermark by the last
//! poll time and write every snapshot. Temperature readings are deduped by
//! `guid = philips-hue:{sensor-id}:{ts-minute}` (minute-resolution avoids
//! flooding on back-to-back polls). Motion events are deduped by
//! `guid = philips-hue:motion:{sensor-id}:{ts-minute}`.
//!
//! **TLS:** The bridge ships a self-signed certificate. We build a custom
//! rustls `ClientConfig` that accepts any cert — this is intentional and only
//! applies to the bridge's own IP address (the token is scoped to that bridge
//! anyway). Users who want cert pinning can upgrade to PKCS12 import later.
//!
//! **Auth** is a secret (bridge IP + username): pasted via
//! [`ConnectMethod::TokenPaste`] as `ip|username`, stored under `.trove/sync/`
//! (0600). The link-button pairing step is done externally (the user presses
//! the button and calls the pairing endpoint themselves, then pastes the
//! resulting username here). A future UI wizard could automate this.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

/// Contract reading stream (temperature sensor readings).
const DIR: &str = "home/philips-hue";
/// Home events (motion presence events).
const EVENTS_DIR: &str = "home/philips-hue/events";
/// Raw snapshots — full bridge state, unconditional.
const RAW_DIR: &str = "home/philips-hue/raw";
/// Non-secret rebuildable cursor. NOT under `.trove/sync/` (that's 0600 secrets).
const SYNC_FILE: &str = ".trove/philips-hue-sync.json";
/// Secret-store service id for the stored `ip|username` credential pair.
const SERVICE: &str = "philips-hue";

/// Seconds between syncs. Polling hourly is fine; the bridge state changes
/// infrequently and there is no rate limit for personal LAN use.
pub const HUE_SYNC_SECS: u64 = 3600;

/// Hard timeout for any single HTTP request. The bridge is LAN-local so 10 s
/// is generous; a longer timeout would stall the watcher loop.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Cursor — non-secret, rebuildable.

/// Persisted cursor: tracks the last poll time so we can compute minute-level
/// dedupe keys consistently across runs.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 of the last successful sync (or empty on first run).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    last_synced: String,
}

impl Vault {
    /// Read the cursor (used in tests to verify the watermark was advanced).
    #[allow(dead_code)]
    fn read_hue_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_hue_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Periodic pass: a connectivity failure (bridge offline, IP changed) is a
/// quiet skip — the loop must not error on a downed LAN device.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match pull(vault) {
        Ok(out) => {
            let readings = out.counts.get("readings").copied().unwrap_or(0);
            let events = out.counts.get("events").copied().unwrap_or(0);
            Ok(crate::registry::CollectOutcome::note_if(
                readings + events > 0,
                || format!("Philips Hue synced — {readings} sensor readings, {events} motion events"),
            ))
        }
        Err(e) => {
            // Swallow LAN-connectivity failures (bridge unreachable, LAN offline).
            // Other errors (bad credentials, parse failure) propagate so the hub
            // surfaces them rather than silently zero-collecting.
            let is_connectivity = e.chain().any(|cause| {
                let s = cause.to_string();
                s.contains("unreachable")
                    || s.contains("connection refused")
                    || s.contains("timed out")
                    || s.contains("No route to host")
            });
            if is_connectivity {
                Ok(crate::registry::CollectOutcome::note(format!(
                    "Philips Hue sync skipped (bridge unreachable): {e}"
                )))
            } else {
                Err(e)
            }
        }
    }
}

/// Manual "Sync now": surfaces errors to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let readings = out.counts.get("readings").copied().unwrap_or(0);
    let events = out.counts.get("events").copied().unwrap_or(0);
    let headline = if readings == 0 && events == 0 {
        "Philips Hue is up to date — no new data".to_string()
    } else {
        format!("Philips Hue synced — {readings} sensor readings, {events} motion events")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "philips-hue",
        name: "Philips Hue",
        // Has a TokenPaste connection → must be CloudSync (even though the
        // bridge is LAN-local: connections_are_well_formed enforces this).
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description:
            "Polls your Philips Hue bridge over the local network for light state, \
             temperature sensor readings, and motion events. No cloud dependency — \
             all data stays on your LAN. Trove accumulates history by polling hourly.",
        domain: "home",
        vault_path: "home/philips-hue/",
        toggleable: true,
        setup: &[
            "Press the link button on your Hue bridge.",
            "Within 30 seconds, POST to https://<bridge-ip>/api with body \
             {\"devicetype\":\"trove#trove\"} to receive your username.",
            "Paste the bridge IP and the username below as ip|username \
             (e.g. 192.168.1.10|abc123def456).",
        ],
        caveats:
            "The bridge uses a self-signed TLS certificate — Trove accepts it by design. \
             The bridge keeps no history; gaps while Trove is asleep are honest. \
             Bridge IP must be static or reserved via DHCP.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(HUE_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("philips-hue"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection.

/// Parse `ip|username` from the pasted string.
fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!("empty — paste your bridge IP and username as ip|username");
    }
    let (ip, username) = pasted
        .split_once('|')
        .ok_or_else(|| anyhow::anyhow!("paste the bridge IP and username separated by |, e.g. 192.168.1.10|abc123def456"))?;
    let ip = ip.trim().to_string();
    let username = username.trim().to_string();
    if ip.is_empty() {
        bail!("missing bridge IP — paste as ip|username");
    }
    if username.is_empty() {
        bail!("missing username — paste as ip|username");
    }
    Ok((ip, username))
}

/// Verify the credentials by polling `/clip/v2/resource/bridge` (a lightweight
/// call that returns bridge info), then store the pair (0600).
fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (ip, username) = parse_credentials(pasted)?;
    let client = HueClient::new(ip.clone(), username.clone());
    client
        .bridge_info()
        .with_context(|| {
            format!(
                "Could not reach the Hue bridge at {ip} — check the IP, ensure the bridge is on, \
                 and that the username was obtained after pressing the link button."
            )
        })?;
    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: pasted.trim().to_string(),
            refresh_token: None,
            token_type: Some("HueLAN".into()),
            scope: None,
            expires_at: None,
        },
    )
}

/// Forget the stored credentials. Synced data and cursor stay in the vault.
fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

/// Connected = credentials are stored.
fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let display = if let Some((ip, _)) = token.access_token.split_once('|') {
            format!("Bridge at {}", ip.trim())
        } else {
            "Philips Hue Bridge".to_string()
        };
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: display,
            connected_at: None,
            expires_at: None, // username never expires
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "philips-hue",
    display_name: "Philips Hue",
    methods: &[ConnectMethod::TokenPaste {
        label: "Bridge IP and Username",
        help: "Press the link button on your bridge, then POST to https://<ip>/api with \
               body {\"devicetype\":\"trove#trove\"} to get a username. \
               Paste both as ip|username — stored locally, sent only to your bridge.",
        placeholder: "192.168.1.10|abc123def456abc123",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["philips-hue"],
    setup: &[
        "Press the physical link button on top of your Hue bridge.",
        "Within 30 seconds, POST to https://<bridge-ip>/api with body {\"devicetype\":\"trove#trove\"}.",
        "Copy the username from the response and paste it here as ip|username.",
    ],
};

// ---------------------------------------------------------------------------
// TLS: accept the bridge's self-signed certificate via a custom verifier.
//
// The bridge is a LAN device the user controls; the username token already
// proves identity. This is the same approach recommended in the Hue API docs.
// The verifier is intentionally minimal — we accept any cert presented by the
// bridge IP (no hostname checks either, since the bridge serves its own IP).

#[derive(Debug)]
struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Build a ureq agent that accepts self-signed TLS (the Hue bridge's cert).
fn hue_agent() -> ureq::Agent {
    let tls = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    ureq::AgentBuilder::new()
        .tls_config(Arc::new(tls))
        .timeout(HTTP_TIMEOUT)
        .build()
}

// ---------------------------------------------------------------------------
// HTTP client — injectable for offline tests.

/// Errors from the Hue bridge.
#[derive(Debug)]
enum HueError {
    Unauthorized,
    Unreachable(String),
    Other(String),
}

impl std::fmt::Display for HueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HueError::Unauthorized => write!(f, "unauthorized (check username/link-button pairing)"),
            HueError::Unreachable(m) => write!(f, "unreachable: {m}"),
            HueError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for HueError {}

/// The endpoints the pull needs. Trait so tests drive logic with fixtures.
trait HueApi {
    fn bridge_info(&self) -> Result<Value, HueError>;
    fn lights(&self) -> Result<Vec<Value>, HueError>;
    fn motion(&self) -> Result<Vec<Value>, HueError>;
    fn temperature(&self) -> Result<Vec<Value>, HueError>;
    fn rooms(&self) -> Result<Vec<Value>, HueError>;
    fn grouped_lights(&self) -> Result<Vec<Value>, HueError>;
}

/// Live HTTP client backed by ureq with the self-signed-cert-accepting agent.
struct HueClient {
    base: String,
    username: String,
}

impl HueClient {
    fn new(ip: String, username: String) -> Self {
        // Strip any trailing slash from ip, remove any protocol prefix.
        let ip = ip.trim_end_matches('/').to_string();
        let base = if ip.starts_with("http://") || ip.starts_with("https://") {
            ip
        } else {
            format!("https://{ip}")
        };
        HueClient { base, username }
    }

    fn get_resource(&self, resource_type: &str) -> Result<Vec<Value>, HueError> {
        let url = format!("{}/clip/v2/resource/{resource_type}", self.base);
        let resp = hue_agent()
            .get(&url)
            .set("hue-application-key", &self.username)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                    HueError::Unauthorized
                }
                ureq::Error::Transport(t) => HueError::Unreachable(t.to_string()),
                ureq::Error::Status(code, r) => {
                    HueError::Other(format!("HTTP {code}: {}", r.status_text()))
                }
            })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| HueError::Other(format!("JSON parse: {e}")))?;
        // CLIP v2 response: {"data": [...], "errors": [...]}
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(data)
    }
}

impl HueApi for HueClient {
    fn bridge_info(&self) -> Result<Value, HueError> {
        let url = format!("{}/clip/v2/resource/bridge", self.base);
        let resp = hue_agent()
            .get(&url)
            .set("hue-application-key", &self.username)
            .call()
            .map_err(|e| match e {
                ureq::Error::Status(401, _) | ureq::Error::Status(403, _) => {
                    HueError::Unauthorized
                }
                ureq::Error::Transport(t) => HueError::Unreachable(t.to_string()),
                ureq::Error::Status(code, r) => {
                    HueError::Other(format!("HTTP {code}: {}", r.status_text()))
                }
            })?;
        let body: Value = resp
            .into_json()
            .map_err(|e| HueError::Other(format!("JSON parse: {e}")))?;
        Ok(body)
    }

    fn lights(&self) -> Result<Vec<Value>, HueError> {
        self.get_resource("light")
    }

    fn motion(&self) -> Result<Vec<Value>, HueError> {
        self.get_resource("motion")
    }

    fn temperature(&self) -> Result<Vec<Value>, HueError> {
        self.get_resource("temperature")
    }

    fn rooms(&self) -> Result<Vec<Value>, HueError> {
        self.get_resource("room")
    }

    fn grouped_lights(&self) -> Result<Vec<Value>, HueError> {
        self.get_resource("grouped_light")
    }
}

// ---------------------------------------------------------------------------
// Raw-line wrapper.

/// A raw snapshot line: tagged with `ts` (for month partitioning) but written
/// as the verbatim bridge object. The `ts` is skipped in serialization; only
/// the inner `value` lands on disk.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Mapping helpers.

/// Extract the resource `id` (a UUID string) from a CLIP v2 object.
fn resource_id(obj: &Value) -> &str {
    obj.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Extract the `metadata.name` from a CLIP v2 object. Empty when absent.
fn metadata_name(obj: &Value) -> &str {
    obj.get("metadata")
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// A minute-resolution timestamp key for dedupe (truncate to the minute).
/// Returns `""` on parse failure.
fn minute_key(ts: &str) -> String {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|t| t.with_timezone(&Utc).format("%Y-%m-%dT%H:%M").to_string())
        .unwrap_or_default()
}

/// Map a CLIP v2 temperature resource into a `HomeReading`.
///
/// CLIP v2 shape — canonical paths (CLIP v2 / Hue API v2):
/// ```json
/// {
///   "id": "...",
///   "type": "temperature",
///   "temperature": {
///     "temperature_report": { "temperature": 21.5, "changed": "2026-06-10T21:05:00.000Z" },
///     "temperature": 21.5,          // deprecated, fallback for old firmware
///     "temperature_valid": true     // deprecated
///   },
///   "metadata": { "name": "..." }  // may be absent on service resources; name lives on owner device
/// }
/// ```
/// Prefer `temperature_report.temperature` (canonical). Fall back to the
/// deprecated flat `temperature.temperature` for old firmware that has not yet
/// populated the report object.
fn temperature_to_reading(obj: &Value, now_ts: &str) -> Option<HomeReading> {
    let id = resource_id(obj);
    if id.is_empty() {
        return None;
    }

    let temp_obj = obj.get("temperature")?;

    // Prefer canonical temperature_report; fall back to deprecated flat field.
    let temp = temp_obj
        .get("temperature_report")
        .and_then(|r| r.get("temperature"))
        .and_then(Value::as_f64)
        .or_else(|| temp_obj.get("temperature").and_then(Value::as_f64))?;

    // Prefer canonical report (when present, it supersedes deprecated valid flag).
    // If the report is absent, fall back to the deprecated temperature_valid field.
    let report_present = temp_obj.get("temperature_report").is_some();
    let valid = if report_present {
        true // report only populated when reading is valid
    } else {
        temp_obj
            .get("temperature_valid")
            .and_then(Value::as_bool)
            .unwrap_or(true) // absent → assume valid
    };
    if !valid {
        return None;
    }

    if Partition::Month.key(now_ts).is_none() {
        return None;
    }

    let name = metadata_name(obj);
    let mk = minute_key(now_ts);

    let mut r = HomeReading::new("philips-hue", "temperature", temp, now_ts.to_string());
    r.unit = "C".to_string();
    r.device = id.to_string();
    // NOTE: In CLIP v2, the human name lives on the owner device resource, not
    // on this service resource. metadata.name is typically absent here.
    // We leave place empty rather than misusing it for the sensor UUID/name.
    // A future enhancement can resolve owner.rid → device.metadata.name.
    let _ = name; // suppress unused-variable warning

    let mut extra = Map::new();
    extra.insert(
        "guid".into(),
        Value::String(format!("philips-hue:temperature:{id}:{mk}")),
    );
    r.extra = extra;
    Some(r)
}

/// Map a CLIP v2 motion resource into a home-event `Value` when presence is
/// currently detected (`motion_report.motion == true` or deprecated
/// `motion.motion == true`).
///
/// CLIP v2 shape — canonical paths:
/// ```json
/// {
///   "id": "...",
///   "type": "motion",
///   "motion": {
///     "motion_report": {
///       "motion": true,
///       "changed": "2026-06-10T21:05:00.000Z"  // authoritative event time
///     },
///     "motion": true,        // deprecated, fallback for old firmware
///     "motion_valid": true   // deprecated
///   },
///   "metadata": { "name": "..." }  // may be absent; name lives on owner device
/// }
/// ```
/// The authoritative event time is `motion_report.changed`. We use it for
/// both the event `ts` and the dedupe `guid`, so two polls that both catch an
/// ongoing presence event emit exactly one record (stable key).
/// Falls back to the poll time (`now_ts`) only when `motion_report` is absent
/// (old firmware), mirroring ring.rs `event_ts()`.
fn motion_to_event(obj: &Value, now_ts: &str) -> Option<Value> {
    let id = resource_id(obj);
    if id.is_empty() {
        return None;
    }

    let motion_obj = obj.get("motion")?;

    // Prefer canonical motion_report; fall back to deprecated flat field.
    let report = motion_obj.get("motion_report");
    let detected = report
        .and_then(|r| r.get("motion"))
        .and_then(Value::as_bool)
        .or_else(|| motion_obj.get("motion").and_then(Value::as_bool))
        .unwrap_or(false);

    // Only emit an event when presence is actually detected.
    if !detected {
        return None;
    }

    // Validity: report presence supersedes deprecated motion_valid.
    let report_present = report.is_some();
    let valid = if report_present {
        true // report only populated when reading is valid
    } else {
        motion_obj
            .get("motion_valid")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    };
    if !valid {
        return None;
    }

    // Authoritative event timestamp: use motion_report.changed when available;
    // fall back to poll time only on old firmware without the report object.
    let event_ts: String = report
        .and_then(|r| r.get("changed"))
        .and_then(Value::as_str)
        // Normalise to RFC3339 with timezone offset (parse + reformat).
        .and_then(|s| {
            DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.to_rfc3339())
        })
        .unwrap_or_else(|| now_ts.to_string());

    if Partition::Month.key(&event_ts).is_none() {
        return None;
    }

    // Stable dedupe key: sensor id + authoritative change time (not poll minute).
    // When the report is absent we fall back to minute-granularity of poll time.
    let guid = if report_present {
        format!("philips-hue:motion:{id}:{event_ts}")
    } else {
        format!("philips-hue:motion:{id}:{}", minute_key(now_ts))
    };

    // Device label: metadata.name on service resources is typically absent in
    // CLIP v2 (name lives on the owner device). Fall back to the UUID so the
    // field is always populated.
    let name = metadata_name(obj);
    let device_label = if name.is_empty() { id } else { name };

    let mut ev = Map::new();
    ev.insert("ts".into(), Value::String(event_ts));
    ev.insert("source".into(), Value::String("philips-hue".into()));
    ev.insert("device".into(), Value::String(device_label.to_string()));
    ev.insert("event".into(), Value::String("motion".into()));
    ev.insert("guid".into(), Value::String(guid));

    let mut extra = Map::new();
    extra.insert("sensor_id".into(), Value::String(id.to_string()));
    ev.insert("extra".into(), Value::Object(extra));

    Some(Value::Object(ev))
}

// ---------------------------------------------------------------------------
// Dedupe helpers.

/// Extract the guid from an event `Value` (the guid field at the top level).
fn event_guid(v: &Value) -> String {
    v.get("guid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Extract the guid from a HomeReading's extra map.
fn reading_guid(r: &HomeReading) -> String {
    r.extra
        .get("guid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Write new temperature readings (contract) and motion events to the vault,
/// deduping each by guid. Returns `(readings_written, events_written)`.
fn write_layers(
    vault: &Vault,
    readings: Vec<HomeReading>,
    events: Vec<Value>,
    raws: Vec<(String, Value)>,
) -> Result<(u64, u64)> {
    let contract = vault.stream(DIR, Partition::Month);
    let ev_stream = vault.stream(EVENTS_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    // --- contract readings: dedupe by guid --------------------------------
    let mut seen_reading_guids: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for v in contract.read::<Value>(&key)? {
            let g = v
                .get("extra")
                .and_then(|e| e.get("guid"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if !g.is_empty() {
                seen_reading_guids.insert(g);
            }
        }
    }
    let mut new_readings: Vec<HomeReading> = Vec::new();
    for r in readings {
        let g = reading_guid(&r);
        if g.is_empty() || !seen_reading_guids.insert(g) {
            continue;
        }
        new_readings.push(r);
    }

    // --- events: dedupe by guid -------------------------------------------
    let mut seen_event_guids: HashSet<String> = HashSet::new();
    for key in ev_stream.partitions()? {
        for v in ev_stream.read::<Value>(&key)? {
            let g = event_guid(&v);
            if !g.is_empty() {
                seen_event_guids.insert(g);
            }
        }
    }
    let mut new_events: Vec<RawLine> = Vec::new();
    for ev in events {
        let g = event_guid(&ev);
        if g.is_empty() || !seen_event_guids.insert(g) {
            continue;
        }
        if let Some(ts) = ev.get("ts").and_then(Value::as_str).map(str::to_string) {
            new_events.push(RawLine { ts, value: ev });
        }
    }

    // --- raw: write unconditionally (full fidelity) ----------------------
    let mut new_raws: Vec<RawLine> = Vec::new();
    for (ts, v) in raws {
        new_raws.push(RawLine { ts, value: v });
    }

    let readings_written = new_readings.len() as u64;
    let events_written = new_events.len() as u64;

    if !new_readings.is_empty() {
        contract.append(&new_readings, |r| &r.ts)?;
    }
    if !new_events.is_empty() {
        ev_stream.append(&new_events, |r| &r.ts)?;
    }
    if !new_raws.is_empty() {
        raw_stream.append(&new_raws, |r| &r.ts)?;
    }

    Ok((readings_written, events_written))
}

// ---------------------------------------------------------------------------
// The pull.

/// Resolve credentials and sync. Missing creds → clear error on the manual
/// path, quiet skip on the periodic path (handled by `def_collect`).
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    let pasted = vault
        .load_sync_token(SERVICE)?
        .map(|t| t.access_token)
        .filter(|t| !t.trim().is_empty())
        .context("Philips Hue is not connected — add your bridge IP and username in the Integrations tab")?;
    let (ip, username) = parse_credentials(&pasted)?;
    let client = HueClient::new(ip, username);
    pull_with(vault, &client)
}

/// The pull body over an injected API — the testable seam.
fn pull_with(vault: &Vault, api: &impl HueApi) -> Result<PullOutcome> {
    let now_ts = Local::now().to_rfc3339();

    // Fetch all resource types from the bridge. A failure on any one is
    // propagated; the bridge is LAN-local and should be fully reachable.
    let temp_sensors = api.temperature().map_err(|e| fetch_err("temperature", e))?;
    let motion_sensors = api.motion().map_err(|e| fetch_err("motion", e))?;
    let lights = api.lights().map_err(|e| fetch_err("lights", e))?;
    let rooms = api.rooms().map_err(|e| fetch_err("rooms", e))?;
    let grouped = api.grouped_lights().map_err(|e| fetch_err("grouped_lights", e))?;

    // --- contract layer: temperature readings ---------------------
    let mut readings: Vec<HomeReading> = Vec::new();
    for obj in &temp_sensors {
        if let Some(r) = temperature_to_reading(obj, &now_ts) {
            readings.push(r);
        }
    }

    // --- contract layer: motion events ---------------------------
    let mut events: Vec<Value> = Vec::new();
    for obj in &motion_sensors {
        if let Some(ev) = motion_to_event(obj, &now_ts) {
            events.push(ev);
        }
    }

    // --- raw layer: full bridge snapshot unconditionally ---------
    // Snapshot every resource type under raw/, tagged with the poll time.
    let mut raws: Vec<(String, Value)> = Vec::new();
    for obj in temp_sensors
        .iter()
        .chain(motion_sensors.iter())
        .chain(lights.iter())
        .chain(rooms.iter())
        .chain(grouped.iter())
    {
        raws.push((now_ts.clone(), obj.clone()));
    }

    let (readings_written, events_written) = write_layers(vault, readings, events, raws)?;

    // Advance cursor.
    vault.write_hue_sync(&SyncState {
        last_synced: now_ts,
    })?;

    let mut counts = BTreeMap::new();
    counts.insert("readings", readings_written);
    counts.insert("events", events_written);
    Ok(PullOutcome {
        headline: format!("{readings_written} readings, {events_written} events"),
        counts,
    })
}

/// Map a [`HueError`] at the top of an endpoint into an anyhow error.
fn fetch_err(endpoint: &str, e: HueError) -> anyhow::Error {
    match e {
        HueError::Unauthorized => anyhow::anyhow!(
            "Philips Hue rejected the credentials on the {endpoint} endpoint — reconnect from the Integrations tab"
        ),
        HueError::Unreachable(m) => anyhow::anyhow!(
            "unreachable: Philips Hue {endpoint} endpoint could not be reached: {m}"
        ),
        HueError::Other(m) => anyhow::anyhow!("Philips Hue {endpoint} fetch failed: {m}"),
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
            .join(format!("trove-philips-hue-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // --- CLIP v2 fixtures (real CLIP v2 shape with canonical *_report fields) ---

    /// A CLIP v2 temperature resource — canonical shape with `temperature_report`
    /// (the authoritative field), plus deprecated flat fields for firmware
    /// compat. The `temperature_report` is what current firmware populates;
    /// the flat `temperature` and `temperature_valid` are deprecated fallbacks.
    fn fixture_temp_sensor() -> Value {
        json!({
            "id": "a3b4c5d6-e7f8-1234-abcd-ef1234567890",
            "id_v1": "/sensors/12",
            "type": "temperature",
            "temperature": {
                "temperature_report": {
                    "temperature": 21.46,
                    "changed": "2026-06-10T21:05:00.000Z"
                },
                "temperature": 21.46,
                "temperature_valid": true
            },
            // In CLIP v2 service resources typically have no metadata.name;
            // the name lives on the owner device. We include an empty/absent
            // metadata object to test that code path.
            "owner": {
                "rid": "parent-device-uuid",
                "rtype": "device"
            }
        })
    }

    /// Old-firmware temperature resource: has only deprecated flat fields,
    /// no `temperature_report`. Parser must fall back gracefully.
    fn fixture_temp_sensor_old_firmware() -> Value {
        json!({
            "id": "a3b4c5d6-e7f8-1234-abcd-ef1234567890",
            "id_v1": "/sensors/12",
            "type": "temperature",
            "temperature": {
                "temperature": 21.46,
                "temperature_valid": true
            },
            "owner": { "rid": "parent-device-uuid", "rtype": "device" }
        })
    }

    /// A CLIP v2 temperature resource with temperature_valid=false (stale/invalid,
    /// old-firmware path — no report object).
    fn fixture_invalid_temp_sensor() -> Value {
        json!({
            "id": "bbbbbbbb-0000-0000-0000-000000000001",
            "type": "temperature",
            "temperature": {
                "temperature": 0.0,
                "temperature_valid": false
            }
        })
    }

    /// A CLIP v2 motion resource with presence detected — canonical shape with
    /// `motion_report` (authoritative; carries `changed` for the event time and
    /// `motion` for the detected state). Also includes deprecated flat fields.
    fn fixture_motion_active() -> Value {
        json!({
            "id": "d4e5f6a7-b8c9-4321-dcba-fedcba987654",
            "id_v1": "/sensors/13",
            "type": "motion",
            "motion": {
                "motion_report": {
                    "motion": true,
                    "changed": "2026-06-10T21:05:00.000Z"
                },
                "motion": true,
                "motion_valid": true
            },
            // No metadata.name on the service resource — name lives on the device.
            "owner": {
                "rid": "parent-device-uuid",
                "rtype": "device"
            }
        })
    }

    /// Old-firmware motion resource: deprecated flat fields only, no
    /// `motion_report`. Parser must fall back to poll-time and minute-granularity.
    fn fixture_motion_active_old_firmware() -> Value {
        json!({
            "id": "d4e5f6a7-b8c9-4321-dcba-fedcba987654",
            "type": "motion",
            "motion": {
                "motion": true,
                "motion_valid": true
            }
        })
    }

    /// A CLIP v2 motion resource with no presence detected.
    fn fixture_motion_idle() -> Value {
        json!({
            "id": "e5f6a7b8-c9d0-4321-abcd-fedcba987655",
            "type": "motion",
            "motion": {
                "motion_report": {
                    "motion": false,
                    "changed": "2026-06-10T20:00:00.000Z"
                },
                "motion": false,
                "motion_valid": true
            }
        })
    }

    /// A CLIP v2 light resource (full fidelity for raw layer).
    fn fixture_light() -> Value {
        json!({
            "id": "f1e2d3c4-b5a6-7890-abcd-ef1234567891",
            "type": "light",
            "on": { "on": true },
            "dimming": { "brightness": 75.0 },
            "color_temperature": { "mirek": 366 },
            "color": { "xy": { "x": 0.3227, "y": 0.3290 } },
            "metadata": { "name": "Living room" },
            "owner": { "rid": "room-uuid", "rtype": "room" }
        })
    }

    // --- Stub API for offline tests ---

    struct StubApi {
        temp: Vec<Value>,
        motion: Vec<Value>,
        lights: Vec<Value>,
        rooms: Vec<Value>,
        grouped: Vec<Value>,
        fail: bool,
    }

    impl StubApi {
        fn new(
            temp: Vec<Value>,
            motion: Vec<Value>,
            lights: Vec<Value>,
        ) -> Self {
            StubApi {
                temp,
                motion,
                lights,
                rooms: vec![],
                grouped: vec![],
                fail: false,
            }
        }

        fn unreachable() -> Self {
            StubApi {
                temp: vec![],
                motion: vec![],
                lights: vec![],
                rooms: vec![],
                grouped: vec![],
                fail: true,
            }
        }
    }

    impl HueApi for StubApi {
        fn bridge_info(&self) -> Result<Value, HueError> {
            if self.fail {
                Err(HueError::Unreachable("connection refused".into()))
            } else {
                Ok(json!({"data": [{"id": "bridge-id", "type": "bridge"}]}))
            }
        }

        fn lights(&self) -> Result<Vec<Value>, HueError> {
            if self.fail {
                Err(HueError::Unreachable("connection refused".into()))
            } else {
                Ok(self.lights.clone())
            }
        }

        fn motion(&self) -> Result<Vec<Value>, HueError> {
            if self.fail {
                Err(HueError::Unreachable("connection refused".into()))
            } else {
                Ok(self.motion.clone())
            }
        }

        fn temperature(&self) -> Result<Vec<Value>, HueError> {
            if self.fail {
                Err(HueError::Unreachable("connection refused".into()))
            } else {
                Ok(self.temp.clone())
            }
        }

        fn rooms(&self) -> Result<Vec<Value>, HueError> {
            if self.fail {
                Err(HueError::Unreachable("connection refused".into()))
            } else {
                Ok(self.rooms.clone())
            }
        }

        fn grouped_lights(&self) -> Result<Vec<Value>, HueError> {
            if self.fail {
                Err(HueError::Unreachable("connection refused".into()))
            } else {
                Ok(self.grouped.clone())
            }
        }
    }

    // --- Pure mapping tests -----------------------------------------------

    #[test]
    fn philips_hue_temperature_sensor_maps_to_reading() {
        let obj = fixture_temp_sensor();
        let ts = "2026-06-10T14:05:00-07:00";
        let r = temperature_to_reading(&obj, ts).expect("should produce a reading");
        assert_eq!(r.source, "philips-hue");
        assert_eq!(r.metric, "temperature");
        // Must read from canonical temperature_report.temperature (21.46).
        assert_eq!(r.value, 21.46);
        assert_eq!(r.unit, "C");
        assert_eq!(r.device, "a3b4c5d6-e7f8-1234-abcd-ef1234567890");
        // place is not set from metadata.name on service resources (name lives on device).
        assert!(r.place.is_empty(), "place must be empty for bare service resource: {:?}", r.place);
        // guid must be stable and contain the sensor id + minute key.
        let guid = r.extra.get("guid").and_then(Value::as_str).unwrap_or("");
        assert!(guid.starts_with("philips-hue:temperature:a3b4c5d6-e7f8-1234-abcd-ef1234567890:"),
            "guid must contain sensor id: {guid}");
        assert!(Partition::Month.key(&r.ts).is_some(), "ts has a month key");
    }

    /// Old-firmware fixture (no temperature_report): parser falls back to flat field.
    #[test]
    fn philips_hue_temperature_old_firmware_fallback() {
        let obj = fixture_temp_sensor_old_firmware();
        let ts = "2026-06-10T14:05:00-07:00";
        let r = temperature_to_reading(&obj, ts).expect("old-firmware sensor must still produce a reading");
        assert_eq!(r.value, 21.46, "flat field fallback value correct");
        assert_eq!(r.unit, "C");
        let guid = r.extra.get("guid").and_then(Value::as_str).unwrap_or("");
        assert!(guid.starts_with("philips-hue:temperature:a3b4c5d6-e7f8-1234-abcd-ef1234567890:"),
            "guid contains sensor id even on fallback path: {guid}");
    }

    #[test]
    fn philips_hue_invalid_temperature_yields_no_reading() {
        let obj = fixture_invalid_temp_sensor();
        let ts = "2026-06-10T14:05:00-07:00";
        assert!(
            temperature_to_reading(&obj, ts).is_none(),
            "temperature_valid=false must not produce a reading"
        );
    }

    #[test]
    fn philips_hue_temperature_without_id_yields_no_reading() {
        // A resource with no id can't be stably keyed.
        let obj = json!({"type": "temperature", "temperature": {"temperature": 20.0}});
        assert!(temperature_to_reading(&obj, "2026-06-10T14:05:00-07:00").is_none());
    }

    #[test]
    fn philips_hue_motion_active_produces_event_with_authoritative_ts() {
        let obj = fixture_motion_active();
        let poll_ts = "2026-06-10T18:42:11-07:00"; // poll time (different minute)
        let ev = motion_to_event(&obj, poll_ts).expect("active motion must produce an event");
        assert_eq!(ev.get("source").and_then(Value::as_str), Some("philips-hue"));
        assert_eq!(ev.get("event").and_then(Value::as_str), Some("motion"));
        // Device falls back to UUID when metadata.name absent on service resource.
        assert_eq!(ev.get("device").and_then(Value::as_str),
            Some("d4e5f6a7-b8c9-4321-dcba-fedcba987654"),
            "device falls back to sensor UUID when no metadata.name");
        let event_ts = ev.get("ts").and_then(Value::as_str).unwrap_or("");
        // ts must come from motion_report.changed, NOT from the poll time.
        assert!(event_ts.contains("2026-06-10"),
            "event ts should be the authoritative changed time, not poll time: {event_ts}");
        assert!(!event_ts.contains("18:42"),
            "event ts must NOT be the poll minute: {event_ts}");
        // guid keyed on sensor id + authoritative event time (stable across polls).
        let guid = ev.get("guid").and_then(Value::as_str).unwrap_or("");
        assert!(guid.starts_with("philips-hue:motion:d4e5f6a7-b8c9-4321-dcba-fedcba987654:"),
            "guid must contain sensor id: {guid}");
        // guid must NOT contain the poll minute (that was the old buggy key).
        assert!(!guid.contains("18:42"),
            "guid must not embed poll minute: {guid}");
        assert!(Partition::Month.key(event_ts).is_some(), "event ts has a month key");
    }

    /// Old-firmware motion (no motion_report): falls back to poll time + minute-granularity.
    #[test]
    fn philips_hue_motion_old_firmware_fallback() {
        let obj = fixture_motion_active_old_firmware();
        let poll_ts = "2026-06-10T18:42:11-07:00";
        let ev = motion_to_event(&obj, poll_ts).expect("old-firmware active motion must produce an event");
        let event_ts = ev.get("ts").and_then(Value::as_str).unwrap_or("");
        // No motion_report → falls back to poll_ts.
        assert_eq!(event_ts, poll_ts,
            "old firmware: event ts falls back to poll time");
        let guid = ev.get("guid").and_then(Value::as_str).unwrap_or("");
        // Falls back to minute-granularity key.
        assert!(guid.starts_with("philips-hue:motion:d4e5f6a7-b8c9-4321-dcba-fedcba987654:"),
            "old firmware guid has sensor id: {guid}");
    }

    /// Cross-poll dedup: two polls that both catch the SAME physical motion event
    /// must produce ONE record, not two (the old bug).
    #[test]
    fn philips_hue_motion_cross_poll_dedup() {
        // The same motion_report.changed means the same physical event.
        let obj = fixture_motion_active();
        let poll1_ts = "2026-06-10T18:42:11-07:00";
        let poll2_ts = "2026-06-10T18:43:05-07:00"; // different minute — old code would duplicate
        let ev1 = motion_to_event(&obj, poll1_ts).expect("first poll emits event");
        let ev2 = motion_to_event(&obj, poll2_ts).expect("second poll also emits event object");
        let guid1 = ev1.get("guid").and_then(Value::as_str).unwrap_or("");
        let guid2 = ev2.get("guid").and_then(Value::as_str).unwrap_or("");
        assert_eq!(guid1, guid2,
            "same motion_report.changed → same guid across polls → deduplicated: g1={guid1} g2={guid2}");
    }

    #[test]
    fn philips_hue_motion_idle_yields_no_event() {
        let obj = fixture_motion_idle();
        let ts = "2026-06-10T18:42:11-07:00";
        assert!(
            motion_to_event(&obj, ts).is_none(),
            "motion=false must not produce an event"
        );
    }

    #[test]
    fn philips_hue_motion_without_id_yields_no_event() {
        let obj = json!({"type": "motion", "motion": {"motion_report": {"motion": true, "changed": "2026-06-10T21:05:00.000Z"}, "motion": true, "motion_valid": true}});
        assert!(motion_to_event(&obj, "2026-06-10T14:05:00-07:00").is_none());
    }

    #[test]
    fn philips_hue_minute_key_truncates_to_minute() {
        let k1 = minute_key("2026-06-10T14:05:00-07:00");
        let k2 = minute_key("2026-06-10T14:05:59-07:00");
        // Both should map to the same UTC minute.
        assert!(!k1.is_empty());
        assert_eq!(k1, k2, "same minute → same key (dedupe within a minute)");
        let k3 = minute_key("2026-06-10T14:06:00-07:00");
        assert_ne!(k1, k3, "different minute → different key");
    }

    #[test]
    fn philips_hue_parse_credentials_ok() {
        let (ip, username) = parse_credentials("192.168.1.10|abc123def456").unwrap();
        assert_eq!(ip, "192.168.1.10");
        assert_eq!(username, "abc123def456");
    }

    #[test]
    fn philips_hue_parse_credentials_trims_whitespace() {
        let (ip, username) = parse_credentials("  192.168.1.10 | abc123  ").unwrap();
        assert_eq!(ip, "192.168.1.10");
        assert_eq!(username, "abc123");
    }

    #[test]
    fn philips_hue_parse_credentials_missing_pipe_errors() {
        assert!(parse_credentials("192.168.1.10").is_err());
    }

    #[test]
    fn philips_hue_parse_credentials_empty_errors() {
        assert!(parse_credentials("").is_err());
    }

    // --- Pull integration tests: pull_with + vault writes -----------------

    #[test]
    fn philips_hue_pull_writes_both_layers_and_dedupes() {
        let vault = temp_vault("fullpull");
        let api = StubApi::new(
            vec![fixture_temp_sensor()],
            vec![fixture_motion_active(), fixture_motion_idle()],
            vec![fixture_light()],
        );

        let out = pull_with(&vault, &api).unwrap();
        let readings = out.counts.get("readings").copied().unwrap_or(0);
        let events = out.counts.get("events").copied().unwrap_or(0);
        assert_eq!(readings, 1, "one temperature reading from one valid sensor");
        assert_eq!(events, 1, "one motion event from one active sensor (idle skipped)");

        // Raw layer: one object per resource (1 temp + 1 active motion + 1 idle + 1 light).
        let raw = vault.stream(RAW_DIR, Partition::Month);
        let mut raw_count = 0usize;
        for key in raw.partitions().unwrap() {
            raw_count += raw.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(raw_count, 4, "all 4 resource objects go to raw");

        // Contract readings.
        let contract = vault.stream(DIR, Partition::Month);
        let mut all_readings: Vec<HomeReading> = Vec::new();
        for key in contract.partitions().unwrap() {
            all_readings.extend(contract.read::<HomeReading>(&key).unwrap());
        }
        assert_eq!(all_readings.len(), 1);
        assert_eq!(all_readings[0].metric, "temperature");
        assert_eq!(all_readings[0].value, 21.46);
        assert_eq!(all_readings[0].unit, "C");

        // Events.
        let ev_stream = vault.stream(EVENTS_DIR, Partition::Month);
        let mut all_events: Vec<Value> = Vec::new();
        for key in ev_stream.partitions().unwrap() {
            all_events.extend(ev_stream.read::<Value>(&key).unwrap());
        }
        assert_eq!(all_events.len(), 1);
        assert_eq!(all_events[0].get("event").and_then(Value::as_str), Some("motion"));

        // Cursor was written.
        let state = vault.read_hue_sync();
        assert!(!state.last_synced.is_empty(), "cursor advanced after pull");

        // Re-run with same data: guid dedupe → zero new writes.
        let api2 = StubApi::new(
            vec![fixture_temp_sensor()],
            vec![fixture_motion_active()],
            vec![fixture_light()],
        );
        let out2 = pull_with(&vault, &api2).unwrap();
        assert_eq!(out2.counts.get("readings").copied().unwrap_or(0), 0,
            "all reading guids already stored → deduped");
        assert_eq!(out2.counts.get("events").copied().unwrap_or(0), 0,
            "all event guids already stored → deduped");
    }

    #[test]
    fn philips_hue_pull_empty_bridge_is_ok() {
        // Bridge returns no sensors, no lights — zero writes, no error.
        let vault = temp_vault("empty");
        let api = StubApi::new(vec![], vec![], vec![]);
        let out = pull_with(&vault, &api).unwrap();
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 0);
        assert_eq!(out.counts.get("events").copied().unwrap_or(0), 0);
    }

    #[test]
    fn philips_hue_pull_unreachable_returns_error() {
        // An unreachable bridge is an Err, which def_collect converts to a
        // quiet log (never panics the loop).
        let vault = temp_vault("unreachable");
        // No credential needed — we fail before hitting the secret store.
        let api = StubApi::unreachable();
        let err = pull_with(&vault, &api).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unreachable") || msg.contains("connection"),
            "error must mention unreachability: {msg}"
        );
    }

    #[test]
    fn philips_hue_connection_def_is_token_paste() {
        assert_eq!(CONNECTION.id, "philips-hue");
        assert!(
            CONNECTION.method("token-paste").is_some(),
            "must expose a token-paste method"
        );
        assert_eq!(DEF.connection, Some("philips-hue"));
    }

    #[test]
    fn philips_hue_cursor_back_compat_empty_deserializes() {
        // An empty cursor file deserializes to the default (no last_synced).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.last_synced.is_empty());
        // An older cursor with just last_synced also loads fine.
        let partial: SyncState =
            serde_json::from_str(r#"{"last_synced":"2026-06-10T14:05:00-07:00"}"#).unwrap();
        assert!(!partial.last_synced.is_empty());
    }

    #[test]
    fn philips_hue_reading_round_trips() {
        let obj = fixture_temp_sensor();
        let ts = "2026-06-10T14:05:00-07:00";
        let r = temperature_to_reading(&obj, ts).unwrap();
        let j = serde_json::to_string(&r).unwrap();
        let r2: HomeReading = serde_json::from_str(&j).unwrap();
        assert_eq!(r.ts, r2.ts);
        assert_eq!(r.metric, r2.metric);
        assert_eq!(r.value, r2.value);
        assert_eq!(r.unit, r2.unit);
    }

    #[test]
    fn philips_hue_connection_stores_and_retrieves_credentials() {
        let vault = temp_vault("creds");
        vault
            .save_sync_token(
                SERVICE,
                &TokenSet {
                    access_token: "192.168.1.99|secret-username".into(),
                    refresh_token: None,
                    token_type: Some("HueLAN".into()),
                    scope: None,
                    expires_at: None,
                },
            )
            .unwrap();

        let status = def_status(&vault).unwrap();
        assert!(status.configured);
        assert_eq!(status.accounts.len(), 1);
        assert!(status.accounts[0].label.contains("192.168.1.99"));

        def_disconnect(&vault, "philips-hue").unwrap();
        assert!(def_status(&vault).unwrap().accounts.is_empty());
    }
}
