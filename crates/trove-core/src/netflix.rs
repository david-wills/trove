//! Netflix — official viewing-activity CSV export from your account page.
//!
//! **Import** (no API, no network, no credentials): the user downloads the
//! CSV themselves at netflix.com/account → Privacy → Viewing activity →
//! "Download all", then drops it in the import box. One CSV is exported per
//! profile; the import box accepts multiple files and tags each row with the
//! originating filename as the profile name.
//!
//! ## Export format
//!
//! The "quick CSV" has exactly two columns:
//!
//! ```text
//! Title,Date
//! Stranger Things: Season 4: Chapter 1: The Hellfire Club,"01/05/2022"
//! Heat,12/25/2021
//! ```
//!
//! - **Title** — for TV content the full title embeds show name, season, and
//!   episode title separated by `: ` (colon+space). Films are just the title.
//! - **Date** — date-only, no time. Format varies by account locale:
//!   US accounts use `MM/DD/YY`; international accounts may use `DD/MM/YY`
//!   or `YYYY-MM-DD`. Parsed defensively; ambiguous dates (where month and
//!   day could swap) are accepted as-is (MM/DD/YY bias, same as Netflix).
//!
//! ## Vault layout
//!
//! - **Raw layer:** `media/plays/netflix/raw/` — the imported CSV files
//!   copied verbatim (idempotent: re-importing the same file is a no-op).
//! - **Contract layer:** `media/plays/netflix/YYYY-MM.jsonl` — one
//!   [`crate::media::MediaItem`] per row, partitioned by watch-date month.
//!
//! ## Series/episode parsing
//!
//! A title string matching `Show: Season N: Episode …` (two or more colons)
//! is split: `title` = everything after the first `: ` (the episode portion),
//! `subtitle` = the text before the first `: ` (the show name — the grouping
//! key for top charts). Films (zero or one colon) pass through: `title` and
//! `subtitle` are both the whole title string (the film-chart grouping key).
//!
//! ## Dedupe
//!
//! `guid` = SHA-256 hash of `"<profile>\0<title>\0<date>"` (profile is the
//! source filename stem, lowercased). Re-importing an overlapping export is
//! idempotent: any guid already on disk is skipped.
//!
//! `letterboxd.rs` is the reference import module.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const DIR: &str = "media/plays/netflix";
const RAW_DIR: &str = "media/plays/netflix/raw";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "netflix",
        name: "Netflix",
        kind: IntegrationKind::Import,
        default_on: true,
        description: "Import your Netflix viewing history from the official CSV download. \
                      Each profile's history becomes dated play events in the media stream; \
                      re-imports are safe — duplicates are skipped.",
        domain: "media",
        vault_path: "media/plays/netflix/",
        toggleable: false,
        setup: &[
            "netflix.com → Account → Privacy → Viewing activity → \"Download all\".",
            "Each profile generates a separate CSV — import them one at a time (the filename \
             is used as the profile label).",
            "For ongoing capture, connect Trakt and scrobble from your player — Netflix has no \
             live API.",
        ],
        caveats: "The quick CSV contains title and date only; no watch duration is available. \
                  A richer ZIP export (Privacy Settings → \"Request information about your \
                  account\") adds duration and device, but the download link expires after \
                  72 hours — import it promptly.",
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

/// One row of the Netflix viewing-activity CSV.
#[derive(Debug, Deserialize)]
struct ViewingRow {
    #[serde(rename = "Title")]
    title: String,
    #[serde(rename = "Date")]
    date: String,
}

/// Parse the viewing-activity CSV body into rows.
fn parse_csv(body: &str) -> Vec<ViewingRow> {
    let mut rdr = csv::Reader::from_reader(body.as_bytes());
    rdr.deserialize::<ViewingRow>()
        .filter_map(|r| r.ok())
        .collect()
}

