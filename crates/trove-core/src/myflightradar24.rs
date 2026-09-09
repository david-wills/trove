//! myFlightRadar24 — CSV flight-log importer.
//!
//! myFlightRadar24 is Flightradar24's personal flight-logbook feature. Users
//! record their flights and export the log as a CSV from:
//!   `my.flightradar24.com → Settings → Export`
//!
//! No API, no login inside Trove — the user fetches the CSV out-of-band and
//! drops it into the generic import box.
//!
//! ## CSV format (confirmed from `imikailoby/fr24-csv-parser` constants)
//!
//! Columns (in order): `Date, Flight number, From, To, Dep time, Arr time,
//! Duration, Airline, Aircraft, Registration, Seat number, Seat type, Flight
//! class, Flight reason, Note, Dep_id, Arr_id, Airline_id, Aircraft_id`
//!
//! Date format: `YYYY-MM-DD`. All columns after `To` are optional (may be
//! empty). The three mandatory columns every row carries are `Date`, `From`,
//! and `To`.
//!
//! ## Two layers — unconditional
//!
//! - **Raw** — every CSV row verbatim as a JSON object at
//!   `travel/myflightradar24/raw/YYYY.jsonl`, one file per flight year.
//! - **Contract** — each row mapped to a [`Segment`] (`type:"flight"`) at
//!   `travel/myflightradar24/YYYY-MM.jsonl` (the unified travel domain,
//!   [`docs/vault-spec/domains/travel.md`]). Partitioned by the local month
//!   of the flight date.
//!
//! ## Dedupe
//!
//! No native row id; `guid` = SHA-256(date | from | to | flight_number) so
//! re-importing an overlapping export or a row already present from Flighty
//! never duplicates. The same four-field hash is what the Flighty brief uses
//! as the shared anchor for cross-source dedup at read time.
//!
//! See `docs/integrations/myflightradar24.md` for the brief.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveTime, TimeZone};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::{write_atomic, Partition};
use crate::travel::Segment;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Constants

/// Contract-layer travel segment stream for this source.
const DIR: &str = "travel/myflightradar24";

/// Full-fidelity raw rows, one file per flight year.
const RAW_DIR: &str = "travel/myflightradar24/raw";

// ---------------------------------------------------------------------------
// CSV column names — verbatim from the confirmed myFlightRadar24 export format.
// Source: `imikailoby/fr24-csv-parser` src/constants/csv.ts `EXPECTED_CSV_COLUMNS`.

const COL_DATE: &str = "Date";
const COL_FLIGHT_NUMBER: &str = "Flight number";
const COL_FROM: &str = "From";
const COL_TO: &str = "To";
const COL_DEP_TIME: &str = "Dep time";
const COL_ARR_TIME: &str = "Arr time";
const COL_DURATION: &str = "Duration";
const COL_AIRLINE: &str = "Airline";
const COL_AIRCRAFT: &str = "Aircraft";
const COL_REGISTRATION: &str = "Registration";
const COL_SEAT_NUMBER: &str = "Seat number";
const COL_SEAT_TYPE: &str = "Seat type";
const COL_FLIGHT_CLASS: &str = "Flight class";
const COL_FLIGHT_REASON: &str = "Flight reason";
const COL_NOTE: &str = "Note";
const COL_DEP_ID: &str = "Dep_id";
const COL_ARR_ID: &str = "Arr_id";
// Airline_id and Aircraft_id are stored in raw only (internal IDs, not user-facing).

// ---------------------------------------------------------------------------
// Stable guid computation

