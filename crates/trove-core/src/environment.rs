//! The `environment` domain contract: ambient public feeds — the world
//! around the user — in one normalized, source-agnostic store.
//!
//! Air quality, pollen, river/tide levels, buoy waves, station climate, space
//! weather, earthquakes, wildfire detections, weather alerts, and sun/moon
//! almanacs all land here in three record shapes. Each public feed writes its
//! own `environment/<source>/` folder (the contract's "source = folder name"
//! rule); readers scan them together. **Owned home sensors write `home/` with
//! the identical reading core and merge at read time** — public feed vs. owned
//! device is the only thing that decides which folder.
//!
//! All three shapes are now bound here as Rust types:
//!
//! - [`EnvReading`] — `environment/<source>/YYYY-MM.jsonl`: a scalar
//!   measurement (one `metric`, one numeric `value`) at a place and time. This
//!   is the **shared scalar-reading core**; `home/`-owned-sensor readings use
//!   the identical four required fields (`ts`/`source`/`metric`/`value`), so a
//!   public AQI feed and an owned indoor sensor land the same shape and merge
//!   at read time. `guid` is *optional* — readings have a natural key
//!   (station/site + metric + ts), so a sparse feed (a bare dB sample) writes
//!   just the four required fields.
//! - [`EnvGeoEvent`] — `environment/<source>/events/YYYY-MM.jsonl`: a located,
//!   dated hazard or phenomenon (quake, fire, alert, …). Distinct from a
//!   reading because it is a discrete event with a magnitude/severity rather
//!   than a continuous metric. `guid` is *required* here — it is the source's
//!   stable dedupe key (USGS event id, alert id, detection hash) and the only
//!   thing that lets a re-pull skip what it already stored. `event_type` is an
//!   open vocabulary so `tsunami`/`volcano`/… arrive additively.
//! - [`Almanac`] — `environment/<source>/almanac/YYYY-MM.jsonl`: one day of
//!   solar/lunar geometry for a location (sun/twilight times, day length, moon)
//!   keyed by `date` (+ rounded `lat`/`lon`). USNO and sunrise-sunset write this
//!   shape. Unlike the other two, an almanac has **no `ts`** — it is reference
//!   geometry keyed by the calendar `date`, so it is month-partitioned by the
//!   month of `date` (not a `ts`), and only `date`/`source` are required (a
//!   sun-only feed omits every moon field). The dedupe key is `date` + rounded
//!   `lat`/`lon` rather than a `guid`.
//!
//! Reading and geo-event are append-only event streams, month-partitioned by
//! `ts`; the almanac is an append-only day stream, month-partitioned by `date`.
//! Collectors only *report* their own folder and their own stable keys; all
//! cross-source reconciliation (which AQI station wins, dedup across
//! overlapping feeds) is read-time work, never baked in at write time. Write
//! the value the source gave you with its real `unit` — don't normalize C↔F or
//! AQI scales at write time, that's a read-time opinion. Anything a source
//! carries that the normalized columns don't map rides verbatim in `extra`,
//! full fidelity at write time.
//!
//! Types are prefixed `Env-` because `home/` also has a "reading" — see
//! [`crate::home_assistant`] and the `home` domain — so a bare `Reading` would
//! collide. See [`docs/vault-spec/domains/environment.md`] for the field-level
//! spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One scalar reading — one line of `environment/<source>/YYYY-MM.jsonl`.
///
/// An event on the timeline: one `metric`, one numeric `value`, at a place and
/// time. Only `ts`/`source`/`metric`/`value` are required (the shared scalar
/// core with `home/`); everything else is optional and omitted when empty.
/// `guid` is optional — a reading has a natural key, so a bare sample writes
/// just the four required fields. Source-specific fields the normalized columns
/// don't carry ride verbatim under [`extra`](EnvReading::extra).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct EnvReading {
    /// RFC3339 local time of the observation. Always serialized.
    pub ts: String,
    /// Collector id, identical to the source folder name (`airnow`,
    /// `usgs-water`, …). Always serialized.
    pub source: String,
    /// What was measured, snake_case (`temperature`, `pm25`, `discharge`,
    /// `noise_db`, …) — kept consistent with the `home/` reading contract so
    /// the two merge by `metric` at read time. Always serialized.
    pub metric: String,
    /// The measured value; the unit rides in [`unit`](EnvReading::unit).
    /// Always serialized.
    pub value: f64,
    /// Unit of `value` (`C`, `F`, `percent`, `ppm`, `ug_m3`, `aqi`, `db`,
    /// `ft`, `cfs`, `m`, `hpa`, `index`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Human label of where (station / city / reporting-area name).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    /// Latitude of the observation, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    /// Longitude of the observation, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// Source-native station / site / sensor id.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub station: String,
    /// Source-unique id, the dedupe key — *optional* for readings, which have
    /// a natural key (typically station/site + metric + ts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guid: Option<String>,
    /// Everything source-specific the normalized columns don't carry (AQI
    /// category, raw concentration, flood-stage, per-pollutant subindices, …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One located, dated geo-event — one line of
