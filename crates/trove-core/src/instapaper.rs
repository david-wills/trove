//! Instapaper — read-later service; CSV export import into the reading contract.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/instapaper.md.
//!
//! Builds the **import (v1)** path: Instapaper → Settings → Export → Download CSV
//! yields a UTF-8 file with **five** columns: `URL`, `Title`, `Selection`,
//! `Folder`, `Timestamp`.  The `Timestamp` column carries the save time as a
//! Unix epoch (integer seconds) — confirmed against the Instapaper export script
//! (DEVONtechnologies) and the live export header `URL,Title,Selection,Folder,Timestamp`.
//! Up to 2,000 of the most-recently-saved articles.  Highlights are API-only
//! (not in the CSV) and are deferred to the xAuth follow-on.
//!
//! ## CSV shape (Settings → Export → Download CSV)
//!
//! ```text
//! URL,Title,Selection,Folder,Timestamp
//! https://example.com/article,"Article Title","Highlighted or selected text","Unread",1609459200
//! https://example.com/other,"Other Title","","Archive",1625097600
//! https://example.com/star,"Star Article","","Starred",1640995200
//! ```
//!
//! The first four columns are always present; `Selection` and `Title` may be
//! empty.  `Folder` is the Instapaper folder name: `"Unread"` (default inbox),
//! `"Archive"`, `"Starred"`, or a user-created folder name.  `Timestamp` is
//! present in normal exports; when absent or empty (e.g. very old API-stripped
//! exports) the row falls back to import-date-at-noon.
//!
//! ## Two vault layers
//!
//! - **Raw** → `reading/instapaper/raw/YYYY-MM.jsonl` — verbatim CSV rows
//!   (one JSON object per row, full fidelity, unconditional). Partitioned by
//!   the save-time month (or import month when timestamp is absent).
//! - **Contract** → `reading/instapaper/YYYY-MM.jsonl` — normalized
//!   [`crate::reading::Item`] rows, deduped by `guid` (= SHA-256 of
//!   `URL + "|" + timestamp_str`, matching the reading spec and the pocket.rs
//!   sibling pattern).  `ts` = RFC3339 local time of the save (or import-date
//!   at noon when no timestamp is available).  `state` maps from `Folder`.
//!
//! ## Guid / dedupe strategy
//!
//! We use `sha256(url + "|" + timestamp_str)[0..64]` (full hex) as the stable
//! guid — the same URL saved at two different times (a real re-add pattern)
//! gets distinct guids.  When no timestamp is available we use the import-noon
//! RFC3339 string as the second component (the same fallback used for `ts`).
//! Re-importing the same export is idempotent.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Item;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "reading/instapaper";
const RAW_DIR: &str = "reading/instapaper/raw";

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "instapaper",
        name: "Instapaper",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your Instapaper saves — every article you saved, with save time, \
                      folder (Unread/Archive/Starred), selection, and title. Re-runnable: \
                      re-importing a newer export only adds new saves.",
        domain: "reading",
        vault_path: "reading/instapaper/",
        toggleable: false,
        setup: &[
            "Open instapaper.com and sign in.",
            "Go to Settings → Export → Download .CSV file.",
            "Import the downloaded CSV here.",
        ],
        caveats: "The CSV export covers at most the 2,000 most-recently-saved articles. \
                  Highlights are not included in the CSV; they require the full API \
                  (future upgrade).",
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

// ---------------------------------------------------------------------------
// CSV row.

/// One row from the Instapaper CSV export.
///
/// Headers: `URL,Title,Selection,Folder,Timestamp`
/// (5 columns; Timestamp = Unix epoch seconds of the save time).
/// All columns are always present in normal exports; values may be empty.
/// `Timestamp` defaults to an empty string so the struct also parses older
/// 4-column exports without error.
#[derive(Debug, Deserialize)]
struct CsvRow {
    #[serde(rename = "URL")]
    url: String,
    #[serde(rename = "Title", default)]
    title: String,
    #[serde(rename = "Selection", default)]
    selection: String,
    #[serde(rename = "Folder", default)]
    folder: String,
    /// Unix epoch seconds (e.g. `"1609459200"`). Empty when absent.
    #[serde(rename = "Timestamp", default)]
    timestamp: String,
}

