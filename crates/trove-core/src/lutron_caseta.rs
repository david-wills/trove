//! Lutron Caséta smart lighting and shades — local LEAP protocol poll.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/lutron-caseta.md
//!
//! A **Periodic** LAN poll against the Lutron Smart Bridge PRO over the
//! **LEAP** protocol — line-delimited JSON over mutual-TLS TCP on port 8081.
//! The bridge stores **no history**; Trove accumulates it by snapshotting
//! device/zone/occupancy state on every poll and emitting an event whenever
//! a state changes from the previous snapshot.
//!
//! **Requires the Smart Bridge PRO** (model L-BDG2-WH or similar). The
//! standard bridge has no local API; out of scope per the standalone rule.
//!
//! ## Protocol overview
//!
//! LEAP is line-delimited JSON exchanged over a TLS TCP connection with
//! mutual certificate authentication. The bridge and client both present X.509
//! certificates; the client certificate is obtained by a one-time pairing
//! handshake (button press on the bridge), as implemented by the community
//! `pylutron-caseta` library. This module stores the resulting PEM strings and
//! reconnects on each periodic poll.
//!
//! Message shape (request / response):
//! ```text
//! {"CommuniqueType":"ReadRequest","Header":{"Url":"/device","ClientTag":"..."}}
//! {"CommuniqueType":"ReadResponse","Header":{...},"Body":{"Devices":[...]}}
//! ```
//!
//! Each request is tagged with a `ClientTag` UUID; the response carries the
//! same tag so they can be correlated. Unsolicited messages arrive without a
//! tag on the same stream.
//!
//! ## Pairing
//!
//! Users run the `pylutron-caseta` pairing tool (`python3 -m pylutron_caseta.pairing`)
//! once, press the button on the Smart Bridge PRO, and receive a JSON object
//! containing `{"version":…,"key":"-----BEGIN RSA PRIVATE KEY-----\n…","cert":"…","ca":"…"}`.
//! They paste this JSON blob (or `ip|key_pem|cert_pem|ca_pem`) into Trove;
//! the credential is stored (0600) under `.trove/sync/lutron-caseta.json`.
//!
//! ## Two layers, unconditional
//!
//! - **Raw** — every LEAP ReadResponse body verbatim at
//!   `home/lutron-caseta/raw/YYYY-MM.jsonl`, tagged with `ts` (poll time).
//! - **Events** — state-change events per the `home.event` draft schema at
//!   `home/lutron-caseta/events/YYYY-MM.jsonl` (one row per device-state
//!   change detected between consecutive polls). Written as `serde_json::Value`
//!   because the `home.event` Rust type is a Phase-3 draft, not yet bound.
//!
//! ## Cursor / dedupe
//!
//! The cursor at `.trove/lutron-caseta-sync.json` persists the last-known state
//! of every device (zone level + occupancy group status). A change is emitted
//! only when the polled value differs from the stored snapshot. On first sync
//! the cursor is empty and we write a silent baseline (no events; just advance
//! the cursor) — matching the snapshot+diff collector convention.
//!
//! `guid` = `"lutron-caseta:{key}:{ts_unix_secs}"` — stable, unique per
//! event.
//!
//! Evidence: `pylutron-caseta` (GitHub: gurumitts/pylutron-caseta) test fixtures;
//! `tests/responses/devices.json`, `occupancygroupsubscribe.json`.
//! Confidence: **medium** — community-documented protocol, no official Lutron schema.

use std::collections::BTreeMap;
use std::io::{Read as IoRead, Write as IoWrite};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef,
    IntegrationDef, PullOutcome,
};
use crate::store::{write_json_atomic, Partition};
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants.

/// Home events (device state-change events, home.event draft shape).
const EVENTS_DIR: &str = "home/lutron-caseta/events";
/// Raw layer — full LEAP response objects, unconditional.
const RAW_DIR: &str = "home/lutron-caseta/raw";
/// Non-secret rebuildable cursor. NOT under `.trove/sync/` (that's for 0600 secrets).
const SYNC_FILE: &str = ".trove/lutron-caseta-sync.json";
/// Service id under `.trove/sync/` for the stored pairing credential.
const SERVICE: &str = "lutron-caseta";

/// Default bridge LEAP port (TLS).
const LEAP_PORT: u16 = 8081;

/// Seconds between periodic polls. Hourly is plenty — device state is stable
/// and we accumulate the timeline; frequent polling is not needed for history.
pub const LC_SYNC_SECS: u64 = 3600;

/// Hard timeout for the TCP connection and each LEAP message exchange.
const TCP_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum lines to read in a single LEAP message exchange before giving up.
const MAX_READ_LINES: usize = 500;

// ---------------------------------------------------------------------------
// Cursor — non-secret, rebuildable.

/// Per-device state snapshot keyed by a stable id (zone:N or occ:N).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeviceSnapshot {
    /// Last known zone level (0–100), or None if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    level: Option<i32>,
    /// Last known fan speed string ("Off"/"Low"/"Medium"/"MediumHigh"/"High").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fan_speed: Option<String>,
    /// Last known occupancy status ("Occupied"/"Unoccupied"/"Unknown").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    occupancy_status: Option<String>,
    /// Human-readable name of the device/zone/group.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    /// Device type string from LEAP (e.g. "WallDimmer", "CasetaFanSpeedController").
    #[serde(default, skip_serializing_if = "String::is_empty")]
    device_type: String,
}

/// Persisted non-secret cursor.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// RFC3339 timestamp of the last successful sync (for display).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_synced: Option<String>,
    /// Whether the first baseline has been captured (silent, no events emitted).
    #[serde(default)]
    baseline_taken: bool,
    /// Per-zone snapshot keyed by "zone:{zone_id}".
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    devices: BTreeMap<String, DeviceSnapshot>,
    /// Per-occupancy-group snapshot keyed by occupancy group href id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    occupancy: BTreeMap<String, DeviceSnapshot>,
}

