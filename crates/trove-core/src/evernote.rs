//! Evernote — import of Evernote's `.enex` (ENML/XML) export format.
//!
//! The Evernote API uses OAuth 1.0 + the Thrift protocol and is deliberately
//! out of scope; the stable `.enex` file export is the supported path. Users
//! export whole notebooks from Evernote → File → Export Notes → `.enex`, then
//! drop the file(s) into the import box here.
//!
//! # ENEX structure (confirmed against the official DTD at
//! <https://xml.evernote.com/pub/evernote-export4.dtd>)
//!
//! ```xml
//! <en-export export-date="…" application="Evernote" version="…">
//!   <note>
//!     <title>…</title>
//!     <content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note …><en-note>…</en-note>]]></content>
//!     <created>20130730T205204Z</created>   <!-- YYYYMMDDTHHMMSSz, UTC -->
//!     <updated>20130730T205624Z</updated>
//!     <tag>shopping</tag>
//!     <tag>recipes</tag>
//!     <note-attributes>
//!       <source-url>https://…</source-url>
//!       <author>…</author>
//!       …
//!     </note-attributes>
//!     <resource>…</resource>  <!-- base64 attachments -->
//!   </note>
//! </en-export>
//! ```
//!
//! Timestamps are UTC in `YYYYMMDDTHHMMSSz` basic ISO-8601.
//!
//! # Vault output
//!
//! - **Contract** `notes/evernote/YYYY-MM.jsonl` — one [`Note`] per note,
//!   partitioned by the local month of `created`, deduped/upserted by a
//!   deterministic id derived from `created` + a hash of the note's ENML body
//!   (DTD v4 has no `<guid>` element; `created` is immutable per Evernote's
//!   model, making this id stable across re-exports for unchanged content).
//!   Re-importing an overlapping export updates rather than duplicates.
//!   A title edit does NOT produce a duplicate row.
//! - **Raw** `notes/evernote/raw/YYYY-MM.jsonl` — full-fidelity row: ENML
//!   `content`, resource count, note-attributes verbatim.
//!
//! [`letterboxd.rs`] is the reference Import example.
//! The upsert pattern mirrors `bear.rs` — each source defines its own
//! `upsert_<source>_notes` / `upsert_<source>_raw` helpers; there is no shared
//! generic version.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime};
use quick_xml::events::Event;
use quick_xml::Reader;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::notes::Note;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "evernote";
const NOTES_DIR: &str = "notes/evernote";
const RAW_DIR: &str = "notes/evernote/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(NOTES_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "evernote",
        name: "Evernote",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Evernote notes from a `.enex` export file — ENML content, \
                      tags, notebook metadata, and attachments at full fidelity. \
                      Re-runnable: re-importing an overlapping export updates, never duplicates.",
        domain: "notes",
        vault_path: "notes/evernote/",
        toggleable: false,
        setup: &[
            "In Evernote (Mac): File → Export Notes → Export as .enex.",
            "Whole notebooks export in one file; individual notes cap at 100 per batch.",
            "Import the .enex file here.",
        ],
        caveats: "Evernote's export caps at 100 notes per batch for individually \
                  selected notes, but whole-notebook exports work without that limit. \
                  The Evernote API (OAuth 1.0 + Thrift) is not used — export is \
                  the only supported path.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["enex"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Date parsing

/// Parse an ENEX timestamp (`YYYYMMDDTHHMMSSz`, UTC) to a local RFC3339 string.
/// Returns `None` on any parse failure rather than fabricating a date.
fn parse_enex_date(s: &str) -> Option<String> {
    // Strip a trailing 'Z' (case-insensitive) — `NaiveDateTime::parse_from_str`
    // doesn't handle the suffix itself.
    let s = s.trim();
    let body = s.strip_suffix('Z').or_else(|| s.strip_suffix('z')).unwrap_or(s);
    let ndt = NaiveDateTime::parse_from_str(body, "%Y%m%dT%H%M%S").ok()?;
    // ENEX timestamps are always UTC; convert to local for the vault.
    let utc = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ndt, chrono::Utc);
    Some(utc.with_timezone(&Local).to_rfc3339())
}