/// `environment/<source>/events/YYYY-MM.jsonl`.
///
/// A discrete hazard or phenomenon (earthquake, wildfire detection, weather or
/// geomagnetic alert) with a magnitude/severity rather than a continuous
/// metric. `ts`/`source`/`guid`/`event_type` are required — unlike a reading,
/// `guid` is **mandatory** because it is the only stable dedupe key a re-pull
/// can skip on. `event_type` is an open vocabulary (`quake`|`fire`|`alert`|…)
/// so new types arrive additively without a new folder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct EnvGeoEvent {
    /// RFC3339 local time the event occurred (quake origin time, detection
    /// acquisition, alert onset). Always serialized.
    pub ts: String,
    /// Collector id, identical to the source folder name (`usgs-earthquakes`,
    /// `nws`, `nasa-firms`, …). Always serialized.
    pub source: String,
    /// Source-unique id (USGS event id, alert id, detection hash) — the dedupe
    /// key, **required** for geo-events. Always serialized.
    pub guid: String,
    /// Open vocabulary: `quake` | `fire` | `alert` | … (new types arrive
    /// additively without a new folder). Always serialized.
    pub event_type: String,
    /// Quake magnitude (or any scalar intensity the event carries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub magnitude: Option<f64>,
    /// Human place description (`"12km NE of Ojai, CA"`, the alert area).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    /// Latitude, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    /// Longitude, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// Source-native severity (`"Severe"`, `"Extreme"`, alert level).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub severity: String,
    /// One-line human summary (alert headline).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub headline: String,
    /// Canonical link to the event page.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// RFC3339 local; when an alert stops being active.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expires: String,
    /// Everything source-specific the normalized columns don't carry
    /// (`depth_km`, tsunami flag, felt reports, fire confidence/FRP/satellite,
    /// …).
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

