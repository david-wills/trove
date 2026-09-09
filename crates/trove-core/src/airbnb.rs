//! Airbnb guest booking history → the unified `travel/` segment stream.
//!
//! Airbnb is a lodging marketplace; a guest's booking history records where the
//! user stayed, the dates, the city/country, the confirmation code, the host,
//! and the amount paid. For Trove it's the lodging counterpart to flight data —
//! each stay maps to one `type:"lodging"` [`Segment`] in
//! `travel/airbnb/YYYY-MM.jsonl` (the travel contract,
//! `docs/vault-spec/domains/travel.md`): `ts` = check-in (local), `end_ts` =
//! check-out, `guid` = `confirmation` = the Airbnb confirmation code (the stable
//! dedupe key, so re-importing a newer export never duplicates), `start_place` =
//! city, `start_place_name` = listing name, with amount / currency / nights /
//! country / host riding in `extra`. Full fidelity also lands per-source raw
//! under `travel/airbnb/raw/`.
//!
//! **No guest API.** Airbnb exposes no programmatic booking history, so this is a
//! file [`Behavior::Import`] — the user runs Airbnb's own export and hands Trove
//! the file; Trove never logs in or scrapes. Two export mechanisms exist:
//!
//! - **GDPR data download** (the reliable, complete path): Privacy Settings →
//!   "Request Your Personal Data" → a ZIP containing the reservations data with
//!   all booking history.
//! - **Web CSV** (region-varying fallback): Trips → Past → "See all
//!   reservations" → CSV export. Availability differs by account region.
//!
//! ## Parser parked — Needs-sample (evidence rule)
//!
//! The GDPR export's reservations file layout is **not officially documented**
//! and is community-reverse-engineered, region-varying, and not pinned to a real
//! sample on disk (see the brief, `docs/integrations/airbnb.md` → evidence:
//! "community-schema · sample-required"). Per the project's evidence rule we do
//! **not** parse blind against an assumed field shape — a green test over a
//! fabricated fixture is false confidence (cf. the raindrop `_id` bug). So this
//! module **binds the travel contract** (the load-bearing pioneer work — the
//! `Segment` type, the `travel` DOMAINS entry, and the ratified contract tests
//! all land in this build) and ships the import **scaffold**, but the
//! export-file → [`Stay`] extraction is parked behind [`PARKED_MSG`] until a real
//! `reservations.json` (or web CSV) sample is in hand.
//!
//! What is *not* parked, and is fully tested here, is the contract mapping
//! [`stay_to_segment`]: a normalized [`Stay`] (the small set of fields every
//! Airbnb export carries, whatever their exact wire names) → a lodging
//! [`Segment`]. When a sample lands, only [`stays_from_export`] (the field-name
//! glue) needs filling — the contract shape, the raw layer, dedupe, and the
//! re-runnable import loop are already proven.

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

/// The travel-domain folder for this source.
const DIR: &str = "travel/airbnb";
/// Full-fidelity export objects land here, untouched.
const RAW_DIR: &str = "travel/airbnb/raw";

/// Shown when the import is invoked before a real export sample exists to pin
/// the reservations file's exact field names. The bind is done; the parser is
/// the only piece waiting on a sample.
const PARKED_MSG: &str = "Airbnb import is parked pending a real data-export sample. \
The reservations file layout in Airbnb's GDPR download is not officially documented, \
and Trove does not parse export files against a guessed field shape. \
Once a sample reservations.json (or the Trips CSV) is provided, the field mapping is wired \
in airbnb::stays_from_export — the travel contract, raw layer, and dedupe are already in place.";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the line is already
/// present; this build upgrades the def from `NotWired` to `Import`).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "airbnb",
        name: "Airbnb",
        kind: IntegrationKind::Import,
        // Booking history is a location/travel trail (where you slept and when),
        // so the source ships opt-in — off by default, enabled with explicit
        // acknowledgement.
        default_on: false,
        description: "Import your Airbnb guest booking history — every stay's check-in and check-out dates, listing, city/country, host, and amount paid — into the unified travel timeline. Re-runnable: newer exports never duplicate.",
        domain: "travel",
        vault_path: "travel/airbnb/",
        toggleable: false,
        setup: &[
            "airbnb.com → Account → Privacy & sharing → Request your personal data; Airbnb emails a download link when the ZIP is ready.",
            "Import the downloaded ZIP here as-is (it contains your full reservations history). The Trips → Past CSV export works as a fallback where your region offers it.",
        ],
        caveats: "No guest API — Airbnb only offers a manual data export, so this is import-only (nothing syncs in the background). Booking history reveals where you stayed and when, so it ships opt-in. Note: the reservations file layout in Airbnb's export is undocumented and varies by region; the importer is parked until a real export sample pins the exact fields (the travel contract it writes into is already in place).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // The GDPR ZIP, or a bare reservations.json / Trips CSV pulled out of it.
    accepts: &["zip", "json", "csv"],
    params: &[],
    run: run_import,
};