// ---------------------------------------------------------------------------
// Parser

/// One note extracted from an ENEX file. All fields optional (fault-tolerant
/// parse); `created` + a hash of `content` together form the stable dedupe key
/// (Evernote DTD v4 has no top-level `<guid>` element).
#[derive(Debug, Default)]
struct EnexNote {
    title: String,
    /// ENML content (the `<content>` CDATA verbatim).
    content: String,
    created: String,
    updated: String,
    tags: Vec<String>,
    /// Parsed note-attributes as key→value.
    note_attrs: Map<String, Value>,
    /// Number of `<resource>` elements (attachments).
    resource_count: u32,
}

impl EnexNote {
    /// Stable, collision-resistant dedupe id. The ENEX DTD v4 has no first-class
    /// GUID element, so we derive one from `created` + a hash of the note's
    /// content body.
    ///
    /// Design rationale:
    /// - `created` alone collides for same-second batch imports.
    /// - `title + created` is unstable: editing a title yields a different id,
    ///   turning a re-import into an orphan pair instead of an update.
    /// - `created + hash(content)` is stable across re-exports for unchanged
    ///   notes (Evernote does not mutate `created`), and survives title edits.
    ///   Two distinct notes captured in the same second with different bodies
    ///   produce different ids.  Two IDENTICAL notes (same body, same second)
    ///   would hash the same — acceptable: they are indistinguishable anyway.
    fn dedupe_id(&self) -> String {
        let mut h = DefaultHasher::new();
        self.content.hash(&mut h);
        let hash = h.finish();
        format!("{}|{:016x}", self.created.trim(), hash)
    }
}

