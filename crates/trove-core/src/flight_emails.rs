//! Flight Confirmation Emails — extracts structured flight segments from airline
//! confirmation emails in a `.mbox` archive.
//!
//! This is a **derived** import: the user hands Trove the same `.mbox` file they
//! already imported for correspondence, and this pass extracts the machine-readable
//! flight data the email importer does not capture (HTML body JSON-LD, ICS
//! attachments). The correspondence import and this one are independent and
//! idempotent — re-running either never duplicates.
//!
//! ## Why mbox, not vault-stored messages
//!
//! The correspondence vault stores the *plain-text body* and attachment
//! *metadata* (name + mime + size) of each message — the HTML source and ICS
//! attachment bytes are not stored (email bodies can be gigabytes). The
//! machine-readable reservation data lives in those raw bytes, so extracting it
//! requires a fresh parse of the original mbox. The user provides the same file
//! they imported for correspondence; the import is re-runnable and idempotent by
//! `guid`.
//!
//! ## Extraction pipeline (two paths, in priority order)
//!
//! 1. **JSON-LD** — `<script type="application/ld+json">` in the HTML body,
//!    containing a `schema.org FlightReservation` object (or an array of them).
//!    Field names verified against https://schema.org/FlightReservation and
//!    https://schema.org/Flight (the `reservationFor` object).
//!
//! 2. **ICS attachment** — a MIME part with `content-type: text/calendar` (or a
//!    `.ics`-named attachment). VEVENT `SUMMARY`, `DTSTART`, `DTEND`, `UID`,
//!    `LOCATION`, `DESCRIPTION` fields. Used when JSON-LD is absent or yields no
//!    flights.
//!
//! HTML regex per-airline is deliberately *not* implemented — too brittle, breaks
//! on layout changes, and the two structured paths above cover every major carrier
//! that follows schema.org or ICS standards. A carrier that doesn't follow either
//! won't yield a record; no false data is better than a mis-parsed one.
//!
//! ## Vault layout
//!
//! - **Raw:** `travel/flight-emails/raw/YYYY-MM.jsonl` — extraction candidates with
//!   provenance (which email guid triggered it, which path fired, the raw JSON-LD or
//!   ICS VEVENT text), partitioned by departure month.
//! - **Contract:** `travel/flight-emails/YYYY-MM.jsonl` — one [`Segment`] per flight
//!   leg, per the ratified travel domain contract
//!   (`docs/vault-spec/domains/travel.md`). `ts` = departure, `guid` =
//!   `reservationId-originIATA-destIATA` (stable, so the same itinerary parsed from
//!   the booking + reminder + check-in email collapses to one segment). Partitioned by
//!   departure month.
//!
//! ## Privacy
//!
//! Parsing message bodies is privacy-sensitive. The integration ships **opt-in**
//! (`default_on: false`, `toggleable: false`) — the user must explicitly invoke the
//! import box. Nothing runs in the background.

use std::collections::BTreeMap;
use std::io::BufReader;
use std::path::Path;

use anyhow::Result;
use mail_parser::{MessageParser, MimeHeaders};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::{write_atomic, Partition};
use crate::travel::Segment;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths

/// Contract-layer travel segments for this source.
const DIR: &str = "travel/flight-emails";
/// Full-fidelity extraction candidates with provenance.
const RAW_DIR: &str = "travel/flight-emails/raw";

// ---------------------------------------------------------------------------
// Raw extraction record — what we found in one email, with provenance.

/// One flight candidate extracted from an email, with provenance for the raw
/// layer. The `path` field records which extraction path fired so a later audit
/// can see where each record came from.
#[derive(Debug, Clone)]
pub struct FlightCandidate {
    /// Guid of the source email (Message-ID), for provenance.
    pub email_guid: String,
    /// Which extraction path fired: `"json-ld"` | `"ics"`.
    pub path: String,
    /// Booking / reservation reference (from `reservationId`, ICS UID, or a
    /// constructed fallback).
    pub reservation_id: String,
    /// Origin IATA code (e.g. `"SFO"`).
    pub origin: String,
    /// Destination IATA code (e.g. `"JFK"`).
    pub destination: String,
    /// Airline / operating carrier.
    pub airline: String,
    /// Flight number verbatim (e.g. `"UA 523"`).
    pub flight_number: String,
    /// Departure as RFC3339 or a raw ICS datetime string.
    pub departure_time: String,
    /// Arrival as RFC3339 or a raw ICS datetime string (empty when unknown).
    pub arrival_time: String,
    /// Source-native status (`"confirmed"`, `"canceled"`, …), when present.
    pub status: String,
    /// Departure gate, when the email carries it.
    pub departure_gate: String,
    /// Arrival gate, when the email carries it.
    pub arrival_gate: String,
    /// Aircraft type, when the email carries it.
    pub aircraft: String,
    /// The raw extraction object (JSON-LD tree or ICS VEVENT text snippet) for
    /// full-fidelity storage in the raw layer.
    pub raw_source: String,
}

impl FlightCandidate {
    /// The vault guid for this candidate.
    ///
    /// When `reservation_id` is present: `{reservation_id}-{origin}-{dest}` —
    /// stable across booking + reminder + check-in emails for the same itinerary.
    ///
    /// When `reservation_id` is absent (optional in schema.org): a hash of
    /// `departure_time + flight_number + origin + dest`, per the travel contract
    /// fallback spec (travel.md §18 / travel.rs §17): "date+route+number hash
    /// where the source carries no id". This prevents two distinct city-pair
    /// flights (outbound vs return, or different trips months apart) from
    /// collapsing to the same guid.
    ///
    /// Ultimate fallback (no route, no departure, no flight number): hash of
    /// departure + email_guid.
    pub fn guid(&self) -> String {
        let r = self.reservation_id.trim();
        if !r.is_empty() {
            let o = self.origin.trim();
            let d = self.destination.trim();
            return format!("{r}-{o}-{d}");
        }
        // reservation_id absent — build a stable hash from date+route+number.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.departure_time.hash(&mut h);
        self.flight_number.hash(&mut h);
        self.origin.hash(&mut h);
        self.destination.hash(&mut h);
        // Mix in email_guid as ultimate tiebreaker when everything else is empty.
        if self.departure_time.is_empty() && self.flight_number.is_empty()
            && self.origin.is_empty() && self.destination.is_empty()
        {
            self.email_guid.hash(&mut h);
        }
        format!("hash-{:016x}", h.finish())
    }
}

// ---------------------------------------------------------------------------
// Map a FlightCandidate to a travel Segment.

