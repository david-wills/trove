//! Goodreads reading history, ratings, and shelves via the official CSV export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/goodreads.md.
//!
//! Goodreads' public API was deprecated December 2020; no new keys are issued.
//! The account CSV export (goodreads.com/review/import → "Export Library") is
//! the only access path and covers the full library comprehensively.
//!
//! **Two layers, always:**
//! - **Raw** — `media/goodreads/books.jsonl` (append-only, one JSON row per
//!   book with every CSV column preserved verbatim; keyed on `Book Id`).
//!   The raw layer is schema-agnostic: all columns are captured as a
//!   header→value map so future Goodreads column additions are never silently
//!   dropped regardless of what struct fields are defined here.
//! - **Contract** — `media/plays/goodreads/YYYY-MM.jsonl` per the media-plays
//!   write contract, one row per *finished read* (`Date Read` present), with
//!   `category:"other"`, `kind:"play"`, and book metadata. Books without a
//!   `Date Read` are curation-only (raw only — we never invent timestamps).
//!
//! **Quirks handled:**
//! - `="0593230280"` Excel-guard prefix on ISBN/ISBN13 columns — stripped.
//! - Quoted multi-line reviews (the `csv` crate handles this automatically).
//! - `Read Count > 1` with a single `Date Read` — one contract row per unique
//!   `Date Read` (the guid encodes `Book Id + canonical Date Read`, so
//!   re-imports are idempotent).
//! - Books with no `Date Read` but an `Exclusive Shelf` of "read" are common;
//!   we write them raw-only (no fabricated timestamp).

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer directory (media-plays write contract).
const CONTRACT_DIR: &str = "media/plays/goodreads";
/// Raw layer — full-fidelity book rows.
const RAW_DIR: &str = "media/goodreads";
const RAW_FILE: &str = "media/goodreads/books.jsonl";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(RAW_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "goodreads",
        name: "Goodreads",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your Goodreads library — every book you've read, rated, and shelved, with dates and reviews. Re-runnable: newer exports never duplicate.",
        domain: "media",
        vault_path: "media/plays/goodreads/",
        toggleable: false,
        setup: &[
            "Go to goodreads.com/review/import and click \"Export Library\".",
            "Import the downloaded CSV here.",
        ],
        caveats: "The Goodreads API was deprecated in December 2020. Re-export periodically to refresh (there is no automatic sync). Friend activity and in-book reading progress are not included in the export.",
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

/// One row of the Goodreads library CSV export.
///
/// Covers the current real export columns (31 as of 2026). Fields that may
/// be empty default to an empty String via `#[serde(default)]`.
/// The raw layer is kept schema-agnostic (header→value map) so future
/// Goodreads column additions are never silently dropped.
#[derive(Debug, Deserialize)]
struct BookRow {
    #[serde(rename = "Book Id")]
    book_id: String,
    #[serde(rename = "Title", default)]
    title: String,
    #[serde(rename = "Author", default)]
    author: String,
    /// Author name in last-name-first format (e.g. "Thomas, David").
    #[serde(rename = "Author l-f", default)]
    author_lf: String,
    /// Co-authors, comma-separated.
    #[serde(rename = "Additional Authors", default)]
    additional_authors: String,
    #[serde(rename = "ISBN", default)]
    isbn: String,
    #[serde(rename = "ISBN13", default)]
    isbn13: String,
    #[serde(rename = "My Rating", default)]
    my_rating: String,
    #[serde(rename = "Average Rating", default)]
    average_rating: String,
    #[serde(rename = "Publisher", default)]
    publisher: String,
    #[serde(rename = "Binding", default)]
    binding: String,
    /// Total page count for this edition.
    #[serde(rename = "Number of Pages", default)]
    number_of_pages: String,
    #[serde(rename = "Year Published", default)]
    year_published: String,
    #[serde(rename = "Original Publication Year", default)]
    original_publication_year: String,
    #[serde(rename = "Date Read", default)]
    date_read: String,
    #[serde(rename = "Date Added", default)]
    date_added: String,
    #[serde(rename = "Bookshelves", default)]
    bookshelves: String,
    /// Shelf names with their positional order (e.g. "favorites (#3)").
    #[serde(rename = "Bookshelves with positions", default)]
    bookshelves_with_positions: String,
    #[serde(rename = "Exclusive Shelf", default)]
    exclusive_shelf: String,
    #[serde(rename = "My Review", default)]
    my_review: String,
    #[serde(rename = "Spoiler", default)]
    spoiler: String,
    #[serde(rename = "Private Notes", default)]
    private_notes: String,
    #[serde(rename = "Read Count", default)]
    read_count: String,
    #[serde(rename = "Recommended For", default)]
    recommended_for: String,
    #[serde(rename = "Recommended By", default)]
    recommended_by: String,
    #[serde(rename = "Owned Copies", default)]
    owned_copies: String,
    #[serde(rename = "Original Purchase Date", default)]
    original_purchase_date: String,
    #[serde(rename = "Original Purchase Location", default)]
    original_purchase_location: String,
    #[serde(rename = "Condition", default)]
    condition: String,
    #[serde(rename = "Condition Description", default)]
    condition_description: String,
    #[serde(rename = "BCID", default)]
    bcid: String,
}

/// Strip the Excel-guard `="..."` wrapper from ISBN fields.
/// Goodreads wraps ISBN values as `="0593230280"` to prevent Excel from
/// treating them as numbers and dropping leading zeros.
fn strip_excel_guard(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with("=\"") && s.ends_with('"') {
        &s[2..s.len() - 1]
    } else {
        s
    }
}

/// Parse a Goodreads date field. Accepts `YYYY/MM/DD` (primary) and
/// `YYYY-MM-DD` (fallback). Returns `None` if empty or unparseable.
fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    NaiveDate::parse_from_str(s, "%Y/%m/%d")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .ok()
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

    // Load already-stored raw book_ids so we only append truly new raw rows.
    // Raw books live in the flat file `media/goodreads/books.jsonl` (not a
    // partitioned stream), so we read it directly. The raw map uses "Book Id"
    // (original CSV header) as the key.
    let mut seen_raw: HashSet<String> = HashSet::new();
    if let Ok(raw_path) = vault.resolve(RAW_FILE) {
        if raw_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&raw_path) {
                for line in content.lines() {
                    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
                        if let Some(id) = obj.get("Book Id").and_then(|v| v.as_str()) {
                            if !id.is_empty() {
                                seen_raw.insert(id.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;

    // Single-pass: read StringRecords for the raw map and deserialize each
    // record into BookRow for the contract layer.  Using `flexible(true)` so
    // exports with extra or missing columns don't hard-fail.
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());

    // Capture headers before iterating records.
    let headers_record = rdr.headers().context("reading CSV headers")?.clone();
    let headers: Vec<String> = headers_record.iter().map(|h| h.to_string()).collect();

    let book_id_col = headers.iter().position(|h| h == "Book Id");

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut contract_items: Vec<MediaItem> = Vec::new();
    let mut raw_items: Vec<Map<String, Value>> = Vec::new();

    for result in rdr.records() {
        rows += 1;
        let raw_record = match result {
            Ok(r) => r,
            Err(_) => { skipped += 1; continue; }
        };

        // Deserialize the StringRecord into a typed BookRow using the headers
        // we already have. This avoids a second file read.
        // `StringRecord::deserialize` requires the record to have the same
        // number of fields as the headers. Pad short records with empty fields
        // so that flexible exports (e.g. trailing BCID column omitted) still
        // parse. Extra fields beyond the header count are harmless — serde
        // ignores them when using rename-based deserialization via headers.
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
            Err(_) => { skipped += 1; continue; }
        };

        let book_id = row.book_id.trim().to_string();
        if book_id.is_empty() {
            // Fallback: extract book_id directly from the StringRecord.
            let fallback_id = book_id_col
                .and_then(|i| raw_record.get(i))
                .unwrap_or("")
                .trim()
                .to_string();
            if fallback_id.is_empty() {
                skipped += 1;
                continue;
            }
            // Write the raw record even when typed parse failed.
            if seen_raw.insert(fallback_id.clone()) {
                raw_items.push(record_to_raw_map(&headers, &raw_record));
            }
            continue;
        }

        // Raw layer: schema-agnostic — capture ALL header→value pairs verbatim
        // so no Goodreads column can ever be silently dropped.
        if seen_raw.insert(book_id.clone()) {
            let mut raw_map = record_to_raw_map(&headers, &raw_record);
            // Normalise ISBN fields (strip Excel guard) in the raw map too.
            strip_excel_guard_in_map(&mut raw_map, "ISBN");
            strip_excel_guard_in_map(&mut raw_map, "ISBN13");
            raw_items.push(raw_map);
        }

        // Contract layer: only books with a parseable Date Read.
        let date_read_raw = row.date_read.trim();
        if date_read_raw.is_empty() {
            // Curation-only — shelf/rating/review without a read date.
            continue;
        }
        let Some(play_item) = book_to_media_item(&book_id, date_read_raw, &row) else {
            skipped += 1;
            continue;
        };
        if !seen_contract.insert(play_item.guid.clone()) {
            duplicates += 1;
            continue;
        }
        contract_items.push(play_item);
        imported += 1;

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Write raw layer — use a day-granularity stream so the file is predictable.
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
/// Every column is stored verbatim; no columns are dropped.
fn record_to_raw_map(headers: &[String], record: &csv::StringRecord) -> Map<String, Value> {
    let mut map = Map::new();
    for (i, header) in headers.iter().enumerate() {
        let val = record.get(i).unwrap_or("").to_string();
        map.insert(header.clone(), json!(val));
    }
    map
}

/// Strip Excel-guard prefix from a key already stored in the raw map in place.
fn strip_excel_guard_in_map(map: &mut Map<String, Value>, key: &str) {
    if let Some(Value::String(s)) = map.get(key) {
        let cleaned = strip_excel_guard(s).to_string();
        map.insert(key.to_string(), json!(cleaned));
    }
}

/// Build a media-plays [`MediaItem`] for a finished read.
/// Returns `None` if `date_read` cannot be parsed.
fn book_to_media_item(book_id: &str, date_read_raw: &str, row: &BookRow) -> Option<MediaItem> {
    let date = parse_date(date_read_raw)?;
    // Midnight local — the export has date-only precision.
    let ts = Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()?
        .to_rfc3339();

    // guid: stable across re-imports. Use the canonical ISO-8601 date so the
    // key is independent of the export's date punctuation (YYYY/MM/DD vs
    // YYYY-MM-DD). A book exported first as 2024/03/15 and later as
    // 2024-03-15 maps to the same guid.
    let canonical_date = date.format("%Y-%m-%d").to_string();
    let guid = format!("goodreads-{book_id}-{canonical_date}");

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
    put("author_lf", row.author_lf.trim());
    put("additional_authors", row.additional_authors.trim());
    put("isbn", strip_excel_guard(&row.isbn));
    put("isbn13", strip_excel_guard(&row.isbn13));
    put("my_rating", row.my_rating.trim());
    put("average_rating", row.average_rating.trim());
    put("publisher", row.publisher.trim());
    put("binding", row.binding.trim());
    put("number_of_pages", row.number_of_pages.trim());
    put("year_published", row.year_published.trim());
    put("original_publication_year", row.original_publication_year.trim());
    put("date_added", row.date_added.trim());
    put("bookshelves", row.bookshelves.trim());
    put("bookshelves_with_positions", row.bookshelves_with_positions.trim());
    put("exclusive_shelf", row.exclusive_shelf.trim());
    put("my_review", row.my_review.trim());
    put("spoiler", row.spoiler.trim());
    put("private_notes", row.private_notes.trim());
    put("read_count", row.read_count.trim());
    put("recommended_for", row.recommended_for.trim());
    put("recommended_by", row.recommended_by.trim());
    put("owned_copies", row.owned_copies.trim());
    put("original_purchase_date", row.original_purchase_date.trim());
    put("original_purchase_location", row.original_purchase_location.trim());
    put("condition", row.condition.trim());
    put("condition_description", row.condition_description.trim());
    put("bcid", row.bcid.trim());

    Some(MediaItem {
        ts,
        source: "goodreads".into(),
        category: "other".into(),
        device: String::new(),
        kind: "play".into(),
        title: row.title.trim().to_string(),
        // Author is the grouping key (chart books by author).
        subtitle: row.author.trim().to_string(),
        // Goodreads has no canonical URL per-book in the export.
        detail: String::new(),
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

/// Write raw books to `media/goodreads/books.jsonl`.
///
/// The raw layer is a flat JSONL file without date partitioning (books don't
/// have a single authoritative timestamp). We write it as a simple
/// newline-delimited file, appending new rows. Vault::resolve protects the path.
/// Each row is a complete header→value map capturing ALL columns from the
/// export — no columns are dropped regardless of what struct fields exist.
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
            .join(format!("trove-goodreads-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // A realistic 31-column Goodreads CSV export snippet covering key edge cases:
    // - Normal finished read (has Date Read)
    // - Book with no Date Read (curation-only)
    // - ISBN with Excel-guard prefix
    // - Multi-line review (quoted in CSV)
    // - Read Count > 1 with one Date Read
    //
    // Real Goodreads "Export Library" header (31 columns):
    //   Book Id, Title, Author, Author l-f, Additional Authors, ISBN, ISBN13,
    //   My Rating, Average Rating, Publisher, Binding, Number of Pages,
    //   Year Published, Original Publication Year, Date Read, Date Added,
    //   Bookshelves, Bookshelves with positions, Exclusive Shelf, My Review,
    //   Spoiler, Private Notes, Read Count, Recommended For, Recommended By,
    //   Owned Copies, Original Purchase Date, Original Purchase Location,
    //   Condition, Condition Description, BCID
    //
    // Row 1: finished read with a multi-line review, read_count=2.
    // Row 2: to-read shelf, no Date Read (curation-only).
    // Row 3: finished read, read_count=3 (one contract row per unique Date Read).
    const SAMPLE_CSV: &str = "Book Id,Title,Author,Author l-f,Additional Authors,ISBN,ISBN13,My Rating,Average Rating,Publisher,Binding,Number of Pages,Year Published,Original Publication Year,Date Read,Date Added,Bookshelves,Bookshelves with positions,Exclusive Shelf,My Review,Spoiler,Private Notes,Read Count,Recommended For,Recommended By,Owned Copies,Original Purchase Date,Original Purchase Location,Condition,Condition Description,BCID\n\
1234567,The Pragmatic Programmer,David Thomas,\"Thomas, David\",Andrew Hunt,=\"020161622X\",=\"9780201616224\",5,4.36,Addison-Wesley,Paperback,352,2019,1999,2024/03/15,2023/11/01,favorites,favorites (#1),read,\"Great book.\nHighly recommended.\",false,,2,,,,,,,,\n\
7654321,Clean Code,Robert C. Martin,\"Martin, Robert C.\",,=\"0132350882\",=\"9780132350884\",4,3.72,Prentice Hall,Paperback,431,2008,2008,,2024/01/10,,to-read (#5),to-read,,,,,,,,,,,\n\
9999999,The Mythical Man-Month,Frederick P. Brooks Jr.,\"Brooks, Frederick P.\",\"Jones, T.\",=\"0201835959\",=\"9780201835953\",5,4.10,Addison-Wesley,Paperback,322,1995,1975,2023/07/20,2022/12/05,,read (#2),read,,,,3,,,,,,,,\n\
";

    fn run(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("goodreads_library_export.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_finished_reads_and_skips_unread() {
        let v = temp_vault("basic");
        let out = run(&v, SAMPLE_CSV);
        assert_eq!(
            out.headline,
            "2 books imported, 0 duplicates skipped",
            "two books have a Date Read; one is curation-only"
        );

        // Contract layer: month-partitioned JSONL in media/plays/goodreads/
        let path_2024 = v.root().join("media/plays/goodreads/2024-03.jsonl");
        let raw = fs::read_to_string(&path_2024).unwrap();
        assert_eq!(raw.lines().count(), 1, "one book read in 2024-03");
        let item: MediaItem = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(item.source, "goodreads");
        assert_eq!(item.category, "other");
        assert_eq!(item.kind, "play");
        assert_eq!(item.title, "The Pragmatic Programmer");
        assert_eq!(item.subtitle, "David Thomas");
        assert_eq!(item.seconds, 0);
        // guid uses canonical ISO date (YYYY-MM-DD), not YYYY/MM/DD
        assert_eq!(item.guid, "goodreads-1234567-2024-03-15");
        assert_eq!(item.extra.get("medium"), Some(&json!("book")));
        assert_eq!(item.extra.get("my_rating"), Some(&json!("5")));
        // Excel-guard stripped from ISBN
        assert_eq!(item.extra.get("isbn"), Some(&json!("020161622X")));
        // New fields present in contract extra
        assert_eq!(item.extra.get("author_lf"), Some(&json!("Thomas, David")));
        assert_eq!(item.extra.get("additional_authors"), Some(&json!("Andrew Hunt")));
        assert_eq!(item.extra.get("number_of_pages"), Some(&json!("352")));
        assert_eq!(item.extra.get("bookshelves_with_positions"), Some(&json!("favorites (#1)")));

        // Multi-line review preserved in extra
        let review = item.extra.get("my_review").and_then(|v| v.as_str()).unwrap_or("");
        assert!(review.contains("Great book"), "review preserved: {review}");

        // 2023-07 book (read count 3, one date)
        let path_2023 = v.root().join("media/plays/goodreads/2023-07.jsonl");
        let raw_2023 = fs::read_to_string(&path_2023).unwrap();
        assert_eq!(raw_2023.lines().count(), 1, "one contract row for read_count=3");
        let item_2023: MediaItem =
            serde_json::from_str(raw_2023.lines().next().unwrap()).unwrap();
        assert_eq!(item_2023.extra.get("read_count"), Some(&json!("3")));
    }

    #[test]
    fn raw_layer_written_unconditionally() {
        let v = temp_vault("raw");
        run(&v, SAMPLE_CSV);
        let raw_path = v.root().join(RAW_FILE);
        assert!(raw_path.exists(), "raw books.jsonl written");
        let raw_content = fs::read_to_string(&raw_path).unwrap();
        assert_eq!(raw_content.lines().count(), 3, "all three books in raw");
        // First raw book has stripped ISBN and all 31 columns
        let raw_obj: serde_json::Value =
            serde_json::from_str(raw_content.lines().next().unwrap()).unwrap();
        assert_eq!(raw_obj["ISBN"].as_str(), Some("020161622X"), "excel-guard stripped in raw");
        assert_eq!(raw_obj["Book Id"].as_str(), Some("1234567"));
        // The 4 previously-missing columns are now present in raw
        assert!(raw_obj.get("Author l-f").is_some(), "Author l-f present in raw");
        assert!(raw_obj.get("Additional Authors").is_some(), "Additional Authors present in raw");
        assert!(raw_obj.get("Number of Pages").is_some(), "Number of Pages present in raw");
        assert!(raw_obj.get("Bookshelves with positions").is_some(), "Bookshelves with positions present in raw");
        // No-Date-Read book appears in raw
        assert!(
            raw_content.contains("7654321"),
            "curation-only book in raw: {raw_content}"
        );
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idempotent");
        let out1 = run(&v, SAMPLE_CSV);
        assert_eq!(out1.counts.get("imported"), Some(&2));

        let out2 = run(&v, SAMPLE_CSV);
        assert_eq!(
            out2.headline, "0 books imported, 2 duplicates skipped",
            "second import: all are duplicates"
        );

        // Contract files are unchanged after the second import.
        let path_2024 = v.root().join("media/plays/goodreads/2024-03.jsonl");
        let lines_after = fs::read_to_string(&path_2024).unwrap().lines().count();
        assert_eq!(lines_after, 1, "no new contract rows after re-import");
    }

    #[test]
    fn guid_uses_canonical_date_independent_of_punctuation() {
        // A row with date_read "2024/03/15" and the same row with "2024-03-15"
        // must produce identical guids (dedup key must not depend on punctuation).
        let v1 = temp_vault("guid-slash");
        run(&v1, SAMPLE_CSV);
        let path = v1.root().join("media/plays/goodreads/2024-03.jsonl");
        let item: MediaItem =
            serde_json::from_str(fs::read_to_string(&path).unwrap().lines().next().unwrap())
                .unwrap();
        let guid_slash = item.guid.clone();

        // Build a CSV with the same row but dashes in the date.
        let csv_dash = SAMPLE_CSV.replace("2024/03/15", "2024-03-15");
        let v2 = temp_vault("guid-dash");
        run(&v2, &csv_dash);
        let path2 = v2.root().join("media/plays/goodreads/2024-03.jsonl");
        let item2: MediaItem =
            serde_json::from_str(fs::read_to_string(&path2).unwrap().lines().next().unwrap())
                .unwrap();

        assert_eq!(guid_slash, item2.guid, "guid must be identical regardless of date punctuation");
        assert_eq!(guid_slash, "goodreads-1234567-2024-03-15");
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        run(&v, SAMPLE_CSV);
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "goodreads").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2024-03"));
    }

    #[test]
    fn media_timeline_sees_goodreads_books() {
        let v = temp_vault("timeline");
        run(&v, SAMPLE_CSV);
        let day = v.media_timeline("2024-03-15").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "goodreads");
        assert_eq!(day[0].title, "The Pragmatic Programmer");
    }

    #[test]
    fn unknown_future_columns_are_preserved_in_raw() {
        // Simulate a hypothetical future Goodreads export with an extra column
        // not known at compile time. The raw layer must capture it verbatim.
        let v = temp_vault("future-col");
        // Insert a fictitious "Kindle Highlights Count" column.
        let csv_extra = "Book Id,Title,Author,Author l-f,Additional Authors,ISBN,ISBN13,My Rating,Average Rating,Publisher,Binding,Number of Pages,Year Published,Original Publication Year,Date Read,Date Added,Bookshelves,Bookshelves with positions,Exclusive Shelf,My Review,Spoiler,Private Notes,Read Count,Recommended For,Recommended By,Owned Copies,Original Purchase Date,Original Purchase Location,Condition,Condition Description,BCID,Kindle Highlights Count\n\
1234567,The Pragmatic Programmer,David Thomas,\"Thomas, David\",Andrew Hunt,=\"020161622X\",=\"9780201616224\",5,4.36,Addison-Wesley,Paperback,352,2019,1999,2024/03/15,2023/11/01,favorites,favorites (#1),read,,false,,2,,,,,,,,,42\n";
        run(&v, csv_extra);
        let raw_path = v.root().join(RAW_FILE);
        let raw_content = fs::read_to_string(&raw_path).unwrap();
        let raw_obj: serde_json::Value =
            serde_json::from_str(raw_content.lines().next().unwrap()).unwrap();
        assert_eq!(
            raw_obj["Kindle Highlights Count"].as_str(),
            Some("42"),
            "unknown future column preserved in raw layer"
        );
    }
}