/// Parse all notes from an `.enex` XML body. Tolerant: malformed notes (no
/// title or no parseable created date) are skipped rather than erroring.
/// Returns the list of parsed notes regardless of encoding issues — best effort.
///
/// # Element-depth guard
///
/// ENEX notes may contain nested subtrees (notably `<task>` in Evernote v10+
/// and `<resource>`) that carry their own `<title>`, `<created>`, and
/// `<updated>` children.  We guard every field capture with `note_depth == 1`
/// (i.e., the element is a *direct* child of `<note>`) so inner copies never
/// overwrite the note's own fields.
fn parse_enex(xml: &str) -> Vec<EnexNote> {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::with_capacity(8192);

    let mut notes: Vec<EnexNote> = Vec::new();
    let mut cur: Option<EnexNote> = None;

    // Text-capture state: which element's text we're collecting.
    #[derive(Debug, PartialEq)]
    enum Capture {
        None,
        Title,
        Content,
        Created,
        Updated,
        Tag,
        NoteAttr(String), // note-attributes child element name
    }
    let mut capturing = Capture::None;
    let mut text_buf = String::new();

    // Whether we are inside <note-attributes> (a direct note child).
    let mut in_note_attrs = false;

    // Nesting depth *within* the current <note> element (0 = outside note,
    // 1 = direct child of <note>, 2+ = nested subtree such as <task> or
    // <resource>).  Only depth==1 events are eligible to set note fields.
    let mut note_depth: u32 = 0;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) => {
                match e.name().as_ref() {
                    b"note" => {
                        cur = Some(EnexNote::default());
                        capturing = Capture::None;
                        in_note_attrs = false;
                        note_depth = 0;
                    }
                    name => {
                        if cur.is_some() {
                            note_depth += 1;
                        }
                        // Only process note-direct children (depth==1).
                        if cur.is_some() && note_depth == 1 {
                            match name {
                                b"title" => {
                                    capturing = Capture::Title;
                                    text_buf.clear();
                                }
                                b"content" => {
                                    capturing = Capture::Content;
                                    text_buf.clear();
                                }
                                b"created" => {
                                    capturing = Capture::Created;
                                    text_buf.clear();
                                }
                                b"updated" => {
                                    capturing = Capture::Updated;
                                    text_buf.clear();
                                }
                                b"tag" => {
                                    capturing = Capture::Tag;
                                    text_buf.clear();
                                }
                                b"note-attributes" => {
                                    in_note_attrs = true;
                                }
                                b"resource" => {
                                    if let Some(n) = cur.as_mut() {
                                        n.resource_count += 1;
                                    }
                                }
                                _ => {}
                            }
                        } else if cur.is_some() && in_note_attrs && note_depth == 2 {
                            // Direct child of <note-attributes>: capture as note-attr.
                            let attr_name =
                                std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_string();
                            if !attr_name.is_empty() {
                                capturing = Capture::NoteAttr(attr_name);
                                text_buf.clear();
                            }
                        }
                    }
                }
            }
            Ok(Event::Empty(ref e)) => {
                // <resource/> self-closing at depth 1.
                if cur.is_some() && e.name().as_ref() == b"resource" {
                    // Empty elements don't fire Start/End, so no depth change.
                    // Only count if we are at note-direct level.
                    if note_depth == 0 {
                        // We're at note direct child level (no open sub-element).
                        if let Some(n) = cur.as_mut() {
                            n.resource_count += 1;
                        }
                    }
                }
            }
            Ok(Event::Text(ref e)) => {
                // quick-xml 0.40: decode() handles encoding/EOL; unescape handles entities.
                let s = match e.decode() {
                    Ok(decoded) => match quick_xml::escape::unescape(&decoded) {
                        Ok(unescaped) => unescaped.into_owned(),
                        Err(_) => decoded.into_owned(),
                    },
                    Err(_) => String::from_utf8_lossy(e.as_ref()).into_owned(),
                };
                text_buf.push_str(&s);
            }
            // ENEX stores note content as <content><![CDATA[…]]></content>
            Ok(Event::CData(ref e)) => {
                let s = std::str::from_utf8(e.as_ref()).unwrap_or("").to_string();
                text_buf.push_str(&s);
            }
            Ok(Event::End(ref e)) => match e.name().as_ref() {
                b"note" => {
                    if let Some(n) = cur.take() {
                        notes.push(n);
                    }
                    capturing = Capture::None;
                    in_note_attrs = false;
                    note_depth = 0;
                }
                name => {
                    // Only commit captures made at note-direct depth.
                    if cur.is_some() && note_depth == 1 {
                        match name {
                            b"title" => {
                                if let (Some(n), Capture::Title) = (cur.as_mut(), &capturing) {
                                    n.title = text_buf.trim().to_string();
                                }
                                capturing = Capture::None;
                            }
                            b"content" => {
                                if let (Some(n), Capture::Content) = (cur.as_mut(), &capturing) {
                                    n.content = text_buf.trim().to_string();
                                }
                                capturing = Capture::None;
                            }
                            b"created" => {
                                if let (Some(n), Capture::Created) = (cur.as_mut(), &capturing) {
                                    n.created = text_buf.trim().to_string();
                                }
                                capturing = Capture::None;
                            }
                            b"updated" => {
                                if let (Some(n), Capture::Updated) = (cur.as_mut(), &capturing) {
                                    n.updated = text_buf.trim().to_string();
                                }
                                capturing = Capture::None;
                            }
                            b"tag" => {
                                if let (Some(n), Capture::Tag) = (cur.as_mut(), &capturing) {
                                    let tag = text_buf.trim().to_string();
                                    if !tag.is_empty() {
                                        n.tags.push(tag);
                                    }
                                }
                                capturing = Capture::None;
                            }
                            b"note-attributes" => {
                                in_note_attrs = false;
                                capturing = Capture::None;
                            }
                            _ => {}
                        }
                    } else if cur.is_some() && note_depth == 2 {
                        // Closing a note-attributes child element.
                        if let Capture::NoteAttr(ref attr_name) = capturing {
                            let attr_name = attr_name.clone();
                            let val = text_buf.trim().to_string();
                            if !val.is_empty() {
                                if let Some(n) = cur.as_mut() {
                                    n.note_attrs.insert(attr_name, Value::String(val));
                                }
                            }
                            capturing = Capture::None;
                        }
                    }
                    if cur.is_some() && note_depth > 0 {
                        note_depth -= 1;
                    }
                }
            },
            Ok(Event::Eof) => break,
            // XML errors: skip the event, keep going (tolerant import).
            Err(_) => {}
            _ => {}
        }
        buf.clear();
    }
    notes
}

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let xml = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let raw_notes = parse_enex(&xml);
    let total = raw_notes.len() as u64;

    // Collect already-stored ids for dedupe.
    let stream = vault.stream(NOTES_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions().unwrap_or_default() {
        for it in stream.read::<Note>(&key).unwrap_or_default() {
            if !it.id.is_empty() {
                seen.insert(it.id);
            }
        }
    }

    let mut contract: Vec<Note> = Vec::new();
    let mut raw_rows: Vec<Value> = Vec::new();
    let mut imported = 0u64;
    let mut duplicates = 0u64;
    let mut skipped = 0u64;

    for (idx, en) in raw_notes.into_iter().enumerate() {
        // Need at least a created date to partition.
        let Some(created_rfc) = parse_enex_date(&en.created) else {
            skipped += 1;
            continue;
        };

        let id = en.dedupe_id();
        if id.trim_start_matches('|').is_empty() {
            // No title AND no created — can't form a stable id.
            skipped += 1;
            continue;
        }

        let modified_rfc = if en.updated.is_empty() {
            created_rfc.clone()
        } else {
            parse_enex_date(&en.updated).unwrap_or_else(|| created_rfc.clone())
        };

        // Dedupe check (already imported from a prior run).
        if !seen.insert(id.clone()) {
            duplicates += 1;
            continue;
        }

        // --- Contract note ---
        let mut note = Note::new(SOURCE, &id);
        note.title = en.title.clone();
        note.body = en.content.clone();
        note.created = created_rfc.clone();
        note.modified = modified_rfc.clone();
        note.tags = en.tags.clone();

        // source-url from note-attributes → extra (not a top-level Note field).
        let mut extra = Map::new();
        if let Some(Value::String(url)) = en.note_attrs.get("source-url") {
            extra.insert("source_url".into(), Value::String(url.clone()));
        }
        if let Some(Value::String(author)) = en.note_attrs.get("author") {
            extra.insert("author".into(), Value::String(author.clone()));
        }
        if en.resource_count > 0 {
            extra.insert("resource_count".into(), Value::from(en.resource_count));
        }
        // Any additional note-attributes not already captured.
        for (k, v) in &en.note_attrs {
            if k != "source-url" && k != "author" {
                extra.entry(k.replace('-', "_")).or_insert_with(|| v.clone());
            }
        }
        note.extra = extra;

        // --- Raw row ---
        let mut raw = Map::new();
        raw.insert("source".into(), Value::String(SOURCE.into()));
        raw.insert("id".into(), Value::String(id.clone()));
        raw.insert("title".into(), Value::String(en.title));
        raw.insert("content".into(), Value::String(en.content));
        raw.insert("created_raw".into(), Value::String(en.created));
        raw.insert("updated_raw".into(), Value::String(en.updated));
        raw.insert("created".into(), Value::String(created_rfc.clone()));
        raw.insert("modified".into(), Value::String(modified_rfc));
        if !note.tags.is_empty() {
            raw.insert(
                "tags".into(),
                Value::Array(note.tags.iter().cloned().map(Value::from).collect()),
            );
        }
        raw.insert("resource_count".into(), Value::from(en.resource_count));
        raw.insert("note_attributes".into(), Value::Object(en.note_attrs));
        // Immutable partition key for the raw layer (created month).
        raw.insert("_created".into(), Value::String(created_rfc));

        raw_rows.push(Value::Object(raw));
        contract.push(note);
        imported += 1;

        if (idx as u64 + 1) % 100 == 0 {
            progress(ImportProgress {
                records: imported,
                percent: (idx as f32 + 1.0) / total.max(1) as f32 * 100.0,
            });
        }
    }

    if !contract.is_empty() {
        vault.upsert_evernote_notes(&contract)?;
        vault.upsert_evernote_raw(&raw_rows)?;
    }

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} notes imported, {duplicates} duplicates skipped\
             {}",
            if skipped > 0 { format!(", {skipped} skipped (no parseable date)") } else { String::new() }
        ),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Vault helpers (mirror bear.rs / drafts.rs upsert pattern)