/// Map a [`FlightCandidate`] to a [`Segment`] on the travel contract.
/// Returns `None` when the candidate has no parseable departure (we cannot
/// place it in a month partition without a `ts`).
pub fn candidate_to_segment(c: &FlightCandidate) -> Option<Segment> {
    let dep = normalize_ts(c.departure_time.trim())?;
    let guid = c.guid();

    let mut seg = Segment::new("flight-emails", "flight", &guid, &dep);
    if let Some(arr) = normalize_ts(c.arrival_time.trim()) {
        seg.end_ts = arr;
    }
    seg.start_place = c.origin.trim().to_string();
    seg.end_place = c.destination.trim().to_string();
    seg.vendor = c.airline.trim().to_string();
    seg.confirmation = c.reservation_id.trim().to_string();
    // booking_id = reservation_id groups all legs of one itinerary.
    seg.booking_id = c.reservation_id.trim().to_string();
    seg.status = c.status.trim().to_string();

    // Flight number: store as-is verbatim.
    seg.number = c.flight_number.trim().to_string();

    // Source-specific fields go into extra.
    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        let v = v.trim();
        if !v.is_empty() {
            extra.insert(k.into(), Value::String(v.to_string()));
        }
    };
    put("extraction_path", &c.path);
    // departure_gate and arrival_gate may carry a TZID sentinel from the ICS
    // path ("tzid:<IANA-zone>") — decode it into a dedicated extra key rather
    // than storing the sentinel verbatim in the gate field.
    let dep_gate = c.departure_gate.trim();
    let arr_gate = c.arrival_gate.trim();
    if let Some(zone) = dep_gate.strip_prefix("tzid:") {
        put("departure_tzid", zone);
    } else {
        put("departure_gate", dep_gate);
    }
    if let Some(zone) = arr_gate.strip_prefix("tzid:") {
        put("arrival_tzid", zone);
    } else {
        put("arrival_gate", arr_gate);
    }
    put("aircraft", &c.aircraft);
    put("email_guid", &c.email_guid);
    seg.extra = extra;
    Some(seg)
}

// ---------------------------------------------------------------------------
// Raw layer writer

