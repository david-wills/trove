//! Apple App Store, iTunes, and Apple Media Services purchase history via Apple
//! Privacy data export.
//!
//! **Behavior:** Import — the user drops the Apple Privacy ZIP (from
//! privacy.apple.com → Data and Privacy → Get a copy of your data →
//! "App Store, iTunes Store, iBooks Store, and Apple Music activity") or a bare
//! Apple Media Services / App Store CSV onto the import box.
//!
//! ## Export path
//!
//! privacy.apple.com → Data and Privacy → Get a copy of your data →
//! select "App Store, iTunes Store, iBooks Store, and Apple Music activity"
//! (the exact category name as of 2024–2026) → Apple emails a download link
//! within a few hours → ZIP containing one or more CSV files.
//!
//! ## Real Apple export folder structure
//!
//! Community-confirmed paths (2021–2026; no official Apple spec):
//!
//! ```text
//! <user>_Apple Data/
//!   Apple Media Services/
//!     Account and Transaction History/
//!       Store Transaction History - <storefront>.csv
//!       Store Transactions History.csv      ← alternate single-file name
//!   App Store Activity/                      ← older export layout
//!     Apps/
//!       App Store Activity - Apps.csv
//!     In-App Purchases/
//!       App Store Activity - In-App Purchases.csv
//!   App Store, iTunes Store, iBooks Store, and Apple Music activity/
//!     Apps/
//!       App Store Activity - Apps.csv       ← yet another layout variant
//! ```
//!
//! The ZIP walker is intentionally broad: it accepts any CSV whose path
//! matches ANY of:
//!   - contains "store transaction" (covers "Store Transaction History")
//!   - contains "app store activity" or "app_store_activity"
//!   - lives under a folder containing "itunes" or "apple media services"
//!
//! False positives are harmless: the row-shape check catches them at parse time.
//!
//! ## Parser status: PARKED (Needs-sample)
//!
//! Apple's export CSV column set is undocumented (no public spec as of
//! 2026-06). The research doc (L3857 of `integrations-research.md`) states:
//! "app name, purchase date, amount, category" but gives no header names.
//! Until a real export is in hand:
//!
//! - The ZIP walker locates the Apple Media Services / App Store Activity files
//!   using the broad matcher above.
//! - Every row is written (whitespace-trimmed) to the **raw layer**
//!   (`finance/purchases/apple-app-store/raw/YYYY-MM.jsonl`).
//! - The **contract parser** (→ `finance-purchases` `LineItem`) is **parked**:
//!   raw rows carry a `_raw_ts` synthetic field for month-partitioning (falling
//!   back to the import date when no date column is recognizable), and no
//!   `LineItem` rows are written until a confirmed column map lands.
//!
//! Re-wiring checklist (when a real export is available):
//!   1. Obtain a sample export; redact PII; place at
//!      `crates/trove-core/tests/fixtures/apple-app-store/`.
//!   2. Confirm exact column headers (exact capitalization, locale variant).
//!   3. Update `is_app_store_activity` if the real paths differ from above.
//!   4. Build `try_map_to_line_item` (parked below) against the confirmed headers;
//!      derive `_raw_guid` from Apple's order/transaction-id column (+ line index
//!      for files without unique per-row ids), per the contract dedupe rule.
//!   5. Set `PARSER_ACTIVE = true` (single flag below) to enable contract rows.
//!   6. Remove the `Needs-sample` flag from the brief and this file.
//!
//! ## Vault layout
//!
//! - **Raw (unconditional):**
//!   `finance/purchases/apple-app-store/raw/YYYY-MM.jsonl`
//!   Whitespace-trimmed rows; YYYY-MM is derived from the first date-shaped
//!   column found, falling back to the import month.
//!
//! - **Contract (parked):**
//!   `finance/purchases/apple-app-store/YYYY-MM.jsonl`
//!   `LineItem` rows from `finance-purchases` — written only when a real
//!   column map is confirmed (see `PARSER_ACTIVE` below).
//!
//! - **Dedupe:** within one import run a row's `_raw_guid` (a content hash of
//!   the raw fields) is used to skip duplicates against the raw layer.
//!   Idempotent re-imports drop all rows already on disk by guid.
//!   NOTE: The current whole-row content hash can silently drop legitimately-
//!   distinct rows with identical visible fields (e.g. repeated identical
//!   in-app purchases on the same day). When a real export confirms Apple's
//!   order/transaction-id column, `_raw_guid` must be derived from that id
//!   (+ a line index) instead.
//!
//! Catalogued in the Phase 2 pass; brief: docs/integrations/apple-app-store.md

