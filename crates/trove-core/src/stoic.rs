//! Stoic — JSON full-backup import for the Stoic journaling app.
//!
//! **Export:** app menu > Import & Export → JSON (full backup). The export
//! is re-runnable; Trove deduplicates by entry UUID so re-importing a newer
//! backup never creates duplicates.
//!
//! **Format (inferred / folklore — NEEDS SAMPLE VERIFICATION):**
//! The Stoic JSON export format is not publicly documented and no real export
//! file was available during development. The parser below is a scaffold built
//! from:
//!   - The Phase-2 research brief (docs/integrations/stoic.md): "entry text,
//!     timestamps, attachments flag, mood/metrics; structurally near-identical
//!     to Day One".
//!   - Common patterns from iOS journaling apps (Day One, Diarium, etc.) that
//!     use either `{"entries":[…]}` wrappers or bare `[…]` arrays, with
//!     camelCase timestamp fields and a string UUID.
//!
//! **IMPORTANT:** The exact field names for mood, metrics, and potentially for
//! the entry UUID and timestamps are unconfirmed. Before shipping to users,
//! obtain a real export file and verify/update `StoicEntry` field names and
//! the `entry_to_note` mapping. See `Needs-sample` flag.
//!
//! **Contract:** writes the [`notes`](crate::notes) domain:
//! - Contract layer: `notes/stoic/YYYY-MM.jsonl` (one [`Note`] per entry,
//!   partitioned by the local month of the creation timestamp, deduped by UUID).
//! - Raw layer: `notes/stoic/raw/YYYY-MM.jsonl` (full-fidelity entry JSON,
//!   partitioned by `_created`, deduped by `uuid`).
//!
//! **Re-runnable:** deduplicated by UUID (updates entries in place on re-import).
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/stoic.md.

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

const SOURCE: &str = "stoic";
const NOTES_DIR: &str = "notes/stoic";
const RAW_DIR: &str = "notes/stoic/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "stoic",
        name: "Stoic",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Stoic journal entries from Stoic's JSON full-backup \
                      export. Entries (including mood and metric values) are stored \
                      with full fidelity. Re-importing a newer export updates entries \
                      in place without creating duplicates.",
        domain: "notes",
        vault_path: "notes/stoic/",
        toggleable: false,
        setup: &[
            "In the Stoic app: Settings (gear icon) → Import & Export → Export as JSON.",
            "Import the downloaded JSON file here.",
        ],
        caveats: "Photos are only included in the export if 'Include Attachments' is \
                  enabled when exporting. Re-importing a newer export updates entries \
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
    accepts: &["json", "zip"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Entry shape (folklore — field names unconfirmed, needs real export sample)
//
// Stoic is documented as "structurally near-identical to Day One". The fields
// below are best-effort guesses from: (a) the research brief, (b) Day One's
// known shape, (c) common iOS journaling app conventions.
//
// Fields that are NOT present in the real export will simply deserialize as
// Default (empty/false/0) due to `#[serde(default)]`. Unknown fields in the
// real export are captured via the full-fidelity raw layer and surfaced in
// `extra` via the untyped `Value` pass-through.
//
// VERIFY BEFORE SHIPPING: run `cargo test -p trove-core stoic::` against a
// real export and confirm uuid/text/timestamp field names match.

/// One journal entry from the Stoic JSON export.
///
/// All fields except `uuid` carry `#[serde(default)]` so that a real export
/// with different casing or absent optional fields still deserialises without
/// error. The full `Value` is also captured (raw layer) so no data is lost.
///
/// **Timestamp encoding:** `created_at` and `modified_at` are typed as
/// `Value` (not `String`) so that both string-encoded ISO 8601 dates *and*
/// integer epoch timestamps (seconds or milliseconds — common in iOS app
/// exports) deserialize correctly.  `to_local_rfc3339_value` handles both
/// branches.  When the field is absent the value defaults to
/// `Value::Null` and the timestamp is treated as missing.
#[derive(Debug, Deserialize)]
struct StoicEntry {
    // Identity — likely `uuid` (Day One convention); may also be `id`.
    #[serde(rename = "uuid", alias = "id", alias = "entryId", alias = "entry_id", default)]
    uuid: String,

    // Body text — `text` (Day One) or `body` or `content`.
    #[serde(rename = "text", alias = "body", alias = "content", alias = "entryText",
            alias = "entry_text", default)]
    text: String,

    // Creation timestamp — either an ISO 8601 string or an integer epoch
    // (seconds or milliseconds); field may be `createdAt`, `creationDate`,
    // `created_at`, `date`, etc.
    #[serde(rename = "createdAt", alias = "creationDate", alias = "created_at",
            alias = "date", alias = "createdDate", alias = "created",
            default = "default_null")]
    created_at: Value,

    // Modified timestamp — same encoding flexibility as `created_at`.
    #[serde(rename = "modifiedAt", alias = "modifiedDate", alias = "modified_at",
            alias = "updatedAt", alias = "updated_at", alias = "lastModified",
            default = "default_null")]
    modified_at: Value,

    // Tags — `tags` is universal.
    #[serde(default)]
    tags: Vec<String>,

    // Attachments flag.
    #[serde(rename = "hasAttachments", alias = "has_attachments", alias = "hasMedia",
            default)]
    has_attachments: bool,

    // Title / journal folder — not universally present; captured here so the
    // known_keys list can exclude them from extra (they survive in raw).
    // Will be mapped to Note.title / Note.folder once a real sample confirms
    // the exact field names.
    #[serde(default)]
    title: String,
    #[serde(rename = "folder", alias = "journal", alias = "journalName", default)]
    folder: String,
}