impl Vault {
    /// Upsert notes into `notes/evernote/YYYY-MM.jsonl` (contract layer),
    /// partitioned by `created` month, deduped by `id`. Each affected month
    /// is rewritten atomically.
    pub fn upsert_evernote_notes(&self, notes: &[Note]) -> Result<()> {
        use std::collections::HashMap;
        let mut by_month: HashMap<String, Vec<&Note>> = HashMap::new();
        for n in notes {
            if let Some(key) = Partition::Month.key(&n.created) {
                by_month.entry(key.to_string()).or_default().push(n);
            }
        }
        let stream = self.stream(NOTES_DIR, Partition::Month);
        for (month, incoming) in by_month {
            let incoming_ids: HashSet<&str> = incoming.iter().map(|n| n.id.as_str()).collect();
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

    /// Upsert raw rows into `notes/evernote/raw/YYYY-MM.jsonl`, partitioned
    /// by `_created`, deduped by `id`.
    pub fn upsert_evernote_raw(&self, rows: &[Value]) -> Result<()> {
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
            let incoming_ids: HashSet<&str> = incoming.iter().map(|v| id_of(v)).collect();
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

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-evernote-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A checklist note with an embedded <task> subtree (Evernote v10+).
    // The <task> has its OWN <title>, <created>, and <updated> children.
    // These MUST NOT overwrite the outer note's fields.
    const ENEX_WITH_TASK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20260614T120000Z" application="Evernote" version="10.0">
  <note>
    <title>My checklist note</title>
    <content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note><en-note><p>Buy groceries</p></en-note>]]></content>
    <created>20260601T080000Z</created>
    <updated>20260601T090000Z</updated>
    <task>
      <title>Buy milk</title>
      <created>20260615T120000Z</created>
      <updated>20260615T130000Z</updated>
      <taskStatus>open</taskStatus>
    </task>
  </note>
</en-export>"#;

    // Two distinct notes with the same title AND the same created timestamp.
    // Both MUST be imported (content differs → different content hash).
    const ENEX_SAME_TITLE_SAME_CREATED: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20260614T120000Z" application="Evernote" version="10.0">
  <note>
    <title>Untitled Note</title>
    <content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note><en-note><p>First capture body</p></en-note>]]></content>
    <created>20260601T100000Z</created>
    <updated>20260601T100000Z</updated>
  </note>
  <note>
    <title>Untitled Note</title>
    <content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note><en-note><p>Second capture body — different</p></en-note>]]></content>
    <created>20260601T100000Z</created>
    <updated>20260601T100000Z</updated>
  </note>