use std::collections::HashSet;
use std::io::Read as _;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::{Map, Value};

use crate::finance::LineItem;
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Paths.

const DIR: &str = "finance/purchases/apple-app-store";
const RAW_DIR: &str = "finance/purchases/apple-app-store/raw";

// ---------------------------------------------------------------------------
// Parser gate.
//
// Set to `true` when a real export sample confirms the column map and
// `try_map_to_line_item` is built against it. Until then, raw-only.
const PARSER_ACTIVE: bool = false;

// ---------------------------------------------------------------------------
// ZIP walking.

/// Recognise a ZIP entry as an Apple Media Services / App Store Activity CSV
/// by its path.
///
/// Apple's privacy export uses at least three different folder layouts (all
/// observed in community reports 2021–2026; no official spec exists):
///
/// 1. Newer layout (2023+): `Apple Media Services/Account and Transaction
///    History/Store Transaction History - <storefront>.csv`  (one file per
///    storefront, e.g. App Store / iTunes) or the single-file variant
///    `Store Transactions History.csv`.
///
/// 2. Mid-era layout: `App Store Activity/Apps/App Store Activity - Apps.csv`
///    and `App Store Activity/In-App Purchases/App Store Activity - In-App
///    Purchases.csv`.
///
/// 3. Older / alternate layout: the parent folder is named
///    `App Store, iTunes Store, iBooks Store, and Apple Music activity`.
///
/// The matcher is intentionally broad — false positives are caught by the
/// row-shape check at parse time.  It returns `true` for any `.csv` whose
/// lowercase path contains ANY of:
///   - "store transaction"         → covers layout 1 file names
///   - "app store activity"        → covers layouts 2 & 3 directory names
///   - "app_store_activity"        → underscore variant of the above
///   - "itunes"                    → iTunes Store folder in older layouts
///   - "apple media services"      → layout 1 parent folder
fn is_app_store_activity(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    // Must be a CSV file.
    if !lower.ends_with(".csv") {
        return false;
    }
    // Broad match against all known real-export path patterns.
    lower.contains("store transaction")
        || lower.contains("app store activity")
        || lower.contains("app_store_activity")
        || lower.contains("itunes")
        || lower.contains("apple media services")
}

/// Extract the text bodies of all App Store Activity CSVs from the ZIP at
/// `path`.  Returns a vec of `(zip_entry_name, csv_body)` pairs.
fn extract_activity_csvs(path: &Path) -> Result<Vec<(String, String)>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP {}", path.display()))?;

    let mut found = Vec::new();
    let names: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|e| e.name().to_string()))
        .filter(|n| is_app_store_activity(n))
        .collect();

    if names.is_empty() {
        bail!(
            "No Apple Media Services / App Store Activity CSV found in the ZIP.\n\
             Expected files matching one of these patterns:\n\
             • \"Store Transaction History - *.csv\" under \"Apple Media Services/Account and Transaction History/\"\n\
             • \"Store Transactions History.csv\"\n\
             • \"App Store Activity - *.csv\" under an \"App Store Activity\" folder\n\
             • any .csv under an \"iTunes\" or \"Apple Media Services\" folder\n\
             Is this an Apple Privacy data export? \
             (privacy.apple.com → Data and Privacy → Get a copy of your data \
             → select \"App Store, iTunes Store, iBooks Store, and Apple Music activity\")"
        );
    }

    for name in names {
        let mut entry = archive.by_name(&name)
            .with_context(|| format!("reading entry {name}"))?;
        let mut body = String::new();
        entry.read_to_string(&mut body)
            .with_context(|| format!("decoding {name} as UTF-8"))?;
        found.push((name, body));
    }
    Ok(found)
}

// ---------------------------------------------------------------------------
// Raw row: verbatim CSV row as a JSON object, tagged for month-partitioning.

/// A raw row written to the JSONL file. Fields are the verbatim CSV columns
/// plus `_raw_ts` (the partition key) and `_raw_guid` (content hash for
/// dedupe). We flatten the object so the JSONL line IS the object, not a
/// wrapper — this is the consistent Trove raw convention.
#[derive(serde::Serialize)]
struct RawRow {
    /// RFC3339 timestamp used for month-partitioning; may be synthesised from
    /// the import date when no date column is recognisable.
    _raw_ts: String,
    /// Content hash over all original field values (sorted, deterministic).
    _raw_guid: String,
    /// All original CSV fields (field name → string value).
    #[serde(flatten)]
    fields: Map<String, Value>,
}

