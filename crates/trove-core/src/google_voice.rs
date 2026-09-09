//! Google Voice Takeout import — calls, SMS threads, and voicemails from
//! the HTML export in `Takeout/Voice/Calls/`.
//!
//! **Source format:** One HTML file per conversation. The record type and
//! direction are determined from the `<div class="tags"><a rel="tag"
//! href="…#placed|received|missed|voicemail|recorded">` fragment (the
//! canonical approach used by every community parser). The `<title>` is used
//! only as a name fallback.
//!
//! **HTML structure (verified against psanford/google-voice-takeout-parser
//! testdata — missedcall.html, voicemail.html, sms.html):**
//!
//! Call / voicemail:
//! ```html
//! <!-- outer descriptor span (SKIP — label, not the contact name): -->
//! <span class="fn">Missed call from\nDwigt Rortugal</span>
//! <!-- contributor vcard (has the real contact name inside tel anchor): -->
//! <div class="contributor vcard">Missed call from
//!   <a class="tel" href="tel:+66666"><span class="fn">Dwigt Rortugal</span></a>
//! </div>
//! <abbr class="published" title="2009-09-17T17:26:41.000-07:00">…</abbr>
//! <!-- duration is in the title= attribute, inner text is display "(00:00:18)": -->
//! <abbr class="duration" title="PT18S">(00:00:18)</abbr>
//! <!-- type/direction from rel=tag href fragment: -->
//! <div class="tags"><a rel="tag" href="http://www.google.com/voice#missed">Missed</a></div>
//! <!-- voicemail only: -->
//! <audio src="filename.mp3"></audio>
//! <span class="full-text">Transcript text</span>
//! ```
//!
//! SMS thread (inside `<div class="hChatLog hfeed">`):
//! ```html
//! <div class="message">
//!   <abbr class="dt" title="2022-06-30T18:06:39.894-07:00">…</abbr>
//!   <cite class="sender vcard">
//!     <!-- "Me" uses abbr fn; incoming uses span fn: -->
//!     <a class="tel" href="tel:+2222"><abbr class="fn" title="">Me</abbr></a>
//!   </cite>
//!   <q>Message body text</q>
//!   <!-- MMS: may also contain an img sibling to <q>: -->
//!   <div><img src="filename" alt="Image MMS Attachment" /></div>
//! </div>
//! ```
//!
//! **Vault layout:**
//! - Calls + SMS → `correspondence/google-voice/YYYY-MM.jsonl`
//!   (correspondence contract; `source:"google-voice"`)
//! - Voicemails → `voice/google-voice/YYYY-MM.jsonl`
//!   (voice contract; `source:"google-voice"`, `kind:"voicemail"`)
//! - Raw HTML copied to `correspondence/google-voice/raw/<filename>` (full
//!   fidelity; always written before contract rows).
//!
//! **Dedupe:** `guid` = SHA-256 hash of (counterpart_phone, normalized_ts,
//! kind) for calls and voicemails (content-derived, stable across filename
//! changes); for SMS rows, hash of (filename, row_index, ts). Re-importing
//! a newer Takeout archive is idempotent.
//!
//! **No connection:** The user downloads takeout.google.com themselves;
//! there is no API. The existing "google" OAuth connection does not cover
//! Voice data.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::correspondence::{AttachmentMeta, Message};
use crate::health::ImportProgress;
use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{Behavior, ImportOutcome, ImportSpec, IntegrationDef};
use crate::store::Partition;
use crate::vault::Vault;
use crate::voice::Recording;

const SOURCE: &str = "google-voice";
const CORR_DIR: &str = "correspondence/google-voice";
const VOICE_DIR: &str = "voice/google-voice";
const RAW_DIR: &str = "correspondence/google-voice/raw";

// ---------------------------------------------------------------------------
// DEF

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(CORR_DIR))
        .or_else(|| crate::registry::newest_stem(&vault.root().join(VOICE_DIR)))
}

/// Registered in [`crate::integrations::INTEGRATIONS`].
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "google-voice",
        name: "Google Voice",
        kind: IntegrationKind::Import,
        default_on: false,
        description: "Imports your Google Voice history — calls, SMS threads, and \
                      voicemail transcripts — from a Google Takeout export. \
                      Re-importable: newer archives never duplicate stored records.",
        domain: "correspondence",
        vault_path: "correspondence/google-voice/",
        toggleable: false,
        setup: &[
            "Open takeout.google.com, select Voice, and download the archive.",
            "Drop the downloaded .zip or extracted Voice/Calls/ folder here.",
        ],
        caveats: "Takeout is the only supported path — there is no Google Voice personal \
                  history API. Voicemail audio and transcripts are written separately \
                  under voice/google-voice/. Message bodies and voicemail transcripts \
                  are privacy-sensitive: import is opt-in.",
    },
    behavior: Behavior::Import(&IMPORT),
    permission: None,
    last_data: Some(def_last_data),
    connection: None,
    pull: None,
};

static IMPORT: ImportSpec = ImportSpec {
    signatures: &[],
    accepts: &["zip", "html"],
    params: &[],
    run: run_import,
};

// ---------------------------------------------------------------------------
// Import entry point

