//! The `reading` domain contract: everything the user saved, read, or marked
//! up on the web and in books — read-later saves, bookmarks, RSS reads, and the
//! highlights/annotations made on top of them — in one normalized,
//! source-agnostic store.
//!
//! Two record shapes share the domain:
//!
//! - **[`Item`]** — one saved/read article, bookmark, or RSS item, under
//!   `reading/<source>/YYYY-MM.jsonl` (`<source>` is the collector id and the
//!   folder name; the month is the month of [`Item::ts`]). Read-later and
//!   bookmark services (Raindrop, Instapaper, Pocket, Pinboard, Omnivore,
//!   Wallabag, Linkding), RSS readers (Feedly, Inoreader, NetNewsWire, Reeder),
//!   and Readwise Reader write this shape.
//! - **[`Highlight`]** — one highlighted passage or annotation, with its parent
//!   document referenced inline, under `reading/<source>/highlights/YYYY-MM.jsonl`
//!   (month of [`Highlight::ts`]). Highlight hubs and annotators (Readwise,
//!   Kindle clippings, Hypothesis, Snipd) write this shape. Readwise spans both
//!   (Reader → items, Readwise → highlights).
//!
//! Both streams are **append-only** — a save/read/highlight happens once, like a
//! [`crate::social::Post`] — and collectors skip guids they already hold (`guid`
//! is the dedupe key in each shape). Only `ts`/`source`/`guid` are required;
//! everything else is omit-empty, so a bare bookmark writes three fields while a
//! richly-tagged read-later save with progress fills more. Source-specific
//! fields the normalized columns don't carry ride verbatim under `extra` rather
//! than being dropped.
//!
//! Cross-source overlap (the same article saved in two apps, the same Kindle
//! highlight via both `kindle` and `readwise`) is reconciled at *read* time —
//! each source keeps its own folder and stable guids; nothing is merged or
//! dropped at write time. A `feed-subscription` list, OPML import, page
//! snapshot, or article full-text body is **not** an item or a highlight — those
//! stay per-source raw under `reading/<source>/` (e.g. `feeds.jsonl`, `raw/`).
//! Saved *social* posts stay under `social/<source>/`; a book read as a play
//! belongs in `media/plays/`.
//!
//! See [`docs/vault-spec/domains/reading.md`] for the field-level spec; the
//! schema field descriptions there are authoritative for names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One saved/read article, bookmark, or RSS item — one line of
/// `reading/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`guid`
/// are required; everything else is omit-empty. Matches
/// `reading.item.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Item {
    /// RFC3339 local time the item was saved (or read/published — the most
    /// identity-bearing time the source exposes). Always serialized; its month
    /// is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`raindrop`,
    /// `readwise`, `instapaper`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key (Raindrop/Linkding/Wallabag/Reader item
    /// id, Pinboard hash, URL+saved-at hash for export files). Always
    /// serialized.
    pub guid: String,
    /// The saved/linked page.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Article / bookmark title.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Byline, where the source carries one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// Publisher / domain (`"example.com"`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub site: String,
    /// RSS feed / publication title, for reader sources.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub feed: String,
    /// Snippet, selection, or the bookmark's own note/description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub excerpt: String,
    /// User tags / folders / collection labels.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Coarse read state: `"saved"` (default, unread) | `"archived"` | `"read"`
    /// | `"favorite"` (starred) — an open string normalizing each app's own
    /// vocabulary; the source's exact flags stay in `extra`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    /// Read progress as an integer percent, 0–100 (Instapaper, Reader). A
    /// source that exposes a 0..1 fraction scales it to a percent at write time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<i64>,
    /// RFC3339 local time the item was read / last opened.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub read_at: String,
    /// Everything source-specific the normalized fields don't carry (collection
    /// name, cover, shared flag, original CSV position, …) — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Item {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, guid: impl Into<String>, ts: impl Into<String>) -> Self {
        Item {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            url: String::new(),
            title: String::new(),
            author: String::new(),
            site: String::new(),
            feed: String::new(),
            excerpt: String::new(),
            tags: Vec::new(),
            state: String::new(),
            progress: None,
            read_at: String::new(),
            extra: Map::new(),
        }
    }
}