/// SHA-256(date | from | to | flight_number) — the stable dedupe key.
///
/// Uses a fixed separator (`\x1f`, ASCII Unit Separator) between fields so
/// the hash is unambiguous even when a field is empty or contains the
/// separator character. Encoded as 64 lowercase hex chars.
fn flight_guid(date: &str, from: &str, to: &str, flight_number: &str) -> String {
    let mut h = Sha256::new();
    h.update(date.trim().as_bytes());
    h.update(b"\x1f");
    h.update(from.trim().as_bytes());
    h.update(b"\x1f");
    h.update(to.trim().as_bytes());
    h.update(b"\x1f");
    h.update(flight_number.trim().as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// A normalized flight row — the seam between the CSV parser and the contract
// mapping. `fields` holds the full header→value map for the raw layer.

/// One flight row, normalized from the myFlightRadar24 CSV columns.
#[derive(Debug, Clone, Default)]
pub struct FlightRecord {
    /// `Date` column — `YYYY-MM-DD`.
    pub date: String,
    /// `Flight number` column — e.g. `"UA 523"`.
    pub flight_number: String,
    /// `From` column — IATA or ICAO airport code of the origin.
    pub from: String,
    /// `To` column — IATA or ICAO airport code of the destination.
    pub to: String,
    /// `Dep time` column — local departure time `HH:MM` (optional).
    pub dep_time: String,
    /// `Arr time` column — local arrival time `HH:MM` (optional).
    pub arr_time: String,
    /// `Duration` column — e.g. `"02:35"` or `"2:35"` (optional).
    pub duration: String,
    /// `Airline` column — carrier name (optional).
    pub airline: String,
    /// `Aircraft` column — aircraft type (optional), e.g. `"Boeing 737-900"`.
    pub aircraft: String,
    /// `Registration` column — tail number (optional).
    pub registration: String,
    /// `Seat number` column (optional).
    pub seat_number: String,
    /// `Seat type` column (optional).
    pub seat_type: String,
    /// `Flight class` column (optional).
    pub flight_class: String,
    /// `Flight reason` column (optional).
    pub flight_reason: String,
    /// `Note` column (optional).
    pub note: String,
    /// `Dep_id` — internal departure airport id (optional).
    pub dep_id: String,
    /// `Arr_id` — internal arrival airport id (optional).
    pub arr_id: String,
    /// Full verbatim CSV row as a JSON map — written to the raw layer unchanged.
    pub raw: Map<String, Value>,
}

// ---------------------------------------------------------------------------
// CSV → FlightRecord

/// Parse a myFlightRadar24 CSV export (as a string slice) into flight records.
///
/// Accepts files with any subset of the 19 known columns; missing columns are
/// treated as empty. Rows without a `Date`, `From`, or `To` value are skipped
/// and counted as `skipped_rows` in the returned pair.
///
/// Returns `(records, skipped_rows)`.
pub fn records_from_csv(body: &str) -> Result<(Vec<FlightRecord>, u64)> {
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());

    let headers = rdr
        .headers()
        .context("reading CSV header row — is this a myFlightRadar24 CSV export?")?
        .clone();

    // Build an index: column_name → position in each row.
    let col_idx = |name: &str| -> Option<usize> { headers.iter().position(|h| h == name) };

    let idx_date = col_idx(COL_DATE);
    let idx_flight_number = col_idx(COL_FLIGHT_NUMBER);
    let idx_from = col_idx(COL_FROM);
    let idx_to = col_idx(COL_TO);
    let idx_dep_time = col_idx(COL_DEP_TIME);
    let idx_arr_time = col_idx(COL_ARR_TIME);
    let idx_duration = col_idx(COL_DURATION);
    let idx_airline = col_idx(COL_AIRLINE);
    let idx_aircraft = col_idx(COL_AIRCRAFT);
    let idx_registration = col_idx(COL_REGISTRATION);
    let idx_seat_number = col_idx(COL_SEAT_NUMBER);
    let idx_seat_type = col_idx(COL_SEAT_TYPE);
    let idx_flight_class = col_idx(COL_FLIGHT_CLASS);
    let idx_flight_reason = col_idx(COL_FLIGHT_REASON);
    let idx_note = col_idx(COL_NOTE);
    let idx_dep_id = col_idx(COL_DEP_ID);
    let idx_arr_id = col_idx(COL_ARR_ID);

    let get = |rec: &csv::StringRecord, idx: Option<usize>| -> String {
        idx.and_then(|i| rec.get(i))
            .unwrap_or("")
            .trim()
            .to_string()
    };

    let mut records: Vec<FlightRecord> = Vec::new();
    let mut skipped: u64 = 0;

    for result in rdr.records() {
        let rec = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let date = get(&rec, idx_date);
        let from = get(&rec, idx_from);
        let to = get(&rec, idx_to);

        // The three mandatory fields — skip rows that lack any of them.
        if date.is_empty() || from.is_empty() || to.is_empty() {
            skipped += 1;
            continue;
        }

        // Build the verbatim header→value map for the raw layer.
        let raw: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.trim().to_string())))
            .collect();

        records.push(FlightRecord {
            date,
            flight_number: get(&rec, idx_flight_number),
            from,
            to,
            dep_time: get(&rec, idx_dep_time),
            arr_time: get(&rec, idx_arr_time),
            duration: get(&rec, idx_duration),
            airline: get(&rec, idx_airline),
            aircraft: get(&rec, idx_aircraft),
            registration: get(&rec, idx_registration),
            seat_number: get(&rec, idx_seat_number),
            seat_type: get(&rec, idx_seat_type),
            flight_class: get(&rec, idx_flight_class),
            flight_reason: get(&rec, idx_flight_reason),
            note: get(&rec, idx_note),
            dep_id: get(&rec, idx_dep_id),
            arr_id: get(&rec, idx_arr_id),
            raw,
        });
    }

    Ok((records, skipped))
}

