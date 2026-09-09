//! Google Keep — import via Google Takeout ZIP export.
//!
//! Google Keep has **no public API**; the only programmatic path is a manual
//! Google Takeout export (`takeout.google.com → Keep → Download`). The ZIP
//! contains a flat `Keep/` folder with one JSON + one HTML per note, plus any
//! attached image files. We read only the JSON files; the HTML is for human
//! readability and duplicates what the JSON carries.
//!
//! # Takeout JSON structure (confirmed against Obsidian importer + community)
//!
//! ```json
//! {
//!   "title": "My note",
//!   "textContent": "Note body text",
//!   "listContent": [
//!     { "text": "Buy milk",  "isChecked": false },
//!     { "text": "Call vet",  "isChecked": true  }
//!   ],
//!   "color": "DEFAULT",
//!   "isPinned": false,
//!   "isArchived": false,
//!   "isTrashed": false,
//!   "createdTimestampUsec": 1640000000000000,
//!   "userEditedTimestampUsec": 1641000000000000,
//!   "labels": [{ "name": "shopping" }],
//!   "attachments": [{ "filePath": "Keep/abc.jpg", "mimetype": "image/jpeg" }]
//! }
//! ```
//!
//! Timestamps are **microseconds** since the Unix epoch; divide by 1 000 000
//! for seconds. Both `createdTimestampUsec` and `userEditedTimestampUsec` must
//! be present — the Obsidian importer treats both as required.
//!
//! # Vault output
//!
//! - **Contract** `notes/google-keep/YYYY-MM.jsonl` — one [`Note`] per note,
//!   partitioned by the local month of `created`, deduped/upserted by `id`
//!   (the JSON filename stem — stable across Takeout runs).
//! - **Raw** `notes/google-keep/raw/YYYY-MM.jsonl` — full-fidelity JSON row
//!   verbatim, partitioned by `_created` (immutable key).
//!
//! `listContent`, `color`, and `attachments` ride in `extra` on the contract
//! row.
//!
//! # Re-import / dedupe
//!
//! The dedupe key is the JSON filename stem (e.g. `MyNote_123456789.json` →
//! `MyNote_123456789`). Takeout generates stable filenames — re-importing a
//! newer export updates the row in place; it never duplicates.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "google-keep";
const NOTES_DIR: &str = "notes/google-keep";
const RAW_DIR: &str = "notes/google-keep/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-keep",
        name: "Google Keep",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Google Keep notes and checklists from a Google Takeout \
                      archive. Re-runnable: re-importing a newer export updates notes in \
                      place and never duplicates. Google Keep has no public API — Takeout \
                      is the only available path.",
        domain: "notes",
        vault_path: "notes/google-keep/",
        toggleable: false,
        setup: &[
            "Go to takeout.google.com and sign in.",
            "Click 'Deselect all', then scroll to 'Keep' and select it.",
            "Click 'Next step' → 'Create export' → download the ZIP when ready.",
            "Import the downloaded ZIP here.",
        ],
        caveats: "Timestamps are microseconds in the raw JSON — the importer converts \
                 them automatically. Images referenced in attachments are inside the \
                 Takeout ZIP; the vault stores the file-path reference only.",
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

// ---------------------------------------------------------------------------
// Takeout JSON deserialization

/// One Google Keep Takeout per-note JSON.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeepNote {
    #[serde(default)]
    title: String,
    /// Present on plain-text notes; absent on list notes.
    #[serde(default)]
    text_content: String,
    /// Present on checklist notes; absent on plain-text notes.
    #[serde(default)]
    list_content: Vec<ListItem>,
    /// Color label enum, e.g. `"DEFAULT"`, `"RED"`, `"YELLOW"`, etc.
    #[serde(default)]
    color: String,
    #[serde(default)]
    is_pinned: bool,
    #[serde(default)]
    is_archived: bool,
    #[serde(default)]
    is_trashed: bool,
    /// Microseconds since Unix epoch — required for a valid note.
    created_timestamp_usec: Option<i64>,
    /// Microseconds since Unix epoch — required for a valid note.
    user_edited_timestamp_usec: Option<i64>,
    #[serde(default)]
    labels: Vec<LabelEntry>,
    #[serde(default)]
    attachments: Vec<AttachmentEntry>,
}