/// Default function for serde: returns `Value::Null` for absent timestamp fields.
fn default_null() -> Value {
    Value::Null
}

/// Top-level structure: either `{"entries":[…]}` (Day One–style wrapper) or
/// an object with another common key. We try multiple shapes.
#[derive(Debug, Deserialize)]
struct StoicExport {
    #[serde(alias = "data", alias = "items", alias = "journal", default)]
    entries: Vec<Value>,
}

// ---------------------------------------------------------------------------
// Timestamp helpers

/// Parse a timestamp value (RFC3339 string, bare date string, integer epoch
/// seconds, or integer epoch milliseconds) into a local RFC3339 string.
/// Returns `None` on parse failure or if the value is absent/null.
///
/// Handles:
/// - RFC3339 / ISO 8601 strings (e.g. `"2026-06-10T07:30:00Z"`)
/// - Bare date strings `"YYYY-MM-DD"` → midnight local
/// - Integer epoch **seconds** (10-digit, e.g. `1749545400`)
/// - Integer epoch **milliseconds** (13-digit, e.g. `1749545400000`)
///
/// The 10-vs-13 digit heuristic: values ≥ 1e12 are treated as ms; values in
/// [1e9, 1e12) are treated as seconds.  This cleanly separates years
/// 2001–2286 (seconds) from 2001–2286 (ms).
fn to_local_rfc3339_value(v: &Value) -> Option<String> {
    match v {
        Value::Number(n) => {
            use chrono::TimeZone as _;
            let ms = if let Some(i) = n.as_i64() {
                if i >= 1_000_000_000_000 {
                    i  // already milliseconds
                } else {
                    i.checked_mul(1_000)?  // seconds → milliseconds
                }
            } else {
                return None;
            };
            let dt = chrono::Utc.timestamp_millis_opt(ms).single()?;
            Some(dt.with_timezone(&Local).to_rfc3339())
        }
        Value::String(s) => to_local_rfc3339_str(s),
        _ => None,
    }
}

