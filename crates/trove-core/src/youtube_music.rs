//! YouTube Music — listen history via Google Takeout import.
//!
//! Google's music-streaming service has **no live API** for listening history;
//! the only path is a Google Takeout archive
//! (takeout.google.com → "YouTube and YouTube Music", History format: JSON).
//! The archive contains `history/watch-history.json`: a JSON array that mixes
//! plain YouTube video watches (`"header": "YouTube"`) with YouTube Music
//! listens (`"header": "YouTube Music"`).  This module reads that file and
//! **filters to `header == "YouTube Music"` records only**, mirroring the
//! reference parsers (YTMtoMaloja `select(.header=="YouTube Music")`, beebls).
//!
//! **Field names confirmed against multiple external sources (YTMtoMaloja,
//! beebls, purarue/google_takeout_parser) and the google_takeout.rs fixture
//! which already uses the same subtitles[] shape:**
//!
//! - `header`           — record type discriminator; only `"YouTube Music"`
//!                        records are ingested; all others are skipped.
//! - `title`            — listen sentence, e.g. "Watched <song>" in English
//!                        exports; the action-verb prefix is stripped to yield
//!                        the bare song name (locale note: the prefix is English
//!                        only — non-English exports keep the localized prefix
//!                        intact rather than mangling the title).
//! - `subtitles[0].name` — artist channel name; **required** (records without
//!                          `subtitles` lack an artist and are dropped, matching
//!                          `select(has("subtitles"))` in the reference parser).
//!                          Auto-generated topic channels carry a " - Topic"
//!                          suffix that is stripped before storing.
//! - `time`             — ISO 8601 UTC timestamp (same format as watch-history)
//! - `titleUrl`         — YouTube Music URL (carried in `extra`)
//!
//! **No album, no duration, no ms_played** — the export is deliberately sparse.
//!
//! Two vault layers:
//! - **Raw** (`media/plays/youtube-music/raw/YYYY-MM.jsonl`) — verbatim
//!   JSON object, full fidelity, unconditional.
//! - **Contract** (`media/plays/youtube-music/YYYY-MM.jsonl`) — one
//!   `MediaItem` per listen, deduped by `guid` = `ym-{raw_time}-{title_slug}`
//!   so re-importing overlapping archives is idempotent.
//!
//! The import accepts the full Takeout zip (reads
//! `…/history/watch-history.json` from inside and filters to YTM records),
//! or a bare `watch-history.json` / pre-filtered JSON file.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use serde_json::{Map, Value};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::media::MediaItem;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

const SOURCE: &str = "youtube-music";
const DIR: &str = "media/plays/youtube-music";
const RAW_DIR: &str = "media/plays/youtube-music/raw";
/// The path inside the Takeout zip; matches both "history/" and locale-nested paths.
/// YouTube Music listens are a filtered subset of this file (header == "YouTube Music").
const WATCH_HISTORY_SUFFIX: &str = "history/watch-history.json";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "youtube-music",
        name: "YouTube Music",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import your YouTube Music listen history from a Google Takeout archive. \
                      Your listens live inside watch-history.json alongside YouTube video \
                      watches — zero extra download cost. Re-runnable: newer exports never \
                      duplicate.",
        domain: "media",
        vault_path: "media/plays/youtube-music/",
        toggleable: false,
        setup: &[
            "Go to takeout.google.com and deselect everything, then select \
             \"YouTube and YouTube Music\" only.",
            "Click \"Multiple formats\", find History, and switch the format \
             from HTML to JSON.",
            "Export and download the archive, then drop it here as-is \
             (or the bare watch-history.json from inside it).",
        ],
        caveats: "Takeout history has sparse fields — no album or duration. \
                  There is no live API for YouTube Music listen history; \
                  re-export periodically to stay current. \
                  For best results, use English as your Google account language \
                  when exporting — the song-name prefix (\"Watched \") is \
                  recognized in English only; non-English exports keep the \
                  localized prefix in the title field. \
                  For ongoing automatic capture, point Web Scrobbler \
                  at Last.fm while you listen — that feed is picked up \
                  by the Last.fm integration.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "json"],
    params: &[],
    run: run_import,
};

