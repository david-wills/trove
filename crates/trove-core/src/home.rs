//! The `home` domain contract: everything an **owned** smart device or sensor
//! records about the home — air/climate readings, discrete device events, and
//! metered energy/water — in one normalized, source-agnostic store.
//!
//! Three record shapes share the domain (see
//! [`docs/vault-spec/domains/home.md`]):
//!
//! - **[`HomeReading`]** — one scalar observation (temperature, humidity, CO2,
//!   PM2.5, a thermostat's ambient temp, a PWS's outdoor temp), under
//!   `home/<source>/YYYY-MM.jsonl` (month of [`HomeReading::ts`]). Indoor-air
//!   monitors (Airthings, Awair, Aranet, Netatmo, SwitchBot), personal weather
//!   stations (Tempest, Ambient Weather), and thermostats write this shape.
//! - **home event** — a discrete thing that happened (a lock unlocked, motion
//!   fired, a vacuum ran), under `home/<source>/events/YYYY-MM.jsonl`.
//! - **home energy** — a metered interval (grid/solar electricity, gas, water),
//!   under `home/<source>/energy/YYYY-MM.jsonl`.
//!
//! Only the **reading** shape is bound here as a Rust type so far — the first
//! `home` collector ([`crate::ambient_weather`], a personal weather station)
//! writes readings. The event and energy shapes stay Phase-3 drafts (schema +
//! example only) until the first collector that writes them binds them, exactly
//! as the `environment` domain bound only its reading/geo-event/almanac shapes
//! as their collectors arrived. One `DOMAINS` entry covers the whole domain
//! (the `environment`/`reading` precedent).
//!
//! The **four-field reading core (`ts`, `source`, `metric`, `value`) is shared
//! verbatim with the [`crate::environment`] domain**, so an owned indoor sensor
//! and a public outdoor feed of the same `metric` stack in one read-time view.
//! The split from `environment/` is ownership, not shape: a sensor the user owns
//! writes `home/` and carries an optional `device`; a public feed writes
//! `environment/` and more often carries a `station`. All cross-source
//! reconciliation (which sensor wins, dedupe across a device seen directly and
//! again through Home Assistant) is read-time work — collectors only report
//! their own folder and their own stable keys, never a write-time merge. Write
//! the value the source gave you with its real `unit` — don't normalize C↔F at
//! write time. Anything a source carries that the normalized columns don't map
//! rides verbatim under [`extra`](HomeReading::extra), full fidelity.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One scalar reading from an owned sensor/device — one line of
/// `home/<source>/YYYY-MM.jsonl`.
///
/// An event on the timeline: one `metric`, one numeric `value`, at a place and
/// time. Only `ts`/`source`/`metric`/`value` are required (the shared scalar
/// core with [`crate::environment::EnvReading`]); everything else is optional
/// and omitted when empty. `home/` readings add the optional `device` (the
/// owned sensor's id/name); `lat`/`lon` ride along when the device reports them
/// (an outdoor PWS). Source-specific fields the normalized columns don't carry
/// ride verbatim under [`extra`](HomeReading::extra). Matches
/// `home.reading.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct HomeReading {
    /// RFC3339 local time the reading was taken. Always serialized; its month
    /// is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`airthings`,
    /// `ambient-weather`, …). Always serialized.
    pub source: String,
    /// What was measured, snake_case (`temperature`, `humidity`, `co2`,
    /// `pm25`, `pressure`, `uv`, …) — kept stable and consistent with the
    /// `environment/` reading contract so the two merge by `metric` at read
    /// time. Always serialized.
    pub metric: String,
    /// The numeric reading; the unit rides in [`unit`](HomeReading::unit).
    /// Always serialized.
    pub value: f64,
    /// Unit of `value` (`C`, `F`, `percent`, `ppm`, `ug_m3`, `bq_m3`, `db`,
    /// `hpa`, `inHg`, `mph`, `aqi`, `index`, …) — omit only when truly unitless.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unit: String,
    /// Human label of where: room name (`"Bedroom"`) or station name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub place: String,
    /// The owned device/sensor id or name (home-specific).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub device: String,
    /// Latitude in degrees, when the device reports it (outdoor PWS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    /// Longitude in degrees, when the device reports it (outdoor PWS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// Everything source-specific the normalized columns don't carry (battery,
    /// score, raw index, …) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl HomeReading {
    /// A minimal record with only the four required fields set.
    pub fn new(
        source: impl Into<String>,
        metric: impl Into<String>,
        value: f64,
        ts: impl Into<String>,
    ) -> Self {
        HomeReading {
            ts: ts.into(),
            source: source.into(),
            metric: metric.into(),
            value,
            unit: String::new(),
            place: String::new(),
            device: String::new(),
            lat: None,
            lon: None,
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_reading_serializes_only_the_four_required_fields() {
        // A bare sensor sample writes just ts/source/metric/value; every
        // optional (unit/place/device/lat/lon/extra) is omitted when empty.
        let r = HomeReading::new("awair", "co2", 812.0, "2026-06-10T14:05:00-07:00");
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "ts": "2026-06-10T14:05:00-07:00",
                "source": "awair",
                "metric": "co2",
                "value": 812.0
            })
        );
    }

    #[test]
    fn full_reading_round_trips_with_float_coords() {
        // The spec's worked outdoor-PWS example: lat/lon/value survive as f64
        // without precision drift, and `device` is the home-specific column.
        let line = json!({
            "ts": "2026-06-10T14:04:48-07:00",
            "source": "weatherflow-tempest",
            "metric": "temperature",
            "value": 21.4,
            "unit": "C",
            "place": "Backyard",
            "device": "ST-00012345",
            "lat": 37.7793,
            "lon": -122.4193
        });
        let r: HomeReading = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(r.value, 21.4);
        assert_eq!(r.device, "ST-00012345");
        assert_eq!(r.lat, Some(37.7793));
        assert_eq!(r.lon, Some(-122.4193));
        assert_eq!(serde_json::to_value(&r).unwrap(), line);
    }

    #[test]
    fn reading_with_extra_round_trips_and_unknown_field_tolerated() {
        // Source-specific overflow rides in extra; an unknown top-level field is
        // ignored on parse and dropped on re-serialize (additive evolution).
        let line = json!({
            "ts": "2026-06-10T14:05:00-07:00",
            "source": "airthings",
            "metric": "radon",
            "value": 48,
            "unit": "bq_m3",
            "place": "Bedroom",
            "device": "2960123456",
            "extra": {"battery": 86},
            "future_field": "ignored"
        });
        let r: HomeReading = serde_json::from_value(line).unwrap();
        // An integer JSON `48` parses into the f64 `value`.
        assert_eq!(r.value, 48.0);
        assert_eq!(r.extra.get("battery"), Some(&json!(86)));
        let re = serde_json::to_value(&r).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert_eq!(re["value"], json!(48.0));
    }
}