/// Parse an ISO 8601 / RFC3339 timestamp **string** to a local RFC3339 string.
/// Returns `None` on parse failure.
fn to_local_rfc3339_str(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Try RFC3339 first (covers "Z" suffix and "+HH:MM" offsets).
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).to_rfc3339());
    }
    // Try bare date "YYYY-MM-DD" → midnight local.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        use chrono::TimeZone as _;
        return d
            .and_hms_opt(0, 0, 0)
            .and_then(|ndt| Local.from_local_datetime(&ndt).single())
            .map(|dt| dt.to_rfc3339());
    }
    None
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Load the existing UUIDs so re-import can detect updates vs. new entries.
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for note in stream.read::<Note>(&key)? {
            if !note.id.is_empty() {
                seen.insert(note.id.clone());
            }
        }
    }

    let raw_entries = read_entries(path)?;
    let total = raw_entries.len() as u64;

    let (mut imported, mut updated, mut skipped) = (0u64, 0u64, 0u64);
    let mut contract: Vec<Note> = Vec::new();
    let mut raw: Vec<Value> = Vec::new();

    for (idx, raw_val) in raw_entries.into_iter().enumerate() {
        let entry: StoicEntry = match serde_json::from_value(raw_val.clone()) {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        // UUID is required; an empty UUID means we can't deduplicate.
        if entry.uuid.is_empty() {
            skipped += 1;
            continue;
        }

        let is_update = seen.contains(&entry.uuid);
        seen.insert(entry.uuid.clone());

        // Parse creation timestamp — required for month partitioning.
        let Some(created) = to_local_rfc3339_value(&entry.created_at) else {
            skipped += 1;
            continue;
        };

        let modified = if entry.modified_at.is_null() {
            String::new()
        } else {
            to_local_rfc3339_value(&entry.modified_at).unwrap_or_default()
        };

        // --- Contract row ---
        let mut note = Note::new(SOURCE, &entry.uuid);
        note.created = created.clone();
        note.modified = modified;
        note.body = entry.text.clone();
        note.tags = entry.tags.clone();

        // Source-specific fields -> extra (mood, metrics, attachments flag,
        // and any other Stoic-specific data from the raw Value).
        let mut extra = Map::new();
        if entry.has_attachments {
            extra.insert("hasAttachments".into(), Value::Bool(true));
        }
        // Map title/folder to Note fields when present (pending real sample
        // to confirm exact field names; see Needs-sample validation row).
        if !entry.title.is_empty() {
            note.title = entry.title.clone();
        }
        if !entry.folder.is_empty() {
            note.folder = entry.folder.clone();
        }
        // Capture mood / metrics / any other top-level keys that aren't
        // already mapped to Note fields — surfaced verbatim in extra so no
        // Stoic-specific data is lost regardless of the exact field names.
        let known_keys = [
            "uuid", "id", "entryId", "entry_id",
            "text", "body", "content", "entryText", "entry_text",
            "createdAt", "creationDate", "created_at", "date", "createdDate", "created",
            "modifiedAt", "modifiedDate", "modified_at", "updatedAt", "updated_at",
            "lastModified",
            "tags",
            "hasAttachments", "has_attachments", "hasMedia",
            "title",
            "folder", "journal", "journalName",
        ];
        if let Some(obj) = raw_val.as_object() {
            for (k, v) in obj {
                if !known_keys.contains(&k.as_str()) {
                    extra.insert(k.clone(), v.clone());
                }
            }
        }
        note.extra = extra;

        // --- Raw row: full-fidelity entry plus housekeeping keys ---
        let mut raw_obj = raw_val.clone();
        if let Some(obj) = raw_obj.as_object_mut() {
            obj.insert("_source".into(), Value::String(SOURCE.into()));
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
        vault.upsert_stoic_notes(&contract)?;
        vault.upsert_stoic_raw(&raw)?;
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

/// Read journal entries from a `.json` file (or `.zip` containing a `.json`).
///
/// Tries three shapes, in order:
/// 1. `{"entries": […]}` (or `{"data":[…]}` / `{"items":[…]}` / `{"journal":[…]}`).
/// 2. A bare JSON array `[{…}, …]`.
/// 3. A single entry object `{…}` (treated as a 1-element list).
fn read_entries(path: &Path) -> Result<Vec<Value>> {
    let body = if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        read_from_zip(path)?
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))?
    };

    let top: Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing JSON from {}", path.display()))?;

    // Shape 1: wrapped object with an array field.
    if top.is_object() {
        if let Ok(export) = serde_json::from_value::<StoicExport>(top.clone()) {
            if !export.entries.is_empty() {
                return Ok(export.entries);
            }
        }
        // Shape 3: single entry object (the whole file is one entry).
        return Ok(vec![top]);
    }

    // Shape 2: bare array.
    if let Some(arr) = top.as_array() {
        return Ok(arr.clone());
    }

    Ok(Vec::new())
}

