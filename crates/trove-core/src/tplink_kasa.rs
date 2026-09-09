//! TP-Link Kasa smart plugs — local LAN protocol (no cloud needed for Kasa).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/tplink-kasa.md.
//!
//! ## Protocol
//!
//! Kasa devices use a proprietary **XOR autokey cipher** over TCP port 9999,
//! documented by Lubomir Stroetmann and Tobias Esser in their 2016 reverse
//! engineering write-up (softScheck.com). Every message is a 4-byte big-endian
//! length header followed by the XOR-encrypted payload. The XOR stream cipher
//! starts with key byte `0xAB = 171`; each output byte also becomes the key for
//! the next byte (autokey). Discovery uses the same encryption over UDP broadcast
//! to 255.255.255.255:9999, sending `{"system":{"get_sysinfo":{}}}`.
//!
//! Commands and responses are JSON objects: the command selects a module
//! (`"system"`, `"emeter"`) and an action (`"get_sysinfo"`, `"get_realtime"`,
//! `"get_daystat"`, `"get_monthstat"`).
//!
//! ## Data collected
//!
//! Per-device, each poll:
//!
//! 1. `system.get_sysinfo` — alias, model, deviceId, relay_state (on/off), mac,
//!    feature string. All models.
//! 2. `emeter.get_realtime` — `power_mw` (mW), `voltage_mv` (mV), `current_ma`
//!    (mA), `total_wh` (Wh). Older firmware may omit the `_m*` suffix: `power`,
//!    `voltage`, `current`, `total` — the parser handles both.
//! 3. `emeter.get_daystat` and `emeter.get_monthstat` — per-day and per-month
//!    kWh totals (`day_list` / `month_list`; entries carry `energy_wh` or
//!    `energy` in kWh depending on firmware).
//!
//! Only models with the `"ENE"` feature flag in sysinfo support emeter; the
//! collector skips emeter commands for non-ENE devices without erroring.
//!
//! ## Vault layout
//!
//! - **Raw layer** (unconditional): `home/tplink-kasa/raw/YYYY-MM.jsonl` —
//!   one full response object per poll per device, tagged with `ts`.
//! - **Contract layer** (`home/tplink-kasa/YYYY-MM.jsonl`): power/voltage/
//!   current readings fan out into [`HomeReading`] rows (metric=`power`/
//!   `voltage`/`current`, units=`w`/`v`/`a` after conversion from mW/mV/mA),
//!   reusing the bound `home` reading contract (pioneer: `ambient_weather`).
//! - **Energy raw** (`home/tplink-kasa/energy/YYYY-MM.jsonl`): daily and
//!   monthly kWh totals written raw (the `home.energy` shape is an unbound
//!   sibling draft; energy lines follow the draft schema field-for-field for
//!   forward compatibility but are NOT bound via a Rust type — raw only).
//!
//! ## On/Off State
//!
//! `relay_state` (0/1) is captured in the raw layer for all devices.
//! Writing it to a contract shape (home.event on/off rows) is deferred pending
//! the home.event binding — the shape is an unbound sibling draft. On/off
//! history is available from the raw vault but is NOT yet emitted as contract
//! rows; this is a documented gap vs the brief's capability table.
//!
//! ## Cursor
//!
//! A non-secret rebuildable cursor at `.trove/tplink-kasa-sync.json` records:
//! - Per-device (`deviceId`) the last-poll timestamp (RFC3339). For
//!   **instantaneous readings** (power/voltage/current), the first poll
//!   baselines to "now" — no backfill, gaps during app-closure are honest.
//!   For **energy totals** (daily/monthly kWh), the device's full retained
//!   history is pulled on every poll; date-stable guids deduplicate re-runs,
//!   so historical totals accumulate idempotently from the first poll.
//! - The last known device list (IP → deviceId) for stable poll without
//!   broadcast every run. Discovery re-runs when the device list is stale
//!   (> 1 hour since last discovery) or when a configured device is unreachable.
//!
//! ## No CONNECTION
//!
//! Classic Kasa is fully local — no cloud account, no token. The integration
//! has no `ConnectionDef` for v1. Tapo support (encrypted local protocol that
//! requires a one-time cloud credential exchange) is a future slice.

use std::collections::{BTreeMap, HashSet};
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::home::HomeReading;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, Cadence, CollectOutcome, IntegrationDef, PullOutcome};
use crate::store::{write_json_atomic, Partition};
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths and cursor.

const DIR: &str = "home/tplink-kasa";
const RAW_DIR: &str = "home/tplink-kasa/raw";
const ENERGY_DIR: &str = "home/tplink-kasa/energy";
const SYNC_FILE: &str = ".trove/tplink-kasa-sync.json";
const SOURCE: &str = "tplink-kasa";

/// How often to poll devices (5-minute cadence, matching brief).
const POLL_SECS: u64 = 300;

/// UDP broadcast target for discovery.
const BROADCAST_ADDR: &str = "255.255.255.255:9999";
/// TCP port for per-device commands.
const KASA_PORT: u16 = 9999;
/// Socket timeout for TCP connections and UDP receives.
const SOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Discovery receive timeout (slightly longer — wait for all devices on LAN).
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
/// How long device-list cache stays valid before re-discovery (1 hour).
const DISCOVERY_CACHE_SECS: i64 = 3600;

