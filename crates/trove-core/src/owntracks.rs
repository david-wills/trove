//! OwnTracks — live GPS receiver for the OwnTracks iOS/Android app in HTTP mode.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/owntracks.md.
//!
//! OwnTracks POSTs JSON payloads to a user-configured HTTP endpoint. Trove's
//! `troved` daemon exposes that endpoint and feeds payloads to this module via
//! the public [`ingest`] function. The [`LiveCollector`] drain-loop writes
//! whatever the HTTP side deposits into the vault.
//!
//! # Two payload types handled
//!
//! - `_type: "location"` — a GPS fix. Required fields: `lat`, `lon`, `tst`.
//!   Optional: `acc` (accuracy, m), `alt` (altitude, m), `vel` (speed, km/h),
//!   `cog` (course over ground, °), `batt`/`bs` (battery %), `tid`, `topic`,
//!   `inregions`, `SSID`, `BSSID`, `conn`, `t` (trigger), `m` (monitoring
//!   mode), `created_at`, `motionactivities`, `_id`, `p`, `poi`, `vac`, etc.
//! - `_type: "transition"` — region enter/leave event. Required: `event`,
//!   `tst`, `wtst`, `acc`. Optional: `lat`, `lon`, `desc`, `tid`, `t`, `rid`.
//!   Stored as a raw line AND, when lat/lon are present, as a Fix with
//!   `extra.ot_event="enter"|"leave"` so the trail view shows geofence touches.
//!
//! All other `_type` values (lwt, waypoint, configuration, status, beacon,
//! cmd, steps, card, waypoints, encrypted, request) are stored raw-only.
//!
//! # Vault layout
//!
//! - **Raw:** `location/owntracks/raw/YYYY-MM.jsonl` — every payload verbatim,
//!   one JSON object per line, month-partitioned by `tst`.
//! - **Contract:** `location/owntracks/YYYY-MM-DD.jsonl` — one [`Fix`] row per
//!   location fix (and per transition that carries lat/lon), day-partitioned by
//!   local `ts`. Deduplicated by `guid` = `ot-{tst}-{lat6}-{lon6}`.
//!
//! # Deduplication
//!
//! The HTTP endpoint is append-only. `guid` is `ot-{tst}-{lat}-{lon}` (6dp)
//! so resends of the same fix are silently dropped. No cursor: the phone
//! owns send-state; the receiver just dedupes on ingest.
//!
//! # Privacy
//!
//! Continuous location trail — `default_on: false`, never toggleable without
//! explicit user acknowledgement. See the brief for the privacy-gate notes.

use std::collections::HashSet;
use std::sync::mpsc::{self, Receiver, Sender};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::integrations::{Integration, IntegrationKind};
use crate::location::Fix;
use crate::registry::{Behavior, IntegrationDef, LiveCollector};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths

/// Day-partitioned contract stream (Fix rows).
const DIR: &str = "location/owntracks";
/// Month-partitioned raw stream (verbatim payloads).
const RAW_DIR: &str = "location/owntracks/raw";
/// `source` field value on every Fix we emit.
const SOURCE: &str = "owntracks";

// ---------------------------------------------------------------------------
// Parsed OwnTracks payloads