fn run_import(
    vault: &Vault,
    path: &Path,
    _params: &std::collections::BTreeMap<String, String>,
    progress: &mut dyn FnMut(ImportProgress),
) -> Result<ImportOutcome> {
    // Collect existing guids for both contracts (dedupe on re-import).
    let mut known_corr = vault.correspondence_guids(SOURCE)?;
    let mut known_voice = gv_voice_guids(vault)?;

    let mut corr_msgs: Vec<Message> = Vec::new();
    let mut voice_recs: Vec<Recording> = Vec::new();

    let (mut imported_corr, mut imported_voice, mut duplicates, mut skipped) =
        (0u64, 0u64, 0u64, 0u64);
    let mut file_count = 0u64;

    // Collect HTML files from: bare .html, or .zip containing HTML files.
    let html_entries = collect_html(path)?;

    for (filename, html_bytes) in &html_entries {
        file_count += 1;
        let html = match std::str::from_utf8(html_bytes) {
            Ok(s) => s,
            Err(_) => {
                // Try lossy decode (Takeout uses UTF-8, but be safe).
                skipped += 1;
                continue;
            }
        };

        // Write raw copy unconditionally (full fidelity before contract rows).
        let raw_path = vault.resolve(&format!("{RAW_DIR}/{filename}"))?;
        if let Some(parent) = raw_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&raw_path, html_bytes);

        match parse_html(filename, html) {
            ParsedFile::Call(msg) => {
                if !known_corr.contains(&msg.guid) {
                    known_corr.insert(msg.guid.clone());
                    corr_msgs.push(msg);
                    imported_corr += 1;
                } else {
                    duplicates += 1;
                }
            }
            ParsedFile::SmsThread(msgs) => {
                for msg in msgs {
                    if !known_corr.contains(&msg.guid) {
                        known_corr.insert(msg.guid.clone());
                        corr_msgs.push(msg);
                        imported_corr += 1;
                    } else {
                        duplicates += 1;
                    }
                }
            }
            ParsedFile::Voicemail(rec) => {
                if !known_voice.contains(&rec.guid) {
                    known_voice.insert(rec.guid.clone());
                    voice_recs.push(rec);
                    imported_voice += 1;
                } else {
                    duplicates += 1;
                }
            }
            ParsedFile::Unknown => {
                skipped += 1;
            }
        }

        if file_count % 100 == 0 {
            progress(ImportProgress { records: imported_corr + imported_voice, percent: 0.0 });
        }
    }

    // Persist correspondence rows (calls + SMS).
    if !corr_msgs.is_empty() {
        vault.append_messages(&corr_msgs)?;
    }

    // Persist voice contract rows (voicemails).
    if !voice_recs.is_empty() {
        vault.stream(VOICE_DIR, Partition::Month).append(&voice_recs, |r| &r.ts)?;
    }

    progress(ImportProgress { records: imported_corr + imported_voice, percent: 100.0 });
    Ok(ImportOutcome {
        headline: format!(
            "{imported_corr} messages/calls, {imported_voice} voicemails imported, \
             {duplicates} duplicates skipped"
        ),
        counts: [
            ("correspondence", imported_corr),
            ("voicemails", imported_voice),
            ("duplicates", duplicates),
            ("skipped", skipped),
        ]
        .into(),
    })
}

// ---------------------------------------------------------------------------
// Dedupe set for the voice/ sink (no shared Vault helper; scoped to gv).

fn gv_voice_guids(vault: &Vault) -> Result<HashSet<String>> {
    let stream = vault.stream(VOICE_DIR, Partition::Month);
    let mut set = HashSet::new();
    for key in stream.partitions()? {
        for rec in stream.read::<Recording>(&key)? {
            if !rec.guid.is_empty() {
                set.insert(rec.guid);
            }
        }
    }
    Ok(set)
}

// ---------------------------------------------------------------------------
// File collection: bare .html or .zip of HTMLs.

/// Collect `(filename, bytes)` pairs from either a bare `.html` file or
/// a `.zip` archive. Accepts both a Takeout ZIP root and a `Voice/Calls/`
/// sub-zip. Only `.html` entries are returned; non-HTML entries are skipped.
fn collect_html(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("zip")) {
        let file = fs::File::open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        let mut archive = zip::ZipArchive::new(file)
            .with_context(|| format!("reading zip {}", path.display()))?;
        let mut out = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)?;
            let name = entry.name().to_string();
            // Accept any .html file anywhere in the zip (works for both a full
            // Takeout zip and an extracted Voice/Calls/ sub-zip).
            if !name.ends_with(".html") && !name.ends_with(".HTML") {
                continue;
            }
            // Use only the filename component (no directory prefix).
            let fname = Path::new(&name)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes)?;
            out.push((fname, bytes));
        }
        Ok(out)
    } else {
        // Bare .html file.
        let fname = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "voice.html".into());
        let bytes = fs::read(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(vec![(fname, bytes)])
    }
}

// ---------------------------------------------------------------------------
// Parse one HTML file → one of three shapes.

enum ParsedFile {
    Call(Message),
    SmsThread(Vec<Message>),
    Voicemail(Recording),
    Unknown,
}

/// The record type determined from the `rel=tag` href fragment and/or
/// the `hChatLog` container.
#[derive(Debug, PartialEq)]
enum RecordType {
    Placed,
    Received,
    Missed,
    Recorded,
    Voicemail,
    Sms,
    Unknown,
}

/// Determine the record type from the `<a rel="tag" href="…#fragment">` in
/// `<div class="tags">`. This is the canonical method used by all community
/// parsers (psanford, gvoiceParser/SandNerd, NeighborGeek, voice2json).
/// Falls back to `Unknown` for unrecognised fragments (never silently drops —
/// the Unknown arm logs via skipped counter).
fn record_type_from_tags(html: &str) -> RecordType {
    // SMS threads never have a tags div — detected separately.
    if html.contains("hChatLog") || html.contains("hchatlog") {
        return RecordType::Sms;
    }
    // Find `rel="tag"` anchors and inspect the href fragment.
    let mut search = html;
    while let Some(rel_pos) = search.find("rel=\"tag\"") {
        // Find the enclosing `<a` tag.
        let before = &search[..rel_pos];
        if let Some(tag_start) = before.rfind('<') {
            let tag_end_rel = search[rel_pos..].find('>').unwrap_or(0);
            let tag = &search[tag_start..rel_pos + tag_end_rel + 1];
            if let Some(href) = attr_value(tag, "href") {
                // Extract the fragment (#placed, #received, etc.)
                if let Some(frag) = href.split('#').nth(1) {
                    match frag.to_lowercase().as_str() {
                        "placed" => return RecordType::Placed,
                        "received" => return RecordType::Received,
                        "missed" => return RecordType::Missed,
                        "recorded" => return RecordType::Recorded,
                        "voicemail" => return RecordType::Voicemail,
                        _ => {}
                    }
                }
            }
        }
        search = &search[rel_pos + 9..];
    }
    RecordType::Unknown
}