#[derive(Debug, Deserialize)]
struct ListItem {
    #[serde(default)]
    text: String,
    #[serde(default, rename = "isChecked")]
    is_checked: bool,
}

#[derive(Debug, Deserialize)]
struct LabelEntry {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Deserialize)]
struct AttachmentEntry {
    #[serde(default, rename = "filePath")]
    file_path: String,
    #[serde(default)]
    mimetype: String,
}

// ---------------------------------------------------------------------------
// Timestamp conversion

/// Convert a microsecond timestamp to a local RFC3339 string.
/// Returns `None` if the value is zero or conversion fails.
fn usec_to_rfc3339(usec: i64) -> Option<String> {
    if usec <= 0 {
        return None;
    }
    let secs = usec / 1_000_000;
    let nanos = ((usec % 1_000_000) * 1_000) as u32;
    let utc = Utc
        .timestamp_opt(secs, nanos)
        .single()?;
    let local: DateTime<Local> = utc.with_timezone(&Local);
    Some(local.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Body helpers

/// Render `listContent` into a plain-text body (used on the contract layer).
fn render_list(items: &[ListItem]) -> String {
    let mut lines = Vec::with_capacity(items.len());
    for item in items {
        let mark = if item.is_checked { "[x]" } else { "[ ]" };
        lines.push(format!("{} {}", mark, item.text));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Collect all per-note JSON file contents from the Keep/ subtree of the ZIP.
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading zip {}", path.display()))?;

    // Gather (stem, json_content) pairs for all Keep/*.json files.
    let mut json_entries: Vec<(String, String)> = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).with_context(|| "reading zip entry")?;
        let name = entry.name().to_string();
        // Keep notes live in a `Keep/` subtree and are JSON files.
        // Exclude labels.json (a metadata file, not a note).
        if !name.ends_with(".json") {
            continue;
        }
        let lower = name.to_lowercase();
        if !lower.contains("keep/") {
            continue;
        }
        // Skip the labels.json metadata file.
        let filename = Path::new(&name)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        if filename.eq_ignore_ascii_case("labels.json") {
            continue;
        }
        // The stem is the stable note id.
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(filename)
            .to_string();

        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body)
            .with_context(|| format!("reading {name}"))?;
        json_entries.push((stem, body));
    }

    let total = json_entries.len() as u64;

    // `seen` is used ONLY to deduplicate stems within the current zip.
    // Do NOT pre-populate from the vault — upsert_google_keep_notes /
    // upsert_google_keep_raw already replace existing rows by id (filter-then-
    // extend), so pre-loading vault ids here would silently skip edited notes
    // that the caller re-imports with a newer Takeout export.
    let mut seen: HashSet<String> = HashSet::new();

    let mut contract: Vec<Note> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut imported = 0u64;
    let mut duplicates = 0u64;
    let mut skipped = 0u64;

    for (idx, (stem, json_text)) in json_entries.into_iter().enumerate() {
        let kn: KeepNote = match serde_json::from_str(&json_text) {
            Ok(v) => v,
            Err(e) => {
                // Log and skip rather than aborting the whole import.
                eprintln!("google-keep: skipping {stem}: {e}");
                skipped += 1;
                continue;
            }
        };

        // Both timestamps are required to form a valid record.
        let (Some(created_usec), Some(edited_usec)) = (
            kn.created_timestamp_usec,
            kn.user_edited_timestamp_usec,
        ) else {
            skipped += 1;
            continue;
        };
        let Some(created_rfc) = usec_to_rfc3339(created_usec) else {
            skipped += 1;
            continue;
        };
        let modified_rfc = usec_to_rfc3339(edited_usec)
            .unwrap_or_else(|| created_rfc.clone());

        // Dedupe only within the current zip (same stem appearing twice in one
        // Takeout export, which should not happen but is defensive).
        if !seen.insert(stem.clone()) {
            duplicates += 1;
            continue;
        }

        // Build tags from labels.
        let tags: Vec<String> = kn
            .labels
            .iter()
            .filter(|l| !l.name.is_empty())
            .map(|l| l.name.clone())
            .collect();

        // Build body: prefer textContent; fall back to rendered listContent.
        let body = if !kn.text_content.is_empty() {
            kn.text_content.clone()
        } else if !kn.list_content.is_empty() {
            render_list(&kn.list_content)
        } else {
            String::new()
        };

        // --- Contract note ---
        let mut note = Note::new(SOURCE, &stem);
        note.title = kn.title.clone();
        note.body = body;
        note.created = created_rfc.clone();
        note.modified = modified_rfc.clone();
        note.tags = tags;
        if kn.is_pinned {
            note.pinned = Some(true);
        }
        if kn.is_archived {
            note.archived = Some(true);
        }
        if kn.is_trashed {
            note.trashed = Some(true);
        }

        // Extra: color, listContent structure, and attachments.
        let mut extra = Map::new();
        if !kn.color.is_empty() && kn.color != "DEFAULT" {
            extra.insert("color".into(), Value::String(kn.color.clone()));
        }
        if !kn.list_content.is_empty() {
            let items: Vec<Value> = kn
                .list_content
                .iter()
                .map(|it| {
                    serde_json::json!({
                        "text": it.text,
                        "isChecked": it.is_checked
                    })
                })
                .collect();
            extra.insert("listContent".into(), Value::Array(items));
        }
        if !kn.attachments.is_empty() {
            let atts: Vec<Value> = kn
                .attachments
                .iter()
                .map(|a| {
                    let mut m = Map::new();
                    m.insert("filePath".into(), Value::String(a.file_path.clone()));
                    if !a.mimetype.is_empty() {
                        m.insert("mimetype".into(), Value::String(a.mimetype.clone()));
                    }
                    Value::Object(m)
                })
                .collect();
            extra.insert("attachments".into(), Value::Array(atts));
        }
        note.extra = extra;

        // --- Raw row (full fidelity) ---
        let mut raw = serde_json::from_str::<Map<String, Value>>(&json_text)
            .unwrap_or_default();
        raw.insert("source".into(), Value::String(SOURCE.into()));
        raw.insert("id".into(), Value::String(stem.clone()));
        raw.insert("created".into(), Value::String(created_rfc.clone()));
        raw.insert("modified".into(), Value::String(modified_rfc));
        // Immutable partition key for the raw layer.
        raw.insert("_created".into(), Value::String(created_rfc));

        raw_rows.push(Value::Object(raw));
        contract.push(note);
        imported += 1;

        if (idx as u64 + 1) % 50 == 0 {
            progress(ImportProgress {
                records: imported,
                percent: (idx as f32 + 1.0) / total.max(1) as f32 * 100.0,
            });
        }
    }

    if !contract.is_empty() {
        vault.upsert_google_keep_notes(&contract)?;
        vault.upsert_google_keep_raw(&raw_rows)?;
    }

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} notes imported, {duplicates} duplicates skipped{}",
            if skipped > 0 {
                format!(", {skipped} skipped (missing timestamps or parse error)")
            } else {
                String::new()
            }
        ),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers

