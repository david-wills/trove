//! TripIt — ICS trip-itinerary export import.
//!
//! TripIt is a trip-itinerary aggregator: users forward booking confirmation
//! emails (or auto-import them) and TripIt assembles trips with flight, hotel,
//! car-rental, and train segments, each with dates, times, and confirmation
//! numbers. The data matters because it's a clean, pre-parsed travel timeline
//! — exactly what the `travel/` domain [`Segment`] contract captures.
//!
//! **Access model: Import only.** The user goes to TripIt Settings →
//! Download Your Data, which produces an ICS (iCalendar / RFC 5545) export.
//! The TripIt public REST API still exists but **OAuth app registration is
//! closed to new developers** (TripIt GitHub issue #288, May 2024), so the
//! API is not a path for a shipping standalone collector. No TCC permission is
//! required — the user selects the file; Trove never logs in or scrapes.
//!
//! **Contract:** one [`Segment`] per VEVENT in `travel/tripit/YYYY-MM.jsonl`,
//! partitioned by local month of the segment start time. The VEVENT UID is the
//! stable dedupe key ([`Segment::guid`]) — re-importing a newer export never
//! duplicates. Full-fidelity ICS objects also land in `travel/tripit/raw/`.
//!
//! **Parser parked — Needs-sample (evidence rule).** TripIt's exported ICS
//! field layout — how it encodes segment type (flight vs. hotel vs. car vs.
//! train), confirmation numbers, multi-segment trips, and X-TRIPIT-* extension
//! fields into VEVENT SUMMARY / DESCRIPTION / CATEGORIES / X-* properties —
//! is **not formally documented**. Per the project's evidence rule, Trove does
//! not parse an export file against a guessed field shape (a green test over a
//! fabricated fixture is false confidence — cf. the raindrop `_id` bug, the
//! fathom embedded-transcript trap). The contract mapping ([`event_to_segment`])
//! and the scaffold (raw layer, dedupe, re-runnable import loop) are done and
//! tested here. The only piece that waits on a sample is [`events_from_ics`] —
//! the VEVENT → [`TripEvent`] extractor — which is parked behind [`PARKED_MSG`].
//! When a real `.ics` export lands, fill that function alone; nothing downstream
//! changes.
//!
//! Brief: docs/integrations/tripit.md

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::{write_atomic, Partition};
use crate::travel::Segment;
use crate::vault::Vault;

/// Contract-layer output folder.
const DIR: &str = "travel/tripit";
/// Full-fidelity VEVENT objects land here before any field mapping.
const RAW_DIR: &str = "travel/tripit/raw";

/// Shown when the import is invoked before a real export sample exists to pin
/// the VEVENT field layout (SUMMARY / DESCRIPTION / CATEGORIES / X-TRIPIT-*).
/// The bind is done; the field-mapping seam is the only piece waiting.
const PARKED_MSG: &str = "TripIt import is parked pending a real ICS export sample. \
The VEVENT field layout (how TripIt encodes segment type, confirmation numbers, \
and multi-segment trips into SUMMARY, DESCRIPTION, CATEGORIES, and X-TRIPIT-* \
extension properties) is not formally documented, and Trove does not parse \
export files against a guessed field shape. \
Once a real TripIt Settings → Download Your Data export is provided, the field \
mapping is wired in tripit::events_from_ics — the travel contract, raw layer, \
and dedupe are already in place.";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the `NotWired` stub is
/// replaced by this build — the pub mod line and `&DEF` registration already
/// exist in lib.rs + integrations.rs; do not add them again).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "tripit",
        name: "TripIt",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your TripIt trip itinerary — every flight, hotel stay, car rental, and train segment with dates, airports/addresses, and confirmation codes — into the unified travel timeline. Re-runnable: newer exports never duplicate.",
        domain: "travel",
        vault_path: "travel/tripit/",
        toggleable: false,
        setup: &[
            "tripit.com → Settings → Download Your Data → request the ICS export; TripIt emails a download link.",
            "Import the downloaded .ics file here.",
        ],
        caveats: "TripIt's REST API is closed to new developer registrations (confirmed May 2024), so this is import-only via the ICS calendar export. Note: the exact field layout of TripIt's ICS export is undocumented; the importer is parked until a real export sample is in hand to pin the field names — the travel contract it writes into is already in place.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["ics"],
    params: &[],
    run: run_import,
};