/// One day of solar/lunar geometry for a location — one line of
/// `environment/<source>/almanac/YYYY-MM.jsonl`.
///
/// Reference geometry, not an event: sun/twilight times, day length, and moon
/// for a single calendar `date` at a place. Keyed by `date` (+ rounded
/// `lat`/`lon`) so a re-pull is idempotent — there is **no `ts` and no `guid`**;
/// the partition key is the month of [`date`](Almanac::date), and the dedupe key
/// is `date` + rounded coords. Only `date`/`source` are required — a sun-only
/// feed omits every moon field; every other field is omitted when empty. All the
/// time fields are RFC3339 *local* (carrying the location's UTC offset).
/// Source-specific fields the normalized columns don't carry (illumination
/// fraction, named moon-phase times, …) ride verbatim under [`extra`](Almanac::extra).
/// Matches `environment.almanac.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Almanac {
    /// The local calendar day (`YYYY-MM-DD`). Always serialized; its month is
    /// the partition key, and it is part of the dedupe key.
    pub date: String,
    /// Collector id, identical to the source folder name (`usno`,
    /// `sunrise-sunset`). Always serialized.
    pub source: String,
    /// Latitude the geometry was computed for, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    /// Longitude the geometry was computed for, in degrees.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// RFC3339 local time of sunrise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sunrise: String,
    /// RFC3339 local time of sunset.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sunset: String,
    /// RFC3339 local time of solar noon (the sun's upper transit).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub solar_noon: String,
    /// RFC3339 local time civil twilight begins.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub civil_twilight_begin: String,
    /// RFC3339 local time civil twilight ends.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub civil_twilight_end: String,
    /// RFC3339 local time nautical twilight begins.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nautical_twilight_begin: String,
    /// RFC3339 local time nautical twilight ends.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub nautical_twilight_end: String,
    /// RFC3339 local time astronomical twilight begins.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub astronomical_twilight_begin: String,
    /// RFC3339 local time astronomical twilight ends.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub astronomical_twilight_end: String,
    /// Duration of daylight, source-native (`"14:21"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub day_length: String,
    /// RFC3339 local; start of evening golden hour where the source reports it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub golden_hour: String,
    /// RFC3339 local time of moonrise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub moonrise: String,
    /// RFC3339 local time of moonset.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub moonset: String,
    /// Named moon phase (`"Waning Gibbous"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub moon_phase: String,
    /// Everything source-specific the normalized columns don't carry
    /// (illumination fraction, named moon-phase times, …) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_reading_serializes_only_the_four_required_fields() {
        // A sparse feed (a bare dB sample) writes just ts/source/metric/value;
        // guid is optional for readings, so it must not appear when None.
        let r = EnvReading {
            ts: "2026-06-10T14:05:00-07:00".into(),
            source: "macos-microphone".into(),
            metric: "noise_db".into(),
            value: 48.2,
            unit: String::new(),
            place: String::new(),
            lat: None,
            lon: None,
            station: String::new(),
            guid: None,
            extra: Map::new(),
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "ts": "2026-06-10T14:05:00-07:00",
                "source": "macos-microphone",
                "metric": "noise_db",
                "value": 48.2
            })
        );
    }

    #[test]
    fn full_reading_round_trips_with_float_coords() {
        // lat/lon/value must survive as f64 without precision drift.
        let line = json!({
            "ts": "2026-06-10T13:00:00-07:00",
            "source": "airnow",
            "metric": "pm25",
            "value": 42,
            "unit": "aqi",
            "place": "Los Angeles-North Main Street",
            "lat": 34.0667,
            "lon": -118.2269,
            "station": "060371103",
            "guid": "airnow:060371103:pm25:2026-06-10T13:00",
            "extra": {"category": "Good", "raw_concentration": 10.1, "raw_unit": "ug_m3"}
        });
        let r: EnvReading = serde_json::from_value(line.clone()).unwrap();
        // Float fields survive without precision drift.
        assert_eq!(r.lat, Some(34.0667));
        assert_eq!(r.lon, Some(-118.2269));
        assert_eq!(r.value, 42.0);
        assert_eq!(r.guid.as_deref(), Some("airnow:060371103:pm25:2026-06-10T13:00"));
        assert_eq!(r.extra.get("raw_concentration"), Some(&json!(10.1)));
        // `value` is f64: an integer JSON `42` re-serializes as the number 42.0,
        // which is schema-valid (the schema's `value` is `number`). spec_validation
        // proves schema round-trip; here we prove typed values are stable across
        // a second parse (no drift in the f64 fields).
        let re = serde_json::to_value(&r).unwrap();
        let r2: EnvReading = serde_json::from_value(re).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn minimal_geo_event_serializes_only_required_fields() {
        let e = EnvGeoEvent {
            ts: "2026-06-10T11:00:00-07:00".into(),
            source: "nws".into(),
            guid: "urn:oid:abc".into(),
            event_type: "alert".into(),
            magnitude: None,
            place: String::new(),
            lat: None,
            lon: None,
            severity: String::new(),
            headline: String::new(),
            url: String::new(),
            expires: String::new(),
            extra: Map::new(),
        };
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            json!({
                "ts": "2026-06-10T11:00:00-07:00",
                "source": "nws",
                "guid": "urn:oid:abc",
                "event_type": "alert"
            })
        );
    }

    #[test]
    fn full_geo_event_round_trips() {
        let line = json!({
            "ts": "2026-06-10T03:42:11-07:00",
            "source": "usgs-earthquakes",
            "guid": "ci40123456",
            "event_type": "quake",
            "magnitude": 4.2,
            "place": "12km NE of Ojai, CA",
            "lat": 34.5483,
            "lon": -119.1817,
            "url": "https://earthquake.usgs.gov/earthquakes/eventpage/ci40123456",
            "extra": {"depth_km": 8.3, "alert": "green", "tsunami": false, "felt": 214}
        });
        let e: EnvGeoEvent = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(e.guid, "ci40123456");
        assert_eq!(e.magnitude, Some(4.2));
        assert_eq!(serde_json::to_value(&e).unwrap(), line);
    }

    #[test]
    fn minimal_almanac_serializes_only_the_two_required_fields() {
        // An almanac has no ts/guid; only date/source are required. A sparse
        // (sun-and-moon-less) row is exactly those two keys — every optional is
        // omitted when empty/None.
        let a = Almanac {
            date: "2026-06-10".into(),
            source: "usno".into(),
            lat: None,
            lon: None,
            sunrise: String::new(),
            sunset: String::new(),
            solar_noon: String::new(),
            civil_twilight_begin: String::new(),
            civil_twilight_end: String::new(),
            nautical_twilight_begin: String::new(),
            nautical_twilight_end: String::new(),
            astronomical_twilight_begin: String::new(),
            astronomical_twilight_end: String::new(),
            day_length: String::new(),
            golden_hour: String::new(),
            moonrise: String::new(),
            moonset: String::new(),
            moon_phase: String::new(),
            extra: Map::new(),
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"date": "2026-06-10", "source": "usno"})
        );
    }

    #[test]
    fn full_almanac_round_trips_with_float_coords() {
        // The spec's worked example: a full USNO day with sun/twilight/moon and
        // illumination in extra. lat/lon survive as f64 without precision drift.
        let line = json!({
            "date": "2026-06-10",
            "source": "usno",
            "lat": 34.05,
            "lon": -118.25,
            "sunrise": "2026-06-10T05:42:00-07:00",
            "sunset": "2026-06-10T20:03:00-07:00",
            "solar_noon": "2026-06-10T12:52:00-07:00",
            "civil_twilight_begin": "2026-06-10T05:14:00-07:00",
            "civil_twilight_end": "2026-06-10T20:31:00-07:00",
            "day_length": "14:21",
            "moonrise": "2026-06-10T01:18:00-07:00",
            "moonset": "2026-06-10T13:47:00-07:00",
            "moon_phase": "Waning Gibbous",
            "extra": {"illumination": 0.78}
        });
        let a: Almanac = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(a.date, "2026-06-10");
        assert_eq!(a.lat, Some(34.05));
        assert_eq!(a.lon, Some(-118.25));
        assert_eq!(a.moon_phase, "Waning Gibbous");
        assert_eq!(a.extra.get("illumination"), Some(&json!(0.78)));
        // Round-trips byte-for-byte: omit-empty never drops a populated field,
        // and the absent twilight/golden-hour fields stay absent.
        assert_eq!(serde_json::to_value(&a).unwrap(), line);
    }

    #[test]
    fn unknown_almanac_field_tolerated_and_sun_only_row_ok() {
        // Forward-compat (additive evolution): an unknown top-level field is
        // ignored on parse and dropped on re-serialize. A sun-only feed
        // (sunrise-sunset) omits every moon field and is still valid.
        let line = json!({
            "date": "2026-06-10",
            "source": "sunrise-sunset",
            "lat": 40.71,
            "lon": -74.01,
            "sunrise": "2026-06-10T05:24:00-04:00",
            "sunset": "2026-06-10T20:26:00-04:00",
            "golden_hour": "2026-06-10T19:39:00-04:00",
            "future_field": "ignored"
        });
        let a: Almanac = serde_json::from_value(line).unwrap();
        assert_eq!(a.source, "sunrise-sunset");
        assert!(a.moon_phase.is_empty(), "no moon data on a sun-only feed");
        assert!(a.moonrise.is_empty());
        let re = serde_json::to_value(&a).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("moon_phase").is_none(), "empty moon_phase omitted");
        assert!(re.get("golden_hour").is_some(), "populated golden_hour kept");
    }
}
