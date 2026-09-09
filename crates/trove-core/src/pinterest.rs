//! Pinterest — official data-export import (board and pin metadata).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/pinterest.md.
//!
//! ## What this imports
//!
//! Pinterest's "Request your data" ZIP (Settings > Privacy and Data > Request
//! your data — email with a link within ~48 hours) contains metadata-only
//! records for boards, pins, followers, following, and account info.  Images
//! are **not** included; pin URLs point at Pinterest's CDN.
//!
//! Pins are **saves/curation** (not authored posts), so they do NOT route to
//! the `social` contract (which is for authored content only per `social.rs`).
//! Everything lands raw-only under `social/pinterest/raw/`.
//!
//! ## Parser status — parked, needs sample
//!
//! The export format is not publicly documented and no sample is available on
//! disk.  The scaffold walks every ZIP entry and writes each CSV file as JSON
//! objects (one per row, columns from the header row).  Field names and
//! structure come from the header rows in a real export — when a real sample
//! is available, the per-section parsers can be replaced with typed structs
//! and proper guid-based dedupe.
//!
//! **Needs-sample** — raw/scaffold is live; structured parser is deferred.
//!
//! ## Vault layout
//!
//! ```text
//! social/pinterest/raw/
//!   boards.jsonl         ← one JSON object per CSV row
//!   pins.jsonl           ← one JSON object per CSV row
//!   followers.jsonl      ← one JSON object per CSV row
//!   following.jsonl      ← one JSON object per CSV row
//!   <other>.jsonl        ← any other CSV/JSON entry in the export
//! ```
//!
//! All raw files are **content-hash deduplicated** so re-importing the same
//! export is a no-op.  The hash is scoped per-section (section name is
//! mixed into the hash input) so byte-identical rows that appear in two
//! different sections (e.g. a mutual follow appearing in both
//! `followers.csv` and `following.csv`) are written to both JSONL files —
//! cross-section deduplication would silently drop legitimate data.

use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

const RAW_DIR: &str = "social/pinterest/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join(RAW_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "pinterest",
        name: "Pinterest",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Pinterest boards and saved pins from the official \
                      data export. Captures board/pin metadata and CDN image URLs; \
                      no image files are downloaded. Pins are saves/curation and \
                      stay in the per-source raw vault. Re-runnable.",
        domain: "social",
        vault_path: "social/pinterest/",
        toggleable: false,
        setup: &[
            "Pinterest → Settings → Privacy and Data → Request your data.",
            "You will receive an email with a ZIP download link within ~48 hours.",
            "Import the downloaded ZIP here.",
        ],
        caveats: "The official export contains metadata and CDN URLs only — image \
                  copies are not included. Pins are saves, not authored posts, \
                  so they land in the per-source raw vault rather than the \
                  social-posts timeline.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip"],
    params: &[],
    run: run_import,
};

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP {}", path.display()))?;

    // Load existing content hashes from all raw sections to skip duplicates
    // on re-import.  Hashes are scoped per-section (the section name is
    // mixed in) so identical rows in different sections are both preserved.
    let mut seen_hashes = load_seen_hashes(vault)?;

    let mut rows_written = 0u64;
    let mut rows_skipped = 0u64;

    // Collect entry names first (ZipArchive borrow).
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
        .collect();

    for (idx, name) in names.iter().enumerate() {
        // Skip directories and macOS metadata entries.
        if name.ends_with('/') || name.starts_with("__MACOSX") || name.contains("/.") {
            continue;
        }

        let section = section_name(name);
        let lower = name.to_lowercase();

        let mut entry = archive
            .by_name(name)
            .with_context(|| format!("reading ZIP entry {name}"))?;
        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .with_context(|| format!("decoding {name}"))?;

        let (written, skipped) = if lower.ends_with(".csv") {
            write_csv_section(vault, &section, &body, &mut seen_hashes)?
        } else if lower.ends_with(".json") {
            write_json_section(vault, &section, &body, &mut seen_hashes)?
        } else {
            (0, 0)
        };

        rows_written += written;
        rows_skipped += skipped;

        if idx % 5 == 0 {
            progress(ImportProgress { records: rows_written, percent: 0.0 });
        }
    }

    progress(ImportProgress { records: rows_written, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{rows_written} rows imported, {rows_skipped} duplicates skipped"),
        counts: [("imported", rows_written), ("duplicates", rows_skipped)].into(),
    })
}

