//! Kindle Highlights — community-documented My Clippings.txt device export.
//! Catalogued in the Phase 2 pass; brief: docs/integrations/kindle.md
//!
//! An **Import** that ingests `My Clippings.txt` from the user's physical
//! Kindle (mounted over USB at `/Volumes/<KindleName>/documents/My
//! Clippings.txt`) through the generic import box.
//!
//! Each clipping block → one [`crate::reading::Highlight`] under
//! `reading/kindle/highlights/YYYY-MM.jsonl` (`guid` = sha2(title, location,
//! added-date, kind); `ts` = Added date parsed to RFC3339 local; `title`/
//! `author` from the header line; `text` = highlighted passage (omitted for
//! bookmarks and capped clippings); `note` = user note for Note-type blocks;
//! `location` = location/page range). Raw blocks land verbatim under
//! `reading/kindle/raw/` for full fidelity.
//!
//! Two layers unconditionally:
//! - **Raw layer:** `reading/kindle/raw/` — one JSONL line per parsed block,
//!   full fidelity (title, author, kind, location, date string, text).
//! - **Contract layer:** `reading/kindle/highlights/YYYY-MM.jsonl` — one
//!   normalized [`Highlight`] per clipping, deduped by `guid`.
//!
//! ## File format (long-stable, community-documented)
//!
//! Each clipping is a 3-or-4-line block terminated by `==========`:
//!
//! ```text
//! Book Title (Author Name)
//! - Your Highlight on page 3 | location 429-430 | Added on Thursday, 8 June 2026 20:11:00
//!
//! The highlighted passage.
//! ==========
//! ```
//!
//! The **dominant** real-world metadata line has three pipe-separated segments:
//! kind/page, Kindle location range, and the Added date. Older or alternate
//! firmware may emit a two-segment form without the page: `"- Your Highlight at
//! location 142-145 | Added on <date>"`. Both forms are supported.
//!
//! Block anatomy:
//! - **Line 1:** `<Title> (<Author>)` — the title and author wrapped in
//!   parentheses. If there is no author the line is just `<Title>`. The BOM
//!   (`\u{FEFF}`) that some firmware versions prepend to the first block is
//!   stripped.
//! - **Line 2 (three-segment):** `- Your Highlight on page 3 | location 429-430
//!   | Added on <date>` — page, Kindle location range, and the added timestamp.
//! - **Line 2 (two-segment):** `- Your Highlight at location 142-145 | Added on
//!   <date>` — older firmware omits the page segment.
//!   Locale and firmware vary the exact words and date format; the parser
//!   accepts all confirmed variants.
//! - **Line 3:** blank (the clipping separator between the metadata and text).
//! - **Line 4+:** the clipping text (may be multi-line for notes). Absent for
//!   bookmarks. A purchased book that has hit Amazon's ~10% clipping cap
//!   carries `<You have reached the clipping limit for this item>` (some
//!   firmware variants emit a slightly different message); the parser records
//!   an empty `text` and sets `capped = true` in `extra` rather than treating
//!   it as an error.
//! - **Separator:** `==========` (ten `=` signs).
//!
//! Date variants observed across firmware versions:
//! - `Thursday, 8 June 2026 20:11:00` — English, day-of-week prefix
//! - `June 8, 2026 8:11:00 PM` — US format with AM/PM
//! - `Monday, June 8, 2026 8:11:00 AM` — US format with weekday
//! - `8 June 2026 20:11:00` — English without weekday
//! The parser tries each format in order; on failure it stores the raw
//! string and still writes the row with an approximate timestamp.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{Local, NaiveDateTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::reading::Highlight;
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;

/// Contract-layer highlights live under `highlights/`, raw under `raw/`.
const HIGHLIGHTS_DIR: &str = "reading/kindle/highlights";
const RAW_DIR: &str = "reading/kindle/raw";

/// The clipping separator that divides blocks.
const SEPARATOR: &str = "==========";

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(HIGHLIGHTS_DIR))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "kindle",
        name: "Kindle Highlights",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports highlights, notes, and bookmarks from your Kindle's \
                      My Clippings.txt file. Connect your Kindle over USB, then \
                      import the file from /Volumes/<KindleName>/documents/My \
                      Clippings.txt. Re-importing appends only new clippings.",
        domain: "reading",
        vault_path: "reading/kindle/",
        toggleable: false,
        setup: &[
            "Connect your Kindle to your Mac with a USB cable.",
            "Open Finder — your Kindle appears as a drive under Locations.",
            "Navigate to documents/My Clippings.txt inside the Kindle drive.",
            "Drag that file into the import box here.",
        ],
        caveats: "Device file only — highlights made in the Kindle app on a phone \
                  or tablet are NOT included here. Purchased books may be capped at \
                  ~10% of the book's text by Amazon; capped clippings are recorded \
                  as empty highlights with a flag in extra. Sideloaded books have no \
                  cap. Readwise is the cloud complement for users who sync highlights \
                  automatically.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["txt"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Raw row shape — one parsed block verbatim, tagged with the contract ts so