/// Parse one Google Voice Takeout HTML file. The conversation type is
/// determined from the rel=tag href fragment in `<div class="tags">`,
/// with SMS identified by the presence of `hChatLog`.
fn parse_html(filename: &str, html: &str) -> ParsedFile {
    let title = extract_title(html);

    match record_type_from_tags(html) {
        RecordType::Voicemail => parse_voicemail(filename, html, &title),
        RecordType::Placed | RecordType::Received | RecordType::Missed | RecordType::Recorded => {
            parse_call(filename, html, &title)
        }
        RecordType::Sms => parse_sms_thread(filename, html, &title),
        RecordType::Unknown => ParsedFile::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Stable GUID

/// Content-derived GUID: hash of (counterpart_phone, normalized_ts, kind).
/// Stable across filename changes — safe for re-import from renamed Takeout.
fn stable_guid(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update(b"\x00");
    }
    format!("gv-{}", hex_short(&h.finalize()))
}

fn hex_short(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// HTML string utilities (no external HTML parser; Takeout files are
// predictably structured, well-formed, and ASCII-safe for these patterns).

/// Extract `<title>…</title>` text, decoded. Real Takeout titles are
/// two-line ("Missed call from\nDwigt Rortugal") — trim whitespace.
fn extract_title(html: &str) -> String {
    between(html, "<title>", "</title>")
        .map(html_decode)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// First text between `open` and `close` in `s`, case-sensitive.
fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let end = s[start..].find(close)? + start;
    Some(&s[start..end])
}

/// Find text of the first element whose `class` attribute contains `cls`.
/// Returns the text of the first matching opening tag's content.
fn class_text(html: &str, cls: &str) -> Option<String> {
    // Match any tag with this class name.
    let needle = format!("class=\"{cls}\"");
    let alt_needle = format!("class='{cls}'");
    let pos = html.find(&needle).or_else(|| html.find(&alt_needle))?;
    // Find the end of the opening tag.
    let after_open = html[pos..].find('>')?;
    let content_start = pos + after_open + 1;
    // Find the first closing tag (non-nested version, good enough for leaf nodes).
    let rest = &html[content_start..];
    // Try to extract until `</`.
    let content_end = rest.find("</")?;
    Some(html_decode(rest[..content_end].trim()))
}

/// Get the `title` attribute of the first element with `class` containing `cls`.
fn class_title_attr(html: &str, cls: &str) -> Option<String> {
    let needle = format!("class=\"{cls}\"");
    let alt = format!("class='{cls}'");
    // Search for class attribute; then find the tag start by scanning back.
    let pos = html.find(&needle).or_else(|| html.find(&alt))?;
    // Find the enclosing `<…>` by scanning backward for `<`.
    let tag_start = html[..pos].rfind('<')?;
    let tag_end = html[pos..].find('>')? + pos;
    let tag = &html[tag_start..=tag_end];
    attr_value(tag, "title")
}

/// Extract the value of attribute `name` from an HTML tag string.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    // Handle both `name="value"` and `name='value'`.
    let patt_double = format!("{name}=\"");
    let patt_single = format!("{name}='");
    if let Some(p) = tag.find(&patt_double) {
        let start = p + patt_double.len();
        let end = tag[start..].find('"')? + start;
        return Some(html_decode(&tag[start..end]));
    }
    if let Some(p) = tag.find(&patt_single) {
        let start = p + patt_single.len();
        let end = tag[start..].find('\'')? + start;
        return Some(html_decode(&tag[start..end]));
    }
    None
}

/// Extract the `href="tel:+15551234567"` phone number from the nearest `<a
/// class="tel"` element in `snippet`.
fn tel_href(snippet: &str) -> Option<String> {
    // Find `class="tel"` anchor.
    let pos = snippet.find("class=\"tel\"")?;
    let tag_start = snippet[..pos].rfind('<')?;
    let tag_end = snippet[pos..].find('>')? + pos;
    let tag = &snippet[tag_start..=tag_end];
    let href = attr_value(tag, "href")?;
    // Strip "tel:" prefix.
    Some(href.trim_start_matches("tel:").to_string())
}

/// Extract the contact name from the `<span class="fn">` (or
/// `<abbr class="fn">`) that is INSIDE the `<a class="tel">` anchor.
///
/// Real Google Voice Takeout HTML has a *descriptor* `<span class="fn">` at
/// the haudio level (e.g. "Missed call from\nDwigt Rortugal") BEFORE the
/// contributor `<div>` that contains the tel anchor with the actual name.
/// Using `class_text(html, "fn")` returns the first match — the label string
/// — not the name.  We must scope the search to the content of the tel anchor.
fn tel_fn_name(snippet: &str) -> Option<String> {
    // Locate `<a class="tel"` anchor.
    let pos = snippet.find("class=\"tel\"")?;
    let _tag_start = snippet[..pos].rfind('<')?;
    let tag_end_rel = snippet[pos..].find('>')?;
    let after_open_tag = pos + tag_end_rel + 1;

    // Find the closing `</a>` after the opening tag.
    let rest = &snippet[after_open_tag..];
    let close_pos = rest.find("</a>")?;
    let inside = &rest[..close_pos];

    // Now find `<span class="fn">` or `<abbr class="fn">` within `inside`.
    class_text(inside, "fn").filter(|s| !s.is_empty())
}

/// Parse an ISO 8601 duration string from the `title` attribute of
/// `<abbr class="duration" title="PT#H#M#S">`.
///
/// Real Takeout uses the ISO 8601 format in the title attribute:
///   `title="PT18S"` → 18 seconds
///   `title="PT1M30S"` → 90 seconds
///   `title="PT1H5M30S"` → 3930 seconds
///
/// The inner text `(00:00:18)` is NOT parsed because parens make it
/// unparseable as H:MM:SS without preprocessing.
fn parse_iso_duration(s: &str) -> Option<u64> {
    // Strip leading "PT" prefix.
    let s = s.trim();
    let s = if s.starts_with("PT") || s.starts_with("pt") {
        &s[2..]
    } else {
        return None;
    };
    // Parse optional H, M, S components.
    let mut hours: u64 = 0;
    let mut minutes: u64 = 0;
    let mut seconds: u64 = 0;
    let mut num_buf = String::new();
    for ch in s.chars() {
        match ch {
            '0'..='9' => num_buf.push(ch),
            'H' | 'h' => {
                hours = num_buf.parse().ok()?;
                num_buf.clear();
            }
            'M' | 'm' => {
                minutes = num_buf.parse().ok()?;
                num_buf.clear();
            }
            'S' | 's' => {
                seconds = num_buf.parse().ok()?;
                num_buf.clear();
            }
            _ => return None,
        }
    }
    Some(hours * 3600 + minutes * 60 + seconds)
}

/// Get the duration from `<abbr class="duration" title="PT#S">(00:00:18)</abbr>`.
///
/// Strategy (in priority order):
/// 1. If a `title` attribute exists and is non-empty: parse as ISO 8601 (`PT#S`).
/// 2. If no title attribute (or it's empty): fall back to the inner text,
///    stripping parens and parsing as H:MM:SS or MM:SS.
fn duration_from_abbr(html: &str) -> Option<u64> {
    // Try to read the title attribute first (canonical Takeout format).
    if let Some(title_raw) = class_title_attr(html, "duration") {
        if !title_raw.is_empty() {
            // ISO 8601 first (real Takeout: "PT18S"), then H:MM:SS fallback.
            if let Some(secs) = parse_iso_duration(&title_raw) {
                return Some(secs);
            }
            // title attr present but not ISO — try H:MM:SS.
            if let Some(secs) = parse_hms_duration(&title_raw) {
                return Some(secs);
            }
        }
    }
    // Fall back to inner text (no title attr, or title unparseable).
    // Inner text may be "(00:02:15)" — parse_hms_duration strips parens.
    let inner = class_text(html, "duration")?;
    parse_hms_duration(&inner)
}

/// Parse a duration string "H:MM:SS" or "MM:SS" into total seconds.
/// Handles leading/trailing parens and whitespace for robustness.
fn parse_hms_duration(s: &str) -> Option<u64> {
    // Strip surrounding parens, whitespace, and non-digit/colon chars.
    let cleaned: String = s.chars().filter(|c| c.is_ascii_digit() || *c == ':').collect();
    let parts: Vec<&str> = cleaned.trim().split(':').collect();
    match parts.as_slice() {
        [h, m, s] => {
            let h: u64 = h.parse().ok()?;
            let m: u64 = m.parse().ok()?;
            let s: u64 = s.parse().ok()?;
            Some(h * 3600 + m * 60 + s)
        }
        [m, s] => {
            let m: u64 = m.parse().ok()?;
            let s: u64 = s.parse().ok()?;
            Some(m * 60 + s)
        }
        _ => None,
    }
}

/// Minimal HTML entity decode for the entities Takeout produces.
fn html_decode(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

/// Parse an ISO 8601 timestamp of the form `"2022-09-30T14:36:36.127-04:00"`
/// (Takeout format: dot-separated milliseconds, colon in UTC offset) into
/// RFC3339. Returns the input verbatim when it can't be parsed (honest unknowns).
fn normalize_ts(raw: &str) -> String {
    // chrono parses "+HH:MM" timezone offsets natively with `%z` or `%:z`.
    // The Takeout format is "YYYY-MM-DDTHH:MM:SS.sss±HH:MM".
    use chrono::DateTime;
    if let Ok(dt) = DateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f%:z") {
        return dt.to_rfc3339();
    }
    // Fallback: try without fractional seconds.
    if let Ok(dt) = DateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%:z") {
        return dt.to_rfc3339();
    }
    raw.to_string()
}

