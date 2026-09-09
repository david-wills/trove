//! WhatsApp — import of per-chat .txt exports from the macOS/iPhone app.
//!
//! WhatsApp's only clean data-export path for personal users is a per-chat
//! .txt file (optionally zipped with media). The full message database lives
//! on-phone behind E2EE and is not accessible from the Mac desktop.
//!
//! ## Export format
//!
//! The .txt format is **locale-variant and community-characterised** (not
//! formally documented by Meta). Two main shapes are known:
//!
//! **Bracketed** (iOS / macOS, most common for users exporting from Mac):
//! ```text
//! [DD/MM/YYYY, HH:MM:SS] Author Name: message text
//! [DD/MM/YYYY, HH:MM:SS] Author Name: <Media omitted>
//! [DD/MM/YYYY, HH:MM:SS] Author Name: ‎<attached: photo.jpg>
//! [DD/MM/YYYY, HH:MM:SS] System event text (no ": " after the name)
//! ```
//!
//! iOS .zip exports always name the inner text file `_chat.txt` regardless
//! of the actual chat name.  The chat name is therefore derived from the
//! outer .zip filename, not from the inner text-file stem.
//!
//! **Unbracketed** (Android, some locales):
//! ```text
//! M/D/YY, H:MM AM - Author Name: message text
//! DD.MM.YYYY, HH:MM - Author Name: <Media omitted>
//! DD.MM.YYYY, HH:MM - Author Name: filename.jpg (file attached)
//! ```
//!
//! iOS bracketed timestamps may use a **space** instead of ", " between date
//! and time in some locale/version combinations:
//! ```text
//! [3/6/18 1:55:00 PM] Author: text
//! ```
//!
//! Lines may be prefixed with U+200E (LRM) and/or U+200F (RLM) control
//! characters; these are stripped before classification.
//!
//! The first line is always a system header: "Messages and calls are end-to-end
//! encrypted…" (English) — locale variants of this string exist.
//!
//! Multi-line messages appear as continuation lines with no timestamp prefix.
//!
//! Call lines appear as system events (the `: Author` separator is absent and
//! the text matches well-known call-event strings — whole-line exact match,
//! never substring). English variants known: "WhatsApp Call", "Voice call",
//! "Video call", "Missed voice call", "Missed video call". Non-English
//! variants need real samples — hence the `Needs-sample` flag.
//!
//! ## Needs-sample
//!
//! The real export parser is built for documented English formats. Non-English
//! timestamp formats (different date orders, RTL scripts, localised call/media
//! strings) require real sample exports for validation. The parser is built
//! but marked as needing verification against non-English real exports.
//!
//! ## Dedupe
//!
//! WhatsApp exports have **no native message IDs**. The guid is a SHA-256
//! content hash of `chat\0ts_minute\0author\0text` (first 16 hex chars),
//! where `ts_minute` is the timestamp truncated to minute precision
//! (YYYY-MM-DDTHH:MM).  Truncating to minutes means the same logical message
//! exported once with seconds (iOS) and once without (Android 24h) produces
//! the same guid, avoiding full-history duplication on re-import across
//! formats.
//!
//! Documented residual weakness: two identical messages by the same author
//! in the same minute collide — acceptable given the export's inherent
//! lossiness.
//!
//! ## Calls
//!
//! Call history is inaccessible from macOS — it remains on-phone behind E2EE.
//! Any call event lines that appear in the .txt (some are included by WhatsApp)
//! are stored as `kind:"call"` best-effort; duration is not available.
//! Call events are system lines (no author separator) whose body exactly
//! matches a known call string; regular authored messages are never
//! misclassified as calls.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use sha2::{Digest, Sha256};

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::vault::Vault;

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_mtime(&vault.root().join("correspondence/whatsapp"))
}

fn run_import(
    vault: &Vault,
    path: &Path,
    params: &std::collections::BTreeMap<String, String>,
    _progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    let me = params.get("me").map(String::as_str).map(str::trim).filter(|s| !s.is_empty());
    let s = vault.import_whatsapp_export(path, me)?;
    Ok(ImportOutcome {
        headline: format!(
            "{} messages imported from {} chats, {} duplicates skipped",
            s.imported, s.chats, s.duplicates
        ),
        counts: [
            ("imported", s.imported),
            ("duplicates", s.duplicates),
            ("chats", s.chats as u64),
        ]
        .into(),
    })
}

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["txt", "zip"],
    params: &[crate::registry::ImportParam {
        key: "me",
        label: "Your display name in WhatsApp",
        placeholder: "name as it appears in your exported chats (optional)",
        required: false,
    }],
    run: run_import,
};

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "whatsapp",
        name: "WhatsApp",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Import WhatsApp chat history from per-chat .txt exports. \
                      Drop one or more .txt files or a .zip with media. \
                      Re-runnable — re-importing the same chat never duplicates messages.",
        domain: "correspondence",
        vault_path: "correspondence/whatsapp/",
        toggleable: false,
        setup: &[
            "iPhone/Mac: open WhatsApp → any chat → … → Export Chat → Without Media \
             (or With Media for a .zip). One .txt or .zip per chat.",
            "Drop the file(s) here. Enter your display name so your own messages are marked \
             as sent (optional but recommended).",
        ],
        caveats: "WhatsApp call history can't be collected — it lives on-phone behind \
                  end-to-end encryption with no export path. Any call event lines that \
                  appear in a chat export are stored best-effort as kind:call with no \
                  duration. Non-English timestamp formats may need a manual format \
                  hint — collect real exports to improve locale coverage.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

/// Result of one export import pass, returned to the UI headline.
#[derive(Debug, Clone)]
pub struct WhatsAppImportStats {
    pub imported: u64,
    pub duplicates: u64,
    pub chats: u32,
}

// ---------------------------------------------------------------------------
// Timestamp parsing — the locale-variant heart of the importer
// ---------------------------------------------------------------------------