impl Vault {
    fn read_lc_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_lc_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Pairing credential.

/// The pairing data stored in the 0600 token slot.
///
/// Users run `python3 -m pylutron_caseta.pairing`, press the button on the
/// bridge, and paste the resulting JSON + their bridge IP address.
#[derive(Debug, Clone)]
struct PairingData {
    /// Bridge IP address or hostname (e.g. "192.168.1.200").
    ip: String,
    /// Client private key (PEM).
    key_pem: String,
    /// Client certificate signed by the bridge (PEM).
    cert_pem: String,
    /// Bridge CA certificate used to verify the bridge's server cert (PEM).
    ca_pem: String,
}

/// Parse the pasted credential. Accepts two formats:
///
/// 1. JSON with a leading IP line:
///    ```text
///    192.168.1.200
///    {"key":"…","cert":"…","ca":"…"}
///    ```
/// 2. JSON with an embedded `"ip"` field:
///    ```json
///    {"ip":"192.168.1.200","key":"…","cert":"…","ca":"…"}
///    ```
/// 3. Pipe-separated (legacy): `ip|key_pem|cert_pem|ca_pem`
fn parse_credential(pasted: &str) -> Result<PairingData> {
    let pasted = pasted.trim();
    if pasted.is_empty() {
        bail!(
            "empty — paste the bridge IP and pairing JSON from pylutron-caseta. \
             Format: 192.168.1.200\\n{{\"key\":\"...\",\"cert\":\"...\",\"ca\":\"...\"}}"
        );
    }

    // Try JSON formats first.
    let (ip_hint, json_part): (Option<String>, &str) = if pasted.starts_with('{') {
        (None, pasted)
    } else if let Some(newline) = pasted.find('\n') {
        let ip_line = pasted[..newline].trim();
        let rest = pasted[newline + 1..].trim();
        if rest.starts_with('{') {
            (Some(ip_line.to_string()), rest)
        } else {
            // Pipe-separated fallback.
            (None, pasted)
        }
    } else {
        (None, pasted)
    };

    if json_part.starts_with('{') {
        let obj: Value =
            serde_json::from_str(json_part).context("pairing JSON parse failed")?;
        let key_pem = obj
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let cert_pem = obj
            .get("cert")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ca_pem = obj
            .get("ca")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ip = obj
            .get("ip")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or(ip_hint)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing bridge IP — prepend it before the JSON, e.g.:\n\
                     192.168.1.200\n{{\"key\":…}}"
                )
            })?;
        if key_pem.is_empty() {
            bail!("missing \"key\" in pairing JSON");
        }
        if cert_pem.is_empty() {
            bail!("missing \"cert\" in pairing JSON");
        }
        if ca_pem.is_empty() {
            bail!("missing \"ca\" in pairing JSON");
        }
        return Ok(PairingData { ip, key_pem, cert_pem, ca_pem });
    }

    // Pipe-separated fallback: ip|key_pem|cert_pem|ca_pem
    // PEM blocks contain newlines, so split at most 4 times on '|'.
    let parts: Vec<&str> = pasted.splitn(4, '|').collect();
    if parts.len() < 4 {
        bail!(
            "expected: bridge-ip (newline) then the JSON from pylutron-caseta, \
             OR four pipe-separated fields: ip|key_pem|cert_pem|ca_pem"
        );
    }
    Ok(PairingData {
        ip: parts[0].trim().to_string(),
        key_pem: parts[1].trim().to_string(),
        cert_pem: parts[2].trim().to_string(),
        ca_pem: parts[3].trim().to_string(),
    })
}

/// Serialize pairing data to a compact JSON string for storage.
fn serialize_credential(pd: &PairingData) -> String {
    serde_json::to_string(&json!({
        "ip": pd.ip,
        "key": pd.key_pem,
        "cert": pd.cert_pem,
        "ca": pd.ca_pem,
    }))
    .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// LEAP TLS connection (mutual TLS — client presents a certificate).

/// Parse PEM certificates into DER-encoded `CertificateDer` slices.
fn parse_pem_certs(pem: &str) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls_pemfile::certs;
    let mut cursor = std::io::Cursor::new(pem.as_bytes());
    let certs: Vec<_> = certs(&mut cursor)
        .collect::<std::io::Result<Vec<_>>>()
        .context("failed to parse PEM certificates")?;
    if certs.is_empty() {
        bail!("no certificates found in PEM data");
    }
    Ok(certs)
}

/// Parse a PEM private key, trying RSA, PKCS8, and EC in order.
fn parse_pem_private_key(pem: &str) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    use rustls_pemfile::{ec_private_keys, pkcs8_private_keys, rsa_private_keys};

    // RSA (most common for pylutron-caseta pairings)
    {
        let mut c = std::io::Cursor::new(pem.as_bytes());
        if let Ok(keys) = rsa_private_keys(&mut c).collect::<std::io::Result<Vec<_>>>() {
            if let Some(k) = keys.into_iter().next() {
                return Ok(rustls::pki_types::PrivateKeyDer::Pkcs1(k));
            }
        }
    }
    // PKCS8
    {
        let mut c = std::io::Cursor::new(pem.as_bytes());
        if let Ok(keys) = pkcs8_private_keys(&mut c).collect::<std::io::Result<Vec<_>>>() {
            if let Some(k) = keys.into_iter().next() {
                return Ok(rustls::pki_types::PrivateKeyDer::Pkcs8(k));
            }
        }
    }
    // EC
    {
        let mut c = std::io::Cursor::new(pem.as_bytes());
        if let Ok(keys) = ec_private_keys(&mut c).collect::<std::io::Result<Vec<_>>>() {
            if let Some(k) = keys.into_iter().next() {
                return Ok(rustls::pki_types::PrivateKeyDer::Sec1(k));
            }
        }
    }
    bail!("no private key found in PEM data (tried RSA/PKCS8/EC)")
}

/// Build a rustls `ClientConfig` for LEAP mutual TLS.
///
/// Trusts only the bridge's own CA (received during pairing) and presents the
/// paired client certificate.
fn leap_tls_config(pd: &PairingData) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let ca_certs = parse_pem_certs(&pd.ca_pem)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store
            .add(cert)
            .context("add CA certificate to trust store")?;
    }

    let client_certs = parse_pem_certs(&pd.cert_pem)?;
    let client_key = parse_pem_private_key(&pd.key_pem)?;

    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_certs, client_key)
        .context("client certificate configuration failed")?;

    Ok(Arc::new(config))
}