// ---------------------------------------------------------------------------
// XOR autokey cipher — the core of the Kasa LAN protocol.
// Documented by Stroetmann & Esser (2016): key starts at 171 (0xAB),
// each encrypted byte is `key XOR plaintext_byte`, and the key for the next
// byte is the (just-emitted) ciphertext byte (autokey). Decryption is the
// same: `key XOR ciphertext_byte`, next key = ciphertext_byte (not plaintext).

/// Encrypt `data` into the Kasa wire format: 4-byte big-endian length prefix
/// followed by the XOR-encrypted payload.
fn kasa_encrypt(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + data.len());
    let len = data.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    let mut key: u8 = 171;
    for &b in data {
        let enc = b ^ key;
        key = enc;
        out.push(enc);
    }
    out
}

/// Decrypt the XOR payload (everything after the 4-byte length prefix).
fn kasa_decrypt(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut key: u8 = 171;
    for &b in payload {
        out.push(b ^ key);
        key = b;
    }
    out
}

// ---------------------------------------------------------------------------
// Transport layer — injectable for tests.

/// A send-a-command-get-a-response abstraction. Tests inject a mock; the real
/// impl does the XOR-over-TCP dance.
trait KasaTransport: Sync {
    /// Send a JSON command to `device_ip` on the Kasa TCP port and return the
    /// decrypted response as a `Value`. Returns an error when the device is
    /// unreachable or the response is malformed.
    fn send(&self, device_ip: &str, cmd: &Value) -> Result<Value>;
    /// Broadcast a discovery packet and collect responses for up to
    /// [`DISCOVERY_TIMEOUT`]. Returns `(ip, sysinfo)` pairs.
    fn discover(&self) -> Result<Vec<(String, Value)>>;
}

/// The real network implementation.
struct NetworkTransport;

impl KasaTransport for NetworkTransport {
    fn send(&self, device_ip: &str, cmd: &Value) -> Result<Value> {
        let addr = format!("{device_ip}:{KASA_PORT}");
        let mut sock = TcpStream::connect_timeout(
            &addr.parse().context("invalid IP")?,
            SOCK_TIMEOUT,
        )
        .context("TCP connect")?;
        sock.set_read_timeout(Some(SOCK_TIMEOUT)).ok();
        sock.set_write_timeout(Some(SOCK_TIMEOUT)).ok();

        let payload = serde_json::to_vec(cmd).context("serialize cmd")?;
        let enc = kasa_encrypt(&payload);
        sock.write_all(&enc).context("write cmd")?;

        // Read the 4-byte length header then the payload.
        let mut header = [0u8; 4];
        sock.read_exact(&mut header).context("read header")?;
        let len = u32::from_be_bytes(header) as usize;
        // Guard against unreasonably large responses (max 1 MiB).
        if len > 1_048_576 {
            bail!("response too large ({len} bytes)");
        }
        let mut enc_body = vec![0u8; len];
        sock.read_exact(&mut enc_body).context("read body")?;
        let dec = kasa_decrypt(&enc_body);
        let v: Value = serde_json::from_slice(&dec).context("parse response JSON")?;
        Ok(v)
    }

    fn discover(&self) -> Result<Vec<(String, Value)>> {
        // Bind to an ephemeral port; SO_BROADCAST needed for the broadcast.
        let sock = UdpSocket::bind("0.0.0.0:0").context("bind UDP socket")?;
        sock.set_broadcast(true).context("set_broadcast")?;
        sock.set_read_timeout(Some(DISCOVERY_TIMEOUT)).ok();

        // The discovery payload is the encrypted get_sysinfo command (no length
        // prefix for UDP — devices respond to the raw XOR payload directly, but
        // the length-prefixed form also works and is safer across firmware).
        let cmd = json!({"system": {"get_sysinfo": {}}});
        let payload = serde_json::to_vec(&cmd)?;
        let enc = kasa_encrypt(&payload);
        sock.send_to(&enc, BROADCAST_ADDR)
            .context("UDP broadcast send")?;

        // Collect responses until timeout.
        let mut results = Vec::new();
        let mut buf = vec![0u8; 65536];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, peer)) => {
                    let ip = peer.ip().to_string();
                    // Kasa responses may or may not include the 4-byte header.
                    let dec = if n > 4 {
                        let maybe_len = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
                        if maybe_len == n - 4 {
                            kasa_decrypt(&buf[4..n]) // has header
                        } else {
                            kasa_decrypt(&buf[..n]) // no header
                        }
                    } else {
                        kasa_decrypt(&buf[..n])
                    };
                    if let Ok(v) = serde_json::from_slice::<Value>(&dec) {
                        if let Some(info) = v
                            .get("system")
                            .and_then(|s| s.get("get_sysinfo"))
                        {
                            results.push((ip, info.clone()));
                        }
                    }
                }
                Err(_) => break, // timeout or no more packets
            }
        }
        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Cursor state.

/// Per-device last-poll record, keyed by `deviceId`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DeviceState {
    /// Alias / friendly name at last poll.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    alias: String,
    /// Last-known IP address of this device.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ip: String,
    /// RFC3339 timestamp of the last successful poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_polled: Option<String>,
}

/// Non-secret rebuildable cursor.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncState {
    /// `deviceId` → per-device state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    devices: BTreeMap<String, DeviceState>,
    /// RFC3339 time of the last successful discovery scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_discovery: Option<String>,
    /// RFC3339 time of the last successful sync (any device polled).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated: Option<String>,
}