/// Strip leading Unicode directional marks (LRM U+200E, RLM U+200F, BOM U+FEFF)
/// from a string slice.  These appear at the start of lines and inside bodies
/// in real WhatsApp exports across multiple locales.
fn strip_leading_marks(s: &str) -> &str {
    let mut slice = s;
    // Iteratively strip known prefixes (each is 3 bytes in UTF-8).
    loop {
        if let Some(rest) = slice
            .strip_prefix('\u{200e}')
            .or_else(|| slice.strip_prefix('\u{200f}'))
            .or_else(|| slice.strip_prefix('\u{feff}'))
        {
            slice = rest;
        } else {
            break;
        }
    }
    slice
}

/// Try to parse a WhatsApp timestamp string into a local [`DateTime`].
///
/// Handles the two main bracket / no-bracket families and multiple date orders.
/// Returns `None` when the string doesn't match any known pattern.
///
/// Known patterns (English iOS/Android — non-English **needs-sample**):
/// - `[DD/MM/YYYY, HH:MM:SS]` — iOS bracketed (most common on macOS export)
/// - `[DD/MM/YY, HH:MM:SS]`
/// - `[M/D/YYYY, H:MM:SS AM]` — bracketed 12h
/// - `[3/6/18 1:55:00 PM]`    — iOS space-separator (no comma) variant
/// - `M/D/YY, H:MM AM`        — Android 12h unbracketed
/// - `DD.MM.YYYY, HH:MM`      — Android EU 24h unbracketed
/// - `DD/MM/YYYY, HH:MM`      — Android EU / — unbracketed 24h
pub(crate) fn parse_wa_timestamp(ts: &str) -> Option<DateTime<Local>> {
    // Strip directional marks, then strip brackets.
    let ts = strip_leading_marks(ts.trim());
    let ts = ts.trim().trim_matches('[').trim_end_matches(']').trim();

    // Split into date+time portions.
    // Accept both ", " (canonical) and " " (iOS space-separator variant) between date and time.
    // We try ", " first (most common), then fall back to space-only separation.
    let (date_part, time_part) = if let Some(p) = ts.split_once(", ") {
        p
    } else {
        // Space-only separator: date is everything up to the first space that is followed
        // by a digit (the time).  Find the last space before the time component.
        // Strategy: split on the FIRST space, then check if what follows looks like a time.
        // For "[3/6/18 1:55:00 PM]" → date="3/6/18", time="1:55:00 PM"
        if let Some(space_pos) = ts.find(' ') {
            let candidate_date = &ts[..space_pos];
            let candidate_time = &ts[space_pos + 1..];
            // Validate date part has a numeric separator.
            if candidate_date.contains('/')
                || candidate_date.contains('.')
                || candidate_date.contains('-')
            {
                (candidate_date, candidate_time)
            } else {
                return None;
            }
        } else {
            return None;
        }
    };

    // Detect AM/PM (handle e.g. "p. m." from Spanish locale by normalising first).
    let time_normalised = time_part.replace("p. m.", "PM").replace("a. m.", "AM");
    let time_up = time_normalised.to_ascii_uppercase();
    let has_ampm = time_up.ends_with("AM") || time_up.ends_with("PM");
    let is_pm = time_up.ends_with("PM");

    // Strip the AM/PM suffix for numeric parsing.
    let time_clean = if has_ampm {
        time_normalised
            .trim_end_matches("AM")
            .trim_end_matches("PM")
            .trim_end_matches("am")
            .trim_end_matches("pm")
            .trim()
            .to_string()
    } else {
        time_part.trim().to_string()
    };

    // Parse time: "HH:MM:SS" or "H:MM:SS" or "HH:MM" or "H:MM".
    let (hour, min, sec) = parse_time_parts(&time_clean)?;
    let hour = adjust_ampm(hour, has_ampm, is_pm);

    // Parse date: try "/" separator then "." separator then "-".
    let sep = if date_part.contains('/') {
        '/'
    } else if date_part.contains('.') {
        '.'
    } else if date_part.contains('-') {
        '-'
    } else {
        return None;
    };

    let parts: Vec<&str> = date_part.split(sep).collect();
    if parts.len() != 3 {
        return None;
    }

    let (day, month, year) = infer_date_order(parts[0], parts[1], parts[2])?;

    let ndt = NaiveDateTime::new(
        chrono::NaiveDate::from_ymd_opt(year, month, day)?,
        chrono::NaiveTime::from_hms_opt(hour, min, sec)?,
    );
    Local.from_local_datetime(&ndt).single()
}

fn parse_time_parts(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [h, m] => Some((h.trim().parse().ok()?, m.trim().parse().ok()?, 0)),
        [h, m, sec] => {
            // Seconds may have trailing non-digits (e.g. " AM" already stripped,
            // but guard anyway).
            let s: u32 = sec.trim().parse().ok()?;
            Some((h.trim().parse().ok()?, m.trim().parse().ok()?, s))
        }
        _ => None,
    }
}

fn adjust_ampm(hour: u32, has_ampm: bool, is_pm: bool) -> u32 {
    if !has_ampm {
        return hour;
    }
    match (hour, is_pm) {
        (12, false) => 0,  // 12 AM -> 0
        (12, true) => 12,  // 12 PM -> 12
        (h, true) => h + 12,
        (h, false) => h,
    }
}

/// Infer (day, month, year) from three string parts and the ordering convention.
///
/// Returns `(day, month, year)` where year is an `i32` (as required by chrono).
///
/// Ordering heuristics (applied in order):
/// - If `a.len() == 4` → YYYY/MM/DD
/// - If `c.len() == 4` → last slot is 4-digit year; determine D/M vs M/D from values
/// - 2-digit year in last slot (`c.len() < 4`): same D/M vs M/D heuristic
///
/// D/M vs M/D disambiguation:
/// - If `bv > 12` → `b` is the day, so order is M/D (US)
/// - Else if `av > 12` → `a` is the day, so order is D/M (EU)
/// - Else ambiguous — default to D/M (more common globally)
///
/// For 2-digit years: 00-49 = 2000s, 50-99 = 1900s.
fn infer_date_order(a: &str, b: &str, c: &str) -> Option<(u32, u32, i32)> {
    let av: u32 = a.trim().parse().ok()?;
    let bv: u32 = b.trim().parse().ok()?;
    let cv: u32 = c.trim().parse().ok()?;

    // Which slot is the year?
    let (first, second, year_raw): (u32, u32, u32) = if a.len() == 4 {
        // YYYY/MM/DD
        return Some((cv, bv, if av >= 100 { av as i32 } else { 2000 + av as i32 }));
    } else if c.len() == 4 {
        (av, bv, cv)
    } else {
        // 2-digit year in last slot.
        (av, bv, cv)
    };

    // D/M vs M/D disambiguation.
    let (day, month) = if second > 12 {
        // b (second) > 12 → must be the day → M/D order (US Android)
        (second, first)
    } else if first > 12 {
        // a (first) > 12 → must be the day → D/M order (EU)
        (first, second)
    } else {
        // Ambiguous: default D/M (day first, more common globally)
        (first, second)
    };

    let year: i32 = if year_raw < 100 {
        if year_raw < 50 { (2000 + year_raw) as i32 } else { (1900 + year_raw) as i32 }
    } else {
        year_raw as i32
    };

    if month < 1 || month > 12 || day < 1 || day > 31 {
        return None;
    }
    Some((day, month, year))
}