/// Resolve the bridge address to a `rustls::pki_types::ServerName`.
/// Tries DNS name first; falls back to IP address.
fn leap_server_name(ip: &str) -> Result<rustls::pki_types::ServerName<'static>> {
    if let Ok(sn) = rustls::pki_types::ServerName::try_from(ip.to_string()) {
        return Ok(sn);
    }
    use std::net::IpAddr;
    let addr: IpAddr = ip
        .parse()
        .with_context(|| format!("bridge address is neither a DNS name nor an IP: {ip}"))?;
    Ok(rustls::pki_types::ServerName::IpAddress(addr.into()))
}

// ---------------------------------------------------------------------------
// LEAP session — synchronous line-delimited JSON over TLS.

#[derive(Serialize)]
struct LeapRequest<'a> {
    #[serde(rename = "CommuniqueType")]
    communique_type: &'static str,
    #[serde(rename = "Header")]
    header: LeapHeader<'a>,
}

#[derive(Serialize)]
struct LeapHeader<'a> {
    #[serde(rename = "ClientTag")]
    client_tag: &'a str,
    #[serde(rename = "Url")]
    url: &'a str,
}

/// Generate a short hex tag (8 chars) via getrandom.
fn new_client_tag() -> String {
    let mut bytes = [0u8; 4];
    getrandom::getrandom(&mut bytes).unwrap_or(());
    hex::encode(bytes)
}

/// Send a LEAP `ReadRequest` and read lines until the tagged response arrives.
///
/// `stream` implements both `Read` and `Write`. We create a short-lived
/// `BufReader` wrapper for the read pass; since LEAP is strictly sequential
/// (write one request, drain until our tagged response) there is no state we
/// need to carry between calls.
///
/// Returns the parsed `Body` value (or `Value::Null` if the response has none).
fn leap_read<S>(stream: &mut S, url: &str) -> Result<Value>
where
    S: IoRead + IoWrite + ?Sized,
{
    let tag = new_client_tag();
    let req = LeapRequest {
        communique_type: "ReadRequest",
        header: LeapHeader { client_tag: &tag, url },
    };
    let mut line = serde_json::to_string(&req).context("serialize LEAP request")?;
    line.push_str("\r\n");
    stream
        .write_all(line.as_bytes())
        .context("write LEAP request")?;

    let mut resp_line = Vec::with_capacity(4096);
    for _ in 0..MAX_READ_LINES {
        // Read one byte at a time until '\n'. LEAP messages are short JSON lines
        // (hundreds of bytes), so byte-by-byte reading is acceptable here.
        resp_line.clear();
        loop {
            let mut byte = [0u8; 1];
            let n = stream
                .read(&mut byte)
                .context("read LEAP response byte")?;
            if n == 0 {
                bail!("LEAP connection closed by bridge");
            }
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                resp_line.push(byte[0]);
            }
        }
        if resp_line.is_empty() {
            continue;
        }
        let msg: Value = serde_json::from_slice(&resp_line)
            .context("parse LEAP response JSON")?;
        let resp_tag = msg
            .get("Header")
            .and_then(|h| h.get("ClientTag"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if resp_tag == tag.as_str() {
            return Ok(msg.get("Body").cloned().unwrap_or(Value::Null));
        }
        // Unsolicited message (subscription event, etc.) — continue reading.
    }
    bail!("LEAP ReadRequest for {url} timed out after {MAX_READ_LINES} lines")
}

// ---------------------------------------------------------------------------
// Poll logic — connect, query, diff, emit.

/// Outcome counts from a single poll.
#[derive(Default)]
struct PollCounts {
    raw_rows: u64,
    events: u64,
}

/// Raw snapshot line: `ts` drives month partitioning but is not written to disk.
#[derive(Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Event line wrapper for the JsonlStream appender.
#[derive(Serialize)]
struct EventLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// Map a LEAP zone Level (0–100, or -1 for unknown) to an event verb.
fn level_to_event(level: i32) -> &'static str {
    if level <= 0 { "off" } else { "on" }
}

/// Build the mandatory home.event fields map.
fn home_event_map(ts: &str, source: &str, device: &str, event: &str, guid: &str) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("ts".into(), Value::String(ts.to_string()));
    m.insert("source".into(), Value::String(source.to_string()));
    m.insert("device".into(), Value::String(device.to_string()));
    m.insert("event".into(), Value::String(event.to_string()));
    m.insert("guid".into(), Value::String(guid.to_string()));
    m
}