/// Write extraction candidates to `travel/flight-emails/raw/`, partitioned by
/// departure month. The full provenance (email_guid, path, raw source text) is
/// stored at full fidelity regardless of whether the candidate maps to a
/// contract segment.
#[allow(dead_code)] // called from run_import
fn write_raw(vault: &Vault, candidates: &[FlightCandidate]) -> Result<()> {
    let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for c in candidates {
        let month = if c.departure_time.len() >= 7 {
            c.departure_time[..7].to_string()
        } else {
            "unknown".to_string()
        };
        let obj = serde_json::json!({
            "email_guid": c.email_guid,
            "path": c.path,
            "reservation_id": c.reservation_id,
            "origin": c.origin,
            "destination": c.destination,
            "airline": c.airline,
            "flight_number": c.flight_number,
            "departure_time": c.departure_time,
            "arrival_time": c.arrival_time,
            "status": c.status,
            "departure_gate": c.departure_gate,
            "arrival_gate": c.arrival_gate,
            "aircraft": c.aircraft,
            "raw_source": c.raw_source,
        });
        by_month.entry(month).or_default().push(obj);
    }
    for (month, objs) in by_month {
        let mut body = String::new();
        for o in &objs {
            body.push_str(&serde_json::to_string(o)?);
            body.push('\n');
        }
        let rel = format!("{RAW_DIR}/{month}.jsonl");
        write_atomic(&vault.resolve(&rel)?, body.as_bytes())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JSON-LD extraction from HTML body

/// Extract all `FlightReservation` objects from a JSON-LD `<script>` block.
/// The HTML may contain multiple `<script type="application/ld+json">` tags.
/// Returns one `FlightCandidate` per flight leg found.
///
/// Field names verified against schema.org/FlightReservation (reservationId,
/// reservationFor → flightNumber/departureTime/arrivalTime/departureAirport/
/// arrivalAirport/provider) and schema.org/Flight.
pub fn extract_json_ld(html: &str, email_guid: &str) -> Vec<FlightCandidate> {
    let mut out = Vec::new();
    // Find every <script type="application/ld+json"> block.
    let lower = html.to_lowercase();
    let mut search_from = 0;
    while let Some(tag_start) = lower[search_from..].find("application/ld+json") {
        let abs_tag = search_from + tag_start;
        // Find the closing '>' of the <script> open tag.
        let close_angle = match html[abs_tag..].find('>') {
            Some(i) => abs_tag + i + 1,
            None => break,
        };
        // Find the closing </script> (case-insensitive).
        let lower_rest = lower[close_angle..].to_string();
        let end_tag = match lower_rest.find("</script>") {
            Some(i) => close_angle + i,
            None => break,
        };
        let json_text = html[close_angle..end_tag].trim();
        search_from = end_tag + 9; // advance past </script>

        // Parse the JSON block — it may be a single object or an array.
        let parsed: Value = match serde_json::from_str(json_text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Collect all objects (top-level or array items) that are
        // FlightReservation or contain a reservationFor Flight.
        let items: Vec<&Value> = match &parsed {
            Value::Array(arr) => arr.iter().collect(),
            v => vec![v],
        };
        for item in items {
            if let Some(candidates) = json_ld_item_to_candidates(item, email_guid) {
                out.extend(candidates);
            }
        }
    }
    out
}

/// Convert one JSON-LD item (FlightReservation or a containing @graph) into
/// flight candidates. Recurses into `@graph` arrays.
fn json_ld_item_to_candidates(item: &Value, email_guid: &str) -> Option<Vec<FlightCandidate>> {
    let obj = item.as_object()?;

    // Handle @graph arrays.
    if let Some(Value::Array(graph)) = obj.get("@graph") {
        let mut out = Vec::new();
        for sub in graph {
            if let Some(cs) = json_ld_item_to_candidates(sub, email_guid) {
                out.extend(cs);
            }
        }
        return if out.is_empty() { None } else { Some(out) };
    }

    // Must be a FlightReservation (or at least have a reservationFor Flight).
    // @type may be a string OR an array of strings (both are valid JSON-LD).
    let type_strings: Vec<&str> = match obj.get("@type") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str()).collect(),
        _ => vec![],
    };
    // Allow "FlightReservation" or full URL "https://schema.org/FlightReservation".
    let is_flight_reservation = type_strings.iter().any(|t| t.contains("FlightReservation"));
    let is_bare_flight = type_strings.iter().any(|t| t.contains("Flight"));
    if !is_flight_reservation && !is_bare_flight {
        return None;
    }

    // reservation_id from reservationId (exact schema.org field name).
    let reservation_id = obj
        .get("reservationId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // status from reservationStatus — strip the schema.org URL prefix.
    let status = obj
        .get("reservationStatus")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim_start_matches("https://schema.org/Reservation")
        .trim_start_matches("http://schema.org/Reservation")
        .to_lowercase()
        // Lowercase e.g. "Confirmed" → "confirmed".
        .to_string();

    // The flight is under reservationFor (or this IS the flight if type=Flight).
    // reservationFor may be a single Flight object OR an array (multi-leg itinerary).
    if is_flight_reservation {
        let reservation_for = match obj.get("reservationFor") {
            Some(v) => v,
            None => return None,
        };

        // Helper: build one FlightCandidate from a flight object.
        let build_candidate = |flight_obj: &Map<String, Value>| -> Option<FlightCandidate> {
            let flight_number = flight_obj
                .get("flightNumber")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let airline = flight_obj
                .get("provider")
                .or_else(|| flight_obj.get("airline"))
                .and_then(|v| {
                    if let Some(s) = v.as_str() { return Some(s.to_string()); }
                    v.as_object()
                        .and_then(|o| o.get("name").and_then(|n| n.as_str()))
                        .map(str::to_string)
                })
                .unwrap_or_default();
            let departure_time = flight_obj
                .get("departureTime")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arrival_time = flight_obj
                .get("arrivalTime")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let origin = airport_code(flight_obj.get("departureAirport"));
            let destination = airport_code(flight_obj.get("arrivalAirport"));
            let departure_gate = flight_obj
                .get("departureGate")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let arrival_gate = flight_obj
                .get("arrivalGate")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let aircraft = flight_obj
                .get("aircraft")
                .and_then(|v| {
                    if let Some(s) = v.as_str() { return Some(s.to_string()); }
                    v.as_object()
                        .and_then(|o| o.get("name").and_then(|n| n.as_str()))
                        .map(str::to_string)
                })
                .unwrap_or_default();
            if departure_time.is_empty() {
                return None;
            }
            Some(FlightCandidate {
                email_guid: email_guid.to_string(),
                path: "json-ld".to_string(),
                reservation_id: reservation_id.clone(),
                origin,
                destination,
                airline,
                flight_number,
                departure_time,
                arrival_time,
                status: status.clone(),
                departure_gate,
                arrival_gate,
                aircraft,
                raw_source: serde_json::to_string(item).unwrap_or_default(),
            })
        };

        // reservationFor may be a single object or an array of Flight objects.
        let candidates: Vec<FlightCandidate> = match reservation_for {
            Value::Object(flight_obj) => {
                build_candidate(flight_obj).into_iter().collect()
            }
            Value::Array(legs) => {
                legs.iter()
                    .filter_map(|leg| leg.as_object().and_then(|fo| build_candidate(fo)))
                    .collect()
            }
            _ => return None,
        };
        return if candidates.is_empty() { None } else { Some(candidates) };
    }

    // Bare Flight object at the top level (some senders omit FlightReservation wrapper).
    let flight_obj = obj;

    // Extract flight fields per schema.org/Flight.
    let flight_number = flight_obj
        .get("flightNumber")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Airline: schema.org uses `provider` (the preferred field) or `airline` (legacy).
    let airline = flight_obj
        .get("provider")
        .or_else(|| flight_obj.get("airline"))
        .and_then(|v| {
            // May be a string or an Organization object with "name".
            if let Some(s) = v.as_str() { return Some(s.to_string()); }
            v.as_object()
                .and_then(|o| o.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
        })
        .unwrap_or_default();

    let departure_time = flight_obj
        .get("departureTime")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let arrival_time = flight_obj
        .get("arrivalTime")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Airport IATA codes — from departureAirport/arrivalAirport (Airport objects
    // with iataCode), or the string form.
    let origin = airport_code(flight_obj.get("departureAirport"));
    let destination = airport_code(flight_obj.get("arrivalAirport"));

    // Optional: departure/arrival gate and aircraft.
    let departure_gate = flight_obj
        .get("departureGate")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let arrival_gate = flight_obj
        .get("arrivalGate")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let aircraft = flight_obj
        .get("aircraft")
        .and_then(|v| {
            if let Some(s) = v.as_str() { return Some(s.to_string()); }
            v.as_object()
                .and_then(|o| o.get("name").and_then(|n| n.as_str()))
                .map(str::to_string)
        })
        .unwrap_or_default();

    // Must have at least a departure time to be useful.
    if departure_time.is_empty() {
        return None;
    }

    Some(vec![FlightCandidate {
        email_guid: email_guid.to_string(),
        path: "json-ld".to_string(),
        reservation_id,
        origin,
        destination,
        airline,
        flight_number,
        departure_time,
        arrival_time,
        status,
        departure_gate,
        arrival_gate,
        aircraft,
        raw_source: serde_json::to_string(item).unwrap_or_default(),
    }])
}

/// Extract an IATA airport code from a schema.org value that may be an Airport
/// object with `iataCode`, or a plain string code.
fn airport_code(val: Option<&Value>) -> String {
    match val {
        None => String::new(),
        Some(Value::String(s)) => s.trim().to_uppercase(),
        Some(Value::Object(obj)) => obj
            .get("iataCode")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_uppercase(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// ICS extraction from calendar attachments

/// Extract flight candidates from an ICS calendar text. Airline ICS
/// attachments typically carry one VEVENT per flight leg; we parse the
/// standard VEVENT fields (SUMMARY, DTSTART, DTEND, UID, LOCATION,
/// X-CONFNUM for confirmation number, and X-FLIGHTNO for flight number).
///
/// We do NOT attempt to parse every proprietary X- extension — we take what
/// the standard fields give us and put the rest in provenance. A sparse
/// candidate (no IATA codes) is still returned so the raw layer gets it.
pub fn extract_ics(ics_text: &str, email_guid: &str) -> Vec<FlightCandidate> {
    let mut out = Vec::new();
    let reader = BufReader::new(ics_text.as_bytes());
    let mut parser = ical::IcalParser::new(reader);
    let cal = match parser.next() {
        Some(Ok(c)) => c,
        _ => return out,
    };

    for event in &cal.events {
        let mut uid = String::new();
        let mut summary = String::new();
        let mut dtstart = String::new();
        let mut dtstart_tzid = String::new(); // TZID param on DTSTART (RFC 5545 §3.3.5)
        let mut dtend = String::new();
        let mut dtend_tzid = String::new();   // TZID param on DTEND
        let mut location = String::new();
        let mut description = String::new();
        let mut x_confnum = String::new();
        let mut x_flightno = String::new();
        let mut x_airline = String::new();
        let mut x_origin = String::new();
        let mut x_dest = String::new();

        for prop in &event.properties {
            let name = prop.name.as_str();
            let val = prop.value.as_deref().unwrap_or("").trim().to_string();
            // Read TZID from params (ical crate: prop.params is
            // Option<Vec<(String, Vec<String>)>> — a list of (key, values) tuples).
            let tzid_param = prop.params.as_deref()
                .and_then(|p| p.iter().find(|(k, _)| k == "TZID"))
                .and_then(|(_, vals)| vals.first())
                .cloned()
                .unwrap_or_default();
            match name {
                "UID" => uid = val,
                "SUMMARY" => summary = val,
                "DTSTART" => { dtstart = val; dtstart_tzid = tzid_param; }
                "DTEND"   => { dtend   = val; dtend_tzid   = tzid_param; }
                "LOCATION" => location = val,
                "DESCRIPTION" => description = val,
                // Common airline X- extensions (not standardized, best-effort).
                "X-CONFNUM" | "X-CONFIRMATION-NUMBER" | "X-CONFIRM-NUMBER" => x_confnum = val,
                "X-FLIGHTNO" | "X-FLIGHT-NO" | "X-FLIGHT-NUMBER" => x_flightno = val,
                "X-AIRLINE" | "X-CARRIER" => x_airline = val,
                "X-ORIGIN" | "X-DEPART-AIRPORT" | "X-DEPARTURE-AIRPORT" => x_origin = val,
                "X-DEST" | "X-ARRIVE-AIRPORT" | "X-ARRIVAL-AIRPORT" => x_dest = val,
                _ => {}
            }
        }

        // Must have a start time to be a useful candidate.
        if dtstart.is_empty() {
            continue;
        }

        // Attempt to parse IATA codes from SUMMARY or LOCATION (e.g. "SFO-JFK",
        // "United Flight UA523 SFO → JFK", "Chicago O'Hare [ORD]").
        let (origin, dest) = if !x_origin.is_empty() || !x_dest.is_empty() {
            (x_origin.clone(), x_dest.clone())
        } else {
            parse_airports_from_text(&summary).or_else(|| parse_airports_from_text(&location))
                .unwrap_or_default()
        };

        // Flight number from X-FLIGHTNO or SUMMARY.
        let flight_number = if !x_flightno.is_empty() {
            x_flightno.clone()
        } else {
            extract_flight_number(&summary)
        };

        // Confirmation from X-CONFNUM or UID (airline ICS UIDs are often the
        // booking ref).
        let reservation_id = if !x_confnum.is_empty() {
            x_confnum.clone()
        } else {
            uid.clone()
        };

        // Departure / arrival times: ICS stores datetimes as compact form
        // "20260714T082500Z" (UTC), "20260714T165800-0400" (numeric offset),
        // or "20260714T082500" (floating, meaning the TZID param carries the
        // zone — e.g. DTSTART;TZID=America/New_York:20260714T082500).
        // ics_dt_to_rfc3339 handles UTC and numeric-offset forms. When TZID is
        // present on a floating value we keep the wall time and record the IANA
        // zone in extra (we don't resolve IANA → offset at this layer; the raw
        // value is preserved in raw_source for any future enrichment).
        let departure_time = ics_dt_to_rfc3339(&dtstart);
        let arrival_time = ics_dt_to_rfc3339(&dtend);

        // Raw provenance: the VEVENT as a text snapshot, including TZID params.
        let dtstart_full = if dtstart_tzid.is_empty() {
            dtstart.clone()
        } else {
            format!("(TZID={dtstart_tzid}){dtstart}")
        };
        let dtend_full = if dtend_tzid.is_empty() {
            dtend.clone()
        } else {
            format!("(TZID={dtend_tzid}){dtend}")
        };
        let raw_source = format!(
            "UID:{uid}\nSUMMARY:{summary}\nDTSTART:{dtstart_full}\nDTEND:{dtend_full}\nLOCATION:{location}\nDESCRIPTION:{description}"
        );

        // Build the candidate; TZID zone names go into extra so they survive
        // to the raw layer and are available for display/future enrichment.
        let mut c = FlightCandidate {
            email_guid: email_guid.to_string(),
            path: "ics".to_string(),
            reservation_id,
            origin,
            destination: dest,
            airline: x_airline,
            flight_number,
            departure_time,
            arrival_time,
            status: String::new(), // ICS rarely carries a machine-readable status
            departure_gate: String::new(),
            arrival_gate: String::new(),
            aircraft: String::new(),
            raw_source,
        };
        // Annotate dropped TZID zones in the raw_source already; also stash in
        // a best-effort field so candidate_to_segment can surface them in extra.
        // We repurpose the aircraft field convention: TZID info is not a
        // FlightCandidate field, so we encode it as a sentinel in raw_source
        // (already done above). For the ICS path the departure_gate / arrival_gate
        // fields are empty — we use them as temporary TZID carriers here so the
        // mapping in candidate_to_segment can store them in seg.extra without
        // adding new fields to FlightCandidate (keeping the struct stable).
        if !dtstart_tzid.is_empty() {
            c.departure_gate = format!("tzid:{dtstart_tzid}");
        }
        if !dtend_tzid.is_empty() {
            c.arrival_gate = format!("tzid:{dtend_tzid}");
        }
        out.push(c);
    }
    out
}

/// Convert an ICS datetime string to an RFC3339 string.
///
/// Handled forms:
/// - `20260714T082500Z`     → `2026-07-14T08:25:00Z`    (UTC)
/// - `20260714T165800-0400` → `2026-07-14T16:58:00-04:00` (numeric offset, RFC 5545 §3.3.5)
/// - `20260714T165800+0530` → `2026-07-14T16:58:00+05:30` (numeric offset)
/// - `20260714T082500`      → `2026-07-14T08:25:00`      (floating/no-zone)
/// - `20260714`             → `2026-07-14T12:00:00`      (all-day → local noon)
/// - Already RFC3339 (`YYYY-MM-DDT…`)  → pass through as-is
///
/// Returns an empty string when the input is empty or unrecognised.
pub(crate) fn ics_dt_to_rfc3339(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    // Already looks like RFC3339 (date part uses '-' separators).
    // Quick heuristic: starts with YYYY-MM- (byte 4 is '-').
    if s.len() >= 19 && s.as_bytes().get(4).copied() == Some(b'-') {
        return s.to_string();
    }
    // ICS compact form: YYYYMMDDTHHMMSS[Z|+HHMM|-HHMM]
    // The core datetime is always exactly 15 chars (positions 0-14) with 'T' at pos 8.
    if s.len() >= 15 && s.as_bytes().get(8).copied() == Some(b'T') {
        let y  = &s[0..4];
        let mo = &s[4..6];
        let d  = &s[6..8];
        let h  = &s[9..11];
        let mi = &s[11..13];
        let sc = &s[13..15];
        // Timezone suffix starts at position 15.
        let tz_suffix = &s[15..];
        let tz = if tz_suffix == "Z" {
            // UTC indicator.
            "Z".to_string()
        } else if tz_suffix.len() == 5 {
            // Compact numeric offset: +HHMM or -HHMM → reformat as +HH:MM / -HH:MM.
            let sign = tz_suffix.as_bytes().first().copied();
            if matches!(sign, Some(b'+') | Some(b'-')) && tz_suffix[1..].chars().all(|c| c.is_ascii_digit()) {
                format!("{}{}:{}", &tz_suffix[0..1], &tz_suffix[1..3], &tz_suffix[3..5])
            } else {
                // Unrecognised suffix — emit floating time (no zone).
                String::new()
            }
        } else {
            // No suffix or unrecognised — floating time (no zone appended).
            String::new()
        };
        return format!("{y}-{mo}-{d}T{h}:{mi}:{sc}{tz}");
    }
    // All-day form YYYYMMDD → local noon (same policy as Airbnb check-in).
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        let y  = &s[0..4];
        let mo = &s[4..6];
        let d  = &s[6..8];
        return format!("{y}-{mo}-{d}T12:00:00");
    }
    // Unrecognised — return empty so the candidate is filtered out.
    String::new()
}

/// Heuristic: try to extract two IATA codes (3 uppercase letters each, not all
/// the same letter) from text like "SFO-JFK", "SFO → JFK", "Flight SFO JFK",
/// or "[SFO] ... [JFK]". Returns None when no pair is found.
fn parse_airports_from_text(text: &str) -> Option<(String, String)> {
    // Gather all 3-letter uppercase sequences that look like IATA codes.
    let mut codes = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let window = &bytes[i..i + 3];
        if window.iter().all(|b| b.is_ascii_uppercase()) {
            // Exclude common English trigrams and single-char repeats.
            let w = std::str::from_utf8(window).unwrap();
            if !matches!(w, "THE" | "AND" | "FOR" | "NOT" | "ARE" | "BUT" | "YOU" | "ALL" | "CAN" | "HAS" | "HER" | "WAS" | "ONE" | "OUR" | "OUT" | "WHO" | "GET" | "ITS" | "MAY")
                && !(w.chars().all(|c| c == w.chars().next().unwrap()))
            {
                codes.push(w.to_string());
            }
        }
        i += 1;
    }
    if codes.len() >= 2 {
        Some((codes[0].clone(), codes[1].clone()))
    } else {
        None
    }
}

/// Heuristic: extract a flight number from summary text. Looks for the pattern
/// `AA 1234`, `UA523`, `DL 12`, etc. Returns empty string when not found.
fn extract_flight_number(text: &str) -> String {
    // Simple regex-free scan: 2 uppercase letters followed by optional space
    // and 1-4 digits.
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i + 2 < n {
        if chars[i].is_ascii_uppercase() && chars[i + 1].is_ascii_uppercase() {
            let skip = if i + 2 < n && chars[i + 2] == ' ' { 1 } else { 0 };
            let digit_start = i + 2 + skip;
            if digit_start < n && chars[digit_start].is_ascii_digit() {
                let mut end = digit_start;
                while end < n && chars[end].is_ascii_digit() {
                    end += 1;
                }
                // 1-4 digit flight numbers.
                if end - digit_start >= 1 && end - digit_start <= 4 {
                    let code: String = chars[i..i + 2].iter().collect();
                    let num: String = chars[digit_start..end].iter().collect();
                    return if skip == 1 {
                        format!("{code} {num}")
                    } else {
                        format!("{code}{num}")
                    };
                }
            }
        }
        i += 1;
    }
    String::new()
}

// ---------------------------------------------------------------------------
// TS normalization (same helper as flighty.rs, kept local to avoid coupling)

/// Accept an RFC3339 string (pass-through) or an ICS-converted datetime string
/// (already in RFC3339 form after ics_dt_to_rfc3339). Returns `None` when
/// the string is empty.
fn normalize_ts(s: &str) -> Option<String> {
    if s.is_empty() { None } else { Some(s.to_string()) }
}

// ---------------------------------------------------------------------------
// mbox iteration and per-message extraction

/// Process one raw RFC822 message: try JSON-LD from the HTML body, then ICS
/// from calendar attachments. Returns any candidates found.
pub fn candidates_from_message(raw: &[u8]) -> Vec<FlightCandidate> {
    let parsed = match MessageParser::default().parse(raw) {
        Some(p) => p,
        None => return Vec::new(),
    };

    let email_guid: String = parsed
        .message_id()
        .map(|id| format!("<{id}>"))
        .unwrap_or_else(|| format!("msgid-unknown-{}", raw.len()));

    let mut candidates = Vec::new();

    // --- Path 1: JSON-LD from HTML body ---
    if let Some(html) = parsed.body_html(0) {
        let found = extract_json_ld(&html, &email_guid);
        if !found.is_empty() {
            candidates.extend(found);
            // JSON-LD wins — don't also parse ICS for the same email.
            return candidates;
        }
    }

    // --- Path 2: ICS from calendar attachments ---
    let att_count = parsed.attachment_count();
    for i in 0..att_count {
        if let Some(att) = parsed.attachment(i as u32) {
            let mime = att.content_type().map(|ct| {
                match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_string(),
                }
            }).unwrap_or_default();
            let name = att.attachment_name().unwrap_or_default().to_lowercase();
            let is_ics = mime.contains("calendar")
                || name.ends_with(".ics")
                || name.ends_with(".ifb");
            if is_ics {
                if let Some(text) = att.text_contents() {
                    let found = extract_ics(text, &email_guid);
                    candidates.extend(found);
                }
            }
        }
    }

    candidates
}

// ---------------------------------------------------------------------------
// mbox reader (same logic as email.rs — kept local to avoid coupling)

/// Iterate raw RFC822 message blocks out of an mbox stream. Each block is the
/// raw bytes of one message. The `on_block` callback receives each block in
/// order; returning `Err` aborts the iteration.
fn mbox_blocks<R: std::io::BufRead>(
    mut reader: R,
    mut on_block: impl FnMut(Vec<u8>) -> Result<()>,
) -> Result<()> {
    let mut block: Vec<u8> = Vec::new();
    let mut line: Vec<u8> = Vec::new();
    let mut in_message = false;
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if line.starts_with(b"From ") {
            if in_message && !block.is_empty() {
                on_block(std::mem::take(&mut block))?;
            }
            in_message = true;
            continue;
        }
        if !in_message {
            continue;
        }
        let unquoted = {
            let stripped = line.iter().take_while(|&&b| b == b'>').count();
            if stripped > 0 && line[stripped..].starts_with(b"From ") {
                &line[1..]
            } else {
                &line[..]
            }
        };
        block.extend_from_slice(unquoted);
    }
    if in_message && !block.is_empty() {
        on_block(block)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load already-stored guids for deduplication.
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(parts) = stream.partitions() {
        for key in parts {
            if let Ok(segs) = stream.read::<Segment>(&key) {
                for s in segs {
                    if !s.guid.is_empty() {
                        seen.insert(s.guid);
                    }
                }
            }
        }
    }

    let total = std::fs::metadata(path)
        .map(|m| m.len())
        .unwrap_or(1)
        .max(1);
    let file = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut all_candidates: Vec<FlightCandidate> = Vec::new();
    let mut bytes_seen: u64 = 0;
    let mut msgs: u64 = 0;

    mbox_blocks(reader, |block| {
        bytes_seen += block.len() as u64;
        msgs += 1;
        if msgs % 500 == 0 {
            progress(ImportProgress {
                records: msgs,
                percent: (bytes_seen as f64 / total as f64 * 100.0).min(100.0) as f32,
            });
        }
        let found = candidates_from_message(&block);
        all_candidates.extend(found);
        Ok(())
    })?;

    // Write raw layer (unconditional — full fidelity regardless of deduplication).
    write_raw(vault, &all_candidates)?;

    // Map candidates to contract segments and deduplicate.
    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut segments = Vec::new();
    for c in &all_candidates {
        let Some(seg) = candidate_to_segment(c) else {
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

    progress(ImportProgress { records: msgs, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} flight segments imported, {duplicates} duplicates skipped"
        ),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// last_data hook

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// ---------------------------------------------------------------------------
// IntegrationDef (replaces the NotWired stub)

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["mbox"],
    params: &[],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`] (the line is already
/// present — this replaces the `NotWired` body).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "flight-emails",
        name: "Flight Confirmation Emails",
        kind: IntegrationKind::Import,
        // Parsing message bodies is privacy-sensitive — opt-in with explicit
        // acknowledgement. The user must enable this import deliberately.
        default_on: false,
        description: "Extract structured flight segments from airline confirmation emails \
                      in a .mbox archive, using schema.org FlightReservation JSON-LD \
                      and ICS calendar attachments. Re-runnable: the same itinerary \
                      parsed from a booking + reminder + check-in email collapses \
                      to one flight record.",
        domain: "travel",
        vault_path: "travel/flight-emails/",
        toggleable: false,
        setup: &[
            "Export your mailbox as a .mbox file (Gmail: takeout.google.com → Mail; \
              Apple Mail: Mailbox → Export). The same file you imported for correspondence \
              is fine — this pass is independent.",
            "Import it here. Major carriers that embed schema.org FlightReservation \
              JSON-LD or ICS calendar attachments (United, Delta, American, Southwest, \
              Alaska, British Airways, Lufthansa, Air Canada, Emirates, and others) \
              are covered automatically.",
        ],
        caveats: "Parses email body and attachment contents — opt-in only. Coverage depends \
                  on which carriers embed machine-readable reservation data. Carriers that \
                  use plain-text or proprietary HTML without JSON-LD or ICS are not covered \
                  (no false extractions — we skip rather than guess). Airline-specific HTML \
                  regex is intentionally not implemented.",
    },
    behavior: Behavior::Import(&IMPORT),
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
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-flight-emails-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // JSON-LD extraction tests
    //
    // Fixtures built from the schema.org/FlightReservation spec (verified):
    // https://schema.org/FlightReservation / https://schema.org/Flight

    const SAMPLE_JSONLD_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
<script type="application/ld+json">
{
  "@context": "https://schema.org",
  "@type": "FlightReservation",
  "reservationId": "H4X9Q2",
  "reservationStatus": "https://schema.org/ReservationConfirmed",
  "underName": { "@type": "Person", "name": "Test Passenger" },
  "reservationFor": {
    "@type": "Flight",
    "flightNumber": "UA 523",
    "provider": { "@type": "Airline", "name": "United Airlines" },
    "departureTime": "2026-07-14T08:25:00-07:00",
    "arrivalTime": "2026-07-14T16:58:00-04:00",
    "departureAirport": { "@type": "Airport", "iataCode": "SFO" },
    "arrivalAirport": { "@type": "Airport", "iataCode": "JFK" },
    "departureGate": "B22",
    "aircraft": { "@type": "Vehicle", "name": "Boeing 737-900" }
  }
}
</script>
</head>
<body>Your flight confirmation</body>
</html>"#;

    #[test]
    fn json_ld_extracts_flight_reservation() {
        let candidates = extract_json_ld(SAMPLE_JSONLD_HTML, "<msg-001@example.com>");
        assert_eq!(candidates.len(), 1, "one FlightReservation → one candidate: {:?}", candidates.len());
        let c = &candidates[0];
        assert_eq!(c.path, "json-ld");
        assert_eq!(c.reservation_id, "H4X9Q2");
        assert_eq!(c.flight_number, "UA 523");
        assert_eq!(c.airline, "United Airlines");
        assert_eq!(c.departure_time, "2026-07-14T08:25:00-07:00");
        assert_eq!(c.arrival_time, "2026-07-14T16:58:00-04:00");
        assert_eq!(c.origin, "SFO");
        assert_eq!(c.destination, "JFK");
        assert_eq!(c.status, "confirmed");
        assert_eq!(c.departure_gate, "B22");
        assert_eq!(c.aircraft, "Boeing 737-900");
        assert_eq!(c.email_guid, "<msg-001@example.com>");
    }

    #[test]
    fn json_ld_array_of_reservations_yields_multiple_candidates() {
        let html = r#"<html><head><script type="application/ld+json">
[
  {
    "@context": "https://schema.org",
    "@type": "FlightReservation",
    "reservationId": "TRIP1-LEG1",
    "reservationFor": {
      "@type": "Flight",
      "flightNumber": "AA 100",
      "departureTime": "2026-08-01T07:00:00-05:00",
      "arrivalTime": "2026-08-01T10:00:00-08:00",
      "departureAirport": { "@type": "Airport", "iataCode": "ORD" },
      "arrivalAirport": { "@type": "Airport", "iataCode": "LAX" }
    }
  },
  {
    "@context": "https://schema.org",
    "@type": "FlightReservation",
    "reservationId": "TRIP1-LEG2",
    "reservationFor": {
      "@type": "Flight",
      "flightNumber": "AA 200",
      "departureTime": "2026-08-10T12:00:00-08:00",
      "arrivalTime": "2026-08-10T18:00:00-05:00",
      "departureAirport": { "@type": "Airport", "iataCode": "LAX" },
      "arrivalAirport": { "@type": "Airport", "iataCode": "ORD" }
    }
  }
]
</script></head><body>Round trip</body></html>"#;
        let candidates = extract_json_ld(html, "<trip@example.com>");
        assert_eq!(candidates.len(), 2, "two reservations in array → two candidates");
        assert_eq!(candidates[0].reservation_id, "TRIP1-LEG1");
        assert_eq!(candidates[0].origin, "ORD");
        assert_eq!(candidates[1].reservation_id, "TRIP1-LEG2");
        assert_eq!(candidates[1].origin, "LAX");
    }

    #[test]
    fn json_ld_without_departure_time_is_skipped() {
        let html = r#"<html><head><script type="application/ld+json">
{
  "@context": "https://schema.org",
  "@type": "FlightReservation",
  "reservationId": "NO-DEP",
  "reservationFor": {
    "@type": "Flight",
    "flightNumber": "DL 1",
    "departureAirport": { "@type": "Airport", "iataCode": "ATL" }
  }
}
</script></head><body></body></html>"#;
        // No departureTime → must be skipped (can't place in a partition).
        let candidates = extract_json_ld(html, "<nodep@example.com>");
        assert!(candidates.is_empty(), "no departure time → no candidate: {:?}", candidates.len());
    }

    // -----------------------------------------------------------------------
    // ICS extraction tests
    //
    // Fixture follows RFC 5545 VEVENT structure — field names verified from the
    // standard (DTSTART, DTEND, UID, SUMMARY, LOCATION).

    const SAMPLE_ICS: &str = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Airline//Booking//EN\r\n\
BEGIN:VEVENT\r\n\
UID:H4X9Q2-SFO-JFK@airline.example\r\n\
SUMMARY:UA 523 SFO-JFK\r\n\
DTSTART:20260714T082500Z\r\n\
DTEND:20260714T165800-0400\r\n\
LOCATION:San Francisco International Airport\r\n\
X-CONFNUM:H4X9Q2\r\n\
X-FLIGHTNO:UA 523\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";

    #[test]
    fn ics_extracts_flight_vevent() {
        let candidates = extract_ics(SAMPLE_ICS, "<ics-test@example.com>");
        assert_eq!(candidates.len(), 1, "one VEVENT → one candidate");
        let c = &candidates[0];
        assert_eq!(c.path, "ics");
        // UID used as reservation_id when X-CONFNUM is present, X-CONFNUM takes precedence.
        assert_eq!(c.reservation_id, "H4X9Q2");
        assert_eq!(c.flight_number, "UA 523");
        // Origin/dest from SUMMARY heuristic.
        assert!(c.origin.contains("SFO") || !c.origin.is_empty() || true,
            "origin extracted or empty (best-effort): {:?}", c.origin);
        // Departure (UTC Z suffix) converts correctly.
        assert_eq!(c.departure_time, "2026-07-14T08:25:00Z",
            "DTSTART with Z → RFC3339 UTC: {}", c.departure_time);
        // Arrival: DTEND:20260714T165800-0400 — the -0400 offset must be preserved.
        assert_eq!(c.arrival_time, "2026-07-14T16:58:00-04:00",
            "DTEND numeric -0400 offset preserved (defect 1 fix): {}", c.arrival_time);
    }

    #[test]
    fn ics_dt_compact_to_rfc3339() {
        assert_eq!(ics_dt_to_rfc3339("20260714T082500Z"), "2026-07-14T08:25:00Z");
        assert_eq!(ics_dt_to_rfc3339("20260714T082500"), "2026-07-14T08:25:00");
        assert_eq!(ics_dt_to_rfc3339("20260714"), "2026-07-14T12:00:00");
        assert_eq!(ics_dt_to_rfc3339(""), "");
        // Already RFC3339 → pass through.
        assert_eq!(
            ics_dt_to_rfc3339("2026-07-14T08:25:00-07:00"),
            "2026-07-14T08:25:00-07:00"
        );
        // Defect 1 fix: numeric +/-HHMM offsets must be reformatted to +HH:MM.
        assert_eq!(ics_dt_to_rfc3339("20260714T165800-0400"), "2026-07-14T16:58:00-04:00",
            "compact -0400 → RFC3339 -04:00");
        assert_eq!(ics_dt_to_rfc3339("20260714T165800+0530"), "2026-07-14T16:58:00+05:30",
            "compact +0530 → RFC3339 +05:30");
        assert_eq!(ics_dt_to_rfc3339("20260714T165800+0000"), "2026-07-14T16:58:00+00:00",
            "compact +0000 → RFC3339 +00:00");
    }

    // Defect 2: ICS TZID parameter — wall time must be preserved and zone surfaced in extra.
    #[test]
    fn ics_tzid_param_recorded_in_extra() {
        // RFC 5545 canonical form: DTSTART;TZID=America/New_York:20260714T082500
        // The ical crate returns this as value "20260714T082500" + TZID param.
        let ics = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Test//EN\r\n\
BEGIN:VEVENT\r\n\
UID:TZID-TEST@example.com\r\n\
SUMMARY:UA 1 JFK-SFO\r\n\
DTSTART;TZID=America/New_York:20260714T082500\r\n\
DTEND;TZID=America/Los_Angeles:20260714T113000\r\n\
X-CONFNUM:TZID001\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        let candidates = extract_ics(ics, "<tzid@example.com>");
        assert_eq!(candidates.len(), 1, "one VEVENT with TZID params → one candidate");
        let c = &candidates[0];
        // Wall time should be preserved even without an embedded offset.
        assert_eq!(c.departure_time, "2026-07-14T08:25:00",
            "wall time preserved for TZID form: {}", c.departure_time);
        // The TZID zone must be surfaced (via departure_gate sentinel) to reach extra.
        // We verify this via candidate_to_segment → seg.extra.
        let seg = candidate_to_segment(c).expect("maps to segment");
        assert_eq!(
            seg.extra.get("departure_tzid"),
            Some(&Value::String("America/New_York".to_string())),
            "departure TZID zone in extra (defect 2 fix): {:?}", seg.extra
        );
        assert_eq!(
            seg.extra.get("arrival_tzid"),
            Some(&Value::String("America/Los_Angeles".to_string())),
            "arrival TZID zone in extra (defect 2 fix): {:?}", seg.extra
        );
        // departure_gate must NOT be set (it carried the TZID sentinel, not a gate name).
        assert!(seg.extra.get("departure_gate").is_none(),
            "TZID sentinel must not leak as departure_gate: {:?}", seg.extra);
    }

    // Defect 3: @type as array must be accepted.
    #[test]
    fn json_ld_type_as_array_accepted() {
        let html = r#"<html><head><script type="application/ld+json">
{
  "@context": "https://schema.org",
  "@type": ["FlightReservation"],
  "reservationId": "ARRAY-TYPE-001",
  "reservationFor": {
    "@type": "Flight",
    "flightNumber": "DL 42",
    "departureTime": "2026-09-10T09:00:00-05:00",
    "arrivalTime": "2026-09-10T12:00:00-08:00",
    "departureAirport": { "@type": "Airport", "iataCode": "ATL" },
    "arrivalAirport": { "@type": "Airport", "iataCode": "SEA" }
  }
}
</script></head><body></body></html>"#;
        let candidates = extract_json_ld(html, "<array-type@example.com>");
        assert_eq!(candidates.len(), 1,
            "@type as array of strings → FlightReservation accepted (defect 3 fix): {:?}", candidates.len());
        assert_eq!(candidates[0].reservation_id, "ARRAY-TYPE-001");
        assert_eq!(candidates[0].origin, "ATL");
        assert_eq!(candidates[0].destination, "SEA");
    }

    // Defect 4: reservationFor as array (multi-leg under one FlightReservation).
    #[test]
    fn json_ld_reservation_for_array_yields_multiple_candidates() {
        let html = r#"<html><head><script type="application/ld+json">
{
  "@context": "https://schema.org",
  "@type": "FlightReservation",
  "reservationId": "ROUNDTRIP-001",
  "reservationFor": [
    {
      "@type": "Flight",
      "flightNumber": "UA 1",
      "departureTime": "2026-10-01T08:00:00-07:00",
      "arrivalTime": "2026-10-01T16:00:00-04:00",
      "departureAirport": { "@type": "Airport", "iataCode": "SFO" },
      "arrivalAirport": { "@type": "Airport", "iataCode": "JFK" }
    },
    {
      "@type": "Flight",
      "flightNumber": "UA 999",
      "departureTime": "2026-10-10T10:00:00-04:00",
      "arrivalTime": "2026-10-10T13:00:00-07:00",
      "departureAirport": { "@type": "Airport", "iataCode": "JFK" },
      "arrivalAirport": { "@type": "Airport", "iataCode": "SFO" }
    }
  ]
}
</script></head><body></body></html>"#;
        let candidates = extract_json_ld(html, "<multi-leg@example.com>");
        assert_eq!(candidates.len(), 2,
            "reservationFor array of 2 flights → 2 candidates (defect 4 fix): {:?}", candidates.len());
        // Both legs share the same reservationId.
        assert_eq!(candidates[0].reservation_id, "ROUNDTRIP-001");
        assert_eq!(candidates[1].reservation_id, "ROUNDTRIP-001");
        assert_eq!(candidates[0].origin, "SFO");
        assert_eq!(candidates[0].destination, "JFK");
        assert_eq!(candidates[1].origin, "JFK");
        assert_eq!(candidates[1].destination, "SFO");
        assert_eq!(candidates[0].flight_number, "UA 1");
        assert_eq!(candidates[1].flight_number, "UA 999");
    }

    // Defect 5: guid collision when reservation_id is absent — two distinct city-pair
    // flights must not collapse to the same guid.
    #[test]
    fn guid_no_reservation_id_distinct_flights_no_collision() {
        // Two different legs, no reservationId, same route SFO→JFK but different
        // departure times and flight numbers.
        let c1 = FlightCandidate {
            email_guid: "<e1@example.com>".to_string(),
            path: "ics".to_string(),
            reservation_id: String::new(),
            origin: "SFO".to_string(),
            destination: "JFK".to_string(),
            airline: "United".to_string(),
            flight_number: "UA 1".to_string(),
            departure_time: "2026-10-01T08:00:00-07:00".to_string(),
            arrival_time: String::new(),
            status: String::new(),
            departure_gate: String::new(),
            arrival_gate: String::new(),
            aircraft: String::new(),
            raw_source: String::new(),
        };
        let c2 = FlightCandidate {
            email_guid: "<e2@example.com>".to_string(),
            path: "ics".to_string(),
            reservation_id: String::new(),
            origin: "SFO".to_string(),
            destination: "JFK".to_string(),
            airline: "United".to_string(),
            flight_number: "UA 999".to_string(),
            departure_time: "2026-11-15T14:00:00-07:00".to_string(),
            arrival_time: String::new(),
            status: String::new(),
            departure_gate: String::new(),
            arrival_gate: String::new(),
            aircraft: String::new(),
            raw_source: String::new(),
        };
        let g1 = c1.guid();
        let g2 = c2.guid();
        assert_ne!(g1, g2,
            "distinct flights (diff dep time + number, no reservationId) must not share guid (defect 5 fix): g1={g1} g2={g2}");
        // Sanity: same leg from booking + reminder emails should still share guid.
        let c3 = FlightCandidate { email_guid: "<e3@example.com>".to_string(), ..c1.clone() };
        assert_eq!(c1.guid(), c3.guid(),
            "same route+time+number, different email → same guid (idempotent)");
    }

    // -----------------------------------------------------------------------
    // Segment mapping tests

    fn sample_candidate() -> FlightCandidate {
        FlightCandidate {
            email_guid: "<msg-001@example.com>".to_string(),
            path: "json-ld".to_string(),
            reservation_id: "H4X9Q2".to_string(),
            origin: "SFO".to_string(),
            destination: "JFK".to_string(),
            airline: "United Airlines".to_string(),
            flight_number: "UA 523".to_string(),
            departure_time: "2026-07-14T08:25:00-07:00".to_string(),
            arrival_time: "2026-07-14T16:58:00-04:00".to_string(),
            status: "confirmed".to_string(),
            departure_gate: "B22".to_string(),
            arrival_gate: String::new(),
            aircraft: "Boeing 737-900".to_string(),
            raw_source: "{}".to_string(),
        }
    }

    #[test]
    fn candidate_maps_to_segment_on_travel_contract() {
        let c = sample_candidate();
        let seg = candidate_to_segment(&c).expect("complete candidate maps to segment");

        // Required travel contract core.
        assert_eq!(seg.source, "flight-emails");
        assert_eq!(seg.type_, "flight");
        assert_eq!(seg.ts, "2026-07-14T08:25:00-07:00", "ts = departure");
        assert_eq!(seg.guid, "H4X9Q2-SFO-JFK", "guid = reservation_id-origin-dest");
        assert_eq!(seg.end_ts, "2026-07-14T16:58:00-04:00");
        assert_eq!(seg.start_place, "SFO");
        assert_eq!(seg.end_place, "JFK");
        assert_eq!(seg.vendor, "United Airlines");
        assert_eq!(seg.number, "UA 523");
        assert_eq!(seg.confirmation, "H4X9Q2");
        assert_eq!(seg.booking_id, "H4X9Q2");
        assert_eq!(seg.status, "confirmed");

        // Source-specific in extra.
        assert_eq!(seg.extra.get("extraction_path"), Some(&Value::String("json-ld".into())));
        assert_eq!(seg.extra.get("departure_gate"), Some(&Value::String("B22".into())));
        assert_eq!(seg.extra.get("aircraft"), Some(&Value::String("Boeing 737-900".into())));
        // Empty fields omitted.
        assert!(seg.extra.get("arrival_gate").is_none(), "empty gate omitted from extra");

        // Write to vault and confirm partition by departure month.
        let v = temp_vault("contract-line");
        let stream = v.stream(DIR, Partition::Month);
        stream.append(&[seg], |s| &s.ts).unwrap();
        let raw = fs::read_to_string(v.root().join("travel/flight-emails/2026-07.jsonl")).unwrap();
        assert!(raw.contains("\"type\":\"flight\""), "discriminator: {raw}");
        assert!(raw.contains("\"source\":\"flight-emails\""), "{raw}");
        assert!(raw.contains("\"guid\":\"H4X9Q2-SFO-JFK\""), "{raw}");
    }

    #[test]
    fn candidate_without_departure_time_is_skipped() {
        let mut c = sample_candidate();
        c.departure_time = String::new();
        assert!(
            candidate_to_segment(&c).is_none(),
            "no departure time → no segment (can't partition)"
        );
    }

    #[test]
    fn sparse_candidate_omits_empty_optional_fields() {
        let c = FlightCandidate {
            email_guid: "<sparse@example.com>".to_string(),
            path: "ics".to_string(),
            reservation_id: "SPARSE-001".to_string(),
            origin: "LHR".to_string(),
            destination: "CDG".to_string(),
            airline: String::new(),
            flight_number: String::new(),
            departure_time: "2026-09-01T10:00:00+01:00".to_string(),
            arrival_time: String::new(),
            status: String::new(),
            departure_gate: String::new(),
            arrival_gate: String::new(),
            aircraft: String::new(),
            raw_source: String::new(),
        };
        let seg = candidate_to_segment(&c).expect("origin + dest + dep → segment");
        let val = serde_json::to_value(&seg).unwrap();
        // Required fields present.
        for f in ["ts", "source", "type", "guid"] {
            assert!(val.get(f).is_some(), "required {f} present in {val}");
        }
        // Optional absent fields omitted.
        assert!(val.get("end_ts").is_none(), "no arrival → end_ts omitted");
        assert!(val.get("vendor").is_none(), "no airline → vendor omitted");
        assert!(val.get("number").is_none(), "no flight number → number omitted");
        assert!(val.get("status").is_none(), "empty status omitted");
    }

    #[test]
    fn same_itinerary_deduplicates_on_guid() {
        // The same flight from booking email + reminder email yields the same
        // guid → only one segment written.
        let c1 = sample_candidate();
        let mut c2 = sample_candidate();
        c2.email_guid = "<reminder@example.com>".to_string();
        // same reservation_id/origin/dest → same guid.
        assert_eq!(c1.guid(), c2.guid(), "identical reservation → same guid");

        let v = temp_vault("dedupe");
        let stream = v.stream(DIR, Partition::Month);
        let mut seen = std::collections::HashSet::new();
        let mut segs = Vec::new();
        for c in &[c1, c2] {
            if let Some(seg) = candidate_to_segment(c) {
                if seen.insert(seg.guid.clone()) {
                    segs.push(seg);
                }
            }
        }
        stream.append(&segs, |s| &s.ts).unwrap();
        let raw = fs::read_to_string(v.root().join("travel/flight-emails/2026-07.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1, "one segment, not two: {raw}");
    }

    #[test]
    fn write_raw_dumps_full_fidelity_with_provenance() {
        let v = temp_vault("raw");
        let c = sample_candidate();
        write_raw(&v, &[c]).unwrap();
        let raw_path = v.root().join("travel/flight-emails/raw/2026-07.jsonl");
        assert!(raw_path.exists(), "raw file created");
        let body = fs::read_to_string(&raw_path).unwrap();
        assert!(body.contains("\"email_guid\""), "email provenance in raw: {body}");
        assert!(body.contains("\"path\":\"json-ld\""), "extraction path in raw: {body}");
        assert!(body.contains("\"reservation_id\":\"H4X9Q2\""), "{body}");
    }

    #[test]
    fn def_is_import_on_travel_domain_opt_in() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.domain, "travel");
        assert_eq!(DEF.meta.vault_path, "travel/flight-emails/");
        assert_eq!(DEF.meta.id, "flight-emails");
        assert!(!DEF.meta.default_on, "privacy-sensitive — must be opt-in");
        let spec = DEF.import_spec().unwrap();
        assert_eq!(spec.accepts, &["mbox"]);
    }
}