/// Read the first `.json` file from the root of a `.zip` archive.
fn read_from_zip(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", path.display()))?;
    let names: Vec<String> = archive.file_names().map(|s| s.to_string()).collect();
    for name in &names {
        // Root-level JSON only (no subdirectory paths).
        if !name.ends_with(".json") || name.contains('/') || name.contains('\\') {
            continue;
        }
        let mut entry = archive
            .by_name(name)
            .with_context(|| format!("reading {name} from zip"))?;
        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .with_context(|| format!("reading {name}"))?;
        return Ok(body);
    }
    anyhow::bail!("no JSON file found at the root of the zip — is this a Stoic export?")
}

// ---------------------------------------------------------------------------
// Vault helpers

impl Vault {
    /// Upsert contract rows into `notes/stoic/YYYY-MM.jsonl`, partitioned by
    /// the local month of `created`, deduped by `id`. Each affected month file
    /// is read, lines with a matching `id` are replaced by the incoming note,
    /// and the file is rewritten atomically.
    pub fn upsert_stoic_notes(&self, notes: &[Note]) -> Result<()> {
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            let Some(key) = Partition::Month.key(&n.created) else {
                continue;
            };
            by_month.entry(key.to_string()).or_default().push(n);
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|n| n.id.as_str()).collect();
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

    /// Upsert raw rows into `notes/stoic/raw/YYYY-MM.jsonl`, partitioned by
    /// the month of `_created`, deduped by `uuid` / `id`.
    fn upsert_stoic_raw(&self, rows: &[Value]) -> Result<()> {
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("uuid")
                .or_else(|| v.get("id"))
                .or_else(|| v.get("entryId"))
                .and_then(|i| i.as_str())
                .unwrap_or("")
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
//
// NOTE: These tests use a plausible / inferred fixture shape because no real
// Stoic export sample was available. Field names (uuid, text, createdAt, etc.)
// are best-effort guesses. When a real export is obtained, verify that the
// field names match and update both the fixtures and the `StoicEntry` struct.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-stoic-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Plausible Stoic JSON export fixture (INFERRED — field names unconfirmed).
    // Two entries:
    //   A: has mood + metrics + tags (2026-06)
    //   B: plain text, hasAttachments:true (2026-06)
    // One malformed entry (no uuid) — must be skipped.
    // Shape: `{"entries":[…]}` wrapper (Day One–style, most likely).
    const EXPORT_JSON: &str = r#"{
  "entries": [
    {
      "uuid": "STOIC-A1B2C3D4-E5F6-7890-ABCD-EF1234567890",
      "text": "Morning reflection: feeling grateful today.",
      "createdAt": "2026-06-10T07:30:00Z",
      "modifiedAt": "2026-06-10T07:45:00Z",
      "tags": ["gratitude", "morning"],
      "hasAttachments": false,
      "mood": 4,
      "metrics": [
        {"name": "anxiety", "value": 2},
        {"name": "energy", "value": 5}
      ]
    },
    {
      "uuid": "STOIC-B2C3D4E5-F6A7-8901-BCDE-F12345678901",
      "text": "Evening check-in. Long day but productive.",
      "createdAt": "2026-06-10T21:00:00Z",
      "modifiedAt": "2026-06-10T21:05:00Z",
      "tags": [],
      "hasAttachments": true,
      "mood": 3
    },
    {
      "text": "Entry with no UUID — must be skipped.",
      "createdAt": "2026-06-10T12:00:00Z"
    }
  ]
}"#;