// ---------------------------------------------------------------------------
// Line classification
// ---------------------------------------------------------------------------

/// English-language strings that appear as the **complete** text body of call
/// system events (exact whole-line match only — never substring search).
/// Non-English equivalents need real samples — hence Needs-sample.
///
/// IMPORTANT: these are matched case-insensitively against system lines only
/// (lines where the author separator ": " is absent).  Regular authored
/// messages that happen to contain these words are never classified as calls.
const CALL_BODY_EN: &[&str] = &[
    "WhatsApp Call",
    "Voice call",
    "Video call",
    "Missed voice call",
    "Missed video call",
    "Missed WhatsApp Call",
    "WhatsApp Video",
    "Call, no answer",
    "Video call, no answer",
];

/// English-language strings for media omitted / attachment lines.
const MEDIA_BODY_EN: &[&str] = &[
    "<Media omitted>",
    "image omitted",
    "video omitted",
    "audio omitted",
    "document omitted",
    "sticker omitted",
    "Contact card omitted",
    "GIF omitted",
];

/// Determine whether a system-event body is a call event (English).
///
/// Only called for system lines (author == None).  Uses whole-line exact
/// case-insensitive match — never a substring search — to avoid false
/// positives on regular authored text.
fn is_call_body(text: &str) -> bool {
    let t = text.trim();
    CALL_BODY_EN.iter().any(|s| t.eq_ignore_ascii_case(s))
}

/// Strip leading directional marks from a body string and return owned String.
fn clean_body(s: &str) -> String {
    strip_leading_marks(s).to_string()
}