impl Vault {
    fn read_kasa_sync(&self) -> SyncState {
        self.resolve(SYNC_FILE)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn write_kasa_sync(&self, state: &SyncState) -> Result<()> {
        write_json_atomic(&self.resolve(SYNC_FILE)?, state)
    }
}

// ---------------------------------------------------------------------------
// Sysinfo parsing helpers.

/// The `feature` field on real Kasa devices is **colon-delimited** (e.g.
/// `"TIM:ENE"`), as confirmed by python-kasa fixtures (KP125, HS110) and the
/// library source (`kasa/iot/iotdevice.py`: `features.split(":")`).
/// We also accept whitespace as a delimiter for robustness.
/// `ENE` means energy monitoring (emeter) is supported.
fn has_emeter(sysinfo: &Value) -> bool {
    sysinfo
        .get("feature")
        .and_then(Value::as_str)
        .map(|f| f.split(|c: char| c == ':' || c.is_whitespace()).any(|t| t == "ENE"))
        .unwrap_or(false)
}

/// Extract a string field from sysinfo, trimmed. Empty string when absent.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Emeter parsing — handles both old (non-suffixed) and new (_mw/_mv/_ma/_wh)
// firmware response formats. Older devices report in raw floats (W/V/A); newer
// ones report in milli-units and use the `_mw`/`_mv`/`_ma` suffix.

/// Power in watts from `emeter.get_realtime`. Returns `None` when the field is
/// absent in both naming variants.
fn realtime_power_w(rt: &Value) -> Option<f64> {
    // Newer: power_mw (milliwatts) → divide by 1000.
    if let Some(mw) = rt.get("power_mw").and_then(Value::as_f64) {
        return Some(mw / 1000.0);
    }
    // Older: power (watts directly).
    rt.get("power").and_then(Value::as_f64)
}

/// Voltage in volts from `emeter.get_realtime`.
fn realtime_voltage_v(rt: &Value) -> Option<f64> {
    if let Some(mv) = rt.get("voltage_mv").and_then(Value::as_f64) {
        return Some(mv / 1000.0);
    }
    rt.get("voltage").and_then(Value::as_f64)
}

/// Current in amperes from `emeter.get_realtime`.
fn realtime_current_a(rt: &Value) -> Option<f64> {
    if let Some(ma) = rt.get("current_ma").and_then(Value::as_f64) {
        return Some(ma / 1000.0);
    }
    rt.get("current").and_then(Value::as_f64)
}

/// Total energy in kWh from `emeter.get_realtime`. `total_wh` → divide by 1000;
/// `total` is already in kWh on older firmware. Stored raw; not in HomeReading
/// (HomeReading maps instantaneous power/voltage/current, not cumulative energy).
#[allow(dead_code)]
fn realtime_total_kwh(rt: &Value) -> Option<f64> {
    if let Some(wh) = rt.get("total_wh").and_then(Value::as_f64) {
        return Some(wh / 1000.0);
    }
    rt.get("total").and_then(Value::as_f64)
}

/// Per-entry energy in kWh from a `day_list` or `month_list` entry. The field
/// is `energy_wh` (integer Wh) on some firmware, `energy` (float kWh) on others.
fn stat_energy_kwh(entry: &Value) -> Option<f64> {
    if let Some(wh) = entry.get("energy_wh").and_then(Value::as_f64) {
        return Some(wh / 1000.0);
    }
    entry.get("energy").and_then(Value::as_f64)
}

// ---------------------------------------------------------------------------
// Contract rows: power/voltage/current → HomeReading.

/// Map an `emeter.get_realtime` response + device metadata into the
/// [`HomeReading`] contract rows (one per present metric). Guid is
/// `tplink-kasa:{deviceId}:{metric}:{ts_epoch_ms}` for deduplication.
fn readings_from_realtime(
    rt: &Value,
    device_id: &str,
    alias: &str,
    ts: &str,
    ts_ms: i64,
) -> Vec<HomeReading> {
    let metrics: &[(&str, Option<f64>, &str)] = &[
        ("power", realtime_power_w(rt), "w"),
        ("voltage", realtime_voltage_v(rt), "v"),
        ("current", realtime_current_a(rt), "a"),
    ];
    let mut rows = Vec::new();
    for (metric, val, unit) in metrics {
        let Some(v) = val else { continue };
        let mut r = HomeReading::new(SOURCE, *metric, *v, ts);
        r.unit = (*unit).to_string();
        r.device = device_id.to_string();
        r.place = alias.to_string();
        let mut extra = Map::new();
        extra.insert(
            "guid".into(),
            Value::String(format!("tplink-kasa:{device_id}:{metric}:{ts_ms}")),
        );
        r.extra = extra;
        rows.push(r);
    }
    rows
}

// ---------------------------------------------------------------------------
// Energy interval rows (home.energy unbound draft — raw JSONL, not a Rust type).

/// Build a `home.energy`-shaped JSONL row for a daily or monthly stat entry.
/// Fields follow the draft schema field-for-field (source, ts, device, circuit,
/// kwh, interval_secs, direction, guid) for forward compatibility.
/// Returns `None` when the entry has no parseable energy value or valid date.
fn energy_row_from_stat(
    entry: &Value,
    device_id: &str,
    alias: &str,
    kind: &str, // "day" or "month"
) -> Option<Value> {
    let kwh = stat_energy_kwh(entry)?;
    let year = entry.get("year").and_then(Value::as_i64)? as i32;
    let month = entry.get("month").and_then(Value::as_i64)? as u32;
    // Day entries have `day`; month entries use day=1 as the interval start.
    let day = if kind == "day" {
        entry.get("day").and_then(Value::as_i64)? as u32
    } else {
        1
    };
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let ts = format!("{}T00:00:00", date.format("%Y-%m-%d"));

    let interval_secs: u64 = if kind == "day" { 86400 } else { 0 };
    let guid = format!("tplink-kasa:{device_id}:{kind}:{year}-{month:02}-{day:02}");

    let mut row = json!({
        "ts": ts,
        "source": SOURCE,
        "device": device_id,
        "circuit": alias,
        "kwh": kwh,
        "direction": "consumption",
        "guid": guid,
    });
    if interval_secs > 0 {
        if let Some(m) = row.as_object_mut() {
            m.insert("interval_secs".into(), Value::Number(interval_secs.into()));
        }
    }
    Some(row)
}

// ---------------------------------------------------------------------------
// Write helpers.

/// Append contract readings, deduping by `extra.guid` against already-stored rows.
fn append_readings(vault: &Vault, rows: Vec<HomeReading>) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(DIR, Partition::Month);
    // Collect already-stored guids.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(g) = v
                .get("extra")
                .and_then(|e| e.get("guid"))
                .and_then(Value::as_str)
            {
                seen.insert(g.to_string());
            }
        }
    }
    let fresh: Vec<HomeReading> = rows
        .into_iter()
        .filter(|r| {
            let g = r.extra.get("guid").and_then(Value::as_str).unwrap_or("");
            g.is_empty() || seen.insert(g.to_string())
        })
        .collect();
    let n = fresh.len() as u64;
    stream.append(&fresh, |r| &r.ts)?;
    Ok(n)
}

