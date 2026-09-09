//! Google Maps Saved Places — user-curated favourites from Google Takeout.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/google-maps.md.
//!
//! An **Import** (no API, no token, no network): the user exports their saved
//! places from Google Takeout (takeout.google.com → Maps (your places)) and
//! drops the produced file(s) into the generic import box.
//!
//! ## Export format
//!
//! Google Takeout exports one GeoJSON `FeatureCollection` per list. Each
//! feature carries a `geometry.coordinates: [lon, lat]` Point and a
//! `properties` object with at least a `Title`, `Published`, `Updated`, and
//! `URL` for the place's Google Maps page; a nested `Location` object holds
//! the formatted name, address, and sometimes a `countryCode`.  Detection key:
//! a top-level `"type": "FeatureCollection"` that has features whose
//! `properties` contain a `"Title"` field.
//!
//! Accepted inputs:
//! - A bare `Saved Places.json` (any list file from the Takeout `Maps (your
//!   places)/` folder).
//! - A Takeout `.zip` — all `.json` entries under `Maps` or `maps` with a
//!   recognisable shape are imported in one pass.
//!
//! ## Vault layout (raw-only)
//!
//! Saved places are **place/visit-shaped, not GPS fixes** — the `location`
//! domain spec explicitly excludes them from the `Fix` contract until a
//! visits-shaped contract lands (see `crates/trove-core/src/location.rs`).
//! Until then the whole export is preserved verbatim under
//! `location/google-maps/raw/`, one file per list (named by list slug +
//! content hash so re-dropping the same export is idempotent), and the feature
//! count is returned as the import headline.  Nothing is fabricated into a
//! shape the spec doesn't yet have.
//!
//! ## Deduplication
//!
//! Content-hash naming deduplicates re-drops of the same file byte-for-byte.
//! A changed export (same list name, different content) gets a new hash and is
//! archived alongside the old one — full fidelity, append-only.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

/// Vault-relative path for the raw archive directory.
const RAW_DIR: &str = "location/google-maps/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    // Files land in raw/ as *.json, not *.jsonl in DIR — use mtime of the
    // actual written path (mirrors ancestrydna/23andme peer pattern).
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-maps",
        name: "Google Maps Saved Places",
        kind: IntegrationKind::Import,
        // Privacy-sensitive (location data — curated places, not a trail):
        // off by default, opt-in with explicit acknowledgement.
        default_on: false,
        description:
            "Import your saved places from Google Maps — Starred, Home, Work, Want to go, \
             and any custom lists — via a Google Takeout GeoJSON export. Re-runnable: \
             re-dropping the same export never duplicates.",
        domain: "location",
        vault_path: "location/google-maps/",
        toggleable: false,
        setup: &[
            "Go to takeout.google.com and choose Maps (your places).",
            "Download the export and import the produced .json file (or the entire Takeout .zip) here.",
        ],
        caveats: "This imports your curated saved places, not visit history. \
                  GPS location history comes from the separate Google Timeline importer. \
                  Saved-place rows are archived at full fidelity; a normalized places \
                  contract is pending a future domain ratification.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["json", "zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Detection

/// Returns `true` if `v` looks like a Google Maps Saved Places
/// `FeatureCollection` — a top-level `"type": "FeatureCollection"` whose
/// first feature (if any) has a `properties.Title` field.
fn is_saved_places(v: &Value) -> bool {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return false,
    };
    if obj.get("type").and_then(Value::as_str) != Some("FeatureCollection") {
        return false;
    }
    // Accept an empty FeatureCollection (no features yet) — still a valid
    // Saved Places export.
    let features = match obj.get("features").and_then(Value::as_array) {
        Some(f) => f,
        None => return false,
    };
    if features.is_empty() {
        return true; // empty list is still a valid export
    }
    // Verify the first feature has a `properties.Title` — the stable key
    // Google has included in every documented Saved Places export.
    features
        .iter()
        .next()
        .and_then(|f| f.as_object())
        .and_then(|f| f.get("properties"))
        .and_then(Value::as_object)
        .map(|p| p.contains_key("Title"))
        .unwrap_or(false)
}

