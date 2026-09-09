//! Google Timeline / Location History — the continuous GPS trail Google Maps
//! records, imported into the bound [`crate::location`] contract. Catalogued in
//! the Phase 2 pass; brief: docs/integrations/google-timeline.md. **First
//! collector in the `location` domain** — this build binds the contract (see
//! [`crate::location`] / [`crate::contracts`]).
//!
//! An **Import** (no API, no network, no connection): the user exports their
//! history from the Google Maps app and drops the file into the generic import
//! box (`letterboxd.rs` is the reference shape — one module, an [`ImportSpec`],
//! and a `DEF`, with no Tauri command or hub wiring). Three documented export
//! shapes must all be accepted so users migrating off old Takeout aren't
//! stranded:
//!
//! - **On-device** (2024–2025+, the only live path): a `Timeline.json` /
//!   `location-history.json` with top-level `semanticSegments`, `rawSignals`,
//!   `userLocationProfile`. Coordinates are **strings** with a degree symbol —
//!   `"50.0506312°, 14.3439906°"` — not numbers.
//! - **Legacy Takeout "Semantic Location History"**: `timelineObjects[]`, each a
//!   `placeVisit` or `activitySegment`; coordinates are `latitudeE7`/
//!   `longitudeE7` integers (degrees × 10⁷).
//! - **Legacy Takeout "Records"** (raw signals): a top-level `locations[]` of
//!   `latitudeE7`/`longitudeE7` points.
//!
//! ## Raw now, contract parked (Needs-sample)
//!
//! The **raw layer** is written unconditionally and at full fidelity: the whole
//! imported payload is preserved verbatim under
//! `location/google-timeline/raw/<format>-<hash>.json` (one file per import,
//! named by format + content hash, so re-dropping the same export is idempotent).
//! Nothing the user exported is ever dropped.
//!
//! The **contract layer** — mapping movement segments to [`crate::location::Fix`]
//! rows (lat/lon parsed from the string/E7 forms, `ts` → local, a stable `guid`
//! per fix, the segment's transport class verbatim in `mode`) — is **parked
//! pending a verified sample** (the brief's "Parser-last / Needs-sample" plan).
//! The three formats diverge (iOS-vs-Android variance; iOS reportedly omits
//! `semanticSegments`), the only authoritative coordinate shapes come from
//! community schemas (locationhistoryformat.com) rather than an official Google
//! doc, and the direct web export was removed for many users in 2025 — so no
//! real `Timeline.json` is yet on disk to test field extraction against. Per the
//! evidence rule (a green test against an assumed-shape fixture is false
//! confidence — the raindrop `_id` lesson), this build does **not** fabricate a
//! parser against an unverified shape. It binds the contract (done, the
//! load-bearing artifact every future location collector inherits), ships the
//! full import scaffold (format detection + raw preservation + idempotent
//! re-drop), and surfaces a clear "contract mapping parked" headline. When a
//! real per-platform sample lands, [`map_fixes`] is the one seam to fill — it is
//! `unimplemented!`-free (returns an empty set today) so the binding and
//! scaffold stay green meanwhile.
//!
//! **Place visits stay raw.** Semantic place-visits (name, coordinates,
//! duration) are place/visit-shaped, not fixes; they are never forced into a
//! `Fix` row and wait for a visits-shaped contract (the location-domain ruling).

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::location::Fix;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer fix stream (day-partitioned, written once the parser is
/// unparked); raw payloads nest under `raw/`.
const DIR: &str = "location/google-timeline";
const RAW_DIR: &str = "location/google-timeline/raw";

/// The collector id, identical to the source folder name — the `source` every
/// [`Fix`] carries once [`map_fixes`] is unparked. Referenced by the write-seam
/// tests today; `allow(dead_code)` because the parked parser doesn't emit rows
/// yet.
#[allow(dead_code)]
const SOURCE: &str = "google-timeline";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-timeline",
        name: "Google Timeline",
        kind: IntegrationKind::Import,
        // Privacy-sensitive (a continuous where-you've-been trail): off by
        // default, enabled only with explicit acknowledgement.
        default_on: false,
        description: "Import your GPS location history exported from the Google Maps app \
                      (Timeline.json) — the full export is preserved at full fidelity. Legacy \
                      Takeout location-history files are also accepted. Re-runnable: re-dropping \
                      the same export never duplicates.",
        domain: "location",
        vault_path: "location/google-timeline/",
        toggleable: false,
        setup: &[
            "Google Maps app → your profile → Your Timeline → Settings → Export Timeline data.",
            "Import the produced Timeline.json here (or a legacy Takeout location-history file / zip).",
        ],
        caveats: "Highly sensitive: this is a continuous record of everywhere you've been, so it \
                  ships off by default. The export is preserved in full; the normalized trail rows \
                  await a verified per-platform sample (iOS and Android exports differ), so for now \
                  the import archives your data losslessly without yet charting it.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    // A bare Timeline.json, or a Takeout .zip that contains it.
    accepts: &["json", "zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Format detection (the documented export shapes).

/// Which documented Google location-history export a payload is — detected by
/// its top-level key, so the right slug names the raw file and the (future)
/// mapping knows which shape it's parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    /// On-device export: top-level `semanticSegments` (+ `rawSignals`,
    /// `userLocationProfile`). Coordinates are degree-suffixed strings.
    OnDevice,
    /// Legacy Takeout "Semantic Location History": top-level `timelineObjects[]`
    /// of `placeVisit`/`activitySegment`. Coordinates are E7 integers.
    LegacySemantic,
    /// Legacy Takeout "Records" (raw signals): top-level `locations[]` of E7
    /// points.
    LegacyRecords,
}

impl Format {
    /// A stable slug for the raw filename.
    fn slug(self) -> &'static str {
        match self {
            Format::OnDevice => "on-device",
            Format::LegacySemantic => "legacy-semantic",
            Format::LegacyRecords => "legacy-records",
        }
    }

    /// Detect the export shape from a parsed payload's top-level keys, or `None`
    /// if it matches no documented Google location-history format.
    fn detect(v: &Value) -> Option<Format> {
        let o = v.as_object()?;
        if o.contains_key("semanticSegments") {
            Some(Format::OnDevice)
        } else if o.contains_key("timelineObjects") {
            Some(Format::LegacySemantic)
        } else if o.contains_key("locations") {
            Some(Format::LegacyRecords)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Reading the export body (bare .json or extracted from a Takeout .zip).

/// The location-history JSON body: read from a bare `.json`, or extracted from a
/// Takeout `.zip` (the first `.json` entry whose payload is a recognized
/// location-history shape — a Takeout zip carries many unrelated JSON files).
fn read_payload(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut archive =
            zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            if !entry.name().to_ascii_lowercase().ends_with(".json") {
                continue;
            }
            let mut body = String::new();
            if entry.read_to_string(&mut body).is_err() {
                continue; // not UTF-8 text — skip, keep scanning
            }
            if serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| Format::detect(&v))
                .is_some()
            {
                return Ok(body);
            }
        }
        bail!(
            "no Google location-history JSON found in {} — expected a Timeline.json (semanticSegments) \
             or a legacy Takeout location-history file inside the zip",
            path.display()
        );
    } else {
        std::fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// The import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = read_payload(path)?;
    let payload: Value = serde_json::from_str(&body)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let Some(format) = Format::detect(&payload) else {
        bail!(
            "{} doesn't look like a Google location-history export — expected a top-level \
             semanticSegments (on-device), timelineObjects (legacy Takeout), or locations \
             (legacy Records)",
            path.display()
        );
    };
    progress(ImportProgress { records: 0, percent: 25.0 });

    // --- Raw layer (unconditional, full fidelity) ------------------------
    // The whole export preserved verbatim, one file per import named by format
    // + content hash. Re-dropping the same export is a no-op (idempotent).
    let hash = content_hash(body.as_bytes());
    let rel = format!("{RAW_DIR}/{}-{hash}.json", format.slug());
    let raw_path = vault.resolve(&rel)?;
    let already = raw_path.exists();
    if !already {
        crate::store::write_atomic(&raw_path, body.as_bytes())?;
    }
    progress(ImportProgress { records: 0, percent: 75.0 });

    // --- Contract layer (PARKED — Needs-sample) --------------------------
    // `map_fixes` is the single seam to fill when a verified per-platform sample
    // lands; today it returns an empty set, so no fabricated rows are written.
    let fixes = map_fixes(&payload, format);
    let mapped = fixes.len() as u64;
    if !fixes.is_empty() {
        write_fixes(vault, &fixes)?;
    }
    progress(ImportProgress { records: mapped, percent: 100.0 });

    let raw_note = if already {
        "export already archived (idempotent re-drop)"
    } else {
        "export archived in full"
    };
    let headline = if mapped > 0 {
        format!("{mapped} location fixes imported ({} format); {raw_note}", format.slug())
    } else {
        // The honest parked state: data is preserved, charting awaits a sample.
        format!(
            "{} export recognized — {raw_note}. Trail mapping is pending a verified sample, so no \
             fixes were charted yet (your data is preserved in full under \
             location/google-timeline/raw/).",
            format.slug()
        )
    };
    Ok(ImportOutcome {
        headline,
        counts: [("fixes", mapped), ("archived", u64::from(!already))].into(),
    })
}

/// Append new [`Fix`] rows (day-partitioned by local `ts`), deduped by `guid`
/// against what's already on disk — re-runnable: a re-import never duplicates
/// (the letterboxd/readwise pattern). Active only once [`map_fixes`] is
/// unparked; harmless today (it's fed an empty set).
fn write_fixes(vault: &Vault, fixes: &[Fix]) -> Result<()> {
    let stream = vault.stream(DIR, Partition::Day);
    let mut seen = std::collections::HashSet::new();
    for key in stream.partitions()? {
        for f in stream.read::<Fix>(&key)? {
            if !f.guid.is_empty() {
                seen.insert(f.guid);
            }
        }
    }
    let fresh: Vec<&Fix> = fixes
        .iter()
        .filter(|f| f.guid.is_empty() || seen.insert(f.guid.clone()))
        .collect();
    stream.append(&fresh, |f| &f.ts)?;
    Ok(())
}

/// Map a recognized export payload into contract [`Fix`] rows.
///
/// **PARKED — Needs-sample.** The three documented formats diverge and no
/// verified `Timeline.json` is yet on disk to confirm exact nested field names /
/// units (the brief's Needs-sample flag; the evidence rule forbids parsing
/// blind). Returns an empty set today so the binding + scaffold stay green; this
/// is the single seam to fill when a real per-platform sample lands. The
/// documented mapping target, for the agent who unparks it:
///
/// - **On-device** `semanticSegments[]`: each segment with an `activity`
///   (movement) → one [`Fix`] per `timelinePath[]` point (`point` = a
///   `"<lat>°, <lon>°"` string → parse to numbers; `time` → `ts` local; `trail`
///   = the segment's start time; `mode` = `activity.topCandidate.type` verbatim;
///   `distanceMeters` → `extra`). A `visit` segment is a **place visit** — stays
///   raw, never a `Fix`.
/// - **Legacy `timelineObjects[]`**: an `activitySegment` → fixes from its
///   `startLocation`/`endLocation` + `waypointPath` (`latitudeE7`/`longitudeE7`
///   ÷ 10⁷; `activityType` → `mode`; `distance` → `extra`). A `placeVisit` stays
///   raw.
/// - **Legacy `locations[]`**: each raw signal → a [`Fix`] (`latitudeE7`/
///   `longitudeE7` ÷ 10⁷; `timestamp`/`timestampMs` → `ts`; `accuracy` →
///   `accuracy`; `altitude` → `ele`; `velocity` → `speed`; `heading` →
///   `heading`; a `(ts,lat,lon)` hash for `guid` where no id exists).
fn map_fixes(_payload: &Value, _format: Format) -> Vec<Fix> {
    Vec::new()
}

/// A short stable hex content hash for naming the raw artifact (dedup key for an
/// idempotent re-drop). Not cryptographic — collision-resistance enough to keep
/// distinct exports in distinct files (FNV-1a, 64-bit).
fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir =
            std::env::temp_dir().join(format!("trove-google-timeline-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import(v: &Vault, file: &str, body: &str) -> Result<ImportOutcome> {
        let path = v.root().join(file);
        fs::write(&path, body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {})
    }

    // --- documented export fixtures (verbatim field names from the brief /
    // locationhistoryformat.com / the Dawarich on-device reference) ---------

    /// On-device export: one movement segment (a `timelinePath` of two degree-
    /// suffixed string points) plus one place `visit`. The exact shape the
    /// (parked) on-device mapping will parse.
    fn on_device() -> String {
        json!({
            "semanticSegments": [
                {
                    "startTime": "2024-04-03T08:00:00.000+02:00",
                    "endTime": "2024-04-03T08:30:00.000+02:00",
                    "timelinePath": [
                        {"point": "50.0506312°, 14.3439906°", "time": "2024-04-03T08:14:00.000+02:00"},
                        {"point": "50.0612345°, 14.3501234°", "time": "2024-04-03T08:22:00.000+02:00"}
                    ],
                    "activity": {
                        "probability": 0.92,
                        "topCandidate": {"type": "cycling", "probability": 0.88},
                        "distanceMeters": 1840.0
                    }
                },
                {
                    "startTime": "2024-04-03T08:30:00.000+02:00",
                    "endTime": "2024-04-03T20:10:18.000+02:00",
                    "visit": {
                        "probability": 0.85,
                        "topCandidate": {
                            "placeId": "ChIJN1t_tDeuEmsRUsoyG83frY4",
                            "semanticType": "UNKNOWN",
                            "placeLocation": {"latLng": "50.0506312°, 14.3439906°"}
                        }
                    }
                }
            ],
            "rawSignals": [],
            "userLocationProfile": {}
        })
        .to_string()
    }

    /// Legacy Takeout "Semantic Location History": a `timelineObjects[]` with one
    /// `activitySegment` and one `placeVisit` (E7 integer coordinates).
    fn legacy_semantic() -> String {
        json!({
            "timelineObjects": [
                {
                    "activitySegment": {
                        "startLocation": {"latitudeE7": 500506312, "longitudeE7": 143439906},
                        "endLocation": {"latitudeE7": 500612345, "longitudeE7": 143501234},
                        "duration": {
                            "startTimestamp": "2024-04-03T06:00:00Z",
                            "endTimestamp": "2024-04-03T06:30:00Z"
                        },
                        "distance": 1840,
                        "activityType": "CYCLING",
                        "confidence": "HIGH"
                    }
                },
                {
                    "placeVisit": {
                        "location": {
                            "latitudeE7": 500506312,
                            "longitudeE7": 143439906,
                            "placeId": "ChIJN1t_tDeuEmsRUsoyG83frY4",
                            "name": "Home"
                        },
                        "duration": {
                            "startTimestamp": "2024-04-03T06:30:00Z",
                            "endTimestamp": "2024-04-03T18:10:18Z"
                        }
                    }
                }
            ]
        })
        .to_string()
    }

    /// Legacy Takeout "Records" (raw signals): a top-level `locations[]`.
    fn legacy_records() -> String {
        json!({
            "locations": [
                {
                    "latitudeE7": 500506312,
                    "longitudeE7": 143439906,
                    "accuracy": 12,
                    "timestamp": "2024-04-03T06:14:00Z"
                }
            ]
        })
        .to_string()
    }

    // --- format detection -------------------------------------------------

    #[test]
    fn detects_each_documented_format() {
        assert_eq!(Format::detect(&serde_json::from_str(&on_device()).unwrap()), Some(Format::OnDevice));
        assert_eq!(
            Format::detect(&serde_json::from_str(&legacy_semantic()).unwrap()),
            Some(Format::LegacySemantic)
        );
        assert_eq!(
            Format::detect(&serde_json::from_str(&legacy_records()).unwrap()),
            Some(Format::LegacyRecords)
        );
        // An unrelated JSON object is not a location-history export.
        assert_eq!(Format::detect(&json!({"foo": 1})), None);
        assert_eq!(Format::detect(&json!([1, 2, 3])), None);
    }

    // --- the import scaffold (raw preserved, contract parked) -------------

    #[test]
    fn on_device_import_preserves_raw_verbatim_and_parks_contract() {
        let v = temp_vault("ondevice");
        let body = on_device();
        let out = import(&v, "Timeline.json", &body).unwrap();
        // Parked: zero charted fixes, but the export is archived in full.
        assert_eq!(out.counts.get("fixes"), Some(&0), "contract mapping is parked");
        assert_eq!(out.counts.get("archived"), Some(&1));
        assert!(out.headline.contains("on-device"), "headline names the format: {}", out.headline);
        assert!(out.headline.contains("pending"), "headline is honest about the park: {}", out.headline);

        // Raw layer: the whole payload preserved byte-for-byte under raw/.
        let raw_dir = v.root().join("location/google-timeline/raw");
        let files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "one raw file per import");
        let raw = fs::read_to_string(files[0].path()).unwrap();
        assert_eq!(raw, body, "raw is the export verbatim, full fidelity");
        assert!(
            files[0].file_name().to_string_lossy().starts_with("on-device-"),
            "raw file named by format slug"
        );

        // No contract day-files were written (the park leaves the stream empty).
        assert!(
            !v.root().join("location/google-timeline/2024-04-03.jsonl").exists(),
            "no fabricated contract rows"
        );
    }

    #[test]
    fn re_dropping_the_same_export_is_idempotent() {
        let v = temp_vault("idempotent");
        let body = on_device();
        let first = import(&v, "Timeline.json", &body).unwrap();
        assert_eq!(first.counts.get("archived"), Some(&1), "first drop archives");

        let again = import(&v, "Timeline.json", &body).unwrap();
        assert_eq!(again.counts.get("archived"), Some(&0), "re-drop archives nothing new");
        assert!(again.headline.contains("idempotent"), "headline notes the no-op: {}", again.headline);

        // Still exactly one raw file (content-hash naming dedupes the re-drop).
        let raw_dir = v.root().join("location/google-timeline/raw");
        assert_eq!(fs::read_dir(&raw_dir).unwrap().flatten().count(), 1);
    }

    #[test]
    fn legacy_formats_are_accepted_and_archived() {
        let v = temp_vault("legacy");
        let sem = import(&v, "Semantic Location History.json", &legacy_semantic()).unwrap();
        assert!(sem.headline.contains("legacy-semantic"), "{}", sem.headline);
        assert_eq!(sem.counts.get("archived"), Some(&1));

        let rec = import(&v, "Records.json", &legacy_records()).unwrap();
        assert!(rec.headline.contains("legacy-records"), "{}", rec.headline);
        assert_eq!(rec.counts.get("archived"), Some(&1));

        // Two distinct legacy exports → two distinct raw files.
        let raw_dir = v.root().join("location/google-timeline/raw");
        assert_eq!(fs::read_dir(&raw_dir).unwrap().flatten().count(), 2);
    }

    #[test]
    fn imports_location_history_from_a_takeout_zip() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("takeout.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        // Decoy: an unrelated Takeout JSON that must be skipped.
        w.start_file("Takeout/archive_browser.json", opts).unwrap();
        w.write_all(br#"{"unrelated": true}"#).unwrap();
        // The real location history, nested as Takeout nests it.
        w.start_file("Takeout/Location History (Timeline)/Records.json", opts).unwrap();
        w.write_all(legacy_records().as_bytes()).unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert!(out.headline.contains("legacy-records"), "found the right entry: {}", out.headline);
        let raw_dir = v.root().join("location/google-timeline/raw");
        let raw = fs::read_dir(&raw_dir).unwrap().flatten().next().unwrap();
        assert_eq!(fs::read_to_string(raw.path()).unwrap(), legacy_records(), "the located JSON, verbatim");
    }

    #[test]
    fn rejects_a_non_location_json() {
        let v = temp_vault("reject-json");
        let err = import(&v, "notes.json", r#"{"notes": []}"#).unwrap_err().to_string();
        assert!(err.contains("location-history"), "clear rejection, no panic: {err}");
    }

    #[test]
    fn rejects_invalid_json() {
        let v = temp_vault("reject-bad");
        let err = import(&v, "broken.json", "{not json").unwrap_err().to_string();
        assert!(err.contains("not valid JSON"), "clear rejection: {err}");
    }

    #[test]
    fn rejects_a_zip_without_location_history() {
        let v = temp_vault("reject-zip");
        let zip_path = v.root().join("empty.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Takeout/other.json", opts).unwrap();
        w.write_all(br#"{"something": "else"}"#).unwrap();
        w.finish().unwrap();
        let err = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap_err().to_string();
        assert!(err.contains("no Google location-history JSON"), "clear rejection: {err}");
    }

    // --- the parked seam --------------------------------------------------

    #[test]
    fn map_fixes_is_parked_returns_empty_for_every_format() {
        // The seam is intentionally empty pending a verified sample; every
        // recognized format maps to zero fixes today (no parsing-blind output).
        assert!(map_fixes(&serde_json::from_str(&on_device()).unwrap(), Format::OnDevice).is_empty());
        assert!(map_fixes(&serde_json::from_str(&legacy_semantic()).unwrap(), Format::LegacySemantic).is_empty());
        assert!(map_fixes(&serde_json::from_str(&legacy_records()).unwrap(), Format::LegacyRecords).is_empty());
    }

    // --- the write seam (proves the parked->live path is wired) -----------

    #[test]
    fn write_fixes_day_partitions_and_dedupes_by_guid() {
        // Exercises the (currently-dormant) contract-write path directly, so the
        // day-partition + guid-dedupe plumbing is proven before the parser is
        // unparked. Fed hand-built Fix rows, not parsed output.
        let v = temp_vault("writefixes");
        let mut a = Fix::new(SOURCE, "2024-04-03T08:14:00+02:00", 50.0506312, 14.3439906);
        a.guid = "gt-seg1-0".into();
        a.mode = "cycling".into();
        a.trail = "seg-2024-04-03T08:00:00".into();
        let mut b = Fix::new(SOURCE, "2024-04-03T08:22:00+02:00", 50.0612345, 14.3501234);
        b.guid = "gt-seg1-1".into();
        write_fixes(&v, &[a.clone(), b]).unwrap();

        let day = fs::read_to_string(v.root().join("location/google-timeline/2024-04-03.jsonl")).unwrap();
        assert_eq!(day.lines().count(), 2, "both fixes in the day file");
        assert!(day.contains("\"lat\":50.0506312"), "numeric lat on disk: {day}");
        assert!(day.contains("\"mode\":\"cycling\""));

        // Re-writing the same guids appends nothing (idempotent).
        write_fixes(&v, &[a]).unwrap();
        let day2 = fs::read_to_string(v.root().join("location/google-timeline/2024-04-03.jsonl")).unwrap();
        assert_eq!(day2.lines().count(), 2, "guid dedupe: no duplicate row");
    }

    // --- the generic surfaces the registry wires for free -----------------

    #[test]
    fn hub_exposes_an_import_box_and_the_def_is_location_domain() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "google-timeline").expect("registered in INTEGRATIONS");
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["json", "zip"]);
        assert_eq!(DEF.meta.domain, "location");
        assert!(!DEF.meta.default_on, "privacy-sensitive: off by default");
        assert_eq!(DEF.connection, None, "pure import, no login");
    }
}