/// Extract `watch-history.json` from a Takeout zip, or read a bare `.json`
/// file. YouTube Music listens are embedded in this file alongside plain
/// YouTube video watches; callers must filter by `header == "YouTube Music"`.
/// The file lives at a path ending in `history/watch-history.json`
/// (case-insensitive) inside the archive.
fn watch_history_json(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", path.display()))?;
        let mut name: Option<String> = None;
        for i in 0..archive.len() {
            let entry = archive.by_index(i)?;
            let n = entry.name().replace('\\', "/");
            if n.to_ascii_lowercase().ends_with(WATCH_HISTORY_SUFFIX) {
                name = Some(n);
                break;
            }
        }
        let name = name.context(
            "no history/watch-history.json in the archive — \
             did you select \"YouTube and YouTube Music\" with History format set to JSON \
             (not HTML) at takeout.google.com?",
        )?;
        let mut entry = archive
            .by_name(&name)
            .with_context(|| format!("opening {name} in zip"))?;
        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .with_context(|| format!("reading {name}"))?;
        Ok(body)
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("opening {}", path.display()))
    }
}

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let stream = vault.stream(DIR, Partition::Month);
    let raw = vault.stream(RAW_DIR, Partition::Month);

    // Collect already-stored guids for idempotent re-import (the same rule as
    // google-takeout, letterboxd, etc. — overlapping archives never duplicate).
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for item in stream.read::<MediaItem>(&key)? {
            if !item.guid.is_empty() {
                seen.insert(item.guid);
            }
        }
    }

    let body = watch_history_json(path)?;
    let records: Vec<Value> = serde_json::from_str(&body).context(
        "watch-history.json is not a JSON array — \
         make sure you selected JSON (not HTML) format in Takeout",
    )?;

    let (mut imported, mut duplicates, mut skipped, mut rows) = (0u64, 0u64, 0u64, 0u64);
    let mut items: Vec<MediaItem> = Vec::new();
    let mut raws: Vec<RawLine> = Vec::new();

    for record in &records {
        rows += 1;
        let Some(item) = listen_from(record) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(item.guid.clone()) {
            duplicates += 1;
            continue;
        }
        raws.push(RawLine { ts: item.ts.clone(), value: record.clone() });
        items.push(item);
        imported += 1;
        if rows % 500 == 0 {
            progress(ImportProgress { records: imported, percent: 0.0 });
        }
    }

    // Write both layers; both partitioned by local month of `ts`.
    stream.append(&items, |i| &i.ts)?;
    raw.append(&raws, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!("{imported} listens imported, {duplicates} duplicates skipped"),
        counts: [
            ("imported", imported),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

/// Verbatim record for the raw layer, tagged with the contract `ts` only so
/// the month-partition writer files it under the right month — only `value`
/// is serialized.
#[derive(serde::Serialize)]
struct RawLine {
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

/// One `watch-history.json` record → a contract [`MediaItem`], or `None` when:
/// - `header` ≠ `"YouTube Music"` (plain YouTube video watch or ad — skip),
/// - `subtitles` is absent or empty (no artist — skip, matching reference
///   parser `select(has("subtitles"))`),
/// - no parseable timestamp or no title.
fn listen_from(record: &Value) -> Option<MediaItem> {
    // header — discriminator: only "YouTube Music" records belong here.
    // Plain YouTube video watches carry header == "YouTube"; ads/others differ.
    // Mirroring YTMtoMaloja's `select(.header=="YouTube Music")`.
    let header = str_field(record, "header");
    if !header.eq_ignore_ascii_case("YouTube Music") {
        return None;
    }

    // subtitles[0].name — artist channel name; required.
    //
    // Records without a subtitles array have no usable artist and no reliable
    // music classification — drop them (mirrors reference parser
    // `select(has("subtitles"))`).  Auto-generated "Topic" channels on YouTube
    // Music carry a " - Topic" suffix (e.g. "Tame Impala - Topic") that is not
    // part of the artist name and corrupts top-charts grouping; strip it.
    let artist_raw = record
        .get("subtitles")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("name"))
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())?; // None (absent/empty) → skip record
    if artist_raw.is_empty() {
        return None;
    }
    let artist = strip_topic_suffix(&artist_raw);

    // title — required; the song name.
    //
    // Real Takeout records carry a localized action-verb prefix on the title
    // field (English: "Watched <song>"; other locales use their own verb).
    // Strip the known English prefix when present; for non-English exports
    // where the title does not start with the recognized prefix, pass the
    // title through unchanged rather than mangling it.  This mirrors the
    // approach in google_takeout.rs (strip_prefix + locale-agnostic guard).
    let raw_title = str_field(record, "title");
    let song_name = strip_watch_prefix(&raw_title);
    if song_name.is_empty() {
        return None;
    }

    // time — ISO 8601 UTC; required for partitioning.
    let raw_time = str_field(record, "time");
    if raw_time.is_empty() {
        return None;
    }
    let ts = to_local(&raw_time);
    // Reject if we can't determine a month partition key.
    Partition::Month.key(&ts)?;

    // guid = deterministic key on (raw_time, title_slug) so overlapping
    // re-exports dedupe correctly (same strategy as google-takeout's
    // "gt-search-{time}-{query}" — Takeout rows carry no native id).
    // The guid uses the bare song name (prefix already stripped) so it is
    // stable regardless of export locale.
    let guid = format!("ym-{}-{}", raw_time, slug(&song_name));

    // titleUrl → carry in extra for full fidelity.
    let url = str_field(record, "titleUrl");
    let mut extra = Map::new();
    if !url.is_empty() {
        extra.insert("titleUrl".into(), Value::String(url.clone()));
    }

    Some(MediaItem {
        ts,
        source: SOURCE.into(),
        category: "music".into(),
        device: String::new(),
        kind: "play".into(),
        title: song_name,
        // artist is the grouping key for top-charts (= subtitle in the contract).
        subtitle: artist,
        // album is absent from the export; keep the URL for completeness.
        detail: url,
        // duration is absent from the export; 0 = honest unknown.
        seconds: 0,
        favicon: String::new(),
        guid,
        extra,
    })
}

/// An RFC3339/ISO-8601 timestamp → RFC3339 local. YouTube Music Takeout
/// stamps UTC with a `Z` suffix and optional fractional seconds
/// (e.g. "2024-09-14T18:32:11.000Z"), which `DateTime::parse_from_rfc3339`
/// accepts. An unparseable value passes through verbatim; the caller's
/// `Partition::Month.key()` then rejects it.
fn to_local(s: &str) -> String {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Local).to_rfc3339())
        .unwrap_or_else(|_| s.to_string())
}