// the month-partition writer files it under the right month.

#[derive(Serialize, Deserialize)]
struct RawLine {
    /// RFC3339 local — the partition key (skipped on serialization, used only
    /// to drive `stream.append`).
    #[serde(skip)]
    ts: String,
    #[serde(flatten)]
    value: Value,
}

// ---------------------------------------------------------------------------
// Block parsing.

/// A parsed clipping block before mapping to the contract.
#[derive(Debug, PartialEq)]
struct Block {
    title: String,
    author: String,
    kind: String,   // "highlight" | "note" | "bookmark"
    /// Kindle location range (e.g. "429-430") — the more granular anchor.
    /// Preferred as the contract `location` field. Empty when not present.
    location: String,
    /// Physical page number from the metadata line, when present.
    /// Stored in `extra.page`; not used as the primary location.
    page: String,
    /// Added date as parsed RFC3339 local string (best-effort).
    date_ts: String,
    /// The raw "Added on …" date string — kept for full fidelity even when
    /// parsing fails so the row is not dropped.
    date_raw: String,
    /// Highlighted passage / note body. Empty for bookmarks and capped clippings.
    text: String,
    /// True when the clipping-limit marker was found instead of text.
    capped: bool,
    /// The original raw block text (everything between separators, trimmed),
    /// stored verbatim in the raw layer for full fidelity and future re-parsing.
    raw_block: String,
}

/// Strip the UTF-8 BOM (`\u{FEFF}`) that some Kindle firmware versions
/// prepend to the very first byte of My Clippings.txt.
fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{FEFF}').unwrap_or(s)
}

/// Parse `"Book Title (Author Name)"` into `(title, author)`.
/// If there is no parenthesized author suffix the whole string is the title.
fn parse_title_author(s: &str) -> (String, String) {
    // Find the LAST pair of parentheses — titles themselves sometimes contain
    // parentheses (e.g. "Harry Potter and the Goblet of Fire (Book 4)").
    let s = s.trim();
    if let Some(close) = s.rfind(')') {
        if let Some(open) = s[..close].rfind('(') {
            let title = s[..open].trim().to_string();
            let author = s[open + 1..close].trim().to_string();
            if !title.is_empty() && !author.is_empty() {
                return (title, author);
            }
        }
    }
    (s.to_string(), String::new())
}

/// Parse the metadata line. Two real-world shapes exist:
///
/// **Three-segment (dominant):**
/// `- Your Highlight on page 3 | location 429-430 | Added on Thursday, 8 June 2026 20:11:00`
/// → page="3", location="429-430"
///
/// **Two-segment (older/alternate firmware):**
/// `- Your Highlight at location 142-145 | Added on Thursday, 8 June 2026 20:11:00`
/// → page="", location="142-145"
///
/// Returns `(kind, location, page, date_ts, date_raw)`.
/// `kind` is `"highlight"`, `"note"`, or `"bookmark"`.
/// `location` is the Kindle location range (preferred; more granular anchor).
/// `page` is the physical page number when present, else empty.
/// `date_ts` is RFC3339 local (best-effort; falls back to epoch ts).
/// `date_raw` is the raw date substring for full fidelity.
fn parse_metadata(line: &str) -> Option<(String, String, String, String, String)> {
    // Strip the leading dash and whitespace.
    let line = line.trim().strip_prefix('-')?.trim();

    // Split on " | Added on " or " | Added " (some older firmware omits "on").
    let (pre_added, date_part) = if let Some(idx) = line.find(" | Added on ") {
        (&line[..idx], &line[idx + " | Added on ".len()..])
    } else if let Some(idx) = line.find(" | Added ") {
        (&line[..idx], &line[idx + " | Added ".len()..])
    } else {
        return None;
    };

    // `pre_added` may now be:
    //   (a) "Your Highlight on page 3 | location 429-430"  (three-segment)
    //   (b) "Your Highlight at location 142-145"            (two-segment)
    //
    // In case (a) the first pipe splits into the kind/page segment and a
    // dedicated location segment.
    let (kind_page_seg, explicit_location_seg) = if let Some(idx) = pre_added.find(" | ") {
        (&pre_added[..idx], Some(&pre_added[idx + " | ".len()..]))
    } else {
        (pre_added, None)
    };

    // Extract kind from "Your Highlight …", "Your Note …", "Your Bookmark …"
    let kind = if kind_page_seg.to_ascii_lowercase().contains("note") {
        "note"
    } else if kind_page_seg.to_ascii_lowercase().contains("bookmark") {
        "bookmark"
    } else {
        "highlight"
    };

    // Determine location and page:
    // - If there is an explicit location segment (three-segment form), parse it
    //   for the Kindle location range and parse the kind/page segment for the
    //   physical page number.
    // - Otherwise (two-segment form), fall through to extract_location_value on
    //   the combined segment.
    let (location, page) = if let Some(loc_seg) = explicit_location_seg {
        // Explicit segment: "location 429-430" or "Location 429-430"
        let loc = extract_location_value(loc_seg);
        // Page from the kind/page segment: "Your Highlight on page 3"
        let pg = extract_page_value(kind_page_seg);
        (loc, pg)
    } else {
        // Two-segment: kind/location all in one segment.
        // "Your Highlight at location 142-145" → location="142-145", page=""
        let loc = extract_location_value(kind_page_seg);
        // Still check for a page keyword in case of unusual two-segment forms.
        let pg = extract_page_value(kind_page_seg);
        // If both present, prefer location as the primary; page fallback.
        let loc_empty = loc.is_empty();
        let final_loc = if !loc_empty { loc } else { pg.clone() };
        let final_pg = if loc_empty { String::new() } else { pg };
        (final_loc, final_pg)
    };

    let date_raw = date_part.trim().to_string();
    let date_ts = parse_date(&date_raw);

    Some((kind.to_string(), location, page, date_ts, date_raw))
}