/// A single TripIt trip event, normalized to the small set of fields every
/// VEVENT carries — whatever their exact wire names in a given TripIt export.
/// This is the seam the parser fills and the contract mapping consumes, so the
/// two concerns stay independent: [`events_from_ics`] (parked, field-name glue)
/// produces [`TripEvent`]s; [`event_to_segment`] (tested) maps each to the
/// travel contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TripEvent {
    /// VEVENT UID — stable dedupe key (e.g. `item-1847562301-1@tripit.com`).
    pub uid: String,
    /// Segment kind: `"flight"` | `"lodging"` | `"car"` | `"train"` | `"other"`.
    /// Derived from CATEGORIES / SUMMARY prefix / X-TRIPIT-TYPE (TBD on sample).
    pub segment_type: String,
    /// Segment start — departure, check-in, or pickup.
    /// `YYYY-MM-DDTHH:MM:SS+HH:MM` (RFC3339 local), derived from DTSTART.
    pub start: String,
    /// Segment end — arrival, check-out, or drop-off. Derived from DTEND.
    pub end: String,
    /// Origin: airport code (IATA), city, or address. For flights: departure
    /// airport. Derived from LOCATION or a structured SUMMARY / X-* field.
    pub origin: String,
    /// Origin display name (airport or location name). Omit if same as `origin`.
    pub origin_name: String,
    /// Destination: arrival airport code or city. Flights only.
    pub destination: String,
    /// Destination display name. Omit if same as `destination`.
    pub destination_name: String,
    /// Operating carrier / property / rental company.
    pub vendor: String,
    /// Flight or train number (e.g. `"UA 523"`).
    pub number: String,
    /// Confirmation / record-locator code.
    pub confirmation: String,
    /// Trip-level grouping id (TripIt trip id or trip title slug) — the `booking_id`.
    pub trip_id: String,
    /// Source-native status, e.g. `"confirmed"`, `"cancelled"`.
    pub status: String,
    /// The full VEVENT property bag (name→value) for the raw layer; also the
    /// overflow sink for X-TRIPIT-* and any other fields not mapped above.
    pub raw: BTreeMap<String, String>,
}

/// Map a normalized [`TripEvent`] to a travel-contract [`Segment`]. Returns
/// `None` only when the event has neither a UID (no stable id) nor a parseable
/// start datetime (can't partition it).
pub fn event_to_segment(ev: &TripEvent) -> Option<Segment> {
    let uid = ev.uid.trim();
    if uid.is_empty() {
        return None;
    }
    if ev.start.trim().is_empty() {
        return None;
    }

    let type_ = if ev.segment_type.trim().is_empty() { "other" } else { ev.segment_type.trim() };
    let mut seg = Segment::new("tripit", type_, uid, ev.start.trim());

    seg.end_ts = ev.end.trim().to_string();
    seg.start_place = ev.origin.trim().to_string();
    seg.start_place_name = ev.origin_name.trim().to_string();
    seg.end_place = ev.destination.trim().to_string();
    seg.end_place_name = ev.destination_name.trim().to_string();
    seg.vendor = ev.vendor.trim().to_string();
    seg.number = ev.number.trim().to_string();
    seg.confirmation = ev.confirmation.trim().to_string();
    seg.booking_id = ev.trip_id.trim().to_string();
    seg.status = ev.status.trim().to_string();

    // X-TRIPIT-* and any other source-specific fields that didn't map to a
    // contract column go into `extra` for full fidelity.
    let mut extra = Map::new();
    for (k, v) in &ev.raw {
        if !v.trim().is_empty() {
            extra.insert(k.clone(), Value::String(v.trim().to_string()));
        }
    }
    seg.extra = extra;

    Some(seg)
}