/// Derive a vault section name from a ZIP entry path.
/// e.g. `"data/boards.csv"` → `"boards"`, `"pin-data.csv"` → `"pin_data"`.
fn section_name(zip_entry: &str) -> String {
    let leaf = zip_entry.rsplit('/').next().unwrap_or(zip_entry);
    // Strip the file extension (last `.`-segment).
    let stem = match leaf.rfind('.') {
        Some(dot) => &leaf[..dot],
        None => leaf,
    };
    stem.replace(['-', ' '], "_").to_lowercase()
}

/// Parse a CSV body (header row + data rows) into JSON objects and append to
/// `social/pinterest/raw/<section>.jsonl`.  Content-hash dedupe.
///
/// This is the scaffold layer.  When a real Pinterest export is available, the
/// parser can be replaced with a typed struct carrying proper guid-based dedupe.
fn write_csv_section(
    vault: &Vault,
    section: &str,
    body: &str,
    seen: &mut HashSet<String>,
) -> Result<(u64, u64)> {
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers: Vec<String> = match rdr.headers() {
        Ok(h) => h.iter().map(|s| s.trim().to_string()).collect(),
        Err(_) => return Ok((0, 0)),
    };

    let rel = format!("{RAW_DIR}/{section}.jsonl");
    let mut written = 0u64;
    let mut skipped = 0u64;

    for result in rdr.records() {
        let record = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let mut obj = Map::new();
        for (k, v) in headers.iter().zip(record.iter()) {
            let v = v.trim();
            if !v.is_empty() {
                obj.insert(k.clone(), Value::String(v.to_string()));
            }
        }
        if obj.is_empty() {
            continue;
        }

        let hash = content_hash(section, &obj);
        if !seen.insert(hash) {
            skipped += 1;
            continue;
        }

        append_raw_line(vault, &rel, &Value::Object(obj))?;
        written += 1;
    }
    Ok((written, skipped))
}

/// Parse a JSON file (array or single object) and write each item as a JSONL line.
fn write_json_section(
    vault: &Vault,
    section: &str,
    body: &str,
    seen: &mut HashSet<String>,
) -> Result<(u64, u64)> {
    let val: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return Ok((0, 0)),
    };

    let items: Vec<&Value> = match &val {
        Value::Array(arr) => arr.iter().collect(),
        other => vec![other],
    };

    let rel = format!("{RAW_DIR}/{section}.jsonl");
    let mut written = 0u64;
    let mut skipped = 0u64;

    for item in items {
        let hash = content_hash_value(section, item);
        if !seen.insert(hash) {
            skipped += 1;
            continue;
        }
        append_raw_line(vault, &rel, item)?;
        written += 1;
    }
    Ok((written, skipped))
}

/// Append one JSON value as a newline-terminated JSONL line under the vault.
fn append_raw_line(vault: &Vault, rel: &str, val: &Value) -> Result<()> {
    use std::io::Write;
    let path = vault.resolve(rel)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {rel}"))?;
    writeln!(f, "{}", serde_json::to_string(val)?)?;
    Ok(())
}

/// SHA-256 of `<section>:<canonical-json>` — scoped per-section so that a
/// byte-identical row appearing in two different export sections (e.g. a
/// mutual follow in both `followers` and `following`) is preserved in each.
fn content_hash(section: &str, obj: &Map<String, Value>) -> String {
    content_hash_value(section, &Value::Object(obj.clone()))
}