    fn do_import(v: &Vault, json_body: &str) -> ImportOutcome {
        let path = v.root().join("stoic-export.json");
        fs::write(&path, json_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_entries_deduplicates_by_uuid() {
        let v = temp_vault("basic");
        let out = do_import(&v, EXPORT_JSON);

        assert_eq!(out.counts["imported"], 2, "two valid entries imported");
        assert_eq!(out.counts["skipped"], 1, "one no-uuid entry skipped");
        assert_eq!(out.counts["updated"], 0);

        let june = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2026-06")
            .unwrap();
        assert_eq!(june.len(), 2);

        let a = june
            .iter()
            .find(|n| n.id == "STOIC-A1B2C3D4-E5F6-7890-ABCD-EF1234567890")
            .unwrap();
        assert_eq!(a.source, "stoic");
        assert!(a.body.contains("Morning reflection"));
        assert_eq!(a.tags, vec!["gratitude", "morning"]);
        assert!(a.created.starts_with("2026-06-10"), "UTC → local, same day");
        assert!(!a.modified.is_empty(), "modifiedAt parsed");
        // mood and metrics ride in extra (source-specific).
        assert!(a.extra.contains_key("mood"), "mood in extra");
        assert!(a.extra.contains_key("metrics"), "metrics in extra");
        // hasAttachments false → omitted from extra (default false).
        assert!(!a.extra.contains_key("hasAttachments"), "false hasAttachments omitted");

        let b = june
            .iter()
            .find(|n| n.id == "STOIC-B2C3D4E5-F6A7-8901-BCDE-F12345678901")
            .unwrap();
        assert!(b.body.contains("Evening check-in"));
        // hasAttachments true → present in extra.
        assert_eq!(
            b.extra.get("hasAttachments").and_then(|v| v.as_bool()),
            Some(true),
            "hasAttachments:true in extra"
        );
    }

    #[test]
    fn raw_layer_is_full_fidelity() {
        let v = temp_vault("raw");
        do_import(&v, EXPORT_JSON);

        let raw_june = v
            .stream(RAW_DIR, Partition::Month)
            .read::<Value>("2026-06")
            .unwrap();
        assert_eq!(raw_june.len(), 2, "two raw rows for June");
        let raw_a = raw_june
            .iter()
            .find(|r| r["uuid"] == "STOIC-A1B2C3D4-E5F6-7890-ABCD-EF1234567890")
            .unwrap();
        assert_eq!(raw_a["_source"].as_str(), Some("stoic"));
        assert!(raw_a["_created"].as_str().unwrap().starts_with("2026-06-10"));
        // Original mood and metrics preserved verbatim.
        assert_eq!(raw_a["mood"], Value::Number(4.into()));
        assert!(raw_a["metrics"].is_array(), "metrics array preserved in raw");
    }

    #[test]
    fn re_import_updates_in_place_no_duplicates() {
        let v = temp_vault("rerun");
        let out1 = do_import(&v, EXPORT_JSON);
        assert_eq!(out1.counts["imported"], 2);

        // Re-import the same file.
        let out2 = do_import(&v, EXPORT_JSON);
        assert_eq!(out2.counts["imported"], 0, "no new entries on re-import");
        assert_eq!(out2.counts["updated"], 2, "both entries updated");
        assert_eq!(out2.counts["skipped"], 1, "no-uuid still skipped");

        // Still exactly 2 lines after re-import.
        let june = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2026-06")
            .unwrap();
        assert_eq!(june.len(), 2, "no duplicates after re-import");
    }

    #[test]
    fn bare_array_shape_is_accepted() {
        let bare = r#"[
          {"uuid": "BARE-0001", "text": "bare array entry",
           "createdAt": "2026-06-11T09:00:00Z", "modifiedAt": "2026-06-11T09:01:00Z"}
        ]"#;
        let v = temp_vault("bare");
        let out = do_import(&v, bare);
        assert_eq!(out.counts["imported"], 1, "bare array parsed");
        let june = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2026-06")
            .unwrap();
        assert_eq!(june.len(), 1);
        assert_eq!(june[0].id, "BARE-0001");
    }

    #[test]
    fn accepts_zip_containing_json() {
        use std::io::Write as _;
        let v = temp_vault("zip");
        let zip_path = v.root().join("stoic-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("stoic-backup.json", opts).unwrap();
        w.write_all(EXPORT_JSON.as_bytes()).unwrap();
        // Decoy: non-JSON file at root.
        w.start_file("README.txt", opts).unwrap();
        w.write_all(b"Stoic backup").unwrap();
        w.finish().unwrap();

        let out =
            (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 2, "entries parsed from zip");
    }

    #[test]
    fn alias_field_names_accepted() {
        // Day One–style field names (creationDate / modifiedDate / id) to verify
        // the serde aliases work if the real export uses those instead.
        let alt_json = r#"{"entries": [
          {"id": "ALT-UUID-001", "body": "alias body test",
           "creationDate": "2026-06-12T10:00:00Z", "modifiedDate": "2026-06-12T10:05:00Z",
           "tags": ["test"]}
        ]}"#;
        let v = temp_vault("alias");
        let out = do_import(&v, alt_json);
        assert_eq!(out.counts["imported"], 1, "alias fields deserialized");
        let june = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2026-06")
            .unwrap();
        assert_eq!(june[0].id, "ALT-UUID-001");
        assert_eq!(june[0].body, "alias body test");
        assert_eq!(june[0].tags, vec!["test"]);
    }

