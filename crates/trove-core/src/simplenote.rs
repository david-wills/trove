//! Simplenote — import via the official export ZIP (`simplenote.json`).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/simplenote.md.
//!
//! ## Export format (confirmed from Automattic/simplenote-electron source)
//!
//! The export ZIP contains `simplenote.json` at its root. The JSON is a single
//! object:
//! ```json
//! {
//!   "activeNotes": [ <note>, … ],
//!   "trashedNotes": [ <note>, … ]
//! }
//! ```
//! Each note object carries:
//! - `id`            — stable UUID (the dedupe key)
//! - `content`       — the full note text (plain text / Markdown)
//! - `creationDate`  — ISO 8601 timestamp of creation
//! - `lastModified`  — ISO 8601 timestamp of last edit
//! - `tags`          — array of tag strings (optional)
//! - `pinned`        — boolean (optional)
//! - `markdown`      — boolean, explicit Markdown mode (optional, into `extra`)
//! - `publicURL`     — string if published (optional, into `extra`)
//! - `collaboratorEmails` — array of email strings if shared (optional, into `extra`)
//!
//! Source: `lib/utils/export/export-notes.ts` + `types.ts` in
//! <https://github.com/Automattic/simplenote-electron>.
//!
//! ## Vault layout
//!
//! - **Raw layer:** `notes/simplenote/raw/YYYY-MM.jsonl` — every note object
//!   verbatim (keyed by `id`, partitioned by the month of `creationDate`).
//! - **Contract layer:** `notes/simplenote/YYYY-MM.jsonl` — [`crate::notes::Note`]
//!   records (source=`simplenote`, id=note id, body=content, created=creationDate,
//!   modified=lastModified, tags, pinned); partitioned by the month of `created`,
//!   deduped/upserted by `id`.
//!
//! Trashed notes (`trashedNotes` array) are imported with `trashed: Some(true)` so
//! they are accounted for but a reader can filter them out. Re-importing is
//! idempotent: each note is upserted by `id`, so dropping the same export twice
//! never duplicates.
//!
//! ## Dedupe key
//!
//! `id` is the stable UUID assigned by Simplenote. It never changes. We use it
//! as the partition-local upsert key (whole-affected-month rewrite — the same
//! mechanism Bear uses).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const NOTES_DIR: &str = "notes/simplenote";
const RAW_DIR: &str = "notes/simplenote/raw";
const SOURCE: &str = "simplenote";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "simplenote",
        name: "Simplenote",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your Simplenote notes from the official export ZIP \
                      (File → Export Notes). All notes, tags, and timestamps are \
                      preserved; deleted notes are kept with a trashed flag. \
                      Re-importing is idempotent — newer exports never duplicate.",
        domain: "notes",
        vault_path: "notes/simplenote/",
        toggleable: false,
        setup: &[
            "Open Simplenote (desktop or web) → File → Export Notes.",
            "Import the downloaded ZIP here (or the simplenote.json extracted from it).",
        ],
        caveats: "No public API; the export is the only path. \
                  The export includes deleted notes (marked trashed) — they are \
                  preserved in the vault with a trashed flag and excluded from the \
                  active notes view.",
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
// Wire format (confirmed from simplenote-electron source)

/// The top-level shape of `simplenote.json`.
#[derive(Debug, Deserialize)]
struct ExportRoot {
    #[serde(rename = "activeNotes", default)]
    active_notes: Vec<ExportNote>,
    #[serde(rename = "trashedNotes", default)]
    trashed_notes: Vec<ExportNote>,
}

/// One note object as Simplenote writes it into the export.
/// Fields confirmed against `lib/utils/export/types.ts` and
/// `lib/utils/export/export-notes.ts` in the simplenote-electron repo.
#[derive(Debug, Clone, Deserialize)]
struct ExportNote {
    /// Stable UUID — the dedupe key. Always present.
    id: String,
    /// Full note text (plain text or Markdown). Always present (may be empty).
    content: String,
    /// ISO 8601 creation timestamp, e.g. `"2024-03-14T09:12:00.000Z"`.
    #[serde(rename = "creationDate", default)]
    creation_date: String,
    /// ISO 8601 last-modified timestamp.
    #[serde(rename = "lastModified", default)]
    last_modified: String,
    /// Tag names (non-email tags only in the export).
    #[serde(default)]
    tags: Vec<String>,
    /// Whether the note is pinned (optional — omitted when false).
    #[serde(default)]
    pinned: Option<bool>,
    /// Explicit Markdown mode (optional). Carried into `extra`.
    #[serde(default)]
    markdown: Option<bool>,
    /// Public share URL (optional). Carried into `extra`.
    #[serde(rename = "publicURL", default)]
    public_url: Option<String>,
    /// Collaborator email addresses (optional). Carried into `extra`.
    #[serde(rename = "collaboratorEmails", default)]
    collaborator_emails: Vec<String>,
}