/// One highlighted passage or annotation — one line of
/// `reading/<source>/highlights/YYYY-MM.jsonl` — with its parent document
/// referenced inline so no join is needed: a book carries `title` + `author`, a
/// web page carries `url` + `title`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`guid`
/// are required; `text` is omitted on rows that have none (a Kindle bookmark, or
/// a clipping that hit Amazon's clipping-limit cap, which records its marker in
/// `extra` instead). A passage with no user annotation omits `note`. Matches
/// `reading.highlight.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Highlight {
    /// RFC3339 local time the highlight was made / added. Always serialized; its
    /// month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`readwise`, `kindle`,
    /// `hypothesis`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key (Readwise/Hypothesis annotation id, or
    /// hash(title,location,added) for Kindle clippings). Always serialized.
    pub guid: String,
    /// The highlighted passage (omitted for bookmarks / capped clippings).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The user's annotation / note on the passage.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// Parent document title (book or page).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// Parent author, for books.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// Parent page URL, for web annotations.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Page / Kindle location range / position / CFI / in-episode timestamp.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// Highlight color, where the source has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color: String,
    /// Tags on the highlight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Everything source-specific the normalized fields don't carry (AI summary,
    /// group id, category, `highlighted_at`, clipping-limit flag, …) — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Highlight {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, guid: impl Into<String>, ts: impl Into<String>) -> Self {
        Highlight {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            text: String::new(),
            note: String::new(),
            title: String::new(),
            author: String::new(),
            url: String::new(),
            location: String::new(),
            color: String::new(),
            tags: Vec::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_item_serializes_only_required_fields() {
        let it = Item::new("pinboard", "a1b2c3d4e5f6", "2024-02-18T22:15:00-08:00");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&it).unwrap(),
            json!({"ts": "2024-02-18T22:15:00-08:00", "source": "pinboard", "guid": "a1b2c3d4e5f6"})
        );
    }

    #[test]
    fn minimal_highlight_serializes_only_required_fields() {
        let h = Highlight::new("readwise", "rw-hl-884412", "2026-06-08T20:11:00-07:00");
        assert_eq!(
            serde_json::to_value(&h).unwrap(),
            json!({"ts": "2026-06-08T20:11:00-07:00", "source": "readwise", "guid": "rw-hl-884412"})
        );
    }

    #[test]
    fn full_item_round_trips_with_integer_progress() {
        let line = json!({
            "ts": "2026-06-10T14:03:00-07:00",
            "source": "raindrop",
            "guid": "rd-1029384",
            "url": "https://example.com/a-deep-dive",
            "title": "A Deep Dive into Local-First Software",
            "site": "example.com",
            "excerpt": "The cloud is just someone else's computer.",
            "tags": ["software", "local-first"],
            "state": "saved",
            "progress": 63,
            "extra": {"collection": "Reading", "cover": "https://example.com/cover.jpg"}
        });
        let it: Item = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(it.progress, Some(63), "progress is an integer percent, not a fraction");
        assert_eq!(it.state, "saved");
        assert_eq!(it.tags, vec!["software", "local-first"]);
        assert_eq!(serde_json::to_value(&it).unwrap(), line);
    }

    #[test]
    fn full_highlight_round_trips() {
        let line = json!({
            "ts": "2026-06-08T20:11:00-07:00",
            "source": "readwise",
            "guid": "rw-hl-884412",
            "text": "Attention is the rarest and purest form of generosity.",
            "note": "cf. Weil on prayer",
            "title": "Gravity and Grace",
            "author": "Simone Weil",
            "location": "142",
            "color": "yellow",
            "tags": ["attention", "ethics"],
            "extra": {"category": "books", "highlighted_at": "2026-06-08T20:11:00-07:00"}
        });
        let h: Highlight = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(h.text, "Attention is the rarest and purest form of generosity.");
        assert_eq!(h.note, "cf. Weil on prayer");
        assert_eq!(h.location, "142");
        assert_eq!(serde_json::to_value(&h).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_textless_highlight_ok() {
        // Forward-compat: an unknown top-level field is ignored; a textless
        // highlight (a bookmark / capped clipping) is valid and omits `text`.
        let line = json!({
            "ts": "2026-05-30T09:05:00-07:00",
            "source": "kindle",
            "guid": "f3a9c1b27e",
            "title": "The Nicomachean Ethics",
            "author": "Aristotle",
            "location": "1099-1101",
            "future_field": "ignored"
        });
        let h: Highlight = serde_json::from_value(line).unwrap();
        assert_eq!(h.title, "The Nicomachean Ethics");
        assert!(h.text.is_empty(), "no text on a bookmark-style highlight");
        let re = serde_json::to_value(&h).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("text").is_none(), "empty text omitted");
    }
}
