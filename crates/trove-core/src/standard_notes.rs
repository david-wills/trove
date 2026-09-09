//! Standard Notes — end-to-end-encrypted note-taking app.
//!
//! Because Standard Notes encrypts all data at rest (E2EE), the only readable
//! surface is the user-initiated **Decrypted** export: Preferences → Backups →
//! Download Backup (choose Decrypted). This produces a ZIP containing:
//!
//! - `Standard Notes Backup and Import File.txt` (or `.json`) — a JSON document
//!   with `{"items": [...], "version": "..."}` holding every note, tag, and
//!   extension item as a `DecryptedTransferPayload`.
//! - Individual plain-text or Markdown files (one per note) for human
//!   readability; **not parsed here** — the JSON backup is the canonical source
//!   with all metadata (uuid, timestamps, tags).
//!
//! # JSON item structure (confirmed against `PurePayload.ts` / SN source)
//!
//! ```json
//! {
//!   "uuid": "4F8A2C1E-0B7D-4E2A-9F3C-1A2B3C4D5E6F",
//!   "content_type": "Note",
//!   "created_at": "2024-03-14T09:12:00.000Z",
//!   "updated_at": "2024-04-02T18:40:00.000Z",
//!   "deleted": false,
//!   "content": {
//!     "title": "Garden planting plan",
//!     "text": "# Garden planting plan\n\n- Tomatoes in the south bed",
//!     "references": [],
//!     "noteType": "plain-text"
//!   }
//! }
//! ```
//!
//! Tags are `content_type: "Tag"` items whose `content.references` lists the
//! UUIDs of notes belonging to that tag (`{uuid, content_type: "Note"}`). We
//! invert this at import time to build a per-note tag list.
//!
//! # Extension note types
//!
//! Notes whose `content.noteType` / `content.editorIdentifier` are non-plain
//! (Super, Markdown, Code, Spreadsheet, etc.) keep their `text` body verbatim
//! and carry `extra.note_type` + `extra.editor_identifier` so the type is
//! preserved for future reading. This is graceful: all bodies are still
//! valid strings — the rich formatting is inside the text.
//!
//! # Dedupe
//!
//! The `guid` is the note's `uuid` (stable across exports). Re-importing a
//! newer export upserts updated notes and never duplicates.
//!
//! # Vault output
//!
//! - **Contract** `notes/standard-notes/YYYY-MM.jsonl` — one [`Note`] per note,
//!   partitioned by the local month of `created`, deduped by `uuid`.
//! - **Raw** `notes/standard-notes/raw/YYYY-MM.jsonl` — full-fidelity item JSON
//!   (complete `content` object verbatim), partitioned by `_created`.
//!
//! The importer accepts the ZIP as-is, or the bare backup JSON/TXT file.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "standard-notes";
const NOTES_DIR: &str = "notes/standard-notes";
const RAW_DIR: &str = "notes/standard-notes/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "standard-notes",
        name: "Standard Notes",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your notes from a Standard Notes decrypted export. \
                      Because Standard Notes uses end-to-end encryption, the only \
                      readable path is the Decrypted backup you download from \
                      Preferences → Backups. Re-runnable: newer exports update \
                      notes in place and never duplicate.",
        domain: "notes",
        vault_path: "notes/standard-notes/",
        toggleable: false,
        setup: &[
            "Open Standard Notes → Preferences → Backups.",
            "Click 'Download Backup' and choose Decrypted.",
            "Import the downloaded ZIP (or the backup .txt/.json from inside it) here.",
        ],
        caveats: "Extension note types (Super editor, Code, Spreadsheets) keep \
                  their text body verbatim with a note_type marker; formatting is \
                  preserved as-is. The Encrypted export option requires the account \
                  password to decrypt and cannot be imported here — use Decrypted.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "txt", "json"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Backup JSON deserialization

/// The top-level backup file wrapper.
#[derive(Debug, Deserialize)]
struct BackupFile {
    /// The items array (notes, tags, extensions, etc.).
    #[serde(default)]
    items: Vec<Value>,
}