/// Extract the Kindle location range value from a segment string.
/// Matches "location X-Y", "Location X-Y" (Kindle capitalizes it in some
/// firmware), "at location X-Y", "on location X-Y", "at position X", etc.
/// Returns the range/number string (e.g. "429-430"), empty if not found.
fn extract_location_value(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    // Order matters: longer/more-specific keywords first to avoid partial matches.
    for kw in &["at location ", "on location ", "at position ", "location "] {
        if let Some(idx) = lower.find(kw) {
            let rest = s[idx + kw.len()..].trim();
            let loc = rest.split_whitespace().next().unwrap_or("").trim();
            if !loc.is_empty() {
                return loc.to_string();
            }
        }
    }
    String::new()
}

/// Extract the physical page number from a segment string.
/// Matches "on page X", "at page X", "page X".
/// Returns the page number string (e.g. "3"), empty if not found.
fn extract_page_value(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    for kw in &["on page ", "at page ", "page "] {
        if let Some(idx) = lower.find(kw) {
            let rest = s[idx + kw.len()..].trim();
            let pg = rest.split_whitespace().next().unwrap_or("").trim();
            if !pg.is_empty() {
                return pg.to_string();
            }
        }
    }
    String::new()
}

/// Parse a Kindle date string to RFC3339 local. Returns a fallback
/// `"1970-01-01T00:00:00+00:00"` when no format matches (the row is still
/// written; the raw date is preserved in `extra`).
///
/// Kindle dates come in two main families:
///
/// **US family** (month-before-day, AM/PM, optional weekday):
/// - `"Sunday, June 7, 2026 9:05:00 AM"`
/// - `"June 8, 2026 8:11:00 AM"`
/// - `"Monday, June 1, 2026 6:00:00 AM"`
///
/// **European family** (day-before-month, 24h, optional weekday prefix):
/// - `"Thursday, 8 June 2026 20:11:00"` — chrono validates the weekday
///   against the date, so we also try stripping the "Weekday, " prefix.
/// - `"8 June 2026 20:11:00"`
///
/// Note: chrono's `%A` validates the weekday name against the date (e.g.
/// "Thursday" only matches if the date really is a Thursday). Because Kindle
/// devices occasionally emit the wrong weekday (firmware/timezone edge cases),
/// and because fixtures use test-convenient dates that may not align with the
/// written weekday, we parse with `%A` first (validates correctness) and fall
/// back to stripping the weekday prefix entirely (accepts the date regardless).
fn parse_date(s: &str) -> String {
    let s = s.trim();

    // US family — month before day, AM/PM.
    static US_FMTS: &[&str] = &[
        // With weekday, AM/PM time
        "%A, %B %d, %Y %I:%M:%S %p",
        // Without weekday, AM/PM time
        "%B %d, %Y %I:%M:%S %p",
        // Date only, with weekday
        "%A, %B %d, %Y",
        // Date only
        "%B %d, %Y",
    ];

    // European family — day before month, 24h. We try without any weekday
    // prefix (after stripping) because chrono's %A validates the weekday name.
    static EU_FMTS: &[&str] = &[
        "%d %B %Y %H:%M:%S",
        "%d %B %Y",
    ];

    // European with validated weekday (only works when weekday is correct).
    static EU_WITH_WEEKDAY_FMTS: &[&str] = &[
        "%A, %d %B %Y %H:%M:%S",
        "%A, %d %B %Y",
    ];

    // Try US formats first (they include the weekday validator).
    for fmt in US_FMTS {
        if let Ok(ts) = try_fmt(s, fmt) {
            return ts;
        }
    }

    // Try European with weekday (validates correctness).
    for fmt in EU_WITH_WEEKDAY_FMTS {
        if let Ok(ts) = try_fmt(s, fmt) {
            return ts;
        }
    }

    // European without weekday (no prefix).
    for fmt in EU_FMTS {
        if let Ok(ts) = try_fmt(s, fmt) {
            return ts;
        }
    }

    // Strip a "WEEKDAY, " prefix and re-try the European formats.
    // This handles firmware/timezone edge cases where the weekday name is wrong
    // relative to the date, or test fixtures with made-up weekday labels.
    if let Some(stripped) = strip_weekday_prefix(s) {
        for fmt in EU_FMTS {
            if let Ok(ts) = try_fmt(&stripped, fmt) {
                return ts;
            }
        }
        // Also try US formats on the stripped form (just in case).
        for fmt in US_FMTS {
            if let Ok(ts) = try_fmt(&stripped, fmt) {
                return ts;
            }
        }
    }

    // Fallback — unparseable date; still write the row.
    "1970-01-01T00:00:00+00:00".to_string()
}

