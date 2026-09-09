//! The `location` domain contract: standalone GPS trails — every location fix,
//! from any logger, in one normalized, source-agnostic stream.
//!
//! One record shape, [`Fix`], one line of `location/<source>/YYYY-MM-DD.jsonl`
//! (`<source>` is the collector id and the folder name; the file is the **local
//! day** of [`Fix::ts`]). Continuous loggers (OwnTracks, Overland), movement
//! segments (Google Timeline, Arc trips), GPX/FIT imports, and sparse vehicle
//! pings (Smartcar, Tesla) all write one record **per fix** — never a points
//! array — so the high-volume stream stays day-partitionable. A track / logger
//! batch / movement segment / vehicle trip is **the set of fixes sharing one
//! [`Fix::trail`] id**: readers reconstruct a path by grouping the fixes that
//! share `source`+`trail` and sorting by `ts`. A raw logger with no native
//! segmentation may omit `trail` and let the reader segment by time gaps.
//!
//! Only `ts`/`source`/`lat`/`lon` are required; everything else is omit-empty,
//! so a sparse vehicle ping writes four fields (plus its odometer under `extra`)
//! while a rich logger fills elevation, speed, accuracy, heading, mode, and a
//! `trail`. Source-specific fields the normalized columns don't carry (odometer,
//! charge, battery, motion type, wifi SSID, place confidence, …) ride verbatim
//! under `extra` rather than being dropped.
//!
//! This is **privacy-sensitive** data — a continuous trail of where the owner has
//! been — so every source ships opt-in with explicit acknowledgement; the
//! contract doesn't re-encode that (it's a per-def needs-flag). The stream is
//! **append-only**, and collectors skip the guids they already hold (`guid` is
//! the dedupe key). Visits and saved places (Swarm check-ins, Google Maps saved
//! places, Google Timeline / Arc **place visits**) are place/visit-shaped, not
//! fixes — they stay per-source raw under `location/<source>/` until a
//! visits-shaped contract lands, and are never forced into this fix row.
//! Workout-embedded GPS routes are **not** here: a Strava/Garmin/Apple-Health
//! workout is one whole `health/` record whose route the location view joins at
//! read time.
//!
//! See [`docs/vault-spec/domains/location.md`] for the field-level spec; the
//! schema field descriptions there are authoritative for names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One GPS fix — one line of `location/<source>/YYYY-MM-DD.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`lat`/
/// `lon` are required; everything else is omit-empty. Matches
/// `location.fix.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Fix {
    /// RFC3339 local time of the fix. Always serialized; its day is the
    /// partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`owntracks`,
    /// `google-timeline`, `smartcar`). Always serialized.
    pub source: String,
    /// Latitude, in decimal degrees (WGS84), −90..90. Always serialized.
    pub lat: f64,
    /// Longitude, in decimal degrees (WGS84), −180..180. Always serialized.
    pub lon: f64,
    /// Elevation, in meters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ele: Option<f64>,
    /// Ground speed, in meters/second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    /// Horizontal accuracy radius, in meters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<f64>,
    /// Course over ground, in degrees (0–360, clockwise from north).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heading: Option<f64>,
    /// Stable id grouping the fixes of one track/batch/segment/trip — the
    /// read-time grouping key. Omitted by raw loggers that leave segmentation to
    /// the reader.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trail: String,
    /// Display name for the trail (GPX track name, segment label).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trail_name: String,
    /// Movement mode where the source classifies it, verbatim from the source
    /// (`"walking"`, `"cycling"`, `"driving"`, `"transit"`, …) — an open string,
    /// never a closed enum.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    /// Source-unique id, the dedupe key (a fix id, a trip-id+seq, or a
    /// `(ts,lat,lon)` hash where the source gives no id).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    /// Everything source-specific the normalized fields don't carry (odometer,
    /// charge, battery, motion type, wifi SSID, place confidence, …) — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Fix {
    /// A minimal record with only the four required fields set.
    pub fn new(source: impl Into<String>, ts: impl Into<String>, lat: f64, lon: f64) -> Self {
        Fix {
            ts: ts.into(),
            source: source.into(),
            lat,
            lon,
            ele: None,
            speed: None,
            accuracy: None,
            heading: None,
            trail: String::new(),
            trail_name: String::new(),
            mode: String::new(),
            guid: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_fix_serializes_only_required_fields() {
        // A sparse vehicle ping: exactly the four required keys (the odometer
        // would ride in `extra`, added separately).
        let f = Fix::new("smartcar", "2026-06-10T18:02:11-07:00", 37.4419, -122.143);
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            json!({
                "ts": "2026-06-10T18:02:11-07:00",
                "source": "smartcar",
                "lat": 37.4419,
                "lon": -122.143
            })
        );
    }

    #[test]
    fn full_fix_round_trips_with_numeric_lat_lon() {
        // A rich logger fix with every optional present.
        let line = json!({
            "ts": "2026-06-10T08:14:03-07:00",
            "source": "owntracks",
            "lat": 37.77493,
            "lon": -122.41942,
            "ele": 28.4,
            "speed": 1.3,
            "accuracy": 5.0,
            "heading": 92.0,
            "trail": "phone-2026-06-10",
            "guid": "ot-1718031243-37.77493--122.41942",
            "extra": {"batt": 74, "topic": "owntracks/dave/iphone"}
        });
        let f: Fix = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(f.lat, 37.77493, "lat is a number, not a string");
        assert_eq!(f.lon, -122.41942, "lon is a number, not a string");
        assert_eq!(f.ele, Some(28.4));
        assert_eq!(f.trail, "phone-2026-06-10");
        assert_eq!(serde_json::to_value(&f).unwrap(), line);
    }

    #[test]
    fn movement_segment_fix_with_mode_and_trail() {
        // A Google Timeline movement-segment fix: lat/lon/ts, a stable trail id,
        // a verbatim mode string, source-specific overflow in extra.
        let line = json!({
            "ts": "2026-06-10T08:31:50-07:00",
            "source": "google-timeline",
            "lat": 37.80331,
            "lon": -122.44896,
            "trail": "seg-2026-06-10T08:22:00",
            "mode": "cycling",
            "guid": "gt-2026-06-10T08:22:00-cycling",
            "extra": {"distance_m": 4120, "point_confidence": "high"}
        });
        let f: Fix = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(f.mode, "cycling", "mode is verbatim from the source");
        assert_eq!(f.trail, "seg-2026-06-10T08:22:00");
        assert!(f.ele.is_none() && f.speed.is_none(), "absent optionals omitted");
        assert_eq!(serde_json::to_value(&f).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_empty_optionals_omitted() {
        // Forward-compat: an unknown top-level field is ignored on re-serialize;
        // omit-empty drops the unset optionals.
        let line = json!({
            "ts": "2026-06-10T18:02:11-07:00",
            "source": "smartcar",
            "lat": 37.4419,
            "lon": -122.143,
            "extra": {"odometer_km": 48213.6, "make": "TESLA"},
            "future_field": "ignored"
        });
        let f: Fix = serde_json::from_value(line).unwrap();
        assert_eq!(f.source, "smartcar");
        let re = serde_json::to_value(&f).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("trail").is_none() && re.get("mode").is_none(), "empty optionals omitted");
        assert_eq!(re["extra"]["odometer_km"], json!(48213.6), "odometer rides in extra");
    }
}