/// One Standard Notes item from the backup JSON, partially typed.
/// We keep `content` as `Value` to handle all content_types uniformly in the
/// raw layer and to avoid schema coupling to extension-specific shapes.
#[derive(Debug, Deserialize)]
struct SnItem {
    #[serde(default)]
    uuid: String,
    #[serde(default)]
    content_type: String,
    /// ISO 8601 timestamp string (e.g. "2024-03-14T09:12:00.000Z").
    #[serde(default)]
    created_at: String,
    /// ISO 8601 timestamp string.
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    deleted: bool,
    /// The item's content object; None if absent or null.
    content: Option<Value>,
}

// ---------------------------------------------------------------------------
// Timestamp parsing

/// Parse a Standard Notes ISO 8601 UTC timestamp string to local RFC3339.
/// Returns `None` if the string is empty or unparseable.
fn sn_ts_to_rfc3339(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Standard Notes emits UTC timestamps ending in "Z" or with explicit offset.
    let utc: DateTime<Utc> = DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
        // Fallback: some older exports omit the trailing Z.
        .or_else(|| {
            // Try appending Z and parsing again.
            let with_z = format!("{}Z", s.trim_end_matches('Z'));
            DateTime::parse_from_rfc3339(&with_z)
                .ok()
                .map(|t| t.with_timezone(&Utc))
        })?;
    let local: DateTime<Local> = utc.with_timezone(&Local);
    Some(local.to_rfc3339())
}

// ---------------------------------------------------------------------------
// Tag resolution