/// Try parsing `s` with a single format, returning RFC3339 local on success.
fn try_fmt(s: &str, fmt: &str) -> Result<String, ()> {
    // Try as a full datetime first.
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
        if let Some(local_dt) = Local.from_local_datetime(&ndt).earliest() {
            return Ok(local_dt.to_rfc3339());
        }
    }
    // Try as a date-only (midnight).
    if let Ok(nd) = chrono::NaiveDate::parse_from_str(s, fmt) {
        if let Some(ndt) = nd.and_hms_opt(0, 0, 0) {
            if let Some(local_dt) = Local.from_local_datetime(&ndt).earliest() {
                return Ok(local_dt.to_rfc3339());
            }
        }
    }
    Err(())
}

/// Strip a leading `"WEEKDAY, "` prefix from a date string.
/// Returns `Some(stripped)` when such a prefix is found, `None` otherwise.
/// Weekday names are the English long forms Kindle devices use.
fn strip_weekday_prefix(s: &str) -> Option<String> {
    static WEEKDAYS: &[&str] = &[
        "Monday, ", "Tuesday, ", "Wednesday, ", "Thursday, ",
        "Friday, ", "Saturday, ", "Sunday, ",
    ];
    for wd in WEEKDAYS {
        if let Some(rest) = s.strip_prefix(wd) {
            return Some(rest.to_string());
        }
    }
    None
}

/// True when the text line signals Amazon's clipping-limit cap. The exact
/// phrasing varies slightly across firmware but always contains "clipping limit".
fn is_capped(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("clipping limit") || lower.contains("you have reached")
}

/// Parse the entire file body into a list of [`Block`]s. Unknown or malformed
/// blocks are silently skipped (tolerant parse).
fn parse_clippings(body: &str) -> Vec<Block> {
    let body = strip_bom(body);
    let mut blocks = Vec::new();

    for raw_block in body.split(SEPARATOR) {
        let raw_block = raw_block.trim_matches(['\r', '\n', ' ']);
        if raw_block.is_empty() {
            continue;
        }
        let mut lines: Vec<&str> = raw_block.lines().collect();
        // Strip leading blank lines (some firmware adds one before the title).
        while lines.first().is_some_and(|l| l.trim().is_empty()) {
            lines.remove(0);
        }
        if lines.len() < 2 {
            continue; // need at least title + metadata
        }
        let title_line = lines[0].trim();
        let meta_line = lines[1].trim();

        let (title, author) = parse_title_author(title_line);
        if title.is_empty() {
            continue;
        }
        let Some((kind, location, page, date_ts, date_raw)) = parse_metadata(meta_line) else {
            continue;
        };

        // Lines after the blank separator (line index 2) are the clipping body.
        // Collect non-empty text lines, skip the blank separator.
        let raw_text: String = lines
            .get(3..)
            .unwrap_or(&[])
            .iter()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let capped = is_capped(&raw_text);
        let text = if capped || kind == "bookmark" { String::new() } else { raw_text };

        blocks.push(Block {
            title,
            author,
            kind,
            location,
            page,
            date_ts,
            date_raw,
            text,
            capped,
            raw_block: raw_block.to_string(),
        });
    }
    blocks
}

/// Normalize a location range string for stable guid hashing.
/// Replaces en-dash (U+2013) and em-dash (U+2014) with ASCII hyphen so that
/// the same passage re-exported after a firmware/locale change that switches
/// dash style still produces the same guid (idempotency guarantee).
/// Also collapses surrounding whitespace.
fn normalize_location_for_guid(loc: &str) -> String {
    loc.trim()
        .replace('\u{2013}', "-") // en-dash → hyphen
        .replace('\u{2014}', "-") // em-dash → hyphen
}

/// Stable deterministic guid for a Kindle clipping: sha256(title || kind ||
/// normalized_location || date_raw). We avoid the text body in the hash because
/// capped clippings carry no text but are still unique records.
/// Dashes in the location are normalized (en/em → ASCII) so that firmware and
/// locale variants that differ only in dash style produce the same guid.
fn clipping_guid(title: &str, kind: &str, location: &str, date_raw: &str) -> String {
    let norm_loc = normalize_location_for_guid(location);
    let mut h = Sha256::new();
    h.update(title.as_bytes());
    h.update(b"|");
    h.update(kind.as_bytes());
    h.update(b"|");
    h.update(norm_loc.as_bytes());
    h.update(b"|");
    h.update(date_raw.as_bytes());
    format!("{:x}", h.finalize())
}

