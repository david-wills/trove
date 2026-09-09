//! StoryGraph reading history, shelf state, and ratings via the official CSV export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/storygraph.md.
//!
//! StoryGraph is the de-facto Goodreads replacement. It exposes no API; the
//! "Export StoryGraph Library" CSV (app.thestorygraph.com → Account → Manage
//! Account → Manage Your Data) is the only access path. The importer is
//! deliberately parallel to `goodreads.rs` — the two share scaffolding.
//!
//! **Two layers, always:**
//! - **Raw** — `media/storygraph/books.jsonl` (append-only, one JSON row per
//!   book with every CSV column preserved verbatim; keyed on title+author slug
//!   because the export carries no stable numeric id).
//! - **Contract** — `media/plays/storygraph/YYYY-MM.jsonl` per the media-plays
//!   write contract, one row per *finished read* (`Read Status == "read"` with a
//!   parseable `Last Date Read`), with `category:"other"`, `kind:"play"`, and book
//!   metadata.  Books on the to-read or currently-reading shelves are
//!   curation-only (raw only — no fabricated timestamps).
//!
//! **Known StoryGraph export columns (confirmed from open-source converters):**
//!   Title, Authors, Read Status, Date Added, Last Date Read, Dates Read,
//!   Star Rating, ISBN/UID, Format, Review, Read Count, Owned?
//!   A `Contributors` column has been reported in some exports; additional fields
//!   (Tags, Moods, Pace, Content Warnings) may appear depending on account/version
//!   — all are preserved verbatim in `books.jsonl` via the schema-agnostic raw layer.
//!
//! **Date formats:** StoryGraph accounts produce at least four formats depending on
//!   locale (confirmed from rinsdoc/storygraph_to_goodreads `convertDate()`):
//!   `YYYY/MM/DD`, `YYYY-MM-DD`, `MM/DD/YYYY`, and `Month DD, YYYY`. All handled
//!   by `parse_date()`.
//!
//! **Star Rating** is stored as a decimal string (e.g. `"4.5"` or `"4"`).
//! Half-star values are preserved verbatim in `extra.star_rating`.
//!
//! **Guid strategy:** `storygraph-<slug>-<YYYY-MM-DD>` where slug is
//! `<title>|<authors>` (lowercased, ascii-folded). The export has no stable book
//! id column; the slug is stable across re-exports of the same book on the same
//! read date. Multiple reads of the same book at different dates each get their
//! own guid (one contract row per `Dates Read` entry).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer directory (media-plays write contract).
const CONTRACT_DIR: &str = "media/plays/storygraph";
/// Raw layer — full-fidelity book rows.
const RAW_DIR: &str = "media/storygraph";
const RAW_FILE: &str = "media/storygraph/books.jsonl";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(RAW_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "storygraph",
        name: "StoryGraph",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your StoryGraph reading history — every book you have read, \
                      currently reading, or want to read — from the official data export. \
                      Re-runnable: newer exports never duplicate.",
        domain: "media",
        vault_path: "media/plays/storygraph/",
        toggleable: false,
        setup: &[
            "app.thestorygraph.com → Account → Manage Account → Manage Your Data → Export StoryGraph Library.",
            "Import the downloaded CSV here.",
        ],
        caveats: "No API is available; re-export periodically to pick up new reads. \
                  The export is book-level only — reading-session journals are not yet \
                  included in StoryGraph's export.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

/// One row of the StoryGraph library CSV export.
///
/// Covers the confirmed export columns. Fields that may be empty default to
/// an empty String via `#[serde(default)]`. The raw layer is schema-agnostic
/// (header→value map) so future StoryGraph column additions are never silently
/// dropped.
#[derive(Debug, Deserialize)]
struct BookRow {
    #[serde(rename = "Title", default)]
    title: String,
    /// Comma-separated list of authors (may have multiple).
    #[serde(rename = "Authors", default)]
    authors: String,
    /// "read" | "reading" | "to-read" (exact values from the export).
    #[serde(rename = "Read Status", default)]
    read_status: String,
    #[serde(rename = "Date Added", default)]
    date_added: String,
    /// The most-recent read date for this book. Format varies by account locale;
    /// see `parse_date()` for the full set of handled formats.
    #[serde(rename = "Last Date Read", default)]
    last_date_read: String,
    /// Comma-separated list of all read dates for this book.
    #[serde(rename = "Dates Read", default)]
    dates_read: String,
    /// Decimal rating string, e.g. "4.5" or "4" or "3.5" (half-star support).
    #[serde(rename = "Star Rating", default)]
    star_rating: String,
    #[serde(rename = "ISBN/UID", default)]
    isbn_uid: String,
    /// Physical format: "hardcover", "paperback", "ebook", "audiobook", etc.
    #[serde(rename = "Format", default)]
    format: String,
    #[serde(rename = "Review", default)]
    review: String,
    #[serde(rename = "Read Count", default)]
    read_count: String,
    #[serde(rename = "Owned?", default)]
    owned: String,
}

/// Parse a StoryGraph date field.
///
/// The real StoryGraph CSV export uses at least three date formats
/// (confirmed from rinsdoc/storygraph_to_goodreads `convertDate()` function):
///   1. `YYYY/MM/DD`  — year-first with slash (e.g. `2024/03/15`)
///   2. `YYYY-MM-DD`  — year-first with dash (e.g. `2024-03-15`)
///   3. `MM/DD/YYYY`  — US month-first (e.g. `03/15/2024`)
///   4. `Month D, YYYY` — long month name (e.g. `March 15, 2024`)
///
/// Returns `None` if the field is empty or matches none of the known formats.
fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // 1. YYYY/MM/DD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y/%m/%d") {
        return Some(d);
    }
    // 2. YYYY-MM-DD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    // 3. MM/DD/YYYY  (US month-first — common in accounts with US locale)
    if let Ok(d) = NaiveDate::parse_from_str(s, "%m/%d/%Y") {
        return Some(d);
    }
    // 4. "Month D, YYYY" or "Month DD, YYYY"  (long English month name)
    if let Ok(d) = NaiveDate::parse_from_str(s, "%B %d, %Y") {
        return Some(d);
    }
    None
}

