//! IMDb ratings, watchlist, and custom lists — official CSV export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/imdb.md.
//!
//! IMDb offers three export types, all from the same "Export" menu at
//! imdb.com/exports. A user may drop any subset.
//!
//! **Ratings CSV** — `Const, Your Rating, Date Rated, Title, URL, Title Type,
//! IMDb Rating, Runtime (mins), Year, Genres, Num Votes, Release Date, Directors`.
//! Partitioned by the `Date Rated` month and stored at `media/imdb/ratings/YYYY-MM.jsonl`,
//! guid-merged on `Const` so a re-drop never duplicates.
//!
//! **Watchlist / custom list CSV** — modern export header includes the same
//! columns AS ratings (including "Your Rating" and "Date Rated") PLUS leading
//! list-specific columns: `Position, Const, Created, Modified, Description`.
//! The distinguishing marker is the presence of a `Position` or `Created` header
//! (these columns are absent from pure ratings exports). Stored as a full snapshot
//! rewrite at `media/imdb/watchlist.jsonl` (watchlist) or
//! `media/imdb/lists/<stem>.jsonl` (custom lists). Within a list export,
//! `Your Rating` and `Date Rated` are optional — unrated entries have empty
//! values and must not be skipped.
//!
//! The drop filename distinguishes watchlist from custom lists via a "watchlist"
//! substring match (IMDb names the file `WATCHLIST.csv`); custom list files
//! are `ls<id>.csv`.
//!
//! The `Const` IMDb id (e.g. `tt1234567`) is the natural guid and cross-references
//! Trakt and Letterboxd rows at read time.
//!
//! **Column evidence:** real modern watchlist export header (Position, Const,
//! Created, Modified, Description, Title, URL, Title Type, IMDb Rating,
//! Runtime (mins), Year, Genres, Num Votes, Release Date, Directors, Your Rating,
//! Date Rated) corroborated by multiple community sources (romiojoseph/
//! imdb-watchlist-export-visualizer, TMDB community thread). Header-name matching
//! (never positional) tolerates future column additions.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSignature, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Vault paths.