/// Raw line for the poll — includes ts for partitioning.
#[derive(Serialize)]
struct RawLine {
    ts: String,
    #[serde(flatten)]
    data: Value,
}

/// Append a raw poll object under `home/tplink-kasa/raw/YYYY-MM.jsonl`.
fn append_raw(vault: &Vault, ts: &str, data: Value) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let line = RawLine { ts: ts.to_string(), data };
    stream.append(&[line], |r| &r.ts)?;
    Ok(())
}

/// Append a raw energy interval row under `home/tplink-kasa/energy/YYYY-MM.jsonl`.
/// `ts` is the ISO start of the interval (YYYY-MM-DD or YYYY-MM-DDT00:00:00).
#[derive(Serialize)]
struct EnergyLine {
    #[serde(skip)]
    partition_ts: String,
    #[serde(flatten)]
    data: Value,
}

fn append_energy_raw(vault: &Vault, rows: &[Value]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // Dedup by guid before appending.
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
            EnergyLine { partition_ts: ts, data: r.clone() }
        })
        .collect();
    stream.append(&fresh, |r| &r.partition_ts)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Main poll logic.

/// Poll a single device at `ip`, write raw + contract rows. Returns the number
/// of new contract readings written.
fn poll_device(
    vault: &Vault,
    transport: &dyn KasaTransport,
    ip: &str,
    sysinfo: &Value,
    state: &mut SyncState,
) -> Result<u64> {
    let device_id = str_field(sysinfo, "deviceId");
    let alias = str_field(sysinfo, "alias");
    let now = Local::now();
    let ts = now.to_rfc3339();
    let ts_ms = now.timestamp_millis();

    // Update the cursor.
    let dev_entry = state.devices.entry(device_id.clone()).or_default();
    dev_entry.alias = alias.clone();
    dev_entry.ip = ip.to_string();
    dev_entry.last_polled = Some(ts.clone());

    // Raw: start with sysinfo.
    let mut raw_obj = json!({
        "sysinfo": sysinfo.clone(),
    });

    let mut readings: Vec<HomeReading> = Vec::new();
    let mut energy_rows: Vec<Value> = Vec::new();

    // Emeter: only for ENE-capable models.
    if has_emeter(sysinfo) {
        // get_realtime → power/voltage/current readings.
        let rt_cmd = json!({"emeter": {"get_realtime": {}}});
        if let Ok(rt_resp) = transport.send(ip, &rt_cmd) {
            if let Some(rt) = rt_resp
                .get("emeter")
                .and_then(|e| e.get("get_realtime"))
            {
                raw_obj
                    .as_object_mut()
                    .unwrap()
                    .insert("emeter_realtime".into(), rt.clone());
                let rows =
                    readings_from_realtime(rt, &device_id, &alias, &ts, ts_ms);
                readings.extend(rows);
            }
        }

        // get_daystat for the current month → energy raw.
        let now_local = Local::now();
        let day_cmd = json!({
            "emeter": {
                "get_daystat": {
                    "month": now_local.month(),
                    "year": now_local.year()
                }
            }
        });
        if let Ok(day_resp) = transport.send(ip, &day_cmd) {
            if let Some(day_list) = day_resp
                .get("emeter")
                .and_then(|e| e.get("get_daystat"))
                .and_then(|d| d.get("day_list"))
                .and_then(Value::as_array)
            {
                raw_obj
                    .as_object_mut()
                    .unwrap()
                    .insert("emeter_daystat".into(), Value::Array(day_list.clone()));
                for entry in day_list {
                    if let Some(row) = energy_row_from_stat(entry, &device_id, &alias, "day") {
                        energy_rows.push(row);
                    }
                }
            }
        }

        // get_monthstat for the current year → energy raw.
        let month_cmd = json!({
            "emeter": {
                "get_monthstat": { "year": now_local.year() }
            }
        });
        if let Ok(month_resp) = transport.send(ip, &month_cmd) {
            if let Some(month_list) = month_resp
                .get("emeter")
                .and_then(|e| e.get("get_monthstat"))
                .and_then(|d| d.get("month_list"))
                .and_then(Value::as_array)
            {
                raw_obj
                    .as_object_mut()
                    .unwrap()
                    .insert("emeter_monthstat".into(), Value::Array(month_list.clone()));
                for entry in month_list {
                    if let Some(row) = energy_row_from_stat(entry, &device_id, &alias, "month") {
                        energy_rows.push(row);
                    }
                }
            }
        }
    }

    // Write all layers.
    append_raw(vault, &ts, raw_obj)?;
    let n = append_readings(vault, readings)?;
    append_energy_raw(vault, &energy_rows)?;

    Ok(n)
}