/// Parse a date string from the Netflix CSV.
///
/// Netflix accounts in the US export `MM/DD/YY` (two-digit year);
/// international locales may export `DD/MM/YY` or `YYYY-MM-DD`.
/// Strategy: try most-specific first, then fall back gracefully.
fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    // ISO format (unambiguous): YYYY-MM-DD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(d);
    }
    // Slash-separated: US %m/%d/%y (MM/DD/YY) — the dominant Netflix locale.
    // Two-digit year is interpreted as 2000s by chrono (2000–2068).
    if let Ok(d) = NaiveDate::parse_from_str(s, "%m/%d/%y") {
        return Some(d);
    }
    // European DD/MM/YY — try this last as it's ambiguous with the above for
    // days 1–12; we bias toward the US format per Netflix's own export.
    if let Ok(d) = NaiveDate::parse_from_str(s, "%d/%m/%y") {
        return Some(d);
    }
    // Four-digit year slash variants.
    if let Ok(d) = NaiveDate::parse_from_str(s, "%m/%d/%Y") {
        return Some(d);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%d/%m/%Y") {
        return Some(d);
    }
    None
}

/// Derive a stable GUID from (profile, title, canonical-date).
///
/// `date` is the **parsed** `NaiveDate` — hashing the canonical `YYYY-MM-DD`
/// representation ensures re-imports are idempotent regardless of how the date
/// was formatted in the source CSV (e.g. `05/27/22` vs `05/27/2022` vs
/// `2022-05-27` all produce the same guid for the same watch event).
fn make_guid(profile: &str, title: &str, date: NaiveDate) -> String {
    let mut h = Sha256::new();
    h.update(profile.to_lowercase().as_bytes());
    h.update(b"\0");
    h.update(title.as_bytes());
    h.update(b"\0");
    h.update(date.format("%Y-%m-%d").to_string().as_bytes());
    format!("netflix-{:x}", h.finalize())
}

/// Parse a Netflix title string into (title, subtitle).
///
/// Netflix TV titles embed show name (and optionally season) and episode title
/// as colon-separated segments, e.g.:
///   - `"Show: Season N: Episode Name"` (multi-season series)
///   - `"Show: Episode Name"` (limited series / anthology with no season number)
///
/// We split on the FIRST `": "` whenever it is present:
/// - `subtitle` = the text before the first `": "` (the show/franchise name —
///   the grouping key for top charts).
/// - `title` = everything after the first `": "` (the episode or sub-title
///   portion).
///
/// This handles both the two-colon `Show: Season N: Episode` pattern and the
/// single-colon `Show: Episode` pattern used by limited series and anthologies.
/// Films whose titles contain `": "` (e.g. `Spider-Man: No Way Home`) will
/// also split — their subtitle becomes the franchise prefix, which is a
/// reasonable grouping key in the chart context.
///
/// Titles with no `": "` (plain film titles, no colon at all) pass through:
/// `title` and `subtitle` are both the whole string.
fn parse_title(raw: &str) -> (String, String) {
    if let Some(colon_pos) = raw.find(": ") {
        let show = raw[..colon_pos].trim();
        let rest = raw[colon_pos + 2..].trim();
        return (rest.to_string(), show.to_string());
    }
    // No ": " separator — plain title (film or show with no colon).
    (raw.to_string(), raw.to_string())
}

/// Extract the profile name from the import filename (stem, lowercased).
/// Falls back to `"netflix"` if the path has no stem.
fn profile_from_path(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "netflix".into())
}