// ---------------------------------------------------------------------------
// FlightRecord → Segment (the travel contract binding)

/// Map a [`FlightRecord`] to a flight [`Segment`] on the unified travel contract.
///
/// - `ts` = flight date at local noon (the CSV carries a date, not a time, for
///   all rows; departure time, when present, is stored in `extra`). Noon in the
///   machine's local zone avoids midnight-boundary partition surprises (the
///   letterboxd / airbnb precedent).
/// - `guid` = SHA-256(date | from | to | flight_number) — stable across
///   re-imports and compatible with the Flighty dedup anchor.
/// - `start_place` = `From`, `end_place` = `To`, `vendor` = airline,
///   `number` = flight number.
/// - Source-specific fields (aircraft, registration, seat, class, reason, note,
///   dep_id, arr_id, duration, arr_time) → `extra`, all as strings.
///
/// Returns `None` when the flight date is not parseable (the row is skipped).
pub fn record_to_segment(r: &FlightRecord) -> Option<Segment> {
    let ts = local_noon(r.date.trim())?;
    let guid = flight_guid(&r.date, &r.from, &r.to, &r.flight_number);

    let mut seg = Segment::new("myflightradar24", "flight", guid, ts);
    seg.start_place = r.from.trim().to_string();
    seg.end_place = r.to.trim().to_string();
    seg.vendor = r.airline.trim().to_string();
    seg.number = r.flight_number.trim().to_string();

    // Extra: source-specific fields not in the shared columns.
    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().to_string()));
        }
    };
    put("aircraft", &r.aircraft);
    put("registration", &r.registration);
    put("seat_number", &r.seat_number);
    put("seat_type", &r.seat_type);
    put("flight_class", &r.flight_class);
    put("flight_reason", &r.flight_reason);
    put("note", &r.note);
    put("dep_time", &r.dep_time);
    put("arr_time", &r.arr_time);
    put("duration", &r.duration);
    put("dep_id", &r.dep_id);
    put("arr_id", &r.arr_id);
    seg.extra = extra;

    Some(seg)
}

/// A `YYYY-MM-DD` date at local noon, RFC3339. The CSV carries a flight date,
/// not a departure time; noon avoids midnight-boundary partition surprises.
/// Returns `None` when the string is not a valid date.
fn local_noon(date: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        Local
            .from_local_datetime(&d.and_time(NaiveTime::from_hms_opt(12, 0, 0)?))
            .earliest()?
            .to_rfc3339(),
    )
}

// ---------------------------------------------------------------------------
// Raw writer