/// Discover devices and poll each one. Returns total new contract readings written.
fn pull_with(vault: &Vault, transport: &dyn KasaTransport) -> Result<PullOutcome> {
    let mut state = vault.read_kasa_sync();
    let now = Local::now();

    // Decide whether to re-discover: stale cache or first run.
    let needs_discovery = state.last_discovery.as_deref().map_or(true, |ts| {
        DateTime::parse_from_rfc3339(ts).map_or(true, |t| {
            now.timestamp() - t.timestamp() > DISCOVERY_CACHE_SECS
        })
    });

    if needs_discovery {
        let found = transport.discover()?;
        for (ip, sysinfo) in &found {
            let device_id = str_field(sysinfo, "deviceId");
            if device_id.is_empty() {
                continue;
            }
            let dev = state.devices.entry(device_id).or_default();
            dev.ip = ip.clone();
        }
        state.last_discovery = Some(now.to_rfc3339());
    }

    // Poll each known device.
    let mut total = 0u64;
    let device_ids: Vec<String> = state.devices.keys().cloned().collect();
    for device_id in device_ids {
        let ip = state.devices[&device_id].ip.clone();
        if ip.is_empty() {
            continue;
        }
        // Fresh sysinfo per poll (device may have changed alias/state).
        let sysinfo_cmd = json!({"system": {"get_sysinfo": {}}});
        match transport.send(&ip, &sysinfo_cmd) {
            Ok(resp) => {
                if let Some(info) = resp.get("system").and_then(|s| s.get("get_sysinfo")) {
                    let n = poll_device(vault, transport, &ip, info, &mut state)?;
                    total += n;
                }
            }
            Err(_) => {
                // Device unreachable — skip this cycle, mark as missing.
                // Don't error: a device being off is normal.
            }
        }
    }

    state.updated = Some(now.to_rfc3339());
    vault.write_kasa_sync(&state)?;

    Ok(PullOutcome {
        headline: format!("{total} readings"),
        counts: BTreeMap::from([("readings", total)]),
    })
}