fn content_hash_value(section: &str, val: &Value) -> String {
    let canonical = serde_json::to_string(val).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(section.as_bytes());
    hasher.update(b":");
    hasher.update(canonical.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Read all existing content hashes from each raw section JSONL file.
///
/// The hash stored on disk and the hash computed at import time both use the
/// **same** scoped formula (`section:json`), so re-imports are idempotent
/// while cross-section duplicates are correctly preserved.
fn load_seen_hashes(vault: &Vault) -> Result<HashSet<String>> {
    let mut seen = HashSet::new();
    let raw_path = vault.resolve(RAW_DIR)?;
    let Ok(entries) = std::fs::read_dir(&raw_path) else {
        return Ok(seen);
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "jsonl") {
            // Derive the section name from the file stem (mirrors section_name()).
            let section = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if let Ok(body) = std::fs::read_to_string(&p) {
                for line in body.lines().filter(|l| !l.trim().is_empty()) {
                    if let Ok(val) = serde_json::from_str::<Value>(line) {
                        seen.insert(content_hash_value(&section, &val));
                    }
                }
            }
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-pinterest-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, path: &Path) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    /// Build a minimal synthetic ZIP exercising the scaffold import.
    fn make_zip(name: &str, csv_boards: &str, csv_pins: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir()
            .join(format!("trove-pinterest-zip-{}-{name}.zip", std::process::id()));
        let _ = fs::remove_file(&path);
        let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        z.start_file("data/boards.csv", opts).unwrap();
        z.write_all(csv_boards.as_bytes()).unwrap();

        z.start_file("data/pins.csv", opts).unwrap();
        z.write_all(csv_pins.as_bytes()).unwrap();

        z.finish().unwrap();
        path
    }

    // Synthetic CSV fixtures — column names are plausible guesses until a
    // real Pinterest export confirms the actual headers (Needs-sample).
    const BOARDS_CSV: &str = "\
Board Name,Board URL,Board Description,Date Created
Travel Plans,https://pinterest.com/user/travel-plans,Places to visit,2023-01-15
Home Decor,https://pinterest.com/user/home-decor,Interior ideas,2022-06-01
";

    const PINS_CSV: &str = "\
Pin ID,Board Name,Pin Title,Pin Description,Pin Link,Image URL,Date Pinned
123456,Travel Plans,Santorini,Beautiful views,https://example.com/santorini,https://i.pinimg.com/img1.jpg,2024-03-10
789012,Home Decor,Minimalist shelf,Clean shelving,https://example.com/shelf,https://i.pinimg.com/img2.jpg,2024-04-22
";

    #[test]
    fn imports_boards_and_pins_raw() {
        let v = temp_vault("basic");
        let zip = make_zip("basic", BOARDS_CSV, PINS_CSV);

        let out = run(&v, &zip);
        // 2 boards + 2 pins = 4 rows.
        assert_eq!(out.counts["imported"], 4, "should import 4 rows: {out:?}");
        assert_eq!(out.counts["duplicates"], 0);

        let boards =
            fs::read_to_string(v.root().join("social/pinterest/raw/boards.jsonl")).unwrap();
        assert_eq!(boards.lines().count(), 2, "two board rows");
        assert!(boards.contains("Travel Plans"), "board name present");

        let pins =
            fs::read_to_string(v.root().join("social/pinterest/raw/pins.jsonl")).unwrap();
        assert_eq!(pins.lines().count(), 2, "two pin rows");
        assert!(pins.contains("123456"), "pin id present");
    }

    #[test]
    fn import_is_rerunnable() {
        let v = temp_vault("rerun");
        let zip = make_zip("rerun", BOARDS_CSV, PINS_CSV);

        let out1 = run(&v, &zip);
        assert_eq!(out1.counts["imported"], 4);

        // Re-import the same ZIP — no duplicates.
        let out2 = run(&v, &zip);
        assert_eq!(out2.counts["imported"], 0, "re-run adds nothing");
        assert_eq!(out2.counts["duplicates"], 4, "all 4 already known");

        // File on disk unchanged.
        let pins =
            fs::read_to_string(v.root().join("social/pinterest/raw/pins.jsonl")).unwrap();
        assert_eq!(pins.lines().count(), 2, "no extra lines after re-run");
    }

    #[test]
    fn section_name_strips_path_and_extension() {
        assert_eq!(section_name("data/boards.csv"), "boards");
        assert_eq!(section_name("pin-data.csv"), "pin_data");
        assert_eq!(section_name("User Profile.csv"), "user_profile");
        assert_eq!(section_name("followers.json"), "followers");
        assert_eq!(section_name("data/following_list.csv"), "following_list");
    }

    #[test]
    fn def_is_import_and_no_connection() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)), "Behavior::Import");
        assert_eq!(DEF.meta.id, "pinterest");
        assert!(DEF.connection.is_none(), "no connection");
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"zip"), "accepts zip");
    }

    /// Regression: a mutual follow (same content row in both followers.csv and
    /// following.csv) must appear in BOTH output files.  The old code shared a
    /// single HashSet across all sections and hashed only the JSON content, so
    /// the second section's row was silently dropped.  The fix scopes the hash
    /// per-section (section name mixed into the hash input).
    #[test]
    fn cross_section_identical_rows_both_written() {
        // Build a ZIP where followers.csv and following.csv share one identical
        // row — a mutual follow (same profile URL, same schema).
        let shared_csv = "Profile URL\nhttps://pinterest.com/mutual_friend\n";
        let unique_follower = "Profile URL\nhttps://pinterest.com/only_follower\n";
        let unique_following = "Profile URL\nhttps://pinterest.com/only_following\n";

        let zip_path = {
            let path = std::env::temp_dir().join(format!(
                "trove-pinterest-xsect-{}.zip",
                std::process::id()
            ));
            let _ = fs::remove_file(&path);
            let mut z = zip::ZipWriter::new(fs::File::create(&path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();

            // followers: mutual + unique_follower
            z.start_file("data/followers.csv", opts).unwrap();
            z.write_all(shared_csv.as_bytes()).unwrap();
            z.write_all(
                unique_follower
                    .lines()
                    .skip(1) // skip header
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_bytes(),
            ).unwrap();

            // following: mutual + unique_following
            z.start_file("data/following.csv", opts).unwrap();
            z.write_all(shared_csv.as_bytes()).unwrap();
            z.write_all(
                unique_following
                    .lines()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .as_bytes(),
            ).unwrap();

            z.finish().unwrap();
            path
        };

        let v = temp_vault("cross_section");
        let out = run(&v, &zip_path);

        // 2 rows total: mutual appears once in followers, once in following.
        // unique_follower + unique_following = 2 more only if they parse, but
        // with the simple shared_csv header the mutual row is both sections' row.
        // At minimum: both section files must exist and each contain the mutual.
        let followers_path = v.root().join("social/pinterest/raw/followers.jsonl");
        let following_path = v.root().join("social/pinterest/raw/following.jsonl");

        assert!(followers_path.exists(), "followers.jsonl must be created");
        assert!(following_path.exists(), "following.jsonl must be created");

        let followers_body = fs::read_to_string(&followers_path).unwrap();
        let following_body = fs::read_to_string(&following_path).unwrap();

        assert!(
            followers_body.contains("mutual_friend"),
            "mutual follow must appear in followers.jsonl"
        );
        assert!(
            following_body.contains("mutual_friend"),
            "mutual follow must appear in following.jsonl (was silently dropped before fix)"
        );

        // Re-run must still be idempotent: counts imported=0, duplicates=total.
        let out2 = run(&v, &zip_path);
        assert_eq!(out2.counts["imported"], 0, "re-run should import nothing");
        assert_eq!(
            out2.counts["duplicates"],
            out.counts["imported"],
            "all rows already known on re-run"
        );
    }
}