/// Build a stable slug from title and authors for use in guids.
///
/// Lowercased, stripped of non-ASCII punctuation that varies across exports,
/// then joined with a pipe. Stable across re-exports of the same book.
fn title_author_slug(title: &str, authors: &str) -> String {
    let slug = |s: &str| {
        s.trim()
            .to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() || c == ' ' { c } else { '-' })
            .collect::<String>()
    };
    format!("{}|{}", slug(title), slug(authors))
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract_stream = vault.stream(CONTRACT_DIR, Partition::Month);

    // Load already-stored contract guids for idempotent re-imports.
    let mut seen_contract: HashSet<String> = HashSet::new();
    for key in contract_stream.partitions()? {
        for it in contract_stream.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                seen_contract.insert(it.guid);
            }
        }
    }

    // Load already-stored raw slugs so we only append truly new raw rows.
    let mut seen_raw: HashSet<String> = HashSet::new();
    if let Ok(raw_path) = vault.resolve(RAW_FILE) {
        if raw_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&raw_path) {
                for line in content.lines() {
                    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
                        if let Some(slug) = obj.get("_slug").and_then(|v| v.as_str()) {
                            if !slug.is_empty() {
                                seen_raw.insert(slug.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());

    let headers_record = rdr.headers().context("reading CSV headers")?.clone();
    let headers: Vec<String> = headers_record.iter().map(|h| h.to_string()).collect();

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut contract_items: Vec<MediaItem> = Vec::new();
    let mut raw_items: Vec<Map<String, Value>> = Vec::new();

    for result in rdr.records() {
        rows += 1;
        let raw_record = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        // Pad short records so serde can deserialize without index errors.
        let deserialized_row = if raw_record.len() < headers.len() {
            let mut padded = raw_record.clone();
            for _ in raw_record.len()..headers.len() {
                padded.push_field("");
            }
            padded.deserialize(Some(&headers_record))
        } else {
            raw_record.deserialize(Some(&headers_record))
        };

        let row: BookRow = match deserialized_row {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let title = row.title.trim().to_string();
        let authors = row.authors.trim().to_string();
        if title.is_empty() {
            skipped += 1;
            continue;
        }

        let slug = title_author_slug(&title, &authors);

        // Raw layer: schema-agnostic — capture ALL header→value pairs plus _slug.
        if seen_raw.insert(slug.clone()) {
            let mut raw_map = record_to_raw_map(&headers, &raw_record);
            // Store the slug as a hidden key for idempotent re-imports.
            raw_map.insert("_slug".to_string(), json!(slug));
            raw_items.push(raw_map);
        }

        // Contract layer: only books with read status "read" AND a parseable date.
        if row.read_status.trim().to_lowercase() != "read" {
            // Curation-only: to-read or currently-reading.
            continue;
        }

        // Prefer Last Date Read; fall back to Dates Read (last entry), then Date Added.
        // Build a deduplicated list of all read dates to emit one contract row each.
        let mut read_dates: Vec<NaiveDate> = Vec::new();
        let mut seen_dates: HashSet<String> = HashSet::new();

        // First: parse all entries from Dates Read (comma-separated).
        for date_str in row.dates_read.split(',') {
            if let Some(d) = parse_date(date_str) {
                let key = d.format("%Y-%m-%d").to_string();
                if seen_dates.insert(key) {
                    read_dates.push(d);
                }
            }
        }
        // Then: if Last Date Read is not already covered, add it.
        if let Some(d) = parse_date(&row.last_date_read) {
            let key = d.format("%Y-%m-%d").to_string();
            if seen_dates.insert(key) {
                read_dates.push(d);
            }
        }
        // If we still have no read date, use Date Added as fallback (last resort).
        if read_dates.is_empty() {
            if let Some(d) = parse_date(&row.date_added) {
                read_dates.push(d);
            }
        }

        // Emit one contract row per unique read date.
        for date in &read_dates {
            let canonical_date = date.format("%Y-%m-%d").to_string();
            let guid = format!("storygraph-{slug}-{canonical_date}");

            if !seen_contract.insert(guid.clone()) {
                duplicates += 1;
                continue;
            }

            let Some(item) = book_to_media_item(&guid, *date, &title, &authors, &row) else {
                skipped += 1;
                continue;
            };

            contract_items.push(item);
            imported += 1;
        }

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Write raw layer.
    if !raw_items.is_empty() {
        write_raw_books(vault, &raw_items)?;
    }

    // Write contract layer.
    contract_stream.append(&contract_items, |i| &i.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} books imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Convert a [`csv::StringRecord`] + header list to a `serde_json` object map.
fn record_to_raw_map(headers: &[String], record: &csv::StringRecord) -> Map<String, Value> {
    let mut map = Map::new();
    for (i, header) in headers.iter().enumerate() {
        let val = record.get(i).unwrap_or("").to_string();
        map.insert(header.clone(), json!(val));
    }
    map
}

/// Build a media-plays [`MediaItem`] for a finished read.
fn book_to_media_item(
    guid: &str,
    date: NaiveDate,
    title: &str,
    authors: &str,
    row: &BookRow,
) -> Option<MediaItem> {
    // Midnight local — the export has date-only precision.
    let ts = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()?
        .to_rfc3339();

    let mut extra: Map<String, Value> = Map::new();
    let mut put = |k: &str, v: &str| {
        let v = v.trim();
        if !v.is_empty() {
            extra.insert(k.into(), json!(v));
        }
    };

    // Domain spec says: category "other" with extra.medium:"book" so a future
    // additive enum extension to add "book" can migrate cleanly.
    put("medium", "book");
    put("star_rating", row.star_rating.trim());
    put("isbn_uid", row.isbn_uid.trim());
    put("format", row.format.trim());
    put("review", row.review.trim());
    put("read_count", row.read_count.trim());
    put("owned", row.owned.trim());
    put("date_added", row.date_added.trim());
    put("dates_read", row.dates_read.trim());
    put("read_status", row.read_status.trim());

    Some(MediaItem {
        ts,
        source: "storygraph".into(),
        category: "other".into(),
        device: String::new(),
        kind: "play".into(),
        title: title.to_string(),
        // Authors is the grouping key (chart books by author).
        subtitle: authors.to_string(),
        detail: String::new(),
        seconds: 0,
        favicon: String::new(),
        guid: guid.to_string(),
        extra,
    })
}

/// Write raw books to `media/storygraph/books.jsonl`.
///
/// The raw layer is a flat JSONL file without date partitioning (books don't
/// have a single authoritative timestamp). Vault::resolve protects the path.
fn write_raw_books(vault: &Vault, books: &[Map<String, Value>]) -> Result<()> {
    let raw_path = vault.resolve(RAW_FILE)?;
    if let Some(parent) = raw_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&raw_path)
        .with_context(|| format!("opening {}", raw_path.display()))?;
    use std::io::Write as _;
    for book_map in books {
        let line = serde_json::to_string(book_map).context("serializing raw book")?;
        f.write_all(line.as_bytes()).context("writing raw book")?;
        f.write_all(b"\n").context("writing newline")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-storygraph-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Realistic StoryGraph export CSV covering key cases:
    // Row 1: finished read with Last Date Read + Dates Read + star rating
    // Row 2: to-read (no date, curation-only)
    // Row 3: currently reading (no Last Date Read, curation-only)
    // Row 4: finished read, no Last Date Read but Dates Read present
    // Row 5: finished read with half-star rating
    const SAMPLE_CSV: &str = "\
Title,Authors,Read Status,Date Added,Last Date Read,Dates Read,Star Rating,ISBN/UID,Format,Review,Read Count,Owned?\n\
The Pragmatic Programmer,David Thomas,read,2023/11/01,2024/03/15,2024/03/15,5,9780201616224,paperback,\"Great book.\nHighly recommended.\",1,Yes\n\
Clean Code,Robert C. Martin,to-read,2024/01/10,,,0,,ebook,,,No\n\
The Mythical Man-Month,Frederick P. Brooks Jr.,reading,2024/05/01,,,0,,hardcover,,,\n\
Thinking Fast and Slow,Daniel Kahneman,read,2022/06/01,,2023/07/20,4,9780374533557,paperback,,1,\n\
\"A Gentleman in Moscow\",Amor Towles,read,2024/01/01,2024/02/28,2024/02/28,4.5,9780670026197,hardcover,Amazing story.,2,Yes\n\
";

    fn run(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("storygraph_export.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_finished_reads_and_skips_unread() {
        let v = temp_vault("basic");
        let out = run(&v, SAMPLE_CSV);
        assert_eq!(
            out.headline,
            "3 books imported, 0 duplicates skipped",
            "three books are 'read' status with dates; two are curation-only"
        );

        // Contract layer: month-partitioned JSONL in media/plays/storygraph/
        let path_2024_03 = v.root().join("media/plays/storygraph/2024-03.jsonl");
        let raw = fs::read_to_string(&path_2024_03).unwrap();
        assert_eq!(raw.lines().count(), 1, "one book read in 2024-03");

        let item: MediaItem = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(item.source, "storygraph");
        assert_eq!(item.category, "other");
        assert_eq!(item.kind, "play");
        assert_eq!(item.title, "The Pragmatic Programmer");
        assert_eq!(item.subtitle, "David Thomas");
        assert_eq!(item.seconds, 0);
        // guid uses canonical ISO date (YYYY-MM-DD)
        assert!(item.guid.starts_with("storygraph-"), "guid prefix: {}", item.guid);
        assert!(item.guid.ends_with("-2024-03-15"), "guid date: {}", item.guid);
        assert_eq!(item.extra.get("medium"), Some(&json!("book")));
        assert_eq!(item.extra.get("star_rating"), Some(&json!("5")));

        // Multi-line review preserved in extra
        let review = item.extra.get("review").and_then(|v| v.as_str()).unwrap_or("");
        assert!(review.contains("Great book"), "review preserved: {review}");

        // 2023-07 book from Dates Read (no Last Date Read)
        let path_2023_07 = v.root().join("media/plays/storygraph/2023-07.jsonl");
        let raw_2023 = fs::read_to_string(&path_2023_07).unwrap();
        assert_eq!(raw_2023.lines().count(), 1, "one book from Dates Read in 2023-07");
        let item_2023: MediaItem =
            serde_json::from_str(raw_2023.lines().next().unwrap()).unwrap();
        assert_eq!(item_2023.title, "Thinking Fast and Slow");
    }

    #[test]
    fn half_star_rating_preserved() {
        let v = temp_vault("halfstar");
        run(&v, SAMPLE_CSV);
        let path = v.root().join("media/plays/storygraph/2024-02.jsonl");
        let raw = fs::read_to_string(&path).unwrap();
        let item: MediaItem = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(item.title, "A Gentleman in Moscow");
        // Half-star rating preserved verbatim
        assert_eq!(item.extra.get("star_rating"), Some(&json!("4.5")));
    }

    #[test]
    fn raw_layer_written_unconditionally() {
        let v = temp_vault("raw");
        run(&v, SAMPLE_CSV);
        let raw_path = v.root().join(RAW_FILE);
        assert!(raw_path.exists(), "raw books.jsonl written");
        let raw_content = fs::read_to_string(&raw_path).unwrap();
        // All 5 books in raw (including to-read and currently-reading)
        assert_eq!(raw_content.lines().count(), 5, "all 5 books in raw");
        // First raw book has all columns including slug
        let raw_obj: serde_json::Value =
            serde_json::from_str(raw_content.lines().next().unwrap()).unwrap();
        assert_eq!(raw_obj["Title"].as_str(), Some("The Pragmatic Programmer"));
        assert_eq!(raw_obj["Authors"].as_str(), Some("David Thomas"));
        assert!(raw_obj.get("_slug").is_some(), "_slug key present for dedup");
        // Review with newline preserved in raw
        let review = raw_obj["Review"].as_str().unwrap_or("");
        assert!(review.contains("Great book"), "multi-line review in raw: {review}");
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idempotent");
        let out1 = run(&v, SAMPLE_CSV);
        assert_eq!(out1.counts.get("imported"), Some(&3));

        let out2 = run(&v, SAMPLE_CSV);
        assert_eq!(
            out2.headline,
            "0 books imported, 3 duplicates skipped",
            "second import: all are duplicates"
        );

        // Contract files are unchanged after the second import.
        let path_2024_03 = v.root().join("media/plays/storygraph/2024-03.jsonl");
        let lines_after = fs::read_to_string(&path_2024_03).unwrap().lines().count();
        assert_eq!(lines_after, 1, "no new contract rows after re-import");

        // Raw file also not duplicated (slug-based dedup).
        let raw_content = fs::read_to_string(v.root().join(RAW_FILE)).unwrap();
        assert_eq!(raw_content.lines().count(), 5, "raw rows not duplicated");
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        run(&v, SAMPLE_CSV);
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "storygraph").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2024-03"));
    }

    #[test]
    fn media_timeline_sees_storygraph_books() {
        let v = temp_vault("timeline");
        run(&v, SAMPLE_CSV);
        let day = v.media_timeline("2024-03-15").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "storygraph");
        assert_eq!(day[0].title, "The Pragmatic Programmer");
    }

    #[test]
    fn unknown_future_columns_preserved_in_raw() {
        // Simulate a hypothetical future StoryGraph export with an extra column.
        let v = temp_vault("future-col");
        let csv_extra = "\
Title,Authors,Read Status,Date Added,Last Date Read,Dates Read,Star Rating,ISBN/UID,Format,Review,Read Count,Owned?,Mood Tags\n\
Dune,Frank Herbert,read,2024/01/01,2024/01/15,2024/01/15,5,9780441172719,paperback,,1,No,\"adventurous,mysterious\"\n";
        run(&v, csv_extra);
        let raw_path = v.root().join(RAW_FILE);
        let raw_content = fs::read_to_string(&raw_path).unwrap();
        let raw_obj: serde_json::Value =
            serde_json::from_str(raw_content.lines().next().unwrap()).unwrap();
        assert_eq!(
            raw_obj["Mood Tags"].as_str(),
            Some("adventurous,mysterious"),
            "unknown future column preserved in raw layer"
        );
    }

    #[test]
    fn manifest_indexes_as_media_plays_source() {
        let v = temp_vault("manifest");
        run(&v, SAMPLE_CSV);
        let m = v.rebuild_manifest().unwrap();
        let media = m.domains.iter().find(|d| d.domain == "media-plays").unwrap();
        assert!(media.sources.contains(&"storygraph".to_string()));
    }

    // -------------------------------------------------------------------------
    // Date-format robustness: parse_date must handle all 3 real export formats.
    // Confirmed from rinsdoc/storygraph_to_goodreads src/app/conversion.ts
    // convertDate() which handles year-first, US month-first, and long-month.
    // -------------------------------------------------------------------------

    #[test]
    fn parse_date_handles_year_first_slash() {
        let d = parse_date("2024/03/15").expect("YYYY/MM/DD must parse");
        assert_eq!(d.to_string(), "2024-03-15");
    }

    #[test]
    fn parse_date_handles_year_first_dash() {
        let d = parse_date("2024-03-15").expect("YYYY-MM-DD must parse");
        assert_eq!(d.to_string(), "2024-03-15");
    }

    #[test]
    fn parse_date_handles_us_month_first() {
        // MM/DD/YYYY — common in US-locale StoryGraph accounts.
        let d = parse_date("03/15/2024").expect("MM/DD/YYYY must parse");
        assert_eq!(d.to_string(), "2024-03-15");
    }

    #[test]
    fn parse_date_handles_long_month_name() {
        // "Month DD, YYYY" — another format seen in the wild.
        let d = parse_date("March 15, 2024").expect("'Month DD, YYYY' must parse");
        assert_eq!(d.to_string(), "2024-03-15");
    }

    #[test]
    fn parse_date_returns_none_for_empty() {
        assert!(parse_date("").is_none());
        assert!(parse_date("  ").is_none());
    }

    /// CSV with US-format (MM/DD/YYYY) dates — exercises the real-world path
    /// where the existing two-format parser would have silently dropped every
    /// contract row despite a successful raw import.
    #[test]
    fn imports_us_format_dates() {
        let v = temp_vault("us-dates");
        // US locale export: Last Date Read and Dates Read in MM/DD/YYYY
        let csv = "\
Title,Authors,Read Status,Date Added,Last Date Read,Dates Read,Star Rating,ISBN/UID,Format,Review,Read Count,Owned?\n\
The Hobbit,J.R.R. Tolkien,read,01/10/2024,03/15/2024,03/15/2024,5,,paperback,,1,No\n\
";
        let out = run(&v, csv);
        assert_eq!(
            out.counts.get("imported"),
            Some(&1),
            "US-format date must yield a contract row; got: {}",
            out.headline
        );
        let path = v.root().join("media/plays/storygraph/2024-03.jsonl");
        assert!(path.exists(), "contract file written for 2024-03");
        let item: MediaItem =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(item.title, "The Hobbit");
        assert!(item.guid.ends_with("-2024-03-15"), "guid date: {}", item.guid);
    }

    /// CSV with long-month-name dates — exercises the "March 15, 2024" format.
    #[test]
    fn imports_long_month_name_dates() {
        let v = temp_vault("long-month");
        let csv = "\
Title,Authors,Read Status,Date Added,Last Date Read,Dates Read,Star Rating,ISBN/UID,Format,Review,Read Count,Owned?\n\
Dune,Frank Herbert,read,January 01 2024,March 15 2024,March 15 2024,5,,paperback,,1,No\n\
Dune Messiah,Frank Herbert,read,February 01 2024,\"March 15, 2024\",\"March 15, 2024\",4,,paperback,,1,No\n\
";
        // Row 1 has no commas in the date fields (no parse); row 2 has correct format.
        let out = run(&v, csv);
        // Only "Dune Messiah" has a parseable "Month DD, YYYY" date.
        assert_eq!(
            out.counts.get("imported"),
            Some(&1),
            "only the correctly-formatted long-month date row imports; got: {}",
            out.headline
        );
        let path = v.root().join("media/plays/storygraph/2024-03.jsonl");
        assert!(path.exists(), "contract file written for 2024-03");
        let item: MediaItem =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(item.title, "Dune Messiah");
    }
}