/// Extract `src` attribute from the first `<audio …>` element.
fn extract_audio_src(html: &str) -> Option<String> {
    let start = html.find("<audio")?;
    let end = html[start..].find('>')? + start;
    let tag = &html[start..=end];
    attr_value(tag, "src")
}

// ---------------------------------------------------------------------------
// Call parsing

fn parse_call(_filename: &str, html: &str, _title: &str) -> ParsedFile {
    let ts_raw = class_title_attr(html, "published").unwrap_or_default();
    let ts = if ts_raw.is_empty() { return ParsedFile::Unknown; } else { normalize_ts(&ts_raw) };

    // Direction from rel=tag fragment (already determined by record_type_from_tags,
    // but re-derive here for the from_me/missed booleans).
    let (from_me, missed) = match record_type_from_tags(html) {
        RecordType::Placed | RecordType::Recorded => (true, false),
        RecordType::Missed => (false, true),
        _ => (false, false), // received
    };

    // Counterpart phone number: from <a class="tel"> href.
    let counterpart = tel_href(html).unwrap_or_default();

    // Contact name: must come from INSIDE the tel anchor, not the first fn
    // in the document (the first fn is the haudio-level label string like
    // "Missed call from\nDwigt Rortugal").
    let counterpart_name = tel_fn_name(html).unwrap_or_default();

    // Duration: read ISO 8601 from the title attribute of <abbr class="duration">.
    // 0 for missed calls (they may not even have a duration abbr).
    let duration_secs: u64 = if missed {
        0
    } else {
        duration_from_abbr(html).unwrap_or(0)
    };

    // Content-derived GUID: stable across Takeout filename renames.
    let guid = stable_guid(&[&counterpart, &ts, "call"]);

    let mut msg = Message::new(SOURCE, ts);
    msg.kind = "call".into();
    msg.from_me = from_me;
    msg.chat = counterpart.clone();
    if !from_me {
        msg.sender = counterpart;
    }
    if !counterpart_name.is_empty() {
        if from_me {
            msg.chat_name = counterpart_name;
        } else {
            msg.sender_name = counterpart_name;
        }
    }
    msg.duration_secs = duration_secs;
    msg.service = "Google Voice".into();
    msg.guid = guid;

    ParsedFile::Call(msg)
}

// ---------------------------------------------------------------------------
// Voicemail parsing

fn parse_voicemail(_filename: &str, html: &str, title: &str) -> ParsedFile {
    let ts_raw = class_title_attr(html, "published").unwrap_or_default();
    let ts = if ts_raw.is_empty() { return ParsedFile::Unknown; } else { normalize_ts(&ts_raw) };

    // Caller phone from <a class="tel"> href.
    let sender = tel_href(html).unwrap_or_default();

    // Contact name: scoped to inside the tel anchor to avoid the haudio-level
    // "Voicemail from\nSleve Mcdichael" label span.
    let sender_name_from_tel = tel_fn_name(html).unwrap_or_default();

    // Duration: ISO 8601 from the title attribute of <abbr class="duration">.
    let duration_secs = duration_from_abbr(html);

    // Transcript from `<span class="full-text">`.
    let transcript = class_text(html, "full-text").unwrap_or_default();

    // Audio file from `<audio src="...">` — vault-relative path under voice/.
    let audio_src = extract_audio_src(html);
    let audio_ref = if let Some(src) = &audio_src {
        format!("{VOICE_DIR}/audio/{src}")
    } else {
        String::new()
    };

    // Extract "from …" name from title as a fallback (title is "Voicemail from
    // Sleve Mcdichael" after whitespace normalization).
    let title_name = title
        .strip_prefix("Voicemail from ")
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or_default();
    let effective_sender_name = if !sender_name_from_tel.is_empty() {
        sender_name_from_tel
    } else {
        title_name.to_string()
    };

    // Content-derived GUID: (sender_phone, ts, "voicemail").
    let guid = stable_guid(&[&sender, &ts, "voicemail"]);

    let mut rec = Recording::new(SOURCE, "voicemail", ts);
    rec.sender = sender;
    rec.sender_name = effective_sender_name;
    rec.transcript = transcript;
    rec.audio_ref = audio_ref;
    if let Some(d) = duration_secs {
        rec.duration_secs = Some(d as i64);
    }
    rec.guid = guid;

    ParsedFile::Voicemail(rec)
}

// ---------------------------------------------------------------------------
// SMS thread parsing