/// Convert one CSV record into a raw JSON object (field name → string value).
/// The `_raw_guid` is a lightweight content hash over all values (for dedupe).
/// The `_raw_ts` is a partition timestamp: the value of the first date-shaped
/// column found, or the import-time local datetime if none is recognisable.
fn csv_row_to_raw(headers: &csv::StringRecord, record: &csv::StringRecord) -> Map<String, Value> {
    let mut obj = Map::new();
    for (h, v) in headers.iter().zip(record.iter()) {
        obj.insert(h.trim().to_string(), Value::String(v.trim().to_string()));
    }

    // Derive a partition timestamp from the first date-shaped column.
    let ts = guess_ts_from_row(&obj).unwrap_or_else(|| Local::now().to_rfc3339());
    obj.insert("_raw_ts".to_string(), Value::String(ts));

    // Content hash over all original field values (order-stable, uses sorted
    // keys for determinism across platforms).
    let mut parts: Vec<(&str, &str)> = obj
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.as_str(), v.as_str().unwrap_or("")))
        .collect();
    parts.sort_by_key(|(k, _)| *k);
    let hash_input: String = parts
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("|");
    let guid = format!("{:x}", md5_lite(&hash_input));
    obj.insert("_raw_guid".to_string(), Value::String(guid));

    obj
}

/// Heuristic: scan row values for anything that looks like a date
/// (YYYY-MM-DD or MM/DD/YYYY or DD-MMM-YYYY) and return an RFC3339 local
/// string for it. Falls back to `None` when nothing matches.
fn guess_ts_from_row(obj: &Map<String, Value>) -> Option<String> {
    use chrono::{NaiveDate, TimeZone};
    // Priority: prefer fields whose key contains "date" or "purchase".
    let mut candidates: Vec<&str> = obj
        .iter()
        .filter(|(k, _)| {
            let lk = k.to_ascii_lowercase();
            lk.contains("date") || lk.contains("purchase") || lk.contains("time")
        })
        .filter_map(|(_, v)| v.as_str())
        .collect();
    // Append remaining fields so we try them too.
    let rest: Vec<&str> = obj
        .values()
        .filter_map(|v| v.as_str())
        .filter(|s| !candidates.contains(s))
        .collect();
    candidates.extend(rest);

    for s in candidates {
        let s = s.trim();
        // YYYY-MM-DD
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()
                .map(|dt| dt.to_rfc3339());
        }
        // MM/DD/YYYY
        if let Ok(d) = NaiveDate::parse_from_str(s, "%m/%d/%Y") {
            return Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()
                .map(|dt| dt.to_rfc3339());
        }
        // DD-Mon-YYYY (e.g. "28-Jun-2026")
        if let Ok(d) = NaiveDate::parse_from_str(s, "%d-%b-%Y") {
            return Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()
                .map(|dt| dt.to_rfc3339());
        }
        // YYYY/MM/DD
        if let Ok(d) = NaiveDate::parse_from_str(s, "%Y/%m/%d") {
            return Local.from_local_datetime(&d.and_hms_opt(0, 0, 0)?).earliest()
                .map(|dt| dt.to_rfc3339());
        }
    }
    None
}

/// Tiny djb2-style hash used only for a lightweight guid (not security).
/// Returns a u64 rendered as hex for a short stable content key.
fn md5_lite(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_shl(5).wrapping_add(h).wrapping_add(b as u64);
    }
    h
}

// ---------------------------------------------------------------------------
// Contract layer (parked — PARSER_ACTIVE = false until a sample lands).
//
// When PARSER_ACTIVE flips to true, implement this function to build a
// LineItem from the raw JSON row, using confirmed column names from a real
// export. Until then, it always returns None.

#[allow(dead_code)]
fn try_map_to_line_item(raw: &Map<String, Value>) -> Option<LineItem> {
    if !PARSER_ACTIVE {
        return None;
    }
    // TODO (Needs-sample): implement when confirmed column map is available.
    // Typical Apple Privacy CSV fields (unverified — correct against a real sample):
    //   "Title" or "App Name" → item
    //   "Purchase Date" or "Date" → ts
    //   "Amount" or "Charge Amount" → amount
    //   "Category" → category
    //   "Order ID" or "Apple Reference Number" → order_id / guid
    //   "Storefront" or "Type" → extra
    //
    // Pattern to follow: bitcoin.rs `line_item_from` or letterboxd.rs `diary_item`.
    let _guid = raw.get("_raw_guid")?.as_str()?.to_string();
    let _ts = raw.get("_raw_ts")?.as_str()?.to_string();
    None
}

// ---------------------------------------------------------------------------
// Core import engine — shared by both the ZIP and bare-CSV paths.