/// If the text is an attachment / media line, return the attachment metadata.
///
/// Handles:
/// - `‎<attached: filename.jpg>` — iOS explicit attachment (may have LRM prefix)
/// - `<Media omitted>` and locale-variant "omitted" strings
/// - `filename.jpg (file attached)` — Android form
/// - `filename.jpg <attached>` — Android alternate form
///
/// Returns `None` if the line is a regular text message.
fn attachment_from_body(text: &str) -> Option<AttachmentMeta> {
    // Strip leading directional marks before classification.
    let t = strip_leading_marks(text.trim());

    // iOS "<attached: name>" — explicit attachment (may have LRM prefix stripped above).
    if t.starts_with("<attached:") && t.ends_with('>') {
        let inner = t[10..t.len() - 1].trim();
        return Some(AttachmentMeta {
            name: inner.to_string(),
            mime: String::new(),
            bytes: 0,
        });
    }

    // "<Media omitted>" and locale equivalents.
    if MEDIA_BODY_EN.iter().any(|s| t.eq_ignore_ascii_case(s)) {
        return Some(AttachmentMeta {
            name: String::new(),
            mime: String::new(),
            bytes: 0,
        });
    }

    // Android form: "filename.ext (file attached)" or "filename.ext <attached>"
    // Pattern: ends with " (file attached)" or " <attached>"
    if let Some(name) = t.strip_suffix(" (file attached)") {
        let name = name.trim();
        if !name.is_empty() && name.contains('.') {
            return Some(AttachmentMeta {
                name: name.to_string(),
                mime: String::new(),
                bytes: 0,
            });
        }
    }
    if let Some(name) = t.strip_suffix(" <attached>") {
        let name = name.trim();
        if !name.is_empty() && name.contains('.') {
            return Some(AttachmentMeta {
                name: name.to_string(),
                mime: String::new(),
                bytes: 0,
            });
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Line parser
// ---------------------------------------------------------------------------

/// A parsed line from the .txt export.
#[derive(Debug)]
enum WaLine {
    /// A new message: (timestamp_str, author_or_none, body).
    /// `author` is None for system-event lines (no `: ` separator).
    Message { ts: String, author: Option<String>, body: String },
    /// A continuation of the previous message (no timestamp).
    Continuation(String),
    /// The file header / end-to-end encryption notice — skip.
    Header,
    /// Empty or unparseable.
    Blank,
}

/// Detect whether a line starts with a WhatsApp timestamp.
///
/// Returns `(ts_str, rest)` where `ts_str` is the raw timestamp text
/// (without brackets) and `rest` is everything after the closing `] ` or ` - `.
///
/// Strips leading LRM (U+200E), RLM (U+200F), and BOM (U+FEFF) characters
/// before attempting to match.
fn extract_timestamp(line: &str) -> Option<(String, &str)> {
    // Strip all leading directional marks (LRM, RLM, BOM).
    let line = strip_leading_marks(line);

    if let Some(line) = line.strip_prefix('[') {
        // Bracketed: [DD/MM/YYYY, HH:MM:SS] rest  or  [3/6/18 1:55:00 PM] rest
        let close = line.find(']')?;
        let ts = &line[..close];
        let rest = line[close + 1..].trim_start_matches(' ');
        // Validate that it looks like a timestamp (has numeric separator chars).
        if ts.contains('/') || ts.contains('.') || ts.contains('-') {
            // Confirm parse succeeds.
            if parse_wa_timestamp(ts).is_some() {
                return Some((ts.to_string(), rest));
            }
        }
        return None;
    }

    // Unbracketed: "M/D/YY, H:MM AM - rest" or "DD.MM.YYYY, HH:MM - rest"
    // Find the " - " separator.
    if let Some(dash_pos) = line.find(" - ") {
        let candidate = &line[..dash_pos];
        // Must have a date-like separator and at least one digit.
        if (candidate.contains(", ") || candidate.contains('/') || candidate.contains('.'))
            && candidate.chars().any(|c| c.is_ascii_digit())
        {
            // Validate parse to confirm it's a timestamp (not a stray " - " in text).
            if parse_wa_timestamp(candidate).is_some() {
                return Some((candidate.to_string(), &line[dash_pos + 3..]));
            }
        }
    }

    None
}

fn parse_line(line: &str) -> WaLine {
    let line = line.trim_end();
    if line.is_empty() {
        return WaLine::Blank;
    }

    let Some((ts, rest)) = extract_timestamp(line) else {
        // No timestamp: continuation or blank.
        if line.trim().is_empty() {
            return WaLine::Blank;
        }
        return WaLine::Continuation(line.trim_end().to_string());
    };

    // After the timestamp, bracketed format has "- Author: text" or just system text.
    // Unbracketed format already strips " - "; bracketed has "- " prefix remaining.
    let rest = if rest.starts_with("- ") {
        &rest[2..]
    } else {
        rest
    };

    // Detect the "Messages and calls are end-to-end encrypted" header line.
    if rest.to_ascii_lowercase().contains("end-to-end encrypted")
        || rest.to_ascii_lowercase().contains("end to end encrypted")
    {
        return WaLine::Header;
    }

    // Split author from body: first ": " is the separator.
    // System events (e.g. "You were added", "Group created") have no ": ".
    match rest.find(": ") {
        Some(colon) => {
            let author = rest[..colon].to_string();
            let body = rest[colon + 2..].to_string();
            WaLine::Message { ts, author: Some(author), body }
        }
        None => WaLine::Message { ts, author: None, body: rest.to_string() },
    }
}

// ---------------------------------------------------------------------------
// Content-hash guid (no native IDs in WhatsApp exports)
// ---------------------------------------------------------------------------

/// First 16 hex chars of SHA-256(chat + NUL + ts_minute + NUL + author + NUL + text).
///
/// `ts_minute` is the RFC 3339 timestamp truncated to minute precision
/// ("YYYY-MM-DDTHH:MM") so the same logical message exported with seconds
/// (iOS) and without (Android) produces the same guid.
fn content_guid(chat: &str, ts_minute: &str, author: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(chat.as_bytes());
    h.update(b"\x00");
    h.update(ts_minute.as_bytes());
    h.update(b"\x00");
    h.update(author.as_bytes());
    h.update(b"\x00");
    h.update(text.as_bytes());
    let d = h.finalize();
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Truncate an RFC 3339 timestamp to minute precision for use in the guid.
/// "2026-06-18T09:15:30+10:00" → "2026-06-18T09:15"
fn ts_to_minute(rfc3339: &str) -> String {
    // The format is always at least "YYYY-MM-DDTHH:MM" (16 chars) for valid RFC 3339.
    // Find the 'T' and take up through the first ':MM' after it.
    if let Some(t_pos) = rfc3339.find('T') {
        let after_t = &rfc3339[t_pos + 1..];
        // after_t starts with "HH:MM" (at least 5 chars).
        if after_t.len() >= 5 {
            return format!("{}T{}", &rfc3339[..t_pos], &after_t[..5]);
        }
    }
    // Fallback: return as-is (shouldn't happen for valid timestamps).
    rfc3339.to_string()
}

// ---------------------------------------------------------------------------
// Chat-name derivation helpers
// ---------------------------------------------------------------------------

/// Derive a chat name from a file path (a .txt or .zip filename).
///
/// Strips well-known WhatsApp export prefixes:
/// - "WhatsApp Chat with <name>"
/// - "WhatsApp Chat - <name>"   (alternate form used in some exports)
/// - "WhatsApp Chat <name>"     (no separator)
///
/// Falls back to the raw stem if no prefix matches.
fn chat_name_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "WhatsApp Chat".to_string());
    strip_chat_prefix(&stem)
}

/// Strip WhatsApp export filename prefixes from a stem string.
fn strip_chat_prefix(stem: &str) -> String {
    for prefix in &[
        "WhatsApp Chat with ",
        "WhatsApp Chat - ",
        "WhatsApp Chat with: ",
    ] {
        if let Some(rest) = stem.strip_prefix(prefix) {
            let r = rest.trim();
            if !r.is_empty() {
                return r.to_string();
            }
        }
    }
    stem.to_string()
}

// ---------------------------------------------------------------------------
// The main import
// ---------------------------------------------------------------------------

impl Vault {
    /// Import a WhatsApp per-chat .txt export (or a .zip containing it).
    ///
    /// Re-runnable: content-hash guids already in vault are skipped.
    /// `me` is your WhatsApp display name — used to mark `from_me`.
    pub fn import_whatsapp_export(
        &self,
        path: &Path,
        me: Option<&str>,
    ) -> Result<WhatsAppImportStats> {
        let ext = path.extension().map(|e| e.to_ascii_lowercase());
        if ext.as_deref() == Some(std::ffi::OsStr::new("zip")) {
            self.import_whatsapp_zip(path, me)
        } else {
            // Single .txt file — derive chat name from filename.
            let chat_name = chat_name_from_path(path);
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let mut seen = self.correspondence_guids("whatsapp")?;
            let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
            let mut batch: Vec<Message> = Vec::new();
            self.parse_whatsapp_txt(&body, &chat_name, me, &mut seen, &mut batch, &mut stats);
            self.append_messages(&batch)?;
            if stats.imported + stats.duplicates > 0 {
                stats.chats = 1;
            }
            Ok(stats)
        }
    }

    fn import_whatsapp_zip(
        &self,
        path: &Path,
        me: Option<&str>,
    ) -> Result<WhatsAppImportStats> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut zip = zip::ZipArchive::new(file)
            .with_context(|| format!("reading {}", path.display()))?;
        let mut seen = self.correspondence_guids("whatsapp")?;
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch: Vec<Message> = Vec::new();

        // Derive the chat name from the outer .zip filename, not the inner text
        // file stem.  iOS .zip exports always name the inner file "_chat.txt"
        // regardless of chat — the zip itself carries the chat name.
        let zip_chat_name = chat_name_from_path(path);

        // Find the .txt file(s) in the zip (skip media).
        let mut txt_entries: Vec<(String, String)> = Vec::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i)?;
            let name = entry.name().to_string();
            if name.ends_with(".txt") || name.to_ascii_lowercase().ends_with(".txt") {
                let mut body = String::new();
                entry.read_to_string(&mut body)?;
                txt_entries.push((name, body));
            }
        }

        for (inner_name, body) in txt_entries {
            // Determine the chat name:
            // - If the inner file is "_chat.txt" (iOS canonical name), use the zip
            //   filename as the chat key.
            // - If the inner file has a meaningful name (e.g. multi-chat zips from
            //   some Android backup tools), derive from the inner file stem.
            let inner_stem = std::path::Path::new(&inner_name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();

            let chat_name = if inner_stem == "_chat" || inner_stem.is_empty() {
                // iOS _chat.txt — use the zip's chat name.
                zip_chat_name.clone()
            } else {
                // Inner file has a meaningful stem — derive from it.
                strip_chat_prefix(&inner_stem)
            };

            let before = stats.imported + stats.duplicates;
            self.parse_whatsapp_txt(&body, &chat_name, me, &mut seen, &mut batch, &mut stats);
            if stats.imported + stats.duplicates > before {
                stats.chats += 1;
            }
        }

        self.append_messages(&batch)?;
        Ok(stats)
    }

    /// Parse a single WhatsApp .txt body and extend `batch` / `stats`.
    fn parse_whatsapp_txt(
        &self,
        body: &str,
        chat: &str,
        me: Option<&str>,
        seen: &mut HashSet<String>,
        batch: &mut Vec<Message>,
        stats: &mut WhatsAppImportStats,
    ) {
        let mut pending: Option<PendingMsg> = None;

        for raw_line in body.lines() {
            match parse_line(raw_line) {
                WaLine::Header | WaLine::Blank => {
                    // flush current pending if any
                }
                WaLine::Continuation(cont) => {
                    if let Some(ref mut p) = pending {
                        p.body.push('\n');
                        p.body.push_str(&cont);
                    }
                    continue;
                }
                WaLine::Message { ts, author, body: body_text } => {
                    // Flush previous pending message.
                    if let Some(p) = pending.take() {
                        self.flush_pending(p, chat, me, seen, batch, stats);
                    }
                    pending = Some(PendingMsg { ts, author, body: body_text });
                    continue;
                }
            }
            // After Header/Blank, flush pending if any.
            if let Some(p) = pending.take() {
                self.flush_pending(p, chat, me, seen, batch, stats);
            }
        }

        // Final flush.
        if let Some(p) = pending.take() {
            self.flush_pending(p, chat, me, seen, batch, stats);
        }

        // Flush batch in chunks to keep memory bounded.
        if batch.len() >= 2000 {
            let _ = self.append_messages(batch);
            batch.clear();
        }
    }

    fn flush_pending(
        &self,
        p: PendingMsg,
        chat: &str,
        me: Option<&str>,
        seen: &mut HashSet<String>,
        batch: &mut Vec<Message>,
        stats: &mut WhatsAppImportStats,
    ) {
        let local = match parse_wa_timestamp(&p.ts) {
            Some(t) => t,
            None => return, // unparseable timestamp — skip
        };
        let ts_rfc3339 = local.to_rfc3339();
        // Use minute-precision timestamp in the guid to make iOS (with seconds)
        // and Android (without seconds) exports produce the same guid for the
        // same logical message.
        let ts_minute = ts_to_minute(&ts_rfc3339);

        let author_str = p.author.as_deref().unwrap_or("system");
        // Strip leading directional marks from the body before hashing and storing.
        let body_clean = clean_body(&p.body);
        let guid = content_guid(chat, &ts_minute, author_str, &body_clean);

        if !seen.insert(guid.clone()) {
            stats.duplicates += 1;
            return;
        }

        let mut m = Message::new("whatsapp", ts_rfc3339);
        m.guid = guid;
        m.chat = chat.to_string();
        m.service = "WhatsApp".to_string();

        match &p.author {
            None => {
                // System event — check for call body (exact whole-line match only).
                if is_call_body(&body_clean) {
                    m.kind = "call".to_string();
                } else {
                    m.kind = "event".to_string();
                }
                m.sender = "system".to_string();
                m.text = body_clean.trim().to_string();
            }
            Some(author) => {
                m.from_me = me.map(|n| n.eq_ignore_ascii_case(author)).unwrap_or(false);
                if !m.from_me {
                    m.sender = author.clone();
                    m.sender_name = author.clone();
                }

                // Authored messages: never call — classification is for system lines only.
                if let Some(att) = attachment_from_body(&body_clean) {
                    m.attachments = vec![att];
                    // No text body for pure-media lines.
                } else {
                    m.text = body_clean.trim_end().to_string();
                }
            }
        }

        // Skip empty non-call, non-attachment messages.
        if m.kind == "message" && m.text.is_empty() && m.attachments.is_empty() {
            return;
        }

        batch.push(m);
        stats.imported += 1;
    }
}