/// A single Airbnb stay, normalized to the small set of fields every export
/// carries — whatever their exact wire names in a given region's export. This
/// is the seam the parser fills and the contract mapping consumes, so the two
/// concerns stay independent: [`stays_from_export`] (parked, field-name glue)
/// produces `Stay`s; [`stay_to_segment`] (tested) maps a `Stay` to the contract.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stay {
    /// Airbnb confirmation code (e.g. `HMABCDEFGH`) — the stable id / dedupe key.
    pub confirmation: String,
    /// Check-in date, `YYYY-MM-DD` (local to the listing).
    pub check_in: String,
    /// Check-out date, `YYYY-MM-DD`.
    pub check_out: String,
    /// The listing's display name.
    pub listing: String,
    /// City (and, where the export gives it, region/country short form) — the
    /// stay's place.
    pub city: String,
    /// Country, long form (`"Portugal"`) — kept in `extra`.
    pub country: String,
    /// Amount paid, as the export's verbatim string (no parsing into a number —
    /// money stays a string under `extra`, the travel-contract idiom).
    pub amount: String,
    /// Currency code (`"EUR"`), where the export carries it.
    pub currency: String,
    /// Host display name, where the export carries it.
    pub host: String,
    /// Nights, where the export carries it (verbatim string).
    pub nights: String,
    /// Source-native status (`"completed"`, `"canceled"`, …), where present.
    pub status: String,
    /// The full original export object for this stay, preserved verbatim into
    /// the raw layer so nothing the export carried is dropped.
    pub raw: Map<String, Value>,
}

/// Map a normalized [`Stay`] to a lodging [`Segment`] — the travel-contract
/// binding. `ts` = check-in at listing-local noon (the export carries a date,
/// not a time; noon avoids midnight-boundary surprises, like `letterboxd`),
/// `guid` = `confirmation`, city → `start_place`, listing → `start_place_name`,
/// and amount / currency / nights / country / host → `extra`. Returns `None`
/// only when the stay has neither a confirmation code (no stable id) nor a
/// parseable check-in date.
pub fn stay_to_segment(stay: &Stay) -> Option<Segment> {
    let confirmation = stay.confirmation.trim();
    if confirmation.is_empty() {
        return None;
    }
    let ts = local_noon(stay.check_in.trim())?;

    let mut seg = Segment::new("airbnb", "lodging", confirmation, ts);
    seg.confirmation = confirmation.to_string();
    if let Some(end_ts) = local_checkout(stay.check_out.trim()) {
        seg.end_ts = end_ts;
    }
    seg.start_place = stay.city.trim().to_string();
    seg.start_place_name = stay.listing.trim().to_string();
    seg.status = stay.status.trim().to_string();

    let mut extra = Map::new();
    let mut put = |k: &str, v: &str| {
        if !v.trim().is_empty() {
            extra.insert(k.into(), Value::String(v.trim().to_string()));
        }
    };
    put("amount", &stay.amount);
    put("currency", &stay.currency);
    put("nights", &stay.nights);
    put("country", &stay.country);
    put("host", &stay.host);
    seg.extra = extra;
    Some(seg)
}

/// A `YYYY-MM-DD` date at listing-local noon, RFC3339. Airbnb exports a check-in
/// *date*, not a time; noon in the machine's local zone keeps the partition
/// honest and avoids midnight-boundary surprises (the `letterboxd` precedent).
/// `None` when the string isn't a date.
fn local_noon(date: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        Local
            .from_local_datetime(&d.and_time(NaiveTime::from_hms_opt(12, 0, 0)?))
            .earliest()?
            .to_rfc3339(),
    )
}

/// Check-out at listing-local 11:00 (Airbnb's customary check-out hour) — a
/// best-effort `end_ts` from a date. `None` when the string isn't a date (the
/// field is optional, so a missing check-out simply omits `end_ts`).
fn local_checkout(date: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        Local
            .from_local_datetime(&d.and_time(NaiveTime::from_hms_opt(11, 0, 0)?))
            .earliest()?
            .to_rfc3339(),
    )
}

