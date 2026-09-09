//! The `browser-searches` domain contract: every search query the owner has
//! typed, from any search source, in one normalized stream.
//!
//! A single record shape — **[`Search`]** — one query at one time, under
//! `browser/searches/<source>/YYYY-MM.jsonl` (`<source>` is the collector id and
//! the folder name; the month is the month of [`Search::ts`]). Google's My
//! Activity Search log (via Takeout) and the search-engine visits in Safari's
//! history both reduce to the same atom and write this shape; readers list them
//! chronologically and group by `engine` for "what I searched on Google vs.
//! DuckDuckGo".
//!
//! This is the **query stream only**: the page the owner *visited* after
//! searching is a `browser/` visit, a sibling stream, joined (if at all) at read
//! time. The stream is **append-only** — a search happens once, like a
//! [`crate::social::Post`] — and collectors skip guids they already hold (`guid`
//! is the dedupe key). Only `ts`/`source`/`query` are required; that triple is
//! the whole record a sparse source needs, while `engine`, `url`, and the dedupe
//! `guid` are optional enrichment a richer source fills. The collector owns
//! extracting a clean `query` string (Takeout's `title` is a localized
//! `"Searched for …"` sentence; Safari's is the `q=`/`p=`/`query=` parameter of a
//! visited search-engine URL) — the contract receives the decoded terms, never
//! the wrapper. Source-specific fields the normalized columns don't carry ride
//! verbatim under `extra` rather than being dropped.
//!
//! **What was clicked is not here** — neither queued source can observe which
//! result the owner opened, so the contract carries no `result_clicked` field; a
//! future SERP-aware source could add it additively.
//!
//! See [`docs/vault-spec/domains/browser-searches.md`] for the field-level spec;
//! the schema field descriptions there are authoritative for names/units/meanings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One search query — one line of `browser/searches/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`query`
/// are required; everything else is omit-empty. Matches
/// `browser-searches.search.schema.json` field-for-field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Search {
    /// RFC3339 local time the search was issued. Always serialized; its month is
    /// the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`google-takeout`,
    /// `safari`). Always serialized.
    pub source: String,
    /// The search terms, decoded (URL-unescaped, prefix stripped). Always
    /// serialized.
    pub query: String,
    /// The search engine, lowercased (`"google"`, `"bing"`, `"duckduckgo"`,
    /// `"safari-default"`, …) — the read-time grouping axis.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub engine: String,
    /// The full search URL, when the source has one (Takeout `titleUrl`; the
    /// visited Safari URL).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// Source-unique id, the dedupe key (a `(time, query)` hash for Takeout My
    /// Activity rows, which carry no native id; a search-URL-visit hash for
    /// Safari).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub guid: String,
    /// Everything source-specific the normalized fields don't carry (Takeout
    /// `header`/`products`/`locationInfos`, Safari visit count, …) — full
    /// fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Search {
    /// A minimal record with only the three required fields set.
    pub fn new(
        source: impl Into<String>,
        ts: impl Into<String>,
        query: impl Into<String>,
    ) -> Self {
        Search {
            ts: ts.into(),
            source: source.into(),
            query: query.into(),
            engine: String::new(),
            url: String::new(),
            guid: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_search_serializes_only_required_fields() {
        let s = Search::new("google-takeout", "2026-06-11T08:42:17-07:00", "how to defrost sourdough");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            json!({
                "ts": "2026-06-11T08:42:17-07:00",
                "source": "google-takeout",
                "query": "how to defrost sourdough"
            })
        );
    }

    #[test]
    fn full_search_round_trips() {
        let line = json!({
            "ts": "2026-06-10T21:03:01-07:00",
            "source": "google-takeout",
            "query": "trove app local first",
            "engine": "google",
            "url": "https://www.google.com/search?q=trove+app+local+first",
            "guid": "gt-search-2026-06-10T21:03:01-trove+app+local+first",
            "extra": {"header": "Search", "products": ["Search"]}
        });
        let s: Search = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(s.query, "trove app local first");
        assert_eq!(s.engine, "google");
        assert_eq!(s.guid, "gt-search-2026-06-10T21:03:01-trove+app+local+first");
        assert_eq!(s.extra.get("header"), Some(&json!("Search")));
        // Round-trips byte-for-byte with the same field order.
        assert_eq!(serde_json::to_value(&s).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_empty_optionals_omitted() {
        // Forward-compat: an unknown top-level field is ignored; empty optionals
        // are omitted on re-serialize.
        let line = json!({
            "ts": "2026-06-11T09:15:44-07:00",
            "source": "safari",
            "query": "flights to lisbon",
            "engine": "duckduckgo",
            "future_field": "ignored"
        });
        let s: Search = serde_json::from_value(line).unwrap();
        assert_eq!(s.engine, "duckduckgo");
        assert!(s.url.is_empty() && s.guid.is_empty());
        let re = serde_json::to_value(&s).unwrap();
        assert!(re.get("future_field").is_none(), "unknown field dropped on re-serialize");
        assert!(re.get("url").is_none() && re.get("guid").is_none(), "empty optionals omitted");
    }
}
