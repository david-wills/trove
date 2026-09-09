//! Pocket — historical import (Mozilla shut down the service July 2025).
//! Catalogued in the Phase 2 pass; brief: docs/integrations/pocket.md
//!
//! Pocket exported a single **Netscape bookmarks HTML** file
//! (`ril_export.html`) — the frozen, community-documented format used by
//! every Read-It-Later / bookmarks app. The format is fixed: the service is
//! dead, the export portal closed November 12, 2025.
//!
//! ## HTML export format (confirmed from community documentation)
//!
//! ```html
//! <!DOCTYPE NETSCAPE-Bookmark-file-1>
//! <H1>Pocket Export</H1>
//! <ul><h1>Unread</h1>
//! <li><a href="https://example.com" time_added="1609459200" tags="tag1,tag2">Article Title</a>
//! ...
//! <ul><h1>Read Archive</h1>
//! <li><a href="https://example.com/read" time_added="1609459200" tags="">Read Title</a>
//! ```
//!
//! Key attributes on `<a>` anchor tags:
//! - `href`       — the saved URL
//! - `time_added` — Unix timestamp (seconds) of when it was saved
//! - `tags`       — comma-separated tags (may be empty)
//!
//! The section (`Unread` vs `Read Archive`) determines the `state` field in
//! the contract.
//!
//! ## Contract mapping
//!
//! One [`crate::reading::Item`] per anchor → `reading/pocket/YYYY-MM.jsonl`
//! (month of `time_added`).
//!
//! - `guid` = `sha256(href + "|" + time_added_str)` — stable across re-imports
//!   of the same file; two export files from different dates merge cleanly
//!   (same article saved-at same time → same guid).
//! - `state` = `"saved"` (Unread section) | `"archived"` (Read Archive section)
//! - `tags` → `tags` (comma-split, trimmed, empty entries dropped)
//! - `href` → `url`; `title` text → `title`
//! - Overflow (none for this format — it is sparse) → `extra` (empty)
//!
//! ## Raw layer
//!
//! `reading/pocket/raw/YYYY-MM.jsonl` — each anchor verbatim as a JSON
//! object: `{href, time_added, tags_raw, title, state_raw}`.
//!
//! Re-importing the same file (or a superset export) is a no-op for records
//! already stored: dedupe is by `guid` on both streams.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Item;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "reading/pocket";
const RAW_DIR: &str = "reading/pocket/raw";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "pocket",
        name: "Pocket (historical import)",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your saved articles and tags from Pocket, Mozilla's read-later \
                      service that shut down in July 2025. Accepts the Netscape bookmarks HTML \
                      export file (ril_export.html) you downloaded before November 12, 2025.",
        domain: "reading",
        vault_path: "reading/pocket/",
        toggleable: false,
        setup: &[
            "Locate your downloaded Pocket export file — it is named `ril_export.html` and \
             was downloaded from getpocket.com/export before the portal closed \
             November 12, 2025.",
            "Drop it here to import all your saved and archived articles.",
        ],
        caveats: "Pocket's export portal closed November 12, 2025; only pre-existing export \
                  files can be imported. Re-importing the same file is safe — no duplicates \
                  will be created.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["html", "htm"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Raw-layer record.

/// The anchor tag fields verbatim — full fidelity before mapping.
#[derive(Debug, Serialize, Deserialize)]
struct RawAnchor {
    /// Routing timestamp for month-partition (local RFC3339, same as item.ts).
    ts: String,
    href: String,
    time_added: String,
    tags_raw: String,
    title: String,
    /// `"unread"` or `"archive"` — which `<h3>` section the anchor sat in.
    state_raw: String,
}

// ---------------------------------------------------------------------------
// A parsed anchor.

#[derive(Debug)]
struct ParsedAnchor {
    href: String,
    time_added_raw: String,
    tags_raw: String,
    title: String,
    state: AnchorState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorState {
    Unread,
    Archive,
}

impl AnchorState {
    fn as_contract_state(self) -> &'static str {
        match self {
            AnchorState::Unread => "saved",
            AnchorState::Archive => "archived",
        }
    }
    fn as_raw_label(self) -> &'static str {
        match self {
            AnchorState::Unread => "unread",
            AnchorState::Archive => "archive",
        }
    }
}