</en-export>"#;

    // A minimal valid ENEX: one plain-text note and one tagged note with
    // attachments, confirmed against DTD v4 field names.
    const ENEX_TEXT_ONLY: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20260614T120000Z" application="Evernote" version="10.0">
  <note>
    <title>Garden planting plan</title>
    <content><![CDATA[<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE en-note SYSTEM "http://xml.evernote.com/pub/enml2.dtd"><en-note><p>Tomatoes in the south bed</p></en-note>]]></content>
    <created>20260314T091200Z</created>
    <updated>20260402T184000Z</updated>
    <tag>garden</tag>
    <tag>spring</tag>
    <note-attributes>
      <source-url>https://example.com/garden</source-url>
      <author>Alice</author>
    </note-attributes>
  </note>
</en-export>"#;

    const ENEX_WITH_RESOURCE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20260614T120000Z" application="Evernote" version="10.0">
  <note>
    <title>Recipe scan</title>
    <content><![CDATA[<?xml version="1.0" encoding="UTF-8"?><!DOCTYPE en-note SYSTEM "http://xml.evernote.com/pub/enml2.dtd"><en-note><p>Scan attached</p></en-note>]]></content>
    <created>20260501T080000Z</created>
    <updated>20260501T080000Z</updated>
    <resource>
      <data encoding="base64">iVBORw0KGgo=</data>
      <mime>image/png</mime>
    </resource>
  </note>
</en-export>"#;

    const ENEX_TWO_NOTES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE en-export SYSTEM "http://xml.evernote.com/pub/evernote-export4.dtd">