/// Month-partitioned ratings stream.
const RATINGS_DIR: &str = "media/imdb/ratings";
/// Watchlist snapshot (full rewrite each import; no event date).
const WATCHLIST_REL: &str = "media/imdb/watchlist.jsonl";
/// Parent directory for custom list snapshots.
const LISTS_DIR: &str = "media/imdb/lists";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Prefer the newest ratings partition, else fall back to the watchlist
    // mtime, else a lists-dir mtime.
    crate::registry::newest_stem(&vault.root().join(RATINGS_DIR))
        .or_else(|| crate::registry::file_mtime(&vault.root().join(WATCHLIST_REL)))
        .or_else(|| crate::registry::newest_mtime(&vault.root().join(LISTS_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "imdb",
        name: "IMDb",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your IMDb ratings, watchlist, and custom lists from the \
                      official CSV exports at imdb.com/exports. IMDb Const IDs cross-reference \
                      Trakt and Letterboxd entries. Re-importable: re-dropping a ratings export \
                      never duplicates.",
        domain: "media",
        vault_path: "media/imdb/",
        toggleable: false,
        setup: &[
            "Go to imdb.com/exports (or open Your Ratings → three-dot menu → Export).",
            "Download the ratings CSV, watchlist CSV, or any custom list CSV.",
            "Drop each file here — you can import them one at a time.",
        ],
        caveats: "IMDb has no user API; these are curation records (ratings and watchlist), \
                  not watch-play history. Watchlist and custom list imports rewrite the whole \
                  snapshot (no event date available).",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// The two IMDb export header shapes the drop surface recognizes. `Const`
/// (the `tt…` id column) anchors both; the ratings vs. list distinction is the
/// list-only `Position`/`Created` markers (mirrors [`detect_kind`]). Watchlist
/// and custom-list exports share the same header — the built importer tells
/// them apart by filename, so one `list` signature covers both.
static SIGNATURES: &[ImportSignature] = &[
    ImportSignature {
        label: "IMDb ratings",
        required: &["Const", "Your Rating", "Date Rated", "Title", "URL", "Title Type"],
        absent: &["Position", "Created"],
    },
    ImportSignature {
        label: "IMDb watchlist / custom list",
        required: &["Position", "Const", "Created", "Modified", "Title", "URL", "Title Type"],
        absent: &[],
    },
];

static IMPORT: ImportSpec = ImportSpec {
    signatures: SIGNATURES,
    accepts: &["csv"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// CSV import.

/// A raw row carrying its partition timestamp (or a synthetic one) purely so
/// the `JsonlStream::append` writer can file it into the right partition. Only
/// `value` is serialized to disk (flatten = the full-fidelity object).
#[derive(Serialize)]
struct RawRow {
    /// Month-partition key (`YYYY-MM-DD` → first 7 chars used). For watchlist/
    /// list snapshots this is unused (written via `write_snapshot`).
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// IMDb export kind — detected from the CSV header row.
#[derive(Debug, PartialEq)]
enum ExportKind {
    /// Pure ratings export: has "Your Rating" but lacks "Position" and "Created".
    Ratings,
    /// Watchlist or custom list: has list-specific columns "Position" or "Created"
    /// (present even when the list also carries "Your Rating"/"Date Rated" columns).
    List,
}

/// Classify the export by the presence of list-specific headers.
///
/// Real modern watchlist/list exports contain BOTH list-specific columns
/// (`Position`, `Created`, `Modified`, `Description`) AND rating columns
/// (`Your Rating`, `Date Rated`). A pure ratings CSV has none of the list
/// columns. Keying on `Position` or `Created` (never present in ratings)
/// is the correct discriminator.
fn detect_kind(headers: &csv::StringRecord) -> ExportKind {
    let has_list_marker = headers.iter().any(|h| h == "Position" || h == "Created");
    if has_list_marker {
        ExportKind::List
    } else {
        ExportKind::Ratings
    }
}

/// Classify the drop target path for a watchlist vs a custom list.
/// IMDb names the watchlist file `WATCHLIST.csv`; custom list files are
/// `ls<id>.csv` or anything else. Match case-insensitively.
fn is_watchlist(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().contains("watchlist"))
}

/// Sanitize a filename stem into a safe vault-relative name: ASCII letters,
/// digits, hyphens, underscores only; everything else collapsed to `_`.
/// Prevents path traversal (e.g. "../evil" → "..evil" → safe).
fn safe_stem(path: &Path) -> String {
    let raw = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("list");
    let out: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if out.is_empty() { "list".to_string() } else { out }
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    let headers = rdr
        .headers()
        .context("reading CSV header row — is this an IMDb export CSV?")?
        .clone();

    let kind = detect_kind(&headers);

    match kind {
        ExportKind::Ratings => import_ratings(vault, &headers, rdr, progress),
        ExportKind::List => import_list(vault, path, &headers, rdr, progress),
    }
}

/// Import a ratings CSV — partitioned by `Date Rated`, guid-merged on `Const`.
fn import_ratings(
    vault: &Vault,
    headers: &csv::StringRecord,
    mut rdr: csv::Reader<&[u8]>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let stream = vault.stream(RATINGS_DIR, Partition::Month);

    // Load existing Const ids for re-import deduplication.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for row in stream.read::<Value>(&key)? {
            if let Some(c) = row.get("Const").and_then(Value::as_str) {
                if !c.is_empty() {
                    seen.insert(c.to_string());
                }
            }
        }
    }

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut new_rows: Vec<RawRow> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        // Build a header→value map (full fidelity, verbatim strings).
        let fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();

        let const_id = field(&fields, "Const");
        if const_id.is_empty() {
            skipped += 1;
            continue;
        }

        // Need a parseable Date Rated for the month partition.
        let date_rated = field(&fields, "Date Rated");
        let ts = match parse_ts(date_rated) {
            Some(t) => t,
            None => {
                skipped += 1;
                continue;
            }
        };

        if !seen.insert(const_id.to_string()) {
            duplicates += 1;
            continue;
        }

        new_rows.push(RawRow { ts, value: Value::Object(fields) });
        imported += 1;
        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    stream.append(&new_rows, |r| &r.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} ratings imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Import a watchlist or custom list CSV — full snapshot rewrite (no date).
fn import_list(
    vault: &Vault,
    path: &Path,
    headers: &csv::StringRecord,
    mut rdr: csv::Reader<&[u8]>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Resolve target snapshot path.
    let rel = if is_watchlist(path) {
        WATCHLIST_REL.to_string()
    } else {
        format!("{}/{}.jsonl", LISTS_DIR, safe_stem(path))
    };

    let (mut imported, mut skipped, mut rows) = (0u64, 0u64, 0u64);
    let mut snapshot: Vec<Value> = Vec::new();

    for rec in rdr.records() {
        rows += 1;
        let Ok(rec) = rec else {
            skipped += 1;
            continue;
        };
        let fields: Map<String, Value> = headers
            .iter()
            .zip(rec.iter())
            .map(|(h, v)| (h.to_string(), Value::String(v.to_string())))
            .collect();

        // Must have a Const id to be useful.
        // Note: `Your Rating` and `Date Rated` are OPTIONAL in list/watchlist
        // exports — unrated entries carry empty strings there. Never skip on
        // a missing/empty date here; that skip belongs only in import_ratings.
        let const_id = field(&fields, "Const");
        if const_id.is_empty() {
            skipped += 1;
            continue;
        }

        snapshot.push(Value::Object(fields));
        imported += 1;
        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Snapshot rewrite — current state wins (whole-file atomic replace).
    vault.write_snapshot(&rel, &snapshot)?;
    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} entries imported into {rel}"),
        counts: [("imported", imported), ("skipped", skipped)].into(),
    })
}

// ---------------------------------------------------------------------------
// Helpers.

/// A trimmed string field by header name; `""` when absent or whitespace.
fn field<'a>(fields: &'a Map<String, Value>, key: &str) -> &'a str {
    fields.get(key).and_then(Value::as_str).map(str::trim).unwrap_or("")
}