// ---------------------------------------------------------------------------
// Raw row for full-fidelity storage.

#[derive(Serialize)]
struct RawRow {
    /// Routing timestamp (RFC3339 local) — used by `stream.append` for
    /// month-partition; NOT serialised to disk.
    #[serde(skip)]
    ts: String,
    url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    title: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    selection: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    folder: String,
    /// Raw Unix-epoch string from the CSV, preserved verbatim for fidelity.
    /// Empty when the Timestamp column was absent or empty.
    #[serde(skip_serializing_if = "String::is_empty")]
    timestamp: String,
    /// 1-based row index from the CSV (preserves original ordering).
    csv_row: u64,
}

// ---------------------------------------------------------------------------
// Helpers.

/// Unix timestamp string → RFC3339 local time (same as pocket.rs).
///
/// `ts` is an integer-second Unix epoch string like `"1609459200"`.
/// Returns `None` when `ts` is empty or non-numeric so the caller can fall
/// back to the import-noon timestamp.
fn unix_ts_to_local(ts: &str) -> Option<String> {
    let s = ts.trim();
    if s.is_empty() {
        return None;
    }
    let secs: i64 = s.parse().ok()?;
    let dt = Utc.timestamp_opt(secs, 0).single()?;
    Some(dt.with_timezone(&Local).to_rfc3339())
}

/// Stable guid = sha256(url + "|" + timestamp_component).
///
/// `timestamp_component` is the raw CSV epoch string when available, otherwise
/// the import-noon RFC3339 string.  Using both fields means two saves of the
/// same URL at different times (a normal re-add pattern) get distinct guids,
/// and re-importing the same export always produces the same guids.
///
/// Mirrors pocket.rs `make_guid(href, time_added)`.
fn make_guid(url: &str, timestamp_component: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    h.update(b"|");
    h.update(timestamp_component.as_bytes());
    format!("{:x}", h.finalize())
}

/// Map an Instapaper folder name to a reading contract `state` value.
///
/// - `"Unread"` (the default save folder) → `"saved"`
/// - `"Archive"` → `"archived"`
/// - `"Starred"` → `"favorite"`
/// - anything else (user folder) → `"saved"` (the user saved it)
fn folder_to_state(folder: &str) -> &'static str {
    match folder.trim() {
        "Archive" => "archived",
        "Starred" => "favorite",
        _ => "saved",
    }
}

// ---------------------------------------------------------------------------
// Mapping.

/// Map one CSV row + its index into a contract [`Item`] and a [`RawRow`].
///
/// `import_ts` is the import-date-at-noon RFC3339 string used as the fallback
/// when the row's `Timestamp` column is absent or empty.
///
/// Returns `None` when the URL is absent or empty (no usable guid/url).
fn row_to_item(row: &CsvRow, row_idx: u64, import_ts: &str) -> Option<Item> {
    let url = row.url.trim();
    if url.is_empty() {
        return None;
    }

    // Resolve the save timestamp: CSV epoch → local RFC3339, fallback to noon.
    let (ts, ts_component) = match unix_ts_to_local(row.timestamp.trim()) {
        Some(local_ts) => {
            let raw = row.timestamp.trim().to_string();
            (local_ts, raw)
        }
        None => {
            // No timestamp in this row — use import noon; include it in the
            // guid so the key is still deterministic across imports.
            (import_ts.to_string(), import_ts.to_string())
        }
    };

    let guid = make_guid(url, &ts_component);
    let state = folder_to_state(&row.folder);

    let mut extra = Map::new();
    extra.insert("csv_row".into(), Value::Number(row_idx.into()));
    if !row.folder.trim().is_empty() {
        extra.insert("folder".into(), Value::String(row.folder.trim().into()));
    }

    Some(Item {
        ts,
        source: "instapaper".into(),
        guid,
        url: url.to_string(),
        title: row.title.trim().to_string(),
        author: String::new(),
        site: String::new(),
        feed: String::new(),
        excerpt: row.selection.trim().to_string(),
        tags: Vec::new(),
        state: state.to_string(),
        progress: None,
        read_at: String::new(),
        extra,
    })
}

