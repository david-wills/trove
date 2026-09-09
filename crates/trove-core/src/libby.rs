//! Libby / OverDrive — library ebook and audiobook loan history via CSV export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/libby.md.
//!
//! **Access path:** any library's OverDrive website → History → "Email history"
//! → CSV delivered to the patron's email.  The user saves it and drops it into
//! the Trove import box.  No API, no login, no network calls from Trove.
//!
//! **Confirmed CSV columns** (from primary source — iamdav.in blog post showing
//! the raw CSV header string):
//!   Title, Sub Title, Author, Series, Publisher, Publish Date, Star Rating,
//!   Star Rating Count, Maturity Level, ISBN, Cover Art URL, Borrow Date, Type
//! Note: there is no Return Date in the OverDrive email-history CSV export.
//!
//! **Two layers, always:**
//! - **Raw** — `media/libby/borrows.jsonl`: every CSV column verbatim as a
//!   JSON key/value map, schema-agnostic so future OverDrive column additions
//!   are never silently dropped.
//! - **Contract** — `media/plays/libby/YYYY-MM.jsonl` per the media-plays
//!   write contract: one row per borrow, `ts` = borrow date, `source` =
//!   "libby", `kind` = "play", `category` derived from the Type column.
//!
//! **GUID:** no stable id exists in the export; derived as a SHA-256 prefix
//! over `(title, author, borrow_date)`, normalised before hashing for
//! idempotent re-imports.
//!
//! **Date format:** OverDrive's email-history CSV has no public date-format
//! spec; the parser tries `MM/DD/YYYY` (most-likely US locale format) then
//! `YYYY-MM-DD` as a fallback, then gives up and skips the row cleanly.
//!
//! **Needs-sample flag:** the column names are confirmed from a primary source
//! but no real export has been run against this parser in production; a full
//! validation pass requires a real CSV export. Mark `Needs-sample` accordingly.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer directory (media-plays write contract).
const CONTRACT_DIR: &str = "media/plays/libby";
/// Raw layer — full-fidelity borrow rows.
const RAW_DIR: &str = "media/libby";
const RAW_FILE: &str = "media/libby/borrows.jsonl";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CONTRACT_DIR))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(RAW_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "libby",
        name: "Libby / OverDrive",
        kind: IntegrationKind::Import,
        default_on: false,
        description:
            "Imports your library loan history from Libby / OverDrive as a CSV \
             emailed from your library's OverDrive site (History → Email history). \
             Tracks ebooks and audiobooks borrowed through public library cards. \
             Re-runnable: re-importing the same or an overlapping export never duplicates.",
        domain: "media",
        vault_path: "media/plays/libby/",
        toggleable: false,
        setup: &[
            "Open your library's OverDrive website and sign in.",
            "Go to Loans → History → Email history, enter your email, then Submit.",
            "Save the CSV from the email and import it here.",
        ],
        caveats: "History is patron opt-in and must be enabled per library — patrons whose \
                  library disables reading history will receive an empty export. \
                  Re-export periodically to pick up new borrows (no automatic sync).",
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

/// One row of the OverDrive email-history CSV export.
/// Confirmed column headers (13 columns, from primary source):
///   Title, Sub Title, Author, Series, Publisher, Publish Date, Star Rating,
///   Star Rating Count, Maturity Level, ISBN, Cover Art URL, Borrow Date, Type
#[derive(Debug, Deserialize)]
struct BorrowRow {
    #[serde(rename = "Title", default)]
    title: String,
    #[serde(rename = "Sub Title", default)]
    sub_title: String,
    #[serde(rename = "Author", default)]
    author: String,
    #[serde(rename = "Series", default)]
    series: String,
    #[serde(rename = "Publisher", default)]
    publisher: String,
    #[serde(rename = "Publish Date", default)]
    publish_date: String,
    #[serde(rename = "Star Rating", default)]
    star_rating: String,
    #[serde(rename = "Star Rating Count", default)]
    star_rating_count: String,
    #[serde(rename = "Maturity Level", default)]
    maturity_level: String,
    #[serde(rename = "ISBN", default)]
    isbn: String,
    #[serde(rename = "Cover Art URL", default)]
    cover_art_url: String,
    #[serde(rename = "Borrow Date", default)]
    borrow_date: String,
    #[serde(rename = "Type", default)]
    format_type: String,
}

/// Parse a borrow date.  OverDrive's date format is undocumented; try the most
/// common US locale format first, then ISO-8601.
fn parse_borrow_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Most likely: MM/DD/YYYY (US locale)
    NaiveDate::parse_from_str(s, "%m/%d/%Y")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(s, "%d/%m/%Y"))
        .ok()
}