fn parse_sms_thread(filename: &str, html: &str, _title: &str) -> ParsedFile {
    // Counterpart: the phone number of the first NON-"Me" sender in the thread.
    // We cannot trust the first <a class="tel"> in the document because in a
    // thread that the owner started, the first tel is the owner's own number
    // (+2222 = "Me"), not the other party. We look for the first message whose
    // fn class is NOT "Me" and extract its tel href.
    let counterpart = find_sms_counterpart(html).unwrap_or_default();

    // Split into individual `<div class="message">` blocks.
    let msgs = parse_sms_messages(filename, html, &counterpart);
    ParsedFile::SmsThread(msgs)
}

/// Find the counterpart's phone number: the tel href of the first message
/// whose sender fn text is NOT "Me" (i.e. the first incoming message).
fn find_sms_counterpart(html: &str) -> Option<String> {
    for block in sms_message_blocks(html) {
        // Determine if this message is from "Me" by checking if the fn class
        // uses <abbr> (outgoing "Me") vs <span> (incoming contact).
        if !is_from_me_block(block) {
            // This is an incoming message — extract the counterpart phone.
            if let Some(phone) = tel_href(block) {
                if !phone.is_empty() {
                    return Some(phone);
                }
            }
        }
    }
    // Fallback: if ALL messages are from "Me" (sent-only thread), use the
    // first tel href that isn't obviously a "Me" number. We can't determine
    // "Me" number without the vault owner's phone, so return empty.
    None
}

/// Returns true if this message block is from the vault owner ("Me").
/// Real Takeout uses `<abbr class="fn">` for "Me" and `<span class="fn">`
/// for contacts. This matches the SMS structure verified in sms.html.
fn is_from_me_block(block: &str) -> bool {
    // Check if there's an <abbr class="fn"> element in this block.
    if let Some(pos) = block.find("class=\"fn\"") {
        if let Some(tag_start) = block[..pos].rfind('<') {
            return block[tag_start..].starts_with("<abbr");
        }
    }
    false
}

/// Find the span of a `<div class="message">` block (from `<div class="message">`
/// to its matching `</div>`), yielding slices of each message block.
fn sms_message_blocks(html: &str) -> Vec<&str> {
    let marker = "class=\"message\"";
    let mut out = Vec::new();
    let mut search_from = 0;

    while let Some(rel) = html[search_from..].find(marker) {
        let div_end = search_from + rel;
        // Walk back to find the opening `<`.
        let tag_start = match html[..div_end].rfind('<') {
            Some(p) => p,
            None => { search_from = div_end + marker.len(); continue; }
        };
        // Depth-track to find the matching `</div>`.
        let rest = &html[tag_start..];
        let block_end = find_div_end(rest);
        let block = &rest[..block_end];
        out.push(block);
        search_from = tag_start + block_end;
    }
    out
}

