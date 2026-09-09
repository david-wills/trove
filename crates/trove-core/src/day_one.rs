//! Day One — import of Day One's official JSON export ZIP.
//!
//! **Export format:** File → Export → JSON produces a ZIP containing one or
//! more `<JournalName>.json` files (each an object `{"metadata":…, "entries":[…]}`),
//! plus `photos/`, `audios/`, `pdfs/`, and `videos/` subdirectories. Each
//! entry carries a stable `uuid`, UTC `creationDate` / `modifiedDate`
//! (`YYYY-MM-DDTHH:MM:SSZ`), a Markdown `text` body, optional `tags`
//! (string array), `location`, `weather`, `isPinned`, and `starred`. The
//! journal name (filename minus `.json`) is preserved in `extra.journal`.
//!
//! **Contract:** writes the [`notes`](crate::notes) domain:
//! - Contract layer: `notes/day-one/YYYY-MM.jsonl` (one [`Note`] per entry,
//!   partitioned by the local month of `creationDate`, deduped by `uuid`).
//! - Raw layer: `notes/day-one/raw/YYYY-MM.jsonl` (full-fidelity entry JSON
//!   minus `richText`, partitioned by `creationDate` month, deduped by `uuid`).
//!
//! **Re-runnable:** the import deduplicates by `uuid`. A re-import of a newer
//! export updates entries in place; no duplicates are created.
//!
//! EVIDENCE: confirmed against a real 1,024-entry export at
//! `~/Downloads/data library archive/day-one-json/`. Key facts:
//! - Field: `uuid` (string, stable id); `creationDate` / `modifiedDate` (UTC Z);
//!   `text` (Markdown); `tags` ([]string); `location` (object); `weather` (object);
//!   `isPinned` (bool); `starred` (bool); `timeZone` (string).
//! - Multiple journals: the ZIP root holds `<JournalName>.json` files; the
//!   name embeds the journal label (used in `extra.journal`).
//! - Catalogued in the Phase 2 pass; brief: docs/integrations/day-one.md

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Local;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "day-one";
const NOTES_DIR: &str = "notes/day-one";
const RAW_DIR: &str = "notes/day-one/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "day-one",
        name: "Day One",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Day One journal entries from Day One's official \
                      JSON export ZIP. Each entry — including location, weather, \
                      tags, and media references — is stored with full fidelity. \
                      Re-importing a newer export updates entries in place without \
                      creating duplicates.",
        domain: "notes",
        vault_path: "notes/day-one/",
        toggleable: false,
        setup: &[
            "In Day One: File → Export → JSON.",
            "Import the downloaded .zip here (or a single Journal.json from inside it).",
        ],
        caveats: "Media files (photos, audio, PDFs) are referenced by identifier, \
                  not copied as bytes. Re-importing a newer export updates entries \
                  in place, deduplicating by the entry UUID.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "json"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Entry shape from the Day One JSON export

/// One entry from a Day One `<Journal>.json` file.
///
/// Fields confirmed against a real 1,024-entry export (2014–2026). Only the
/// fields the import actively maps are declared; unknown fields are captured
/// by `extra` at build time (the full raw entry).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    uuid: String,
    #[serde(default)]
    creation_date: String,   // "2026-06-08T07:05:11Z" — UTC, Z suffix
    #[serde(default)]
    modified_date: String,
    #[serde(default)]
    text: String,            // Markdown body
    #[serde(default)]
    tags: Vec<String>,       // string array (confirmed)
    #[serde(default)]
    is_pinned: bool,
    #[serde(default)]
    starred: bool,
}