/// A top-level string field, trimmed; `""` when missing or non-string.
fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The localized prefix Google puts on a YouTube Music listen activity's
/// `title` in English Takeout exports.  Other locales use their native verb
/// (e.g. "Écouter <chanson>" in French); we strip only the known English form
/// and pass non-matching titles through unchanged.
const WATCHED_PREFIX: &str = "Watched ";

/// Strip the "Watched " (English) action-verb prefix from a YTM `title`
/// field, returning the bare song name.  If the prefix is absent (non-English
/// export, or a future format change), the title is returned trimmed but
/// otherwise unchanged — no mangling of unknown-format titles.
fn strip_watch_prefix(title: &str) -> String {
    let t = title.trim();
    if let Some(rest) = t.strip_prefix(WATCHED_PREFIX) {
        let song = rest.trim();
        if !song.is_empty() {
            return song.to_string();
        }
        // "Watched " with nothing after it — treat the whole title as the name
        // (degenerate case; caller will reject if empty anyway).
        return t.to_string();
    }
    t.to_string()
}

/// Strip the " - Topic" suffix that YouTube Music appends to auto-generated
/// artist channel names (e.g. "Tame Impala - Topic" → "Tame Impala").
/// When the suffix is absent the name is returned unchanged.
const TOPIC_SUFFIX: &str = " - Topic";