/// Parse `Date Rated` (`YYYY-MM-DD`) into an RFC3339 local noon timestamp,
/// used as both the partition key and the `ts` stored alongside the row.
/// Returns `None` when unparseable.
fn parse_ts(date_str: &str) -> Option<String> {
    let d = NaiveDate::parse_from_str(date_str.trim(), "%Y-%m-%d").ok()?;
    // No time is given; use noon local to avoid midnight-boundary surprises.
    let ts = Local.from_local_datetime(&d.and_hms_opt(12, 0, 0)?)
        .earliest()?
        .to_rfc3339();
    Some(ts)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-imdb-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn do_import(v: &Vault, filename: &str, csv_body: &str) -> ImportOutcome {
        let path = v.root().join(filename);
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // Ratings CSV: confirmed column set from integrations-research.md L3252.
    const RATINGS_CSV: &str = "\
Const,Your Rating,Date Rated,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors\r\n\
tt0110912,10,2024-03-15,Pulp Fiction,https://www.imdb.com/title/tt0110912/,movie,8.9,154,1994,\"Crime, Drama\",2100000,1994-10-14,Quentin Tarantino\r\n\
tt0068646,10,2024-02-10,The Godfather,https://www.imdb.com/title/tt0068646/,movie,9.2,175,1972,\"Crime, Drama\",1900000,1972-03-24,Francis Ford Coppola\r\n\
tt9999999,8,bad-date,Missing Date Film,https://www.imdb.com/title/tt9999999/,movie,7.5,90,2020,Action,50000,2020-01-01,Director\r\n\
,5,2024-01-01,No Const Film,https://www.imdb.com/title//,movie,6.0,100,2019,Drama,10000,2019-01-01,Some Director\r\n\
";

    // Real modern watchlist/list export header (corroborated by
    // romiojoseph/imdb-watchlist-export-visualizer and TMDB community thread).
    // Key differences from ratings: leading Position/Created/Modified/Description
    // columns; Your Rating + Date Rated are present but may be empty (unrated).
    const WATCHLIST_CSV: &str = "\
Position,Const,Created,Modified,Description,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors,Your Rating,Date Rated\r\n\
1,tt0816692,2023-01-15,2023-01-15,,Interstellar,https://www.imdb.com/title/tt0816692/,movie,8.7,169,2014,\"Adventure, Drama, Sci-Fi\",2000000,2014-11-07,Christopher Nolan,,\r\n\
2,tt0468569,2023-03-20,2023-03-20,,The Dark Knight,https://www.imdb.com/title/tt0468569/,movie,9.0,152,2008,\"Action, Crime, Drama\",2800000,2008-07-18,Christopher Nolan,10,2024-01-05\r\n\
";

    const CUSTOM_LIST_CSV: &str = "\
Position,Const,Created,Modified,Description,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors,Your Rating,Date Rated\r\n\
1,tt0111161,2022-06-10,2022-06-10,,The Shawshank Redemption,https://www.imdb.com/title/tt0111161/,movie,9.3,142,1994,Drama,2700000,1994-09-23,Frank Darabont,,\r\n\
";

    #[test]
    fn imports_ratings_partitioned_by_month() {
        let v = temp_vault("ratings");
        let out = do_import(&v, "ratings.csv", RATINGS_CSV);
        // 2 valid rows; bad-date and no-Const skipped.
        assert_eq!(out.counts["imported"], 2);
        assert_eq!(out.counts["skipped"], 2);
        assert!(out.headline.contains("2 ratings imported"));

        // March partition.
        let march = v.root().join("media/imdb/ratings/2024-03.jsonl");
        let raw = fs::read_to_string(&march).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert!(raw.contains("\"tt0110912\""), "Const preserved: {raw}");
        assert!(raw.contains("Pulp Fiction"), "Title preserved");
        assert!(raw.contains("\"Quentin Tarantino\""), "Directors preserved");

        // February partition.
        let feb = v.root().join("media/imdb/ratings/2024-02.jsonl");
        assert!(feb.exists(), "February partition exists");
    }

    #[test]
    fn ratings_reimport_is_idempotent() {
        let v = temp_vault("rerun");
        let out1 = do_import(&v, "ratings.csv", RATINGS_CSV);
        assert_eq!(out1.counts["imported"], 2);

        let out2 = do_import(&v, "ratings.csv", RATINGS_CSV);
        assert_eq!(out2.counts["imported"], 0, "re-import writes zero new rows");
        assert_eq!(out2.counts["duplicates"], 2, "both rows detected as duplicates");

        // File unchanged.
        let march = v.root().join("media/imdb/ratings/2024-03.jsonl");
        assert_eq!(fs::read_to_string(&march).unwrap().lines().count(), 1);
    }

    #[test]
    fn imports_watchlist_as_snapshot() {
        let v = temp_vault("watchlist");
        let out = do_import(&v, "WATCHLIST.csv", WATCHLIST_CSV);
        assert_eq!(out.counts["imported"], 2);
        assert!(out.headline.contains("watchlist.jsonl"));

        let snap = v.root().join("media/imdb/watchlist.jsonl");
        let raw = fs::read_to_string(&snap).unwrap();
        assert_eq!(raw.lines().count(), 2);
        assert!(raw.contains("tt0816692"), "Interstellar Const present");
        assert!(raw.contains("tt0468569"), "Dark Knight Const present");
    }

    #[test]
    fn watchlist_reimport_rewrites_snapshot() {
        let v = temp_vault("watchlist-rewrite");
        do_import(&v, "WATCHLIST.csv", WATCHLIST_CSV);

        // Drop a new watchlist with only one item (real modern header).
        let updated = "\
Position,Const,Created,Modified,Description,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors,Your Rating,Date Rated\r\n\
1,tt0816692,2023-01-15,2023-01-15,,Interstellar,https://www.imdb.com/title/tt0816692/,movie,8.7,169,2014,\"Adventure, Drama, Sci-Fi\",2000000,2014-11-07,Christopher Nolan,,\r\n\
";
        let out = do_import(&v, "WATCHLIST.csv", updated);
        assert_eq!(out.counts["imported"], 1);

        let snap = v.root().join("media/imdb/watchlist.jsonl");
        let raw = fs::read_to_string(&snap).unwrap();
        assert_eq!(raw.lines().count(), 1, "snapshot rewrites to 1 row");
    }

    #[test]
    fn imports_custom_list_at_lists_subdir() {
        let v = temp_vault("custom-list");
        let out = do_import(&v, "ls123456789.csv", CUSTOM_LIST_CSV);
        assert_eq!(out.counts["imported"], 1);

        let snap = v.root().join("media/imdb/lists/ls123456789.jsonl");
        assert!(snap.exists(), "custom list file created");
        let raw = fs::read_to_string(&snap).unwrap();
        assert!(raw.contains("tt0111161"), "Shawshank Const present");
    }

    #[test]
    fn detect_kind_correctly_classifies() {
        // Pure ratings export: has Your Rating but NO Position/Created.
        let rating_hdr: csv::StringRecord = vec![
            "Const", "Your Rating", "Date Rated", "Title", "URL",
            "Title Type", "IMDb Rating", "Runtime (mins)", "Year",
            "Genres", "Num Votes", "Release Date", "Directors",
        ]
        .into_iter()
        .collect();

        // Real modern watchlist/list export: has Position + Created AND
        // also has Your Rating + Date Rated (both present — this is the key
        // point the original incorrect implementation got wrong).
        let watchlist_hdr: csv::StringRecord = vec![
            "Position", "Const", "Created", "Modified", "Description",
            "Title", "URL", "Title Type", "IMDb Rating", "Runtime (mins)",
            "Year", "Genres", "Num Votes", "Release Date", "Directors",
            "Your Rating", "Date Rated",
        ]
        .into_iter()
        .collect();

        // Minimal list header with only Position as the marker.
        let list_hdr_position_only: csv::StringRecord = vec![
            "Position", "Const", "Title", "URL",
        ]
        .into_iter()
        .collect();

        // Minimal list header with only Created as the marker.
        let list_hdr_created_only: csv::StringRecord = vec![
            "Const", "Created", "Title", "URL",
        ]
        .into_iter()
        .collect();

        assert_eq!(detect_kind(&rating_hdr), ExportKind::Ratings,
            "pure ratings CSV (no Position/Created) → Ratings");
        assert_eq!(detect_kind(&watchlist_hdr), ExportKind::List,
            "watchlist CSV with Your Rating still present → List (Position/Created wins)");
        assert_eq!(detect_kind(&list_hdr_position_only), ExportKind::List,
            "Position alone is sufficient to mark List");
        assert_eq!(detect_kind(&list_hdr_created_only), ExportKind::List,
            "Created alone is sufficient to mark List");
    }

    #[test]
    fn is_watchlist_matches_case_insensitively() {
        assert!(is_watchlist(Path::new("WATCHLIST.csv")));
        assert!(is_watchlist(Path::new("watchlist.csv")));
        assert!(is_watchlist(Path::new("imdb_watchlist_2024.csv")));
        assert!(!is_watchlist(Path::new("ls123.csv")));
        assert!(!is_watchlist(Path::new("ratings.csv")));
    }

    #[test]
    fn safe_stem_sanitizes_dangerous_chars() {
        // Path::file_stem strips the extension and returns the filename part only,
        // so "../evil.csv" → file_stem "evil" → already safe.
        assert_eq!(safe_stem(Path::new("../evil.csv")), "evil");
        assert_eq!(safe_stem(Path::new("my list!.csv")), "my_list_");
        assert_eq!(safe_stem(Path::new("ls123456.csv")), "ls123456");
        assert_eq!(safe_stem(Path::new("my-list.csv")), "my-list");
    }

    #[test]
    fn last_data_reflects_ratings_partition() {
        let v = temp_vault("last-data");
        assert!(def_last_data(&v).is_none(), "none before any import");
        do_import(&v, "ratings.csv", RATINGS_CSV);
        let ld = def_last_data(&v).expect("last_data set after import");
        assert!(ld.starts_with("2024-"), "last_data is a YYYY-MM stem: {ld}");
    }

    /// Regression test: a real modern watchlist export has Your Rating and
    /// Date Rated columns (both present). The old detect_kind() would have
    /// misclassified this as Ratings and silently skipped all unrated rows.
    /// This test verifies: (a) routing goes to import_list not import_ratings,
    /// (b) unrated entries (empty Your Rating / Date Rated) are NOT skipped,
    /// (c) the snapshot lands at watchlist.jsonl, not ratings/YYYY-MM.jsonl.
    #[test]
    fn real_watchlist_header_with_unrated_entries_not_skipped() {
        let v = temp_vault("watchlist-real-header");
        // Real modern header — includes Position, Created (list markers) AND
        // Your Rating, Date Rated (which the old code misread as "Ratings").
        // Row 1: unrated (empty Your Rating + Date Rated).
        // Row 2: rated.
        let csv = "\
Position,Const,Created,Modified,Description,Title,URL,Title Type,IMDb Rating,Runtime (mins),Year,Genres,Num Votes,Release Date,Directors,Your Rating,Date Rated\r\n\
1,tt0816692,2023-01-15,2023-01-15,,Interstellar,https://www.imdb.com/title/tt0816692/,movie,8.7,169,2014,\"Adventure, Drama, Sci-Fi\",2000000,2014-11-07,Christopher Nolan,,\r\n\
2,tt0468569,2023-03-20,2023-03-20,,The Dark Knight,https://www.imdb.com/title/tt0468569/,movie,9.0,152,2008,\"Action, Crime, Drama\",2800000,2008-07-18,Christopher Nolan,10,2024-01-05\r\n\
";
        let out = do_import(&v, "WATCHLIST.csv", csv);

        // Both rows imported — unrated entry must not be skipped.
        assert_eq!(out.counts["imported"], 2,
            "unrated entry must not be skipped (got headline: {})", out.headline);
        assert_eq!(out.counts.get("skipped").copied().unwrap_or(0), 0);

        // Must land at watchlist.jsonl, NOT ratings/YYYY-MM.jsonl.
        let snap = v.root().join("media/imdb/watchlist.jsonl");
        assert!(snap.exists(), "snapshot file must be written to watchlist.jsonl");
        let raw = fs::read_to_string(&snap).unwrap();
        assert_eq!(raw.lines().count(), 2, "both rows in snapshot");
        assert!(raw.contains("tt0816692"), "unrated Interstellar present");
        assert!(raw.contains("tt0468569"), "rated Dark Knight present");

        // Ratings directory must be empty / not exist.
        let ratings_dir = v.root().join("media/imdb/ratings");
        if ratings_dir.exists() {
            let entries: Vec<_> = fs::read_dir(&ratings_dir).unwrap().collect();
            assert!(entries.is_empty(),
                "ratings dir must be empty — watchlist must not pollute ratings");
        }
    }
}