impl Vault {
    /// Upsert contract notes into `notes/google-keep/YYYY-MM.jsonl`,
    /// partitioned by `created` month, deduped by `id`.
    pub fn upsert_google_keep_notes(&self, notes: &[Note]) -> Result<()> {
        use std::collections::HashMap;
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            if let Some(key) = Partition::Month.key(&n.created) {
                by_month.entry(key.to_string()).or_default().push(n);
            }
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|n| n.id.as_str()).collect();
            let mut merged: Vec<Note> = stream
                .read::<Note>(&month)?
                .into_iter()
                .filter(|e| !incoming_ids.contains(e.id.as_str()))
                .collect();
            merged.extend(incoming.into_iter().cloned());
            let rel = format!("{NOTES_DIR}/{month}.jsonl");
            self.write_snapshot(&rel, &merged)?;
        }
        Ok(())
    }

    /// Upsert raw rows into `notes/google-keep/raw/YYYY-MM.jsonl`,
    /// partitioned by `_created`, deduped by `id`.
    pub fn upsert_google_keep_raw(&self, rows: &[Value]) -> Result<()> {
        use std::collections::HashMap;
        fn month_of(v: &Value) -> &str {
            v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
        }
        fn id_of(v: &Value) -> &str {
            v.get("id").and_then(|i| i.as_str()).unwrap_or("")
        }
        let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
        for v in rows {
            if let Some(key) = Partition::Month.key(month_of(v)) {
                by_month.entry(key.to_string()).or_default().push(v);
            }
        }
        let stream = self.stream(RAW_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> =
                incoming.iter().map(|v| id_of(v)).collect();
            let mut merged: Vec<Value> = stream
                .read::<Value>(&month)?
                .into_iter()
                .filter(|e| !incoming_ids.contains(id_of(e)))
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
    use crate::vault::Vault;
    use serde_json::json;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Unique temp vault — uses `std::env::temp_dir()` with a pid-qualified
    /// name, matching the pattern used by other trove-core tests.
    /// Returns (vault, vault_root_dir). The zip file for the test should be
    /// placed in this directory.
    fn temp_vault(name: &str) -> (Vault, std::path::PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("trove-gkeep-{}-{name}", std::process::id()));
        // Clean up any stale dir from a prior run, then (re)create.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault_dir = dir.join("vault");
        let v = Vault::open_or_create(vault_dir).unwrap();
        (v, dir)
    }

    /// Build a minimal Takeout ZIP in memory with the given note JSON files
    /// under a `Takeout/Keep/` subfolder. Returns the zip bytes.
    fn make_takeout_zip(notes: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default();
            for (filename, content) in notes {
                let path = format!("Takeout/Keep/{filename}");
                w.start_file(&path, opts).unwrap();
                w.write_all(content.as_bytes()).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    /// Fixture: a plain-text note.
    /// createdTimestampUsec = 1 740 787 200 000 000 µs = 2025-03-01T00:00:00 UTC.
    /// Field names confirmed against the Obsidian importer source (keep-json.ts).
    fn plain_note_json() -> &'static str {
        r#"{
            "title": "Shopping list",
            "textContent": "Milk\nBread\nEggs",
            "listContent": [],
            "color": "DEFAULT",
            "isPinned": true,
            "isArchived": false,
            "isTrashed": false,
            "createdTimestampUsec": 1740787200000000,
            "userEditedTimestampUsec": 1740873600000000,
            "labels": [{"name": "personal"}],
            "attachments": []
        }"#
    }

    /// Fixture: a checklist note with two items.
    fn checklist_note_json() -> &'static str {
        r#"{
            "title": "Weekend tasks",
            "textContent": "",
            "listContent": [
                {"text": "Buy milk", "isChecked": false},
                {"text": "Call vet", "isChecked": true}
            ],
            "color": "YELLOW",
            "isPinned": false,
            "isArchived": false,
            "isTrashed": false,
            "createdTimestampUsec": 1740787200000000,
            "userEditedTimestampUsec": 1740873600000000,
            "labels": [],
            "attachments": [{"filePath": "Keep/abc.jpg", "mimetype": "image/jpeg"}]
        }"#
    }

    /// Fixture: a trashed note.
    fn trashed_note_json() -> &'static str {
        r#"{
            "title": "Old note",
            "textContent": "No longer needed",
            "listContent": [],
            "color": "DEFAULT",
            "isPinned": false,
            "isArchived": false,
            "isTrashed": true,
            "createdTimestampUsec": 1740787200000000,
            "userEditedTimestampUsec": 1740873600000000,
            "labels": [],
            "attachments": []
        }"#
    }

    #[test]
    fn usec_to_rfc3339_converts_correctly() {
        // 1 740 787 200 000 000 µs = 1 740 787 200 s = 2025-03-01T00:00:00 UTC
        let r = usec_to_rfc3339(1_740_787_200_000_000).unwrap();
        // The string must be a valid RFC3339 and parse back to the same instant.
        let parsed: DateTime<Local> = chrono::DateTime::parse_from_rfc3339(&r)
            .unwrap()
            .with_timezone(&Local);
        let utc = parsed.with_timezone(&Utc);
        assert_eq!(utc.timestamp(), 1_740_787_200);
    }

    #[test]
    fn render_list_formats_items() {
        let items = vec![
            ListItem { text: "Buy milk".into(), is_checked: false },
            ListItem { text: "Call vet".into(), is_checked: true },
        ];
        let body = render_list(&items);
        assert!(body.contains("[ ] Buy milk"), "unchecked item: {body}");
        assert!(body.contains("[x] Call vet"), "checked item: {body}");
    }

    #[test]
    fn import_plain_note() {
        let (vault, dir) = temp_vault("plain");

        let zip_bytes = make_takeout_zip(&[(
            "ShoppingList_abc123.json",
            plain_note_json(),
        )]);
        let zip_path = dir.join("takeout.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        let outcome = run_import(
            &vault,
            &zip_path,
            &Default::default(),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(outcome.counts.get("imported").copied(), Some(1));
        assert_eq!(outcome.counts.get("duplicates").copied(), Some(0));

        // Contract layer: at least one partition written.
        let stream = vault.stream(NOTES_DIR, Partition::Month);
        let partitions = stream.partitions().unwrap();
        assert!(!partitions.is_empty(), "no partitions written");

        let notes: Vec<Note> = partitions
            .iter()
            .flat_map(|p| stream.read::<Note>(p).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 1);

        let n = &notes[0];
        assert_eq!(n.source, "google-keep");
        assert_eq!(n.id, "ShoppingList_abc123");
        assert_eq!(n.title, "Shopping list");
        assert!(n.body.contains("Milk"), "body: {}", n.body);
        assert_eq!(n.tags, vec!["personal"]);
        assert_eq!(n.pinned, Some(true));
        assert_eq!(n.archived, None); // false → omit
        assert_eq!(n.trashed, None);  // false → omit
        // Color DEFAULT should not appear in extra.
        assert!(!n.extra.contains_key("color"), "DEFAULT color leaked into extra");
    }

    #[test]
    fn import_checklist_note() {
        let (vault, dir) = temp_vault("checklist");

        let zip_bytes =
            make_takeout_zip(&[("WeekendTasks_def456.json", checklist_note_json())]);
        let zip_path = dir.join("takeout.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        let outcome = run_import(
            &vault,
            &zip_path,
            &Default::default(),
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(outcome.counts.get("imported").copied(), Some(1));

        let stream = vault.stream(NOTES_DIR, Partition::Month);
        let notes: Vec<Note> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| stream.read::<Note>(p).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 1);

        let n = &notes[0];
        // Body is the rendered checklist.
        assert!(n.body.contains("[ ] Buy milk"), "body: {}", n.body);
        assert!(n.body.contains("[x] Call vet"), "body: {}", n.body);

        // Extra: YELLOW color and listContent structure.
        assert_eq!(
            n.extra.get("color").and_then(|v| v.as_str()),
            Some("YELLOW")
        );
        let list_items = n.extra.get("listContent").and_then(|v| v.as_array());
        assert!(list_items.is_some(), "listContent missing from extra");
        assert_eq!(list_items.unwrap().len(), 2);

        // Attachment in extra.
        let atts = n.extra.get("attachments").and_then(|v| v.as_array());
        assert!(atts.is_some(), "attachments missing from extra");
        assert_eq!(atts.unwrap()[0]["filePath"], json!("Keep/abc.jpg"));
    }

    #[test]
    fn import_trashed_note_sets_flag() {
        let (vault, dir) = temp_vault("trashed");

        let zip_bytes =
            make_takeout_zip(&[("OldNote_ghi789.json", trashed_note_json())]);
        let zip_path = dir.join("takeout.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        run_import(&vault, &zip_path, &Default::default(), &mut |_| {}).unwrap();

        let stream = vault.stream(NOTES_DIR, Partition::Month);
        let notes: Vec<Note> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| stream.read::<Note>(p).unwrap_or_default())
            .collect();
        let n = &notes[0];
        assert_eq!(n.trashed, Some(true));
    }

    #[test]
    fn reimport_deduplicates() {
        let (vault, dir) = temp_vault("dedup");

        let zip_bytes =
            make_takeout_zip(&[("ShoppingList_abc123.json", plain_note_json())]);
        let zip_path = dir.join("takeout.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        // First import.
        let o1 = run_import(&vault, &zip_path, &Default::default(), &mut |_| {}).unwrap();
        assert_eq!(o1.counts.get("imported").copied(), Some(1));

        // Second import of the same (unchanged) zip: upsert replaces the row
        // in place — the vault still has exactly 1 note. The `duplicates`
        // counter reflects intra-zip duplicates only (there are none), so it
        // is 0; the note is re-processed and counts as imported=1.
        let o2 = run_import(&vault, &zip_path, &Default::default(), &mut |_| {}).unwrap();
        assert_eq!(o2.counts.get("imported").copied(), Some(1));
        assert_eq!(o2.counts.get("duplicates").copied(), Some(0));

        // Only one note in the vault after both imports.
        let stream = vault.stream(NOTES_DIR, Partition::Month);
        let notes: Vec<Note> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| stream.read::<Note>(p).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 1, "deduplication failed: {}", notes.len());
    }

    #[test]
    fn reimport_updates_edited_note() {
        let (vault, dir) = temp_vault("edit");

        // First import: original body.
        let zip_v1 = make_takeout_zip(&[("Note_edit001.json", plain_note_json())]);
        let zip_path_v1 = dir.join("takeout_v1.zip");
        std::fs::write(&zip_path_v1, &zip_v1).unwrap();

        run_import(&vault, &zip_path_v1, &Default::default(), &mut |_| {}).unwrap();

        // Second import: same stem but edited body and updated timestamp.
        let edited_json = r#"{
            "title": "Shopping list",
            "textContent": "Milk\nBread\nEggs\nCheese",
            "listContent": [],
            "color": "DEFAULT",
            "isPinned": true,
            "isArchived": false,
            "isTrashed": false,
            "createdTimestampUsec": 1740787200000000,
            "userEditedTimestampUsec": 1740960000000000,
            "labels": [{"name": "personal"}],
            "attachments": []
        }"#;
        let zip_v2 = make_takeout_zip(&[("Note_edit001.json", edited_json)]);
        let zip_path_v2 = dir.join("takeout_v2.zip");
        std::fs::write(&zip_path_v2, &zip_v2).unwrap();

        let o2 = run_import(&vault, &zip_path_v2, &Default::default(), &mut |_| {}).unwrap();
        assert_eq!(o2.counts.get("imported").copied(), Some(1));

        // Vault must contain exactly ONE note (no duplication).
        let stream = vault.stream(NOTES_DIR, Partition::Month);
        let notes: Vec<Note> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| stream.read::<Note>(p).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 1, "edit-reimport created a duplicate: {}", notes.len());

        // The note body must reflect the EDITED version, not the original.
        let n = &notes[0];
        assert!(
            n.body.contains("Cheese"),
            "edited body not stored — got: {}",
            n.body
        );
    }

    #[test]
    fn raw_layer_written() {
        let (vault, dir) = temp_vault("raw");

        let zip_bytes = make_takeout_zip(&[(
            "ShoppingList_abc123.json",
            plain_note_json(),
        )]);
        let zip_path = dir.join("takeout.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        run_import(&vault, &zip_path, &Default::default(), &mut |_| {}).unwrap();

        let raw_stream = vault.stream(RAW_DIR, Partition::Month);
        let raw_parts = raw_stream.partitions().unwrap();
        assert!(!raw_parts.is_empty(), "no raw partition written");

        let rows: Vec<Value> = raw_parts
            .iter()
            .flat_map(|p| raw_stream.read::<Value>(p).unwrap_or_default())
            .collect();
        assert_eq!(rows.len(), 1);

        // Raw row must carry the verbatim source fields.
        let r = &rows[0];
        assert_eq!(r["source"], json!("google-keep"));
        assert_eq!(r["id"], json!("ShoppingList_abc123"));
        // Original usec timestamp must survive in the raw layer.
        assert_eq!(r["createdTimestampUsec"], json!(1_740_787_200_000_000_i64));
    }

    #[test]
    fn multi_note_zip_imports_all() {
        let (vault, dir) = temp_vault("multi");

        let zip_bytes = make_takeout_zip(&[
            ("Note_aaa.json", plain_note_json()),
            ("Note_bbb.json", checklist_note_json()),
            ("Note_ccc.json", trashed_note_json()),
        ]);
        let zip_path = dir.join("takeout.zip");
        std::fs::write(&zip_path, &zip_bytes).unwrap();

        let outcome =
            run_import(&vault, &zip_path, &Default::default(), &mut |_| {}).unwrap();

        assert_eq!(outcome.counts.get("imported").copied(), Some(3));

        let stream = vault.stream(NOTES_DIR, Partition::Month);
        let notes: Vec<Note> = stream
            .partitions()
            .unwrap()
            .iter()
            .flat_map(|p| stream.read::<Note>(p).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 3, "expected 3 notes, got {}", notes.len());
    }

}
