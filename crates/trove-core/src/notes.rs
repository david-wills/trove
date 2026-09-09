//! The `notes` domain contract: every notes, journaling, and
//! personal-knowledge app — Apple Notes, Bear, Drafts, Obsidian, Logseq,
//! Notion, Day One, Stoic, … — in one normalized, source-agnostic store.
//!
//! Each source keeps its own folder of full-fidelity rows under
//! `notes/<source>/YYYY-MM.jsonl` — `<source>` is the collector id and the
//! folder name (the contract's "source = folder name" rule); the month is the
//! month of [`Note::created`]. Each file is a **snapshot** of the present:
//! rewritten whole and atomically per affected month on every sync/import
//! (sibling tmp + rename), keyed by [`Note::id`] within the source. A re-run
//! or an overlapping re-import never duplicates a note — the note's line is
//! replaced in place by `id`.
//!
//! This is the *collected* layer (a mirror of what lives in the user's note
//! apps); `artifacts/` stays the separate user-curated layer Trove authors
//! itself, and the reader keeps the two apart. A journal entry, a daily note,
//! a quick capture, a checklist, and a long-form sheet are all *a titled body
//! of text with timestamps* — the same shape. Only `source` and `id` are
//! required; everything else is omit-empty, so a plain-text note with no title
//! writes `body` + `created`, while a richly-tagged, foldered, pinned note
//! fills more. Anything a source carries that the normalized columns don't map
//! — Day One location/weather/mood, Keep checklist items, Roam backlinks,
//! frontmatter — rides verbatim in [`Note::extra`], full fidelity.
//!
//! See [`docs/vault-spec/domains/notes.md`] for the field-level spec. First
//! collector: [`crate::bear`].

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One collected note — one line of `notes/<source>/YYYY-MM.jsonl`.
///
/// A *current-state* snapshot record (not an event): there is no `ts`, the
/// partition is the month of `created`. Only `source` and `id` are required;
/// a sparse source (a title-only Apple Notes row, an untitled Day One entry)
/// writes a minimal line. Whatever the normalized columns don't carry survives
/// verbatim under [`extra`](Note::extra) rather than being dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Note {
    /// Collector id, identical to the source folder name (`bear`, `day-one`,
    /// `apple-notes`, …). Always serialized.
    pub source: String,
    /// Source-native stable note id — the dedupe key (Bear
    /// `ZUNIQUEIDENTIFIER`, Day One / Notion / Roam uuid, Keep filename, a
    /// vault-relative path). Always serialized.
    pub id: String,
    /// Note title (explicit, or the filename / first line / H1 where the
    /// source has no title field); omit when the source has none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The full note text, verbatim — Markdown / plain text / HTML→Markdown
    /// (full fidelity; trimming or rendering is a read-time opinion).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// RFC3339 local time the note was created (falls back to file mtime where
    /// that is all the source exposes). Its month is the partition key.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub created: String,
    /// RFC3339 local time the note was last edited.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub modified: String,
    /// Note tags, verbatim (Bear/Drafts tags, Day One/Keep labels, frontmatter
    /// tags).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The note's one place: folder / notebook / collection / group name, or
    /// vault-relative path for folder-tree sources.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub folder: String,
    /// Pinned / flagged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Archived (out of the active set, not deleted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// In trash / soft-deleted (kept for fidelity; a read-time filter decides
    /// whether to surface).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trashed: Option<bool>,
    /// Everything source-specific the normalized fields don't carry — Day One
    /// location/weather/mood, Stoic metrics, Keep checklist items + color,
    /// Roam/Reflect backlinks, frontmatter, content-type/format marker — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Note {
    /// A minimal record with only the two required fields set.
    pub fn new(source: impl Into<String>, id: impl Into<String>) -> Self {
        Note {
            source: source.into(),
            id: id.into(),
            title: String::new(),
            body: String::new(),
            created: String::new(),
            modified: String::new(),
            tags: Vec::new(),
            folder: String::new(),
            pinned: None,
            archived: None,
            trashed: None,
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_record_serializes_only_source_and_id() {
        let n = Note::new("apple-notes", "x-coredata://A1F2/ICNote/p5512");
        // Omit-empty: a sparse line is exactly `{"source","id"}`.
        assert_eq!(
            serde_json::to_value(&n).unwrap(),
            json!({"source": "apple-notes", "id": "x-coredata://A1F2/ICNote/p5512"})
        );
    }

    #[test]
    fn full_record_round_trips() {
        let line = json!({
            "source": "bear",
            "id": "4F8A2C1E-0B7D-4E2A-9F3C-1A2B3C4D5E6F",
            "title": "Garden planting plan",
            "body": "# Garden planting plan\n\n- Tomatoes in the south bed\n- #garden #spring",
            "created": "2026-03-14T09:12:00-07:00",
            "modified": "2026-04-02T18:40:00-07:00",
            "tags": ["garden", "spring"],
            "folder": "Home",
            "pinned": true,
            "extra": {"ZUNIQUEIDENTIFIER": "4F8A2C1E"}
        });
        let n: Note = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(n.id, "4F8A2C1E-0B7D-4E2A-9F3C-1A2B3C4D5E6F");
        assert_eq!(n.tags, vec!["garden", "spring"]);
        assert_eq!(n.pinned, Some(true));
        assert_eq!(n.archived, None, "omitted bool deserializes to None, not Some(false)");
        assert!(n.extra.contains_key("ZUNIQUEIDENTIFIER"));
        // Round-trip is byte-identical to the spec example.
        assert_eq!(serde_json::to_value(&n).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_false_flags_round_trip() {
        // Forward-compat: an unknown top-level field is ignored; an explicit
        // `trashed:false` survives (Some(false) is distinct from omitted).
        let line = json!({
            "source": "bear",
            "id": "uid-1",
            "trashed": false,
            "future_field": "ignored",
            "extra": {"encrypted": true}
        });
        let n: Note = serde_json::from_value(line).unwrap();
        assert_eq!(n.trashed, Some(false));
        assert_eq!(n.extra.get("encrypted"), Some(&Value::Bool(true)));
        let re = serde_json::to_value(&n).unwrap();
        assert!(re.get("future_field").is_none());
        assert_eq!(re["trashed"], json!(false));
    }
}