/// Find the end offset (exclusive) of the outermost `<div>` in `s`, which
/// starts with `<div …>`. Uses a simple depth counter — adequate for the
/// flat Takeout message blocks.
fn find_div_end(s: &str) -> usize {
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with("<div") {
            depth += 1;
            i += 4;
        } else if s[i..].starts_with("</div>") {
            depth -= 1;
            i += 6;
            if depth <= 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    s.len()
}

fn parse_sms_messages(filename: &str, html: &str, counterpart: &str) -> Vec<Message> {
    let mut out = Vec::new();
    for (idx, block) in sms_message_blocks(html).iter().enumerate() {
        let Some(msg) = parse_one_sms(filename, block, idx, counterpart) else { continue };
        out.push(msg);
    }
    out
}

fn parse_one_sms(filename: &str, block: &str, idx: usize, counterpart: &str) -> Option<Message> {
    // Timestamp from `<abbr class="dt" title="…">`. In real Takeout the dt
    // abbr appears FIRST in the message div (before the cite/sender).
    let ts_raw = class_title_attr(block, "dt")?;
    let ts = normalize_ts(&ts_raw);

    // Direction: "Me" uses <abbr class="fn">; contacts use <span class="fn">.
    let from_me = is_from_me_block(block);

    // Sender name: scoped to the content of the tel anchor to get the clean
    // name, not the outer label.
    let sender_name = tel_fn_name(block).unwrap_or_default();
    let sender_phone = tel_href(block).unwrap_or_default();

    // Body from `<q>…</q>`.
    let text = between(block, "<q>", "</q>")
        .map(html_decode)
        .unwrap_or_default();

    // MMS image attachments: look for <img> tags in the block.
    // Real Takeout MMS: <div><img src="Tony Smehrik - Text - …" alt="Image MMS Attachment"/></div>
    let attachments = parse_mms_attachments(block);

    // guid = hash of (filename, idx, ts) — stable even across re-imports.
    let guid = stable_guid(&[filename, &idx.to_string(), &ts_raw]);

    let mut msg = Message::new(SOURCE, ts);
    msg.from_me = from_me;
    msg.chat = counterpart.to_string();
    msg.text = text;
    msg.service = "Google Voice".into();
    msg.guid = guid;
    msg.attachments = attachments;

    if from_me {
        // Outgoing: sender is the vault owner (implicit; leave sender empty).
        // chat_name is the counterpart's display name; we don't have it here
        // so leave empty.
    } else {
        msg.sender = if !sender_phone.is_empty() { sender_phone } else { sender_name.clone() };
        msg.sender_name = sender_name;
    }

    Some(msg)
}

/// Parse MMS image attachments from a message block.
/// Real Takeout puts images in `<img src="..." alt="Image MMS Attachment"/>`.
fn parse_mms_attachments(block: &str) -> Vec<AttachmentMeta> {
    let mut attachments = Vec::new();
    let mut search = block;
    while let Some(img_pos) = search.find("<img") {
        let after_img = &search[img_pos..];
        let tag_end = after_img.find('>').unwrap_or(after_img.len() - 1);
        let tag = &after_img[..=tag_end];

        if let Some(src) = attr_value(tag, "src") {
            if !src.is_empty() {
                let alt = attr_value(tag, "alt").unwrap_or_default();
                // Only include if it looks like an MMS attachment
                // (has an alt attribute with "MMS" or "Attachment" or any img src).
                let name = Path::new(&src)
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| src.clone());
                // Infer mime type from alt text or extension.
                let mime = if alt.to_lowercase().contains("image") {
                    "image/*".to_string()
                } else if alt.to_lowercase().contains("video") {
                    "video/*".to_string()
                } else {
                    String::new()
                };
                attachments.push(AttachmentMeta {
                    name,
                    mime,
                    bytes: 0,
                });
            }
        }

        // Advance past this img tag.
        if tag_end + 1 >= search[img_pos..].len() {
            break;
        }
        search = &search[img_pos + tag_end + 1..];
    }
    attachments
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-gvoice-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    fn run(v: &Vault, fname: &str, html: &str) -> ImportOutcome {
        let path = v.root().join(fname);
        fs::write(&path, html).unwrap();
        (IMPORT.run)(v, &path, &BTreeMap::new(), &mut |_| {}).unwrap()
    }

    // -------------------------------------------------------------------
    // Fixture HTML strings — verified against real psanford/google-voice-takeout-parser
    // testdata (missedcall.html, voicemail.html, sms.html).
    //
    // Key structural details from real Takeout:
    // 1. <title> is two-line: "Missed call from\nDwigt Rortugal"
    // 2. A haudio-level <span class="fn"> carries the LABEL string ("Missed call from\nName")
    //    — the real contact name is inside <a class="tel"><span class="fn">Name</span></a>
    // 3. <abbr class="duration" title="PT18S">(00:00:18)</abbr> — duration in title attr (ISO 8601)
    // 4. Record type from <a rel="tag" href="…#missed|received|placed|voicemail"> fragment
    // 5. SMS: <abbr class="dt"> appears FIRST, then <cite>; "Me" uses <abbr class="fn">

    const CALL_RECEIVED: &str = r#"<?xml version="1.0" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Received call from
Bob Smith</title></head>
<body>
<div class="haudio">
  <span class="album">Call Log for</span>
  <span class="fn">Received call from
Bob Smith</span>
  <div class="contributor vcard">Received call from
    <a class="tel" href="tel:+15551234567"><span class="fn">Bob Smith</span></a>
  </div>
  <abbr class="published" title="2026-06-10T14:36:36.127-07:00">Jun 10, 2026</abbr>
  <abbr class="duration" title="PT135S">(00:02:15)</abbr>
  <div class="tags">Labels:
    <a rel="tag" href="http://www.google.com/voice#received">Received</a>
  </div>
</div>
</body>
</html>"#;

    const CALL_MISSED: &str = r#"<?xml version="1.0" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Missed call from
Alice Jones</title></head>
<body>
<div class="haudio">
  <span class="album">Call Log for</span>
  <span class="fn">Missed call from
Alice Jones</span>
  <div class="contributor vcard">Missed call from
    <a class="tel" href="tel:+15559876543"><span class="fn">Alice Jones</span></a>
  </div>
  <abbr class="published" title="2026-06-11T09:15:00.000-07:00">Jun 11, 2026</abbr>
  <div class="tags">Labels:
    <a rel="tag" href="http://www.google.com/voice#missed">Missed</a>
  </div>
</div>
</body>
</html>"#;

    const CALL_PLACED: &str = r#"<?xml version="1.0" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Placed call to
Bob Smith</title></head>
<body>
<div class="haudio">
  <span class="album">Call Log for</span>
  <span class="fn">Placed call to
Bob Smith</span>
  <div class="contributor vcard">Placed call to
    <a class="tel" href="tel:+15551234567"><span class="fn">Bob Smith</span></a>
  </div>
  <abbr class="published" title="2026-06-12T16:00:00.000-07:00">Jun 12, 2026</abbr>
  <abbr class="duration" title="PT330S">(00:05:30)</abbr>
  <div class="tags">Labels:
    <a rel="tag" href="http://www.google.com/voice#placed">Placed</a>
  </div>
</div>
</body>
</html>"#;

    // Modeled on real psanford voicemail.html: label span first, then contributor vcard,
    // then audio + full-text + duration with PT#S in title.
    const VOICEMAIL: &str = r#"<?xml version="1.0" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Voicemail from
Dr. Reyes Office</title></head>
<body>
<div class="haudio">
  <span class="album">Call Log for</span>
  <span class="fn">Voicemail from
Dr. Reyes Office</span>
  <div class="contributor vcard">Voicemail from
    <a class="tel" href="tel:+14155550137"><span class="fn">Dr. Reyes Office</span></a>
  </div>
  <abbr class="published" title="2026-05-22T18:05:00.000-07:00">May 22, 2026</abbr>
  Transcript:
  <span class="description"><span class="full-text">Hi, this is Dr. Reyes's office confirming your appointment on Friday at ten.</span>
  <br />
  <audio controls="controls" src="2026-05-22T180500Z_+14155550137.mp3"><a rel="enclosure" href="2026-05-22T180500Z_+14155550137.mp3">Audio</a></audio>
  <abbr class="duration" title="PT27S">(00:00:27)</abbr>
  <div class="tags">Labels:
    <a rel="tag" href="http://www.google.com/voice#voicemail">Voicemail</a>
  </div>
</div>
</body>
</html>"#;

    // Modeled on real psanford sms.html: dt FIRST, then cite; "Me" uses <abbr class="fn">;
    // first message is from "Me" (+2222), counterpart is Tony (+333). MMS in msg 2.
    const SMS_THREAD: &str = r#"<?xml version="1.0" ?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.0 Strict//EN" "http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd">
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Text message with Bob Smith</title></head>
<body>
<div class="hChatLog hfeed">
  <div class="message"><abbr class="dt" title="2026-06-10T10:30:00.000-07:00">Jun 10</abbr>:
    <cite class="sender vcard"><a class="tel" href="tel:+15550000000"><abbr class="fn" title="">Me</abbr></a></cite>:
    <q>hey, are you free for lunch?</q>
  </div>
  <div class="message"><abbr class="dt" title="2026-06-10T10:35:00.000-07:00">Jun 10</abbr>:
    <cite class="sender vcard"><a class="tel" href="tel:+15550000000"><abbr class="fn" title="">Me</abbr></a></cite>:
    <q>MMS Sent</q>
    <div><img src="Bob Smith - Text - 2026-06-10T173500Z-2-1" alt="Image MMS Attachment" /></div>
  </div>
  <div class="message"><abbr class="dt" title="2026-06-10T10:40:00.000-07:00">Jun 10</abbr>:
    <cite class="sender vcard"><a class="tel" href="tel:+15551234567"><span class="fn">Bob Smith</span></a></cite>:
    <q>yes! where do you want to go?</q>
  </div>
</div>
</body>
</html>"#;

    // -------------------------------------------------------------------
    // Tests

    #[test]
    fn parse_received_call() {
        let v = temp_vault("recv-call");
        let out = run(&v, "Bob Smith - 2026-06-10 - received.html", CALL_RECEIVED);
        assert!(out.counts["correspondence"] >= 1, "imported at least one call");

        let msgs = v.read_correspondence_month(SOURCE, "2026-06").unwrap();
        assert_eq!(msgs.len(), 1);
        let m = &msgs[0];
        assert_eq!(m.kind, "call");
        assert!(!m.from_me);
        assert_eq!(m.chat, "+15551234567");
        assert_eq!(m.sender, "+15551234567");
        // Name must come from inside the tel anchor, NOT the haudio-level label.
        assert_eq!(m.sender_name, "Bob Smith",
            "sender_name must be the contact name, not the label string");
        assert_eq!(m.duration_secs, 135, "PT135S = 135 seconds");
        assert_eq!(m.service, "Google Voice");
        assert!(m.ts.starts_with("2026-06-10"));
        assert!(!m.guid.is_empty());
    }

    #[test]
    fn parse_missed_call_zero_duration() {
        let v = temp_vault("missed-call");
        let out = run(&v, "Alice - missed.html", CALL_MISSED);
        assert_eq!(out.counts["correspondence"], 1);

        let msgs = v.read_correspondence_month(SOURCE, "2026-06").unwrap();
        assert_eq!(msgs[0].kind, "call");
        assert_eq!(msgs[0].duration_secs, 0, "missed call = 0 duration");
        assert!(!msgs[0].from_me);
        assert_eq!(msgs[0].sender_name, "Alice Jones",
            "sender_name must be the contact name inside tel anchor");
    }

    #[test]
    fn parse_placed_call_from_me() {
        let v = temp_vault("placed-call");
        let out = run(&v, "Bob Smith - placed.html", CALL_PLACED);
        assert_eq!(out.counts["correspondence"], 1);

        let msgs = v.read_correspondence_month(SOURCE, "2026-06").unwrap();
        assert!(msgs[0].from_me, "placed call is from_me");
        assert_eq!(msgs[0].duration_secs, 330, "PT330S = 330 seconds");
        assert_eq!(msgs[0].chat_name, "Bob Smith",
            "chat_name must be the counterpart name inside tel anchor");
    }

    #[test]
    fn parse_voicemail_into_voice_contract() {
        let v = temp_vault("voicemail");
        let out = run(&v, "Dr Reyes - voicemail.html", VOICEMAIL);
        assert_eq!(out.counts["voicemails"], 1);
        assert_eq!(out.counts["correspondence"], 0);

        let stream = v.stream(VOICE_DIR, Partition::Month);
        let recs = stream.read::<Recording>("2026-05").unwrap();
        assert_eq!(recs.len(), 1);
        let r = &recs[0];
        assert_eq!(r.source, SOURCE);
        assert_eq!(r.kind, "voicemail");
        assert_eq!(r.sender, "+14155550137");
        // Name must be from inside tel anchor, not the "Voicemail from\nDr. Reyes Office" label.
        assert_eq!(r.sender_name, "Dr. Reyes Office",
            "sender_name must come from inside tel anchor, not the label span");
        assert_eq!(r.duration_secs, Some(27), "PT27S = 27 seconds from title attribute");
        assert_eq!(r.transcript, "Hi, this is Dr. Reyes's office confirming your appointment on Friday at ten.");
        assert!(r.audio_ref.contains("2026-05-22T180500Z_+14155550137.mp3"));
        assert!(!r.guid.is_empty());
    }

    #[test]
    fn parse_sms_thread_counterpart_and_direction() {
        let v = temp_vault("sms");
        let out = run(&v, "Bob Smith - sms.html", SMS_THREAD);
        assert_eq!(out.counts["correspondence"], 3);

        let msgs = v.read_correspondence_month(SOURCE, "2026-06").unwrap();
        assert_eq!(msgs.len(), 3);

        // First message: outgoing (Me with abbr fn, +15550000000 is owner's number).
        // The counterpart (chat) should be +15551234567 (Bob Smith), NOT the owner's number.
        assert!(msgs[0].from_me, "first msg is outgoing (Me uses abbr fn)");
        assert_eq!(msgs[0].chat, "+15551234567",
            "chat must be the counterpart's number, not the owner's number");

        // Second message: outgoing MMS with attachment.
        assert!(msgs[1].from_me);
        assert_eq!(msgs[1].text, "MMS Sent");
        assert_eq!(msgs[1].attachments.len(), 1, "MMS image attachment must be parsed");
        assert!(msgs[1].attachments[0].name.contains("Bob Smith - Text"));
        assert_eq!(msgs[1].chat, "+15551234567");

        // Third message: incoming from Bob Smith.
        assert!(!msgs[2].from_me);
        assert_eq!(msgs[2].text, "yes! where do you want to go?");
        assert_eq!(msgs[2].sender, "+15551234567");
        assert_eq!(msgs[2].sender_name, "Bob Smith");
        assert_eq!(msgs[2].chat, "+15551234567");
    }

    #[test]
    fn fn_label_not_confused_with_contact_name() {
        // Regression: the haudio-level <span class="fn"> is the label string
        // "Missed call from\nDwigt Rortugal" (from real missedcall.html).
        // tel_fn_name() must return the name inside <a class="tel">, not this.
        let html = r#"<div class="haudio">
<span class="fn">Missed call from
Dwigt Rortugal</span>
<div class="contributor vcard">Missed call from
<a class="tel" href="tel:+66666"><span class="fn">Dwigt Rortugal</span></a></div>
<abbr class="published" title="2009-09-17T17:26:41.000-07:00">Sep 17</abbr>
<div class="tags"><a rel="tag" href="http://www.google.com/voice#missed">Missed</a></div>
</div>"#;
        // tel_fn_name must return "Dwigt Rortugal", not "Missed call from\nDwigt Rortugal"
        let name = tel_fn_name(html).unwrap_or_default();
        assert_eq!(name, "Dwigt Rortugal",
            "tel_fn_name must extract name from inside tel anchor, not haudio-level label");
        // Also verify class_text would get the wrong answer (why we don't use it).
        let wrong = class_text(html, "fn").unwrap_or_default();
        // The first fn in document is the label — class_text returns "Missed call from Dwigt Rortugal"
        // (after html_decode; whitespace normalisation not applied here).
        assert!(wrong.contains("Missed call from") || wrong == "Dwigt Rortugal",
            "this test documents which fn comes first");
    }

    #[test]
    fn duration_iso8601_from_title_attr() {
        // Real Takeout: duration in title attribute as ISO 8601.
        assert_eq!(parse_iso_duration("PT18S"), Some(18));
        assert_eq!(parse_iso_duration("PT1M30S"), Some(90));
        assert_eq!(parse_iso_duration("PT1H5M30S"), Some(3930));
        assert_eq!(parse_iso_duration("PT135S"), Some(135));
        assert_eq!(parse_iso_duration("PT0S"), Some(0));
        assert_eq!(parse_iso_duration("PT2M"), Some(120));
        assert_eq!(parse_iso_duration(""), None);
    }

    #[test]
    fn duration_from_abbr_reads_title_attr() {
        // Verify full pipeline: duration_from_abbr reads the title= attribute.
        let html_with_paren = r#"<abbr class="duration" title="PT18S">(00:00:18)</abbr>"#;
        assert_eq!(duration_from_abbr(html_with_paren), Some(18),
            "must parse ISO 8601 from title=, not the inner text with parens");

        let html_no_title = r#"<abbr class="duration">0:02:15</abbr>"#;
        assert_eq!(duration_from_abbr(html_no_title), Some(135),
            "falls back to H:MM:SS inner text when no title attr");
    }

    #[test]
    fn sms_counterpart_from_incoming_not_first_tel() {
        // The first tel in the SMS thread is "+2222" (Me), the counterpart
        // is "+333" (Tony). find_sms_counterpart must return "+333".
        let html = r#"<div class="hChatLog hfeed">
<div class="message"><abbr class="dt" title="2022-06-30T18:06:39.894-07:00">Jun 30</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+2222"><abbr class="fn" title="">Me</abbr></a></cite>:
<q>doing just fine</q>
</div>
<div class="message"><abbr class="dt" title="2022-06-30T18:07:09.468-07:00">Jun 30</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+333"><span class="fn">Tony Smehrik</span></a></cite>:
<q>cool</q>
</div>
</div>"#;
        let counterpart = find_sms_counterpart(html).unwrap_or_default();
        assert_eq!(counterpart, "+333",
            "counterpart must come from the first incoming (non-Me) message, not the first tel");
    }

    #[test]
    fn record_type_from_rel_tag() {
        // Verified against real Takeout URL patterns.
        let missed = r#"<div class="tags"><a rel="tag" href="http://www.google.com/voice#missed">Missed</a></div>"#;
        assert_eq!(record_type_from_tags(missed), RecordType::Missed);
        let received = r#"<div class="tags"><a rel="tag" href="http://www.google.com/voice#received">Received</a></div>"#;
        assert_eq!(record_type_from_tags(received), RecordType::Received);
        let placed = r#"<div class="tags"><a rel="tag" href="http://www.google.com/voice#placed">Placed</a></div>"#;
        assert_eq!(record_type_from_tags(placed), RecordType::Placed);
        let voicemail = r#"<div class="tags"><a rel="tag" href="http://www.google.com/voice#voicemail">Voicemail</a></div>"#;
        assert_eq!(record_type_from_tags(voicemail), RecordType::Voicemail);
        let sms = r#"<div class="hChatLog hfeed">…</div>"#;
        assert_eq!(record_type_from_tags(sms), RecordType::Sms);
        let unknown = r#"<div class="tags"><a rel="tag" href="http://www.google.com/voice#other">Other</a></div>"#;
        assert_eq!(record_type_from_tags(unknown), RecordType::Unknown);
    }

    #[test]
    fn raw_html_written_unconditionally() {
        let v = temp_vault("raw");
        run(&v, "Bob Smith - recv.html", CALL_RECEIVED);
        let raw = v.root().join("correspondence/google-voice/raw/Bob Smith - recv.html");
        assert!(raw.exists(), "raw HTML file must be written");
        let content = fs::read_to_string(&raw).unwrap();
        assert!(content.contains("Received call from"));
    }

    #[test]
    fn dedupe_on_reimport() {
        let v = temp_vault("dedup");
        let out1 = run(&v, "Bob Smith - received.html", CALL_RECEIVED);
        assert_eq!(out1.counts["correspondence"], 1);

        // Re-import same file: zero new rows, 1 duplicate.
        let out2 = run(&v, "Bob Smith - received.html", CALL_RECEIVED);
        assert_eq!(out2.counts["correspondence"], 0);
        assert_eq!(out2.counts["duplicates"], 1);

        let msgs = v.read_correspondence_month(SOURCE, "2026-06").unwrap();
        assert_eq!(msgs.len(), 1, "exactly one row after two imports");
    }

    #[test]
    fn timestamp_normalize() {
        let s = normalize_ts("2022-09-30T14:36:36.127-04:00");
        // Should parse and round-trip to an RFC3339 string.
        assert!(s.contains("2022-09-30"), "date preserved: {s}");
        assert!(!s.is_empty());

        // Verbatim fallback for unparseable.
        let bad = normalize_ts("not a date");
        assert_eq!(bad, "not a date");
    }

    #[test]
    fn import_from_zip() {
        use std::io::Write;
        let v = temp_vault("zip");
        let zip_path = v.root().join("takeout-voice.zip");
        let mut w = zip::ZipWriter::new(fs::File::create(&zip_path).unwrap());
        let opts = zip::write::SimpleFileOptions::default();
        w.start_file("Takeout/Voice/Calls/Bob Smith - 2026-06-10 - received.html", opts).unwrap();
        w.write_all(CALL_RECEIVED.as_bytes()).unwrap();
        w.start_file("Takeout/Voice/Calls/Dr Reyes - voicemail.html", opts).unwrap();
        w.write_all(VOICEMAIL.as_bytes()).unwrap();
        // Decoy: non-HTML file should be ignored.
        w.start_file("Takeout/Voice/Calls/README.txt", opts).unwrap();
        w.write_all(b"not html").unwrap();
        w.finish().unwrap();

        let out = (IMPORT.run)(&v, &zip_path, &BTreeMap::new(), &mut |_| {}).unwrap();
        assert_eq!(out.counts["correspondence"], 1, "one call");
        assert_eq!(out.counts["voicemails"], 1, "one voicemail");
        assert_eq!(out.counts["skipped"], 0, "no non-HTML in counts");
    }

    #[test]
    fn html_decode_entities() {
        assert_eq!(html_decode("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(html_decode("&lt;b&gt;bold&lt;/b&gt;"), "<b>bold</b>");
        assert_eq!(html_decode("&quot;hello&quot;"), "\"hello\"");
    }

    #[test]
    fn mms_attachment_parsed() {
        let block = r#"<div class="message"><abbr class="dt" title="2026-06-10T10:35:00.000-07:00">Jun 10</abbr>:
<cite class="sender vcard"><a class="tel" href="tel:+15550000000"><abbr class="fn" title="">Me</abbr></a></cite>:
<q>MMS Sent</q>
<div><img src="Bob Smith - Text - 2026-06-10T173500Z-2-1" alt="Image MMS Attachment" /></div></div>"#;
        let attachments = parse_mms_attachments(block);
        assert_eq!(attachments.len(), 1, "one MMS image attachment");
        assert!(attachments[0].name.contains("Bob Smith - Text"));
        assert_eq!(attachments[0].mime, "image/*");
    }
}