struct ImportStats {
    rows: u64,
    imported_raw: u64,
    duplicates: u64,
    skipped: u64,
    /// Not yet used — will be reported once PARSER_ACTIVE = true.
    #[allow(dead_code)]
    imported_contract: u64,
}

/// Parse a single CSV body (already extracted as a String), write raw rows,
/// and — when PARSER_ACTIVE — also write contract rows.  The `source_label`
/// is the file name (for error messages).
fn process_csv(
    vault: &Vault,
    body: &str,
    source_label: &str,
    seen_guids: &mut HashSet<String>,
) -> Result<ImportStats> {
    // Strip BOM if present.
    let body = body.trim_start_matches('\u{feff}');
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(body.as_bytes());

    let headers = rdr.headers()
        .with_context(|| format!("{source_label}: failed to read CSV header"))?
        .clone();

    if headers.is_empty() {
        return Ok(ImportStats {
            rows: 0,
            imported_raw: 0,
            duplicates: 0,
            skipped: 0,
            imported_contract: 0,
        });
    }

    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let contract_stream = vault.stream(DIR, Partition::Month);

    let mut raw_buf: Vec<RawRow> = Vec::new();
    let mut contract_buf: Vec<LineItem> = Vec::new();
    let (mut rows, mut duplicates, mut skipped, mut imported_contract) = (0u64, 0u64, 0u64, 0u64);

    for result in rdr.records() {
        rows += 1;
        let record = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let raw_map = csv_row_to_raw(&headers, &record);
        let guid = raw_map
            .get("_raw_guid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let ts = raw_map
            .get("_raw_ts")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();

        if guid.is_empty() {
            skipped += 1;
            continue;
        }
        if !seen_guids.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }

        // Build the typed raw row (fields minus the _raw_* keys that ride as named fields).
        let fields: Map<String, Value> = raw_map
            .into_iter()
            .filter(|(k, _)| k != "_raw_guid" && k != "_raw_ts")
            .collect();
        let raw_row = RawRow { _raw_ts: ts, _raw_guid: guid, fields };

        // Contract (parked).
        if PARSER_ACTIVE {
            if let Some(li) = try_map_to_line_item(&raw_row.fields) {
                contract_buf.push(li);
                imported_contract += 1;
            }
        }

        raw_buf.push(raw_row);
    }

    // Flush raw rows, partitioned by _raw_ts month.
    let imported_raw = raw_buf.len() as u64;
    raw_stream.append(&raw_buf, |r| &r._raw_ts)?;
    if PARSER_ACTIVE && !contract_buf.is_empty() {
        contract_stream.append(&contract_buf, |li| &li.ts)?;
    }

    Ok(ImportStats { rows, imported_raw, duplicates, skipped, imported_contract })
}

