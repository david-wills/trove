//! Amazon Prime Video — watch history import from a privacy-request CSV/ZIP.
//!
//! **Import** (no API, no network, no credentials): the user requests their
//! data at amazon.com → Account → "Request your personal information" → select
//! "PrimeVideo.WatchHistory". Amazon emails a ZIP (typically within minutes to
//! a few hours, up to 30 days). The user drops the ZIP — or the bare
//! `PrimeVideo.WatchHistory.csv` extracted from it — into the import box.
//!
//! ## Export format
//!
//! **Needs-sample — column names and timestamp format derived from secondary
//! sources (Amazon privacy-portal research notes); no real DSAR export has
//! been verified against this parser.** Raw archiving is unconditional; the
//! contract-layer mapping validates expected headers before writing and returns
//! a clear error with the actual headers seen if the shape does not match.
//!
//! The CSV is believed to be `PrimeVideo.WatchHistory.csv` inside the Amazon
//! data ZIP. Expected columns (header-driven; unknown columns are silently
//! discarded at the structured layer but preserved in the raw archive):
//!
//! ```text
//! Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched
//! Succession: Season 1: Episode 1,Fire TV Stick,US,2023-07-21T20:00:00Z,2023-07-21T21:02:37Z,3757
//! The Boys: Season 3: Episode 1,Web Player,GB,2022-06-03T19:30:00Z,2022-06-03T20:42:00Z,4320
//! Heat,Tablet,US,2021-12-25T22:00:00Z,2021-12-26T00:00:00Z,7200
//! ```
//!
//! - **Title** — for TV content the full title typically embeds show name,
//!   season, and episode separated by `: `. Films are plain titles.
//! - **WatchedStartTime** / **WatchedEndTime** — UTC ISO 8601 datetimes
//!   (assumed; not verified against a real export).
//! - **SecondsWatched** — actual viewing duration (integer or float).
//! - **Device** — the device type/name used for playback.
//! - **Country** — ISO country code of the viewer's region.
//! - Unknown/region-variant columns are discarded from the structured layer;
//!   the raw archive preserves full fidelity.
//! - Once a real export is obtained, update this doc and the fixture, then
//!   remove the Needs-sample caveat.
//!
//! ## Vault layout
//!
//! - **Raw layer:** `media/plays/prime-video/raw/` — the imported ZIP or CSV
//!   archived verbatim; idempotent via content hash (re-importing the same
//!   file is a no-op at the raw layer).
//! - **Contract layer:** `media/plays/prime-video/YYYY-MM.jsonl` — one
//!   [`crate::media::MediaItem`] per row, partitioned by the local month of
//!   `ts` (= WatchedStartTime).
//!
//! ## Series/episode parsing
//!
//! Same split-on-first-colon-space heuristic as the Netflix importer:
//! - `subtitle` = text before the first `": "` (show name — chart grouping key).
//! - `title` = everything after (season+episode for TV; the full string for
//!   plain film titles that contain no `": "`).
//!
//! ## Dedupe
//!
//! `guid` = SHA-256 hash of `"prime-video\0<title>\0<epoch_secs>"` where
//! `epoch_secs` is the UTC Unix timestamp of WatchedStartTime as an integer.
//! Using the canonical integer (not the raw ISO string) means all formatting
//! variants of the same instant (Z, +00:00, .000Z) produce the same guid.
//! Re-importing overlapping exports is idempotent: any guid already on disk
//! is skipped.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset, Local};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "media/plays/prime-video";
const RAW_DIR: &str = "media/plays/prime-video/raw";

/// CSV filename inside the Amazon data export ZIP.
const ZIP_CSV_NAME: &str = "PrimeVideo.WatchHistory.csv";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "prime-video",
        name: "Prime Video",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your Amazon Prime Video watch history from the official privacy \
                      export. Every title you watched — with real viewing duration — lands in the \
                      unified media stream. Re-runnable: duplicates are skipped. For ongoing \
                      capture, use Trakt to scrobble directly from Prime Video.",
        domain: "media",
        vault_path: "media/plays/prime-video/",
        toggleable: false,
        setup: &[
            "Go to amazon.com → Account & Lists → Your Account → scroll to \
             \"Request your personal information\".",
            "Select \"Prime Video Watch History\" and click Request. Amazon will email a \
             download link — turnaround is typically minutes to hours but can take up to 30 days.",
            "Download the ZIP from the email link (it expires — download promptly), \
             then drop it into the import box here. You can also extract \
             PrimeVideo.WatchHistory.csv from the ZIP and import that directly.",
        ],
        caveats: "The download link in Amazon's email expires shortly after delivery — download \
                  it before importing. For ongoing capture without repeated data requests, \
                  connecting Trakt and scrobbling is the recommended complement.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "csv"],
    params: &[],
    run: run_import,
};