/// Top-level structure of a `<Journal>.json` file.
#[derive(Debug, Deserialize)]
struct JournalFile {
    #[serde(default)]
    entries: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Timestamp helpers

/// Parse a Day One UTC timestamp (`YYYY-MM-DDTHH:MM:SSZ`) to a local RFC3339
/// string. Returns `None` on parse failure (lenient: bad timestamps are
/// skipped, not errors).
fn utc_to_local_rfc3339(s: &str) -> Option<String> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load the existing guids so re-import can detect duplicates.
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for note in stream.read::<Note>(&key)? {
            if !note.id.is_empty() {
                seen.insert(note.id.clone());
            }
        }
    }

    // Collect all (journal_name, raw_entry_Value) pairs from the path.
    let journal_entries = read_journal_entries(path)?;
    let total = journal_entries.len() as u64;

    let (mut imported, mut updated, mut skipped) = (0u64, 0u64, 0u64);
    let mut contract: Vec<Note> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();

    for (idx, (journal_name, raw_val)) in journal_entries.into_iter().enumerate() {
        // Parse structured fields from the full raw Value.
        let entry: RawEntry = match serde_json::from_value(raw_val.clone()) {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if entry.uuid.is_empty() {
            skipped += 1;
            continue;
        }

        let is_update = seen.contains(&entry.uuid);
        // Either way, upsert: first import OR updated entry both write through.
        seen.insert(entry.uuid.clone());

        // --- Contract row ---
        let created = if entry.creation_date.is_empty() {
            None
        } else {
            utc_to_local_rfc3339(&entry.creation_date)
        };
        let Some(created) = created else {
            // No parseable creation date — cannot file into a month partition.
            skipped += 1;
            continue;
        };
        let modified = if entry.modified_date.is_empty() {
            String::new()
        } else {
            utc_to_local_rfc3339(&entry.modified_date).unwrap_or_default()
        };

        let mut note = Note::new(SOURCE, &entry.uuid);
        note.created = created.clone();
        note.modified = modified.clone();
        note.body = entry.text.clone();
        note.tags = entry.tags.clone();
        if entry.is_pinned {
            note.pinned = Some(true);
        }

        // `extra`: journal name + location + weather + starred (all Day One–specific)
        let mut extra = Map::new();
        if !journal_name.is_empty() {
            extra.insert("journal".into(), Value::String(journal_name.clone()));
        }
        if entry.starred {
            extra.insert("starred".into(), Value::Bool(true));
        }
        if let Some(loc) = raw_val.get("location") {
            extra.insert("location".into(), loc.clone());
        }
        if let Some(wx) = raw_val.get("weather") {
            extra.insert("weather".into(), wx.clone());
        }
        // Media references (photos/audios/videos/pdfAttachments) — identifiers
        // only, never byte content.
        for key in &["photos", "audios", "videos", "pdfAttachments"] {
            if let Some(arr) = raw_val.get(*key) {
                if arr.as_array().is_some_and(|a| !a.is_empty()) {
                    extra.insert(key.to_string(), arr.clone());
                }
            }
        }
        note.extra = extra;

        // --- Raw row: the full entry Value, minus `richText` (large and redundant) ---
        let mut raw_obj = raw_val.clone();
        if let Some(obj) = raw_obj.as_object_mut() {
            obj.remove("richText");
            obj.insert("_source".into(), Value::String(SOURCE.into()));
            obj.insert("_journal".into(), Value::String(journal_name));
            obj.insert("_created".into(), Value::String(created.clone()));
        }

        contract.push(note);
        raw.push(raw_obj);

        if is_update {
            updated += 1;
        } else {
            imported += 1;
        }
        if (idx as u64 + 1) % 200 == 0 {
            let pct = (idx as f32 + 1.0) / total.max(1) as f32 * 90.0;
            progress(ImportProgress { records: imported + updated, percent: pct });
        }
    }

    if !contract.is_empty() {
        vault.upsert_day_one_notes(&contract)?;
        vault.upsert_day_one_raw(&raw)?;
    }
    progress(ImportProgress { records: imported + updated, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} entries imported, {updated} updated, {skipped} skipped"
        ),
        counts: [
            ("imported", imported),
            ("updated", updated),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Collect all (journal_name, raw_entry_Value) pairs from `path`, which is
/// either a `.zip` or a single `.json` file. The journal name is inferred
/// from the JSON filename (minus `.json` extension).
fn read_journal_entries(path: &Path) -> Result<Vec<(String, Value)>> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        read_from_zip(path)
    } else {
        // Bare .json file (e.g. a single Journal.json extracted by the user).
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let journal_name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let jf: JournalFile = serde_json::from_str(&body)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(jf.entries.into_iter().map(|e| (journal_name.clone(), e)).collect())
    }
}

