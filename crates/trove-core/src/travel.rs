//! The `travel` domain contract: trips broken into their segments — flights,
//! lodging stays, car rentals, trains, ferries — in one normalized,
//! source-agnostic store, whatever assembled them.
//!
//! One record shape ([`Segment`]) carries every kind; a [`Segment::type_`]
//! discriminator (`flight | lodging | car | train | ferry | bus | cruise |
//! transfer | activity | other`) says which, and the same handful of shared
//! fields describe them all. Only `ts·source·type·guid` are required; a sparse
//! source (a CSV flight log with a date and two airports) writes a minimal line,
//! while a rich source (TripIt, with terminals and seats) fills more.
//!
//! The stream is **append-only**, partitioned by the local month of [`Segment::ts`]
//! (the segment's start — departure / check-in / pickup), under
//! `travel/<source>/YYYY-MM.jsonl`. Each source writes its own folder with its
//! own stable [`Segment::guid`] (the dedupe key): an ICS UID, an Airbnb
//! confirmation code, a Flighty row id, a `flight-emails` booking-ref+segment, or
//! a stable date+route+number hash where the source carries no id. Two sources
//! that captured the same flight both write it; reconciliation is a *read-time*
//! opinion, never a write-time merge.
//!
//! Type-specific detail with no shared column — seat, terminal, room, address,
//! car class, fare, delay minutes, aircraft, amount, currency — rides under
//! [`Segment::extra`]; full fidelity always also survives in the source's own
//! `travel/<source>/raw/` folder. Flighty and myFlightRadar24 (flight logs),
//! TripIt (the richest: every segment type), Airbnb (lodging stays), and the
//! derived `flight-emails` extractor all write this shape; a reader stitches
//! segments back into trips and onto the timeline at read time.
//!
//! See [`docs/vault-spec/domains/travel.md`] for the field-level spec; the schema
//! field descriptions there are authoritative for names/units/meanings. Matches
//! `travel.segment.schema.json` field-for-field.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One trip segment — one line of `travel/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only
/// `ts`/`source`/`type`/`guid` are required; everything else is omit-empty, so a
/// bare flight-log line writes four fields while a rich itinerary fills more.
/// Matches `travel.segment.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Segment {
    /// RFC3339 local time of the segment's start — departure, check-in, or
    /// pickup. Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`tripit`, `airbnb`,
    /// `flighty`). Always serialized.
    pub source: String,
    /// Segment kind discriminator: `"flight"` | `"lodging"` | `"car"` |
    /// `"train"` | `"ferry"` | `"bus"` | `"cruise"` | `"transfer"` |
    /// `"activity"` | `"other"`. Serialized as `type` (the schema field name);
    /// `type_` here only because `type` is a Rust keyword. Always serialized.
    #[serde(rename = "type")]
    pub type_: String,
    /// Source-unique id, the dedupe key (ICS UID, Airbnb confirmation code,
    /// Flighty row id, booking-ref+segment, or a date+route+number hash). Always
    /// serialized.
    pub guid: String,
    /// RFC3339 local arrival / check-out / drop-off, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_ts: String,
    /// Origin: airport/station code (IATA where it has one) or city — the
    /// departure/check-in place. For lodging this is the stay's city.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub start_place: String,
    /// Display name for `start_place` (airport name, hotel/listing name, city
    /// label). For lodging this is the property/listing name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub start_place_name: String,
    /// Destination: airport/station code or city (flights, trains, transfers,
    /// one-way car rentals).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_place: String,
    /// Display name for `end_place`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_place_name: String,
    /// Operating brand: airline, hotel/lodging brand, rental company, rail
    /// operator.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub vendor: String,
    /// Flight or train number — the one shared typed field; verbatim, e.g.
    /// `"UA 523"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub number: String,
    /// Confirmation / record-locator / booking code shown to the traveller.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub confirmation: String,
    /// Groups segments of one itinerary/trip (a multi-leg booking, a
    /// `flight-emails` booking ref).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub booking_id: String,
    /// Source-native state — an open string (`"confirmed"`, `"canceled"`,
    /// `"completed"`, `"delayed"`, …; sources differ).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    /// Everything source-specific the normalized columns don't carry (seat,
    /// terminal, gate, room, address, car class, amount, currency, delay
    /// minutes, aircraft, …) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Segment {
    /// A minimal record with only the four required fields set.
    pub fn new(
        source: impl Into<String>,
        type_: impl Into<String>,
        guid: impl Into<String>,
        ts: impl Into<String>,
    ) -> Self {
        Segment {
            ts: ts.into(),
            source: source.into(),
            type_: type_.into(),
            guid: guid.into(),
            end_ts: String::new(),
            start_place: String::new(),
            start_place_name: String::new(),
            end_place: String::new(),
            end_place_name: String::new(),
            vendor: String::new(),
            number: String::new(),
            confirmation: String::new(),
            booking_id: String::new(),
            status: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_segment_serializes_only_required_fields() {
        let s = Segment::new("myflightradar24", "flight", "mfr24-3f9a1c7e", "2025-11-02T09:40:00+01:00");
        // Omit-empty: a sparse line is exactly the four required keys, and the
        // discriminator serializes as `type` (not `type_`).
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({
                "ts": "2025-11-02T09:40:00+01:00",
                "source": "myflightradar24",
                "type": "flight",
                "guid": "mfr24-3f9a1c7e"
            })
        );
    }

    #[test]
    fn full_flight_round_trips() {
        let line = json!({
            "ts": "2026-07-14T08:25:00-07:00",
            "source": "tripit",
            "type": "flight",
            "guid": "item-1847562301-1",
            "end_ts": "2026-07-14T16:58:00-04:00",
            "start_place": "SFO",
            "start_place_name": "San Francisco Intl",
            "end_place": "JFK",
            "end_place_name": "John F. Kennedy Intl",
            "vendor": "United Airlines",
            "number": "UA 523",
            "confirmation": "H4X9Q2",
            "booking_id": "trip-298104",
            "status": "confirmed",
            "extra": {"seat": "14C", "cabin": "Economy", "departure_terminal": "3", "aircraft": "Boeing 737-900"}
        });
        let seg: Segment = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(seg.type_, "flight");
        assert_eq!(seg.number, "UA 523");
        assert_eq!(seg.extra.get("seat"), Some(&Value::String("14C".into())));
        assert_eq!(serde_json::to_value(&seg).unwrap(), line);
    }

    #[test]
    fn lodging_segment_round_trips_with_amount_in_extra() {
        // The Airbnb lodging shape: city in start_place, listing name in
        // start_place_name, confirmation code as guid, amount/currency in extra.
        let line = json!({
            "ts": "2026-08-03T15:00:00+02:00",
            "source": "airbnb",
            "type": "lodging",
            "guid": "HMABCDEFGH",
            "end_ts": "2026-08-09T11:00:00+02:00",
            "start_place": "Lisbon, PT",
            "start_place_name": "Sunny Alfama Loft with River View",
            "confirmation": "HMABCDEFGH",
            "extra": {"amount": "742.00", "currency": "EUR", "nights": "6", "country": "Portugal"}
        });
        let seg: Segment = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(seg.type_, "lodging");
        assert_eq!(seg.start_place, "Lisbon, PT");
        assert_eq!(seg.confirmation, "HMABCDEFGH");
        assert_eq!(seg.extra.get("amount"), Some(&Value::String("742.00".into())));
        assert!(seg.vendor.is_empty(), "lodging carries no operating brand here");
        assert_eq!(serde_json::to_value(&seg).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated() {
        // Forward-compat: an unknown top-level field is ignored on the way in
        // and dropped on re-serialize (additive evolution).
        let line = json!({
            "ts": "2026-08-03T15:00:00+02:00",
            "source": "airbnb",
            "type": "lodging",
            "guid": "HMABCDEFGH",
            "future_field": "ignored"
        });
        let seg: Segment = serde_json::from_value(line).unwrap();
        assert_eq!(seg.guid, "HMABCDEFGH");
        let re = serde_json::to_value(&seg).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
    }
}