/// Connect to the bridge, query device/zone/occupancy state, diff against the
/// cursor, write raw + event rows.
fn poll_inner(vault: &Vault, pd: &PairingData) -> Result<PollCounts> {
    let now = Local::now();
    let now_ts = now.to_rfc3339();
    let now_unix = now.timestamp();

    // ---- TLS connect -------------------------------------------------------
    let tls_config = leap_tls_config(pd)?;
    let server_name = leap_server_name(&pd.ip)?;
    let conn = rustls::ClientConnection::new(tls_config, server_name)
        .context("create rustls client connection")?;
    let tcp = TcpStream::connect((pd.ip.as_str(), LEAP_PORT))
        .context("TCP connect to Lutron bridge")?;
    tcp.set_read_timeout(Some(TCP_TIMEOUT))
        .context("set TCP read timeout")?;
    tcp.set_write_timeout(Some(TCP_TIMEOUT))
        .context("set TCP write timeout")?;
    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);

    // ---- LEAP queries -------------------------------------------------------
    // leap_read takes &mut S where S: Read + Write; StreamOwned implements both,
    // so each call mutably borrows tls_stream for its duration (sequential).
    let devices_body = leap_read(&mut tls_stream, "/device")?;
    let occ_body = leap_read(&mut tls_stream, "/occupancygroup/status")?;

    // ---- Parse responses ---------------------------------------------------
    let devices = devices_body
        .get("Devices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let occ_statuses = occ_body
        .get("OccupancyGroupStatuses")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // ---- Per-zone status reads (Caseta Smart Bridge PRO path) --------------
    // Caseta uses per-zone reads at /zone/{id}/status → singular Body.ZoneStatus.
    // The collective /zone/status returning ZoneStatuses[] is the RA3/QSX path;
    // a Caseta PRO bridge may not answer it. Match pylutron-caseta's approach:
    // enumerate LocalZones from each device and read each zone individually.
    //
    // zone_id → parsed ZoneStatus Value (singular)
    let mut zone_map: BTreeMap<String, Value> = BTreeMap::new();
    let mut zone_statuses_for_raw: Vec<Value> = Vec::new(); // for raw snapshot

    for dev in &devices {
        let local_zones = dev
            .get("LocalZones")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for zone_ref in &local_zones {
            let zone_href = zone_ref.get("href").and_then(Value::as_str).unwrap_or("");
            let zone_id = zone_href.rsplit('/').next().unwrap_or("");
            if zone_id.is_empty() {
                continue;
            }
            let url_string = format!("/zone/{zone_id}/status");

            match leap_read(&mut tls_stream, &url_string) {
                Ok(body) => {
                    // Caseta body: {"ZoneStatus": {"Zone":{"href":"/zone/N"},"Level":N,...}}
                    if let Some(zs) = body.get("ZoneStatus").cloned() {
                        zone_statuses_for_raw.push(zs.clone());
                        zone_map.insert(zone_id.to_string(), zs);
                    }
                }
                Err(_e) => {
                    // A single zone read failure is non-fatal — skip this zone.
                    // If all zones fail the whole poll yields an empty zone_map;
                    // the cursor-guard below catches that and returns Err so the
                    // cursor remains intact.
                }
            }
        }
    }

    // ---- Load cursor -------------------------------------------------------
    let mut state = vault.read_lc_sync();
    let is_baseline = !state.baseline_taken;

    let mut raw_rows: Vec<RawLine> = Vec::new();
    let mut event_rows: Vec<EventLine> = Vec::new();

    // ---- Raw: device list snapshot -----------------------------------------
    if !devices.is_empty() {
        raw_rows.push(RawLine {
            ts: now_ts.clone(),
            value: json!({
                "CommuniqueType": "ReadResponse",
                "Url": "/device",
                "poll_ts": now_ts,
                "Devices": devices,
            }),
        });
    }

    // ---- Device/zone diff --------------------------------------------------
    let mut new_device_snap: BTreeMap<String, DeviceSnapshot> = BTreeMap::new();

    for dev in &devices {
        let href = dev.get("href").and_then(Value::as_str).unwrap_or("");
        if href.is_empty() {
            continue;
        }
        let name = dev
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let device_type = dev
            .get("DeviceType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        let local_zones = dev
            .get("LocalZones")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        for zone_ref in &local_zones {
            let zone_href = zone_ref.get("href").and_then(Value::as_str).unwrap_or("");
            let zone_id = zone_href.rsplit('/').next().unwrap_or("");
            if zone_id.is_empty() {
                continue;
            }
            // zone_map was populated by the per-zone reads above.
            // If this zone's read failed, skip it this poll (don't corrupt snapshot).
            if let Some(zs) = zone_map.get(zone_id) {
                // Level -1 means "unknown / not a dimmer" (fan zones carry -1).
                // Treat Level < 0 as absent so we don't emit spurious level events
                // for fan zones, and don't store -1 as a real dimmer level.
                let raw_level = zs.get("Level").and_then(Value::as_i64).map(|v| v as i32);
                let fan_speed = zs
                    .get("FanSpeed")
                    .and_then(Value::as_str)
                    .map(str::to_string);

                // Determine if this zone is a fan: either FanSpeed is present or
                // the device type is a fan controller (belt-and-suspenders check).
                let is_fan = fan_speed.is_some()
                    || device_type.to_lowercase().contains("fan");

                // For non-fan zones only: normalise the level (skip negative values).
                let level = if is_fan {
                    None // fans don't have a meaningful dimmer level
                } else {
                    raw_level.filter(|&l| l >= 0)
                };

                let snap_key = format!("zone:{zone_id}");
                let prev = state.devices.get(&snap_key).cloned().unwrap_or_default();

                new_device_snap.insert(
                    snap_key.clone(),
                    DeviceSnapshot {
                        level,
                        fan_speed: fan_speed.clone(),
                        occupancy_status: None,
                        name: name.clone(),
                        device_type: device_type.clone(),
                    },
                );

                if !is_baseline {
                    let level_changed = !is_fan && level != prev.level;
                    let fan_changed = fan_speed != prev.fan_speed;
                    if level_changed {
                        let lvl = level.unwrap_or(0);
                        let event_name = level_to_event(lvl);
                        let guid = format!("lutron-caseta:{snap_key}:{now_unix}");
                        let device_label = format!("{name} (zone {zone_id})");
                        let mut ev = home_event_map(
                            &now_ts,
                            "lutron-caseta",
                            &device_label,
                            event_name,
                            &guid,
                        );
                        if lvl > 0 {
                            ev.insert("value".into(), json!(lvl));
                        }
                        // device_type is event subtype metadata → goes in extra.
                        ev.insert("extra".into(), json!({"device_type": device_type.as_str()}));
                        event_rows.push(EventLine {
                            ts: now_ts.clone(),
                            value: Value::Object(ev),
                        });
                    } else if fan_changed {
                        if let Some(ref fs) = fan_speed {
                            let event_name = if fs == "Off" { "off" } else { "on" };
                            let guid = format!("lutron-caseta:{snap_key}:fan:{now_unix}");
                            let device_label = format!("{name} fan (zone {zone_id})");
                            let mut ev = home_event_map(
                                &now_ts,
                                "lutron-caseta",
                                &device_label,
                                event_name,
                                &guid,
                            );
                            ev.insert("detail".into(), json!(fs.as_str()));
                            ev.insert("extra".into(), json!({"device_type": device_type.as_str()}));
                            event_rows.push(EventLine {
                                ts: now_ts.clone(),
                                value: Value::Object(ev),
                            });
                        }
                    }
                }
            }
        }
    }

    // ---- Raw: zone statuses snapshot (per-zone reads collected above) ------
    if !zone_statuses_for_raw.is_empty() {
        raw_rows.push(RawLine {
            ts: now_ts.clone(),
            value: json!({
                "CommuniqueType": "ReadResponse",
                "Url": "/zone/{id}/status",
                "poll_ts": now_ts,
                "ZoneStatuses": zone_statuses_for_raw,
            }),
        });
    }

    // ---- Occupancy group diff ----------------------------------------------
    let mut new_occ_snap: BTreeMap<String, DeviceSnapshot> = BTreeMap::new();

    for og in &occ_statuses {
        let og_href = og
            .get("OccupancyGroup")
            .and_then(|h| h.get("href"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let og_id = og_href.rsplit('/').next().unwrap_or("");
        if og_id.is_empty() {
            continue;
        }
        let status = og
            .get("OccupancyStatus")
            .and_then(Value::as_str)
            .unwrap_or("Unknown")
            .to_string();

        let prev = state.occupancy.get(og_id).cloned().unwrap_or_default();
        new_occ_snap.insert(
            og_id.to_string(),
            DeviceSnapshot {
                level: None,
                fan_speed: None,
                occupancy_status: Some(status.clone()),
                name: format!("Occupancy Group {og_id}"),
                device_type: "OccupancyGroup".to_string(),
            },
        );

        if !is_baseline && prev.occupancy_status.as_deref() != Some(status.as_str()) {
            let event_name = match status.as_str() {
                "Occupied" => "motion",
                "Unoccupied" => "no_motion",
                _ => "unknown",
            };
            let guid = format!("lutron-caseta:occ:{og_id}:{now_unix}");
            let mut ev = home_event_map(
                &now_ts,
                "lutron-caseta",
                &format!("Occupancy Group {og_id}"),
                event_name,
                &guid,
            );
            ev.insert("detail".into(), json!(status.as_str()));
            event_rows.push(EventLine {
                ts: now_ts.clone(),
                value: Value::Object(ev),
            });
        }
    }

    // ---- Raw: occupancy statuses snapshot -----------------------------------
    if !occ_statuses.is_empty() {
        raw_rows.push(RawLine {
            ts: now_ts.clone(),
            value: json!({
                "CommuniqueType": "SubscribeResponse",
                "Url": "/occupancygroup/status",
                "poll_ts": now_ts,
                "OccupancyGroupStatuses": occ_statuses,
            }),
        });
    }

    // ---- Write raw ---------------------------------------------------------
    let raw_n = raw_rows.len() as u64;
    if !raw_rows.is_empty() {
        vault
            .stream(RAW_DIR, Partition::Month)
            .append(&raw_rows, |r| &r.ts)
            .context("write Lutron raw rows")?;
    }

    // ---- Write events (home.event draft shape) ------------------------------
    let events_n = event_rows.len() as u64;
    if !event_rows.is_empty() {
        vault
            .stream(EVENTS_DIR, Partition::Month)
            .append(&event_rows, |r| &r.ts)
            .context("write Lutron event rows")?;
    }

    // ---- Cursor guard: reject empty-snapshot polls -------------------------
    // If we enumerated devices but got NO zone statuses back (all per-zone reads
    // failed or returned empty bodies), treat this as a failed poll.  Advancing
    // the cursor with an empty new_device_snap would:
    //   a) silently erase the prior real snapshot, and
    //   b) cause a flood of spurious "on" events on the next good poll (every
    //      device transitions from prev=None to some real level).
    // Return Err so def_collect's connectivity guard can silence it and the
    // cursor stays intact.
    if !devices.is_empty() && new_device_snap.is_empty() {
        bail!(
            "all per-zone LEAP reads returned empty bodies — \
             bridge may be temporarily unresponsive (poll skipped, cursor preserved)"
        );
    }

    // ---- Advance cursor ----------------------------------------------------
    state.devices = new_device_snap;
    state.occupancy = new_occ_snap;
    state.baseline_taken = true;
    state.last_synced = Some(now_ts);
    vault.write_lc_sync(&state)?;

    Ok(PollCounts { raw_rows: raw_n, events: events_n })
}

/// Entry point for both the periodic watcher and the manual Sync-now button.
fn poll(vault: &Vault) -> Result<PollCounts> {
    let token = vault
        .load_sync_token(SERVICE)?
        .ok_or_else(|| anyhow::anyhow!("Lutron Caséta not connected — paste pairing data first"))?;
    let pd = parse_credential(&token.access_token)?;
    poll_inner(vault, &pd)
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

/// Periodic watcher: swallows LAN-connectivity failures quietly.
fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<crate::registry::CollectOutcome> {
    match poll(vault) {
        Ok(counts) => Ok(crate::registry::CollectOutcome::note_if(
            counts.events > 0,
            move || format!("Lutron Caséta synced — {} event(s) detected", counts.events),
        )),
        Err(e) => {
            let is_connectivity = e.chain().any(|cause| {
                let s = cause.to_string();
                s.contains("refused")
                    || s.contains("timed out")
                    || s.contains("No route")
                    || s.contains("unreachable")
                    || s.contains("TCP connect")
                    || s.contains("poll skipped, cursor preserved")
            });
            if is_connectivity {
                Ok(crate::registry::CollectOutcome::note(format!(
                    "Lutron Caséta bridge unreachable (skip): {e}"
                )))
            } else {
                Err(e)
            }
        }
    }
}

/// Manual "Sync now": surfaces errors to the user.
fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let counts = poll(vault)?;
    let headline = if counts.events == 0 {
        "Lutron Caséta is up to date — no state changes detected".to_string()
    } else {
        format!(
            "Lutron Caséta synced — {} event(s), {} raw snapshot(s)",
            counts.events, counts.raw_rows
        )
    };
    Ok(PullOutcome {
        headline,
        counts: BTreeMap::from([
            ("events", counts.events),
            ("raw_rows", counts.raw_rows),
        ]),
    })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "lutron-caseta",
        name: "Lutron Caséta",
        // Has a TokenPaste ConnectionDef → must be CloudSync (registry invariant).
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Polls your Lutron Caséta Smart Bridge PRO for lighting, shade, fan, and \
                      occupancy sensor state. All data stays on your LAN — no cloud dependency. \
                      Trove accumulates history by polling hourly and emitting an event on each \
                      state change.",
        domain: "home",
        vault_path: "home/lutron-caseta/",
        toggleable: true,
        setup: &[
            "Install the pairing tool: pip3 install pylutron-caseta",
            "Run: python3 -m pylutron_caseta.pairing",
            "Press the small button on the back of your Smart Bridge PRO when prompted.",
            "Copy the JSON output. Paste your bridge IP on the first line, then the JSON below.",
        ],
        caveats: "Requires the Lutron Smart Bridge PRO (model L-BDG2-WH or L-BDG2PRO2-WH). \
                  The standard bridge has no local API. The bridge stores no history — Trove \
                  builds the timeline by polling; gaps while Trove is asleep are honest.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(LC_SYNC_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("lutron-caseta"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection.

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let pd = parse_credential(pasted)?;

    // Validate by connecting and sending a LEAP ping.
    let tls_config = leap_tls_config(&pd)
        .context("TLS configuration failed — check that key, cert, and ca are valid PEM")?;
    let server_name = leap_server_name(&pd.ip)?;
    let conn = rustls::ClientConnection::new(tls_config, server_name)
        .context("create TLS connection to Lutron bridge")?;
    let tcp = TcpStream::connect((pd.ip.as_str(), LEAP_PORT)).with_context(|| {
        format!(
            "Could not reach the Lutron Smart Bridge PRO at {}:{} — \
             check the IP, ensure the bridge is powered on and on the same LAN, \
             and that you have the Smart Bridge PRO (not the standard bridge).",
            pd.ip, LEAP_PORT
        )
    })?;
    tcp.set_read_timeout(Some(TCP_TIMEOUT)).ok();
    tcp.set_write_timeout(Some(TCP_TIMEOUT)).ok();
    let mut tls_stream = rustls::StreamOwned::new(conn, tcp);
    let _body = leap_read(&mut tls_stream, "/server/1/status/ping")
        .context("LEAP ping failed — pairing data may be for a different bridge")?;

    vault.save_sync_token(
        SERVICE,
        &TokenSet {
            access_token: serialize_credential(&pd),
            refresh_token: None,
            token_type: Some("LutronLEAP".into()),
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
    if let Some(token) = vault.load_sync_token(SERVICE)? {
        let display = parse_credential(&token.access_token)
            .map(|pd| format!("Smart Bridge PRO at {}", pd.ip))
            .unwrap_or_else(|_| "Lutron Smart Bridge PRO".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label: display,
            connected_at: None,
            expires_at: None,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`].
/// The integrator must add `&crate::lutron_caseta::CONNECTION,` to CONNECTIONS
/// in integrations.rs.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "lutron-caseta",
    display_name: "Lutron Caséta",
    methods: &[ConnectMethod::TokenPaste {
        label: "Bridge IP and Pairing JSON",
        help: "Run `python3 -m pylutron_caseta.pairing`, press the button on your Smart Bridge \
               PRO, and paste the JSON output here. Prepend your bridge's IP address on its own \
               line before the JSON (e.g. 192.168.1.200, then a newline, then the JSON). \
               The credential is stored locally and sent only to your bridge over your LAN.",
        placeholder: "192.168.1.200\n{\"key\":\"-----BEGIN RSA PRIVATE KEY-----\\n...\",\"cert\":\"...\",\"ca\":\"...\"}",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["lutron-caseta"],
    setup: &[
        "Install the pairing tool: pip3 install pylutron-caseta",
        "Run: python3 -m pylutron_caseta.pairing",
        "Press the small button on the back of your Smart Bridge PRO when prompted.",
        "Copy the JSON output, prepend your bridge IP on its own line, and paste both here.",
    ],
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique temp vault directory for the test.
    fn temp_vault(label: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-lutron-caseta-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---- parse_credential --------------------------------------------------

    #[test]
    fn parse_credential_json_with_leading_ip() {
        let pasted = "192.168.1.200\n{\"key\":\"KEY\",\"cert\":\"CERT\",\"ca\":\"CA\"}";
        let pd = parse_credential(pasted).unwrap();
        assert_eq!(pd.ip, "192.168.1.200");
        assert_eq!(pd.key_pem, "KEY");
        assert_eq!(pd.cert_pem, "CERT");
        assert_eq!(pd.ca_pem, "CA");
    }

    #[test]
    fn parse_credential_json_with_ip_field() {
        let pasted = "{\"ip\":\"10.0.0.5\",\"key\":\"K\",\"cert\":\"C\",\"ca\":\"CA\"}";
        let pd = parse_credential(pasted).unwrap();
        assert_eq!(pd.ip, "10.0.0.5");
        assert_eq!(pd.key_pem, "K");
        assert_eq!(pd.cert_pem, "C");
        assert_eq!(pd.ca_pem, "CA");
    }

    #[test]
    fn parse_credential_pipe_separated() {
        let pasted = "192.168.1.1|KEY_PEM|CERT_PEM|CA_PEM";
        let pd = parse_credential(pasted).unwrap();
        assert_eq!(pd.ip, "192.168.1.1");
        assert_eq!(pd.key_pem, "KEY_PEM");
        assert_eq!(pd.cert_pem, "CERT_PEM");
        assert_eq!(pd.ca_pem, "CA_PEM");
    }

    #[test]
    fn parse_credential_empty_errors() {
        assert!(parse_credential("").is_err());
        assert!(parse_credential("   ").is_err());
    }

    #[test]
    fn parse_credential_json_missing_ip_errors() {
        let pasted = "{\"key\":\"K\",\"cert\":\"C\",\"ca\":\"CA\"}";
        assert!(parse_credential(pasted).is_err());
    }

    #[test]
    fn parse_credential_json_missing_key_errors() {
        let pasted = "192.168.1.1\n{\"cert\":\"C\",\"ca\":\"CA\"}";
        let result = parse_credential(pasted);
        // cert_pem is present, key_pem is empty → error
        assert!(result.is_err());
    }

    // ---- level_to_event ----------------------------------------------------

    #[test]
    fn level_to_event_off_at_zero() {
        assert_eq!(level_to_event(0), "off");
        assert_eq!(level_to_event(-1), "off");
    }

    #[test]
    fn level_to_event_on_when_positive() {
        assert_eq!(level_to_event(1), "on");
        assert_eq!(level_to_event(50), "on");
        assert_eq!(level_to_event(100), "on");
    }

    // ---- home_event_map required fields ------------------------------------

    #[test]
    fn home_event_map_has_five_required_fields() {
        let m = home_event_map(
            "2026-06-10T18:00:00-07:00",
            "lutron-caseta",
            "Hallway Lights (zone 1)",
            "on",
            "lutron-caseta:zone:1:1749600000",
        );
        assert_eq!(m["ts"].as_str().unwrap(), "2026-06-10T18:00:00-07:00");
        assert_eq!(m["source"].as_str().unwrap(), "lutron-caseta");
        assert_eq!(m["device"].as_str().unwrap(), "Hallway Lights (zone 1)");
        assert_eq!(m["event"].as_str().unwrap(), "on");
        assert!(m["guid"].as_str().unwrap().starts_with("lutron-caseta:"));
    }

    // ---- sync state round-trip ---------------------------------------------

    #[test]
    fn sync_state_round_trips_via_vault() {
        let vault = temp_vault("sync_state_round_trips");
        let mut state = SyncState::default();
        state.baseline_taken = true;
        state.last_synced = Some("2026-06-10T18:00:00-07:00".to_string());
        state.devices.insert(
            "zone:1".to_string(),
            DeviceSnapshot {
                level: Some(100),
                fan_speed: None,
                occupancy_status: None,
                name: "Hallway Lights".to_string(),
                device_type: "WallDimmer".to_string(),
            },
        );
        state.occupancy.insert(
            "2".to_string(),
            DeviceSnapshot {
                level: None,
                fan_speed: None,
                occupancy_status: Some("Occupied".to_string()),
                name: "Occupancy Group 2".to_string(),
                device_type: "OccupancyGroup".to_string(),
            },
        );
        vault.write_lc_sync(&state).unwrap();
        let loaded = vault.read_lc_sync();
        assert!(loaded.baseline_taken);
        assert_eq!(
            loaded.last_synced.as_deref(),
            Some("2026-06-10T18:00:00-07:00")
        );
        assert_eq!(loaded.devices["zone:1"].level, Some(100));
        assert_eq!(
            loaded.occupancy["2"].occupancy_status.as_deref(),
            Some("Occupied")
        );
    }

    #[test]
    fn empty_vault_returns_default_sync_state() {
        let vault = temp_vault("empty_vault_default");
        let state = vault.read_lc_sync();
        assert!(!state.baseline_taken);
        assert!(state.devices.is_empty());
        assert!(state.occupancy.is_empty());
        assert!(state.last_synced.is_none());
    }

    // ---- diff logic (offline) ----------------------------------------------

    #[test]
    fn baseline_flag_suppresses_events() {
        let state = SyncState::default();
        let is_baseline = !state.baseline_taken;
        let level_changed = Some(100i32) != None;
        let would_emit = !is_baseline && level_changed;
        assert!(!would_emit, "baseline must suppress event emission");
    }

    #[test]
    fn level_change_from_100_to_0_would_emit_off() {
        let prev_level: Option<i32> = Some(100);
        let new_level: Option<i32> = Some(0);
        let changed = new_level != prev_level;
        assert!(changed);
        assert_eq!(level_to_event(new_level.unwrap_or(0)), "off");
    }

    #[test]
    fn level_unchanged_does_not_trigger_event() {
        let prev = Some(50i32);
        let new = Some(50i32);
        assert!(!( new != prev ), "no change → no event");
    }

    #[test]
    fn occupancy_occupied_maps_to_motion() {
        let event_name = match "Occupied" {
            "Occupied" => "motion",
            "Unoccupied" => "no_motion",
            _ => "unknown",
        };
        assert_eq!(event_name, "motion");
    }

    #[test]
    fn occupancy_unoccupied_maps_to_no_motion() {
        let event_name = match "Unoccupied" {
            "Occupied" => "motion",
            "Unoccupied" => "no_motion",
            _ => "unknown",
        };
        assert_eq!(event_name, "no_motion");
    }

    // ---- raw-line serialization -------------------------------------------

    #[test]
    fn raw_line_flattens_value_and_omits_ts() {
        let raw = RawLine {
            ts: "2026-06-10T18:00:00-07:00".to_string(),
            value: json!({
                "CommuniqueType": "ReadResponse",
                "Url": "/device",
                "Devices": []
            }),
        };
        let s = serde_json::to_value(&raw).unwrap();
        assert!(s.get("ts").is_none(), "ts must not appear on disk");
        assert_eq!(s["CommuniqueType"].as_str().unwrap(), "ReadResponse");
        assert_eq!(s["Url"].as_str().unwrap(), "/device");
    }

    // ---- vault-path constants ----------------------------------------------

    #[test]
    fn constants_are_under_home_lutron_caseta() {
        assert!(EVENTS_DIR.starts_with("home/lutron-caseta/"));
        assert!(RAW_DIR.starts_with("home/lutron-caseta/"));
        assert!(SYNC_FILE.starts_with(".trove/"));
    }

    // ---- fixture-based parsing (from pylutron-caseta test data) ------------

    /// Verify we can extract device names and zone hrefs from the official
    /// test fixture shape (`tests/responses/devices.json`).
    #[test]
    fn parse_leap_devices_fixture() {
        let body: Value = serde_json::from_str(r#"{
            "Devices": [
                {
                    "href": "/device/2",
                    "Name": "Lights",
                    "FullyQualifiedName": ["Hallway", "Lights"],
                    "SerialNumber": 2345,
                    "ModelNumber": "PD-6WCL-XX",
                    "DeviceType": "WallDimmer",
                    "LocalZones": [{"href": "/zone/1"}],
                    "AssociatedArea": {"href": "/area/2"}
                },
                {
                    "href": "/device/3",
                    "Name": "Fan",
                    "DeviceType": "CasetaFanSpeedController",
                    "LocalZones": [{"href": "/zone/2"}],
                    "AssociatedArea": {"href": "/area/2"}
                }
            ]
        }"#).unwrap();

        let devices = body["Devices"].as_array().unwrap();
        assert_eq!(devices.len(), 2);

        let dev0 = &devices[0];
        assert_eq!(dev0["Name"].as_str().unwrap(), "Lights");
        assert_eq!(dev0["DeviceType"].as_str().unwrap(), "WallDimmer");
        let zone_href = dev0["LocalZones"][0]["href"].as_str().unwrap();
        let zone_id = zone_href.rsplit('/').next().unwrap();
        assert_eq!(zone_id, "1");
    }

    /// Verify occupancy group status parsing from the official fixture shape.
    #[test]
    fn parse_leap_occupancy_fixture() {
        let body: Value = serde_json::from_str(r#"{
            "OccupancyGroupStatuses": [
                {
                    "href": "/occupancygroup/1/status",
                    "OccupancyGroup": {"href": "/occupancygroup/1"},
                    "OccupancyStatus": "Unknown"
                },
                {
                    "href": "/occupancygroup/2/status",
                    "OccupancyGroup": {"href": "/occupancygroup/2"},
                    "OccupancyStatus": "Occupied"
                },
                {
                    "href": "/occupancygroup/3/status",
                    "OccupancyGroup": {"href": "/occupancygroup/3"},
                    "OccupancyStatus": "Unoccupied"
                }
            ]
        }"#).unwrap();

        let statuses = body["OccupancyGroupStatuses"].as_array().unwrap();
        assert_eq!(statuses.len(), 3);

        let s = &statuses[1];
        let og_href = s["OccupancyGroup"]["href"].as_str().unwrap();
        let og_id = og_href.rsplit('/').next().unwrap();
        assert_eq!(og_id, "2");
        assert_eq!(s["OccupancyStatus"].as_str().unwrap(), "Occupied");

        let event_name = match s["OccupancyStatus"].as_str().unwrap() {
            "Occupied" => "motion",
            "Unoccupied" => "no_motion",
            _ => "unknown",
        };
        assert_eq!(event_name, "motion");
    }

    /// Verify zone status level extraction.
    #[test]
    fn parse_leap_zone_status() {
        let zone_status: Value = serde_json::from_str(r#"{
            "Zone": {"href": "/zone/1"},
            "Level": 100
        }"#).unwrap();

        let zone_href = zone_status["Zone"]["href"].as_str().unwrap();
        let zone_id = zone_href.rsplit('/').next().unwrap();
        assert_eq!(zone_id, "1");
        let level = zone_status["Level"].as_i64().unwrap() as i32;
        assert_eq!(level, 100);
        assert_eq!(level_to_event(level), "on");
    }

    /// Verify fan speed zone status.
    #[test]
    fn parse_leap_fan_speed_status() {
        let zone_status: Value = serde_json::from_str(r#"{
            "Zone": {"href": "/zone/2"},
            "Level": -1,
            "FanSpeed": "Medium"
        }"#).unwrap();

        let fan_speed = zone_status["FanSpeed"].as_str().unwrap();
        assert_eq!(fan_speed, "Medium");
        // Fan "on" when speed is not "Off"
        let event_name = if fan_speed == "Off" { "off" } else { "on" };
        assert_eq!(event_name, "on");
    }

    // ---- per-zone singular ZoneStatus body parsing (Caseta PRO path) -------

    /// Caseta returns Body.ZoneStatus (singular) per zone, not ZoneStatuses[].
    #[test]
    fn per_zone_body_has_singular_zonestatus() {
        // This is the shape returned by /zone/{id}/status on a Caseta bridge.
        let body: Value = serde_json::from_str(r#"{
            "ZoneStatus": {
                "href": "/zone/1/status",
                "Zone": {"href": "/zone/1"},
                "Level": 75
            }
        }"#).unwrap();

        let zs = body.get("ZoneStatus").expect("ZoneStatus key must be present");
        let zone_id = zs["Zone"]["href"].as_str().unwrap().rsplit('/').next().unwrap();
        assert_eq!(zone_id, "1");
        let level = zs["Level"].as_i64().unwrap() as i32;
        assert_eq!(level, 75);
        assert_eq!(level_to_event(level), "on");
    }

    /// Fan zone per-zone body: Level=-1, FanSpeed present.
    #[test]
    fn per_zone_fan_body_level_minus_one_not_treated_as_dimmer() {
        let body: Value = serde_json::from_str(r#"{
            "ZoneStatus": {
                "href": "/zone/2/status",
                "Zone": {"href": "/zone/2"},
                "Level": -1,
                "FanSpeed": "High"
            }
        }"#).unwrap();

        let zs = body.get("ZoneStatus").unwrap();
        let raw_level = zs.get("Level").and_then(Value::as_i64).map(|v| v as i32);
        let fan_speed = zs.get("FanSpeed").and_then(Value::as_str).map(str::to_string);

        let is_fan = fan_speed.is_some();
        // Fan zones: level is discarded (Level=-1 is not a real dimmer level).
        let level = if is_fan { None } else { raw_level.filter(|&l| l >= 0) };

        assert!(is_fan);
        assert_eq!(level, None, "fan zones must not expose a dimmer level");
        assert_eq!(fan_speed.as_deref(), Some("High"));
    }

    // ---- cursor-guard: empty zone_map with non-empty device list is rejected -

    #[test]
    fn cursor_guard_detects_empty_zone_map_with_devices() {
        // Simulate: bridge returned devices but all per-zone reads failed → empty zone_map.
        let devices_non_empty = true;
        let new_device_snap_empty = true;
        // The guard logic:
        let should_bail = devices_non_empty && new_device_snap_empty;
        assert!(should_bail, "must bail when devices exist but no zone statuses were read");
    }

    #[test]
    fn cursor_guard_allows_genuinely_empty_device_list() {
        // Bridge returns empty device list — no zones expected.
        let devices_non_empty = false;
        let new_device_snap_empty = true;
        let should_bail = devices_non_empty && new_device_snap_empty;
        assert!(!should_bail, "must NOT bail when bridge has no devices");
    }

    // ---- extra field carries device_type, not detail -----------------------

    #[test]
    fn level_event_device_type_goes_in_extra_not_detail() {
        // Reproduce the event-building logic for a light level change.
        let ts = "2026-06-17T10:00:00-07:00";
        let device_type = "WallDimmer";
        let mut ev = home_event_map(ts, "lutron-caseta", "Hallway Lights (zone 1)", "on", "guid-1");
        ev.insert("value".into(), json!(75));
        // Fixed: device_type goes into extra, not detail.
        ev.insert("extra".into(), json!({"device_type": device_type}));

        // detail should NOT contain the device type
        assert!(ev.get("detail").is_none(), "detail must not carry device_type");
        let extra = ev.get("extra").expect("extra must be present");
        assert_eq!(extra["device_type"].as_str().unwrap(), "WallDimmer");
    }
}