/// Build a map of note uuid → tag titles by inverting tag references.
/// In Standard Notes a Tag item's `content.references` is a list of
/// `{uuid, content_type}` objects pointing to the notes it contains.
fn build_tag_map(items: &[SnItem]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for item in items {
        if item.content_type != "Tag" || item.deleted {
            continue;
        }
        let content = match &item.content {
            Some(Value::Object(m)) => m,
            _ => continue,
        };
        let title = content
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if title.is_empty() {
            continue;
        }
        let references = content
            .get("references")
            .and_then(|r| r.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        for r in references {
            let ref_type = r.get("content_type").and_then(|v| v.as_str()).unwrap_or("");
            if ref_type != "Note" {
                continue;
            }
            let note_uuid = r.get("uuid").and_then(|v| v.as_str()).unwrap_or("");
            if !note_uuid.is_empty() {
                map.entry(note_uuid.to_string())
                    .or_default()
                    .push(title.clone());
            }
        }
    }
    // Sort tags per note so output is deterministic.
    for tags in map.values_mut() {
        tags.sort();
    }
    map
}

// ---------------------------------------------------------------------------
// Note extraction

/// Convert one `content_type: "Note"` item into a `(Note, raw_row)` pair.
/// Returns `None` only if the item has an empty uuid (truly unusable).
/// The caller decides whether to include the note in the contract layer based
/// on `item.deleted`; the raw row is always valid for archival use.
fn note_to_row(
    item: &SnItem,
    tags: &[String],
) -> Option<(Note, Value)> {
    if item.uuid.is_empty() {
        return None;
    }
    // Fall back to updated_at when created_at is missing/unparseable so a
    // real note is never dropped solely for a missing created_at timestamp.
    let created = sn_ts_to_rfc3339(&item.created_at)
        .or_else(|| sn_ts_to_rfc3339(&item.updated_at))?;
    let modified = sn_ts_to_rfc3339(&item.updated_at)
        .unwrap_or_else(|| created.clone());

    let content = item.content.as_ref().and_then(|v| v.as_object());

    let title = content
        .and_then(|m| m.get("title"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let body = content
        .and_then(|m| m.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // note_type / editorIdentifier — marks extension note types (Super, Code, …)
    let note_type = content
        .and_then(|m| m.get("noteType"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "plain-text")
        .map(|s| s.to_string());

    let editor_id = content
        .and_then(|m| m.get("editorIdentifier"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // User-facing state flags from content object (distinct from top-level
    // `deleted` which is a server-tombstone; content.trashed is "moved to
    // trash" by the user but the note body is still present in the export).
    let trashed = content
        .and_then(|m| m.get("trashed"))
        .and_then(|v| v.as_bool());
    let pinned = content
        .and_then(|m| m.get("pinned"))
        .and_then(|v| v.as_bool());
    let archived = content
        .and_then(|m| m.get("archived"))
        .and_then(|v| v.as_bool());

    // Contract note
    let mut note = Note::new(SOURCE, &item.uuid);
    note.title = title;
    note.body = body;
    note.created = created.clone();
    note.modified = modified.clone();
    note.tags = tags.to_vec();
    note.trashed = trashed;
    note.pinned = pinned;
    note.archived = archived;

    let mut extra = Map::new();
    if let Some(nt) = note_type {
        extra.insert("note_type".into(), Value::String(nt));
    }
    if let Some(ei) = editor_id {
        extra.insert("editor_identifier".into(), Value::String(ei));
    }
    note.extra = extra;

    // Raw row — the full item JSON verbatim, plus navigation helpers
    let mut raw = Map::new();
    raw.insert("source".into(), Value::String(SOURCE.into()));
    raw.insert("id".into(), Value::String(item.uuid.clone()));
    raw.insert("uuid".into(), Value::String(item.uuid.clone()));
    raw.insert("content_type".into(), Value::String(item.content_type.clone()));
    raw.insert("created_at".into(), Value::String(item.created_at.clone()));
    raw.insert("updated_at".into(), Value::String(item.updated_at.clone()));
    raw.insert("deleted".into(), Value::Bool(item.deleted));
    if let Some(c) = &item.content {
        raw.insert("content".into(), c.clone());
    }
    // Immutable partition key (created month — never modified_at, so the row
    // stays in exactly one file and id-upsert is complete).
    raw.insert("_created".into(), Value::String(created));
    if !note.tags.is_empty() {
        let tag_arr = note.tags.iter().cloned().map(Value::String).collect();
        raw.insert("tags".into(), Value::Array(tag_arr));
    }

    Some((note, Value::Object(raw)))
}

// ---------------------------------------------------------------------------
// Backup body extraction (ZIP or bare file)

/// Read the backup JSON body from:
/// - A ZIP → find the first `*.txt` or `*.json` file at the root (the
///   "Standard Notes Backup and Import File") that contains `{"items":`.
/// - A bare `.txt` or `.json` file → read directly.
fn backup_body(path: &Path) -> Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    if ext == "zip" {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", path.display()))?;

        // The backup JSON file sits at the root of the ZIP (no subfolder).
        // It is typically named "Standard Notes Backup and Import File.txt"
        // or ".json". We pick the first root-level file whose name ends in
        // `.txt` or `.json` and whose contents start with `{`.
        let mut chosen: Option<String> = None;
        for i in 0..archive.len() {
            let mut entry = archive
                .by_index(i)
                .with_context(|| "reading zip entry")?;
            let name = entry.name().to_string();
            // Root-level only (no slash except trailing).
            let parts: Vec<&str> = name.trim_end_matches('/').splitn(2, '/').collect();
            if parts.len() > 1 {
                continue; // nested
            }
            let lower = name.to_lowercase();
            if !lower.ends_with(".txt") && !lower.ends_with(".json") {
                continue;
            }
            let mut body = String::new();
            entry
                .read_to_string(&mut body)
                .with_context(|| format!("reading {name}"))?;
            let trimmed = body.trim_start();
            if trimmed.starts_with('{') {
                chosen = Some(body);
                break;
            }
        }
        chosen.with_context(|| {
            "no Standard Notes backup JSON found in this ZIP — \
             make sure you exported a Decrypted backup from Preferences → Backups"
        })
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = backup_body(path)?;
    let backup: BackupFile = serde_json::from_str(&body)
        .context("parsing Standard Notes backup JSON — is this a Decrypted export?")?;

    // Deserialize items array into typed structs (skip unparseable items).
    let items: Vec<SnItem> = backup
        .items
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect();

    // Build note→tags map from Tag items (inverted references).
    let tag_map = build_tag_map(&items);

    // Collect existing note ids already in the vault for dedupe reporting.
    let mut seen: HashSet<String> = HashSet::new();
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    for key in stream.partitions().unwrap_or_default() {
        for n in stream.read::<Note>(&key).unwrap_or_default() {
            if !n.id.is_empty() {
                seen.insert(n.id.clone());
            }
        }
    }

    let total = items.len();
    let mut contract: Vec<Note> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut imported = 0u64;
    let mut duplicates = 0u64;
    let mut skipped = 0u64;

    for (idx, item) in items.iter().enumerate() {
        // Only process Note items; skip Tags, extensions, etc.
        if item.content_type != "Note" {
            continue;
        }

        let tags = tag_map.get(&item.uuid).map(|v| v.as_slice()).unwrap_or(&[]);
        let Some((note, raw)) = note_to_row(item, tags) else {
            // Count skipped only for non-deleted items (deleted tombstones
            // with missing timestamps are an expected edge case).
            if !item.deleted {
                skipped += 1;
            }
            // Still write the raw row for tombstones so the raw layer is a
            // true full-fidelity mirror even for deleted items.
            // note_to_row returned None because uuid was empty — nothing to
            // write in that case, so skip entirely.
            continue;
        };

        // Always include in the raw layer (full-fidelity mirror of all Note
        // items, including server-tombstones, for archival completeness).
        raw_rows.push(raw);

        // Exclude server-tombstones (deleted:true) from the contract layer.
        // Note: content.trashed notes ARE included (trashed != deleted) —
        // the trashed flag is set on note.trashed for read-time filtering.
        if item.deleted {
            continue;
        }

        // Dedupe: if the uuid was already in the vault this is an update
        // (upsert replaces the existing line) — count it as duplicate for the
        // headline but still write it (the body may have changed).
        if seen.contains(&note.id) {
            duplicates += 1;
        } else {
            imported += 1;
        }

        contract.push(note);

        if (idx + 1) % 50 == 0 {
            progress(ImportProgress {
                records: imported,
                percent: (idx as f32 + 1.0) / total.max(1) as f32 * 100.0,
            });
        }
    }

    if !contract.is_empty() {
        vault.upsert_standard_notes(&contract)?;
    }
    if !raw_rows.is_empty() {
        vault.upsert_standard_notes_raw(&raw_rows)?;
    }

    progress(ImportProgress { records: imported + duplicates, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} notes imported, {duplicates} updated{}",
            if skipped > 0 {
                format!(", {skipped} skipped (missing uuid or timestamp)")
            } else {
                String::new()
            }
        ),
        counts: [("imported", imported), ("updated", duplicates), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers

impl Vault {
    /// Upsert contract notes into `notes/standard-notes/YYYY-MM.jsonl`,
    /// partitioned by `created` month, deduped by `id`.
    pub fn upsert_standard_notes(&self, notes: &[Note]) -> Result<()> {
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

    /// Upsert raw rows into `notes/standard-notes/raw/YYYY-MM.jsonl`,
    /// partitioned by `_created`, deduped by `id`.
    pub fn upsert_standard_notes_raw(&self, rows: &[Value]) -> Result<()> {
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
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-sn-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A minimal decrypted Standard Notes backup JSON with two live notes, one
    // trashed note, one server-tombstone, and two tags.
    // Tag A contains note 1; Tag B contains both live notes.
    // note-1 was created 2024-03, note-2 in 2024-06, trashed-note in 2024-05.
    // JSON uses r##"..."## to avoid premature termination by "# inside markdown
    // note bodies (e.g. "# Garden planting plan" contains the "# sequence).
    const BACKUP_JSON: &str = r##"{
  "version": "004",
  "items": [
    {
      "uuid": "note-uuid-1111-1111-1111",
      "content_type": "Note",
      "created_at": "2024-03-14T09:12:00.000Z",
      "updated_at": "2024-04-02T18:40:00.000Z",
      "deleted": false,
      "content": {
        "title": "Garden planting plan",
        "text": "# Garden planting plan\n\n- Tomatoes in the south bed",
        "references": [],
        "noteType": "plain-text"
      }
    },
    {
      "uuid": "note-uuid-2222-2222-2222",
      "content_type": "Note",
      "created_at": "2024-06-01T10:00:00.000Z",
      "updated_at": "2024-06-01T10:00:00.000Z",
      "deleted": false,
      "content": {
        "title": "Super editor note",
        "text": "<p>Rich text content</p>",
        "references": [],
        "noteType": "super",
        "editorIdentifier": "org.standardnotes.super-editor"
      }
    },
    {
      "uuid": "note-uuid-4444-trashed",
      "content_type": "Note",
      "created_at": "2024-05-10T08:00:00.000Z",
      "updated_at": "2024-05-12T09:00:00.000Z",
      "deleted": false,
      "content": {
        "title": "Trashed note",
        "text": "user moved this to trash",
        "trashed": true,
        "pinned": false
      }
    },
    {
      "uuid": "note-uuid-5555-pinned",
      "content_type": "Note",
      "created_at": "2024-07-15T12:00:00.000Z",
      "updated_at": "2024-07-16T12:00:00.000Z",
      "deleted": false,
      "content": {
        "title": "Pinned and archived",
        "text": "this note is both pinned and archived",
        "pinned": true,
        "archived": true
      }
    },
    {
      "uuid": "note-uuid-3333-del",
      "content_type": "Note",
      "created_at": "2024-05-01T10:00:00.000Z",
      "updated_at": "2024-05-01T10:00:00.000Z",
      "deleted": true,
      "content": {
        "title": "Server-tombstone note",
        "text": "should not appear in contract layer"
      }
    },
    {
      "uuid": "tag-uuid-aaaa-gdn",
      "content_type": "Tag",
      "created_at": "2024-01-01T00:00:00.000Z",
      "updated_at": "2024-01-01T00:00:00.000Z",
      "deleted": false,
      "content": {
        "title": "garden",
        "references": [
          { "uuid": "note-uuid-1111-1111-1111", "content_type": "Note" }
        ]
      }
    },
    {
      "uuid": "tag-uuid-bbbb-all",
      "content_type": "Tag",
      "created_at": "2024-01-01T00:00:00.000Z",
      "updated_at": "2024-01-01T00:00:00.000Z",
      "deleted": false,
      "content": {
        "title": "all-notes",
        "references": [
          { "uuid": "note-uuid-1111-1111-1111", "content_type": "Note" },
          { "uuid": "note-uuid-2222-2222-2222", "content_type": "Note" }
        ]
      }
    },
    {
      "uuid": "ext-uuid-cccc-ext",
      "content_type": "SN|Component",
      "created_at": "2024-01-01T00:00:00.000Z",
      "updated_at": "2024-01-01T00:00:00.000Z",
      "deleted": false,
      "content": { "name": "some component" }
    }
  ]
}"##;

    fn import_json(v: &Vault, json: &str) -> ImportOutcome {
        let path = v.root().join("backup.txt");
        fs::write(&path, json).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_notes_with_tags_and_skips_deleted_and_extensions() {
        let v = temp_vault("basic");
        let out = import_json(&v, BACKUP_JSON);

        // 4 live contract notes: note-1, note-2, trashed-4444, pinned-5555.
        // Tombstone (deleted:true) excluded from contract; tags/extensions ignored.
        assert_eq!(out.counts["imported"], 4, "four non-tombstone notes");
        assert_eq!(out.counts["updated"], 0);
        assert_eq!(out.counts["skipped"], 0);
        assert!(out.headline.contains("4 notes imported"));

        // March partition: note 1, tagged [all-notes, garden] (sorted).
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-03").unwrap();
        assert_eq!(mar.len(), 1);
        let n1 = &mar[0];
        assert_eq!(n1.source, "standard-notes");
        assert_eq!(n1.id, "note-uuid-1111-1111-1111");
        assert_eq!(n1.title, "Garden planting plan");
        assert!(n1.body.contains("Tomatoes"));
        assert_eq!(n1.tags, vec!["all-notes", "garden"], "tags sorted, both tags");
        assert!(n1.created.starts_with("2024-03-14"), "created month correct");
        assert!(n1.modified.starts_with("2024-04-02"), "modified preserved");
        // Plain-text note has no extra note_type.
        assert!(n1.extra.get("note_type").is_none(), "plain-text: no note_type in extra");
        // No state flags set on this note.
        assert_eq!(n1.trashed, None);
        assert_eq!(n1.pinned, None);
        assert_eq!(n1.archived, None);

        // June partition: note 2, Super editor type.
        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-06").unwrap();
        assert_eq!(jun.len(), 1);
        let n2 = &jun[0];
        assert_eq!(n2.id, "note-uuid-2222-2222-2222");
        assert_eq!(n2.extra.get("note_type").and_then(|v| v.as_str()), Some("super"),
            "extension note type in extra");
        assert_eq!(n2.extra.get("editor_identifier").and_then(|v| v.as_str()),
            Some("org.standardnotes.super-editor"));
        assert_eq!(n2.tags, vec!["all-notes"]);

        // May partition: trashed note (content.trashed:true) IS in contract layer
        // with trashed==Some(true); tombstone (deleted:true) is NOT.
        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-05").unwrap();
        assert_eq!(may.len(), 1, "trashed note included in contract");
        let n_trashed = &may[0];
        assert_eq!(n_trashed.id, "note-uuid-4444-trashed");
        assert_eq!(n_trashed.trashed, Some(true), "content.trashed propagated to note.trashed");
        assert_eq!(n_trashed.pinned, Some(false), "content.pinned:false preserved");

        // July partition: pinned + archived note.
        let jul = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-07").unwrap();
        assert_eq!(jul.len(), 1);
        let n_pinned = &jul[0];
        assert_eq!(n_pinned.id, "note-uuid-5555-pinned");
        assert_eq!(n_pinned.pinned, Some(true), "content.pinned propagated");
        assert_eq!(n_pinned.archived, Some(true), "content.archived propagated");

        // Raw layer: includes tombstone (deleted:true) for full-fidelity mirror.
        let raw_mar = v.stream(RAW_DIR, Partition::Month).read::<Value>("2024-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        let r = &raw_mar[0];
        assert_eq!(r["uuid"], "note-uuid-1111-1111-1111");
        assert_eq!(r["content_type"], "Note");
        assert!(r["content"]["title"].as_str().unwrap().contains("Garden"));
        assert!(r["_created"].as_str().unwrap().starts_with("2024-03"));
        assert_eq!(r["tags"][0], "all-notes", "tags in raw row");

        // Raw layer for May: both the trashed note AND the tombstone are present.
        let raw_may = v.stream(RAW_DIR, Partition::Month).read::<Value>("2024-05").unwrap();
        assert_eq!(raw_may.len(), 2, "raw layer captures both trashed and tombstone notes");
        let tombstone_raw = raw_may.iter().find(|v| v["uuid"] == "note-uuid-3333-del");
        assert!(tombstone_raw.is_some(), "tombstone in raw layer");
        assert_eq!(tombstone_raw.unwrap()["deleted"], true, "deleted flag preserved in raw");
    }

    #[test]
    fn reimport_upserts_updated_notes_never_duplicates() {
        let v = temp_vault("rerun");
        import_json(&v, BACKUP_JSON);

        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-03").unwrap();
        assert_eq!(mar[0].body, "# Garden planting plan\n\n- Tomatoes in the south bed");

        // Re-import with an edited note 1 (same uuid, new body text).
        // `\n` in JSON text is a JSON escape so must be \\n in the Rust replacement string.
        let updated = BACKUP_JSON.replace(
            "- Tomatoes in the south bed",
            "- Tomatoes in the south bed\\n- Basil between rows",
        );
        let out2 = import_json(&v, &updated);
        assert_eq!(out2.counts["imported"], 0, "all uuids already seen → updated");
        assert_eq!(out2.counts["updated"], 4, "four non-tombstone notes updated");

        let mar2 = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-03").unwrap();
        assert_eq!(mar2.len(), 1, "upsert: exactly one row, not two");
        assert!(mar2[0].body.contains("Basil between rows"), "line replaced in place");
    }

    #[test]
    fn imports_bare_json_file_directly() {
        let v = temp_vault("bare");
        // A bare .json file (not a ZIP) is accepted as-is.
        let path = v.root().join("backup.json");
        fs::write(&path, BACKUP_JSON).unwrap();
        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 4);
    }

    #[test]
    fn imports_from_zip() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("standard-notes-backup.zip");
        {
            let f = fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            // Root-level backup file (the canonical one).
            w.start_file("Standard Notes Backup and Import File.txt", opts).unwrap();
            w.write_all(BACKUP_JSON.as_bytes()).unwrap();
            // Individual plain-text note files (decoys — must not be parsed as backup).
            w.start_file("Garden planting plan.txt", opts).unwrap();
            w.write_all(b"# Garden planting plan\n\n- Tomatoes in the south bed").unwrap();
            w.finish().unwrap();
        }
        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["imported"], 4);
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-03").unwrap();
        assert_eq!(mar[0].title, "Garden planting plan");
    }

    #[test]
    fn missing_uuid_is_skipped() {
        let v = temp_vault("nouuid");
        let json = r#"{"items":[
            {"content_type":"Note","created_at":"2024-03-01T00:00:00.000Z","updated_at":"2024-03-01T00:00:00.000Z","deleted":false,"content":{"title":"No uuid","text":"body"}},
            {"uuid":"valid-uuid-1","content_type":"Note","created_at":"2024-03-02T00:00:00.000Z","updated_at":"2024-03-02T00:00:00.000Z","deleted":false,"content":{"title":"Has uuid","text":"ok"}}
        ]}"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1, "note without uuid skipped");
        assert_eq!(out.counts["skipped"], 1);
    }

    #[test]
    fn serde_back_compat_old_lines_still_deserialize() {
        // A Note line from a hypothetical older/sparser writer must still
        // deserialize — additive schema evolution.
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2024-03.jsonl")),
            "{\"source\":\"standard-notes\",\"id\":\"old-uuid-1\"}\n\
             {\"source\":\"standard-notes\",\"id\":\"old-uuid-2\",\"body\":\"b\",\"created\":\"2024-03-01T00:00:00-07:00\"}\n",
        ).unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-03").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "old-uuid-1");
        assert_eq!(rows[1].body, "b");
    }

    #[test]
    fn trashed_note_in_contract_with_flag_set() {
        // A note with content.trashed:true (user-trash, not server-tombstone)
        // must appear in the contract layer with note.trashed == Some(true).
        // Use mid-month noon UTC to avoid local-timezone month-boundary shifts.
        let v = temp_vault("trashed");
        let json = r#"{"items":[
            {"uuid":"trashed-uuid-1","content_type":"Note",
             "created_at":"2024-09-10T12:00:00.000Z","updated_at":"2024-09-11T12:00:00.000Z",
             "deleted":false,
             "content":{"title":"Trash me","text":"body","trashed":true}},
            {"uuid":"pinned-uuid-2","content_type":"Note",
             "created_at":"2024-09-15T12:00:00.000Z","updated_at":"2024-09-15T12:00:00.000Z",
             "deleted":false,
             "content":{"title":"Pin me","text":"pinned","pinned":true,"archived":false}}
        ]}"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 2);

        let sep = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-09").unwrap();
        assert_eq!(sep.len(), 2);

        let trashed = sep.iter().find(|n| n.id == "trashed-uuid-1").unwrap();
        assert_eq!(trashed.trashed, Some(true), "content.trashed:true -> note.trashed");
        assert_eq!(trashed.pinned, None, "absent pinned -> None");

        let pinned = sep.iter().find(|n| n.id == "pinned-uuid-2").unwrap();
        assert_eq!(pinned.pinned, Some(true), "content.pinned:true -> note.pinned");
        assert_eq!(pinned.archived, Some(false), "content.archived:false preserved as Some(false)");
        assert_eq!(pinned.trashed, None, "absent trashed -> None");
    }

    #[test]
    fn tombstone_note_excluded_from_contract_but_in_raw() {
        // A note with deleted:true (server tombstone) must NOT appear in the
        // contract layer but MUST appear in the raw layer.
        // Use mid-month noon UTC to avoid local-timezone month-boundary shifts.
        let v = temp_vault("tombstone");
        let json = r#"{"items":[
            {"uuid":"live-uuid-1","content_type":"Note",
             "created_at":"2024-08-10T12:00:00.000Z","updated_at":"2024-08-10T12:00:00.000Z",
             "deleted":false,"content":{"title":"Live","text":"body"}},
            {"uuid":"dead-uuid-2","content_type":"Note",
             "created_at":"2024-08-15T12:00:00.000Z","updated_at":"2024-08-15T12:00:00.000Z",
             "deleted":true,"content":{"title":"Dead","text":"tombstone body"}}
        ]}"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1, "only live note in contract");

        let aug = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-08").unwrap();
        assert_eq!(aug.len(), 1, "contract layer excludes tombstone");
        assert_eq!(aug[0].id, "live-uuid-1");

        let raw_aug = v.stream(RAW_DIR, Partition::Month).read::<Value>("2024-08").unwrap();
        assert_eq!(raw_aug.len(), 2, "raw layer includes tombstone");
        let dead = raw_aug.iter().find(|v| v["uuid"] == "dead-uuid-2").unwrap();
        assert_eq!(dead["deleted"], true);
    }

    #[test]
    fn created_at_fallback_to_updated_at() {
        // A note with missing/empty created_at but valid updated_at must not
        // be dropped; it falls back to updated_at for the created timestamp.
        let v = temp_vault("created-fallback");
        let json = r#"{"items":[
            {"uuid":"fallback-uuid-1","content_type":"Note",
             "created_at":"","updated_at":"2024-10-15T12:00:00.000Z",
             "deleted":false,"content":{"title":"Fallback","text":"body"}}
        ]}"#;
        let out = import_json(&v, json);
        assert_eq!(out.counts["imported"], 1, "note with empty created_at saved via fallback");
        assert_eq!(out.counts["skipped"], 0);

        let oct = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2024-10").unwrap();
        assert_eq!(oct.len(), 1);
        assert!(oct[0].created.starts_with("2024-10-15"), "created falls back to updated_at");
    }

    #[test]
    fn tag_map_inverts_references_correctly() {
        // Confirm the tag-inversion logic in isolation.
        let items_json = r#"[
            {"uuid":"n1","content_type":"Note","created_at":"","updated_at":"","deleted":false,"content":{}},
            {"uuid":"n2","content_type":"Note","created_at":"","updated_at":"","deleted":false,"content":{}},
            {"uuid":"t1","content_type":"Tag","created_at":"","updated_at":"","deleted":false,
             "content":{"title":"alpha","references":[{"uuid":"n1","content_type":"Note"},{"uuid":"n2","content_type":"Note"}]}},
            {"uuid":"t2","content_type":"Tag","created_at":"","updated_at":"","deleted":false,
             "content":{"title":"beta","references":[{"uuid":"n2","content_type":"Note"}]}}
        ]"#;
        let items: Vec<SnItem> = serde_json::from_str::<Vec<Value>>(items_json)
            .unwrap()
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        let map = build_tag_map(&items);
        assert_eq!(map["n1"], vec!["alpha"], "n1 only in alpha");
        let mut n2_tags = map["n2"].clone();
        n2_tags.sort();
        assert_eq!(n2_tags, vec!["alpha", "beta"], "n2 in both tags, sorted");
    }
}