/// Derive a stable guid from (title, author, borrow_date) via SHA-256.
/// Normalised before hashing — trimmed, lowercased — so whitespace and casing
/// differences across re-exports don't break idempotency.
fn derive_guid(title: &str, author: &str, borrow_date: &str) -> String {
    let key = format!(
        "libby|{}|{}|{}",
        title.trim().to_lowercase(),
        author.trim().to_lowercase(),
        borrow_date.trim()
    );
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    let result = hasher.finalize();
    format!("libby-{}", hex::encode(&result[..8]))
}

/// Map the OverDrive `Type` field to a media-plays `category`.
/// Type values are undocumented; treat anything containing "audio" as
/// "audiobook", anything else as "book" stored in `extra.medium`.
/// Category "other" + extra.medium mirrors the goodreads/storygraph pattern.
fn type_to_category(format_type: &str) -> &'static str {
    let t = format_type.to_lowercase();
    if t.contains("audio") {
        "audiobook"
    } else {
        // ebooks, magazines, etc. all map to "other"; medium is in extra
        "other"
    }
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

    // Load already-stored raw guids so we only append truly new raw rows.
    let mut seen_raw: HashSet<String> = HashSet::new();
    if let Ok(raw_path) = vault.resolve(RAW_FILE) {
        if raw_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&raw_path) {
                for line in content.lines() {
                    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(line) {
                        if let Some(g) = obj.get("_guid").and_then(|v| v.as_str()) {
                            if !g.is_empty() {
                                seen_raw.insert(g.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;

    // flexible(true) so exports with extra or missing columns don't hard-fail.
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

        // Pad short records so flexible exports still deserialize.
        let deserialized = if raw_record.len() < headers.len() {
            let mut padded = raw_record.clone();
            for _ in raw_record.len()..headers.len() {
                padded.push_field("");
            }
            padded.deserialize(Some(&headers_record))
        } else {
            raw_record.deserialize(Some(&headers_record))
        };

        let row: BorrowRow = match deserialized {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        if row.title.trim().is_empty() {
            skipped += 1;
            continue;
        }

        let guid = derive_guid(&row.title, &row.author, &row.borrow_date);

        // Raw layer: schema-agnostic — capture ALL columns verbatim.
        if seen_raw.insert(guid.clone()) {
            let mut raw_map = record_to_raw_map(&headers, &raw_record);
            raw_map.insert("_guid".to_string(), json!(guid.clone()));
            raw_items.push(raw_map);
        }

        // Contract layer: only borrows with a parseable Borrow Date.
        let borrow_date_raw = row.borrow_date.trim();
        if borrow_date_raw.is_empty() {
            // No borrow date — raw-only.
            continue;
        }
        let Some(item) = borrow_to_media_item(&guid, borrow_date_raw, &row) else {
            skipped += 1;
            continue;
        };
        if !seen_contract.insert(item.guid.clone()) {
            duplicates += 1;
            continue;
        }
        contract_items.push(item);
        imported += 1;

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Write raw layer.
    if !raw_items.is_empty() {
        write_raw_borrows(vault, &raw_items)?;
    }

    // Write contract layer.
    contract_stream.append(&contract_items, |i| &i.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} borrows imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Build a media-plays [`MediaItem`] for one borrow.
/// Returns `None` if `borrow_date` cannot be parsed.
fn borrow_to_media_item(guid: &str, borrow_date_raw: &str, row: &BorrowRow) -> Option<MediaItem> {
    let date = parse_borrow_date(borrow_date_raw)?;
    // Noon local — the export has date-only precision; noon avoids
    // midnight-boundary surprises (same reasoning as letterboxd).
    let ts = Local
        .from_local_datetime(&date.and_hms_opt(12, 0, 0)?)
        .earliest()?
        .to_rfc3339();

    let category = type_to_category(&row.format_type);

    let mut extra: Map<String, Value> = Map::new();
    let mut put = |k: &str, v: &str| {
        let v = v.trim();
        if !v.is_empty() {
            extra.insert(k.into(), json!(v));
        }
    };

    // Always store medium so the category can be queried without the Type field.
    // Audiobooks get category "audiobook"; everything else is category "other"
    // with medium in extra so a future additive enum extension can migrate cleanly.
    if category == "other" {
        put("medium", "book");
    }
    put("format_type", row.format_type.trim());
    put("sub_title", row.sub_title.trim());
    put("series", row.series.trim());
    put("publisher", row.publisher.trim());
    put("publish_date", row.publish_date.trim());
    put("star_rating", row.star_rating.trim());
    put("star_rating_count", row.star_rating_count.trim());
    put("maturity_level", row.maturity_level.trim());
    put("isbn", row.isbn.trim());
    put("cover_art_url", row.cover_art_url.trim());

    Some(MediaItem {
        ts,
        source: "libby".into(),
        category: category.to_string(),
        device: String::new(),
        kind: "play".into(),
        title: row.title.trim().to_string(),
        // Author is the grouping key — charts borrows by author/narrator.
        subtitle: row.author.trim().to_string(),
        detail: String::new(),
        seconds: 0,
        favicon: String::new(),
        guid: guid.to_string(),
        extra,
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

/// Write raw borrows to `media/libby/borrows.jsonl`.
fn write_raw_borrows(vault: &Vault, borrows: &[Map<String, Value>]) -> Result<()> {
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
    for borrow_map in borrows {
        let line = serde_json::to_string(borrow_map).context("serializing raw borrow")?;
        f.write_all(line.as_bytes()).context("writing raw borrow")?;
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
            .join(format!("trove-libby-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // Fixture CSV using the confirmed 13-column OverDrive email-history header.
    // Dates in MM/DD/YYYY (most-likely US locale format from iamdav.in context).
    // Type values approximate — exact strings are unverified (Needs-sample).
    const SAMPLE_CSV: &str = "\
Title,Sub Title,Author,Series,Publisher,Publish Date,Star Rating,Star Rating Count,Maturity Level,ISBN,Cover Art URL,Borrow Date,Type\n\
Project Hail Mary,,Andy Weir,,Ballantine Books,05/04/2021,4.8,285000,General,9780593135204,https://img.overdrive.com/cover/1234,06/15/2024,ebook-overdrive\n\
\"The Hitchhiker's Guide to the Galaxy\",The Restaurant at the End of the Universe,Douglas Adams,Hitchhiker's Guide #1,Pan Books,10/12/1979,4.5,180000,General,9780345391803,https://img.overdrive.com/cover/5678,03/22/2024,ebook-overdrive\n\
Educated,,Tara Westover,,Random House,02/20/2018,4.7,310000,General,9780399590504,https://img.overdrive.com/cover/9012,01/08/2024,audiobook-overdrive\n\
No Date Book,,Some Author,,Publisher,01/01/2020,,,General,1234567890,,ebook-overdrive\n\
";

    fn run(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("overdrive_history.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn imports_borrows_and_skips_dateless() {
        let v = temp_vault("basic");
        let out = run(&v, SAMPLE_CSV);
        // 3 rows have a Borrow Date; 1 has no date (raw-only, not counted as skipped).
        // The dateless row goes to the raw layer only — it does not count as imported
        // or skipped; only contract-layer failures (unparseable dates) count as skipped.
        assert_eq!(
            out.counts.get("imported"),
            Some(&3),
            "three rows with borrow dates imported: {out:?}"
        );

        // Contract layer: month-partitioned in media/plays/libby/.
        let path_june = v.root().join("media/plays/libby/2024-06.jsonl");
        assert!(path_june.exists(), "2024-06 partition created");
        let raw = fs::read_to_string(&path_june).unwrap();
        assert_eq!(raw.lines().count(), 1, "one borrow in June 2024");

        let item: MediaItem = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(item.source, "libby");
        assert_eq!(item.category, "other");   // ebook -> "other"
        assert_eq!(item.kind, "play");
        assert_eq!(item.title, "Project Hail Mary");
        assert_eq!(item.subtitle, "Andy Weir");
        assert_eq!(item.seconds, 0);
        assert!(!item.guid.is_empty(), "guid derived");
        assert!(item.guid.starts_with("libby-"), "guid prefixed");
        assert_eq!(item.extra.get("medium"), Some(&json!("book")));
        assert_eq!(item.extra.get("format_type"), Some(&json!("ebook-overdrive")));
        assert_eq!(item.extra.get("isbn"), Some(&json!("9780593135204")));
        assert_eq!(item.extra.get("star_rating"), Some(&json!("4.8")));

        // Audiobook gets category "audiobook".
        let path_jan = v.root().join("media/plays/libby/2024-01.jsonl");
        let jan_raw = fs::read_to_string(&path_jan).unwrap();
        let jan_item: MediaItem = serde_json::from_str(jan_raw.lines().next().unwrap()).unwrap();
        assert_eq!(jan_item.category, "audiobook");
        assert_eq!(jan_item.title, "Educated");
        // Audiobooks don't get medium:"book"
        assert!(jan_item.extra.get("medium").is_none());

        // March borrow.
        let path_mar = v.root().join("media/plays/libby/2024-03.jsonl");
        assert!(path_mar.exists(), "2024-03 partition created");
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("idempotent");
        let out1 = run(&v, SAMPLE_CSV);
        assert_eq!(out1.counts.get("imported"), Some(&3));

        let out2 = run(&v, SAMPLE_CSV);
        assert_eq!(
            out2.headline,
            "0 borrows imported, 3 duplicates skipped",
            "second import: all rows are duplicates"
        );

        // Contract files are unchanged after the second import.
        let june_lines = fs::read_to_string(v.root().join("media/plays/libby/2024-06.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(june_lines, 1, "no new contract rows after re-import");
    }

    #[test]
    fn raw_layer_written_unconditionally() {
        let v = temp_vault("raw");
        run(&v, SAMPLE_CSV);
        let raw_path = v.root().join(RAW_FILE);
        assert!(raw_path.exists(), "raw borrows.jsonl written");
        let raw_content = fs::read_to_string(&raw_path).unwrap();
        // All 4 rows (including dateless) must appear in raw.
        assert_eq!(raw_content.lines().count(), 4, "all four borrows in raw layer");

        // Verify schema-agnostic columns are present in raw.
        let raw_obj: serde_json::Value =
            serde_json::from_str(raw_content.lines().next().unwrap()).unwrap();
        assert_eq!(raw_obj["Title"].as_str(), Some("Project Hail Mary"));
        assert_eq!(raw_obj["Author"].as_str(), Some("Andy Weir"));
        assert_eq!(raw_obj["ISBN"].as_str(), Some("9780593135204"));
        assert!(raw_obj.get("_guid").is_some(), "_guid stored in raw for dedup");
    }

    #[test]
    fn guid_is_stable_across_whitespace() {
        // Two rows identical except for extra whitespace — must deduplicate.
        let csv1 = "Title,Sub Title,Author,Series,Publisher,Publish Date,Star Rating,Star Rating Count,Maturity Level,ISBN,Cover Art URL,Borrow Date,Type\nProject Hail Mary,,Andy Weir,,,,,,,,, 06/15/2024 ,ebook-overdrive\n";
        let csv2 = "Title,Sub Title,Author,Series,Publisher,Publish Date,Star Rating,Star Rating Count,Maturity Level,ISBN,Cover Art URL,Borrow Date,Type\nProject Hail Mary ,,  Andy Weir  ,,,,,,,,, 06/15/2024 ,ebook-overdrive\n";
        let g1 = derive_guid("Project Hail Mary", "Andy Weir", " 06/15/2024 ");
        let g2 = derive_guid("Project Hail Mary ", "  Andy Weir  ", " 06/15/2024 ");
        assert_eq!(g1, g2, "guid normalizes whitespace and casing");

        let _ = csv1; let _ = csv2; // fixtures defined for documentation clarity
    }

    #[test]
    fn iso_date_fallback_parsed() {
        // Some OverDrive locales may emit ISO dates; the parser must accept them.
        let csv = "Title,Sub Title,Author,Series,Publisher,Publish Date,Star Rating,Star Rating Count,Maturity Level,ISBN,Cover Art URL,Borrow Date,Type\nTest Book,,Test Author,,,,,,,,,2024-09-01,ebook-overdrive\n";
        let v = temp_vault("iso-date");
        let out = run(&v, csv);
        assert_eq!(out.counts.get("imported"), Some(&1), "ISO date row imported");
        let path = v.root().join("media/plays/libby/2024-09.jsonl");
        assert!(path.exists(), "2024-09 partition created for ISO date");
    }

    #[test]
    fn empty_csv_handled_gracefully() {
        let csv = "Title,Sub Title,Author,Series,Publisher,Publish Date,Star Rating,Star Rating Count,Maturity Level,ISBN,Cover Art URL,Borrow Date,Type\n";
        let v = temp_vault("empty");
        let out = run(&v, csv);
        assert_eq!(out.headline, "0 borrows imported, 0 duplicates skipped");
        // No contract file created for an empty import.
        assert!(!v.root().join(CONTRACT_DIR).exists() || {
            v.root().join(CONTRACT_DIR).read_dir().map_or(true, |mut d| d.next().is_none())
        });
    }

    #[test]
    fn media_timeline_sees_libby_borrows() {
        let v = temp_vault("timeline");
        run(&v, SAMPLE_CSV);
        let day = v.media_timeline("2024-06-15").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].source, "libby");
        assert_eq!(day[0].title, "Project Hail Mary");
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        run(&v, SAMPLE_CSV);
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "libby").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["csv"]);
        assert_eq!(card.last_data.as_deref(), Some("2024-06"));
    }

    #[test]
    fn unknown_future_columns_preserved_in_raw() {
        // Simulate a hypothetical extra column "Download Progress" added in a
        // future OverDrive export — the raw layer must capture it verbatim.
        let csv = "Title,Sub Title,Author,Series,Publisher,Publish Date,Star Rating,Star Rating Count,Maturity Level,ISBN,Cover Art URL,Borrow Date,Type,Download Progress\nTest,,Test Author,,,,,,,,,06/01/2024,ebook-overdrive,100%\n";
        let v = temp_vault("future-col");
        let out = run(&v, csv);
        assert_eq!(out.counts.get("imported"), Some(&1));
        let raw_content = fs::read_to_string(v.root().join(RAW_FILE)).unwrap();
        let raw_obj: serde_json::Value =
            serde_json::from_str(raw_content.lines().next().unwrap()).unwrap();
        assert_eq!(
            raw_obj["Download Progress"].as_str(),
            Some("100%"),
            "unknown future column preserved in raw layer"
        );
    }
}