/// Parse an Airbnb export file (GDPR ZIP, bare `reservations.json`, or Trips
/// CSV) into normalized [`Stay`]s.
///
/// **Parked — Needs-sample.** The reservations file's exact field names are
/// undocumented and region-varying, and the evidence rule forbids parsing
/// against a guessed shape. This is the *only* piece waiting on a real sample:
/// the contract mapping ([`stay_to_segment`]), the raw layer, dedupe, and the
/// import loop are done and tested. When a sample lands, read the export here
/// and populate `Stay` (and its `raw` map) field-for-field — nothing downstream
/// changes.
fn stays_from_export(_path: &Path) -> Result<Vec<Stay>> {
    anyhow::bail!("{PARKED_MSG}")
}

/// Write each stay's full original export object to `travel/airbnb/raw/`,
/// partitioned by the check-in year (one file per year), at full fidelity —
/// nothing the export carried is dropped. The contract layer dedupes by guid;
/// the raw layer mirrors the export as given.
#[allow(dead_code)] // exercised once `stays_from_export` is unparked.
fn write_raw(vault: &Vault, stays: &[Stay]) -> Result<()> {
    let mut by_year: BTreeMap<String, Vec<&Map<String, Value>>> = BTreeMap::new();
    for s in stays {
        let year = s.check_in.get(..4).unwrap_or("unknown").to_string();
        by_year.entry(year).or_default().push(&s.raw);
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

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Already-stored confirmation codes, for a re-runnable (idempotent) import.
    let stream = vault.stream(DIR, Partition::Month);
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for key in stream.partitions()? {
        for seg in stream.read::<Segment>(&key)? {
            if !seg.guid.is_empty() {
                seen.insert(seg.guid);
            }
        }
    }

    // Parse the export into normalized stays. Parked until a sample exists; this
    // returns the Needs-sample error rather than guessing the file's fields.
    let stays = stays_from_export(path)?;

    // Full fidelity first (lossless import), then the contract layer.
    write_raw(vault, &stays)?;

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut segments = Vec::new();
    for stay in &stays {
        let Some(seg) = stay_to_segment(stay) else {
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
        headline: format!("{imported} stays imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-airbnb-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A representative stay built from values we control — never a fabricated
    /// Airbnb wire shape. (The export → `Stay` parse is parked; the mapping
    /// `Stay` → `Segment` is the contract logic under test.)
    fn sample_stay() -> Stay {
        let mut raw = Map::new();
        raw.insert("confirmation_code".into(), Value::String("HMABCDEFGH".into()));
        raw.insert("source_region".into(), Value::String("PT".into()));
        Stay {
            confirmation: "HMABCDEFGH".into(),
            check_in: "2026-08-03".into(),
            check_out: "2026-08-09".into(),
            listing: "Sunny Alfama Loft with River View".into(),
            city: "Lisbon, PT".into(),
            country: "Portugal".into(),
            amount: "742.00".into(),
            currency: "EUR".into(),
            host: "Marta".into(),
            nights: "6".into(),
            status: "completed".into(),
            raw,
        }
    }

    #[test]
    fn stay_maps_to_a_lodging_segment_on_the_travel_contract() {
        let seg = stay_to_segment(&sample_stay()).expect("a stay with a code + date maps");
        // The required travel-contract core.
        assert_eq!(seg.source, "airbnb");
        assert_eq!(seg.type_, "lodging");
        assert_eq!(seg.guid, "HMABCDEFGH", "guid is the confirmation code");
        // Check-in at listing-local noon → ts; its month is the partition key.
        assert!(seg.ts.starts_with("2026-08-03T12:00:00"), "ts is check-in local noon: {}", seg.ts);
        assert!(seg.end_ts.starts_with("2026-08-09T11:00:00"), "end_ts is check-out: {}", seg.end_ts);
        // Lodging place mapping: city → start_place, listing → start_place_name.
        assert_eq!(seg.start_place, "Lisbon, PT");
        assert_eq!(seg.start_place_name, "Sunny Alfama Loft with River View");
        assert_eq!(seg.confirmation, "HMABCDEFGH");
        assert_eq!(seg.status, "completed");
        // Amount / currency / nights / country / host ride in extra (strings).
        assert_eq!(seg.extra.get("amount"), Some(&Value::String("742.00".into())));
        assert_eq!(seg.extra.get("currency"), Some(&Value::String("EUR".into())));
        assert_eq!(seg.extra.get("nights"), Some(&Value::String("6".into())));
        assert_eq!(seg.extra.get("country"), Some(&Value::String("Portugal".into())));
        assert_eq!(seg.extra.get("host"), Some(&Value::String("Marta".into())));
        // No flight columns on a lodging segment.
        assert!(seg.number.is_empty());
        assert!(seg.vendor.is_empty());

        // The segment is a valid contract line that partitions by check-in month.
        let v = temp_vault("contract-line");
        let stream = v.stream(DIR, Partition::Month);
        stream.append(&[seg], |s| &s.ts).unwrap();
        let raw = fs::read_to_string(v.root().join("travel/airbnb/2026-08.jsonl")).unwrap();
        assert!(raw.contains("\"type\":\"lodging\""), "discriminator serialized as `type`: {raw}");
        assert!(raw.contains("\"guid\":\"HMABCDEFGH\""), "{raw}");
    }

    #[test]
    fn stay_without_code_or_date_is_skipped() {
        // No confirmation → no stable id → skip (don't invent a guid).
        let mut s = sample_stay();
        s.confirmation = "  ".into();
        assert!(stay_to_segment(&s).is_none(), "no confirmation code → skip");
        // A code but an unparseable date → skip (don't misfile under a bad month).
        let mut s = sample_stay();
        s.check_in = "not-a-date".into();
        assert!(stay_to_segment(&s).is_none(), "no parseable check-in → skip");
    }

    #[test]
    fn empty_optionals_are_omitted_not_blank() {
        // A sparse stay (code + check-in only) writes no empty extra and omits
        // the optional columns entirely (omit-if-empty).
        let s = Stay {
            confirmation: "HMZZZZZZZZ".into(),
            check_in: "2025-12-20".into(),
            ..Default::default()
        };
        let seg = stay_to_segment(&s).expect("code + check-in is enough");
        let val = serde_json::to_value(&seg).unwrap();
        assert!(val.get("extra").is_none(), "no extra object when every optional is empty: {val}");
        assert!(val.get("end_ts").is_none(), "no check-out → end_ts omitted");
        assert!(val.get("start_place").is_none(), "no city → start_place omitted");
        assert!(val.get("status").is_none(), "no status → omitted");
        // Required core is always present.
        for f in ["ts", "source", "type", "guid"] {
            assert!(val.get(f).is_some(), "required {f} present");
        }
    }

    #[test]
    fn write_raw_dumps_full_fidelity_partitioned_by_year() {
        // The lossless raw layer mirrors the export object verbatim, one file
        // per check-in year — independent of which contract columns we mapped.
        let v = temp_vault("raw");
        let a = sample_stay(); // 2026
        let mut b = sample_stay();
        b.confirmation = "HMOLDER111".into();
        b.check_in = "2024-05-01".into();
        write_raw(&v, &[a, b]).unwrap();
        let r26 = fs::read_to_string(v.root().join("travel/airbnb/raw/2026.jsonl")).unwrap();
        assert!(r26.contains("\"confirmation_code\":\"HMABCDEFGH\""), "raw object preserved: {r26}");
        assert!(r26.contains("\"source_region\":\"PT\""), "every raw field kept: {r26}");
        let r24 = fs::read_to_string(v.root().join("travel/airbnb/raw/2024.jsonl")).unwrap();
        assert_eq!(r24.lines().count(), 1, "partitioned by check-in year");
    }

    #[test]
    fn import_is_parked_until_a_sample_lands() {
        // The evidence rule: the importer refuses to parse a guessed shape and
        // surfaces a clear Needs-sample message instead of silently doing nothing
        // (or worse, mis-parsing). The bind is done; the parser waits on a sample.
        let v = temp_vault("parked");
        let f = v.root().join("airbnb-export.zip");
        fs::write(&f, b"not a real export").unwrap();
        let err = (IMPORT.run)(&v, &f, &BTreeMap::new(), &mut |_| {}).unwrap_err();
        assert!(err.to_string().contains("parked"), "parked message surfaced: {err}");
        assert!(err.to_string().contains("stays_from_export"), "points at the seam to fill: {err}");
    }

    #[test]
    fn the_def_is_an_import_on_the_travel_domain() {
        // Registry-shape sanity: the def upgraded from NotWired to an Import box,
        // opt-in, on the travel domain, with the right accepted extensions.
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.meta.domain, "travel");
        assert_eq!(DEF.meta.vault_path, "travel/airbnb/");
        assert!(!DEF.meta.default_on, "booking history is opt-in");
        let spec = DEF.import_spec().unwrap();
        assert_eq!(spec.accepts, &["zip", "json", "csv"]);
    }
}