// ---------------------------------------------------------------------------
// HTML parser — no external HTML crate needed; the format is very regular.
//
// Pocket's Netscape bookmarks file has:
//   - `<h1>Unread</h1>` and `<h1>Read Archive</h1>` section headers
//     (real ril_export.html uses <h1>, not <h3>; we detect by text content
//     to stay tag-agnostic in case of minor format variation)
//   - `<a href="..." time_added="..." tags="...">Title</a>` anchor lines
//
// We parse line-by-line (anchors are always on one line in Pocket's output)
// and track which section we're in.

fn parse_html(body: &str) -> Vec<ParsedAnchor> {
    let mut results = Vec::new();
    let mut current_state = AnchorState::Unread; // default until we see a section header

    for line in body.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();

        // Section headers: <h1>Unread</h1> or <h1>Read Archive</h1>
        // Real ril_export.html uses <h1> (not <h3>). We detect by text content
        // rather than a specific tag name so the parser is robust to minor
        // format variations. The title line "<H1>Pocket Export</H1>" contains
        // neither "unread" nor "read archive" so it is safely ignored.
        let is_heading = lower.contains("<h1>")
            || lower.contains("<h2>")
            || lower.contains("<h3>")
            || lower.contains("<h4>");
        if is_heading {
            if lower.contains("read archive") {
                current_state = AnchorState::Archive;
                continue;
            } else if lower.contains("unread") {
                current_state = AnchorState::Unread;
                continue;
            }
        }

        // Anchor lines: <a href="..." time_added="..." tags="...">Title</a>
        // (Pocket always writes the full anchor on one line.)
        if !lower.contains("<a ") && !lower.starts_with("<a ") {
            continue;
        }

        let Some(anchor) = parse_anchor_line(trimmed, current_state) else {
            continue;
        };
        results.push(anchor);
    }

    results
}

/// Extract `href`, `time_added`, `tags`, and inner text from one `<a ...>...</a>` line.
///
/// Pocket anchor lines look like:
/// `<a href="URL" time_added="UNIX_TS" tags="t1,t2">Title text</a>`
///
/// We use simple `attr="value"` extraction (no full HTML parser needed —
/// this format never nests quotes or uses unusual encodings).
fn parse_anchor_line(line: &str, state: AnchorState) -> Option<ParsedAnchor> {
    // Must contain `href="`
    let href = extract_attr(line, "href")?;
    if href.is_empty() {
        return None;
    }

    let time_added = extract_attr(line, "time_added").unwrap_or_default();
    let tags_raw = extract_attr(line, "tags").unwrap_or_default();

    // Inner text: between the closing `>` of the `<a ...>` tag and `</a>`.
    // We must find the `<a ` start first, then its closing `>`, to skip any
    // preceding tags (e.g. `<li>`) that also contain `>`.
    let title = {
        let lower = line.to_ascii_lowercase();
        let a_start = lower.find("<a ").or_else(|| lower.find("<a\t"))?;
        let after_a = &line[a_start..];
        // The `>` that closes the opening `<a ...>` tag.
        let gt_pos = after_a.find('>')?;
        let after_gt = &after_a[gt_pos + 1..];
        after_gt
            .find("</a>")
            .or_else(|| after_gt.find("</A>"))
            .map(|end| after_gt[..end].trim().to_string())
            .unwrap_or_default()
    };

    Some(ParsedAnchor { href, time_added_raw: time_added, tags_raw, title, state })
}