/// A slug for the raw filename derived from the original file stem (the list
/// name in the Takeout export, e.g. `Saved Places`, `Labeled places - Home`).
/// Lowercased, whitespace → dash, non-alphanumeric stripped.
fn list_slug(stem: &str) -> String {
    let lower = stem.to_ascii_lowercase();
    let mut out = String::with_capacity(lower.len());
    let mut last_dash = true; // skip leading dashes
    for c in lower.chars() {
        if c.is_alphanumeric() {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    // Strip trailing dash.
    let out = out.trim_end_matches('-').to_string();
    if out.is_empty() { "places".to_string() } else { out }
}

/// FNV-1a 64-bit content hash for idempotent re-drop naming — same bytes,
/// same name, so re-dropping is a no-op.
fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Reading: bare .json or extracted from a Takeout .zip.

/// One recognized list payload: the raw JSON body and the list slug inferred
/// from the archive entry name.
struct ListPayload {
    slug: String,
    body: String,
    feature_count: usize,
}

/// Read all recognisable Saved Places payloads from `path` (a bare `.json` or
/// a Takeout `.zip` — every Maps-folder `.json` with the right shape).
fn read_payloads(path: &Path) -> Result<Vec<ListPayload>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    if ext == "zip" {
        let file =
            std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut archive =
            zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;
        let mut payloads = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();
            // Only examine JSON files inside a Maps folder.
            let lname = name.to_ascii_lowercase();
            if !lname.ends_with(".json") {
                continue;
            }
            if !lname.contains("maps") {
                continue;
            }
            let mut body = String::new();
            if entry.read_to_string(&mut body).is_err() {
                continue; // not valid UTF-8 — skip
            }
            let parsed: Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !is_saved_places(&parsed) {
                continue;
            }
            let count = parsed["features"].as_array().map(|f| f.len()).unwrap_or(0);
            // Derive a list slug from the file's stem inside the zip.
            let stem = Path::new(&name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("places");
            payloads.push(ListPayload {
                slug: list_slug(stem),
                body,
                feature_count: count,
            });
        }
        if payloads.is_empty() {
            bail!(
                "no Google Maps Saved Places JSON found in {} — expected a Takeout zip with a \
                 Maps (your places) folder containing a FeatureCollection",
                path.display()
            );
        }
        Ok(payloads)
    } else {
        // Bare JSON file.
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let parsed: Value = serde_json::from_str(&body)
            .with_context(|| format!("{} is not valid JSON", path.display()))?;
        if !is_saved_places(&parsed) {
            bail!(
                "{} doesn't look like a Google Maps Saved Places export — expected a \
                 GeoJSON FeatureCollection with features that have a properties.Title field",
                path.display()
            );
        }
        let count = parsed["features"].as_array().map(|f| f.len()).unwrap_or(0);
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("places");
        Ok(vec![ListPayload { slug: list_slug(stem), body, feature_count: count }])
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
    let payloads = read_payloads(path)?;
    progress(ImportProgress { records: 0, percent: 25.0 });

    let raw_base = vault.resolve(RAW_DIR)?;
    std::fs::create_dir_all(&raw_base).with_context(|| {
        format!("creating raw directory {}", raw_base.display())
    })?;

    let total_lists = payloads.len() as u64;
    let (mut total_features, mut archived, mut already_present) = (0u64, 0u64, 0u64);

    for (i, payload) in payloads.iter().enumerate() {
        let hash = content_hash(payload.body.as_bytes());
        let filename = format!("{}-{}.json", payload.slug, hash);
        let dest = raw_base.join(&filename);
        if dest.exists() {
            already_present += 1;
        } else {
            crate::store::write_atomic(&dest, payload.body.as_bytes())?;
            archived += 1;
        }
        total_features += payload.feature_count as u64;
        let pct = 25.0 + 75.0 * (i + 1) as f32 / payloads.len() as f32;
        progress(ImportProgress { records: total_features, percent: pct });
    }

    let raw_note = match (archived, already_present) {
        (0, _) => "all exports already archived (idempotent re-drop)".to_string(),
        (a, 0) => format!("{a} list(s) archived"),
        (a, p) => format!("{a} list(s) archived, {p} already present"),
    };

    let headline = format!(
        "{total_features} saved places across {total_lists} list(s) imported — {raw_note}. \
         Archived at full fidelity under location/google-maps/raw/ \
         (normalized places contract pending)."
    );

    Ok(ImportOutcome {
        headline,
        counts: [
            ("features", total_features),
            ("lists", total_lists),
            ("archived", archived),
        ]
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;
    use serde_json::json;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-google-maps-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import(v: &Vault, file: &str, body: &str) -> Result<ImportOutcome> {
        let path = v.root().join(file);
        fs::write(&path, body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {})
    }

    // --- canonical fixture: the documented Takeout GeoJSON shape -----------
    // Field names match a real Google Takeout Saved Places export: the URL
    // property key is "Google Maps URL" (not "URL"), confirmed by gist
    // benjibee/takeout-tools and the Takeout fields reference.

    fn saved_places_fixture() -> String {
        json!({
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-122.4194, 37.7749]
                    },
                    "properties": {
                        "Title": "Ferry Building",
                        "Published": "2024-01-15T10:30:00Z",
                        "Updated": "2024-01-15T10:30:00Z",
                        "Google Maps URL": "https://www.google.com/maps/place/?q=place_id:ChIJvdLMGyuAhYAROwan1UdLMEo",
                        "Note": "Great farmers market on weekends",
                        "Location": {
                            "Business Name": "Ferry Building Marketplace",
                            "Address": "1 Ferry Building, San Francisco, CA 94111, USA",
                            "Country Code": "US",
                            "Geo Coordinates": {
                                "Latitude": "37.7955",
                                "Longitude": "-122.3937"
                            }
                        }
                    }
                },
                {
                    "type": "Feature",
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-122.4783, 37.8199]
                    },
                    "properties": {
                        "Title": "Golden Gate Bridge",
                        "Published": "2024-02-20T08:00:00Z",
                        "Updated": "2024-02-20T08:00:00Z",
                        "Google Maps URL": "https://www.google.com/maps/place/?q=place_id:ChIJw____96GhYARCVVwg5cT7c0",
                        "Location": {
                            "Address": "Golden Gate Bridge, San Francisco, CA 94129, USA",
                            "Country Code": "US"
                        }
                    }
                }
            ]
        })
        .to_string()
    }

    fn labeled_home_fixture() -> String {
        json!({
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {
                        "type": "Point",
                        "coordinates": [-122.419, 37.774]
                    },
                    "properties": {
                        "Title": "Home",
                        "Published": "2023-06-01T00:00:00Z",
                        "Updated": "2023-06-01T00:00:00Z",
                        "Google Maps URL": "https://www.google.com/maps/place/?q=place_id:ChIJN1t_tDeuEmsRUsoyG83frY4",
                        "Location": {
                            "Address": "123 Main St, San Francisco, CA 94102, USA",
                            "Country Code": "US"
                        }
                    }
                }
            ]
        })
        .to_string()
    }

    fn empty_list_fixture() -> String {
        json!({
            "type": "FeatureCollection",
            "features": []
        })
        .to_string()
    }

    // --- detection -----------------------------------------------------------

    #[test]
    fn detects_saved_places_collection() {
        let v: Value = serde_json::from_str(&saved_places_fixture()).unwrap();
        assert!(is_saved_places(&v), "canonical fixture is detected");
    }

    #[test]
    fn detects_empty_feature_collection() {
        let v: Value = serde_json::from_str(&empty_list_fixture()).unwrap();
        assert!(is_saved_places(&v), "empty FeatureCollection is still valid");
    }

    #[test]
    fn rejects_non_maps_geojson() {
        // A generic GeoJSON FeatureCollection without properties.Title.
        let v = json!({
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "geometry": {"type": "Point", "coordinates": [0.0, 0.0]},
                    "properties": {"name": "some place", "pop": 1000}
                }
            ]
        });
        assert!(!is_saved_places(&v), "generic GeoJSON rejected");
    }

    #[test]
    fn rejects_location_history_json() {
        let v = json!({"semanticSegments": [], "rawSignals": []});
        assert!(!is_saved_places(&v), "timeline export rejected");
    }

    #[test]
    fn rejects_plain_json_object() {
        assert!(!is_saved_places(&json!({"foo": "bar"})));
        assert!(!is_saved_places(&json!([1, 2, 3])));
    }

    // --- list_slug -----------------------------------------------------------

    #[test]
    fn slug_from_standard_list_names() {
        assert_eq!(list_slug("Saved Places"), "saved-places");
        assert_eq!(list_slug("Labeled places - Home"), "labeled-places-home");
        assert_eq!(list_slug("Want to go"), "want-to-go");
        assert_eq!(list_slug("Starred places"), "starred-places");
        assert_eq!(list_slug(""), "places"); // fallback
    }

    // --- raw preservation ----------------------------------------------------

    #[test]
    fn imports_saved_places_preserves_verbatim_raw() {
        let v = temp_vault("basic");
        let body = saved_places_fixture();
        let out = import(&v, "Saved Places.json", &body).unwrap();

        // Feature count and list count reported.
        assert_eq!(out.counts.get("features"), Some(&2));
        assert_eq!(out.counts.get("lists"), Some(&1));
        assert_eq!(out.counts.get("archived"), Some(&1));

        // Raw layer: exactly one file, verbatim body.
        let raw_dir = v.root().join("location/google-maps/raw");
        let files: Vec<_> = fs::read_dir(&raw_dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1, "one raw file per list");
        let on_disk = fs::read_to_string(files[0].path()).unwrap();
        assert_eq!(on_disk, body, "raw file is the export verbatim");
        // Named with the slug + hash.
        let name = files[0].file_name().to_string_lossy().to_string();
        assert!(name.starts_with("saved-places-"), "slug prefix: {name}");
        assert!(name.ends_with(".json"), "json extension: {name}");

        // No contract JSONL rows (parked).
        assert!(
            !v.root().join("location/google-maps/2024-01-15.jsonl").exists(),
            "no fabricated contract rows"
        );

        // Headline is honest.
        assert!(out.headline.contains("2 saved places"), "{}", out.headline);
        assert!(out.headline.contains("1 list"), "{}", out.headline);
    }

    #[test]
    fn idempotent_re_drop_does_not_archive_twice() {
        let v = temp_vault("idempotent");
        let body = saved_places_fixture();
        let first = import(&v, "Saved Places.json", &body).unwrap();
        assert_eq!(first.counts.get("archived"), Some(&1));

        let again = import(&v, "Saved Places.json", &body).unwrap();
        assert_eq!(again.counts.get("archived"), Some(&0), "second drop archives nothing");

        // Still exactly one raw file.
        let raw_dir = v.root().join("location/google-maps/raw");
        assert_eq!(fs::read_dir(&raw_dir).unwrap().flatten().count(), 1);
    }

    #[test]
    fn multiple_lists_each_get_their_own_raw_file() {
        let v = temp_vault("multilists");

        let starred_body = saved_places_fixture();
        let home_body = labeled_home_fixture();

        // Import each list as a separate file (the user may export list by list).
        import(&v, "Saved Places.json", &starred_body).unwrap();
        import(&v, "Labeled places - Home.json", &home_body).unwrap();

        let raw_dir = v.root().join("location/google-maps/raw");
        let files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(files.len(), 2, "one file per list: {files:?}");
        let has_saved = files.iter().any(|n| n.starts_with("saved-places-"));
        let has_home = files.iter().any(|n| n.starts_with("labeled-places-home-"));
        assert!(has_saved, "starred list archived: {files:?}");
        assert!(has_home, "home list archived: {files:?}");
    }

    #[test]
    fn empty_list_is_accepted() {
        let v = temp_vault("empty-list");
        let body = empty_list_fixture();
        let out = import(&v, "Saved Places.json", &body).unwrap();
        assert_eq!(out.counts.get("features"), Some(&0));
        assert_eq!(out.counts.get("archived"), Some(&1), "even an empty list is archived");
    }

    // --- Takeout zip import --------------------------------------------------

    #[test]
    fn imports_from_a_takeout_zip() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("takeout.zip");

        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Decoy: an unrelated Takeout JSON that must be skipped.
        w.start_file("Takeout/archive_browser.json", opts).unwrap();
        w.write_all(br#"{"unrelated": true}"#).unwrap();

        // The Maps (your places) folder with two list files.
        w.start_file("Takeout/Maps (your places)/Saved Places.json", opts).unwrap();
        w.write_all(saved_places_fixture().as_bytes()).unwrap();

        w.start_file("Takeout/Maps (your places)/Labeled places - Home.json", opts).unwrap();
        w.write_all(labeled_home_fixture().as_bytes()).unwrap();

        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("lists"), Some(&2), "both lists found in zip");
        assert_eq!(out.counts.get("features"), Some(&3), "2 + 1 features total");
        assert_eq!(out.counts.get("archived"), Some(&2));

        let raw_dir = v.root().join("location/google-maps/raw");
        assert_eq!(fs::read_dir(&raw_dir).unwrap().flatten().count(), 2);
    }

    #[test]
    fn rejects_zip_without_maps_folder() {
        let v = temp_vault("reject-zip");
        let zip_path = v.root().join("empty.zip");

        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Takeout/other.json", opts).unwrap();
        w.write_all(br#"{"something": "else"}"#).unwrap();
        w.finish().unwrap();

        let err = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("no Google Maps Saved Places JSON found"), "clear error: {err}");
    }

    #[test]
    fn rejects_non_saved_places_json() {
        let v = temp_vault("reject-json");
        let err = import(&v, "notes.json", r#"{"notes": []}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("doesn't look like"), "clear rejection: {err}");
    }

    #[test]
    fn rejects_invalid_json() {
        let v = temp_vault("reject-bad");
        let err = import(&v, "broken.json", "{not json").unwrap_err().to_string();
        assert!(err.contains("not valid JSON"), "clear rejection: {err}");
    }

    // --- hub / registry wiring -----------------------------------------------

    #[test]
    fn hub_exposes_import_box_and_location_domain() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status
            .iter()
            .find(|s| s.id == "google-maps")
            .expect("registered in INTEGRATIONS");
        let import_info = card.import.as_ref().expect("import box info present");
        assert_eq!(import_info.accepts, &["json", "zip"]);
        assert_eq!(DEF.meta.domain, "location");
        assert!(!DEF.meta.default_on, "privacy-sensitive: off by default");
        assert_eq!(DEF.connection, None, "pure import, no login");
    }
}