/// Public pull entry point used by the registry and "Sync now".
pub fn pull(vault: &Vault) -> Result<PullOutcome> {
    pull_with(vault, &NetworkTransport)
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    // For the periodic path, a missing network (no devices found) is a silent
    // no-op — the watcher loop never errors on LAN availability.
    match pull(vault) {
        Ok(out) => {
            let n = out.counts.get("readings").copied().unwrap_or(0);
            Ok(CollectOutcome::note_if(n > 0, || {
                format!("kasa synced — {n} readings")
            }))
        }
        Err(_) => Ok(CollectOutcome::quiet()),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let out = pull(vault)?;
    let n = out.counts.get("readings").copied().unwrap_or(0);
    let headline = if n == 0 {
        "TP-Link Kasa — no new readings (no ENE devices on LAN, or nothing changed)".to_string()
    } else {
        format!("TP-Link Kasa synced — {n} readings")
    };
    Ok(PullOutcome { headline, counts: out.counts })
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tplink-kasa",
        name: "TP-Link Kasa / Tapo",
        kind: IntegrationKind::LocalSync,
        default_on: false,
        description:
            "Polls TP-Link Kasa smart plugs on the local network every 5 minutes \
             for power (W), voltage (V), current (A), and daily/monthly energy totals. \
             No cloud account needed — all communication is LAN-only. Requires energy-\
             monitoring hardware (HS110, KP115, KP125, EP25) for power data; on/off \
             state works for all models. Tapo (encrypted protocol) is a future slice.",
        domain: "home",
        vault_path: "home/tplink-kasa/",
        toggleable: false,
        setup: &[
            "Ensure your Kasa plugs are on the same Wi-Fi network as this Mac.",
            "Enable Trove — it will discover and poll plugs automatically.",
            "Energy data (W/V/A) requires a model with energy monitoring: HS110, KP115, KP125, or EP25.",
        ],
        caveats: "Only emeter-capable models (HS110, KP115, KP125, EP25) report power data; \
                  other models record on/off state only. Some firmware versions removed the \
                  local API — those devices will not appear. Tapo (separate encrypted \
                  protocol) is not yet supported.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(POLL_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-kasa-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // XOR cipher round-trips.

    #[test]
    fn encrypt_decrypt_round_trips() {
        let msg = b"{\"system\":{\"get_sysinfo\":{}}}";
        let enc = kasa_encrypt(msg);
        // First 4 bytes = big-endian length of the payload.
        assert_eq!(
            u32::from_be_bytes(enc[..4].try_into().unwrap()) as usize,
            msg.len()
        );
        let dec = kasa_decrypt(&enc[4..]);
        assert_eq!(dec, msg);
    }

    #[test]
    fn encrypt_starts_with_correct_key() {
        // The cipher starts with key=171; first encrypted byte = 171 XOR first plaintext byte.
        let msg = b"A"; // 0x41
        let enc = kasa_encrypt(msg);
        assert_eq!(enc[4], 171u8 ^ 0x41, "first byte XOR'd with key=171");
    }

    #[test]
    fn decrypt_handles_empty_payload() {
        // Empty slice → empty output (no panic).
        assert_eq!(kasa_decrypt(&[]), Vec::<u8>::new());
    }

    // -----------------------------------------------------------------------
    // Sysinfo helpers.

    fn sysinfo_ene() -> Value {
        // Real Kasa devices delimit feature tokens with COLON, e.g. "TIM:ENE".
        // This matches python-kasa fixtures (KP125(US), HS110(EU)) and the
        // library's own parser (`features.split(":")`).
        json!({
            "alias": "Coffee Maker",
            "deviceId": "8006B4DE7CD4DA14CE97B5A87CEE3B8D17B9A8C1",
            "model": "KP125(US)",
            "relay_state": 1,
            "mac": "50:D4:F7:AB:CD:EF",
            "feature": "TIM:ENE"
        })
    }

    fn sysinfo_no_ene() -> Value {
        json!({
            "alias": "Lamp",
            "deviceId": "AABBCCDDEEFF001122334455667788990011AABB",
            "model": "HS100(US)",
            "relay_state": 0,
            "mac": "50:D4:F7:11:22:33",
            "feature": "TIM"
        })
    }

    #[test]
    fn has_emeter_detects_ene_feature() {
        assert!(has_emeter(&sysinfo_ene()));
        assert!(!has_emeter(&sysinfo_no_ene()));
        // Missing feature field → false (no panic).
        assert!(!has_emeter(&json!({})));
    }

    // -----------------------------------------------------------------------
    // Emeter parsing — both new (_mw/_mv/_ma/_wh) and old (bare) formats.

    fn realtime_new() -> Value {
        // KP125 style: milliwatt units.
        json!({
            "power_mw": 1234.5,
            "voltage_mv": 120456.0,
            "current_ma": 10300.0,
            "total_wh": 50000.0
        })
    }

    fn realtime_old() -> Value {
        // HS110 older firmware: bare watts.
        json!({
            "power": 1.2345,
            "voltage": 120.456,
            "current": 10.3,
            "total": 50.0
        })
    }

    #[test]
    fn realtime_new_format_parses_correctly() {
        let rt = realtime_new();
        let p = realtime_power_w(&rt).unwrap();
        let v = realtime_voltage_v(&rt).unwrap();
        let c = realtime_current_a(&rt).unwrap();
        let e = realtime_total_kwh(&rt).unwrap();
        // 1234.5 mW → 1.2345 W (within floating-point tolerance)
        assert!((p - 1.2345).abs() < 0.001, "power: {p}");
        // 120456.0 mV → 120.456 V
        assert!((v - 120.456).abs() < 0.001, "voltage: {v}");
        // 10300.0 mA → 10.3 A
        assert!((c - 10.3).abs() < 0.001, "current: {c}");
        // 50000.0 Wh → 50.0 kWh
        assert!((e - 50.0).abs() < 0.001, "energy: {e}");
    }

    #[test]
    fn realtime_old_format_parses_correctly() {
        let rt = realtime_old();
        assert!((realtime_power_w(&rt).unwrap() - 1.2345).abs() < 0.001);
        assert!((realtime_voltage_v(&rt).unwrap() - 120.456).abs() < 0.001);
        assert!((realtime_current_a(&rt).unwrap() - 10.3).abs() < 0.001);
        assert!((realtime_total_kwh(&rt).unwrap() - 50.0).abs() < 0.001);
    }

    #[test]
    fn realtime_missing_fields_return_none() {
        assert!(realtime_power_w(&json!({})).is_none());
        assert!(realtime_voltage_v(&json!({})).is_none());
        assert!(realtime_current_a(&json!({})).is_none());
        assert!(realtime_total_kwh(&json!({})).is_none());
    }

    // -----------------------------------------------------------------------
    // readings_from_realtime → HomeReading rows.

    #[test]
    fn readings_from_realtime_emits_three_rows() {
        let rt = realtime_new();
        let ts = "2026-06-17T10:00:00-07:00";
        let rows = readings_from_realtime(&rt, "DEV123", "Coffee Maker", ts, 1750168800000);
        assert_eq!(rows.len(), 3, "power + voltage + current");
        let by_metric: BTreeMap<&str, &HomeReading> =
            rows.iter().map(|r| (r.metric.as_str(), r)).collect();
        assert_eq!(by_metric["power"].unit, "w");
        assert_eq!(by_metric["voltage"].unit, "v");
        assert_eq!(by_metric["current"].unit, "a");
        // Source and device stamped.
        for r in &rows {
            assert_eq!(r.source, SOURCE);
            assert_eq!(r.device, "DEV123");
            assert_eq!(r.place, "Coffee Maker");
            assert_eq!(r.ts, ts);
        }
        // Guid is stable and device-scoped.
        let power_guid = by_metric["power"]
            .extra
            .get("guid")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(power_guid, "tplink-kasa:DEV123:power:1750168800000");
    }

    #[test]
    fn readings_from_realtime_handles_partial_fields() {
        // Only power present — emits only one row.
        let rt = json!({"power_mw": 500.0});
        let rows = readings_from_realtime(&rt, "D", "Lamp", "2026-06-17T10:00:00-07:00", 0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].metric, "power");
    }

    // -----------------------------------------------------------------------
    // energy_row_from_stat.

    #[test]
    fn energy_row_day_stat_correct_fields() {
        let entry = json!({"year": 2026, "month": 6, "day": 17, "energy_wh": 1250});
        let row = energy_row_from_stat(&entry, "DEV123", "Coffee Maker", "day").unwrap();
        assert_eq!(row["ts"], "2026-06-17T00:00:00");
        assert_eq!(row["source"], SOURCE);
        assert_eq!(row["device"], "DEV123");
        // 1250 Wh → 1.25 kWh
        assert!((row["kwh"].as_f64().unwrap() - 1.25).abs() < 0.001);
        assert_eq!(row["direction"], "consumption");
        assert_eq!(row["guid"], "tplink-kasa:DEV123:day:2026-06-17");
        assert_eq!(row["interval_secs"], 86400);
    }

    #[test]
    fn energy_row_month_stat_correct_fields() {
        // Newer format: energy in kWh directly.
        let entry = json!({"year": 2026, "month": 6, "energy": 42.5});
        let row = energy_row_from_stat(&entry, "DEV123", "Coffee Maker", "month").unwrap();
        assert_eq!(row["ts"], "2026-06-01T00:00:00");
        assert!((row["kwh"].as_f64().unwrap() - 42.5).abs() < 0.001);
        assert_eq!(row["guid"], "tplink-kasa:DEV123:month:2026-06-01");
        // Month rows have no interval_secs.
        assert!(row.get("interval_secs").is_none());
    }

    #[test]
    fn energy_row_missing_energy_returns_none() {
        // No energy field → None (no panic).
        let entry = json!({"year": 2026, "month": 6, "day": 17});
        assert!(energy_row_from_stat(&entry, "D", "L", "day").is_none());
    }

    // -----------------------------------------------------------------------
    // Mock transport + full poll_with integration.

    /// Scripted mock: returns fixed sysinfo for one device on discovery,
    /// and scripted responses per command pattern.
    struct MockTransport {
        sysinfo: Value,
        realtime: Value,
        daystat: Value,
        monthstat: Value,
    }

    impl KasaTransport for MockTransport {
        fn send(&self, _ip: &str, cmd: &Value) -> Result<Value> {
            // Route by which subkey is present in the command.
            if cmd.get("system").is_some() {
                return Ok(json!({"system": {"get_sysinfo": self.sysinfo.clone()}}));
            }
            if let Some(emeter) = cmd.get("emeter") {
                if emeter.get("get_realtime").is_some() {
                    return Ok(json!({"emeter": {"get_realtime": self.realtime.clone()}}));
                }
                if emeter.get("get_daystat").is_some() {
                    return Ok(json!({"emeter": {"get_daystat": {"day_list": self.daystat.clone()}}}));
                }
                if emeter.get("get_monthstat").is_some() {
                    return Ok(json!({"emeter": {"get_monthstat": {"month_list": self.monthstat.clone()}}}));
                }
            }
            bail!("unexpected command")
        }
        fn discover(&self) -> Result<Vec<(String, Value)>> {
            Ok(vec![("192.168.1.100".to_string(), self.sysinfo.clone())])
        }
    }

    fn mock_transport() -> MockTransport {
        MockTransport {
            sysinfo: sysinfo_ene(),
            realtime: realtime_new(),
            daystat: json!([
                {"year": 2026, "month": 6, "day": 16, "energy_wh": 800},
                {"year": 2026, "month": 6, "day": 17, "energy_wh": 1250}
            ]),
            monthstat: json!([
                {"year": 2026, "month": 5, "energy": 30.0},
                {"year": 2026, "month": 6, "energy": 12.5}
            ]),
        }
    }

    #[test]
    fn full_pull_writes_all_three_layers() {
        let v = temp_vault("full");
        let t = mock_transport();
        let out = pull_with(&v, &t).unwrap();

        // 3 readings (power + voltage + current) for the one device.
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 3);

        // Contract layer: 3 HomeReading rows.
        let stream = v.stream(DIR, Partition::Month);
        let readings: Vec<HomeReading> = stream
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|k| stream.read::<HomeReading>(&k).unwrap())
            .collect();
        assert_eq!(readings.len(), 3);

        // Raw layer: at least one raw object.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let raw_rows: Vec<Value> = raw
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|k| raw.read::<Value>(&k).unwrap())
            .collect();
        assert!(!raw_rows.is_empty(), "raw layer written");
        // The raw object includes sysinfo and emeter fields.
        assert!(raw_rows[0].get("sysinfo").is_some());
        assert!(raw_rows[0].get("emeter_realtime").is_some());

        // Energy layer: 4 rows (2 day + 2 month).
        let energy = v.stream(ENERGY_DIR, Partition::Month);
        let energy_rows: Vec<Value> = energy
            .partitions()
            .unwrap()
            .into_iter()
            .flat_map(|k| energy.read::<Value>(&k).unwrap())
            .collect();
        assert_eq!(energy_rows.len(), 4, "2 day + 2 month energy rows");
        // Check one day row.
        let day_rows: Vec<&Value> = energy_rows
            .iter()
            .filter(|r| r.get("guid").and_then(Value::as_str).map_or(false, |g| g.contains(":day:")))
            .collect();
        assert_eq!(day_rows.len(), 2);
        // Check one month row.
        let month_rows: Vec<&Value> = energy_rows
            .iter()
            .filter(|r| r.get("guid").and_then(Value::as_str).map_or(false, |g| g.contains(":month:")))
            .collect();
        assert_eq!(month_rows.len(), 2);

        // Cursor updated.
        let state = v.read_kasa_sync();
        assert!(state.updated.is_some());
        let dev_id = "8006B4DE7CD4DA14CE97B5A87CEE3B8D17B9A8C1";
        assert!(state.devices.contains_key(dev_id));
        assert_eq!(state.devices[dev_id].alias, "Coffee Maker");
    }

    #[test]
    fn re_run_with_same_guid_deduplicates() {
        // Deduplication operates on guid = "tplink-kasa:{device_id}:{metric}:{ts_ms}".
        // Two runs at different wall-clock times emit different guids (new readings).
        // But if the SAME guid is appended twice, the second append is a no-op.
        let v = temp_vault("dedup");
        // Manually write two readings with fixed guids.
        use crate::store::Partition;
        let stream = v.stream(DIR, Partition::Month);
        let mut r1 = HomeReading::new(SOURCE, "power", 1.0, "2026-06-17T10:00:00-07:00");
        let mut extra1 = Map::new();
        extra1.insert("guid".into(), Value::String("tplink-kasa:DEV:power:100".to_string()));
        r1.extra = extra1;
        stream.append(&[r1.clone()], |r| &r.ts).unwrap();

        // Re-append the same reading → dedup rejects it.
        let rows_before: Vec<HomeReading> = stream
            .partitions().unwrap().into_iter()
            .flat_map(|k| stream.read::<HomeReading>(&k).unwrap())
            .collect();
        assert_eq!(rows_before.len(), 1);

        let n = append_readings(&v, vec![r1]).unwrap();
        assert_eq!(n, 0, "identical guid rejected by dedup");

        let rows_after: Vec<HomeReading> = stream
            .partitions().unwrap().into_iter()
            .flat_map(|k| stream.read::<HomeReading>(&k).unwrap())
            .collect();
        assert_eq!(rows_after.len(), 1, "still only one row");
    }

    #[test]
    fn no_ene_device_writes_raw_only() {
        let v = temp_vault("noene");
        let t = MockTransport {
            sysinfo: sysinfo_no_ene(),
            realtime: realtime_new(),
            daystat: json!([]),
            monthstat: json!([]),
        };
        let out = pull_with(&v, &t).unwrap();
        // No contract readings (non-ENE device has no emeter).
        assert_eq!(out.counts.get("readings").copied().unwrap_or(0), 0);
        // But raw layer is written.
        let raw = v.stream(RAW_DIR, Partition::Month);
        let raw_count: usize = raw
            .partitions()
            .unwrap()
            .iter()
            .map(|k| raw.read::<Value>(k).unwrap().len())
            .sum();
        assert!(raw_count > 0, "raw layer written even for non-ENE device");
    }

    #[test]
    fn cursor_back_compat_empty_and_partial_deserialize() {
        // An empty cursor deserializes to all-defaults (first sync).
        let empty: SyncState = serde_json::from_str("{}").unwrap();
        assert!(empty.devices.is_empty());
        assert!(empty.updated.is_none());

        // Partial cursor with only `devices` (no `updated`) still loads.
        let partial: SyncState =
            serde_json::from_str(r#"{"devices":{"DEV1":{"alias":"Lamp","ip":"192.168.1.50"}}}"#)
                .unwrap();
        assert_eq!(partial.devices.get("DEV1").unwrap().alias, "Lamp");
        assert!(partial.updated.is_none());
    }

    #[test]
    fn stat_energy_kwh_handles_both_formats() {
        // energy_wh (integer Wh).
        let e1 = json!({"energy_wh": 2500});
        assert!((stat_energy_kwh(&e1).unwrap() - 2.5).abs() < 0.001);
        // energy (float kWh).
        let e2 = json!({"energy": 2.5});
        assert!((stat_energy_kwh(&e2).unwrap() - 2.5).abs() < 0.001);
        // Neither → None.
        assert!(stat_energy_kwh(&json!({})).is_none());
    }

    #[test]
    fn def_exposes_periodic_behavior_and_no_connection() {
        assert!(matches!(DEF.behavior, Behavior::Periodic { .. }));
        assert!(DEF.connection.is_none(), "Kasa v1 is connectionless");
        assert_eq!(DEF.meta.id, "tplink-kasa");
        assert_eq!(DEF.meta.domain, "home");
    }
}