/// A stable slug for naming the raw archive copy (content hash for idempotent
/// re-drops). Uses FNV-1a 64-bit — not cryptographic, just cheap.
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
    let profile = profile_from_path(path);
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;

    // --- Raw layer (unconditional, full fidelity) -------------------------
    let hash = content_hash(body.as_bytes());
    let raw_rel = format!("{RAW_DIR}/{profile}-{hash}.csv");
    let raw_path = vault.resolve(&raw_rel)?;
    let already_archived = raw_path.exists();
    if !already_archived {
        if let Some(parent) = raw_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating raw dir {}", parent.display()))?;
        }
        std::fs::copy(path, &raw_path)
            .with_context(|| format!("archiving raw CSV to {}", raw_path.display()))?;
    }

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

    let rows = parse_csv(&body);
    let total = rows.len() as u64;
    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut items: Vec<MediaItem> = Vec::new();

    for (i, row) in rows.into_iter().enumerate() {
        if row.title.trim().is_empty() {
            skipped += 1;
            continue;
        }
        let Some(date) = parse_date(&row.date) else {
            skipped += 1;
            continue;
        };
        // Noon local time — the CSV has a date but no time; noon avoids
        // midnight boundary surprises (same convention as letterboxd.rs).
        let Some(ts) = Local
            .from_local_datetime(&date.and_hms_opt(12, 0, 0).unwrap())
            .earliest()
        else {
            skipped += 1;
            continue;
        };
        let guid = make_guid(&profile, row.title.trim(), date);
        if !seen.insert(guid.clone()) {
            duplicates += 1;
            continue;
        }
        let (title, subtitle) = parse_title(row.title.trim());
        let mut extra = Map::new();
        extra.insert("profile".into(), Value::String(profile.clone()));
        items.push(MediaItem {
            ts: ts.to_rfc3339(),
            source: "netflix".into(),
            category: "video".into(),
            device: String::new(),
            kind: "play".into(),
            title,
            subtitle,
            detail: String::new(),
            seconds: 0,
            favicon: String::new(),
            guid,
            extra,
        });
        imported += 1;
        if (i + 1) % 200 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    stream.append(&items, |i| &i.ts)?;
    progress(ImportProgress { records: imported, percent: 100.0 });

    Ok(ImportOutcome {
        headline: format!(
            "{imported} views imported from \"{profile}\", {duplicates} duplicates skipped"
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
        let dir =
            std::env::temp_dir().join(format!("trove-netflix-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    /// Minimal real-shaped Netflix quick-CSV:
    /// - A TV show episode (multi-colon title, US date MM/DD/YY)
    /// - A film (no colon, US date)
    /// - A TV show with a complex multi-colon episode title
    /// - An entry with an ISO date (international locale variant)
    /// - An entry with no parseable date (must be skipped)
    const CSV: &str = "\
Title,Date\r\n\
Stranger Things: Season 4: Chapter 1: The Hellfire Club,05/27/22\r\n\
Heat,12/25/21\r\n\
The Crown: Season 6: Episode 4: Aftermath,11/16/23\r\n\
Oppenheimer,2023-07-21\r\n\
Bad Row,not-a-date\r\n\
";

    fn import(v: &Vault, csv_body: &str, filename: &str) -> ImportOutcome {
        let path = v.root().join(filename);
        fs::write(&path, csv_body).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    #[test]
    fn parses_title_date_and_maps_to_media_contract() {
        let v = temp_vault("basic");
        let out = import(&v, CSV, "my_profile.csv");

        // 4 valid rows, 1 bad-date row skipped.
        assert_eq!(out.counts.get("imported"), Some(&4));
        assert_eq!(out.counts.get("skipped"), Some(&1));
        assert!(out.headline.contains("my_profile"));

        // TV show: title = everything after the first ": " (season+episode),
        // subtitle = show name (the grouping/chart key).
        let day_stranger = v.media_timeline("2022-05-27").unwrap();
        assert_eq!(day_stranger.len(), 1);
        assert_eq!(
            day_stranger[0].title,
            "Season 4: Chapter 1: The Hellfire Club",
            "title = season+episode portion (after first colon-space split)"
        );
        assert_eq!(day_stranger[0].subtitle, "Stranger Things", "show name is the grouping key");
        assert_eq!(day_stranger[0].category, "video");
        assert_eq!(day_stranger[0].kind, "play");
        assert_eq!(day_stranger[0].seconds, 0, "quick CSV has no duration");
        assert_eq!(
            day_stranger[0].extra.get("profile"),
            Some(&serde_json::json!("my_profile")),
            "profile from filename"
        );

        // Film: title and subtitle are both the film title.
        let day_heat = v.media_timeline("2021-12-25").unwrap();
        assert_eq!(day_heat.len(), 1);
        assert_eq!(day_heat[0].title, "Heat");
        assert_eq!(day_heat[0].subtitle, "Heat", "film subtitle = film title (chart key)");

        // ISO date variant (international locale).
        let day_oppen = v.media_timeline("2023-07-21").unwrap();
        assert_eq!(day_oppen.len(), 1);
        assert_eq!(day_oppen[0].title, "Oppenheimer");
    }

    #[test]
    fn reimport_is_idempotent() {
        let v = temp_vault("rerun");
        let first = import(&v, CSV, "profile.csv");
        assert_eq!(first.counts.get("imported"), Some(&4));

        let second = import(&v, CSV, "profile.csv");
        assert_eq!(second.counts.get("imported"), Some(&0));
        assert_eq!(second.counts.get("duplicates"), Some(&4));

        // File on disk unchanged.
        let f1 = fs::read_to_string(v.root().join("media/plays/netflix/2022-05.jsonl")).unwrap();
        assert_eq!(f1.lines().count(), 1, "one row per month partition");
    }

    #[test]
    fn multi_profile_distinguished_by_extra_profile_field() {
        let v = temp_vault("profiles");
        let csv2 = "Title,Date\r\nHeat,12/25/21\r\n";
        import(&v, CSV, "alice.csv");
        import(&v, csv2, "bob.csv");

        // Both imports wrote to the same month file — 2 heat rows (different profiles).
        let day = v.media_timeline("2021-12-25").unwrap();
        assert_eq!(day.len(), 2, "both profiles' Heat rows are present");
        let profiles: Vec<_> = day
            .iter()
            .filter_map(|i| i.extra.get("profile").and_then(|v| v.as_str()))
            .collect();
        assert!(profiles.contains(&"alice"), "alice profile present: {profiles:?}");
        assert!(profiles.contains(&"bob"), "bob profile present: {profiles:?}");
    }

    #[test]
    fn raw_csv_archived_verbatim_and_idempotent() {
        let v = temp_vault("raw");
        import(&v, CSV, "myprofile.csv");

        let raw_dir = v.root().join("media/plays/netflix/raw");
        assert!(raw_dir.exists(), "raw/ directory created");
        let raw_files: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "csv"))
            .collect();
        assert_eq!(raw_files.len(), 1, "one raw file per unique import");

        let archived = fs::read_to_string(raw_files[0].path()).unwrap();
        assert_eq!(archived, CSV, "raw file is verbatim copy");

        // Re-import the same content: no second raw file.
        import(&v, CSV, "myprofile.csv");
        let raw_files2: Vec<_> = fs::read_dir(&raw_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "csv"))
            .collect();
        assert_eq!(raw_files2.len(), 1, "idempotent: no duplicate raw file");
    }

    #[test]
    fn manifest_indexes_netflix_as_media_plays_source() {
        let v = temp_vault("manifest");
        import(&v, CSV, "profile.csv");
        let m = v.rebuild_manifest().unwrap();
        let media = m.domains.iter().find(|d| d.domain == "media-plays").unwrap();
        assert!(media.sources.contains(&"netflix".to_string()));
    }

    #[test]
    fn parse_date_handles_locale_variants() {
        // US MM/DD/YY
        assert_eq!(
            parse_date("05/27/22"),
            Some(NaiveDate::from_ymd_opt(2022, 5, 27).unwrap())
        );
        // ISO
        assert_eq!(
            parse_date("2023-07-21"),
            Some(NaiveDate::from_ymd_opt(2023, 7, 21).unwrap())
        );
        // Four-digit US
        assert_eq!(
            parse_date("12/25/2021"),
            Some(NaiveDate::from_ymd_opt(2021, 12, 25).unwrap())
        );
        // Unparseable
        assert_eq!(parse_date("not-a-date"), None);
        assert_eq!(parse_date(""), None);
    }

    #[test]
    fn parse_title_splits_series_from_episode() {
        // Multi-colon TV series (Show: Season N: Episode) → splits on first ": ".
        let (title, sub) = parse_title("Stranger Things: Season 4: Chapter 1: The Hellfire Club");
        assert_eq!(sub, "Stranger Things");
        assert_eq!(title, "Season 4: Chapter 1: The Hellfire Club");

        // Film (no colon at all) → title == subtitle.
        let (title, sub) = parse_title("Heat");
        assert_eq!(title, "Heat");
        assert_eq!(sub, "Heat");

        // Single-colon limited series / anthology (no season number):
        // Must group under the show, not pass through as a film.
        // Real Netflix examples: "Beef: The Birds Don't Sing...",
        // "Love, Death & Robots: Three Robots", "Wednesday: ...".
        let (title, sub) = parse_title("Beef: The Birds Don't Sing, They Cry Out to the Sky");
        assert_eq!(sub, "Beef", "limited series: subtitle = show name");
        assert_eq!(title, "The Birds Don't Sing, They Cry Out to the Sky");

        let (title, sub) = parse_title("Love, Death & Robots: Three Robots");
        assert_eq!(sub, "Love, Death & Robots", "anthology: subtitle = show name");
        assert_eq!(title, "Three Robots");

        // Multi-colon film title (false-positive risk in old code):
        // e.g. "Borat Subsequent Moviefilm: Delivery...: to American Regime".
        // With split-on-first-colon, subtitle = "Borat Subsequent Moviefilm" —
        // a reasonable franchise grouping key; title = the sub-titled portion.
        let (title, sub) =
            parse_title("Borat Subsequent Moviefilm: Delivery of Prodigious Bribe: to American Regime");
        assert_eq!(sub, "Borat Subsequent Moviefilm");
        assert_eq!(title, "Delivery of Prodigious Bribe: to American Regime");

        // Film with a single colon in the title: splits on first ": ".
        // subtitle becomes the franchise prefix (e.g. "Spider-Man"), which is a
        // reasonable chart grouping key.
        let (title, sub) = parse_title("Spider-Man: No Way Home");
        assert_eq!(sub, "Spider-Man");
        assert_eq!(title, "No Way Home");

        // Four-colon show (Show: Season N: Episode N: Sub-title) → still splits on first.
        let (title, sub) = parse_title("The Crown: Season 6: Episode 4: Aftermath");
        assert_eq!(sub, "The Crown");
        assert_eq!(title, "Season 6: Episode 4: Aftermath");
    }

    #[test]
    fn hub_card_shows_import_box_and_last_data() {
        let v = temp_vault("hub");
        import(&v, CSV, "profile.csv");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "netflix").unwrap();
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["csv"]);
        assert!(card.last_data.is_some(), "last_data populated after import");
    }

    /// The guid uses the canonical YYYY-MM-DD date regardless of how the
    /// source CSV formatted it. Two exports of the same watch event with
    /// different date formats must not create duplicate rows on re-import.
    #[test]
    fn guid_is_stable_across_date_format_variants() {
        // Same watch event expressed with three different date formats.
        let csv_twodigit = "Title,Date\r\nHeat,12/25/21\r\n"; // MM/DD/YY
        let csv_fourdigit = "Title,Date\r\nHeat,12/25/2021\r\n"; // MM/DD/YYYY
        let csv_iso = "Title,Date\r\nHeat,2021-12-25\r\n"; // YYYY-MM-DD

        // Import the two-digit variant first.
        let v = temp_vault("guid_stable");
        import(&v, csv_twodigit, "alice.csv");

        // The four-digit-year variant of the same event must be seen as a
        // duplicate and produce 0 new rows.
        let out4 = import(&v, csv_fourdigit, "alice.csv");
        assert_eq!(
            out4.counts.get("duplicates"),
            Some(&1),
            "MM/DD/YYYY same as MM/DD/YY after date normalisation"
        );

        // The ISO variant must also dedupe.
        let out_iso = import(&v, csv_iso, "alice.csv");
        assert_eq!(
            out_iso.counts.get("duplicates"),
            Some(&1),
            "YYYY-MM-DD same as MM/DD/YY after date normalisation"
        );

        // Only one row on disk.
        let day = v.media_timeline("2021-12-25").unwrap();
        assert_eq!(day.len(), 1, "exactly one Heat row after three format variants");
    }

    /// Single-colon limited-series and anthology entries must group under the
    /// show name (subtitle = show), not pass through as films.
    #[test]
    fn single_colon_limited_series_groups_under_show() {
        // Use a title without commas to avoid CSV quoting ambiguity.
        let csv_simple = "\
Title,Date\r\n\
Beef: The Birds Don't Sing They Cry Out to the Sky,01/10/23\r\n\
";
        let v = temp_vault("limited_series");
        import(&v, csv_simple, "profile.csv");
        let day = v.media_timeline("2023-01-10").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].subtitle, "Beef", "limited series: subtitle = show name");
        assert_eq!(
            day[0].title, "The Birds Don't Sing They Cry Out to the Sky",
            "limited series: title = episode name"
        );
    }
}