fn strip_topic_suffix(name: &str) -> String {
    name.strip_suffix(TOPIC_SUFFIX)
        .unwrap_or(name)
        .to_string()
}

/// ASCII-lowercase slug: non-alphanumeric runs → single `-`, leading/trailing
/// dashes stripped. Used in guid construction (same logic as lastfm.rs).
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-youtube-music-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixtures — modeled on the confirmed Takeout music-history.json schema
    // (research notes L3314-L3316; same subtitles[] shape as watch-history.json
    // which is already exercised in google_takeout.rs fixtures).

    /// A full record using REAL Takeout shape: title has "Watched " prefix,
    /// artist channel has " - Topic" suffix (confirmed by multiple real-data
    /// sources — YTMtoMaloja, data-goblins.com, cinfulsinamon, beebls).
    fn full_record() -> Value {
        json!({
            "header": "YouTube Music",
            "title": "Watched Feels Like We Only Go Backwards",
            "titleUrl": "https://music.youtube.com/watch?v=ILbobUEjxhU",
            "subtitles": [
                {
                    "name": "Tame Impala - Topic",
                    "url": "https://music.youtube.com/channel/UC7MzUEIIRfNvr_ZE7EeZAQQ"
                }
            ],
            "time": "2024-09-14T18:32:11.000Z"
        })
    }

    /// A record with no subtitles array (artist absent).
    /// Must be DROPPED — reference parser does `select(has("subtitles"))`.
    fn sparse_record_no_subtitles() -> Value {
        json!({
            "header": "YouTube Music",
            "title": "Watched Lo-Fi Hip Hop Radio",
            "time": "2024-09-15T09:10:00.000Z"
        })
    }

    /// A record with an empty subtitles array (different from absent).
    /// Also dropped — no first element means no artist name.
    fn empty_subtitles_record() -> Value {
        json!({
            "header": "YouTube Music",
            "title": "Watched Midnight City",
            "subtitles": [],
            "time": "2024-09-16T22:00:00.000Z"
        })
    }

    /// A plain YouTube video watch (header == "YouTube") — must be filtered out.
    fn youtube_video_record() -> Value {
        json!({
            "header": "YouTube",
            "title": "Watched some video",
            "subtitles": [{"name": "Some Channel", "url": "https://www.youtube.com/channel/UC123"}],
            "time": "2024-09-15T10:00:00.000Z"
        })
    }

    /// A record with no title — must be skipped.
    fn no_title_record() -> Value {
        json!({
            "header": "YouTube Music",
            "time": "2024-09-14T08:00:00Z"
        })
    }

    /// A record with no time — must be skipped (can't partition).
    fn no_time_record() -> Value {
        json!({
            "header": "YouTube Music",
            "title": "Watched Some Song"
        })
    }

    /// A record from a non-English export where the title does NOT start with
    /// "Watched " — must pass through the title unchanged (locale guard).
    fn non_english_record() -> Value {
        json!({
            "header": "YouTube Music",
            "title": "You Got One Too",
            "subtitles": [{"name": "Idle Garden - Topic", "url": "https://music.youtube.com/channel/xxx"}],
            "time": "2024-09-17T10:00:00.000Z"
        })
    }

    // -----------------------------------------------------------------------
    // Pure mapping tests

    #[test]
    fn maps_full_record_to_media_item_with_correct_fields() {
        let item = listen_from(&full_record()).expect("full record must map");
        assert_eq!(item.source, "youtube-music");
        assert_eq!(item.category, "music");
        assert_eq!(item.kind, "play");
        // "Watched " prefix stripped → bare song name.
        assert_eq!(item.title, "Feels Like We Only Go Backwards",
            "title must have 'Watched ' prefix stripped");
        // " - Topic" suffix stripped → bare artist name.
        assert_eq!(item.subtitle, "Tame Impala",
            "artist from subtitles[0].name with ' - Topic' suffix stripped");
        assert_eq!(item.seconds, 0, "duration absent → 0 (honest unknown)");

        // guid is stable on (raw_time, bare-title-slug) — uses prefix-stripped name.
        assert_eq!(
            item.guid,
            "ym-2024-09-14T18:32:11.000Z-feels-like-we-only-go-backwards"
        );
        // ts = time, converted to local (same instant as source UTC).
        let expected_utc = DateTime::parse_from_rfc3339("2024-09-14T18:32:11.000Z")
            .unwrap()
            .timestamp();
        let actual_utc = DateTime::parse_from_rfc3339(&item.ts).unwrap().timestamp();
        assert_eq!(actual_utc, expected_utc, "ts preserves the instant");

        // titleUrl preserved in extra and in detail.
        assert_eq!(
            item.extra.get("titleUrl"),
            Some(&json!("https://music.youtube.com/watch?v=ILbobUEjxhU"))
        );
        assert_eq!(item.detail, "https://music.youtube.com/watch?v=ILbobUEjxhU");
    }

    #[test]
    fn drops_record_without_subtitles_array() {
        // Records with no subtitles field are dropped (reference parser: select(has("subtitles"))).
        assert!(
            listen_from(&sparse_record_no_subtitles()).is_none(),
            "no subtitles array → dropped"
        );
    }

    #[test]
    fn drops_record_with_empty_subtitles_array() {
        // Empty subtitles array means no artist — also dropped.
        assert!(
            listen_from(&empty_subtitles_record()).is_none(),
            "empty subtitles array → dropped (no artist name)"
        );
    }

    #[test]
    fn drops_non_youtube_music_header() {
        // Plain YouTube video watch must be filtered out.
        assert!(
            listen_from(&youtube_video_record()).is_none(),
            "header == 'YouTube' → filtered out"
        );
        // Record with no header also filtered out.
        let no_header = json!({
            "title": "Watched Some Song",
            "subtitles": [{"name": "Some Artist", "url": "https://music.youtube.com/x"}],
            "time": "2024-09-15T10:00:00.000Z"
        });
        assert!(
            listen_from(&no_header).is_none(),
            "missing header → filtered out"
        );
    }

    #[test]
    fn skips_record_without_title() {
        assert!(
            listen_from(&no_title_record()).is_none(),
            "no title → skipped"
        );
    }

    #[test]
    fn skips_record_without_time() {
        assert!(
            listen_from(&no_time_record()).is_none(),
            "no time → skipped (can't partition)"
        );
    }

    #[test]
    fn guid_is_deterministic_and_stable() {
        // Same record parsed twice → identical guid.
        let a = listen_from(&full_record()).unwrap();
        let b = listen_from(&full_record()).unwrap();
        assert_eq!(a.guid, b.guid, "guid is deterministic");
    }

    /// Regression: "Watched <song>" title → bare song name (blocking defect fix).
    /// The raw Takeout `title` carries the localized action verb; stripping it
    /// is required for the contract layer.
    #[test]
    fn strips_watched_prefix_from_title() {
        let record = json!({
            "header": "YouTube Music",
            "title": "Watched You Got One Too",
            "subtitles": [{"name": "Idle Garden - Topic", "url": "https://music.youtube.com/channel/xxx"}],
            "time": "2024-09-17T10:00:00.000Z"
        });
        let item = listen_from(&record).expect("record with 'Watched' prefix must map");
        assert_eq!(
            item.title, "You Got One Too",
            "title must be stripped of 'Watched ' prefix; got: {:?}",
            item.title
        );
        // guid must use the bare name, not "watched-you-got-one-too".
        assert!(
            item.guid.contains("you-got-one-too"),
            "guid uses bare title slug: {}",
            item.guid
        );
        assert!(
            !item.guid.contains("watched"),
            "guid must NOT contain 'watched': {}",
            item.guid
        );
    }

    /// Regression: " - Topic" artist suffix stripped (major defect fix).
    /// Auto-generated YouTube Music topic channels append " - Topic" which
    /// must be removed before storing as the grouping key.
    #[test]
    fn strips_topic_suffix_from_artist() {
        let record = json!({
            "header": "YouTube Music",
            "title": "Watched Some Track",
            "subtitles": [{"name": "Tame Impala - Topic", "url": "https://music.youtube.com/channel/yyy"}],
            "time": "2024-09-18T12:00:00.000Z"
        });
        let item = listen_from(&record).expect("record with ' - Topic' artist must map");
        assert_eq!(
            item.subtitle, "Tame Impala",
            "' - Topic' suffix must be stripped from artist; got: {:?}",
            item.subtitle
        );
    }

    /// Locale guard: a non-English title without "Watched " prefix passes
    /// through unchanged (no mangling of non-English exports).
    #[test]
    fn non_english_title_passes_through_unchanged() {
        let item = listen_from(&non_english_record())
            .expect("non-English record must map");
        // Title does not start with "Watched " → kept verbatim.
        assert_eq!(
            item.title, "You Got One Too",
            "non-English title without 'Watched' prefix must be kept verbatim; got: {:?}",
            item.title
        );
        // " - Topic" suffix still stripped from artist.
        assert_eq!(item.subtitle, "Idle Garden",
            "' - Topic' suffix stripped even on non-English record");
    }

    /// Helper unit tests for the strip functions.
    #[test]
    fn strip_watch_prefix_cases() {
        // English prefix present.
        assert_eq!(strip_watch_prefix("Watched Some Song"), "Some Song");
        // Leading/trailing whitespace is trimmed first; "  Watched  Song  " trims to
        // "Watched  Song", then strip_prefix("Watched ") removes "Watched " leaving
        // " Song" (with leading space), which trims to "Song".
        assert_eq!(strip_watch_prefix("  Watched  Song  "), "Song");
        // No prefix → pass-through.
        assert_eq!(strip_watch_prefix("Some Song"), "Some Song");
        // French (non-English) → pass-through.
        assert_eq!(strip_watch_prefix("Écouter Quelque Chose"), "Écouter Quelque Chose");
        // "Watched " with nothing after → input trims to "Watched"; strip_prefix
        // doesn't match (no trailing space), so the trimmed title passes through.
        assert_eq!(strip_watch_prefix("Watched "), "Watched");
    }

    #[test]
    fn strip_topic_suffix_cases() {
        assert_eq!(strip_topic_suffix("Tame Impala - Topic"), "Tame Impala");
        assert_eq!(strip_topic_suffix("Idle Garden - Topic"), "Idle Garden");
        // No suffix → unchanged.
        assert_eq!(strip_topic_suffix("Tame Impala"), "Tame Impala");
        // Partial suffix → unchanged.
        assert_eq!(strip_topic_suffix("Artist - Top"), "Artist - Top");
        assert_eq!(strip_topic_suffix(""), "");
    }

    // -----------------------------------------------------------------------
    // Import tests (bare JSON + zip)

    fn write_json(vault: &Vault, name: &str, records: &Value) -> std::path::PathBuf {
        let path = vault.root().join(name);
        fs::write(&path, serde_json::to_string(records).unwrap()).unwrap();
        path
    }

    #[test]
    fn imports_bare_json_writes_both_layers_and_is_rerunnable() {
        let v = temp_vault("bare");
        // watch-history.json mixes YouTube Music listens (header: "YouTube Music")
        // with plain YouTube video watches (header: "YouTube") and no-subtitles records.
        // Only the full_record() with header + subtitles imports; others are skipped.
        let records = json!([
            full_record(),              // → imported
            sparse_record_no_subtitles(), // → skipped (no subtitles)
            youtube_video_record(),     // → skipped (wrong header)
            no_title_record(),          // → skipped (no title)
            no_time_record(),           // → skipped (no time)
        ]);
        let path = write_json(&v, "watch-history.json", &records);

        let out = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&1), "1 valid YTM record");
        assert_eq!(out.counts.get("skipped"), Some(&4), "4 non-YTM / malformed skipped");
        assert!(
            out.headline.starts_with("1 listens imported"),
            "headline: {}",
            out.headline
        );

        // Contract row in the local month.
        let month_full =
            Partition::Month.key(&to_local("2024-09-14T18:32:11.000Z")).unwrap().to_string();
        let contract =
            fs::read_to_string(v.root().join(format!("media/plays/youtube-music/{month_full}.jsonl")))
                .unwrap();
        // "Watched " prefix must be stripped in the contract layer.
        assert!(
            contract.contains("\"title\":\"Feels Like We Only Go Backwards\""),
            "contract title must be bare (no 'Watched ' prefix): {contract}"
        );
        assert!(
            !contract.contains("\"title\":\"Watched"),
            "contract must NOT contain 'Watched' prefix in title: {contract}"
        );
        // " - Topic" suffix must be stripped in the contract layer.
        assert!(
            contract.contains("\"subtitle\":\"Tame Impala\""),
            "contract subtitle must be bare artist (no ' - Topic' suffix): {contract}"
        );
        assert!(
            !contract.contains("Tame Impala - Topic"),
            "contract must NOT contain ' - Topic' suffix in subtitle: {contract}"
        );
        assert!(
            contract.contains("\"source\":\"youtube-music\""),
            "source field: {contract}"
        );
        assert!(
            contract.contains("\"category\":\"music\""),
            "category field: {contract}"
        );
        assert!(
            contract.contains("\"kind\":\"play\""),
            "kind field: {contract}"
        );

        // Raw layer keeps the verbatim Takeout object (header, subtitles URL, etc.).
        let raw =
            fs::read_to_string(v.root().join(format!("media/plays/youtube-music/raw/{month_full}.jsonl")))
                .unwrap();
        assert!(
            raw.contains("\"header\":\"YouTube Music\""),
            "raw keeps source-only fields: {raw}"
        );
        assert!(
            raw.contains("UC7MzUEIIRfNvr_ZE7EeZAQQ"),
            "raw keeps channel URL: {raw}"
        );
        // Raw layer preserves verbatim values (prefix and suffix unstripped).
        assert!(
            raw.contains("Watched Feels Like We Only Go Backwards"),
            "raw preserves verbatim title (with 'Watched' prefix): {raw}"
        );
        assert!(
            raw.contains("Tame Impala - Topic"),
            "raw preserves verbatim artist channel name (with ' - Topic'): {raw}"
        );

        // Re-import the same file → pure duplicates, contract file unchanged.
        let again = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(again.counts.get("duplicates"), Some(&1), "re-import: 1 duplicate");
        assert_eq!(again.counts.get("imported"), Some(&0));
        let contract2 =
            fs::read_to_string(v.root().join(format!("media/plays/youtube-music/{month_full}.jsonl")))
                .unwrap();
        assert_eq!(contract, contract2, "contract file byte-identical after re-run");
    }

    #[test]
    fn imports_from_full_takeout_zip() {
        use std::io::Write;

        let v = temp_vault("zip");
        let zip_path = v.root().join("takeout.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();

        // watch-history.json is the REAL Takeout file: mixes YouTube Music listens
        // (header: "YouTube Music") with plain YouTube video watches (header: "YouTube").
        // The import must filter to YTM-only and skip the video watch.
        w.start_file(
            "Takeout/YouTube and YouTube Music/history/watch-history.json",
            opts,
        )
        .unwrap();
        let records = json!([
            full_record(),          // YTM listen — imported
            youtube_video_record(), // plain YouTube video — skipped (wrong header)
        ]);
        w.write_all(serde_json::to_string(&records).unwrap().as_bytes())
            .unwrap();

        // An unrelated file in the archive.
        w.start_file("Takeout/Drive/README.md", opts).unwrap();
        w.write_all(b"ignore me").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts.get("imported"), Some(&1), "1 YTM listen imported from zip");
        assert_eq!(out.counts.get("skipped"), Some(&1), "1 video watch skipped");

        // Verify the contract file was written.
        let month = Partition::Month
            .key(&to_local("2024-09-14T18:32:11.000Z"))
            .unwrap()
            .to_string();
        let contract =
            fs::read_to_string(v.root().join(format!("media/plays/youtube-music/{month}.jsonl")))
                .unwrap();
        assert_eq!(contract.lines().count(), 1, "1 line in contract file (only YTM listen)");
        // " - Topic" suffix stripped in contract layer; bare artist name stored.
        assert!(contract.contains("\"subtitle\":\"Tame Impala\""),
            "contract subtitle is bare artist name (no ' - Topic'): {contract}");
        assert!(!contract.contains("Tame Impala - Topic"),
            "contract must NOT contain ' - Topic' suffix: {contract}");
        // Plain YouTube video watch must NOT appear in the contract.
        assert!(!contract.contains("Some Channel"),
            "plain YouTube video watch must not be in contract: {contract}");
    }

    #[test]
    fn zip_without_watch_history_errors_clearly() {
        use std::io::Write;

        let v = temp_vault("zip-nowh");
        let zip_path = v.root().join("takeout.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        // Archive with an unrelated file but no watch-history.json.
        w.start_file("Takeout/Drive/README.md", opts).unwrap();
        w.write_all(b"ignore me").unwrap();
        w.finish().unwrap();

        let err = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("watch-history.json"),
            "clear error names the missing file: {err}"
        );
    }

    #[test]
    fn slug_function_collapses_non_ascii_and_runs() {
        assert_eq!(slug("Tame Impala"), "tame-impala");
        assert_eq!(slug("Feels Like We Only Go Backwards"), "feels-like-we-only-go-backwards");
        assert_eq!(slug("Sigur Rós"), "sigur-r-s"); // non-ASCII → dash
        assert_eq!(slug(""), "");
        assert_eq!(slug("  "), "");
    }

    #[test]
    fn def_has_correct_metadata() {
        assert_eq!(DEF.meta.id, "youtube-music");
        assert!(matches!(DEF.behavior, Behavior::Import(_)));
        assert_eq!(DEF.connection, None, "no connection — import only");
        assert_eq!(DEF.meta.domain, "media");
        let imp = match DEF.behavior {
            Behavior::Import(s) => s,
            _ => panic!("expected Import"),
        };
        assert!(imp.accepts.contains(&"zip"));
        assert!(imp.accepts.contains(&"json"));
    }

    #[test]
    fn html_format_json_error_gives_clear_message() {
        // If the user exports HTML instead of JSON, the file won't be a JSON array.
        let v = temp_vault("html-err");
        let path = v.root().join("watch-history.json");
        fs::write(&path, b"<html><body>Watch History</body></html>").unwrap();
        let err = (IMPORT.run)(&v, &path, &BTreeMap::new(), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("JSON") || err.contains("json"),
            "error should mention JSON format: {err}"
        );
    }
}