// ---------------------------------------------------------------------------
// Import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let contract = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Build the seen-guid set from existing contract rows.
    let mut seen: HashSet<String> = HashSet::new();
    for key in contract.partitions()? {
        for it in contract.read::<Item>(&key)? {
            if !it.guid.is_empty() {
                seen.insert(it.guid);
            }
        }
    }

    // Derive the import-noon fallback timestamp: today at noon local time.
    // Used only when a row's Timestamp column is absent or empty.
    let today = Local::now().date_naive();
    let import_ts = {
        let dt = Local
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(today.year(), today.month(), today.day())
                    .unwrap_or(today)
                    .and_hms_opt(12, 0, 0)
                    .context("building noon timestamp")?,
            )
            .earliest()
            .context("noon timestamp ambiguous (DST)")?;
        dt.to_rfc3339()
    };

    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;

    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut items: Vec<Item> = Vec::new();
    let mut raws: Vec<RawRow> = Vec::new();

    for result in rdr.deserialize::<CsvRow>() {
        rows += 1;
        let row: CsvRow = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        let url = row.url.trim().to_string();
        if url.is_empty() {
            skipped += 1;
            continue;
        }

        let Some(item) = row_to_item(&row, rows, &import_ts) else {
            skipped += 1;
            continue;
        };

        // Check dedupe AFTER computing the item so we use the correct guid
        // (which incorporates the timestamp, not just the URL).
        if !seen.insert(item.guid.clone()) {
            duplicates += 1;
            continue;
        }

        raws.push(RawRow {
            ts: item.ts.clone(),
            url: row.url.trim().to_string(),
            title: row.title.trim().to_string(),
            selection: row.selection.trim().to_string(),
            folder: row.folder.trim().to_string(),
            timestamp: row.timestamp.trim().to_string(),
            csv_row: rows,
        });
        items.push(item);
        imported += 1;

        if rows % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    contract.append(&items, |i| &i.ts)?;
    raw.append(&raws, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!("{imported} saves imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-instapaper-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn do_import(vault: &Vault, csv_body: &str) -> ImportOutcome {
        let path = vault.root().join("instapaper.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(vault, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — matching the REAL Instapaper CSV export shape.
    //
    // Headers: URL,Title,Selection,Folder,Timestamp
    // Timestamp = Unix epoch seconds of the save time.
    // Folder values: "Unread" | "Archive" | "Starred" | <user folder name>
    //
    // Real epoch values used here:
    //   1609459200 = 2021-01-01T00:00:00Z
    //   1625097600 = 2021-07-01T00:00:00Z
    //   1640995200 = 2022-01-01T00:00:00Z
    //   1656633600 = 2022-07-01T00:00:00Z

    const CSV_TYPICAL: &str = "\
URL,Title,Selection,Folder,Timestamp
https://example.com/article-one,\"Getting Started with Local-First Software\",\"The cloud is someone else's computer.\",Unread,1609459200
https://example.com/article-two,\"A History of Bookmarks\",,Archive,1625097600
https://example.com/starred-read,\"Must Read: Local Databases\",,Starred,1640995200
https://example.com/user-folder,\"In My Custom Folder\",,\"My Reading List\",1656633600
";

    const CSV_MINIMAL: &str = "\
URL,Title,Selection,Folder,Timestamp
https://example.com/bare-url,,,Unread,1609459200
";

    const CSV_MISSING_URL: &str = "\
URL,Title,Selection,Folder,Timestamp
,\"No URL Row\",\"Some selection\",Unread,1609459200
https://example.com/valid,\"Valid Row\",,Archive,1625097600
";

    /// 4-column export (no Timestamp column) — older or stripped format.
    /// Must still parse; rows fall back to import-noon for ts.
    const CSV_NO_TIMESTAMP: &str = "\
URL,Title,Selection,Folder
https://example.com/no-ts-one,\"Old Article\",,Unread
https://example.com/no-ts-two,\"Another Old\",,Archive
";

    // -----------------------------------------------------------------------
    // Unix timestamp helper.

    #[test]
    fn unix_ts_to_local_converts_correctly() {
        // 1609459200 = 2021-01-01T00:00:00Z
        let ts = unix_ts_to_local("1609459200").expect("should parse");
        let dt = DateTime::parse_from_rfc3339(&ts).expect("should be valid RFC3339");
        assert_eq!(dt.timestamp(), 1_609_459_200, "instant preserved");
    }

    #[test]
    fn unix_ts_to_local_empty_returns_none() {
        assert!(unix_ts_to_local("").is_none());
        assert!(unix_ts_to_local("  ").is_none());
    }

    #[test]
    fn unix_ts_to_local_non_numeric_returns_none() {
        assert!(unix_ts_to_local("not-a-number").is_none());
    }

    // -----------------------------------------------------------------------
    // Guid helpers.

    #[test]
    fn make_guid_is_stable_and_distinct() {
        let g1 = make_guid("https://example.com/foo", "1609459200");
        let g2 = make_guid("https://example.com/foo", "1609459200");
        let g3 = make_guid("https://example.com/foo", "1625097600");
        let g4 = make_guid("https://example.com/bar", "1609459200");
        assert_eq!(g1, g2, "same inputs → same guid");
        assert_ne!(g1, g3, "same url, different timestamp → different guid");
        assert_ne!(g1, g4, "different url, same timestamp → different guid");
        // sha256 full hex = 64 chars.
        assert_eq!(g1.len(), 64);
        assert!(g1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // -----------------------------------------------------------------------
    // State helpers.

    #[test]
    fn folder_state_mapping() {
        assert_eq!(folder_to_state("Unread"), "saved");
        assert_eq!(folder_to_state("Archive"), "archived");
        assert_eq!(folder_to_state("Starred"), "favorite");
        assert_eq!(folder_to_state("My Reading List"), "saved", "user folder → saved");
        assert_eq!(folder_to_state(""), "saved", "empty folder → saved");
    }

    // -----------------------------------------------------------------------
    // Row mapping.

    #[test]
    fn maps_unread_row_to_saved_state_with_timestamp() {
        let row = CsvRow {
            url: "https://example.com/article-one".into(),
            title: "Getting Started with Local-First Software".into(),
            selection: "The cloud is someone else's computer.".into(),
            folder: "Unread".into(),
            timestamp: "1609459200".into(),
        };
        let item = row_to_item(&row, 1, "2026-06-17T12:00:00+00:00").unwrap();
        assert_eq!(item.source, "instapaper");
        assert_eq!(item.url, "https://example.com/article-one");
        assert_eq!(item.title, "Getting Started with Local-First Software");
        assert_eq!(item.excerpt, "The cloud is someone else's computer.");
        assert_eq!(item.state, "saved");
        // ts should reflect the real save time (epoch 1609459200), NOT import noon.
        let dt = DateTime::parse_from_rfc3339(&item.ts).unwrap();
        assert_eq!(dt.timestamp(), 1_609_459_200, "ts must be the save epoch");
        // guid uses URL + epoch string.
        assert_eq!(item.guid, make_guid("https://example.com/article-one", "1609459200"));
        assert_eq!(item.guid.len(), 64);
        assert_eq!(item.extra.get("csv_row"), Some(&Value::Number(1.into())));
        assert_eq!(
            item.extra.get("folder"),
            Some(&Value::String("Unread".into()))
        );
    }

    #[test]
    fn maps_archived_row() {
        let row = CsvRow {
            url: "https://example.com/article-two".into(),
            title: "A History of Bookmarks".into(),
            selection: String::new(),
            folder: "Archive".into(),
            timestamp: "1625097600".into(),
        };
        let item = row_to_item(&row, 2, "2026-06-17T12:00:00+00:00").unwrap();
        assert_eq!(item.state, "archived");
        assert!(item.excerpt.is_empty());
        let dt = DateTime::parse_from_rfc3339(&item.ts).unwrap();
        assert_eq!(dt.timestamp(), 1_625_097_600);
    }

    #[test]
    fn maps_starred_row() {
        let row = CsvRow {
            url: "https://example.com/star".into(),
            title: String::new(),
            selection: String::new(),
            folder: "Starred".into(),
            timestamp: "1640995200".into(),
        };
        let item = row_to_item(&row, 3, "2026-06-17T12:00:00+00:00").unwrap();
        assert_eq!(item.state, "favorite");
        let dt = DateTime::parse_from_rfc3339(&item.ts).unwrap();
        assert_eq!(dt.timestamp(), 1_640_995_200);
    }

    #[test]
    fn row_without_timestamp_falls_back_to_import_noon() {
        let import_noon = "2026-06-17T12:00:00+00:00";
        let row = CsvRow {
            url: "https://example.com/no-ts".into(),
            title: "No Timestamp Row".into(),
            selection: String::new(),
            folder: "Unread".into(),
            timestamp: String::new(),
        };
        let item = row_to_item(&row, 1, import_noon).unwrap();
        // ts must be the import noon string.
        assert_eq!(item.ts, import_noon);
        // guid uses URL + import_noon (not empty).
        assert_eq!(item.guid, make_guid("https://example.com/no-ts", import_noon));
    }

    #[test]
    fn different_timestamps_same_url_produce_distinct_guids() {
        let import_noon = "2026-06-17T12:00:00+00:00";
        let row1 = CsvRow {
            url: "https://example.com/resaved".into(),
            title: "Article".into(),
            selection: String::new(),
            folder: "Unread".into(),
            timestamp: "1609459200".into(),
        };
        let row2 = CsvRow {
            url: "https://example.com/resaved".into(),
            title: "Article".into(),
            selection: String::new(),
            folder: "Archive".into(),
            timestamp: "1640995200".into(),
        };
        let item1 = row_to_item(&row1, 1, import_noon).unwrap();
        let item2 = row_to_item(&row2, 2, import_noon).unwrap();
        assert_ne!(item1.guid, item2.guid, "same URL, different save time → different guids");
    }

    #[test]
    fn rejects_empty_url() {
        let row = CsvRow {
            url: String::new(),
            title: "No URL".into(),
            selection: String::new(),
            folder: "Unread".into(),
            timestamp: "1609459200".into(),
        };
        assert!(row_to_item(&row, 1, "2026-06-17T12:00:00+00:00").is_none());
    }

    #[test]
    fn serialized_item_omits_empty_optional_fields() {
        let row = CsvRow {
            url: "https://example.com/bare".into(),
            title: String::new(),
            selection: String::new(),
            folder: "Unread".into(),
            timestamp: "1609459200".into(),
        };
        let item = row_to_item(&row, 1, "2026-06-17T12:00:00+00:00").unwrap();
        let v = serde_json::to_value(&item).unwrap();
        assert!(v.get("ts").is_some());
        assert!(v.get("source").is_some());
        assert!(v.get("guid").is_some());
        assert!(v.get("url").is_some());
        assert!(v.get("title").is_none(), "empty title omitted");
        assert!(v.get("excerpt").is_none(), "empty selection omitted");
        assert!(v.get("author").is_none());
        assert!(v.get("tags").is_none());
        assert!(v.get("progress").is_none());
        assert!(v.get("read_at").is_none());
        assert_eq!(v["state"].as_str(), Some("saved"));
    }

    // -----------------------------------------------------------------------
    // Import integration tests.

    #[test]
    fn typical_import_writes_contract_and_raw() {
        let v = temp_vault("typical");
        let out = do_import(&v, CSV_TYPICAL);
        assert_eq!(out.counts.get("imported"), Some(&4), "4 rows: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert_eq!(out.counts.get("skipped"), Some(&0));
        assert!(
            out.headline.contains("4 saves imported"),
            "headline: {}",
            out.headline
        );

        // Contract layer: 4 different save months → up to 4 JSONL files.
        let contract_dir = v.root().join(DIR);
        let files: Vec<_> = fs::read_dir(&contract_dir)
            .unwrap()
            .filter_map(|e| {
                let e = e.unwrap();
                let n = e.file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") { Some(n) } else { None }
            })
            .collect();
        assert!(!files.is_empty(), "contract JSONL written: {files:?}");

        // Spot-check: ts of the first article must match epoch 1609459200.
        let stream = v.stream(DIR, Partition::Month);
        let mut all_items: Vec<Item> = Vec::new();
        for key in stream.partitions().unwrap() {
            all_items.extend(stream.read::<Item>(&key).unwrap());
        }
        let art_one = all_items
            .iter()
            .find(|i| i.url == "https://example.com/article-one")
            .expect("article-one present");
        let dt = DateTime::parse_from_rfc3339(&art_one.ts).unwrap();
        assert_eq!(dt.timestamp(), 1_609_459_200, "ts = real save epoch");
        assert_eq!(
            art_one.guid,
            make_guid("https://example.com/article-one", "1609459200")
        );

        // Raw layer exists.
        let raw_dir = v.root().join(RAW_DIR);
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().to_string();
                if n.ends_with(".jsonl") { Some(n) } else { None }
            })
            .collect();
        assert!(!raw_files.is_empty(), "raw files written");

        // Raw preserves all source fields including timestamp.
        let raw_body: String = raw_files
            .iter()
            .map(|f| fs::read_to_string(v.root().join(RAW_DIR).join(f)).unwrap())
            .collect();
        assert!(raw_body.contains("\"folder\""), "folder in raw");
        assert!(raw_body.contains("\"csv_row\""), "csv_row in raw");
        assert!(raw_body.contains("\"timestamp\""), "timestamp preserved in raw");
        assert!(raw_body.contains("1609459200"), "epoch value preserved in raw");
    }

    #[test]
    fn typical_import_partitions_by_save_month() {
        let v = temp_vault("partitions");
        do_import(&v, CSV_TYPICAL);

        // 4 articles saved in Jan-2021, Jul-2021, Jan-2022, Jul-2022
        // → 4 distinct month partitions (accounting for local TZ offset).
        let stream = v.stream(DIR, Partition::Month);
        let keys: Vec<_> = stream.partitions().unwrap();
        // Must have at least 2 distinct months (could be 4 depending on TZ).
        assert!(keys.len() >= 2, "articles spread across multiple months: {keys:?}");
    }

    #[test]
    fn reimport_same_csv_is_idempotent() {
        let v = temp_vault("reimport");
        let first = do_import(&v, CSV_TYPICAL);
        assert_eq!(first.counts.get("imported"), Some(&4));

        let second = do_import(&v, CSV_TYPICAL);
        assert_eq!(second.counts.get("imported"), Some(&0));
        assert_eq!(second.counts.get("duplicates"), Some(&4));
    }

    #[test]
    fn incremental_import_adds_only_new_saves() {
        let v = temp_vault("incremental");
        let csv_old = "\
URL,Title,Selection,Folder,Timestamp
https://example.com/existing,\"Old Save\",,Unread,1609459200
";
        let csv_new = "\
URL,Title,Selection,Folder,Timestamp
https://example.com/existing,\"Old Save\",,Unread,1609459200
https://example.com/new-one,\"New Save\",,Archive,1625097600
";
        let first = do_import(&v, csv_old);
        assert_eq!(first.counts.get("imported"), Some(&1));

        let second = do_import(&v, csv_new);
        assert_eq!(second.counts.get("imported"), Some(&1), "only the new save");
        assert_eq!(second.counts.get("duplicates"), Some(&1));
    }

    #[test]
    fn same_url_different_timestamp_creates_distinct_saves() {
        // Simulates a URL re-saved at a later date — the guid must differ.
        let v = temp_vault("resave");
        let csv = "\
URL,Title,Selection,Folder,Timestamp
https://example.com/resaved,\"First Save\",,Unread,1609459200
https://example.com/resaved,\"Archived Then Re-added\",,Unread,1640995200
";
        let out = do_import(&v, csv);
        assert_eq!(out.counts.get("imported"), Some(&2), "two distinct saves: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
    }

    #[test]
    fn skips_rows_with_missing_url() {
        let v = temp_vault("missingurl");
        let out = do_import(&v, CSV_MISSING_URL);
        // First row has empty URL → skipped; second row is valid.
        assert_eq!(out.counts.get("imported"), Some(&1));
        assert_eq!(out.counts.get("skipped"), Some(&1));
    }

    #[test]
    fn minimal_row_only_url_imports_cleanly() {
        let v = temp_vault("minimal");
        let out = do_import(&v, CSV_MINIMAL);
        assert_eq!(out.counts.get("imported"), Some(&1));

        // Verify the contract item.
        let contract = v.stream(DIR, Partition::Month);
        let mut all_items: Vec<Item> = Vec::new();
        for key in contract.partitions().unwrap() {
            all_items.extend(contract.read::<Item>(&key).unwrap());
        }
        assert_eq!(all_items.len(), 1);
        let item = &all_items[0];
        assert_eq!(item.url, "https://example.com/bare-url");
        assert_eq!(item.state, "saved");
        assert!(item.title.is_empty());
        assert!(item.excerpt.is_empty());
        // ts should reflect epoch 1609459200.
        let dt = DateTime::parse_from_rfc3339(&item.ts).unwrap();
        assert_eq!(dt.timestamp(), 1_609_459_200);
    }

    #[test]
    fn no_timestamp_column_falls_back_to_import_noon() {
        // 4-column export (no Timestamp column) — still parseable.
        let v = temp_vault("notimestamp");
        let out = do_import(&v, CSV_NO_TIMESTAMP);
        assert_eq!(out.counts.get("imported"), Some(&2), "both rows imported: {out:?}");

        let contract = v.stream(DIR, Partition::Month);
        let mut all_items: Vec<Item> = Vec::new();
        for key in contract.partitions().unwrap() {
            all_items.extend(contract.read::<Item>(&key).unwrap());
        }
        assert_eq!(all_items.len(), 2);
        // Both items should land in today's month partition (import noon fallback).
        // Their guids should differ (different URLs, same import_noon component).
        let g0 = &all_items.iter().find(|i| i.url == "https://example.com/no-ts-one").unwrap().guid;
        let g1 = &all_items.iter().find(|i| i.url == "https://example.com/no-ts-two").unwrap().guid;
        assert_ne!(g0, g1, "different URLs → different guids");
    }

    #[test]
    fn state_mapping_in_full_import() {
        let v = temp_vault("states");
        do_import(&v, CSV_TYPICAL);

        let contract = v.stream(DIR, Partition::Month);
        let mut items: Vec<Item> = Vec::new();
        for key in contract.partitions().unwrap() {
            items.extend(contract.read::<Item>(&key).unwrap());
        }
        assert_eq!(items.len(), 4);

        let by_url: std::collections::HashMap<_, _> =
            items.iter().map(|i| (i.url.as_str(), i.state.as_str())).collect();
        assert_eq!(by_url["https://example.com/article-one"], "saved");
        assert_eq!(by_url["https://example.com/article-two"], "archived");
        assert_eq!(by_url["https://example.com/starred-read"], "favorite");
        assert_eq!(by_url["https://example.com/user-folder"], "saved",
                   "user folder → saved");
    }

    #[test]
    fn def_is_import_behavior_in_reading_domain() {
        assert_eq!(DEF.meta.id, "instapaper");
        assert_eq!(DEF.meta.domain, "reading");
        assert_eq!(DEF.meta.vault_path, "reading/instapaper/");
        assert!(DEF.import_spec().is_some(), "Import behavior");
        let spec = DEF.import_spec().unwrap();
        assert!(spec.accepts.contains(&"csv"), "accepts csv");
        assert!(DEF.connection.is_none(), "no connection for v1 import");
        assert!(DEF.pull.is_none(), "no pull for v1 import");
        assert!(DEF.last_data.is_some());
    }
}