/// Column names expected in the `PrimeVideo.WatchHistory.csv`.
/// Used for header validation before committing the contract layer.
/// NOTE (Needs-sample): these names come from secondary research notes, not a
/// verified real export.  If Amazon's actual export uses different names the
/// parser returns a descriptive error naming the headers it saw instead of
/// silently writing 0 rows.
const REQUIRED_COLS: &[&str] = &["Title", "WatchedStartTime"];

/// One row of the `PrimeVideo.WatchHistory.csv`.
/// Header-driven: the csv crate maps by column name.
/// Unknown/region-variant columns are silently discarded by the csv+serde
/// stack (serde ignores unknown fields by default); they are NOT routed to
/// `extra` at the structured layer (the raw archive preserves them).
#[derive(Debug, Deserialize)]
struct WatchRow {
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Device", default)]
    device: String,
    #[serde(rename = "Country", default)]
    country: String,
    #[serde(rename = "WatchedStartTime")]
    start_time: String,
    #[serde(rename = "WatchedEndTime", default)]
    end_time: String,
    #[serde(rename = "SecondsWatched", default)]
    seconds_watched: String,
}

/// Extract the CSV body from a path — either a bare .csv or the
/// `PrimeVideo.WatchHistory.csv` entry from a .zip archive.
fn watch_history_csv(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive =
            zip::ZipArchive::new(file).with_context(|| format!("reading {}", path.display()))?;

        // Try the canonical name first; if not found, scan for any root-level .csv.
        // We check for the canonical name by scanning the index (avoids borrow
        // conflicts between by_name and the fallback loop).
        let canonical_idx = (0..archive.len()).find(|&i| {
            archive.by_index_raw(i).is_ok_and(|e| e.name() == ZIP_CSV_NAME)
        });
        let target_idx = if let Some(idx) = canonical_idx {
            Some(idx)
        } else {
            // Fall back: first .csv at the archive root (no path separator).
            (0..archive.len()).find(|&i| {
                archive.by_index_raw(i).is_ok_and(|e| {
                    let n = e.name().to_lowercase();
                    !n.contains('/') && n.ends_with(".csv")
                })
            })
        };
        let idx = target_idx.context(
            "no PrimeVideo.WatchHistory.csv (or any .csv) found in the ZIP — \
             is this an Amazon data export?",
        )?;
        let mut entry = archive
            .by_index(idx)
            .context("reading CSV entry from ZIP")?;
        let mut csv_body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut csv_body)
            .context("reading CSV from ZIP")?;
        Ok(csv_body)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("opening {}", path.display()))
    }
}

/// Derive a stable GUID from (title, canonical start instant).
///
/// `start_utc_secs` is the Unix timestamp (seconds since epoch) of
/// WatchedStartTime after parsing.  Hashing a canonical numeric epoch value
/// means re-imports are idempotent regardless of ISO 8601 formatting variants
/// in the source CSV (e.g. `2023-07-21T20:00:00Z`, `2023-07-21T20:00:00+00:00`,
/// and `2023-07-21T20:00:00.000Z` all map to the same integer and therefore the
/// same guid).  Mirrors the Netflix importer's canonical-date guid strategy.
fn make_guid(title: &str, start_utc_secs: i64) -> String {
    let mut h = Sha256::new();
    h.update(b"prime-video\0");
    h.update(title.as_bytes());
    h.update(b"\0");
    h.update(start_utc_secs.to_string().as_bytes());
    format!("pv-{:x}", h.finalize())
}