/// A collected but not-yet-flushed message (waiting for continuation lines).
struct PendingMsg {
    ts: String,
    author: Option<String>,
    body: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_vault(tag: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-whatsapp-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // -----------------------------------------------------------------------
    // Fixture helpers — built from documented English format knowledge.
    // Real-locale fixtures (non-English) require Needs-sample.
    // -----------------------------------------------------------------------

    /// iOS bracketed format, 24h, DD/MM/YYYY (the most common macOS export).
    fn fixture_ios_bracketed() -> &'static str {
        "[18/06/2026, 09:15:30] Alice: Hey, are you free tomorrow?\n\
         [18/06/2026, 09:16:00] Bob: Yes! Let's meet at noon\ncontinued on second line\n\
         [18/06/2026, 09:17:10] Alice: \u{200e}<attached: photo.jpg>\n\
         [18/06/2026, 09:18:00] Alice: \u{200e}image omitted\n\
         [18/06/2026, 09:19:00] Alice: IMG-20260618-WA0001.jpg (file attached)\n\
         [18/06/2026, 09:20:00] Missed voice call\n\
         [18/06/2026, 09:25:00] Voice call\n\
         [18/06/2026, 09:30:00] You created group \"Team Chat\"\n"
    }

    /// Android 12h unbracketed (US locale).
    fn fixture_android_12h() -> &'static str {
        "6/18/26, 9:15 AM - Alice: Morning!\n\
         6/18/26, 9:16 AM - Bob: Hi there\n\
         6/18/26, 9:17 AM - Alice: <Media omitted>\n\
         6/18/26, 9:18 AM - Missed voice call\n"
    }

    /// Android EU 24h unbracketed (European locale, day.month.year).
    fn fixture_android_eu_24h() -> &'static str {
        "18.06.2026, 09:15 - Alice: Guten Morgen!\n\
         18.06.2026, 09:16 - Bob: Hallo!\n\
         18.06.2026, 09:20 - Missed video call\n"
    }

    /// iOS space-separator timestamps (no comma between date and time).
    /// Observed in some iOS versions/locales.
    fn fixture_ios_space_sep() -> &'static str {
        "[3/6/18 1:55:00 PM] Alice: Hello from space-sep format\n\
         [3/6/18 2:00:00 PM] Bob: Works!\n"
    }

    /// RTL/LRM-prefixed lines (simulates iOS export with directional marks).
    fn fixture_rtl_lrm() -> &'static str {
        "\u{200e}[18/06/2026, 10:00:00] Alice: LRM prefixed line\n\
         \u{200f}[18/06/2026, 10:01:00] Bob: RLM prefixed line\n\
         [18/06/2026, 10:02:00] Carol: Normal line\n"
    }

    // -----------------------------------------------------------------------
    // Timestamp parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn ts_ios_bracketed() {
        let t = parse_wa_timestamp("[18/06/2026, 09:15:30]").unwrap();
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-18");
        assert_eq!(t.format("%H:%M:%S").to_string(), "09:15:30");
    }

    #[test]
    fn ts_android_12h_am() {
        let t = parse_wa_timestamp("6/18/26, 9:15 AM").unwrap();
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-18");
        assert_eq!(t.format("%H:%M").to_string(), "09:15");
    }

    #[test]
    fn ts_android_12h_pm() {
        let t = parse_wa_timestamp("6/18/26, 1:30 PM").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "13:30");
    }

    #[test]
    fn ts_android_12h_midnight() {
        // 12 AM = midnight = 00:xx
        let t = parse_wa_timestamp("6/18/26, 12:00 AM").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "00:00");
    }

    #[test]
    fn ts_android_12h_noon() {
        // 12 PM = noon = 12:xx
        let t = parse_wa_timestamp("6/18/26, 12:00 PM").unwrap();
        assert_eq!(t.format("%H:%M").to_string(), "12:00");
    }

    #[test]
    fn ts_android_eu_24h() {
        let t = parse_wa_timestamp("18.06.2026, 09:15").unwrap();
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-18");
        assert_eq!(t.format("%H:%M").to_string(), "09:15");
    }

    #[test]
    fn ts_eu_slash_24h() {
        let t = parse_wa_timestamp("18/06/2026, 09:15").unwrap();
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-18");
    }

    #[test]
    fn ts_ios_space_separator() {
        // "[3/6/18 1:55:00 PM]" — iOS variant with space not comma between date and time.
        let t = parse_wa_timestamp("[3/6/18 1:55:00 PM]").unwrap();
        // Day=3, Month=6, Year=2018, time=13:55:00.
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2018-06-03");
        assert_eq!(t.format("%H:%M:%S").to_string(), "13:55:00");
    }

    #[test]
    fn ts_lrm_prefix_stripped() {
        // U+200E prefix on timestamp string — must still parse.
        let ts_with_lrm = format!("\u{200e}[18/06/2026, 09:15:30]");
        let t = parse_wa_timestamp(&ts_with_lrm).unwrap();
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-18");
    }

    #[test]
    fn ts_rlm_prefix_stripped() {
        // U+200F (RLM) prefix — must still parse.
        let ts_with_rlm = format!("\u{200f}[18/06/2026, 09:15:30]");
        let t = parse_wa_timestamp(&ts_with_rlm).unwrap();
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-18");
    }

    #[test]
    fn ts_garbage_returns_none() {
        assert!(parse_wa_timestamp("not a timestamp").is_none());
        assert!(parse_wa_timestamp("").is_none());
        assert!(parse_wa_timestamp("foo, bar").is_none());
    }

    // -----------------------------------------------------------------------
    // Line classification tests
    // -----------------------------------------------------------------------

    #[test]
    fn call_body_detected_system_only() {
        // Exact matches for known call strings.
        assert!(is_call_body("Voice call"));
        assert!(is_call_body("Missed voice call"));
        assert!(is_call_body("Video call"));
        assert!(is_call_body("WhatsApp Call"));
        assert!(is_call_body("Call, no answer"));
        // Regular messages containing call-related words must NOT trigger is_call_body.
        // (is_call_body is only called for system lines, but test the function itself.)
        assert!(!is_call_body("Let's do a Video call later about the project"));
        assert!(!is_call_body("sorry no answer from the vendor yet"));
        assert!(!is_call_body("Hey, wanna call?"));
        assert!(!is_call_body(""));
        assert!(!is_call_body("Not answered"));   // bare "Not answered" is no longer in list
        assert!(!is_call_body("no answer"));       // bare "no answer" removed from list
    }

    #[test]
    fn authored_messages_never_classified_as_call() {
        // In flush_pending, is_call_body is only called for system lines (author == None).
        // Authored messages go through attachment_from_body / text path only.
        // This test verifies the full pipeline: authored lines with call phrases
        // are stored as kind:message, not kind:call.
        let v = temp_vault("no-call-false-positive");
        let body = "[18/06/2026, 09:00:00] Alice: Let's do a Video call later\n\
                    [18/06/2026, 09:01:00] Bob: sorry no answer from the vendor\n\
                    [18/06/2026, 09:02:00] Missed voice call\n";
        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(body, "Test", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        let month = v.read_correspondence_month("whatsapp", "2026-06").unwrap();
        let calls: Vec<_> = month.iter().filter(|m| m.kind == "call").collect();
        // Only the system "Missed voice call" line should be a call.
        assert_eq!(calls.len(), 1, "only the system call line should be kind:call");
        assert!(calls[0].text.contains("Missed voice call"));

        let msgs: Vec<_> = month.iter().filter(|m| m.kind == "message").collect();
        assert_eq!(msgs.len(), 2, "two authored messages must be kind:message");
    }

    #[test]
    fn attachment_body_parsed() {
        // iOS form with LRM prefix.
        let a = attachment_from_body("\u{200e}<attached: document.pdf>").unwrap();
        assert_eq!(a.name, "document.pdf");

        // iOS form without prefix.
        let b = attachment_from_body("<attached: photo.jpg>").unwrap();
        assert_eq!(b.name, "photo.jpg");

        // Media omitted.
        let c = attachment_from_body("\u{200e}image omitted").unwrap();
        assert_eq!(c.name, "");

        let d = attachment_from_body("<Media omitted>").unwrap();
        assert_eq!(d.name, "");

        // Android "(file attached)" form.
        let e = attachment_from_body("IMG-20260618-WA0001.jpg (file attached)").unwrap();
        assert_eq!(e.name, "IMG-20260618-WA0001.jpg");

        // Android "<attached>" form.
        let f = attachment_from_body("file.jpg <attached>").unwrap();
        assert_eq!(f.name, "file.jpg");

        // Regular text — must return None.
        assert!(attachment_from_body("Hello").is_none());
        assert!(attachment_from_body("no answer from the vendor").is_none());
    }

    #[test]
    fn guid_stable_and_unique() {
        let g1 = content_guid("Alice", "2026-06-18T09:15", "Bob", "Hello");
        let g2 = content_guid("Alice", "2026-06-18T09:15", "Bob", "Hello");
        let g3 = content_guid("Alice", "2026-06-18T09:15", "Bob", "Different");
        assert_eq!(g1, g2, "same inputs → same guid");
        assert_ne!(g1, g3, "different text → different guid");
        assert_eq!(g1.len(), 16, "16 hex chars");
    }

    #[test]
    fn guid_minute_normalisation() {
        // iOS (with seconds) and Android (without seconds) on the same minute
        // must produce the same guid after ts_to_minute normalisation.
        let ts_ios = "2026-06-18T09:15:30+10:00";
        let ts_android = "2026-06-18T09:15:00+10:00";
        let g_ios = content_guid("Alice", &ts_to_minute(ts_ios), "Bob", "Hello");
        let g_android = content_guid("Alice", &ts_to_minute(ts_android), "Bob", "Hello");
        assert_eq!(g_ios, g_android, "same logical minute → same guid across formats");
    }

    #[test]
    fn ts_to_minute_strips_seconds() {
        assert_eq!(ts_to_minute("2026-06-18T09:15:30+10:00"), "2026-06-18T09:15");
        assert_eq!(ts_to_minute("2026-06-18T09:15:00+00:00"), "2026-06-18T09:15");
        assert_eq!(ts_to_minute("2026-06-18T09:15"), "2026-06-18T09:15");
    }

    // -----------------------------------------------------------------------
    // Chat-name derivation tests
    // -----------------------------------------------------------------------

    #[test]
    fn chat_name_strips_prefix_with() {
        assert_eq!(strip_chat_prefix("WhatsApp Chat with Alice"), "Alice");
        assert_eq!(strip_chat_prefix("WhatsApp Chat - Alice"), "Alice");
        assert_eq!(strip_chat_prefix("Alice"), "Alice");
        assert_eq!(strip_chat_prefix("_chat"), "_chat");
    }

    // -----------------------------------------------------------------------
    // Full import tests
    // -----------------------------------------------------------------------

    #[test]
    fn ios_bracketed_import() {
        let v = temp_vault("ios-bracketed");
        let body = fixture_ios_bracketed();
        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(body, "Alice Chat", Some("Bob"), &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        // 2 regular messages + 1 multiline (counts as 1) + 3 attachments + 2 calls + 1 event = 9
        assert!(stats.imported >= 7, "got {}", stats.imported);

        let month = format!("2026-06");
        let msgs = v.read_correspondence_month("whatsapp", &month).unwrap();
        assert!(!msgs.is_empty());

        // Check multiline message preserved newline in body.
        let bob_msg = msgs.iter().find(|m| m.text.contains("noon")).unwrap();
        assert!(bob_msg.text.contains('\n'), "continuation line joined");

        // Check LRM-prefixed attachment parsed (no LRM in stored name).
        let att = msgs.iter().find(|m| !m.attachments.is_empty()).unwrap();
        assert!(!att.attachments[0].name.starts_with('\u{200e}'),
            "attachment name must not contain LRM");
        // The first attachment should be photo.jpg.
        let photo_att = msgs.iter().find(|m| m.attachments.iter().any(|a| a.name == "photo.jpg")).unwrap();
        assert_eq!(photo_att.attachments[0].name, "photo.jpg");

        // Android-form attachment.
        let android_att = msgs.iter()
            .find(|m| m.attachments.iter().any(|a| a.name.contains("IMG-")));
        assert!(android_att.is_some(), "Android (file attached) form not parsed");

        // Check call line (system event "Missed voice call" or "Voice call").
        let calls: Vec<_> = msgs.iter().filter(|m| m.kind == "call").collect();
        assert!(!calls.is_empty(), "expected call records");

        // Check system event (non-call).
        let evt = msgs.iter().find(|m| m.kind == "event").unwrap();
        assert_eq!(evt.kind, "event");

        // Check from_me.
        let bob_says = msgs.iter().find(|m| m.from_me).unwrap();
        assert!(bob_says.from_me);
    }

    #[test]
    fn android_12h_import() {
        let v = temp_vault("android-12h");
        let body = fixture_android_12h();
        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(body, "Friends", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        assert!(stats.imported >= 3, "got {}", stats.imported);
        let month = v.correspondence_guids("whatsapp").unwrap();
        assert!(!month.is_empty());
    }

    #[test]
    fn eu_24h_import() {
        let v = temp_vault("eu-24h");
        let body = fixture_android_eu_24h();
        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(body, "Work", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        assert!(stats.imported >= 2, "got {}", stats.imported);
    }

    #[test]
    fn ios_space_sep_import() {
        // iOS space-separator format must parse and not silently import 0 rows.
        let v = temp_vault("ios-space-sep");
        let body = fixture_ios_space_sep();
        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(body, "Alice", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        assert!(stats.imported >= 2, "space-separator import got {}, expected >= 2", stats.imported);
    }

    #[test]
    fn lrm_rlm_prefix_import() {
        // Lines prefixed with U+200E (LRM) and U+200F (RLM) must parse correctly.
        let v = temp_vault("rtl-lrm");
        let body = fixture_rtl_lrm();
        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(body, "Test", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        // All 3 lines should import successfully (LRM/RLM must not cause zero-import).
        assert_eq!(stats.imported, 3, "LRM/RLM-prefixed lines must all parse; got {}", stats.imported);

        // Text bodies must not contain leading LRM/RLM.
        let month = v.read_correspondence_month("whatsapp", "2026-06").unwrap();
        for m in &month {
            assert!(
                !m.text.starts_with('\u{200e}') && !m.text.starts_with('\u{200f}'),
                "stored text must not start with directional mark: {:?}", m.text
            );
        }
    }

    #[test]
    fn reimport_deduplicates() {
        let v = temp_vault("dedup");
        let body = fixture_ios_bracketed();

        // Write the same file once (same path = same chat name = same guids).
        let txt = write_temp_txt("same-chat", body);

        // First pass.
        let s1 = v.import_whatsapp_export(&txt, Some("Bob")).unwrap();
        assert!(s1.imported > 0, "first import should import rows");
        assert_eq!(s1.duplicates, 0, "no duplicates on first pass");

        // Second pass with the same file — all rows are now duplicates.
        let s2 = v.import_whatsapp_export(&txt, Some("Bob")).unwrap();
        assert_eq!(s2.imported, 0, "re-import should import nothing (all dupes)");
        assert_eq!(s2.duplicates, s1.imported, "duplicate count matches first import count");
    }

    #[test]
    fn cross_format_dedup_minute_normalised() {
        // A message at 09:15:30 (iOS, with seconds) and the same message at
        // 09:15 (Android, no seconds) must dedup to a single record.
        let v = temp_vault("cross-format-dedup");

        // iOS-style (with seconds).
        let ios_body = "[18/06/2026, 09:15:30] Alice: Hello from Alice\n";
        // Android-style (without seconds, same minute).
        let android_body = "6/18/26, 9:15 AM - Alice: Hello from Alice\n";

        let mut seen = v.correspondence_guids("whatsapp").unwrap();
        let mut stats = WhatsAppImportStats { imported: 0, duplicates: 0, chats: 0 };
        let mut batch = Vec::new();
        v.parse_whatsapp_txt(ios_body, "Test", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();
        batch.clear();

        let s1_imported = stats.imported;
        assert_eq!(s1_imported, 1, "iOS pass: expected 1 imported");

        // Re-import with Android format — should be a duplicate.
        v.parse_whatsapp_txt(android_body, "Test", None, &mut seen, &mut batch, &mut stats);
        v.append_messages(&batch).unwrap();

        assert_eq!(stats.duplicates, 1, "Android re-import of same message should dedup");
        assert_eq!(stats.imported, 1, "total imported stays 1 after dedup");
    }

    #[test]
    fn zip_import_ios_chat_txt() {
        // iOS .zip: inner file is always "_chat.txt"; chat name comes from .zip filename.
        use std::io::Write;
        let zip_path = std::env::temp_dir()
            .join(format!("trove-wa-ios-zip-{}.zip", std::process::id()));
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            // iOS always uses _chat.txt as the inner filename.
            w.start_file("_chat.txt", opts).unwrap();
            w.write_all(fixture_ios_bracketed().as_bytes()).unwrap();
            w.start_file("IMG_0001.jpg", opts).unwrap();
            w.write_all(b"\xff\xd8\xff\xe0").unwrap();
            w.finish().unwrap();
        }
        let v = temp_vault("ios-zip");
        let s = v.import_whatsapp_export(&zip_path, Some("Bob")).unwrap();
        assert!(s.imported > 0, "iOS _chat.txt zip must import > 0 messages");
        assert_eq!(s.chats, 1);

        // Verify the chat name is derived from the zip filename, not "_chat".
        let month = v.read_correspondence_month("whatsapp", "2026-06").unwrap();
        assert!(!month.is_empty());
        // Chat key should NOT be "_chat".
        for m in &month {
            assert_ne!(m.chat, "_chat", "chat key must not be '_chat' for iOS _chat.txt zip");
        }
        let _ = fs::remove_file(zip_path);
    }

    #[test]
    fn zip_import_finds_txt() {
        // Non-iOS zip with a meaningful inner txt filename.
        use std::io::Write;
        let zip_path = std::env::temp_dir()
            .join(format!("trove-wa-zip-{}.zip", std::process::id()));
        {
            let f = std::fs::File::create(&zip_path).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file("WhatsApp Chat with Alice.txt", opts).unwrap();
            w.write_all(fixture_ios_bracketed().as_bytes()).unwrap();
            // A media file — should be ignored.
            w.start_file("IMG_0001.jpg", opts).unwrap();
            w.write_all(b"\xff\xd8\xff\xe0").unwrap();
            w.finish().unwrap();
        }
        let v = temp_vault("zip");
        let s = v.import_whatsapp_export(&zip_path, Some("Bob")).unwrap();
        assert!(s.imported > 0);
        assert_eq!(s.chats, 1);
        let _ = fs::remove_file(zip_path);
    }

    fn write_temp_txt(tag: &str, body: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir()
            .join(format!("trove-wa-{}-{}.txt", std::process::id(), tag));
        fs::write(&p, body).unwrap();
        p
    }
}