/// Read all `<Name>.json` files at the ZIP root; skip non-JSON entries and
/// any subdirectory paths (photos/, audios/, etc.).
fn read_from_zip(path: &Path) -> Result<Vec<(String, Value)>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).with_context(|| format!("reading zip {}", path.display()))?;
    let mut out: Vec<(String, Value)> = Vec::new();
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    for name in names {
        // Only root-level .json files (no slashes = no subdirectory path).
        if !name.ends_with(".json") || name.contains('/') || name.contains('\\') {
            continue;
        }
        let journal_name = name
            .strip_suffix(".json")
            .unwrap_or(&name)
            .to_string();
        let mut entry = archive
            .by_name(&name)
            .with_context(|| format!("reading {name} from zip"))?;
        let mut body = String::new();
        entry.read_to_string(&mut body).with_context(|| format!("reading {name}"))?;
        drop(entry);
        let jf: JournalFile = match serde_json::from_str(&body) {
            Ok(j) => j,
            Err(_) => continue, // non-Day-One JSON at the root — skip silently
        };
        for entry in jf.entries {
            out.push((journal_name.clone(), entry));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Vault impl — upsert helpers (same snapshot+upsert pattern as bear.rs)

impl Vault {
    /// Upsert contract rows into `notes/day-one/YYYY-MM.jsonl`, partitioned
    /// by the local month of `created`, deduped by `id`. Each affected month
    /// file is read, lines with a matching `id` are replaced by the incoming
    /// note, and the file is rewritten atomically.
    pub fn upsert_day_one_notes(&self, notes: &[Note]) -> Result<()> {
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            let Some(key) = Partition::Month.key(&n.created) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(n);
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|n| n.id.as_str()).collect();
            let mut merged: Vec<Note> = stream
                .read::<Note>(&month)?
                .into_iter()
                .filter(|existing| !incoming_ids.contains(existing.id.as_str()))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{NOTES_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Upsert raw rows into `notes/day-one/raw/YYYY-MM.jsonl`, partitioned by
    /// the month of `_created` (the immutable local created timestamp), deduped
    /// by `uuid`.
    fn upsert_day_one_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("uuid").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            let Some(key) = Partition::Month.key(month_of(v)) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(v);
        }
        let stream = self.stream(RAW_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|existing| !incoming_ids.contains(id_of(existing)))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{RAW_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write as _;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-day-one-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Minimal real-shaped Day One JSON with two entries:
    // - Entry A: has location, weather, tags, pinned (2026-06)
    // - Entry B: no location/weather, plain text only (2026-06)
    // - Entry C: undated (should be skipped)
    // Data modelled on confirmed field names from the real 1,024-entry export.
    const JOURNAL_JSON: &str = r#"{
  "metadata": {"version": "1.0"},
  "entries": [
    {
      "uuid": "A1B2C3D4E5F60718293A4B5C6D7E8F90",
      "creationDate": "2026-06-08T14:05:11Z",
      "modifiedDate": "2026-06-08T14:31:44Z",
      "text": "Long run along the coast this morning.",
      "tags": ["running", "reflection"],
      "isPinned": false,
      "starred": false,
      "timeZone": "America/Los_Angeles",
      "location": {"placeName": "Lands End", "latitude": 37.7806, "longitude": -122.5111},
      "weather": {"conditionsDescription": "Foggy", "temperatureCelsius": 13.0, "weatherCode": "fog"},
      "richText": "{\"large\":\"ignored\"}",
      "isAllDay": false,
      "duration": 0
    },
    {
      "uuid": "B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2",
      "creationDate": "2026-06-09T08:00:00Z",
      "modifiedDate": "2026-06-09T08:15:00Z",
      "text": "Simple note, no location or weather.",
      "tags": [],
      "isPinned": true,
      "starred": false,
      "timeZone": "America/Los_Angeles",
      "isAllDay": false,
      "duration": 0
    },
    {
      "uuid": "MISSING-DATE-ENTRY",
      "creationDate": "",
      "modifiedDate": "",
      "text": "This entry has no date and must be skipped.",
      "tags": []
    }
  ]
}"#;

    /// Write a minimal Day One ZIP containing one journal JSON.
    fn make_zip(vault: &Vault, journal_name: &str, json_body: &str) -> std::path::PathBuf {
        let zip_path = vault.root().join(format!("{journal_name}.zip"));
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file(format!("{journal_name}.json"), opts).unwrap();
        w.write_all(json_body.as_bytes()).unwrap();
        // Decoy: a photo subfolder (must not be parsed)
        w.start_file("photos/abc123.jpeg", opts).unwrap();
        w.write_all(b"\xff\xd8\xff").unwrap();
        w.finish().unwrap();
        zip_path
    }

    fn import(v: &Vault, path: &std::path::PathBuf) -> ImportOutcome {
        (IMPORT.run)(v, path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_entries_from_a_zip_deduplicates_by_uuid() {
        let v = temp_vault("zip");
        let zip = make_zip(&v, "Journal", JOURNAL_JSON);
        let out = import(&v, &zip);

        // 2 entries imported, 1 skipped (undated)
        assert_eq!(out.counts["imported"], 2, "two entries imported");
        assert_eq!(out.counts["skipped"], 1, "undated entry skipped");
        assert_eq!(out.counts["updated"], 0);

        // Contract layer: both entries in June (UTC 2026-06-08 → local June)
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 2);
        let a = june.iter().find(|n| n.id == "A1B2C3D4E5F60718293A4B5C6D7E8F90").unwrap();
        assert_eq!(a.source, "day-one");
        assert!(a.body.contains("Long run"));
        assert_eq!(a.tags, vec!["running", "reflection"]);
        assert_eq!(a.pinned, None, "isPinned:false omitted");
        assert!(a.created.starts_with("2026-06-"), "UTC → local, same day");
        assert!(a.modified.starts_with("2026-06-"));
        // Extra: journal, location, weather preserved
        assert_eq!(a.extra.get("journal").and_then(|v| v.as_str()), Some("Journal"));
        assert!(a.extra.contains_key("location"), "location in extra");
        assert!(a.extra.contains_key("weather"), "weather in extra");
        assert_eq!(a.extra.get("starred").and_then(|v| v.as_bool()), None, "starred:false omitted");

        let b = june.iter().find(|n| n.id == "B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2").unwrap();
        assert_eq!(b.pinned, Some(true), "isPinned:true mapped");
        assert!(!b.extra.contains_key("location"), "no location for entry B");
        assert!(!b.extra.contains_key("weather"), "no weather for entry B");

        // richText must NOT appear in the contract extra (too large, redundant)
        assert!(!a.extra.contains_key("richText"), "richText dropped");

        // Raw layer: both present, uuid stable, richText stripped, _created set
        let raw_june = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-06").unwrap();
        assert_eq!(raw_june.len(), 2);
        let raw_a = raw_june.iter().find(|r| r["uuid"] == "A1B2C3D4E5F60718293A4B5C6D7E8F90").unwrap();
        assert_eq!(raw_a.get("richText"), None, "richText stripped from raw");
        assert!(raw_a["_created"].as_str().unwrap().starts_with("2026-06-"));
        assert_eq!(raw_a["_journal"].as_str(), Some("Journal"));
        assert!(raw_a.get("location").is_some(), "location preserved in raw");

        // Re-import: all updated, no duplicates
        let again = import(&v, &zip);
        assert_eq!(again.counts["imported"], 0, "re-import: no new entries");
        assert_eq!(again.counts["updated"], 2, "re-import: both updated");
        let june2 = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june2.len(), 2, "re-import: still exactly 2 lines");
    }

    #[test]
    fn imports_bare_json_file() {
        let v = temp_vault("json");
        let json_path = v.root().join("Journal.json");
        fs::write(&json_path, JOURNAL_JSON).unwrap();
        let out = import(&v, &json_path);
        assert_eq!(out.counts["imported"], 2);
        assert_eq!(out.counts["skipped"], 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 2);
    }

    #[test]
    fn multi_journal_zip_assigns_correct_journal_names() {
        let v = temp_vault("multi");
        // Two journals in one zip: "Daily" and "Dream Journal"
        let zip_path = v.root().join("export.zip");
        let daily_json = r#"{
            "metadata": {"version": "1.0"},
            "entries": [
                {"uuid": "DAILY-1111", "creationDate": "2026-06-10T12:00:00Z",
                 "modifiedDate": "2026-06-10T12:00:00Z", "text": "Morning pages.", "tags": []}
            ]
        }"#;
        let dream_json = r#"{
            "metadata": {"version": "1.0"},
            "entries": [
                {"uuid": "DREAM-2222", "creationDate": "2026-06-10T06:00:00Z",
                 "modifiedDate": "2026-06-10T06:00:00Z", "text": "Vivid dream.", "tags": []}
            ]
        }"#;
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Daily.json", opts).unwrap();
        w.write_all(daily_json.as_bytes()).unwrap();
        w.start_file("Dream Journal.json", opts).unwrap();
        w.write_all(dream_json.as_bytes()).unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 2);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(june.len(), 2);
        let daily = june.iter().find(|n| n.id == "DAILY-1111").unwrap();
        assert_eq!(daily.extra["journal"].as_str(), Some("Daily"));
        let dream = june.iter().find(|n| n.id == "DREAM-2222").unwrap();
        assert_eq!(dream.extra["journal"].as_str(), Some("Dream Journal"));
    }

    #[test]
    fn starred_entry_appears_in_extra() {
        let v = temp_vault("starred");
        let json_body = r#"{
            "metadata": {"version": "1.0"},
            "entries": [
                {"uuid": "STARRED-1", "creationDate": "2026-06-05T10:00:00Z",
                 "modifiedDate": "2026-06-05T10:00:00Z",
                 "text": "A starred entry.", "tags": [], "starred": true, "isPinned": false}
            ]
        }"#;
        let path = v.root().join("J.json");
        fs::write(&path, json_body).unwrap();
        let out = import(&v, &path);
        assert_eq!(out.counts["imported"], 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        let e = &june[0];
        assert_eq!(e.extra.get("starred").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn media_references_land_in_extra_not_as_bytes() {
        let v = temp_vault("media");
        let json_body = r#"{
            "metadata": {"version": "1.0"},
            "entries": [
                {"uuid": "MEDIA-1", "creationDate": "2026-06-07T09:00:00Z",
                 "modifiedDate": "2026-06-07T09:00:00Z", "text": "Photo entry.", "tags": [],
                 "photos": [{"identifier": "PHOTO-ID-123", "type": "jpeg", "md5": "abc"}],
                 "audios": [{"identifier": "AUDIO-ID-456", "format": "aac"}]}
            ]
        }"#;
        let path = v.root().join("J.json");
        fs::write(&path, json_body).unwrap();
        let out = import(&v, &path);
        assert_eq!(out.counts["imported"], 1);
        let june = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        let e = &june[0];
        let photos = e.extra.get("photos").and_then(|v| v.as_array()).unwrap();
        assert_eq!(photos[0]["identifier"].as_str(), Some("PHOTO-ID-123"), "photo ref preserved");
        let audios = e.extra.get("audios").and_then(|v| v.as_array()).unwrap();
        assert_eq!(audios[0]["identifier"].as_str(), Some("AUDIO-ID-456"), "audio ref preserved");
    }

    #[test]
    fn serde_back_compat_old_note_lines_deserialize() {
        // A Note written by an older writer (only source+id) must still round-trip.
        let v = temp_vault("backcompat");
        let dir = v.root().join(NOTES_DIR);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("2026-06.jsonl"),
            "{\"source\":\"day-one\",\"id\":\"OLD-UUID\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "OLD-UUID");
    }
}