/// A parsed `_type:location` payload (documented OwnTracks JSON booklet).
///
/// Field names match the OwnTracks spec exactly.
#[derive(Debug, Clone, Deserialize)]
pub struct OtLocation {
    pub lat: f64,
    pub lon: f64,
    /// GPS fix timestamp (Unix epoch seconds, UTC).
    pub tst: i64,
    /// Horizontal accuracy, meters (omitted / 0 when unknown).
    #[serde(default)]
    pub acc: Option<i64>,
    /// Altitude above sea level, meters.
    #[serde(default)]
    pub alt: Option<i64>,
    /// Ground speed, km/h (omitted / 0 when unknown or stationary).
    #[serde(default)]
    pub vel: Option<i64>,
    /// Course over ground, degrees 0–360.
    #[serde(default)]
    pub cog: Option<i64>,
    /// Vertical accuracy, meters.
    #[serde(default)]
    pub vac: Option<i64>,
    /// Battery level, percent.
    #[serde(default)]
    pub batt: Option<i64>,
    /// Battery status: 0=unknown, 1=unplugged, 2=charging, 3=full.
    #[serde(default)]
    pub bs: Option<i64>,
    /// Trigger character: p=ping/manual, c=significant-change, C=circular-region,
    /// b=beacon, r=report-location, u=user, t=timer (move mode), v=frequent-locations (iOS).
    #[serde(default)]
    pub t: Option<String>,
    /// Tracker ID (2-character initials in HTTP mode).
    #[serde(default)]
    pub tid: Option<String>,
    /// MQTT topic (HTTP mode only).
    #[serde(default)]
    pub topic: Option<String>,
    /// Region names the device is currently inside.
    #[serde(default)]
    pub inregions: Option<Vec<String>>,
    /// Region IDs the device is currently inside.
    #[serde(default)]
    pub inrids: Option<Vec<String>>,
    /// WiFi SSID.
    #[serde(rename = "SSID", default)]
    pub ssid: Option<String>,
    /// WiFi BSSID.
    #[serde(rename = "BSSID", default)]
    pub bssid: Option<String>,
    /// Connectivity: w=WiFi, o=offline, m=mobile.
    #[serde(default)]
    pub conn: Option<String>,
    /// Monitoring mode: 1=significant, 2=move.
    #[serde(default)]
    pub m: Option<i64>,
    /// Message construction timestamp (epoch seconds).
    #[serde(default)]
    pub created_at: Option<i64>,
    /// Motion activities (iOS).
    #[serde(default)]
    pub motionactivities: Option<Vec<String>>,
    /// Android message correlation id.
    #[serde(rename = "_id", default)]
    pub msg_id: Option<String>,
    /// Barometric pressure, kPa (extended data, iOS).
    #[serde(default)]
    pub p: Option<f64>,
    /// Point of interest name.
    #[serde(default)]
    pub poi: Option<String>,
    /// Tag name.
    #[serde(default)]
    pub tag: Option<String>,
    /// Radius of a region on enter/leave (in location payloads), meters.
    #[serde(default)]
    pub rad: Option<i64>,
}

/// A parsed `_type:transition` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct OtTransition {
    /// "enter" or "leave".
    pub event: String,
    /// Event timestamp (Unix epoch seconds, UTC).
    pub tst: i64,
    /// Waypoint creation timestamp (Unix epoch seconds).
    pub wtst: i64,
    /// Location accuracy at the event, meters.
    pub acc: i64,
    /// Location at the event — optional in the spec.
    #[serde(default)]
    pub lat: Option<f64>,
    #[serde(default)]
    pub lon: Option<f64>,
    /// Waypoint name.
    #[serde(default)]
    pub desc: Option<String>,
    /// Tracker ID.
    #[serde(default)]
    pub tid: Option<String>,
    /// Trigger character: c=circular-region, b=beacon, l=location.
    #[serde(default)]
    pub t: Option<String>,
    /// Region ID (since Jan 2021).
    #[serde(default)]
    pub rid: Option<String>,
}

/// The three categories we branch on; everything else is raw-only.
#[derive(Debug, Clone)]
pub(crate) enum OtPayload {
    Location(OtLocation),
    Transition(OtTransition),
    /// Any other `_type` value — preserved verbatim as raw, no contract row.
    Other,
}

/// Parse a raw JSON payload into one of the three categories.
pub(crate) fn parse_payload(v: &Value) -> OtPayload {
    let ty = v.get("_type").and_then(Value::as_str).unwrap_or("");
    match ty {
        "location" => match serde_json::from_value::<OtLocation>(v.clone()) {
            Ok(loc) => OtPayload::Location(loc),
            Err(_) => OtPayload::Other, // malformed — preserve raw only
        },
        "transition" => match serde_json::from_value::<OtTransition>(v.clone()) {
            Ok(tr) => OtPayload::Transition(tr),
            Err(_) => OtPayload::Other,
        },
        _ => OtPayload::Other,
    }
}

// ---------------------------------------------------------------------------
// Fix construction

/// Convert a Unix epoch seconds timestamp to an RFC3339 local string.
fn tst_to_rfc3339(tst: i64) -> Option<String> {
    Local.timestamp_opt(tst, 0).single().map(|dt| dt.to_rfc3339())
}

/// Stable guid for a location fix: `ot-{tst}-{lat6}-{lon6}`.
/// lat/lon are formatted to 6 decimal places (~11 cm precision).
pub(crate) fn location_guid(tst: i64, lat: f64, lon: f64) -> String {
    format!("ot-{tst}-{:.6}-{:.6}", lat, lon)
}