/// Parse an ISO 8601 UTC timestamp from the Amazon CSV into an RFC3339 string
/// in the local timezone, suitable for `ts` in the contract.
///
/// Amazon exports `WatchedStartTime` as UTC ISO 8601 (e.g. `2023-07-21T20:00:00Z`).
/// We convert to local RFC3339 so vault timestamps use the same zone convention
/// as every other Trove collector.
fn parse_start_time(s: &str) -> Option<(String, DateTime<FixedOffset>)> {
    let s = s.trim();
    // Try RFC 3339 / ISO 8601 with Z suffix or +00:00 offset.
    let dt = DateTime::parse_from_rfc3339(s)
        .or_else(|_| {
            // Some regions may omit the Z; try appending it.
            DateTime::parse_from_rfc3339(&format!("{s}Z"))
        })
        .ok()?;
    // Convert to the machine's local timezone for the contract ts.
    let local_dt = dt.with_timezone(&Local);
    Some((local_dt.to_rfc3339(), dt))
}

/// Parse series title string into (title, subtitle).
///
/// Amazon Prime Video TV titles embed show name, season, and episode as
/// colon-separated segments (e.g. `"Succession: Season 1: Episode 1"`).
/// Same first-colon-space split as the Netflix importer:
/// - subtitle = text before first `": "` (the show name — chart grouping key)
/// - title = everything after (the season+episode portion for TV)
/// Films with no `": "` pass through: title == subtitle == the whole string.
fn parse_title(raw: &str) -> (String, String) {
    if let Some(pos) = raw.find(": ") {
        let show = raw[..pos].trim();
        let rest = raw[pos + 2..].trim();
        (rest.to_string(), show.to_string())
    } else {
        (raw.to_string(), raw.to_string())
    }
}

/// Cheap content hash for raw-layer idempotence (not cryptographic).
fn content_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // --- Raw layer (unconditional, full fidelity) -------------------------
    let raw_bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let hash = content_hash(&raw_bytes);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("csv");
    let raw_rel = format!("{RAW_DIR}/prime-video-{hash}.{ext}");
    let raw_path = vault.resolve(&raw_rel)?;
    if !raw_path.exists() {
        if let Some(parent) = raw_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating raw dir {}", parent.display()))?;
        }
        std::fs::copy(path, &raw_path)
            .with_context(|| format!("archiving raw file to {}", raw_path.display()))?;
    }

    // --- Extract CSV body from file or ZIP -------------------------------
    let body = watch_history_csv(path)?;

    // --- Contract layer ---------------------------------------------------
    let stream = vault.stream(DIR, Partition::Month);
    // Load guids already on disk for re-runnable dedupe.
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for it in stream.read::<MediaItem>(&key)? {
            if !it.guid.is_empty() {
                seen.insert(it.guid);
            }
        }
    }

    let mut rdr = csv::Reader::from_reader(body.as_bytes());

    // --- Header validation (Needs-sample guard) ---------------------------
    // Validate that the required columns are present before processing any
    // rows.  If the real Amazon export uses different column names than the
    // ones documented in our research notes, this catches the mismatch early
    // and returns a descriptive error naming the actual headers — rather than
    // silently writing 0 rows.
    {
        let headers = rdr.headers().context("reading CSV headers")?;
        let header_strs: Vec<&str> = headers.iter().collect();
        let missing: Vec<&str> = REQUIRED_COLS
            .iter()
            .copied()
            .filter(|col| !header_strs.contains(col))
            .collect();
        if !missing.is_empty() {
            return Err(anyhow::anyhow!(
                "Amazon Prime Video CSV has unexpected column names — the export shape may \
                 differ from the documented format (Needs-sample).\n\
                 Expected columns (missing): {missing:?}\n\
                 Columns actually present:   {header_strs:?}\n\
                 Please open a Trove issue with a screenshot of your export's header row \
                 so the parser can be updated."
            ));
        }
    }

    let (mut imported, mut duplicates, mut skipped, mut total) = (0u64, 0u64, 0u64, 0u64);
    let mut items: Vec<MediaItem> = Vec::new();

    for result in rdr.deserialize::<WatchRow>() {
        total += 1;
        let row = match result {
            Ok(r) => r,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };

        if row.title.trim().is_empty() || row.start_time.trim().is_empty() {
            skipped += 1;
            continue;
        }

        let Some((ts, dt)) = parse_start_time(&row.start_time) else {
            skipped += 1;
            continue;
        };

        // Build guid from the canonical UTC epoch seconds so that formatting
        // variants of the same timestamp (Z suffix, +00:00, milliseconds, etc.)
        // all produce the same guid — preventing duplicate rows across exports.
        let guid = make_guid(row.title.trim(), dt.timestamp());
        if !seen.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }

        let (title, subtitle) = parse_title(row.title.trim());

        // Parse SecondsWatched defensively: treat integer or float strings
        // (e.g. "3757" or "3757.0") as equivalent; unknown formats become 0.
        let seconds: u64 = {
            let s = row.seconds_watched.trim();
            if let Ok(n) = s.parse::<u64>() {
                n
            } else if let Ok(f) = s.parse::<f64>() {
                f.round() as u64
            } else {
                0
            }
        };

        let mut extra = Map::new();
        if !row.country.trim().is_empty() {
            extra.insert("country".into(), Value::String(row.country.trim().into()));
        }
        if !row.end_time.trim().is_empty() {
            extra.insert(
                "watched_end_time".into(),
                Value::String(row.end_time.trim().into()),
            );
        }
        // Store the raw title for reference (before the series split).
        let raw_title = row.title.trim();
        if raw_title != title {
            extra.insert("raw_title".into(), Value::String(raw_title.into()));
        }

        items.push(MediaItem {
            ts,
            source: "prime-video".into(),
            category: "video".into(),
            device: row.device.trim().to_string(),
            kind: "play".into(),
            title,
            subtitle,
            detail: String::new(),
            seconds,
            favicon: String::new(),
            guid,
            extra,
        });
        imported += 1;
        if total % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // --- Silent-empty-write guard -----------------------------------------
    // If every data row failed to deserialize (total>0, imported==0, no
    // duplicates, skipped==total) there is likely a structural mismatch (e.g.
    // the timestamp field name differs from expected).  Return an error rather
    // than silently writing 0 rows and claiming success.
    if total > 0 && imported == 0 && duplicates == 0 && skipped == total {
        return Err(anyhow::anyhow!(
            "0 of {total} rows imported from the Prime Video CSV — every row failed to parse.\n\
             This usually means the timestamp or title column name differs from the expected \
             format (Needs-sample: export shape is not yet verified against a real Amazon \
             DSAR export).\n\
             Re-import after confirming the column names in your file match: {REQUIRED_COLS:?}"
        ));
    }

    stream.append(&items, |i| &i.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} Prime Video plays imported, {duplicates} duplicates skipped"
        ),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
            ("total_rows", total),
        ]
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-prime-video-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal real-shaped Prime Video CSV with the exact documented columns:
    /// Title, Device, Country, WatchedStartTime, WatchedEndTime, SecondsWatched
    ///
    /// Contains:
    /// - A TV show episode (multi-colon title, full watch)
    /// - A film (no colon, substantial watch)
    /// - A row with 0 seconds (still imported — `seconds` = 0 is honest)
    /// - A row missing start time (must be skipped)
    const CSV: &str = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched\r\n\