/// Extract `attr="value"` or `attr='value'` from a tag's attribute string.
///
/// Case-insensitive on the attribute name (Pocket uses lowercase; the format
/// allows uppercase). Returns `None` only if the attribute is entirely absent.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    // Search for `attr="` or `attr='` (case-insensitive on the attr name).
    let lower_tag = tag.to_ascii_lowercase();
    let needle_dq = format!("{attr}=\"");
    let needle_sq = format!("{attr}='");
    let lower_attr = attr.to_ascii_lowercase();
    let needle_dq_l = format!("{lower_attr}=\"");
    let needle_sq_l = format!("{lower_attr}='");

    // Try double-quote variant first.
    let (start, delim) = if let Some(pos) = lower_tag.find(&needle_dq_l) {
        (pos + needle_dq.len(), '"')
    } else if let Some(pos) = lower_tag.find(&needle_sq_l) {
        (pos + needle_sq.len(), '\'')
    } else {
        return None;
    };

    let rest = &tag[start..];
    let end = rest.find(delim).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// Unix timestamp string → RFC3339 local time.
///
/// Pocket's `time_added` is seconds since Unix epoch (a string like `"1609459200"`).
/// Returns a fallback epoch timestamp on parse failure (preserves the row).
fn unix_ts_to_local(ts: &str) -> String {
    let secs: i64 = ts.trim().parse().unwrap_or(0);
    let dt = Utc.timestamp_opt(secs, 0).single().unwrap_or(DateTime::UNIX_EPOCH.into());
    dt.with_timezone(&chrono::Local).to_rfc3339()
}