/// Build a [`Fix`] from a parsed `_type:location` payload.
///
/// Returns `None` only if `tst` is not a representable local time.
pub(crate) fn location_to_fix(loc: &OtLocation) -> Option<Fix> {
    let ts = tst_to_rfc3339(loc.tst)?;
    let guid = location_guid(loc.tst, loc.lat, loc.lon);

    // Only the valid non-zero optional numerics go into the contract columns.
    // vel is km/h in OwnTracks; Fix.speed is m/s — convert.
    // OwnTracks omits acc/vel/cog/vac from the payload when they are zero, so
    // these filters are purely defensive (they guard against malformed senders
    // or edge-case device firmware). A genuine cog=0 (due north) cannot occur
    // per the booklet since the field is absent when unavailable.
    let speed = loc.vel.filter(|&v| v != 0).map(|v| v as f64 / 3.6);
    let accuracy = loc.acc.filter(|&v| v > 0).map(|v| v as f64);
    let ele = loc.alt.map(|v| v as f64);
    let heading = loc.cog.filter(|&v| v != 0).map(|v| v as f64);

    // Source-specific overflow → extra (full fidelity; never drop what OwnTracks sent).
    let mut extra = Map::new();
    if let Some(b) = loc.batt {
        extra.insert("batt".into(), b.into());
    }
    if let Some(bs) = loc.bs {
        extra.insert("bs".into(), bs.into());
    }
    if let Some(t) = &loc.t {
        extra.insert("t".into(), t.clone().into());
    }
    if let Some(tid) = &loc.tid {
        extra.insert("tid".into(), tid.clone().into());
    }
    if let Some(topic) = &loc.topic {
        extra.insert("topic".into(), topic.clone().into());
    }
    if let Some(regions) = &loc.inregions {
        if !regions.is_empty() {
            extra.insert(
                "inregions".into(),
                Value::Array(regions.iter().map(|r| Value::String(r.clone())).collect()),
            );
        }
    }
    if let Some(rids) = &loc.inrids {
        if !rids.is_empty() {
            extra.insert(
                "inrids".into(),
                Value::Array(rids.iter().map(|r| Value::String(r.clone())).collect()),
            );
        }
    }
    if let Some(ssid) = &loc.ssid {
        extra.insert("SSID".into(), ssid.clone().into());
    }
    if let Some(bssid) = &loc.bssid {
        extra.insert("BSSID".into(), bssid.clone().into());
    }
    if let Some(conn) = &loc.conn {
        extra.insert("conn".into(), conn.clone().into());
    }
    if let Some(vac) = loc.vac {
        extra.insert("vac".into(), vac.into());
    }
    if let Some(p) = loc.p {
        extra.insert("p".into(), p.into());
    }
    if let Some(poi) = &loc.poi {
        extra.insert("poi".into(), poi.clone().into());
    }
    if let Some(tag) = &loc.tag {
        extra.insert("tag".into(), tag.clone().into());
    }
    if let Some(rad) = loc.rad {
        extra.insert("rad".into(), rad.into());
    }
    if let Some(m_val) = loc.m {
        extra.insert("m".into(), m_val.into());
    }
    if let Some(acts) = &loc.motionactivities {
        if !acts.is_empty() {
            extra.insert(
                "motionactivities".into(),
                Value::Array(acts.iter().map(|a| Value::String(a.clone())).collect()),
            );
        }
    }
    if let Some(created) = loc.created_at {
        extra.insert("created_at".into(), created.into());
    }
    if let Some(id) = &loc.msg_id {
        extra.insert("_id".into(), id.clone().into());
    }

    Some(Fix {
        ts,
        source: SOURCE.into(),
        lat: loc.lat,
        lon: loc.lon,
        ele,
        speed,
        accuracy,
        heading,
        trail: String::new(), // raw loggers omit trail; reader segments by time gaps
        trail_name: String::new(),
        mode: String::new(), // OwnTracks doesn't classify movement mode
        guid,
        extra,
    })
}

/// Build a [`Fix`] from a parsed `_type:transition` payload, if lat/lon present.
///
/// Returns `None` when lat/lon are absent (transition without location context)
/// or when `tst` is not representable.
pub(crate) fn transition_to_fix(tr: &OtTransition) -> Option<Fix> {
    let lat = tr.lat?;
    let lon = tr.lon?;
    let ts = tst_to_rfc3339(tr.tst)?;
    let guid = format!("ot-tr-{}-{:.6}-{:.6}", tr.tst, lat, lon);

    let mut extra = Map::new();
    extra.insert("ot_event".into(), tr.event.clone().into());
    if let Some(desc) = &tr.desc {
        extra.insert("ot_region".into(), desc.clone().into());
    }
    if let Some(rid) = &tr.rid {
        extra.insert("ot_rid".into(), rid.clone().into());
    }
    extra.insert("wtst".into(), tr.wtst.into());
    if let Some(t) = &tr.t {
        extra.insert("t".into(), t.clone().into());
    }
    if let Some(tid) = &tr.tid {
        extra.insert("tid".into(), tid.clone().into());
    }

    Some(Fix {
        ts,
        source: SOURCE.into(),
        lat,
        lon,
        ele: None,
        speed: None,
        accuracy: Some(tr.acc as f64),
        heading: None,
        trail: String::new(),
        trail_name: String::new(),
        mode: String::new(),
        guid,
        extra,
    })
}