/// Write each flight's full CSV row object to `travel/myflightradar24/raw/`,
/// partitioned by the flight year — full fidelity, nothing dropped.
fn write_raw(vault: &Vault, records: &[FlightRecord]) -> Result<()> {
    let mut by_year: BTreeMap<String, Vec<&Map<String, Value>>> = BTreeMap::new();
    for r in records {
        let year = r.date.get(..4).unwrap_or("unknown").to_string();
        by_year.entry(year).or_default().push(&r.raw);
    }
    for (year, objs) in by_year {
        let mut body = String::new();
        for o in objs {
            body.push_str(&serde_json::to_string(o)?);
            body.push('\n');
        }
        let rel = format!("{RAW_DIR}/{year}.jsonl");
        write_atomic(&vault.resolve(&rel)?, body.as_bytes())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The import

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;
    import_body(vault, &body, progress)
}

/// The import body over an in-memory CSV string — the testable seam.
pub fn import_body(
    vault: &Vault,
    body: &str,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Already-stored guids, for a re-runnable (idempotent) import.
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for seg in stream.read::<Segment>(&key)? {
            if !seg.guid.is_empty() {
                seen.insert(seg.guid);
            }
        }
    }

    let (records, parse_skipped) = records_from_csv(body)?;

    // Raw layer first (unconditional full fidelity).
    write_raw(vault, &records)?;

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, parse_skipped);
    let mut segments: Vec<Segment> = Vec::new();

    for r in &records {
        let Some(seg) = record_to_segment(r) else {
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
        headline: format!("{imported} flights imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// last_data hook

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

// ---------------------------------------------------------------------------
// IntegrationDef

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub line is
/// already present — this build replaces the `NotWired` body).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "myflightradar24",
        name: "myFlightRadar24",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your lifetime flight history from a myFlightRadar24 CSV export — \
                      flight date, origin and destination airports, airline, flight number, \
                      aircraft type, seat, and class — into the unified travel timeline. \
                      Re-runnable: newer exports never duplicate.",
        domain: "travel",
        vault_path: "travel/myflightradar24/",
        toggleable: false,
        setup: &[
            "Log in to my.flightradar24.com → Settings → Export → Download CSV.",
            "Drop the downloaded CSV into the import box here. Free accounts support the export.",
        ],
        caveats: "No API — myFlightRadar24 only offers a manual CSV export, so this is import-only. \
                  Re-import newer exports at any time; the stable date+route+flight-number hash \
                  prevents duplicates across re-imports and deduplicates against Flighty data \
                  at read time.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-mfr24-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A fully-populated CSV matching the confirmed myFlightRadar24 column set.
    const FULL_CSV: &str = "\
Date,Flight number,From,To,Dep time,Arr time,Duration,Airline,Aircraft,Registration,\
Seat number,Seat type,Flight class,Flight reason,Note,Dep_id,Arr_id,Airline_id,Aircraft_id
2024-03-15,UA 523,SFO,JFK,08:25,16:58,05:33,United Airlines,Boeing 737-900,N12345,\
14C,Window,Economy,Leisure,,1234,5678,UA,B739
2025-11-02,BA 112,LHR,JFK,11:25,14:10,07:45,British Airways,Boeing 777-300ER,G-STBA,\
2A,Window,Business,Business,,9012,5678,BA,B77W
";

    /// A sparse CSV with only the three mandatory columns.
    const SPARSE_CSV: &str = "\
Date,From,To
2023-06-21,CDG,LAX
2022-12-25,LAX,SYD
";

    #[test]
    fn full_csv_parses_to_two_records() {
        let (recs, skipped) = records_from_csv(FULL_CSV).expect("parses");
        assert_eq!(recs.len(), 2, "two data rows");
        assert_eq!(skipped, 0);

        let r0 = &recs[0];
        assert_eq!(r0.date, "2024-03-15");
        assert_eq!(r0.from, "SFO");
        assert_eq!(r0.to, "JFK");
        assert_eq!(r0.flight_number, "UA 523");
        assert_eq!(r0.airline, "United Airlines");
        assert_eq!(r0.aircraft, "Boeing 737-900");
        assert_eq!(r0.seat_number, "14C");
        assert_eq!(r0.flight_class, "Economy");
        assert_eq!(r0.dep_time, "08:25");
        assert_eq!(r0.arr_time, "16:58");
        assert_eq!(r0.duration, "05:33");
        // Raw map has all 19 columns.
        assert_eq!(r0.raw.get("Flight number"), Some(&Value::String("UA 523".into())));
        assert_eq!(r0.raw.get("Aircraft_id"), Some(&Value::String("B739".into())));
    }

    #[test]
    fn sparse_csv_parses_mandatory_only() {
        let (recs, skipped) = records_from_csv(SPARSE_CSV).expect("parses");
        assert_eq!(recs.len(), 2, "two rows");
        assert_eq!(skipped, 0);
        let r = &recs[0];
        assert_eq!(r.date, "2023-06-21");
        assert_eq!(r.from, "CDG");
        assert_eq!(r.to, "LAX");
        assert!(r.airline.is_empty());
        assert!(r.flight_number.is_empty());
    }

    #[test]
    fn row_missing_mandatory_field_is_skipped() {
        let csv = "Date,From,To\n2024-01-10,,JFK\n2024-02-15,SFO,\n2024-03-20,SFO,JFK\n";
        let (recs, skipped) = records_from_csv(csv).expect("parses");
        assert_eq!(recs.len(), 1, "only the complete row");
        assert_eq!(skipped, 2, "two rows missing From or To");
    }

    #[test]
    fn guid_is_stable_and_deterministic() {
        // Same four fields → same guid, independent of call order.
        let g1 = flight_guid("2024-03-15", "SFO", "JFK", "UA 523");
        let g2 = flight_guid("2024-03-15", "SFO", "JFK", "UA 523");
        assert_eq!(g1, g2, "deterministic");
        assert_eq!(g1.len(), 64, "sha256 hex = 64 chars");

        // Different flight number → different guid (no collision when only one field differs).
        let g3 = flight_guid("2024-03-15", "SFO", "JFK", "UA 524");
        assert_ne!(g1, g3, "different flight_number → different guid");

        // Empty flight_number still yields a stable guid (mandatory-only rows).
        let g4 = flight_guid("2023-06-21", "CDG", "LAX", "");
        let g5 = flight_guid("2023-06-21", "CDG", "LAX", "");
        assert_eq!(g4, g5, "empty flight_number → stable guid");
    }

    #[test]
    fn record_maps_to_travel_segment_on_contract() {
        let (recs, _) = records_from_csv(FULL_CSV).expect("parses");
        let seg = record_to_segment(&recs[0]).expect("maps");

        // Required travel-contract core.
        assert_eq!(seg.source, "myflightradar24");
        assert_eq!(seg.type_, "flight");
        assert_eq!(seg.guid.len(), 64, "sha256 hex guid");
        // ts is the flight date at local noon.
        assert!(
            seg.ts.starts_with("2024-03-15T12:00:00"),
            "ts = date at noon: {}",
            seg.ts
        );
        // Place mapping.
        assert_eq!(seg.start_place, "SFO");
        assert_eq!(seg.end_place, "JFK");
        assert_eq!(seg.vendor, "United Airlines");
        assert_eq!(seg.number, "UA 523");
        // Source-specific detail in extra.
        assert_eq!(
            seg.extra.get("aircraft"),
            Some(&Value::String("Boeing 737-900".into()))
        );
        assert_eq!(
            seg.extra.get("seat_number"),
            Some(&Value::String("14C".into()))
        );
        assert_eq!(
            seg.extra.get("flight_class"),
            Some(&Value::String("Economy".into()))
        );
        assert_eq!(
            seg.extra.get("dep_time"),
            Some(&Value::String("08:25".into()))
        );
        assert_eq!(
            seg.extra.get("duration"),
            Some(&Value::String("05:33".into()))
        );

        // Partitions correctly to YYYY-MM.jsonl.
        let v = temp_vault("contract-line");
        let stream = v.stream(DIR, Partition::Month);
        stream.append(&[seg.clone()], |s| &s.ts).unwrap();
        let path = v.root().join("travel/myflightradar24/2024-03.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"type\":\"flight\""), "discriminator: {raw}");
        assert!(raw.contains("\"source\":\"myflightradar24\""), "{raw}");
        assert!(raw.contains("\"start_place\":\"SFO\""), "{raw}");
    }

    #[test]
    fn sparse_record_omits_empty_fields_on_segment() {
        // A mandatory-only row serializes to exactly the four required contract
        // fields + the two airport codes (start_place/end_place always set).
        let (recs, _) = records_from_csv(SPARSE_CSV).expect("parses");
        let seg = record_to_segment(&recs[0]).expect("maps");
        let val = serde_json::to_value(&seg).unwrap();
        for f in ["ts", "source", "type", "guid"] {
            assert!(val.get(f).is_some(), "required {f} present");
        }
        assert!(val.get("extra").is_none(), "no extra when no optional fields: {val}");
        assert!(val.get("end_ts").is_none(), "no arrival time → end_ts omitted");
        assert!(val.get("vendor").is_none(), "no airline → vendor omitted");
        assert!(val.get("number").is_none(), "no flight number → number omitted");
        assert_eq!(val["start_place"], "CDG");
        assert_eq!(val["end_place"], "LAX");
    }

    #[test]
    fn reimport_is_idempotent() {
        // Importing the same CSV twice must produce zero duplicates on the
        // second pass.
        let v = temp_vault("idempotent");
        let mut noop = |_| {};
        let r1 = import_body(&v, FULL_CSV, &mut noop).unwrap();
        assert_eq!(r1.counts["imported"], 2);
        assert_eq!(r1.counts["duplicates"], 0);

        let r2 = import_body(&v, FULL_CSV, &mut noop).unwrap();
        assert_eq!(r2.counts["imported"], 0, "second pass: no new rows");
        assert_eq!(r2.counts["duplicates"], 2, "second pass: both seen before");
    }

    #[test]
    fn raw_layer_is_written_full_fidelity_by_year() {
        let v = temp_vault("raw");
        let mut noop = |_| {};
        import_body(&v, FULL_CSV, &mut noop).unwrap();

        // 2024 raw file.
        let r24 =
            fs::read_to_string(v.root().join("travel/myflightradar24/raw/2024.jsonl")).unwrap();
        assert!(r24.contains("\"Flight number\":\"UA 523\""), "raw row preserved: {r24}");
        assert!(r24.contains("\"Aircraft_id\":\"B739\""), "all cols in raw: {r24}");

        // 2025 raw file.
        let r25 =
            fs::read_to_string(v.root().join("travel/myflightradar24/raw/2025.jsonl")).unwrap();
        assert!(r25.contains("\"Flight number\":\"BA 112\""), "{r25}");
        assert_eq!(r25.lines().count(), 1, "one row in 2025");
    }

    #[test]
    fn overlapping_export_appends_only_new_rows() {
        // First import: two flights.
        let v = temp_vault("overlap");
        let mut noop = |_| {};
        import_body(&v, FULL_CSV, &mut noop).unwrap();

        // Second import: same two flights plus one new one.
        let extended_csv = format!(
            "{FULL_CSV}2026-07-04,DL 404,ATL,LAX,07:00,09:30,04:30,Delta Air Lines,,,,,,,,,,,"
        );
        let r2 = import_body(&v, &extended_csv, &mut noop).unwrap();
        assert_eq!(r2.counts["imported"], 1, "only the new flight imported");
        assert_eq!(r2.counts["duplicates"], 2, "two already-seen guids");
    }

    #[test]
    fn unparseable_date_row_is_skipped_on_segment_mapping() {
        // record_to_segment returns None for an invalid date (not a panic).
        let r = FlightRecord {
            date: "not-a-date".to_string(),
            from: "SFO".to_string(),
            to: "JFK".to_string(),
            ..Default::default()
        };
        assert!(record_to_segment(&r).is_none(), "bad date → None");
    }

    #[test]
    fn def_is_an_import_on_the_travel_domain() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.domain, "travel");
        assert_eq!(DEF.meta.vault_path, "travel/myflightradar24/");
        assert_eq!(DEF.meta.id, "myflightradar24");
        assert!(!DEF.meta.default_on, "flight history is opt-in");
        let spec = DEF.import_spec().unwrap();
        assert_eq!(spec.accepts, &["csv"]);
    }
}