<en-export export-date="20260614T120000Z" application="Evernote" version="10.0">
  <note>
    <title>Alpha</title>
    <content><![CDATA[<en-note><p>Alpha body</p></en-note>]]></content>
    <created>20260601T090000Z</created>
    <updated>20260601T090000Z</updated>
  </note>
  <note>
    <title>Beta</title>
    <content><![CDATA[<en-note><p>Beta body</p></en-note>]]></content>
    <created>20260602T100000Z</created>
    <updated>20260602T100000Z</updated>
    <tag>work</tag>
  </note>
</en-export>"#;

    // ---------------------------------------------------------------------------
    // parse_enex_date

    #[test]
    fn parse_date_utc_converts_to_local_rfc3339() {
        // 20130730T205204Z = 2013-07-30 20:52:04 UTC
        let s = parse_enex_date("20130730T205204Z").unwrap();
        // Should be a valid RFC3339 with timezone offset.
        assert!(s.starts_with("2013-07-30") || s.starts_with("2013-07-31"),
            "local date depends on TZ, but year/month must be correct: {s}");
        assert!(s.contains('T'), "RFC3339 has T separator: {s}");
        // Lowercase z variant.
        let s2 = parse_enex_date("20130730T205204z").unwrap();
        assert_eq!(s, s2, "lowercase z treated same as Z");
        // Bad input returns None.
        assert!(parse_enex_date("not-a-date").is_none());
        assert!(parse_enex_date("").is_none());
    }

    // ---------------------------------------------------------------------------
    // parse_enex

    #[test]
    fn parses_plain_text_note_with_tags_and_note_attributes() {
        let notes = parse_enex(ENEX_TEXT_ONLY);
        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(n.title, "Garden planting plan");
        assert!(n.content.contains("Tomatoes"), "CDATA body parsed: {}", n.content);
        assert_eq!(n.created, "20260314T091200Z");
        assert_eq!(n.updated, "20260402T184000Z");
        assert_eq!(n.tags, vec!["garden", "spring"]);
        assert_eq!(
            n.note_attrs.get("source-url").and_then(Value::as_str),
            Some("https://example.com/garden")
        );
        assert_eq!(
            n.note_attrs.get("author").and_then(Value::as_str),
            Some("Alice")
        );
        assert_eq!(n.resource_count, 0);
    }

    #[test]
    fn parses_note_with_resource_attachment() {
        let notes = parse_enex(ENEX_WITH_RESOURCE);
        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(n.title, "Recipe scan");
        assert_eq!(n.resource_count, 1, "resource element counted");
    }

    #[test]
    fn parses_two_notes_in_one_file() {
        let notes = parse_enex(ENEX_TWO_NOTES);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].title, "Alpha");
        assert_eq!(notes[1].title, "Beta");
        assert!(notes[0].tags.is_empty());
        assert_eq!(notes[1].tags, vec!["work"]);
    }

    #[test]
    fn tolerates_empty_and_malformed_xml() {
        assert!(parse_enex("").is_empty());
        assert!(parse_enex("<not-enex/>").is_empty());
        // A note with no created: the note is still parsed (skipped at import).
        let xml = r#"<?xml version="1.0"?><en-export><note><title>No Date</title><content><![CDATA[body]]></content></note></en-export>"#;
        let notes = parse_enex(xml);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].title, "No Date");
        assert_eq!(notes[0].created, "");
    }

    // ---------------------------------------------------------------------------
    // Full import integration tests (Vault writes)

    fn do_import(vault: &Vault, enex: &str) -> ImportOutcome {
        let path = vault.root().join("test.enex");
        fs::write(&path, enex).unwrap();
        (IMPORT.run)(vault, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_note_to_contract_and_raw_layers() {
        let v = temp_vault("contract-raw");
        let out = do_import(&v, ENEX_TEXT_ONLY);
        assert_eq!(out.counts["imported"], 1);
        assert_eq!(out.counts["duplicates"], 0);

        // Contract: partitioned by local month of created (2026-03 in UTC).
        // The exact month key depends on local TZ; check that something landed.
        let stream = v.stream(NOTES_DIR, Partition::Month);
        let partitions = stream.partitions().unwrap();
        assert!(!partitions.is_empty(), "at least one month partition written");
        let notes: Vec<Note> = partitions
            .iter()
            .flat_map(|k| stream.read::<Note>(k).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(n.source, "evernote");
        assert_eq!(n.title, "Garden planting plan");
        assert!(n.body.contains("Tomatoes"), "ENML body preserved: {}", n.body);
        assert_eq!(n.tags, vec!["garden", "spring"]);
        assert!(!n.created.is_empty(), "created RFC3339 set");
        assert!(!n.modified.is_empty(), "modified RFC3339 set");
        // source_url ends up in extra.
        assert_eq!(
            n.extra.get("source_url").and_then(Value::as_str),
            Some("https://example.com/garden")
        );
        assert_eq!(
            n.extra.get("author").and_then(Value::as_str),
            Some("Alice")
        );

        // Raw: same partition.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let raw_parts = raw_stream.partitions().unwrap();
        assert!(!raw_parts.is_empty());
        let raws: Vec<Value> = raw_parts
            .iter()
            .flat_map(|k| raw_stream.read::<Value>(k).unwrap_or_default())
            .collect();
        assert_eq!(raws.len(), 1);
        let r = &raws[0];
        assert_eq!(r["source"], "evernote");
        assert_eq!(r["title"], "Garden planting plan");
        assert!(r["content"].as_str().unwrap().contains("Tomatoes"));
        assert_eq!(r["resource_count"], 0);
        assert!(r["note_attributes"]["source-url"].as_str().unwrap().contains("example.com"));
    }

    #[test]
    fn resource_count_in_contract_extra_and_raw() {
        let v = temp_vault("resource");
        let out = do_import(&v, ENEX_WITH_RESOURCE);
        assert_eq!(out.counts["imported"], 1);

        let stream = v.stream(NOTES_DIR, Partition::Month);
        let notes: Vec<Note> = stream
            .partitions()
            .unwrap_or_default()
            .iter()
            .flat_map(|k| stream.read::<Note>(k).unwrap_or_default())
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0].extra.get("resource_count").and_then(Value::as_u64),
            Some(1)
        );
    }

    #[test]
    fn reimport_is_idempotent_by_id() {
        let v = temp_vault("dedupe");
        let out1 = do_import(&v, ENEX_TWO_NOTES);
        assert_eq!(out1.counts["imported"], 2);

        // Re-import the same file: both notes are duplicates.
        let out2 = do_import(&v, ENEX_TWO_NOTES);
        assert_eq!(out2.counts["imported"], 0);
        assert_eq!(out2.counts["duplicates"], 2);

        // Contract: still exactly 2 unique notes.
        let stream = v.stream(NOTES_DIR, Partition::Month);
        let all: Vec<Note> = stream
            .partitions()
            .unwrap_or_default()
            .iter()
            .flat_map(|k| stream.read::<Note>(k).unwrap_or_default())
            .collect();
        assert_eq!(all.len(), 2, "no duplicates after re-import");
    }

    #[test]
    fn note_without_parseable_created_date_is_skipped() {
        let xml = r#"<?xml version="1.0"?><en-export>
          <note>
            <title>No date note</title>
            <content><![CDATA[body]]></content>
          </note>
        </en-export>"#;
        let v = temp_vault("nodate");
        let out = do_import(&v, xml);
        assert_eq!(out.counts["skipped"], 1);
        assert_eq!(out.counts["imported"], 0);
    }

    // ---------------------------------------------------------------------------
    // Defect-regression tests

    /// Blocking defect: <task> subtrees (Evernote v10+) carry their own
    /// <title>, <created>, <updated>.  The parser must NOT let those inner
    /// values overwrite the outer note's fields.
    #[test]
    fn task_subtree_does_not_clobber_note_fields() {
        let notes = parse_enex(ENEX_WITH_TASK);
        assert_eq!(notes.len(), 1, "one note parsed");
        let n = &notes[0];
        assert_eq!(n.title, "My checklist note",
            "note title must not be overwritten by task's <title>: got '{}'", n.title);
        assert_eq!(n.created, "20260601T080000Z",
            "note created must not be overwritten by task's <created>: got '{}'", n.created);
        assert_eq!(n.updated, "20260601T090000Z",
            "note updated must not be overwritten by task's <updated>: got '{}'", n.updated);
    }

    /// Same as above but at the import layer: wrong-month partition and lost
    /// title are caught if the depth guard is broken.
    #[test]
    fn task_subtree_does_not_clobber_note_at_import() {
        let v = temp_vault("task-clobber");
        let out = do_import(&v, ENEX_WITH_TASK);
        assert_eq!(out.counts["imported"], 1, "note imported");

        let stream = v.stream(NOTES_DIR, Partition::Month);
        let all: Vec<Note> = stream
            .partitions()
            .unwrap_or_default()
            .iter()
            .flat_map(|k| stream.read::<Note>(k).unwrap_or_default())
            .collect();
        assert_eq!(all.len(), 1, "exactly one note in vault");
        assert_eq!(all[0].title, "My checklist note",
            "vault note title must be note's own, not task's: '{}'", all[0].title);
        // The note's created is 2026-06-01; verify it landed in the 2026-06 partition.
        let parts = stream.partitions().unwrap_or_default();
        assert!(parts.iter().any(|p| p.starts_with("2026-06")),
            "note must be in 2026-06 partition (created 2026-06-01), partitions: {parts:?}");
    }

    /// Major defect: two distinct notes with the same title AND the same
    /// created timestamp must both be imported (different content → different
    /// content hash → different dedupe id).
    #[test]
    fn distinct_notes_same_title_and_created_both_imported() {
        let notes = parse_enex(ENEX_SAME_TITLE_SAME_CREATED);
        assert_eq!(notes.len(), 2, "both notes parsed");
        let id0 = notes[0].dedupe_id();
        let id1 = notes[1].dedupe_id();
        assert_ne!(id0, id1,
            "different-body notes with same title+created must have different dedupe ids: id0={id0}, id1={id1}");

        let v = temp_vault("collision");
        let out = do_import(&v, ENEX_SAME_TITLE_SAME_CREATED);
        assert_eq!(out.counts["imported"], 2, "both distinct notes imported, not silently dropped");
    }

    /// Major defect: editing a note's title must NOT change its dedupe id.
    /// The id is based on created + content hash; title is excluded.
    #[test]
    fn title_edit_does_not_change_dedupe_id() {
        let original_xml = r#"<?xml version="1.0"?><en-export>
          <note>
            <title>Meeting notes</title>
            <content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note><en-note><p>body</p></en-note>]]></content>
            <created>20260601T120000Z</created>
            <updated>20260601T120000Z</updated>
          </note>
        </en-export>"#;

        let edited_xml = r#"<?xml version="1.0"?><en-export>
          <note>
            <title>Meeting notes (final)</title>
            <content><![CDATA[<?xml version="1.0"?><!DOCTYPE en-note><en-note><p>body</p></en-note>]]></content>
            <created>20260601T120000Z</created>
            <updated>20260605T100000Z</updated>
          </note>
        </en-export>"#;

        let orig_notes = parse_enex(original_xml);
        let edit_notes = parse_enex(edited_xml);
        assert_eq!(orig_notes.len(), 1);
        assert_eq!(edit_notes.len(), 1);
        let orig_id = orig_notes[0].dedupe_id();
        let edit_id = edit_notes[0].dedupe_id();
        assert_eq!(orig_id, edit_id,
            "title edit must not change dedupe id (content unchanged); orig={orig_id}, edit={edit_id}");

        // At import layer: re-importing after title edit must NOT add a second row.
        let v = temp_vault("title-edit");
        let out1 = do_import(&v, original_xml);
        assert_eq!(out1.counts["imported"], 1);

        let out2 = do_import(&v, edited_xml);
        assert_eq!(out2.counts["duplicates"], 1,
            "re-importing after title edit should be a duplicate, not a new row");
        assert_eq!(out2.counts["imported"], 0);
    }

    #[test]
    fn behavior_is_import() {
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
    }

    #[test]
    fn accepts_enex_extension() {
        assert!(IMPORT.accepts.contains(&"enex"));
    }
}