/// Map a parsed block to a contract [`Highlight`] and a raw [`Value`].
/// Returns `None` only when the ts can't yield a valid month partition
/// (i.e. the fallback date "1970-01-01" — still written but we need a
/// month key; the epoch falls in 1970-01 which is a valid partition).
fn block_to_row(block: &Block) -> Option<(Highlight, Value)> {
    let guid = clipping_guid(&block.title, &block.kind, &block.location, &block.date_raw);
    // Must be partitionable; the fallback ts "1970-01-01T00:00:00+00:00" is valid.
    Partition::Month.key(&block.date_ts)?;

    let mut extra: Map<String, Value> = Map::new();
    // Preserve the kind in extra for query/filter.
    extra.insert("kind".into(), Value::String(block.kind.clone()));
    // Raw date string for full fidelity (the contract `ts` is the parsed form).
    if block.date_ts != block.date_raw {
        extra.insert("date_raw".into(), Value::String(block.date_raw.clone()));
    }
    if block.capped {
        extra.insert("capped".into(), Value::Bool(true));
    }
    // Physical page number when present (three-segment metadata form).
    if !block.page.is_empty() {
        extra.insert("page".into(), Value::String(block.page.clone()));
    }

    // Note-type blocks carry a user annotation without an underlying passage:
    // the body goes into `note`, `text` stays empty (no highlighted passage).
    // Highlight and bookmark blocks put the passage in `text`, `note` is empty.
    let (hl_text, hl_note) = if block.kind == "note" {
        (String::new(), block.text.clone())
    } else {
        (block.text.clone(), String::new())
    };

    let contract = Highlight {
        ts: block.date_ts.clone(),
        source: "kindle".into(),
        guid: guid.clone(),
        text: hl_text,
        note: hl_note,
        title: block.title.clone(),
        author: block.author.clone(),
        url: String::new(),
        location: block.location.clone(),
        color: String::new(),
        tags: Vec::new(),
        extra,
    };

    // The raw value: a JSON object with the verbatim block text for full
    // fidelity, plus parsed fields for convenience. `block_text` is the
    // untrimmed block string as it appeared between `==========` separators —
    // keeping it verbatim means a future re-parse can recover any field the
    // current parser dropped (e.g. if the page/location split logic changes).
    let raw_val = serde_json::json!({
        "guid": guid,
        "block_text": block.raw_block,
        "title": block.title,
        "author": block.author,
        "kind": block.kind,
        "location": block.location,
        "page": block.page,
        "date_raw": block.date_raw,
        "date_ts": block.date_ts,
        "text": block.text,
        "capped": block.capped,
    });

    Some((contract, raw_val))
}