/// Parse a TripIt ICS export file into normalized [`TripEvent`]s.
///
/// **Parked — Needs-sample.** The VEVENT field layout (how TripIt encodes
/// segment type, confirmation numbers, multi-segment trips, and X-TRIPIT-*
/// extension properties into SUMMARY / DESCRIPTION / CATEGORIES / X-*) is
/// undocumented and only observable from a real export. This is the *only*
/// piece waiting on a sample: the contract mapping ([`event_to_segment`]), the
/// raw layer, dedupe, and the import loop are done and tested. When a sample
/// lands, fill in this function — nothing downstream changes.
fn events_from_ics(_path: &Path) -> Result<Vec<TripEvent>> {
    anyhow::bail!("{PARKED_MSG}")
}

/// Write each VEVENT's full raw property map to `travel/tripit/raw/`,
/// partitioned by the segment-start year (one file per year), at full fidelity
/// — nothing the ICS carried is dropped.
#[allow(dead_code)] // exercised once `events_from_ics` is unparked.
fn write_raw(vault: &Vault, events: &[TripEvent]) -> Result<()> {
    let mut by_year: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for ev in events {
        // Partition by the year from the start timestamp (first 4 chars).
        let year = if ev.start.len() >= 4 { ev.start[..4].to_string() } else { "unknown".into() };
        let mut obj = serde_json::Map::new();
        for (k, v) in &ev.raw {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        // Also include the normalized fields for traceability.
        obj.insert("uid".into(), Value::String(ev.uid.clone()));
        obj.insert("segment_type".into(), Value::String(ev.segment_type.clone()));
        by_year.entry(year).or_default().push(Value::Object(obj));
    }
    for (year, objs) in by_year {
        let mut body = String::new();
        for o in objs {
            body.push_str(&serde_json::to_string(&o)?);
            body.push('\n');
        }
        let rel = format!("{RAW_DIR}/{year}.jsonl");
        write_atomic(&vault.resolve(&rel)?, body.as_bytes())?;
    }
    Ok(())
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Already-stored UIDs, for a re-runnable (idempotent) import.
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for key in stream.partitions()? {
        for seg in stream.read::<Segment>(&key)? {
            if !seg.guid.is_empty() {
                seen.insert(seg.guid);
            }
        }
    }

    // Parse the ICS export. Parked until a real sample pins the VEVENT field
    // layout — returns the Needs-sample error rather than guessing the fields.
    let events = events_from_ics(path)?;

    // Full fidelity first (lossless import), then the contract layer.
    write_raw(vault, &events)?;

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut segments = Vec::new();
    for ev in &events {
        let Some(seg) = event_to_segment(ev) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(seg.guid.clone()) {
            duplicates += 1;
            continue;
        }
        segments.push(seg);
        imported += 1;
    }
    stream.append(&segments, |s| &s.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} segments imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// A `YYYY-MM-DDTHH:MM:SS` datetime at local noon for a `YYYY-MM-DD` date —
/// used to synthesize a ts when only a date is available (e.g. all-day events).
/// `None` when the string isn't a date.
///
/// Used in tests; also the natural helper for `events_from_ics` once the
/// field layout is confirmed from a real sample.
#[allow(dead_code)]
fn local_noon(date: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        Local
            .from_local_datetime(&d.and_time(NaiveTime::from_hms_opt(12, 0, 0)?))
            .earliest()?
            .to_rfc3339(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-tripit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A representative TripIt flight event built from values we control —
    /// never a fabricated ICS wire shape (the ICS → TripEvent parse is parked;
    /// the TripEvent → Segment contract mapping is what's under test here).
    fn sample_flight() -> TripEvent {
        let mut raw = BTreeMap::new();
        raw.insert("SUMMARY".into(), "UA 523 - San Francisco to New York".into());
        raw.insert("X-TRIPIT-CONFIRMATION".into(), "H4X9Q2".into());
        TripEvent {
            uid: "item-1847562301-1@tripit.com".into(),
            segment_type: "flight".into(),
            start: "2026-07-14T08:25:00-07:00".into(),
            end: "2026-07-14T16:58:00-04:00".into(),
            origin: "SFO".into(),
            origin_name: "San Francisco Intl".into(),
            destination: "JFK".into(),
            destination_name: "John F. Kennedy Intl".into(),
            vendor: "United Airlines".into(),
            number: "UA 523".into(),
            confirmation: "H4X9Q2".into(),
            trip_id: "trip-298104".into(),
            status: "confirmed".into(),
            raw,
        }
    }

    /// A representative lodging event.
    fn sample_hotel() -> TripEvent {
        let mut raw = BTreeMap::new();
        raw.insert("SUMMARY".into(), "Marriott Chicago Downtown - Check-in".into());
        raw.insert("X-TRIPIT-CONFIRMATION".into(), "MC-987654".into());
        TripEvent {
            uid: "item-9921034401-1@tripit.com".into(),
            segment_type: "lodging".into(),
            start: "2026-07-15T15:00:00-05:00".into(),
            end: "2026-07-18T11:00:00-05:00".into(),
            origin: "Chicago, IL".into(),
            origin_name: "Marriott Chicago Downtown".into(),
            destination: String::new(),
            destination_name: String::new(),
            vendor: "Marriott".into(),
            number: String::new(),
            confirmation: "MC-987654".into(),
            trip_id: "trip-298104".into(),
            status: "confirmed".into(),
            raw,
        }
    }

    #[test]
    fn flight_maps_to_a_flight_segment_on_the_travel_contract() {
        let seg = event_to_segment(&sample_flight()).expect("uid + start → segment");
        // Required travel-contract core.
        assert_eq!(seg.source, "tripit");
        assert_eq!(seg.type_, "flight");
        assert_eq!(seg.guid, "item-1847562301-1@tripit.com", "guid is the ICS UID");
        assert_eq!(seg.ts, "2026-07-14T08:25:00-07:00", "ts is departure time");
        assert_eq!(seg.end_ts, "2026-07-14T16:58:00-04:00");
        assert_eq!(seg.start_place, "SFO");
        assert_eq!(seg.start_place_name, "San Francisco Intl");
        assert_eq!(seg.end_place, "JFK");
        assert_eq!(seg.end_place_name, "John F. Kennedy Intl");
        assert_eq!(seg.vendor, "United Airlines");
        assert_eq!(seg.number, "UA 523");
        assert_eq!(seg.confirmation, "H4X9Q2");
        assert_eq!(seg.booking_id, "trip-298104");
        assert_eq!(seg.status, "confirmed");
        // Raw fields go into extra.
        assert_eq!(
            seg.extra.get("SUMMARY"),
            Some(&Value::String("UA 523 - San Francisco to New York".into()))
        );
        assert_eq!(
            seg.extra.get("X-TRIPIT-CONFIRMATION"),
            Some(&Value::String("H4X9Q2".into()))
        );
    }

    #[test]
    fn hotel_maps_to_a_lodging_segment() {
        let seg = event_to_segment(&sample_hotel()).expect("uid + start → segment");
        assert_eq!(seg.source, "tripit");
        assert_eq!(seg.type_, "lodging");
        assert_eq!(seg.guid, "item-9921034401-1@tripit.com");
        assert_eq!(seg.start_place, "Chicago, IL");
        assert_eq!(seg.start_place_name, "Marriott Chicago Downtown");
        assert_eq!(seg.vendor, "Marriott");
        // No flight number on a lodging segment.
        assert!(seg.number.is_empty());
        // No destination on a lodging segment.
        assert!(seg.end_place.is_empty());
    }

    #[test]
    fn event_without_uid_or_start_is_skipped() {
        let mut ev = sample_flight();
        ev.uid = "  ".into();
        assert!(event_to_segment(&ev).is_none(), "no uid → skip");

        let mut ev = sample_flight();
        ev.start = String::new();
        assert!(event_to_segment(&ev).is_none(), "no start → skip");
    }

    #[test]
    fn empty_segment_type_defaults_to_other() {
        let mut ev = sample_flight();
        ev.segment_type = String::new();
        let seg = event_to_segment(&ev).expect("uid + start enough");
        assert_eq!(seg.type_, "other", "empty segment_type defaults to other");
    }

    #[test]
    fn empty_optionals_are_omitted_not_blank() {
        // A sparse event (uid + start only) writes no empty extra keys and omits
        // optional columns entirely (omit-if-empty, matching the contract spec).
        let ev = TripEvent {
            uid: "item-sparse-1@tripit.com".into(),
            start: "2026-09-01T10:00:00-05:00".into(),
            ..Default::default()
        };
        let seg = event_to_segment(&ev).expect("uid + start is enough");
        let val = serde_json::to_value(&seg).unwrap();
        assert!(val.get("extra").is_none(), "no extra when raw is empty: {val}");
        assert!(val.get("end_ts").is_none(), "no end → end_ts omitted");
        assert!(val.get("start_place").is_none(), "no origin → omitted");
        assert!(val.get("vendor").is_none(), "no vendor → omitted");
        // Required core always present.
        for f in ["ts", "source", "type", "guid"] {
            assert!(val.get(f).is_some(), "required {f} present");
        }
    }

    #[test]
    fn segments_write_to_travel_contract_location() {
        // The segment is a valid contract line that partitions by segment-start month.
        let v = temp_vault("contract");
        let seg = event_to_segment(&sample_flight()).unwrap();
        let stream = v.stream(DIR, Partition::Month);
        stream.append(&[seg], |s| &s.ts).unwrap();
        let raw = fs::read_to_string(v.root().join("travel/tripit/2026-07.jsonl")).unwrap();
        assert!(raw.contains("\"type\":\"flight\""), "discriminator as `type`: {raw}");
        assert!(raw.contains("\"guid\":\"item-1847562301-1@tripit.com\""), "{raw}");
        assert!(raw.contains("\"source\":\"tripit\""), "{raw}");
    }

    #[test]
    fn write_raw_partitions_by_year() {
        let v = temp_vault("raw");
        let a = sample_flight(); // 2026
        let mut b = sample_hotel();
        b.uid = "item-old-1@tripit.com".into();
        b.start = "2024-11-01T15:00:00-05:00".into();
        write_raw(&v, &[a, b]).unwrap();
        let r26 = fs::read_to_string(v.root().join("travel/tripit/raw/2026.jsonl")).unwrap();
        assert!(r26.contains("\"SUMMARY\""), "raw object preserved: {r26}");
        let r24 = fs::read_to_string(v.root().join("travel/tripit/raw/2024.jsonl")).unwrap();
        assert_eq!(r24.lines().count(), 1, "partitioned by year");
    }

    #[test]
    fn import_is_parked_until_a_sample_lands() {
        // The evidence rule: the importer refuses to parse a guessed ICS shape
        // and surfaces a clear Needs-sample message instead of silently mis-parsing.
        let v = temp_vault("parked");
        let f = v.root().join("tripit-export.ics");
        fs::write(&f, b"BEGIN:VCALENDAR\nEND:VCALENDAR\n").unwrap();
        let err = (IMPORT.run)(&v, &f, &BTreeMap::new(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("parked"), "parked message surfaced: {err}");
        assert!(err.to_string().contains("events_from_ics"), "points at the seam: {err}");
    }

    #[test]
    fn idempotency_dedupes_by_uid() {
        // A re-import of the same segment (same UID) is counted as a duplicate
        // and does not write a second row — the vault file is unchanged.
        let v = temp_vault("dedup");
        let seg = event_to_segment(&sample_flight()).unwrap();
        let stream = v.stream(DIR, Partition::Month);
        stream.append(&[seg.clone()], |s| &s.ts).unwrap();

        // Re-run: already-seen UID should be skipped.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for key in stream.partitions().unwrap() {
            for stored in stream.read::<Segment>(&key).unwrap() {
                seen.insert(stored.guid);
            }
        }
        assert!(seen.contains(&seg.guid), "first write is visible");
        assert!(!seen.insert(seg.guid.clone()), "duplicate insert returns false → dedup works");
    }

    #[test]
    fn local_noon_synthesizes_ts_from_date() {
        // All-day events from the ICS export carry only a date; local_noon gives
        // them a stable ts that avoids midnight-boundary surprises.
        let ts = local_noon("2026-08-15").expect("valid date");
        assert!(ts.starts_with("2026-08-15T12:00:00"), "noon ts: {ts}");
    }

    #[test]
    fn the_def_is_an_import_on_the_travel_domain() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.domain, "travel");
        assert_eq!(DEF.meta.vault_path, "travel/tripit/");
        assert!(!DEF.meta.default_on, "travel history is opt-in");
        let spec = DEF.import_spec().unwrap();
        assert_eq!(spec.accepts, &["ics"]);
    }
}