Succession: Season 1: Episode 1: Celebration,Fire TV Stick,US,2023-07-21T20:00:00Z,2023-07-21T21:02:37Z,3757\r\n\
The Boys: Season 3: Episode 1: Payback,Web Player,GB,2022-06-03T19:30:00Z,2022-06-03T20:42:00Z,4320\r\n\
Heat,Tablet,US,2021-12-25T22:00:00Z,2021-12-26T00:00:00Z,7200\r\n\
Short Clip,Mobile,US,2024-01-15T10:00:00Z,2024-01-15T10:00:30Z,0\r\n\
Bad Row,Device,US,,2024-01-15T10:00:30Z,100\r\n\
";

    fn import_csv(v: &Vault, csv_body: &str, filename: &str) -> ImportOutcome {
        let path = v.root().join(filename);
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn parses_columns_and_maps_to_media_contract() {
        let v = temp_vault("basic");
        let out = import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");

        // 4 valid rows (including the 0-seconds one), 1 skipped (empty start time).
        assert_eq!(out.counts.get("imported"), Some(&4));
        assert_eq!(out.counts.get("skipped"), Some(&1));
        assert!(out.headline.contains("Prime Video"));

        // TV show: multi-colon title splits on first ": " — subtitle = show name.
        let local_day_succession = {
            let dt = DateTime::parse_from_rfc3339("2023-07-21T20:00:00Z").unwrap();
            dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
        };
        let day = v.media_timeline(&local_day_succession).unwrap();
        assert_eq!(day.len(), 1, "Succession row on its local date");
        assert_eq!(
            day[0].subtitle, "Succession",
            "subtitle = show name (chart grouping key)"
        );
        assert_eq!(
            day[0].title, "Season 1: Episode 1: Celebration",
            "title = season+episode portion after first colon-space split"
        );
        assert_eq!(day[0].category, "video");
        assert_eq!(day[0].kind, "play");
        assert_eq!(day[0].seconds, 3757, "SecondsWatched preserved as seconds");
        assert_eq!(day[0].device, "Fire TV Stick", "device preserved");
        assert_eq!(
            day[0].extra.get("country"),
            Some(&serde_json::json!("US")),
            "country in extra"
        );
        assert!(
            day[0].extra.contains_key("watched_end_time"),
            "WatchedEndTime in extra"
        );
        assert!(
            day[0].extra.contains_key("raw_title"),
            "raw_title in extra for split titles"
        );

        // Film: no colon → title == subtitle == full film name.
        let local_day_heat = {
            let dt = DateTime::parse_from_rfc3339("2021-12-25T22:00:00Z").unwrap();
            dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
        };
        let day_heat = v.media_timeline(&local_day_heat).unwrap();
        assert_eq!(day_heat.len(), 1, "Heat row on its local date");
        assert_eq!(day_heat[0].title, "Heat");
        assert_eq!(day_heat[0].subtitle, "Heat", "film: subtitle = film name");
        assert_eq!(day_heat[0].seconds, 7200);
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("rerun");
        let first = import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");
        let imported = *first.counts.get("imported").unwrap();
        assert!(imported > 0);

        let second = import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");
        assert_eq!(second.counts.get("imported"), Some(&0));
        assert_eq!(second.counts.get("duplicates"), Some(&imported));
    }

    #[test]
    fn zero_seconds_imported_as_play_with_seconds_zero() {
        let v = temp_vault("zero_sec");
        import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");

        // The "Short Clip" row has SecondsWatched=0 — still imported.
        let local_day_clip = {
            let dt = DateTime::parse_from_rfc3339("2024-01-15T10:00:00Z").unwrap();
            dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
        };
        let day = v.media_timeline(&local_day_clip).unwrap();
        let clip = day.iter().find(|r| r.title == "Short Clip").unwrap();
        assert_eq!(clip.seconds, 0, "0 SecondsWatched → seconds = 0 (honest unknown)");
        assert_eq!(clip.kind, "play");
    }

    #[test]
    fn raw_file_archived_verbatim_and_idempotent() {
        let v = temp_vault("raw");
        import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");

        let raw_dir = v.root().join("media/plays/prime-video/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path().extension().is_some_and(|x| x == "csv")
            })
            .collect();
        assert_eq!(raw_files.len(), 1, "one raw file per unique import");

        let archived = fs::read_to_string(raw_files[0].path()).unwrap();
        assert_eq!(archived, CSV, "raw file is verbatim copy");

        // Re-import the same content: no second raw file.
        import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");
        let raw_files2: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path().extension().is_some_and(|x| x == "csv")
            })
            .collect();
        assert_eq!(raw_files2.len(), 1, "idempotent: no duplicate raw file");
    }

    #[test]
    fn zip_import_extracts_csv_by_canonical_name() {
        use std::io::Write;

        let v = temp_vault("zip");
        // Build a ZIP in memory containing PrimeVideo.WatchHistory.csv.
        let zip_bytes = {
            let buf = Vec::new();
            let w = std::io::Cursor::new(buf);
            let mut archive = zip::ZipWriter::new(w);
            let opts =
                zip::write::FileOptions::<()>::default().compression_method(zip::CompressionMethod::Stored);
            archive.start_file(ZIP_CSV_NAME, opts).unwrap();
            archive.write_all(CSV.as_bytes()).unwrap();
            archive.finish().unwrap().into_inner()
        };

        let zip_path = v.root().join("amazon-data.zip");
        fs::write(&zip_path, &zip_bytes).unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&4));

        // Raw layer: the ZIP itself is archived.
        let raw_dir = v.root().join("media/plays/prime-video/raw");
        let zips: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "zip"))
            .collect();
        assert_eq!(zips.len(), 1, "ZIP archived verbatim in raw/");
    }

    #[test]
    fn parse_title_splits_series_from_episode() {
        // Multi-colon TV series (Show: Season N: Episode …) → first colon-space split.
        let (title, sub) = parse_title("Succession: Season 1: Episode 1: Celebration");
        assert_eq!(sub, "Succession");
        assert_eq!(title, "Season 1: Episode 1: Celebration");

        // Single-colon show (limited series / anthology).
        let (title, sub) = parse_title("The Boys: Season 3: Episode 1: Payback");
        assert_eq!(sub, "The Boys");
        assert_eq!(title, "Season 3: Episode 1: Payback");

        // Film — no colon → title == subtitle.
        let (title, sub) = parse_title("Heat");
        assert_eq!(title, "Heat");
        assert_eq!(sub, "Heat");

        // Film with a colon in the title (e.g. "Spider-Man: No Way Home").
        let (title, sub) = parse_title("Spider-Man: No Way Home");
        assert_eq!(sub, "Spider-Man");
        assert_eq!(title, "No Way Home");
    }

    #[test]
    fn parse_start_time_handles_utc_variants() {
        // Standard Z suffix.
        let result = parse_start_time("2023-07-21T20:00:00Z");
        assert!(result.is_some(), "Z suffix parses");

        // +00:00 offset.
        let result2 = parse_start_time("2023-07-21T20:00:00+00:00");
        assert!(result2.is_some(), "+00:00 offset parses");

        // RFC 3339 with milliseconds.
        let result3 = parse_start_time("2023-07-21T20:00:00.000Z");
        assert!(result3.is_some(), "milliseconds parse");

        // Unparseable.
        assert!(parse_start_time("not-a-date").is_none());
        assert!(parse_start_time("").is_none());
    }

    #[test]
    fn guid_stable_on_reimport() {
        // Same title + canonical epoch seconds must always produce the same guid.
        // Epoch for 2021-12-25T22:00:00Z.
        let epoch = DateTime::parse_from_rfc3339("2021-12-25T22:00:00Z")
            .unwrap()
            .timestamp();
        let g1 = make_guid("Heat", epoch);
        let g2 = make_guid("Heat", epoch);
        assert_eq!(g1, g2);

        // Different start time → different guid.
        let epoch2 = DateTime::parse_from_rfc3339("2021-12-25T23:00:00Z")
            .unwrap()
            .timestamp();
        let g3 = make_guid("Heat", epoch2);
        assert_ne!(g1, g3);
    }

    /// The guid must be stable across ISO 8601 timestamp formatting variants.
    /// Three representations of the same instant must produce the same guid —
    /// preventing duplicate rows when re-importing overlapping exports that
    /// differ only in timestamp formatting.
    #[test]
    fn guid_is_stable_across_timestamp_format_variants() {
        // Three representations of the same instant.
        let csv_z = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched\r\n\
Heat,Tablet,US,2021-12-25T22:00:00Z,2021-12-26T00:00:00Z,7200\r\n\
";
        let csv_offset = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched\r\n\
Heat,Tablet,US,2021-12-25T22:00:00+00:00,2021-12-26T00:00:00Z,7200\r\n\
";
        let csv_millis = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched\r\n\
Heat,Tablet,US,2021-12-25T22:00:00.000Z,2021-12-26T00:00:00Z,7200\r\n\
";

        let v = temp_vault("guid_ts_variants");
        import_csv(&v, csv_z, "watch_z.csv");

        // +00:00 variant of the same event must be a duplicate.
        let out_offset = import_csv(&v, csv_offset, "watch_offset.csv");
        assert_eq!(
            out_offset.counts.get("duplicates"),
            Some(&1),
            "+00:00 timestamp same instant as Z suffix — should be a duplicate"
        );

        // Milliseconds variant must also be a duplicate.
        let out_millis = import_csv(&v, csv_millis, "watch_millis.csv");
        assert_eq!(
            out_millis.counts.get("duplicates"),
            Some(&1),
            ".000Z timestamp same instant as Z suffix — should be a duplicate"
        );

        // Only one row on disk.
        let local_day = {
            let dt = DateTime::parse_from_rfc3339("2021-12-25T22:00:00Z").unwrap();
            dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
        };
        let day = v.media_timeline(&local_day).unwrap();
        assert_eq!(day.len(), 1, "exactly one Heat row after three timestamp variants");
    }

    #[test]
    fn extra_columns_do_not_break_parsing() {
        // A region-variant CSV with an extra unknown column should not fail.
        // NOTE: unknown columns are silently discarded at the structured layer
        // (serde ignores unknown fields by default); full fidelity is preserved
        // in the raw archive only.
        let csv_extra_col = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched,Region\r\n\
Heat,Tablet,DE,2021-12-25T22:00:00Z,2021-12-26T00:00:00Z,7200,EU\r\n\
";
        let v = temp_vault("extra_col");
        let out = import_csv(&v, csv_extra_col, "watch.csv");
        assert_eq!(out.counts.get("imported"), Some(&1));
        // The "Region" extra column is not present in structured items (dropped
        // by serde), but the raw archive preserves it.
        let local_day = {
            let dt = DateTime::parse_from_rfc3339("2021-12-25T22:00:00Z").unwrap();
            dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
        };
        let day = v.media_timeline(&local_day).unwrap();
        assert_eq!(day.len(), 1);
        assert!(
            !day[0].extra.contains_key("region"),
            "extra column 'Region' is dropped from the structured layer (raw archive preserves it)"
        );
    }

    #[test]
    fn wrong_column_names_return_error_not_silent_zero() {
        // If the real Amazon export uses different column names, the import
        // must return an error naming the actual headers — not silent success
        // with 0 rows written.
        let csv_wrong_cols = "\
VideoTitle,DeviceType,Country,ViewingDate,EndDate,Duration\r\n\
Heat,Tablet,US,2021-12-25T22:00:00Z,2021-12-26T00:00:00Z,7200\r\n\
";
        let v = temp_vault("wrong_cols");
        let path = v.root().join("wrong.csv");
        std::fs::write(&path, csv_wrong_cols).unwrap();
        let result = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(
            result.is_err(),
            "wrong column names must return Err, not silent success"
        );
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("Title") || err_msg.contains("WatchedStartTime"),
            "error message must name the missing expected columns: {err_msg}"
        );
    }

    #[test]
    fn all_rows_fail_parse_returns_error_not_silent_zero() {
        // All rows have an empty start time → every row skipped → must return Err.
        let csv_all_bad = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched\r\n\
Heat,Tablet,US,,2021-12-26T00:00:00Z,7200\r\n\
The Boys,Phone,US,,2022-01-01T00:00:00Z,100\r\n\
";
        let v = temp_vault("all_bad");
        let path = v.root().join("all_bad.csv");
        std::fs::write(&path, csv_all_bad).unwrap();
        let result = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {});
        assert!(
            result.is_err(),
            "all rows skipped must return Err, not silent success with 0 imported"
        );
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("0 of 2"),
            "error message must state row counts: {err_msg}"
        );
    }

    #[test]
    fn seconds_watched_float_parsed_correctly() {
        // Real export may emit "3757.0" instead of "3757"; must not become 0.
        let csv_float_sec = "\
Title,Device,Country,WatchedStartTime,WatchedEndTime,SecondsWatched\r\n\
Heat,Tablet,US,2021-12-25T22:00:00Z,2021-12-26T00:00:00Z,7200.0\r\n\
";
        let v = temp_vault("float_sec");
        import_csv(&v, csv_float_sec, "watch.csv");
        let local_day = {
            let dt = DateTime::parse_from_rfc3339("2021-12-25T22:00:00Z").unwrap();
            dt.with_timezone(&Local).format("%Y-%m-%d").to_string()
        };
        let day = v.media_timeline(&local_day).unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].seconds, 7200, "float SecondsWatched rounded to integer seconds");
    }

    #[test]
    fn hub_card_shows_import_box_and_last_data() {
        let v = temp_vault("hub");
        import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "prime-video").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert!(import_info.accepts.contains(&"csv"));
        assert!(import_info.accepts.contains(&"zip"));
        assert!(card.last_data.is_some(), "last_data populated after import");
    }

    #[test]
    fn manifest_indexes_prime_video_as_media_plays_source() {
        let v = temp_vault("manifest");
        import_csv(&v, CSV, "PrimeVideo.WatchHistory.csv");
        let m = v.rebuild_manifest().unwrap();
        let media = m.domains.iter().find(|d| d.domain == "media-plays").unwrap();
        assert!(media.sources.contains(&"prime-video".to_string()));
    }
}