// ---------------------------------------------------------------------------
// The import.

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("opening {}", path.display()))?;

    let blocks = parse_clippings(&body);
    let total = blocks.len() as u64;

    // Load existing guids from the contract stream for dedup.
    let contract_stream = vault.stream(HIGHLIGHTS_DIR, Partition::Month);
    let raw_stream = vault.stream(RAW_DIR, Partition::Month);

    let mut seen: HashSet<String> = HashSet::new();
    for key in contract_stream.partitions()? {
        for v in contract_stream.read::<Value>(&key)? {
            let g = v.get("guid").and_then(Value::as_str).unwrap_or("");
            if !g.is_empty() {
                seen.insert(g.to_string());
            }
        }
    }

    let (mut imported, mut duplicates, mut skipped) = (0u64, 0u64, 0u64);
    let mut new_rows: Vec<Highlight> = Vec::new();
    let mut new_raws: Vec<RawLine> = Vec::new();

    for block in &blocks {
        let Some((contract, raw_val)) = block_to_row(block) else {
            skipped += 1;
            continue;
        };
        if !seen.insert(contract.guid.clone()) {
            duplicates += 1;
            continue;
        }
        new_raws.push(RawLine { ts: contract.ts.clone(), value: raw_val });
        new_rows.push(contract);
        imported += 1;

        if new_rows.len() % 100 == 0 {
            let pct = (new_rows.len() as f32 / total.max(1) as f32) * 100.0;
            progress(ImportProgress { records: imported, percent: pct });
        }
    }

    contract_stream.append(&new_rows, |r| &r.ts)?;
    raw_stream.append(&new_raws, |r| &r.ts)?;

    progress(ImportProgress { records: imported, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported} clippings imported, {duplicates} duplicates skipped"
        ),
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
    use chrono::Datelike;
    use std::collections::BTreeMap;
    use std::fs;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-kindle-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn import(v: &Vault, txt: &str) -> ImportOutcome {
        let path = v.root().join("My Clippings.txt");
        fs::write(&path, txt).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Fixture — a realistic My Clippings.txt modelled on the dominant real-world
    // three-segment metadata format: "- Your Highlight on page X | location Y-Z
    // | Added on <date>".  Blocks:
    //   1. Highlight  — three-segment (page + location), European date — June 8, 2026 = Monday
    //   2. Note       — three-segment (page + location), US date with AM/PM — June 7, 2026 = Sunday
    //   3. Bookmark   — two-segment (no page, at location), European date — June 5, 2026 = Friday
    //   4. Capped     — three-segment, US date — June 1, 2026 = Monday
    //   5. Location-only (two-segment, "at location")  — older firmware variant — June 8 = Monday
    //
    // The weekday in the European format must match the date for chrono %A.
    // US format with %A also validates; date-only European uses no %A.

    const CLIPPINGS: &str = "\
The Nicomachean Ethics (Aristotle)
- Your Highlight on page 14 | location 142-145 | Added on Monday, 8 June 2026 20:11:00

We are what we repeatedly do. Excellence, then, is not an act but a habit.
==========
Meditations (Marcus Aurelius)
- Your Note on page 40 | location 200 | Added on Sunday, June 7, 2026 9:05:00 AM

Remember: you always have the option to have no opinion.
==========
Thinking, Fast and Slow (Daniel Kahneman)
- Your Bookmark at location 450 | Added on Friday, 5 June 2026 14:30:00

==========
The Great Novel (Some Author)
- Your Highlight on page 200 | location 800-805 | Added on Monday, June 1, 2026 6:00:00 AM

<You have reached the clipping limit for this item>
==========
The Art of Strategy (Avinash Dixit)
- Your Highlight at location 320-325 | Added on Monday, 8 June 2026 21:00:00

Every game has a strategic structure.
==========
";

    #[test]
    fn parse_five_block_types() {
        let blocks = parse_clippings(CLIPPINGS);
        assert_eq!(blocks.len(), 5, "five blocks parsed");

        // 1. Highlight — three-segment: page + location range
        let h = &blocks[0];
        assert_eq!(h.title, "The Nicomachean Ethics");
        assert_eq!(h.author, "Aristotle");
        assert_eq!(h.kind, "highlight");
        // The Kindle location range is the primary location field.
        assert_eq!(h.location, "142-145", "location range extracted from 3-segment form");
        // The page number is stored separately.
        assert_eq!(h.page, "14", "page extracted from 3-segment form");
        assert_eq!(h.text, "We are what we repeatedly do. Excellence, then, is not an act but a habit.");
        assert!(!h.capped);
        // Parses to a valid RFC3339 timestamp.
        chrono::DateTime::parse_from_rfc3339(&h.date_ts).expect("valid ts");
        // raw_block is set
        assert!(!h.raw_block.is_empty(), "raw_block captured");

        // 2. Note — three-segment: page + location, US date AM/PM
        let n = &blocks[1];
        assert_eq!(n.title, "Meditations");
        assert_eq!(n.author, "Marcus Aurelius");
        assert_eq!(n.kind, "note");
        assert_eq!(n.location, "200", "note location from 3-segment");
        assert_eq!(n.page, "40", "note page from 3-segment");
        assert_eq!(n.text, "Remember: you always have the option to have no opinion.");
        assert!(!n.capped);

        // 3. Bookmark — two-segment: "at location", no page
        let b = &blocks[2];
        assert_eq!(b.title, "Thinking, Fast and Slow");
        assert_eq!(b.author, "Daniel Kahneman");
        assert_eq!(b.kind, "bookmark");
        assert_eq!(b.location, "450", "two-segment bookmark location");
        assert!(b.page.is_empty(), "two-segment has no page");
        assert!(b.text.is_empty(), "bookmarks have no text");

        // 4. Capped clipping — three-segment
        let c = &blocks[3];
        assert_eq!(c.title, "The Great Novel");
        assert_eq!(c.author, "Some Author");
        assert_eq!(c.kind, "highlight");
        assert_eq!(c.location, "800-805", "capped location from 3-segment");
        assert_eq!(c.page, "200", "capped page from 3-segment");
        assert!(c.text.is_empty(), "capped clipping has empty text");
        assert!(c.capped, "capped flag set");

        // 5. Older firmware two-segment: "at location X-Y"
        let old = &blocks[4];
        assert_eq!(old.title, "The Art of Strategy");
        assert_eq!(old.kind, "highlight");
        assert_eq!(old.location, "320-325", "two-segment at-location form");
        assert!(old.page.is_empty(), "two-segment has no page");
        assert_eq!(old.text, "Every game has a strategic structure.");
    }

    #[test]
    fn import_runs_re_runnable_and_deduplicates() {
        let v = temp_vault("rerun");
        let out = import(&v, CLIPPINGS);
        assert_eq!(out.counts.get("imported"), Some(&5), "all five blocks imported");
        assert_eq!(out.counts.get("duplicates"), Some(&0));

        // Re-import: all five are now duplicates.
        let again = import(&v, CLIPPINGS);
        assert_eq!(again.counts.get("imported"), Some(&0));
        assert_eq!(again.counts.get("duplicates"), Some(&5));

        // Files unchanged.
        let hl_path = v.root().join("reading/kindle/highlights");
        assert!(hl_path.exists(), "highlights dir created");
    }

    #[test]
    fn contract_rows_land_in_correct_month_partition() {
        let v = temp_vault("partition");
        import(&v, CLIPPINGS);

        // The June 8 highlight lands in 2026-06.
        let jun = v.root().join("reading/kindle/highlights/2026-06.jsonl");
        assert!(jun.exists(), "June highlights file exists");
        let content = fs::read_to_string(&jun).unwrap();
        assert!(content.contains("Nicomachean"), "highlight in June file");
        assert!(content.contains("\"kind\":\"highlight\""), "kind in extra");
    }

    #[test]
    fn capped_clipping_has_flag_in_extra_not_an_error() {
        let v = temp_vault("capped");
        let out = import(&v, CLIPPINGS);
        // All four imported, capped one not treated as an error.
        assert_eq!(out.counts.get("skipped"), Some(&0), "capped is not a skip");
        // The capped row appears in the contract with empty text + capped=true.
        let stream = vault_jun_content(&v, "2026-06");
        let capped_row: serde_json::Value = stream
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .find(|v: &serde_json::Value| {
                v.get("extra")
                    .and_then(|e| e.get("capped"))
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            })
            .expect("capped row in contract");
        assert!(
            capped_row.get("text").is_none() || capped_row["text"] == "",
            "empty text on capped row"
        );
    }

    fn vault_jun_content(v: &Vault, month: &str) -> String {
        let path = v.root().join(format!("reading/kindle/highlights/{month}.jsonl"));
        fs::read_to_string(path).unwrap_or_default()
    }

    #[test]
    fn note_block_text_goes_into_note_field_not_text() {
        let v = temp_vault("note_field");
        import(&v, CLIPPINGS);
        // The note block should appear with non-empty `note` and empty/absent `text`.
        let stream = vault_jun_content(&v, "2026-06");
        let note_row: serde_json::Value = stream
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .find(|v: &serde_json::Value| {
                v.get("extra")
                    .and_then(|e| e.get("kind"))
                    .and_then(serde_json::Value::as_str)
                    == Some("note")
            })
            .expect("note row in contract");
        assert!(
            !note_row["note"].as_str().unwrap_or("").is_empty(),
            "note text in `note` field"
        );
        // `text` should be empty or absent for a note-type block.
        let text = note_row.get("text").and_then(serde_json::Value::as_str).unwrap_or("");
        assert!(text.is_empty(), "note-type block leaves `text` empty: {text}");
    }

    #[test]
    fn raw_layer_written_with_full_fidelity() {
        let v = temp_vault("raw");
        import(&v, CLIPPINGS);
        let raw_dir = v.root().join("reading/kindle/raw");
        assert!(raw_dir.exists(), "raw dir created");
        let mut found = false;
        for entry in fs::read_dir(&raw_dir).unwrap().flatten() {
            let body = fs::read_to_string(entry.path()).unwrap_or_default();
            if body.contains("Nicomachean") {
                found = true;
                // Raw keeps the original date string and all fields.
                assert!(body.contains("date_raw"), "raw keeps date_raw field");
                assert!(body.contains("location"), "raw keeps location field");
                // Raw keeps the verbatim block text for future re-parsing.
                assert!(body.contains("block_text"), "raw keeps verbatim block_text");
                // The block_text contains the actual metadata line verbatim.
                assert!(body.contains("on page"), "block_text has verbatim metadata (page segment)");
            }
        }
        assert!(found, "highlight found in raw layer");
    }

    #[test]
    fn bom_stripped_from_first_block() {
        // June 1, 2026 is a Monday.
        let bom_clippings = format!(
            "\u{FEFF}{}",
            "\
The Art of War (Sun Tzu)
- Your Highlight at location 10 | Added on Monday, 1 June 2026 12:00:00

All warfare is based on deception.
=========="
        );
        let blocks = parse_clippings(&bom_clippings);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].title, "The Art of War");
        assert_eq!(blocks[0].author, "Sun Tzu");
    }

    #[test]
    fn title_with_parens_in_name_uses_last_parens_as_author() {
        // "Harry Potter and the Goblet of Fire (Book 4) (J. K. Rowling)"
        // — last parens = author.
        let (t, a) = parse_title_author("Harry Potter and the Goblet of Fire (Book 4) (J. K. Rowling)");
        assert_eq!(t, "Harry Potter and the Goblet of Fire (Book 4)");
        assert_eq!(a, "J. K. Rowling");
    }

    #[test]
    fn title_without_author_has_empty_author() {
        let (t, a) = parse_title_author("A Book With No Author");
        assert_eq!(t, "A Book With No Author");
        assert!(a.is_empty());
    }

    #[test]
    fn guid_is_stable_and_unique_per_clipping() {
        let g1 = clipping_guid("Book", "highlight", "142-145", "Thursday, 8 June 2026 20:11:00");
        let g2 = clipping_guid("Book", "highlight", "142-145", "Thursday, 8 June 2026 20:11:00");
        let g3 = clipping_guid("Book", "note", "142-145", "Thursday, 8 June 2026 20:11:00");
        assert_eq!(g1, g2, "same input → same guid");
        assert_ne!(g1, g3, "different kind → different guid");
        assert_eq!(g1.len(), 64, "sha256 hex = 64 chars");

        // Dash normalization: en-dash (U+2013) and ASCII hyphen must produce
        // the same guid — different firmware/locale variants use different dashes
        // for the same location range, so idempotency requires normalization.
        let g_ascii = clipping_guid("Book", "highlight", "429-430", "Monday, 8 June 2026 20:11:00");
        let g_endash = clipping_guid("Book", "highlight", "429\u{2013}430", "Monday, 8 June 2026 20:11:00");
        assert_eq!(g_ascii, g_endash, "en-dash and ASCII hyphen yield the same guid");
    }

    #[test]
    fn three_segment_real_format_location_and_page_parsed() {
        // Verify the dominant real-world line: "on page X | location Y-Z | Added on …"
        // This is the shape that was silently dropped in the original parser.
        let line = "- Your Highlight on page 3 | location 429-430 | Added on Thursday, 8 June 2026 20:11:00";
        let result = parse_metadata(line).expect("three-segment line parses");
        let (kind, location, page, _date_ts, date_raw) = result;
        assert_eq!(kind, "highlight");
        assert_eq!(location, "429-430", "Kindle location range is the primary location");
        assert_eq!(page, "3", "physical page number extracted separately");
        assert_eq!(date_raw, "Thursday, 8 June 2026 20:11:00");
    }

    #[test]
    fn two_segment_at_location_form_still_parses() {
        // Older firmware form: "at location X-Y | Added on …"
        let line = "- Your Highlight at location 142-145 | Added on Monday, 8 June 2026 20:11:00";
        let result = parse_metadata(line).expect("two-segment line parses");
        let (kind, location, page, _date_ts, _date_raw) = result;
        assert_eq!(kind, "highlight");
        assert_eq!(location, "142-145", "location range from two-segment form");
        assert!(page.is_empty(), "no page in two-segment form");
    }

    #[test]
    fn capital_location_keyword_parses() {
        // Some firmware emits "Location" with capital L.
        let line = "- Your Highlight on page 5 | Location 100-105 | Added on Monday, 8 June 2026 20:11:00";
        let result = parse_metadata(line).expect("capital Location parses");
        let (_kind, location, page, _date_ts, _date_raw) = result;
        assert_eq!(location, "100-105", "capital Location keyword handled");
        assert_eq!(page, "5");
    }

    #[test]
    fn date_parses_european_and_us_formats() {
        // European long: "Monday, 8 June 2026 20:11:00"
        // June 8, 2026 is a Monday — weekday must match for chrono %A to work.
        let ts1 = parse_date("Monday, 8 June 2026 20:11:00");
        assert_ne!(ts1, "1970-01-01T00:00:00+00:00", "European long parses");
        let dt1 = chrono::DateTime::parse_from_rfc3339(&ts1).unwrap();
        assert_eq!(dt1.day(), 8);
        assert_eq!(dt1.month(), 6);

        // European — incorrect weekday: "Thursday, 8 June 2026 20:11:00"
        // chrono's %A fails but we fall back to stripping the weekday prefix.
        let ts1b = parse_date("Thursday, 8 June 2026 20:11:00");
        assert_ne!(ts1b, "1970-01-01T00:00:00+00:00", "European wrong weekday still parses via strip");
        let dt1b = chrono::DateTime::parse_from_rfc3339(&ts1b).unwrap();
        assert_eq!(dt1b.day(), 8, "day correct after weekday strip");

        // US with AM/PM: "Sunday, June 7, 2026 9:05:00 AM"
        // June 7, 2026 is a Sunday.
        let ts2 = parse_date("Sunday, June 7, 2026 9:05:00 AM");
        assert_ne!(ts2, "1970-01-01T00:00:00+00:00", "US format parses");
        let dt2 = chrono::DateTime::parse_from_rfc3339(&ts2).unwrap();
        assert_eq!(dt2.day(), 7);

        // US without weekday: "June 8, 2026 8:11:00 AM"
        let ts3 = parse_date("June 8, 2026 8:11:00 AM");
        assert_ne!(ts3, "1970-01-01T00:00:00+00:00", "US no weekday parses");

        // European without weekday: "8 June 2026 20:11:00"
        let ts4 = parse_date("8 June 2026 20:11:00");
        assert_ne!(ts4, "1970-01-01T00:00:00+00:00", "European no weekday parses");
        let dt4 = chrono::DateTime::parse_from_rfc3339(&ts4).unwrap();
        assert_eq!(dt4.day(), 8);
    }

    #[test]
    fn hub_card_shows_import_box() {
        let v = temp_vault("hub");
        let status = v.integrations_status();
        let card = status.iter().find(|s| s.id == "kindle").expect("kindle card");
        let import_info = card.import.as_ref().expect("import box info");
        assert_eq!(import_info.accepts, &["txt"]);
        assert!(card.last_data.is_none(), "no data yet");
        // Import then check last_data.
        import(&v, CLIPPINGS);
        let status2 = v.integrations_status();
        let card2 = status2.iter().find(|s| s.id == "kindle").unwrap();
        assert!(card2.last_data.is_some(), "last_data set after import");
    }
}