// ---------------------------------------------------------------------------
// Vault writes

/// A thin wrapper that carries a raw JSON payload plus its RFC3339 ts for
/// the stream's `ts` extractor. The payload is stored verbatim (flattened).
#[derive(Serialize, Deserialize)]
struct RawLine {
    ts: String,
    #[serde(flatten)]
    payload: Value,
}

/// Write a raw payload to `location/owntracks/raw/YYYY-MM.jsonl`.
///
/// `ts` is the RFC3339 local time derived from `tst`; falls back to now if
/// `tst` is absent / unrepresentable.
fn write_raw(vault: &Vault, payload: Value, ts: &str) -> Result<()> {
    let line = RawLine { ts: ts.to_string(), payload };
    vault.stream(RAW_DIR, Partition::Month).append(&[line], |r| &r.ts)?;
    Ok(())
}

/// Append a batch of contract Fix rows, deduped by guid.
///
/// Deduplication is bounded to the target day partition for each fix.
/// Re-sends carry the same `tst`, so they land in the same local-day file;
/// reading only that one file keeps per-fix work O(fixes_in_one_day) rather
/// than O(total_fixes_ever), satisfying the codebase principle that
/// vault-touching reads scale to what is displayed / queried.
///
/// Fixes that have no `guid` (empty string) are always written through.
fn write_fixes(vault: &Vault, fixes: &[Fix]) -> Result<()> {
    let stream = vault.stream(DIR, Partition::Day);

    // Group fixes by their day partition key so we do at most one disk read
    // per unique day across the whole batch.  The store's Partition::Day.key()
    // logic is simply ts[..10], so we replicate that here.
    let mut by_day: std::collections::HashMap<&str, Vec<&Fix>> =
        std::collections::HashMap::new();
    for fix in fixes {
        // Partition::Day keys are the first 10 chars of the RFC3339 ts: "YYYY-MM-DD".
        let day_key = fix.ts.get(..10).unwrap_or("");
        by_day.entry(day_key).or_default().push(fix);
    }

    for (day_key, day_fixes) in by_day {
        if day_key.is_empty() {
            // Malformed ts: fall through to append which will return a clear error.
            stream.append(&day_fixes, |f| &f.ts)?;
            continue;
        }
        // Read only the single partition file for this day to build the seen set.
        let mut seen: HashSet<String> = HashSet::new();
        for f in stream.read::<Fix>(day_key)? {
            if !f.guid.is_empty() {
                seen.insert(f.guid);
            }
        }
        let fresh: Vec<&Fix> = day_fixes
            .into_iter()
            .filter(|f| f.guid.is_empty() || seen.insert(f.guid.clone()))
            .collect();
        if !fresh.is_empty() {
            stream.append(&fresh, |f| &f.ts)?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Public ingest entry point (called from troved's HTTP handler)

/// Process one raw HTTP POST body from the OwnTracks app (HTTP mode).
///
/// Accepts a JSON object body (OwnTracks never POSTs arrays). The payload
/// is written raw unconditionally; recognized types also produce contract rows.
///
/// Errors from the vault write are returned; the HTTP handler should respond
/// with 500 on error, 200 on success (OwnTracks discards and retries on
/// non-2xx responses in some modes).
pub fn ingest(vault: &Vault, body: &[u8]) -> Result<()> {
    let payload: Value = serde_json::from_slice(body).context("owntracks: invalid JSON body")?;

    // Determine the raw timestamp for partitioning; use now if absent.
    let ts = payload
        .get("tst")
        .and_then(Value::as_i64)
        .and_then(tst_to_rfc3339)
        .unwrap_or_else(|| DateTime::<Local>::from(std::time::SystemTime::now()).to_rfc3339());

    write_raw(vault, payload.clone(), &ts)?;

    let parsed = parse_payload(&payload);
    match &parsed {
        OtPayload::Location(loc) => {
            if let Some(fix) = location_to_fix(loc) {
                write_fixes(vault, &[fix])?;
            }
        }
        OtPayload::Transition(tr) => {
            if let Some(fix) = transition_to_fix(tr) {
                write_fixes(vault, &[fix])?;
            }
        }
        OtPayload::Other => {} // raw-only
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// LiveCollector implementation
//
// The live channel is fed by the troved HTTP receiver (not yet wired — that
// wiring lives in troved). The tick() drains the channel on each poll and
// writes whatever arrived. The receiver only needs to push `Vec<u8>` bodies
// into the sender; the parser and vault writes live here.

struct OwntrackLive {
    rx: Receiver<Vec<u8>>,
}

fn make_live() -> Box<dyn LiveCollector> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    // Register the sender so troved can push into it. The sender is stored
    // in a global that troved acquires; until troved wires the endpoint the
    // channel simply stays empty.
    *GLOBAL_TX.lock().unwrap() = Some(tx);
    Box::new(OwntrackLive { rx })
}

impl LiveCollector for OwntrackLive {
    fn tick(&mut self, vault: &Vault, _now: DateTime<Local>, enabled: bool) {
        if !enabled {
            // Drain and discard while disabled (don't queue a backlog).
            while self.rx.try_recv().is_ok() {}
            return;
        }
        while let Ok(body) = self.rx.try_recv() {
            if let Err(e) = ingest(vault, &body) {
                eprintln!("owntracks: ingest error: {e:#}");
            }
        }
    }

    fn shutdown(&mut self, vault: &Vault, _now: DateTime<Local>) {
        // Drain any remaining payloads on shutdown.
        while let Ok(body) = self.rx.try_recv() {
            if let Err(e) = ingest(vault, &body) {
                eprintln!("owntracks: ingest error on shutdown: {e:#}");
            }
        }
    }
}

/// The global sender slot — troved's HTTP receiver acquires a clone of this
/// to push payloads into the live collector's channel. `None` when no live
/// collector is running (daemon not started, or OwnTracks disabled).
///
/// This is the only shared state: a `Mutex<Option<Sender>>`. The HTTP receiver
/// sends `Vec<u8>` bodies; multiple concurrent pushes are safe because
/// `mpsc::Sender::send` is `&self` and `Sender` is `Send`.
pub static GLOBAL_TX: std::sync::Mutex<Option<Sender<Vec<u8>>>> =
    std::sync::Mutex::new(None);

/// Push a raw POST body from the HTTP receiver into the live collector's
/// channel. Returns `false` if no collector is running (daemon down / toggled off).
pub fn push_body(body: Vec<u8>) -> bool {
    if let Ok(guard) = GLOBAL_TX.lock() {
        if let Some(tx) = guard.as_ref() {
            return tx.send(body).is_ok();
        }
    }
    false
}

// ---------------------------------------------------------------------------
// last_data

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// ---------------------------------------------------------------------------
// Integration DEF

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "owntracks",
        name: "OwnTracks",
        kind: IntegrationKind::Live,
        default_on: false,
        description:
            "Receive a continuous GPS trail from the open-source OwnTracks iOS or Android \
             app in HTTP mode. Location payloads include coordinates, accuracy, speed, \
             altitude, battery level, and optional region enter/leave events.",
        domain: "location",
        vault_path: "location/owntracks/",
        toggleable: false,
        setup: &[
            "Enable HTTP mode in OwnTracks (Settings → Connection → Mode → HTTP).",
            "Set the URL to the local troved endpoint: http://127.0.0.1:<port>/owntracks.",
            "Optional: set a shared secret in OwnTracks and the matching token in Trove.",
        ],
        caveats: "Requires OwnTracks configured in HTTP mode pointing at the local \
                  troved endpoint; MQTT broker mode is not supported. Continuous location \
                  data is privacy-sensitive — this integration is off by default.",
    },
    behavior: Behavior::Live(make_live),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-owntracks-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures: exact OwnTracks JSON booklet field names / values.
    // Source: https://owntracks.org/booklet/tech/json/

    /// Minimal `_type:location` payload (required fields only).
    fn loc_minimal() -> Value {
        json!({
            "_type": "location",
            "lat": 51.5074,
            "lon": -0.1278,
            "tst": 1718031243
        })
    }

    /// Full `_type:location` payload with all common optional fields.
    fn loc_full() -> Value {
        json!({
            "_type": "location",
            "lat": 51.5074,
            "lon": -0.1278,
            "tst": 1718031243,
            "acc": 10,
            "alt": 25,
            "vel": 36,
            "cog": 180,
            "vac": 5,
            "batt": 82,
            "bs": 1,
            "t": "p",
            "tid": "da",
            "topic": "owntracks/dave/iphone",
            "inregions": ["home", "work"],
            "SSID": "HomeNetwork",
            "BSSID": "aa:bb:cc:dd:ee:ff",
            "conn": "w",
            "m": 1,
            "p": 101.3,
            "created_at": 1718031240
        })
    }

    /// `_type:transition` enter event with lat/lon present.
    fn transition_enter() -> Value {
        json!({
            "_type": "transition",
            "event": "enter",
            "tst": 1718035000,
            "wtst": 1700000000,
            "acc": 20,
            "lat": 51.5080,
            "lon": -0.1270,
            "desc": "home",
            "tid": "da",
            "t": "c",
            "rid": "abc123"
        })
    }

    /// `_type:transition` leave event without lat/lon (no Fix emitted).
    fn transition_leave_no_location() -> Value {
        json!({
            "_type": "transition",
            "event": "leave",
            "tst": 1718038000,
            "wtst": 1700000000,
            "acc": 30,
            "desc": "home"
        })
    }

    /// An unknown payload type (lwt, card, etc.) — raw only.
    fn lwt_payload() -> Value {
        json!({
            "_type": "lwt",
            "tst": 1718031200
        })
    }

    // -----------------------------------------------------------------------
    // parse_payload

    #[test]
    fn owntracks_parse_location_minimal() {
        let v = loc_minimal();
        let p = parse_payload(&v);
        match p {
            OtPayload::Location(loc) => {
                assert_eq!(loc.lat, 51.5074);
                assert_eq!(loc.lon, -0.1278);
                assert_eq!(loc.tst, 1718031243);
                assert_eq!(loc.batt, None);
                assert_eq!(loc.topic, None);
            }
            _ => panic!("expected Location"),
        }
    }

    #[test]
    fn owntracks_parse_location_full() {
        let v = loc_full();
        let p = parse_payload(&v);
        match p {
            OtPayload::Location(loc) => {
                assert_eq!(loc.acc, Some(10));
                assert_eq!(loc.alt, Some(25));
                assert_eq!(loc.vel, Some(36));
                assert_eq!(loc.cog, Some(180));
                assert_eq!(loc.batt, Some(82));
                assert_eq!(loc.bs, Some(1));
                assert_eq!(loc.t.as_deref(), Some("p"));
                assert_eq!(loc.tid.as_deref(), Some("da"));
                assert_eq!(loc.topic.as_deref(), Some("owntracks/dave/iphone"));
                assert_eq!(
                    loc.inregions.as_deref(),
                    Some(&["home".to_string(), "work".to_string()][..])
                );
                assert_eq!(loc.ssid.as_deref(), Some("HomeNetwork"));
                assert_eq!(loc.bssid.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
                assert_eq!(loc.conn.as_deref(), Some("w"));
                assert_eq!(loc.m, Some(1));
                assert!((loc.p.unwrap() - 101.3).abs() < 0.01);
            }
            _ => panic!("expected Location"),
        }
    }

    #[test]
    fn owntracks_parse_transition() {
        let v = transition_enter();
        let p = parse_payload(&v);
        match p {
            OtPayload::Transition(tr) => {
                assert_eq!(tr.event, "enter");
                assert_eq!(tr.tst, 1718035000);
                assert_eq!(tr.wtst, 1700000000);
                assert_eq!(tr.acc, 20);
                assert_eq!(tr.lat, Some(51.5080));
                assert_eq!(tr.lon, Some(-0.1270));
                assert_eq!(tr.desc.as_deref(), Some("home"));
                assert_eq!(tr.rid.as_deref(), Some("abc123"));
            }
            _ => panic!("expected Transition"),
        }
    }

    #[test]
    fn owntracks_parse_lwt_is_other() {
        let p = parse_payload(&lwt_payload());
        assert!(matches!(p, OtPayload::Other), "lwt should be Other");
    }

    // -----------------------------------------------------------------------
    // location_to_fix

    #[test]
    fn owntracks_location_to_fix_minimal() {
        let loc: OtLocation = serde_json::from_value(loc_minimal()).unwrap();
        let fix = location_to_fix(&loc).expect("should produce a Fix");

        assert_eq!(fix.source, "owntracks");
        assert_eq!(fix.lat, 51.5074);
        assert_eq!(fix.lon, -0.1278);
        assert!(!fix.ts.is_empty(), "ts derived from tst");
        assert!(fix.guid.starts_with("ot-"), "guid has ot- prefix");

        assert!(fix.ele.is_none());
        assert!(fix.speed.is_none());
        assert!(fix.accuracy.is_none());
        assert!(fix.heading.is_none());
        assert!(fix.extra.is_empty());
    }

    #[test]
    fn owntracks_location_to_fix_full() {
        let loc: OtLocation = serde_json::from_value(loc_full()).unwrap();
        let fix = location_to_fix(&loc).expect("should produce a Fix");

        // vel 36 km/h → 10.0 m/s.
        assert!((fix.speed.unwrap() - 10.0).abs() < 0.01, "km/h → m/s: {:?}", fix.speed);
        assert_eq!(fix.accuracy, Some(10.0));
        assert_eq!(fix.ele, Some(25.0));
        assert_eq!(fix.heading, Some(180.0));

        // Overflow in extra.
        assert_eq!(fix.extra["batt"], json!(82));
        assert_eq!(fix.extra["topic"], json!("owntracks/dave/iphone"));
        assert_eq!(fix.extra["SSID"], json!("HomeNetwork"));
        assert_eq!(fix.extra["BSSID"], json!("aa:bb:cc:dd:ee:ff"));
        assert_eq!(fix.extra["conn"], json!("w"));

        // mode and trail stay empty (OwnTracks doesn't classify movement).
        assert!(fix.mode.is_empty(), "no mode from OwnTracks");
        assert!(fix.trail.is_empty());
    }

    #[test]
    fn owntracks_location_to_fix_zero_vel_and_acc_omitted() {
        // vel=0 means stationary/unknown; acc=0 means unknown — must not
        // become speed=0.0 / accuracy=0.0 on the Fix.
        let mut v = loc_minimal();
        v["vel"] = json!(0);
        v["acc"] = json!(0);
        let loc: OtLocation = serde_json::from_value(v).unwrap();
        let fix = location_to_fix(&loc).unwrap();
        assert!(fix.speed.is_none(), "zero vel omitted");
        assert!(fix.accuracy.is_none(), "zero acc omitted");
    }

    #[test]
    fn owntracks_transition_to_fix_with_location() {
        let tr: OtTransition = serde_json::from_value(transition_enter()).unwrap();
        let fix = transition_to_fix(&tr).expect("enter with lat/lon yields a Fix");

        assert_eq!(fix.lat, 51.5080);
        assert_eq!(fix.lon, -0.1270);
        assert_eq!(fix.accuracy, Some(20.0));
        assert_eq!(fix.extra["ot_event"], json!("enter"));
        assert_eq!(fix.extra["ot_region"], json!("home"));
        assert_eq!(fix.extra["ot_rid"], json!("abc123"));
        assert_eq!(fix.extra["wtst"], json!(1700000000_i64));
        assert!(fix.guid.starts_with("ot-tr-"), "transition guid prefix");
    }

    #[test]
    fn owntracks_transition_without_location_yields_no_fix() {
        let tr: OtTransition =
            serde_json::from_value(transition_leave_no_location()).unwrap();
        let fix = transition_to_fix(&tr);
        assert!(fix.is_none(), "no lat/lon → no Fix");
    }

    // -----------------------------------------------------------------------
    // ingest (full pipeline)

    #[test]
    fn owntracks_ingest_location_writes_raw_and_contract() {
        let v = temp_vault("ingest_loc");
        let body = serde_json::to_vec(&loc_minimal()).unwrap();
        ingest(&v, &body).unwrap();

        // Raw layer: month file exists.
        let raw_dir = v.root().join(RAW_DIR);
        assert!(raw_dir.exists(), "raw dir created");
        let raw_files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(raw_files.len(), 1, "one raw month file");
        let raw_body = fs::read_to_string(raw_files[0].path()).unwrap();
        assert!(raw_body.contains("\"lat\":51.5074"), "raw has lat");

        // Contract layer: day file has a Fix.
        let day_files: Vec<_> = fs::read_dir(v.root().join(DIR))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .collect();
        assert_eq!(day_files.len(), 1, "one contract day file");
        let day_body = fs::read_to_string(day_files[0].path()).unwrap();
        assert!(day_body.contains("\"source\":\"owntracks\""), "Fix in day file");
        assert!(day_body.contains("\"lat\":51.5074"), "lat on Fix");
    }

    #[test]
    fn owntracks_ingest_duplicate_is_deduped() {
        let v = temp_vault("ingest_dedup");
        let body = serde_json::to_vec(&loc_minimal()).unwrap();

        ingest(&v, &body).unwrap();
        ingest(&v, &body).unwrap(); // same payload sent again

        // Contract: still only one Fix row.
        let day_files: Vec<_> = fs::read_dir(v.root().join(DIR))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .collect();
        let day_body = fs::read_to_string(day_files[0].path()).unwrap();
        assert_eq!(
            day_body.lines().filter(|l| !l.trim().is_empty()).count(),
            1,
            "duplicate Fix deduped"
        );

        // Raw: two raw lines (one per POST — raw is always written verbatim).
        let raw_dir = v.root().join(RAW_DIR);
        let raw_files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        let raw_body = fs::read_to_string(raw_files[0].path()).unwrap();
        assert_eq!(
            raw_body.lines().filter(|l| !l.trim().is_empty()).count(),
            2,
            "raw keeps every POST verbatim"
        );
    }

    #[test]
    fn owntracks_ingest_transition_enter_writes_both_layers() {
        let v = temp_vault("ingest_tr_enter");
        let body = serde_json::to_vec(&transition_enter()).unwrap();
        ingest(&v, &body).unwrap();

        // Raw layer exists.
        assert!(v.root().join(RAW_DIR).exists());

        // Contract layer: Fix with ot_event=enter.
        let day_files: Vec<_> = fs::read_dir(v.root().join(DIR))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .collect();
        assert!(!day_files.is_empty(), "day file written for transition with lat/lon");
        let day_body = fs::read_to_string(day_files[0].path()).unwrap();
        assert!(day_body.contains("\"ot_event\":\"enter\""), "transition event in extra");
    }

    #[test]
    fn owntracks_ingest_transition_no_location_raw_only() {
        let v = temp_vault("ingest_tr_nolatlon");
        let body = serde_json::to_vec(&transition_leave_no_location()).unwrap();
        ingest(&v, &body).unwrap();

        // Raw written.
        assert!(v.root().join(RAW_DIR).exists());

        // No contract day files (no lat/lon → no Fix).
        let dir_path = v.root().join(DIR);
        let day_count = fs::read_dir(&dir_path)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(day_count, 0, "no day file when transition has no lat/lon");
    }

    #[test]
    fn owntracks_ingest_lwt_raw_only() {
        let v = temp_vault("ingest_lwt");
        let body = serde_json::to_vec(&lwt_payload()).unwrap();
        ingest(&v, &body).unwrap();

        // Raw exists.
        assert!(v.root().join(RAW_DIR).exists());

        // No contract day files.
        let dir_path = v.root().join(DIR);
        let day_count = fs::read_dir(&dir_path)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(day_count, 0, "lwt → no contract rows");
    }

    #[test]
    fn owntracks_ingest_invalid_json_errors() {
        let v = temp_vault("ingest_bad_json");
        let err = ingest(&v, b"{not json").unwrap_err().to_string();
        assert!(err.contains("invalid JSON") || err.contains("JSON"), "clear error: {err}");
    }

    #[test]
    fn owntracks_def_is_live_and_location_domain() {
        assert_eq!(DEF.meta.id, "owntracks");
        assert_eq!(DEF.meta.domain, "location");
        assert!(!DEF.meta.default_on, "privacy-sensitive: off by default");
        assert_eq!(DEF.connection, None, "no login needed");
        assert!(matches!(DEF.behavior, Behavior::Live(_)), "behavior is Live");
    }

    #[test]
    fn owntracks_guid_is_stable_and_deterministic() {
        let g1 = location_guid(1718031243, 51.5074, -0.1278);
        let g2 = location_guid(1718031243, 51.5074, -0.1278);
        assert_eq!(g1, g2, "guid is deterministic");
        assert!(g1.starts_with("ot-"), "guid prefix");

        // Different ts or position → different guid.
        let g3 = location_guid(1718031244, 51.5074, -0.1278);
        let g4 = location_guid(1718031243, 51.5075, -0.1278);
        assert_ne!(g1, g3, "different tst → different guid");
        assert_ne!(g1, g4, "different lat → different guid");
    }

    #[test]
    fn owntracks_vel_conversion_km_h_to_m_s() {
        // 36 km/h = 10.0 m/s; 72 km/h = 20.0 m/s; 0 km/h = None.
        let cases: &[(i64, Option<f64>)] = &[(36, Some(10.0)), (72, Some(20.0)), (0, None)];
        for &(vel, expected) in cases {
            let mut v = loc_minimal();
            v["vel"] = json!(vel);
            let loc: OtLocation = serde_json::from_value(v).unwrap();
            let fix = location_to_fix(&loc).unwrap();
            match expected {
                Some(exp) => {
                    assert!((fix.speed.unwrap() - exp).abs() < 0.01, "vel={vel}");
                }
                None => assert!(fix.speed.is_none(), "vel=0 should be omitted"),
            }
        }
    }

    #[test]
    fn owntracks_live_channel_push_and_drain() {
        // Smoke-test the global push_body → tick drain path with a real temp vault.
        let v = temp_vault("live_channel");
        let body = serde_json::to_vec(&loc_minimal()).unwrap();

        // make_live registers the sender in GLOBAL_TX.
        let mut collector = make_live();
        let now = chrono::Local::now();

        // Push a payload via the public API.
        assert!(push_body(body), "push_body returns true when sender is live");

        // tick() should drain it and write to vault.
        collector.tick(&v, now, true);

        let day_files: Vec<_> = fs::read_dir(v.root().join(DIR))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl"))
            .collect();
        assert!(!day_files.is_empty(), "tick drained the channel and wrote a Fix");
    }
}