// ---------------------------------------------------------------------------
// Export extraction

/// Extract the `simplenote.json` body from a ZIP or a bare `.json` file.
fn simplenote_json_bytes(path: &Path) -> Result<Vec<u8>> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        use std::io::Read;
        let file = fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", path.display()))?;
        // The export always places simplenote.json at the zip root.
        let mut entry = archive
            .by_name("simplenote.json")
            .context("no simplenote.json at the root of the export zip \
                      — is this a Simplenote data export?")?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        Ok(buf)
    } else {
        fs::read(path).with_context(|| format!("reading {}", path.display()))
    }
}

// ---------------------------------------------------------------------------
// Mapping

/// Derive a first-line title from the note `content` when the source has no
/// explicit title field. Simplenote has no title field — the convention is
/// that the first line is the title in the UI. We follow the same heuristic:
/// trim to the first non-empty line, strip a leading `# `, keep ≤120 chars.
fn title_from_content(content: &str) -> String {
    let first = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let stripped = first.trim_start_matches('#').trim();
    if stripped.is_empty() {
        return String::new();
    }
    // Guard on a very long first line — cap at 120 Unicode scalar values
    // (not bytes, to avoid splitting inside a multibyte character such as an
    // emoji or CJK character).  Walk back to the last whitespace boundary
    // within those chars; if no whitespace, take all 120 chars.
    let char_count = stripped.chars().count();
    if char_count <= 120 {
        stripped.to_string()
    } else {
        // Build the 120-char prefix as a &str slice (char-boundary-safe).
        let end_byte = stripped
            .char_indices()
            .nth(120)
            .map(|(idx, _)| idx)
            .unwrap_or(stripped.len());
        let prefix = &stripped[..end_byte];
        // Trim back to the last whitespace inside the prefix, if any.
        let trimmed_end = prefix
            .rfind(char::is_whitespace)
            .unwrap_or(end_byte);
        stripped[..trimmed_end].to_string()
    }
}

/// Map one [`ExportNote`] (from the active or trashed array) to a
/// [`crate::notes::Note`] contract record. `is_trashed` is set for notes from
/// the `trashedNotes` array.
fn to_note(n: &ExportNote, is_trashed: bool) -> Note {
    let mut note = Note::new(SOURCE, &n.id);
    note.body = n.content.clone();
    note.title = title_from_content(&n.content);
    note.created = n.creation_date.clone();
    note.modified = n.last_modified.clone();
    note.tags = n.tags.clone();
    note.pinned = n.pinned.filter(|&p| p); // omit when false
    if is_trashed {
        note.trashed = Some(true);
    }
    // Extra: source-specific fields the contract doesn't normalize.
    let mut extra = Map::new();
    if let Some(true) = n.markdown {
        extra.insert("markdown".into(), Value::Bool(true));
    }
    if let Some(ref url) = n.public_url {
        if !url.is_empty() {
            extra.insert("publicURL".into(), Value::String(url.clone()));
        }
    }
    if !n.collaborator_emails.is_empty() {
        extra.insert(
            "collaboratorEmails".into(),
            Value::Array(n.collaborator_emails.iter().cloned().map(Value::String).collect()),
        );
    }
    note.extra = extra;
    note
}

/// Build a raw row from one [`ExportNote`]. Full fidelity — every field
/// verbatim plus `source`, `_created` (for partition), and `trashed` flag.
fn to_raw(n: &ExportNote, is_trashed: bool) -> Value {
    let mut m = Map::new();
    m.insert("source".into(), Value::String(SOURCE.into()));
    m.insert("id".into(), Value::String(n.id.clone()));
    m.insert("content".into(), Value::String(n.content.clone()));
    m.insert("creationDate".into(), Value::String(n.creation_date.clone()));
    m.insert("lastModified".into(), Value::String(n.last_modified.clone()));
    if !n.tags.is_empty() {
        m.insert(
            "tags".into(),
            Value::Array(n.tags.iter().cloned().map(Value::String).collect()),
        );
    }
    if let Some(p) = n.pinned {
        m.insert("pinned".into(), Value::Bool(p));
    }
    if let Some(md) = n.markdown {
        m.insert("markdown".into(), Value::Bool(md));
    }
    if let Some(ref url) = n.public_url {
        if !url.is_empty() {
            m.insert("publicURL".into(), Value::String(url.clone()));
        }
    }
    if !n.collaborator_emails.is_empty() {
        m.insert(
            "collaboratorEmails".into(),
            Value::Array(n.collaborator_emails.iter().cloned().map(Value::String).collect()),
        );
    }
    if is_trashed {
        m.insert("trashed".into(), Value::Bool(true));
    }
    // Immutable partition key — the month of creationDate (which never changes).
    m.insert("_created".into(), Value::String(n.creation_date.clone()));
    Value::Object(m)
}