    /// Regression test for the "silent-zero-collection" defect: if a real Stoic
    /// export encodes timestamps as integer epoch seconds or milliseconds (common
    /// in iOS app exports), the `String`-typed `created_at` field would silently
    /// default to `""`, `to_local_rfc3339` would return `None`, and EVERY entry
    /// would be skipped.
    ///
    /// This test exercises all three numeric timestamp cases:
    ///   - epoch seconds (10-digit)  → 2023-01-15
    ///   - epoch milliseconds (13-digit) → 2024-03-20
    ///   - string fallback still works alongside numeric entries
    #[test]
    fn epoch_timestamps_are_accepted() {
        // 1674777600 = 2023-01-27 00:00:00 UTC (epoch seconds, 10 digits)
        // 1710892800000 = 2024-03-20 00:00:00 UTC (epoch ms, 13 digits)
        let epoch_json = r#"{"entries": [
          {
            "uuid": "EPOCH-SEC-0001",
            "text": "Entry with epoch-second timestamp",
            "createdAt": 1674777600,
            "modifiedAt": 1674777700
          },
          {
            "uuid": "EPOCH-MS-0002",
            "text": "Entry with epoch-millisecond timestamp",
            "createdAt": 1710892800000,
            "modifiedAt": 1710892900000
          },
          {
            "uuid": "STRING-TS-0003",
            "text": "Entry with string timestamp alongside numeric ones",
            "createdAt": "2025-05-10T12:00:00Z"
          }
        ]}"#;
        let v = temp_vault("epoch");
        let out = do_import(&v, epoch_json);
        assert_eq!(out.counts["imported"], 3, "all three entries imported");
        assert_eq!(out.counts["skipped"], 0, "no entries silently skipped");

        // Verify epoch-seconds entry parsed to a real date.
        let jan23 = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2023-01")
            .unwrap();
        assert_eq!(jan23.len(), 1);
        assert_eq!(jan23[0].id, "EPOCH-SEC-0001");
        assert!(jan23[0].created.starts_with("2023-01"), "epoch-sec → 2023-01");
        assert!(!jan23[0].modified.is_empty(), "modified epoch-sec parsed");

        // Verify epoch-milliseconds entry parsed to a real date.
        let mar24 = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2024-03")
            .unwrap();
        assert_eq!(mar24.len(), 1);
        assert_eq!(mar24[0].id, "EPOCH-MS-0002");
        assert!(mar24[0].created.starts_with("2024-03"), "epoch-ms → 2024-03");

        // Verify string-timestamp entry still works.
        let may25 = v
            .stream(NOTES_DIR, Partition::Month)
            .read::<Note>("2025-05")
            .unwrap();
        assert_eq!(may25.len(), 1);
        assert_eq!(may25[0].id, "STRING-TS-0003");
    }

    /// Verify that `to_local_rfc3339_value` unit-level helper handles each case.
    #[test]
    fn to_local_rfc3339_value_unit() {
        use serde_json::json;

        // RFC3339 string → Some
        let r = to_local_rfc3339_value(&json!("2026-06-10T07:30:00Z"));
        assert!(r.is_some(), "RFC3339 string parsed");
        assert!(r.unwrap().contains("2026-06-10"), "correct date");

        // Epoch seconds (10-digit) → Some, year 2023
        let r = to_local_rfc3339_value(&json!(1_674_777_600_i64));
        assert!(r.is_some(), "epoch-sec parsed");
        assert!(r.unwrap().contains("2023-01"), "correct year-month");

        // Epoch milliseconds (13-digit) → Some, year 2024
        let r = to_local_rfc3339_value(&json!(1_710_892_800_000_i64));
        assert!(r.is_some(), "epoch-ms parsed");
        assert!(r.unwrap().contains("2024-03"), "correct year-month");

        // Null → None
        assert!(to_local_rfc3339_value(&Value::Null).is_none(), "null → None");

        // Empty string → None
        assert!(to_local_rfc3339_value(&json!("")).is_none(), "empty str → None");

        // Bare date string → Some
        let r = to_local_rfc3339_value(&json!("2025-05-10"));
        assert!(r.is_some(), "bare date parsed");
        assert!(r.unwrap().contains("2025-05-10"), "correct date");
    }
}