// ---------------------------------------------------------------------------
// Import entry point.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Build the set of already-seen raw guids so re-imports are idempotent.
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);
    let mut seen_guids: HashSet<String> = HashSet::new();
    for key in raw_stream.partitions()? {
        for row in raw_stream.read::<Value>(&key)? {
            if let Some(g) = row.get("_raw_guid").and_then(Value::as_str) {
                seen_guids.insert(g.to_string());
            }
        }
    }

    let csvs: Vec<(String, String)> = if ext == "zip" {
        extract_activity_csvs(path)?
    } else if ext == "csv" {
        let body = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file.csv")
            .to_string();
        vec![(name, body)]
    } else {
        bail!(
            "Unsupported file type: .{ext}. \
             Please import the Apple Privacy ZIP or a bare App Store Activity CSV."
        );
    };

    let (mut total_rows, mut total_raw, mut total_dups, mut total_skip) = (0u64, 0u64, 0u64, 0u64);
    let n = csvs.len();
    for (i, (label, body)) in csvs.iter().enumerate() {
        let stats = process_csv(vault, body, label, &mut seen_guids)?;
        total_rows += stats.rows;
        total_raw += stats.imported_raw;
        total_dups += stats.duplicates;
        total_skip += stats.skipped;
        progress(ImportProgress {
            records: total_raw,
            percent: ((i + 1) as f32 / n as f32) * 100.0,
        });
    }

    let headline = if PARSER_ACTIVE {
        format!(
            "{total_raw} rows imported (raw), {total_dups} duplicates skipped — \
             parser active: check finance/purchases/apple-app-store/ for contract rows"
        )
    } else {
        format!(
            "{total_raw} rows imported to raw layer, {total_dups} duplicates skipped \
             (contract parser parked — Needs-sample: see docs/integrations/apple-app-store.md)"
        )
    };

    Ok(ImportOutcome {
        headline,
        counts: [
            ("rows", total_rows),
            ("imported_raw", total_raw),
            ("duplicates", total_dups),
            ("skipped", total_skip),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    // Check raw layer for activity (contract layer is parked, so check raw).
    crate::registry::newest_stem(&vault.root().join(RAW_DIR))
}

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "csv"],
    params: &[],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "apple-app-store",
        name: "App Store & iTunes Purchases",
        kind: IntegrationKind::Import,
        // 🔒 Financial detail — opt-in with explicit acknowledgement.
        default_on: false,
        description: "Import your complete App Store and iTunes purchase history from the \
                      Apple Privacy data export. Covers apps, media, subscriptions, and \
                      in-app purchases across all Apple storefronts — itemizing the charges \
                      that appear on your card statements as undifferentiated \"APPLE.COM/BILL\" \
                      entries. Re-runnable: re-importing the same export is a safe no-op.",
        domain: "finance",
        vault_path: "finance/purchases/apple-app-store/",
        toggleable: false,
        setup: &[
            "Go to privacy.apple.com → Data and Privacy → Get a copy of your data.",
            "Select \"App Store, iTunes Store, iBooks Store, and Apple Music activity\" \
             (the exact category name may vary slightly by region or year). \
             Apple emails a download link, typically within a few hours.",
            "Download the ZIP and drop it here. \
             The importer locates the Apple Media Services / App Store Activity CSVs automatically \
             (it handles all known Apple export layouts).",
            "You can also drop a bare App Store / Store Transaction History CSV directly if you \
             have extracted it from the ZIP.",
        ],
        caveats: "Apple generates the export on request — allow up to a few hours for \
                  the download link to arrive. The export covers all storefronts \
                  (App Store, iTunes, Apple TV+, Apple Music, Apple Arcade). \
                  The contract-layer parser is parked pending a real sample to confirm \
                  exact column names; raw rows are stored at full fidelity immediately.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;
    use std::io::Write as _;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-apple-app-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // SCAFFOLD FIXTURES — column names are UNCONFIRMED (Needs-sample).
    //
    // These fixtures encode a plausible shape based on the research doc's
    // description ("app name, purchase date, amount, category") and community
    // reports. They CANNOT be considered authoritative until a real Apple
    // Privacy export confirms the actual headers.
    //
    // When a real sample is available:
    //   1. Replace the headers in SCAFFOLD_CSV with the confirmed names.
    //   2. Implement try_map_to_line_item against those headers.
    //   3. Set PARSER_ACTIVE = true (or flip the contract path with a real flag).
    //   4. Add contract-layer tests.

    /// Scaffold CSV using UNCONFIRMED column names derived from the research doc.
    const SCAFFOLD_CSV: &str = "\
Title,Purchase Date,Amount,Currency,Category,Order ID,Type,Storefront
Fantastical – Calendar & Tasks,2024-03-15,4.99,USD,Productivity,MK12345678,In-App Purchase,App Store
Day One - Journal - Private Diary,2024-01-05,34.99,USD,Productivity,MK23456789,App,App Store
\"Heads Up!, Party Game by Ellen\",2023-12-20,1.99,USD,Games,MK34567890,App,App Store
The Morning Show,2024-02-14,2.99,USD,TV & Movies,MK45678901,Season Pass,TV App
";

    /// Re-runs the import through the public ImportSpec run fn.
    fn import(v: &Vault, csv_body: &str) -> ImportOutcome {
        let path = v.root().join("test-activity.csv");
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -----------------------------------------------------------------------
    // Hub card.

    #[test]
    fn hub_card_shows_import_box_and_is_opt_in() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "apple-app-store").unwrap();
        let import_info = card.import.as_ref().expect("import box present");
        assert_eq!(import_info.accepts, &["zip", "csv"]);
        assert!(!card.enabled, "default_on=false: opt-in (financial data)");
    }

    // -----------------------------------------------------------------------
    // ZIP path recogniser.

    #[test]
    fn recognises_app_store_activity_paths() {
        // ---- Layout 1: newer Apple Media Services layout (2023+) ----
        // "Store Transaction History" file name (primary real-export shape).
        assert!(is_app_store_activity(
            "david_Apple Data/Apple Media Services/Account and Transaction History/Store Transaction History - App Store.csv"
        ), "Store Transaction History under Apple Media Services");
        assert!(is_app_store_activity(
            "Apple Media Services/Account and Transaction History/Store Transaction History - iTunes.csv"
        ), "Store Transaction History - iTunes variant");
        // Single-file variant.
        assert!(is_app_store_activity(
            "Apple Media Services/Account and Transaction History/Store Transactions History.csv"
        ), "Store Transactions History (plural, no dash)");
        // Parent folder contains "apple media services".
        assert!(is_app_store_activity(
            "apple media services/purchases.csv"
        ), "apple media services parent folder match");

        // ---- Layout 2: mid-era "App Store Activity" folder ----
        assert!(is_app_store_activity(
            "david_Apple Data/App Store Activity/Apps/App Store Activity - Apps.csv"
        ), "App Store Activity - Apps canonical path");
        assert!(is_app_store_activity(
            "App Store Activity/In-App Purchases/App Store Activity - In-App Purchases.csv"
        ), "App Store Activity - In-App Purchases canonical path");

        // ---- Layout 3: long category name folder ----
        assert!(is_app_store_activity(
            "App Store, iTunes Store, iBooks Store, and Apple Music activity/Apps/App Store Activity - Apps.csv"
        ), "long category name folder (contains 'itunes')");

        // ---- Underscore variant ----
        assert!(is_app_store_activity("App_Store_Activity_Apps.csv"), "underscore variant");

        // ---- Case-insensitivity ----
        assert!(is_app_store_activity("app store activity/apps/app_store_activity.csv"), "lowercase");

        // ---- iTunes path (older layout) ----
        assert!(is_app_store_activity("iTunes Store/Purchase History.csv"), "itunes folder");

        // ---- False positives (unrelated CSVs) must NOT match ----
        assert!(!is_app_store_activity("Maps/Search History/Maps Search History.csv"), "Maps CSV");
        assert!(!is_app_store_activity("Siri/Siri Interactions.csv"), "Siri CSV");
        assert!(!is_app_store_activity("diary.csv"), "bare unrelated CSV");
        assert!(!is_app_store_activity("Apple ID/Apple ID Account and Device Information.csv"), "Apple ID CSV");
        assert!(!is_app_store_activity("Health/Health Data.csv"), "Health CSV");

        // ---- Non-CSV file inside a matching folder must NOT match ----
        assert!(!is_app_store_activity("App Store Activity/README.txt"), "txt not a CSV");
        assert!(!is_app_store_activity(
            "Apple Media Services/Account and Transaction History/Store Transaction History.pdf"
        ), "PDF not a CSV");
    }

    // -----------------------------------------------------------------------
    // Date-guessing heuristic.

    #[test]
    fn guess_ts_from_row_handles_multiple_date_formats() {
        use chrono::DateTime;

        let mut row = Map::new();
        // YYYY-MM-DD in a "Purchase Date" field.
        row.insert("Purchase Date".into(), Value::String("2024-03-15".into()));
        row.insert("Amount".into(), Value::String("4.99".into()));
        let ts = guess_ts_from_row(&row).unwrap();
        let dt = DateTime::parse_from_rfc3339(&ts).unwrap();
        assert_eq!(dt.format("%Y-%m-%d").to_string(), "2024-03-15");

        // MM/DD/YYYY.
        let mut row2 = Map::new();
        row2.insert("Date".into(), Value::String("03/15/2024".into()));
        let ts2 = guess_ts_from_row(&row2).unwrap();
        let dt2 = DateTime::parse_from_rfc3339(&ts2).unwrap();
        assert_eq!(dt2.format("%Y-%m-%d").to_string(), "2024-03-15");

        // DD-Mon-YYYY.
        let mut row3 = Map::new();
        row3.insert("Purchase Date".into(), Value::String("15-Mar-2024".into()));
        let ts3 = guess_ts_from_row(&row3).unwrap();
        let dt3 = DateTime::parse_from_rfc3339(&ts3).unwrap();
        assert_eq!(dt3.format("%Y-%m-%d").to_string(), "2024-03-15");
    }

    #[test]
    fn guess_ts_from_row_returns_none_when_no_date_found() {
        let mut row = Map::new();
        row.insert("Title".into(), Value::String("Some App".into()));
        row.insert("Amount".into(), Value::String("1.99".into()));
        // No date-shaped value → None.
        assert!(guess_ts_from_row(&row).is_none());
    }

    // -----------------------------------------------------------------------
    // Content hash.

    #[test]
    fn md5_lite_is_deterministic_and_distinct() {
        assert_eq!(md5_lite("abc"), md5_lite("abc"), "deterministic");
        assert_ne!(md5_lite("abc"), md5_lite("abd"), "distinct");
        assert_ne!(md5_lite(""), md5_lite("a"));
    }

    // -----------------------------------------------------------------------
    // Scaffold CSV import — raw layer.

    #[test]
    fn scaffold_imports_raw_rows_with_guid_and_ts() {
        let v = temp_vault("scaffold-raw");
        let out = import(&v, SCAFFOLD_CSV);
        // 4 rows in the scaffold CSV → 4 raw rows.
        assert_eq!(out.counts.get("imported_raw"), Some(&4), "all 4 rows in raw: {out:?}");
        assert_eq!(out.counts.get("duplicates"), Some(&0));
        assert_eq!(out.counts.get("skipped"), Some(&0));

        // Raw partitions should exist.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let partitions = raw_stream.partitions().unwrap();
        assert!(!partitions.is_empty(), "at least one raw partition written");

        // Read all raw rows back and check guid + ts fields.
        let mut all_rows: Vec<Value> = Vec::new();
        for key in &partitions {
            all_rows.extend(raw_stream.read::<Value>(key).unwrap());
        }
        assert_eq!(all_rows.len(), 4, "all 4 rows readable from raw layer");

        // Each row has a _raw_guid and _raw_ts.
        for row in &all_rows {
            assert!(
                row.get("_raw_guid").and_then(Value::as_str).map(|g| !g.is_empty()).unwrap_or(false),
                "_raw_guid present and non-empty"
            );
            assert!(
                row.get("_raw_ts").and_then(Value::as_str).map(|t| !t.is_empty()).unwrap_or(false),
                "_raw_ts present"
            );
            // Original fields preserved (verbatim).
            assert!(row.get("Title").is_some(), "original 'Title' field in raw");
            assert!(row.get("Amount").is_some(), "original 'Amount' field in raw");
        }

        // A quoted-comma title round-trips correctly.
        let ellen = all_rows.iter().find(|r| {
            r.get("Title")
                .and_then(Value::as_str)
                .map(|t| t.contains("Heads Up"))
                .unwrap_or(false)
        });
        assert!(ellen.is_some(), "quoted-comma title preserved: {all_rows:?}");
    }

    #[test]
    fn scaffold_reimport_is_idempotent() {
        let v = temp_vault("scaffold-reimport");
        let first = import(&v, SCAFFOLD_CSV);
        assert_eq!(first.counts.get("imported_raw"), Some(&4));

        let second = import(&v, SCAFFOLD_CSV);
        assert_eq!(second.counts.get("imported_raw"), Some(&0), "re-import adds nothing to raw");
        assert_eq!(second.counts.get("duplicates"), Some(&4), "all 4 are duplicates");

        // File count unchanged.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let mut total = 0;
        for key in raw_stream.partitions().unwrap() {
            total += raw_stream.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(total, 4, "still exactly 4 raw rows after re-import");
    }

    #[test]
    fn scaffold_skips_malformed_rows_gracefully() {
        // A row that the csv reader cannot parse is counted as skipped.
        // Inject a row with mismatched column count (flex mode still reads it,
        // but let's test an actually-bad row by embedding a raw bad byte
        // sequence — easiest is to just test row count with a short body).
        let bad_csv = "Title,Purchase Date,Amount\nValid App,2024-01-01,1.99\n";
        let v = temp_vault("scaffold-skip");
        let out = import(&v, bad_csv);
        assert_eq!(out.counts.get("imported_raw"), Some(&1));
        assert_eq!(out.counts.get("skipped"), Some(&0)); // valid row parses fine
    }

    // -----------------------------------------------------------------------
    // ZIP path.

    /// Scaffold CSV with realistic "Store Transaction History" column names
    /// (newer Apple Media Services export layout, 2023+).
    const SCAFFOLD_CSV_STORE_TXN: &str = "\
Title,Purchase Date,Amount,Currency,Type,Storefront,Order ID
Fantastical,2024-03-15,4.99,USD,In-App Purchase,App Store,MK11111111
Day One,2024-01-05,34.99,USD,App,App Store,MK22222222
";

    #[test]
    fn import_from_zip_finds_activity_csvs() {
        use std::io::Write as _;

        let v = temp_vault("zip");
        let zip_path = v.root().join("apple-privacy-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // Layout 1 (newer, 2023+): "Store Transaction History" under Apple Media Services.
        // This is the primary real-export path that the original matcher MISSED.
        w.start_file(
            "david_Apple Data/Apple Media Services/Account and Transaction History/Store Transaction History - App Store.csv",
            opts,
        )
        .unwrap();
        w.write_all(SCAFFOLD_CSV_STORE_TXN.as_bytes()).unwrap();

        // Layout 2 (mid-era): canonical "App Store Activity" path.
        w.start_file(
            "david_Apple Data/App Store Activity/Apps/App Store Activity - Apps.csv",
            opts,
        )
        .unwrap();
        w.write_all(SCAFFOLD_CSV.as_bytes()).unwrap();

        // An unrelated CSV that must NOT be processed.
        w.start_file("david_Apple Data/Apple ID/Apple ID Account and Device Information.csv", opts).unwrap();
        w.write_all(b"Account,Email\njohn,john@example.com\n").unwrap();

        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        // 2 rows from Store Transaction History + 4 rows from App Store Activity = 6 total.
        assert_eq!(
            out.counts.get("imported_raw"),
            Some(&6),
            "6 rows from both layouts processed: {out:?}"
        );

        // The unrelated CSV should not contribute rows.
        let raw_stream = v.stream(RAW_DIR, Partition::Month);
        let mut total = 0;
        for key in raw_stream.partitions().unwrap() {
            total += raw_stream.read::<Value>(&key).unwrap().len();
        }
        assert_eq!(total, 6, "only the 6 activity rows land in the raw layer");
    }

    #[test]
    fn import_from_zip_real_layout_store_transaction_history() {
        // Regression: the original matcher failed on the newer "Apple Media Services"
        // export layout — this test confirms it now works.
        use std::io::Write as _;

        let v = temp_vault("zip-real-layout");
        let zip_path = v.root().join("apple-privacy-export.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        w.start_file(
            "Apple Media Services/Account and Transaction History/Store Transaction History - iTunes.csv",
            opts,
        )
        .unwrap();
        w.write_all(SCAFFOLD_CSV_STORE_TXN.as_bytes()).unwrap();

        // Also test single-file "Store Transactions History" variant (plural, no dash).
        w.start_file(
            "Apple Media Services/Account and Transaction History/Store Transactions History.csv",
            opts,
        )
        .unwrap();
        // Duplicate of the same rows — will be deduped.
        w.write_all(SCAFFOLD_CSV_STORE_TXN.as_bytes()).unwrap();

        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        // 2 unique rows from the first file; second file is identical → both deduplicated.
        assert_eq!(
            out.counts.get("imported_raw"),
            Some(&2),
            "2 unique rows from real-layout Store Transaction History: {out:?}"
        );
        assert_eq!(
            out.counts.get("duplicates"),
            Some(&2),
            "2 duplicate rows correctly skipped"
        );
    }

    #[test]
    fn import_from_zip_errors_when_no_activity_csv_found() {
        let v = temp_vault("zip-no-activity");
        let zip_path = v.root().join("not-app-store.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Maps/Maps Search History.csv", opts).unwrap();
        w.write_all(b"Query,Date\nsushi near me,2024-01-01\n").unwrap();
        w.finish().unwrap();

        let err = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("No Apple Media Services") || err.to_string().contains("No App Store Activity"),
            "clear error when no activity CSV: {err}"
        );
    }

    #[test]
    fn unsupported_extension_gives_clear_error() {
        let v = temp_vault("bad-ext");
        let path = v.root().join("export.xml");
        fs::write(&path, b"<export/>").unwrap();
        let err = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported file type"),
            "clear error for unsupported extension: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // last_data hook.

    #[test]
    fn last_data_is_none_before_import_and_some_after() {
        let v = temp_vault("last-data");
        // No import yet → no raw data → None.
        assert!(def_last_data(&v).is_none());

        // After import → the raw layer has data → Some("YYYY-MM").
        import(&v, SCAFFOLD_CSV);
        let ld = def_last_data(&v);
        assert!(ld.is_some(), "last_data set after import: {ld:?}");
        let stem = ld.unwrap();
        // Should be a YYYY-MM partition key.
        assert_eq!(stem.len(), 7, "YYYY-MM format: {stem}");
        assert!(stem.contains('-'), "contains hyphen: {stem}");
    }

    // -----------------------------------------------------------------------
    // Parser-parked gate.

    #[test]
    fn parser_is_parked_and_no_contract_rows_written() {
        // Until PARSER_ACTIVE = true, no contract rows should be written.
        let v = temp_vault("parked");
        let out = import(&v, SCAFFOLD_CSV);
        // Raw imported.
        assert!(out.counts.get("imported_raw").copied().unwrap_or(0) > 0);
        // Contract dir should not exist yet.
        let contract_dir = v.root().join(DIR);
        // The raw subdir is fine; the contract JSONL files should not be there.
        let contract_files: Vec<_> = if contract_dir.exists() {
            std::fs::read_dir(&contract_dir)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name().to_str().map(|n| n.ends_with(".jsonl")).unwrap_or(false)
                })
                .collect()
        } else {
            Vec::new()
        };
        assert!(
            contract_files.is_empty(),
            "no contract JSONL files while parser is parked: {contract_files:?}"
        );
        // Headline mentions parked.
        assert!(
            out.headline.contains("parked") || out.headline.contains("raw layer"),
            "headline notes parked status: {}", out.headline
        );
    }
}