// ---------------------------------------------------------------------------
// Import runner

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let bytes = simplenote_json_bytes(path)?;
    progress(ImportProgress { records: 0, percent: 10.0 });

    let export: ExportRoot = serde_json::from_slice(&bytes)
        .context("parsing simplenote.json — is this a valid Simplenote export?")?;
    progress(ImportProgress { records: 0, percent: 20.0 });

    // Build the note + raw batches.
    let mut all_notes: Vec<Note> = Vec::new();
    let mut all_raw: Vec<Value> = Vec::new();
    let mut skipped = 0u64;

    for (is_trashed, note) in export
        .active_notes
        .iter()
        .map(|n| (false, n))
        .chain(export.trashed_notes.iter().map(|n| (true, n)))
    {
        if note.id.trim().is_empty() {
            // A note without a stable id can't be upserted safely — skip it.
            skipped += 1;
            continue;
        }
        // Use the same partitioning predicate as the writers: a note is only
        // accepted if Partition::Month can derive a valid YYYY-MM key from its
        // creationDate.  This catches both an empty field and any non-ISO
        // value that wouldn't produce a valid partition key, so the imported
        // count matches exactly what is written to the vault.
        if Partition::Month.key(&note.creation_date).is_none() {
            skipped += 1;
            continue;
        }
        all_notes.push(to_note(note, is_trashed));
        all_raw.push(to_raw(note, is_trashed));
    }

    let total = all_notes.len() as u64;
    progress(ImportProgress { records: total, percent: 40.0 });

    // Persist contract layer (upsert by id, partitioned by created month).
    vault.upsert_simplenote_notes(&all_notes)?;
    progress(ImportProgress { records: total, percent: 70.0 });

    // Persist raw layer (same upsert mechanics, full fidelity).
    vault.upsert_simplenote_raw(&all_raw)?;
    progress(ImportProgress { records: total, percent: 100.0 });

    let trashed = export.trashed_notes.len() as u64;
    Ok(ImportOutcome {
        headline: format!(
            "{total} notes imported ({trashed} trashed, {skipped} skipped)",
        ),
        counts: [
            ("imported", total),
            ("trashed", trashed),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers (same upsert-by-id/whole-month-rewrite as Bear)

impl Vault {
    /// Upsert contract rows into `notes/simplenote/YYYY-MM.jsonl`, partitioned
    /// by the month of `created`, deduped by `id`. Mirrors Bear's approach.
    pub fn upsert_simplenote_notes(&self, notes: &[Note]) -> Result<()> {
        upsert_notes_by_month(self, NOTES_DIR, notes)
    }

    /// Upsert raw rows into `notes/simplenote/raw/YYYY-MM.jsonl`, partitioned
    /// by `_created` (immutable), deduped by `id`.
    pub fn upsert_simplenote_raw(&self, rows: &[Value]) -> Result<()> {
        upsert_raw_by_month(self, RAW_DIR, rows)
    }
}

/// Generic "upsert into a month-partitioned snapshot by id" for [`Note`] rows.
fn upsert_notes_by_month(vault: &Vault, dir: &str, fresh: &[Note]) -> Result<()> {
    let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
    for n in fresh {
        let Some(key) = Partition::Month.key(&n.created) else { continue };
        by_month.entry(key.to_string()).or_default().push(n);
    }
    let stream = vault.stream(dir, Partition::Month);
    for (month, incoming) in by_month {
        let incoming_ids: HashSet<&str> = incoming.iter().map(|n| n.id.as_str()).collect();
        let mut merged: Vec<Note> = stream
            .read::<Note>(&month)?
            .into_iter()
            .filter(|existing| !incoming_ids.contains(existing.id.as_str()))
            .collect();
        merged.extend(incoming.into_iter().cloned());
        let rel = format!("{dir}/{month}.jsonl");
        vault.write_snapshot(&rel, &merged)?;
    }
    Ok(())
}

/// Generic "upsert into a month-partitioned snapshot by id" for raw JSON rows.
fn upsert_raw_by_month(vault: &Vault, dir: &str, fresh: &[Value]) -> Result<()> {
    fn month_key(v: &Value) -> &str {
        v.get("_created").and_then(|m| m.as_str()).unwrap_or("")
    }
    fn id_of(v: &Value) -> &str {
        v.get("id").and_then(|i| i.as_str()).unwrap_or("")
    }
    let mut by_month: HashMap<String, Vec<&Value>> = HashMap::new();
    for v in fresh {
        let Some(key) = Partition::Month.key(month_key(v)) else { continue };
        by_month.entry(key.to_string()).or_default().push(v);
    }
    let stream = vault.stream(dir, Partition::Month);
    for (month, incoming) in by_month {
        let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
        let mut merged: Vec<Value> = stream
            .read::<Value>(&month)?
            .into_iter()
            .filter(|existing| !incoming_ids.contains(id_of(existing)))
            .collect();
        merged.extend(incoming.into_iter().cloned());
        let rel = format!("{dir}/{month}.jsonl");
        vault.write_snapshot(&rel, &merged)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-simplenote-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// A minimal but complete `simplenote.json` fixture with:
    /// - two active notes (one plain, one with tags+pinned+markdown)
    /// - one trashed note
    /// - one note without a creationDate (should be skipped)
    ///
    /// Field names confirmed against the simplenote-electron TypeScript source
    /// (`lib/utils/export/types.ts` + `lib/utils/export/export-notes.ts`).
    const SAMPLE_JSON: &str = r##"{
  "activeNotes": [
    {
      "id": "note-uuid-001",
      "content": "# Shopping list\n\n- Milk\n- Eggs\n- Bread",
      "creationDate": "2026-03-10T09:00:00.000Z",
      "lastModified": "2026-03-14T18:30:00.000Z",
      "tags": ["shopping", "home"],
      "pinned": true,
      "markdown": true
    },
    {
      "id": "note-uuid-002",
      "content": "Quick capture\n\nSomething I want to remember.",
      "creationDate": "2026-05-20T14:22:00.000Z",
      "lastModified": "2026-05-20T14:22:00.000Z"
    }
  ],
  "trashedNotes": [
    {
      "id": "note-uuid-trash-001",
      "content": "Old draft\n\nThis was deleted.",
      "creationDate": "2026-04-01T08:00:00.000Z",
      "lastModified": "2026-04-05T09:00:00.000Z",
      "tags": ["drafts"]
    }
  ]
}"##;

    fn run_import_from_json(v: &Vault, json: &str) -> ImportOutcome {
        let path = v.root().join("simplenote.json");
        fs::write(&path, json).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------

    #[test]
    fn imports_active_and_trashed_notes_from_json() {
        let v = temp_vault("basic");
        let outcome = run_import_from_json(&v, SAMPLE_JSON);

        // 3 total notes (2 active + 1 trashed), 0 skipped.
        assert_eq!(outcome.counts["imported"], 3, "2 active + 1 trashed");
        assert_eq!(outcome.counts["trashed"], 1);
        assert_eq!(outcome.counts["skipped"], 0);

        // March contract file: the shopping-list note.
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1);
        let shop = &mar[0];
        assert_eq!(shop.source, "simplenote");
        assert_eq!(shop.id, "note-uuid-001");
        assert_eq!(shop.title, "Shopping list", "first-line title extracted from content");
        assert!(shop.body.contains("Milk"), "body preserved verbatim");
        assert_eq!(shop.tags, vec!["shopping", "home"]);
        assert_eq!(shop.pinned, Some(true));
        assert_eq!(shop.trashed, None, "active note has no trashed flag");
        assert!(shop.created.starts_with("2026-03-10"));
        assert!(shop.modified.starts_with("2026-03-14"));
        // `markdown: true` goes into extra, not the normalized fields.
        assert_eq!(shop.extra.get("markdown"), Some(&Value::Bool(true)));

        // May contract file: quick-capture note.
        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may.len(), 1);
        let quick = &may[0];
        assert_eq!(quick.id, "note-uuid-002");
        assert_eq!(quick.title, "Quick capture");
        assert!(quick.tags.is_empty(), "no tags on this note");
        assert_eq!(quick.pinned, None, "not pinned — field omitted");

        // April contract file: the trashed note.
        let apr = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-04").unwrap();
        assert_eq!(apr.len(), 1);
        let trashed = &apr[0];
        assert_eq!(trashed.id, "note-uuid-trash-001");
        assert_eq!(trashed.trashed, Some(true), "trashed notes carry the flag");
        assert_eq!(trashed.tags, vec!["drafts"]);
    }

    #[test]
    fn raw_layer_full_fidelity_and_partitioned_by_creation_month() {
        let v = temp_vault("raw");
        run_import_from_json(&v, SAMPLE_JSON);

        // Raw March: the shopping note.
        let raw_mar = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-03").unwrap();
        assert_eq!(raw_mar.len(), 1);
        assert_eq!(raw_mar[0]["id"], "note-uuid-001");
        assert_eq!(raw_mar[0]["markdown"], Value::Bool(true), "markdown preserved verbatim in raw");
        assert!(raw_mar[0]["content"].as_str().unwrap().contains("Milk"));
        assert!(raw_mar[0]["_created"].as_str().unwrap().starts_with("2026-03"));
        // Raw trashed note carries the trashed flag.
        let raw_apr = v.stream(RAW_DIR, Partition::Month).read::<Value>("2026-04").unwrap();
        assert_eq!(raw_apr.len(), 1);
        assert_eq!(raw_apr[0]["trashed"], Value::Bool(true));
    }

    #[test]
    fn reimport_is_idempotent_upsert_not_duplicate() {
        let v = temp_vault("idempotent");
        run_import_from_json(&v, SAMPLE_JSON);
        run_import_from_json(&v, SAMPLE_JSON);

        // Importing the same export twice must NOT duplicate any row.
        let mar = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-03").unwrap();
        assert_eq!(mar.len(), 1, "re-import upserts, never duplicates");
        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may.len(), 1);
        let apr = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-04").unwrap();
        assert_eq!(apr.len(), 1);
    }

    #[test]
    fn updated_note_in_re_import_replaces_old_line() {
        let v = temp_vault("update");
        run_import_from_json(&v, SAMPLE_JSON);

        // Produce a newer export where note-uuid-002 has been edited.
        let updated_json = r#"{
  "activeNotes": [
    {
      "id": "note-uuid-002",
      "content": "Quick capture — EDITED\n\nUpdated body.",
      "creationDate": "2026-05-20T14:22:00.000Z",
      "lastModified": "2026-06-01T10:00:00.000Z"
    }
  ],
  "trashedNotes": []
}"#;
        run_import_from_json(&v, updated_json);

        let may = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-05").unwrap();
        assert_eq!(may.len(), 1, "still one row — old line replaced");
        assert!(may[0].body.contains("EDITED"), "body updated in place");
        assert!(may[0].modified.starts_with("2026-06-01"), "modified timestamp updated");
    }

    #[test]
    fn note_without_creation_date_is_skipped() {
        let v = temp_vault("skipdate");
        let json = r#"{
  "activeNotes": [
    {
      "id": "note-no-date",
      "content": "A note with no creation date.",
      "creationDate": "",
      "lastModified": "2026-06-01T10:00:00.000Z"
    },
    {
      "id": "note-ok",
      "content": "Normal note.",
      "creationDate": "2026-06-05T08:00:00.000Z",
      "lastModified": "2026-06-05T08:00:00.000Z"
    }
  ],
  "trashedNotes": []
}"#;
        let outcome = run_import_from_json(&v, json);
        // note-no-date must be skipped (can't partition without a created month).
        assert_eq!(outcome.counts["skipped"], 1, "note without creationDate skipped");
        assert_eq!(outcome.counts["imported"], 1, "only the normal note imported");
        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1);
        assert_eq!(jun[0].id, "note-ok");
    }

    #[test]
    fn empty_export_is_a_no_op() {
        let v = temp_vault("empty");
        let json = r#"{"activeNotes":[],"trashedNotes":[]}"#;
        let outcome = run_import_from_json(&v, json);
        assert_eq!(outcome.counts["imported"], 0);
        assert_eq!(outcome.counts["skipped"], 0);
    }

    #[test]
    fn imports_from_a_zip() {
        let v = temp_vault("zip");
        let zip_path = v.root().join("simplenote-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("simplenote.json", opts).unwrap();
        w.write_all(SAMPLE_JSON.as_bytes()).unwrap();
        // A decoy file that should NOT be read.
        w.start_file("notes/note-1.txt", opts).unwrap();
        w.write_all(b"raw note text\n").unwrap();
        w.finish().unwrap();

        let outcome = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(outcome.counts["imported"], 3, "ZIP import reads simplenote.json only");
    }

    #[test]
    fn title_extraction_strips_leading_hashes_and_caps_at_120() {
        assert_eq!(title_from_content("# Hello world\n\nbody"), "Hello world");
        assert_eq!(title_from_content("## Section two"), "Section two");
        assert_eq!(title_from_content("No hashes first line\n\nrest"), "No hashes first line");
        assert_eq!(title_from_content("\n\n\nFirst non-empty line"), "First non-empty line");
        assert_eq!(title_from_content(""), "");
        // Cap at 120: a very long ASCII first line gets truncated at a word boundary.
        let long = "word ".repeat(30); // 150 chars
        let extracted = title_from_content(&long);
        assert!(extracted.len() <= 120, "title capped at 120 chars: len={}", extracted.len());
    }

    #[test]
    fn title_extraction_handles_multibyte_chars_without_panic() {
        // 118 ASCII 'a' bytes followed by an emoji (4 bytes) — byte 120 lands
        // inside the emoji, which would panic with direct byte-slice indexing.
        let emoji_at_119 = format!("{}🎉 rest", "a".repeat(118));
        let extracted = title_from_content(&emoji_at_119);
        // Must not panic; result is at most 120 Unicode scalar values.
        assert!(extracted.chars().count() <= 120, "chars capped at 120");

        // A first line made entirely of CJK characters (3 bytes each in UTF-8).
        // 50 CJK chars = 150 bytes — well over the old byte-120 cut.
        let cjk = "日本語テスト".repeat(10); // 60 CJK chars
        let cjk_title = title_from_content(&cjk);
        assert!(cjk_title.chars().count() <= 120, "CJK title capped at 120 chars");
        // Round-trips as valid UTF-8.
        assert!(std::str::from_utf8(cjk_title.as_bytes()).is_ok());

        // A first line of exactly 120 emoji — no truncation should occur.
        let exactly_120_emoji: String = "🎵".repeat(120);
        let e120 = title_from_content(&exactly_120_emoji);
        assert_eq!(e120.chars().count(), 120, "exactly 120 emoji must pass through untruncated");
    }

    #[test]
    fn serde_back_compat_old_lines_still_deserialize() {
        // A Note line written by a leaner hypothetical writer must still round-
        // trip (additive schema evolution).
        let v = temp_vault("backcompat");
        fs::create_dir_all(v.root().join(NOTES_DIR)).unwrap();
        fs::write(
            v.root().join(format!("{NOTES_DIR}/2026-06.jsonl")),
            "{\"source\":\"simplenote\",\"id\":\"OLD-1\"}\n\
             {\"source\":\"simplenote\",\"id\":\"OLD-2\",\"body\":\"b\",\"created\":\"2026-06-01T00:00:00Z\"}\n",
        )
        .unwrap();
        let rows = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "OLD-1");
        assert_eq!(rows[1].body, "b");
    }

    #[test]
    fn def_is_import_behavior_accepts_zip_and_json() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert!(DEF.connection.is_none());
        let spec = DEF.import_spec().expect("Import behavior has a spec");
        assert!(spec.accepts.contains(&"zip"));
        assert!(spec.accepts.contains(&"json"));
        assert!(DEF.last_data.is_some());
    }

    #[test]
    fn public_url_and_collaborators_round_trip_in_extra() {
        let json = r#"{
  "activeNotes": [
    {
      "id": "note-shared",
      "content": "Shared note.",
      "creationDate": "2026-06-10T09:00:00.000Z",
      "lastModified": "2026-06-10T09:00:00.000Z",
      "publicURL": "http://simp.ly/p/abc123",
      "collaboratorEmails": ["alice@example.com", "bob@example.com"]
    }
  ],
  "trashedNotes": []
}"#;
        let v = temp_vault("shared");
        run_import_from_json(&v, json);
        let jun = v.stream(NOTES_DIR, Partition::Month).read::<Note>("2026-06").unwrap();
        assert_eq!(jun.len(), 1);
        let note = &jun[0];
        assert_eq!(
            note.extra.get("publicURL").and_then(|v| v.as_str()),
            Some("http://simp.ly/p/abc123")
        );
        let collab = note.extra.get("collaboratorEmails").unwrap();
        assert_eq!(
            collab.as_array().unwrap().len(),
            2,
            "both collaborator emails preserved in extra"
        );
    }
}