/// Stable guid = sha256(href + "|" + time_added).
///
/// Using both fields means two saves of the same URL at different times
/// (which Pocket allowed) get distinct guids; re-importing the identical file
/// always produces the same guids.
fn make_guid(href: &str, time_added: &str) -> String {
    let mut h = Sha256::new();
    h.update(href.as_bytes());
    h.update(b"|");
    h.update(time_added.as_bytes());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------------------
// Import runner.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let anchors = parse_html(&body);
    let total = anchors.len();

    // Dedupe: load guids already stored in the contract stream.
    let item_stream = vault.stream(DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut seen: HashSet<String> = HashSet::new();
    for key in item_stream.partitions()? {
        for v in item_stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(|v| v.as_str()) {
                seen.insert(g.to_string());
            }
        }
    }

    let mut new_items: Vec<Item> = Vec::new();
    let mut new_raws: Vec<RawAnchor> = Vec::new();
    let mut duplicates: u64 = 0;
    let mut skipped: u64 = 0;

    for (idx, anchor) in anchors.into_iter().enumerate() {
        if anchor.href.is_empty() {
            skipped += 1;
            continue;
        }

        let guid = make_guid(&anchor.href, &anchor.time_added_raw);
        let ts = unix_ts_to_local(&anchor.time_added_raw);

        // Must produce a valid month partition key; otherwise we can't file the row.
        if Partition::Month.key(&ts).is_none() {
            skipped += 1;
            continue;
        }

        if !seen.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }

        // Tags: comma-separated, trimmed, empty entries dropped.
        let tags: Vec<String> = if anchor.tags_raw.is_empty() {
            Vec::new()
        } else {
            anchor
                .tags_raw
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };

        // Derive `site` from the URL host.
        let site = extract_host(&anchor.href);

        new_raws.push(RawAnchor {
            ts: ts.clone(),
            href: anchor.href.clone(),
            time_added: anchor.time_added_raw.clone(),
            tags_raw: anchor.tags_raw.clone(),
            title: anchor.title.clone(),
            state_raw: anchor.state.as_raw_label().to_string(),
        });

        new_items.push(Item {
            ts,
            source: "pocket".into(),
            guid,
            url: anchor.href,
            title: anchor.title,
            author: String::new(),
            site,
            feed: String::new(),
            excerpt: String::new(),
            tags,
            state: anchor.state.as_contract_state().to_string(),
            progress: None,
            read_at: String::new(),
            extra: Map::new(),
        });

        if idx % 200 == 0 {
            let pct = if total > 0 { idx as f32 / total as f32 * 100.0 } else { 0.0 };
            progress(ImportProgress { records: new_items.len() as u64, percent: pct });
        }
    }

    let imported = new_items.len() as u64;

    // Write contract rows, partitioned by saved-at month.
    item_stream.append(&new_items, |i| &i.ts)?;
    // Write raw rows, same partition.
    raw_stream.append(&new_raws, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} articles imported, {duplicates} duplicates skipped"
        ),
        counts: [("imported", imported), ("duplicates", duplicates), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers.

/// Extract the hostname from a URL for the `site` field.
///
/// `https://www.example.com/path?q=1` → `"www.example.com"`
/// Returns an empty string on failure.
fn extract_host(url: &str) -> String {
    // Strip scheme.
    let after_scheme = url
        .find("://")
        .map(|i| &url[i + 3..])
        .unwrap_or(url);
    // Take up to the first `/`, `?`, or `#`.
    let host_part = after_scheme
        .find(|c| c == '/' || c == '?' || c == '#')
        .map(|i| &after_scheme[..i])
        .unwrap_or(after_scheme);
    // Strip port.
    host_part
        .rfind(':')
        .map(|i| &host_part[..i])
        .unwrap_or(host_part)
        .to_string()
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-pocket-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixture: a minimal Pocket HTML export.
    //
    // Real ril_export.html format (confirmed from community archives):
    //   - <h1>Unread</h1> and <h1>Read Archive</h1> section headers
    //   - <a href="URL" time_added="UNIX_TS" tags="t1,t2">Title</a>
    //
    // The `time_added` values are real Unix timestamps:
    //   1609459200 = 2021-01-01T00:00:00Z
    //   1625097600 = 2021-07-01T00:00:00Z
    //   1640995200 = 2022-01-01T00:00:00Z

    const SAMPLE_HTML: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Pocket Export</TITLE>
<H1>Pocket Export</H1>

<ul><h1>Unread</h1>
<li><a href="https://example.com/article-one" time_added="1609459200" tags="tech,programming">Why Local-First Software Matters</a>
<li><a href="https://blog.example.org/two" time_added="1625097600" tags="">A Read-Later Classic (no tags)</a>
</ul>

<ul><h1>Read Archive</h1>
<li><a href="https://news.example.net/archive-article" time_added="1640995200" tags="history,archive">An Archived Article</a>
</ul>
"#;

    /// A duplicate of the first article (same href + time_added → same guid).
    const SAMPLE_HTML_DUP: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<ul><h1>Unread</h1>
<li><a href="https://example.com/article-one" time_added="1609459200" tags="tech,programming">Why Local-First Software Matters</a>
<li><a href="https://brand-new.example.com/new" time_added="1650000000" tags="new">Brand New Article</a>
</ul>
"#;

    fn do_import(v: &Vault, html: &str) -> ImportOutcome {
        let html_path = v.root().join("export.html");
        std::fs::write(&html_path, html).unwrap();
        (IMPORT.run)(v, &html_path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Unit tests: HTML parser.

    #[test]
    fn parse_anchor_extracts_href_time_added_tags_title() {
        let line = r#"<a href="https://example.com/article" time_added="1609459200" tags="tech,foo">Article Title</a>"#;
        let anchor = parse_anchor_line(line, AnchorState::Unread).unwrap();
        assert_eq!(anchor.href, "https://example.com/article");
        assert_eq!(anchor.time_added_raw, "1609459200");
        assert_eq!(anchor.tags_raw, "tech,foo");
        assert_eq!(anchor.title, "Article Title");
        assert_eq!(anchor.state, AnchorState::Unread);
    }

    #[test]
    fn parse_anchor_empty_tags_is_ok() {
        let line = r#"<a href="https://example.com/no-tags" time_added="1625097600" tags="">No Tags</a>"#;
        let anchor = parse_anchor_line(line, AnchorState::Archive).unwrap();
        assert!(anchor.tags_raw.is_empty(), "empty tags_raw preserved");
        assert_eq!(anchor.state, AnchorState::Archive);
    }

    #[test]
    fn parse_anchor_missing_href_returns_none() {
        let line = r#"<a time_added="1609459200" tags="foo">Title</a>"#;
        assert!(parse_anchor_line(line, AnchorState::Unread).is_none(), "no href → None");
    }

    #[test]
    fn parse_html_sections_set_state_correctly() {
        let anchors = parse_html(SAMPLE_HTML);
        assert_eq!(anchors.len(), 3, "three anchors parsed: {anchors:?}");
        // First two are in Unread section.
        assert_eq!(anchors[0].state, AnchorState::Unread);
        assert_eq!(anchors[1].state, AnchorState::Unread);
        // Third is in Read Archive.
        assert_eq!(anchors[2].state, AnchorState::Archive);
        assert_eq!(anchors[2].href, "https://news.example.net/archive-article");
    }

    #[test]
    fn unix_ts_to_local_converts_correctly() {
        // 1609459200 = 2021-01-01T00:00:00Z; local offset shifts the hour.
        let ts = unix_ts_to_local("1609459200");
        let dt = DateTime::parse_from_rfc3339(&ts).expect("should be valid RFC3339");
        assert_eq!(dt.timestamp(), 1_609_459_200, "instant preserved");
    }

    #[test]
    fn unix_ts_non_numeric_falls_back_to_epoch() {
        let ts = unix_ts_to_local("not-a-number");
        let dt = DateTime::parse_from_rfc3339(&ts).expect("should be valid RFC3339");
        // Falls back to epoch (0).
        assert_eq!(dt.timestamp(), 0);
    }

    #[test]
    fn make_guid_is_stable_and_distinct() {
        let g1 = make_guid("https://example.com", "1609459200");
        let g2 = make_guid("https://example.com", "1609459200");
        let g3 = make_guid("https://example.com", "1625097600");
        assert_eq!(g1, g2, "same inputs → same guid");
        assert_ne!(g1, g3, "different time_added → different guid");
        // sha256 hex is 64 chars.
        assert_eq!(g1.len(), 64);
    }

    #[test]
    fn extract_host_strips_scheme_and_path() {
        assert_eq!(extract_host("https://www.example.com/some/path?q=1"), "www.example.com");
        assert_eq!(extract_host("http://example.com:8080/page"), "example.com");
        assert_eq!(extract_host("https://news.ycombinator.com"), "news.ycombinator.com");
        assert_eq!(extract_host(""), "");
    }

    #[test]
    fn tags_split_correctly() {
        let anchors = parse_html(SAMPLE_HTML);
        // First article has "tech,programming".
        let tags_0: Vec<String> = anchors[0]
            .tags_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        assert_eq!(tags_0, vec!["tech", "programming"]);
        // Second has empty tags.
        assert!(anchors[1].tags_raw.is_empty());
    }

    // -----------------------------------------------------------------------
    // Integration tests: full import.

    #[test]
    fn full_import_writes_contract_and_raw_rows() {
        let v = temp_vault("full");
        let out = do_import(&v, SAMPLE_HTML);

        assert_eq!(out.counts["imported"], 3, "3 articles imported: {out:?}");
        assert_eq!(out.counts["duplicates"], 0);
        assert_eq!(out.counts["skipped"], 0);

        // Contract files partitioned by local month.
        let contract_dir = v.root().join("reading/pocket");
        let contract_files: Vec<_> = std::fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") { Some(n) } else { None }
            })
            .collect();
        // 3 articles across 3 different months (Jan 2021, Jul 2021, Jan 2022).
        assert_eq!(contract_files.len(), 3, "3 monthly partition files: {contract_files:?}");

        // Spot-check article-one: guid, url, title, state, tags, source.
        let all_items: Vec<Item> = {
            // Concatenate all contract JSONL files and deserialize.
            let bytes: Vec<u8> = contract_files
                .iter()
                .flat_map(|f| {
                    std::fs::read_to_string(contract_dir.join(f)).unwrap_or_default().into_bytes()
                })
                .collect();
            let content = String::from_utf8_lossy(&bytes).to_string();
            content
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        };
        let art_one = all_items
            .iter()
            .find(|i| i.url == "https://example.com/article-one")
            .expect("article-one should be present");
        assert_eq!(art_one.source, "pocket");
        assert_eq!(art_one.state, "saved", "Unread → saved");
        assert_eq!(art_one.tags, vec!["tech", "programming"]);
        assert_eq!(art_one.title, "Why Local-First Software Matters");
        assert_eq!(art_one.site, "example.com");
        // guid is sha256(href|time_added) — stable.
        assert_eq!(art_one.guid, make_guid("https://example.com/article-one", "1609459200"));

        // Archived article.
        let archived = all_items
            .iter()
            .find(|i| i.url == "https://news.example.net/archive-article")
            .expect("archived article should be present");
        assert_eq!(archived.state, "archived", "Read Archive → archived");

        // Raw layer exists with all 3 rows.
        let raw_dir = v.root().join("reading/pocket/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_count: usize = std::fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") {
                    Some(
                        std::fs::read_to_string(raw_dir.join(&n))
                            .unwrap_or_default()
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .count(),
                    )
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(raw_count, 3, "3 raw rows written: {raw_count}");
    }

    #[test]
    fn reimport_same_file_is_noop() {
        let v = temp_vault("noop");
        let out1 = do_import(&v, SAMPLE_HTML);
        assert_eq!(out1.counts["imported"], 3);

        let out2 = do_import(&v, SAMPLE_HTML);
        assert_eq!(out2.counts["imported"], 0, "re-import: no new rows");
        assert_eq!(out2.counts["duplicates"], 3, "all 3 flagged as duplicates");

        // No extra rows written.
        let contract_dir = v.root().join("reading/pocket");
        let total_rows: usize = std::fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") {
                    Some(
                        std::fs::read_to_string(contract_dir.join(&n))
                            .unwrap_or_default()
                            .lines()
                            .filter(|l| !l.trim().is_empty())
                            .count(),
                    )
                } else {
                    None
                }
            })
            .sum();
        assert_eq!(total_rows, 3, "no extra rows after re-import: {total_rows}");
    }

    #[test]
    fn second_import_superset_adds_only_new_articles() {
        let v = temp_vault("superset");
        let out1 = do_import(&v, SAMPLE_HTML);
        assert_eq!(out1.counts["imported"], 3);

        // SAMPLE_HTML_DUP has article-one (dup) + brand-new.
        let out2 = do_import(&v, SAMPLE_HTML_DUP);
        assert_eq!(out2.counts["imported"], 1, "only brand-new article added");
        assert_eq!(out2.counts["duplicates"], 1, "article-one deduplicated");
    }

    #[test]
    fn empty_html_is_ok() {
        let v = temp_vault("empty");
        let empty = "<!DOCTYPE NETSCAPE-Bookmark-file-1>\n<H1>Pocket Export</H1>\n";
        let out = do_import(&v, empty);
        assert_eq!(out.counts["imported"], 0);
        assert_eq!(out.counts["duplicates"], 0);
        // No vault files created (no rows to write).
    }

    #[test]
    fn last_data_reflects_newest_partition_after_import() {
        let v = temp_vault("lastdata");
        assert!(def_last_data(&v).is_none(), "no data before import");
        do_import(&v, SAMPLE_HTML);
        let ld = def_last_data(&v).unwrap();
        // Should be the newest month (2022-01 or 2021-12 depending on local TZ).
        assert!(!ld.is_empty(), "last_data present after import: {ld}");
        // Must be at least 7 chars (YYYY-MM).
        assert!(ld.len() >= 7);
    }

    #[test]
    fn def_import_spec_accepts_html_and_htm() {
        assert!(IMPORT.accepts.contains(&"html"), "html accepted");
        assert!(IMPORT.accepts.contains(&"htm"), "htm accepted");
    }

    #[test]
    fn contract_items_have_no_extra_fields_for_pocket() {
        let v = temp_vault("noextra");
        do_import(&v, SAMPLE_HTML);
        let contract_dir = v.root().join("reading/pocket");
        for entry in std::fs::read_dir(&contract_dir).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            if !name.ends_with(".jsonl") {
                continue;
            }
            for line in
                std::fs::read_to_string(contract_dir.join(&name)).unwrap().lines()
            {
                if line.trim().is_empty() {
                    continue;
                }
                let v: Item = serde_json::from_str(line).expect("valid Item JSON");
                // Pocket format has no per-article extra data.
                assert!(v.extra.is_empty(), "extra should be empty for Pocket items");
            }
        }
    }
}

